use ash_core::{
    CancellationToken, Message, ModelClient, ModelId, ModelRequest, ModelStreamEvent,
    ToolDefinition,
};
use futures::StreamExt;

use crate::context::{
    apply_summary, estimate_request_tokens, needs_compaction, plan_compaction, prune_tool_outputs,
    summary_output_tokens,
};

pub(crate) const COMPACTION_SYSTEM_PROMPT: &str = "You are an anchored context summarization assistant for coding threads. Summarize only the supplied conversation history. Do not answer the conversation. Preserve exact technical details and respond in the conversation's language.";

#[derive(Clone)]
pub struct ContextRequest {
    pub model: ModelId,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_context_tokens: usize,
}

pub struct PreparedContext {
    pub messages: Vec<Message>,
    pub update: Option<ContextUpdate>,
}

pub struct ContextUpdate {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub dropped_messages: usize,
}

pub(crate) struct CompactedContext {
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
pub struct CodingContextPolicy;

impl CodingContextPolicy {
    pub(crate) async fn compact(
        &self,
        request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> Result<Option<CompactedContext>, ash_core::AshError> {
        let before_tokens = estimate_request_tokens(
            request.system_prompt.as_deref(),
            &request.messages,
            &request.tools,
        );
        let Some(plan) = plan_compaction(&request.messages, request.max_context_tokens) else {
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
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Err(ash_core::AshError::Cancelled),
                next = stream.next() => next,
            };
            let Some(item) = next else {
                break;
            };
            match item? {
                ModelStreamEvent::TextDelta(text) => summary.push_str(&text),
                ModelStreamEvent::ThinkingDelta(_)
                | ModelStreamEvent::Usage { .. }
                | ModelStreamEvent::Stop(_) => {}
                ModelStreamEvent::ToolCall { .. } => {
                    return Err(ash_core::ProtocolError::InvalidResponse(
                        "compaction model unexpectedly requested a tool".to_string(),
                    )
                    .into());
                }
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
        let after_tokens =
            estimate_request_tokens(request.system_prompt.as_deref(), &messages, &request.tools);
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
impl ContextPolicy for CodingContextPolicy {
    async fn prepare(
        &self,
        mut request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> Result<PreparedContext, ash_core::AshError> {
        if let Some(pruned) = prune_tool_outputs(&request.messages) {
            request.messages = pruned;
        }
        let estimated_tokens = estimate_request_tokens(
            request.system_prompt.as_deref(),
            &request.messages,
            &request.tools,
        );
        if !needs_compaction(estimated_tokens, request.max_context_tokens) {
            return Ok(PreparedContext {
                messages: request.messages,
                update: None,
            });
        }
        let Some(compacted) = self.compact(request.clone(), model, cancel).await? else {
            return Ok(PreparedContext {
                messages: request.messages,
                update: None,
            });
        };
        Ok(PreparedContext {
            messages: compacted.messages,
            update: Some(compacted.update),
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PassthroughContextPolicy;

#[async_trait::async_trait]
impl ContextPolicy for PassthroughContextPolicy {
    async fn prepare(
        &self,
        request: ContextRequest,
        _model: &dyn ModelClient,
        _cancel: &CancellationToken,
    ) -> Result<PreparedContext, ash_core::AshError> {
        Ok(PreparedContext {
            messages: request.messages,
            update: None,
        })
    }
}
