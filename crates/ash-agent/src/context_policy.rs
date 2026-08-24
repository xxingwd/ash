use ash_core::{
    CancellationToken, ContextUpdate, Message, ModelClient, ModelEvent, ModelId, ModelRequest,
    ToolDefinition, TurnStats,
};
use futures::StreamExt;
use std::time::Instant;

use crate::context::{
    apply_summary, estimate_request_tokens, estimate_tokens, needs_compaction, plan_compaction,
    prune_tool_outputs, summary_output_tokens,
};
use crate::usage::UsageAccumulator;

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

/// Context preparation result plus any model work performed while producing it.
pub struct ContextOutcome {
    pub result: Result<PreparedContext, ash_core::AshError>,
    pub stats: TurnStats,
}

pub struct CompactedContext {
    pub messages: Vec<Message>,
    pub update: ContextUpdate,
}

pub(crate) struct CompactionOutcome {
    pub result: Result<Option<CompactedContext>, ash_core::AshError>,
    pub stats: TurnStats,
}

#[async_trait::async_trait]
pub trait ContextPolicy: Send + Sync {
    async fn prepare(
        &self,
        request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> ContextOutcome;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultContextPolicy;

impl DefaultContextPolicy {
    pub(crate) async fn compact(
        &self,
        request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> CompactionOutcome {
        let before_tokens = estimate_context_request(&request);
        let durable_budget = request
            .max_context_tokens
            .saturating_sub(crate::context::count_tokens(&request.ephemeral_context));
        let Some(plan) = plan_compaction(&request.messages, durable_budget) else {
            return CompactionOutcome {
                result: Ok(None),
                stats: TurnStats::default(),
            };
        };
        let summary_request = vec![Message::user(&plan.summary_prompt)];
        let estimated_input_tokens =
            estimate_request_tokens(Some(COMPACTION_SYSTEM_PROMPT), &summary_request, &[]);
        let mut stream = match model.stream(ModelRequest {
            model: request.model,
            system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
            messages: summary_request,
            tools: Vec::new(),
            max_tokens: Some(summary_output_tokens(request.max_context_tokens)),
        }) {
            Ok(stream) => stream,
            Err(error) => {
                return CompactionOutcome {
                    result: Err(error.into()),
                    stats: TurnStats::default(),
                };
            }
        };
        let mut summary = String::new();
        let mut generated = String::new();
        let mut first_output_at = None;
        let mut usage = UsageAccumulator::default();
        let mut stop_reason = None;
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => {
                    return failed_compaction(
                        ash_core::AshError::Cancelled,
                        &usage,
                        estimated_input_tokens,
                        &generated,
                        first_output_at,
                    );
                }
                next = stream.next() => next,
            };
            let Some(item) = next else {
                break;
            };
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    return failed_compaction(
                        error.into(),
                        &usage,
                        estimated_input_tokens,
                        &generated,
                        first_output_at,
                    );
                }
            };
            match item {
                ModelEvent::Text(text) => {
                    first_output_at.get_or_insert_with(Instant::now);
                    generated.push_str(&text);
                    summary.push_str(&text);
                }
                ModelEvent::Reasoning(reasoning) => {
                    first_output_at.get_or_insert_with(Instant::now);
                    generated.push_str(&reasoning);
                }
                ModelEvent::Stop(reason) => stop_reason = Some(reason),
                ModelEvent::Usage(reported) => usage.record(reported),
                ModelEvent::ToolCall {
                    name, arguments, ..
                } => {
                    first_output_at.get_or_insert_with(Instant::now);
                    generated.push_str(&name);
                    generated.push_str(&arguments.to_string());
                    return failed_compaction(
                        ash_core::ProtocolError::InvalidResponse(
                            "compaction model unexpectedly requested a tool".to_string(),
                        )
                        .into(),
                        &usage,
                        estimated_input_tokens,
                        &generated,
                        first_output_at,
                    );
                }
            }
        }
        match stop_reason {
            Some(ash_core::StopReason::EndTurn) => {}
            Some(reason) => {
                return failed_compaction(
                    ash_core::ProtocolError::InvalidResponse(format!(
                        "compaction model stopped before completing the summary: {reason}"
                    ))
                    .into(),
                    &usage,
                    estimated_input_tokens,
                    &generated,
                    first_output_at,
                );
            }
            None => {
                return failed_compaction(
                    ash_core::ProtocolError::InvalidResponse(
                        "compaction model stream ended without a terminal marker".to_string(),
                    )
                    .into(),
                    &usage,
                    estimated_input_tokens,
                    &generated,
                    first_output_at,
                );
            }
        }
        let summary = summary.trim();
        if summary.is_empty() {
            return failed_compaction(
                ash_core::ProtocolError::InvalidResponse(
                    "compaction model returned an empty summary".to_string(),
                )
                .into(),
                &usage,
                estimated_input_tokens,
                &generated,
                first_output_at,
            );
        }
        let stats = compaction_stats(&usage, estimated_input_tokens, &generated, first_output_at);
        let messages = apply_summary(summary, plan.tail);
        let after_tokens = estimate_request_tokens_with_ephemeral(
            request.system_prompt.as_deref(),
            &messages,
            &request.ephemeral_context,
            &request.tools,
        );
        if after_tokens >= before_tokens {
            return CompactionOutcome {
                result: Ok(None),
                stats,
            };
        }
        CompactionOutcome {
            result: Ok(Some(CompactedContext {
                messages,
                update: ContextUpdate {
                    before_tokens: u64::try_from(before_tokens).unwrap_or(u64::MAX),
                    after_tokens: u64::try_from(after_tokens).unwrap_or(u64::MAX),
                    dropped_messages: u64::try_from(plan.compacted_messages).unwrap_or(u64::MAX),
                },
            })),
            stats,
        }
    }
}

#[async_trait::async_trait]
impl ContextPolicy for DefaultContextPolicy {
    async fn prepare(
        &self,
        mut request: ContextRequest,
        model: &dyn ModelClient,
        cancel: &CancellationToken,
    ) -> ContextOutcome {
        if let Some(pruned) = prune_tool_outputs(&request.messages) {
            request.messages = pruned;
        }
        let estimated_tokens = estimate_context_request(&request);
        if !needs_compaction(estimated_tokens, request.max_context_tokens) {
            return ContextOutcome {
                result: Ok(request.into_prepared(estimated_tokens)),
                stats: TurnStats::default(),
            };
        }
        let outcome = self.compact(request.clone(), model, cancel).await;
        let compacted = match outcome.result {
            Ok(compacted) => compacted,
            Err(error) => {
                return ContextOutcome {
                    result: Err(error),
                    stats: outcome.stats,
                };
            }
        };
        let Some(compacted) = compacted else {
            return ContextOutcome {
                result: Ok(request.into_prepared(estimated_tokens)),
                stats: outcome.stats,
            };
        };
        ContextOutcome {
            result: Ok(PreparedContext {
                estimated_input_tokens: usize::try_from(compacted.update.after_tokens)
                    .unwrap_or(usize::MAX),
                messages: compacted.messages,
                ephemeral_context: request.ephemeral_context,
                update: Some(compacted.update),
            }),
            stats: outcome.stats,
        }
    }
}

fn failed_compaction(
    error: ash_core::AshError,
    usage: &UsageAccumulator,
    estimated_input_tokens: usize,
    generated: &str,
    first_output_at: Option<Instant>,
) -> CompactionOutcome {
    CompactionOutcome {
        result: Err(error),
        stats: compaction_stats(usage, estimated_input_tokens, generated, first_output_at),
    }
}

fn compaction_stats(
    usage: &UsageAccumulator,
    estimated_input_tokens: usize,
    generated: &str,
    first_output_at: Option<Instant>,
) -> TurnStats {
    TurnStats {
        usage: usage.finish(
            u64::try_from(estimated_input_tokens).unwrap_or(u64::MAX),
            u64::try_from(estimate_tokens(generated)).unwrap_or(u64::MAX),
        ),
        generation_ms: first_output_at.map_or(0, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        }),
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
