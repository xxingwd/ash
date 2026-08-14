use ash_core::{
    CancellationToken, Message, ModelClient, ModelEvent, ModelId, ModelRequest, ToolDefinition,
};
use futures::StreamExt;

use crate::context::{
    apply_summary, estimate_request_tokens, needs_compaction, plan_compaction, prune_tool_outputs,
    summary_output_tokens,
};

pub const COMPACTION_SYSTEM_PROMPT: &str = "You are an anchored context summarization assistant for coding sessions. Summarize only the supplied conversation history. Do not answer the conversation. Preserve exact technical details and respond in the conversation's language.";

#[derive(Clone)]
pub struct ContextRequest {
    pub model: ModelId,
    pub system_prompt: Option<String>,
    /// Durable conversation context. Policy updates to these messages may be
    /// persisted as a context checkpoint.
    pub messages: Vec<Message>,
    /// Turn-scoped context supplied by extensions. It participates in request
    /// sizing but must never be summarized into or written as a checkpoint.
    pub ephemeral_context: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_context_tokens: usize,
}

impl ContextRequest {
    /// Build a prepared context that needs no durable update, consuming the
    /// request's message buffers.
    fn into_prepared(self, estimated_input_tokens: usize) -> PreparedContext {
        PreparedContext {
            estimated_input_tokens,
            messages: self.messages,
            ephemeral_context: self.ephemeral_context,
            update: None,
        }
    }
}

pub struct PreparedContext {
    /// Prepared durable conversation context.
    pub messages: Vec<Message>,
    /// Prepared turn-scoped context, kept separate from durable messages.
    pub ephemeral_context: Vec<Message>,
    pub update: Option<ContextUpdate>,
    /// Token estimate for the prepared messages (system + tools + history),
    /// computed once by the policy so the engine does not re-tokenize.
    pub estimated_input_tokens: usize,
}

pub struct ContextUpdate {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub dropped_messages: usize,
}

pub struct CompactedContext {
    pub messages: Vec<Message>,
    pub update: ContextUpdate,
}

#[async_trait::async_trait]
pub trait ContextPolicy: Send + Sync {
    async fn prepare(
        &self,
        request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> Result<PreparedContext, ash_core::AshError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultContextPolicy;

impl DefaultContextPolicy {
    pub(crate) async fn compact(
        &self,
        request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> Result<Option<CompactedContext>, ash_core::AshError> {
        let before_tokens = estimate_context_request(&request);
        let durable_budget = request
            .max_context_tokens
            .saturating_sub(crate::context::count_tokens(&request.ephemeral_context));
        let Some(plan) = plan_compaction(&request.messages, durable_budget) else {
            return Ok(None);
        };
        let mut stream = model.stream(ModelRequest {
            model: request.model,
            system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
            messages: vec![Message::user(&plan.summary_prompt)],
            tools: Vec::new(),
            max_tokens: Some(summary_output_tokens(request.max_context_tokens)),
        })?;
        let mut summary = String::new();
        let mut stop_reason = None;
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => return Err(ash_core::AshError::Cancelled),
                next = stream.next() => next,
            };
            let Some(item) = next else {
                break;
            };
            match item? {
                ModelEvent::Text(text) => summary.push_str(&text),
                ModelEvent::Stop(reason) => stop_reason = Some(reason),
                ModelEvent::Reasoning(_) | ModelEvent::Usage(_) => {}
                ModelEvent::ToolCall { .. } => {
                    return Err(ash_core::ProtocolError::InvalidResponse(
                        "compaction model unexpectedly requested a tool".to_string(),
                    )
                    .into());
                }
            }
        }
        match stop_reason {
            Some(ash_core::StopReason::EndTurn) => {}
            Some(reason) => {
                return Err(ash_core::ProtocolError::InvalidResponse(format!(
                    "compaction model stopped before completing the summary: {reason}"
                ))
                .into());
            }
            None => {
                return Err(ash_core::ProtocolError::InvalidResponse(
                    "compaction model stream ended without a terminal marker".to_string(),
                )
                .into());
            }
        }
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(ash_core::ProtocolError::InvalidResponse(
                "compaction model returned an empty summary".to_string(),
            )
            .into());
        }
        let messages = apply_summary(summary, plan.tail);
        let after_tokens = estimate_request_tokens_with_ephemeral(
            request.system_prompt.as_deref(),
            &messages,
            &request.ephemeral_context,
            &request.tools,
        );
        if after_tokens >= before_tokens {
            return Ok(None);
        }
        Ok(Some(CompactedContext {
            messages,
            update: ContextUpdate {
                before_tokens,
                after_tokens,
                dropped_messages: plan.compacted_messages,
            },
        }))
    }
}

#[async_trait::async_trait]
impl ContextPolicy for DefaultContextPolicy {
    async fn prepare(
        &self,
        mut request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> Result<PreparedContext, ash_core::AshError> {
        if let Some(pruned) = prune_tool_outputs(&request.messages) {
            request.messages = pruned;
        }
        let estimated_tokens = estimate_context_request(&request);
        if !needs_compaction(estimated_tokens, request.max_context_tokens) {
            return Ok(request.into_prepared(estimated_tokens));
        }
        let Some(compacted) = self.compact(request.clone(), model, cancel).await? else {
            return Ok(request.into_prepared(estimated_tokens));
        };
        Ok(PreparedContext {
            estimated_input_tokens: compacted.update.after_tokens,
            messages: compacted.messages,
            ephemeral_context: request.ephemeral_context,
            update: Some(compacted.update),
        })
    }
}

fn estimate_context_request(request: &ContextRequest) -> usize {
    estimate_request_tokens_with_ephemeral(
        request.system_prompt.as_deref(),
        &request.messages,
        &request.ephemeral_context,
        &request.tools,
    )
}

fn estimate_request_tokens_with_ephemeral(
    system_prompt: Option<&str>,
    messages: &[Message],
    ephemeral_context: &[Message],
    tools: &[ToolDefinition],
) -> usize {
    if ephemeral_context.is_empty() {
        return estimate_request_tokens(system_prompt, messages, tools);
    }
    let mut combined = Vec::with_capacity(messages.len() + ephemeral_context.len());
    combined.extend_from_slice(messages);
    combined.extend_from_slice(ephemeral_context);
    estimate_request_tokens(system_prompt, &combined, tools)
}
