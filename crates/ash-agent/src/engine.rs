use ash_core::{
    CancellationToken, ContentBlock, LiveEvent, Message, ModelClient, ModelEvent, ModelRequest,
    ModelStream, ModelUsage, SessionEventKind, SessionIdentity, StopReason, ToolCallId,
    ToolContext, ToolDefinition, ToolError, ToolOutput, ToolTimeout, TurnStats, Usage,
};
use futures::{future::join_all, StreamExt};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    context::{
        apply_summary, estimate_request_tokens, needs_compaction, plan_compaction,
        prune_tool_outputs, summary_output_tokens,
    },
    log::{ContextCheckpoint, LogEntry},
    Agent,
};

const MAX_AGENT_OUTPUT_BYTES: usize = 64 * 1024;
const AGENT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const AGENT_OUTPUT_TRUNCATION_NOTICE: &str = "\n... tool output truncated by the agent ...\n";
const MAX_RETRIES: u32 = 5;
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(10);
const COMPACTION_SYSTEM_PROMPT: &str = "You are an anchored context summarization assistant for coding sessions. Summarize only the supplied conversation history. Do not answer the conversation. Preserve exact technical details and respond in the conversation's language.";

#[derive(Clone)]
struct PendingToolCall {
    id: ToolCallId,
    name: String,
    arguments: serde_json::Value,
}

/// What the turn loop should do after one model/tool step.
enum Step {
    More,
    Done(StopReason),
}

/// How a step responds to a retryable model failure.
enum RetryAction {
    /// Ready to re-issue the request.
    Retry,
    /// Cancelled during the backoff; the turn must abort.
    Abort,
}

struct CollectedResponse {
    message: Option<Message>,
    usage: Usage,
    generation_ms: u64,
    outcome: ResponseOutcome,
}

enum ResponseOutcome {
    Finished(StopReason),
    ToolCalls(Vec<PendingToolCall>),
    Cancelled,
    Failed(ash_core::ProtocolError),
}

enum StreamExit {
    Exhausted,
    Cancelled,
    Failed(ash_core::ProtocolError),
}

#[derive(Clone, Copy)]
enum OpenResponseBlock {
    Thought,
    Text,
}

#[derive(Debug)]
pub(crate) struct CompactedContext {
    pub(crate) messages: Vec<Message>,
    pub(crate) update: ash_core::ContextUpdate,
}

pub(crate) struct EngineOutcome {
    pub(crate) entries: Vec<LogEntry>,
    pub(crate) result: Result<StopReason, ash_core::AshError>,
    pub(crate) stats: TurnStats,
}

struct AgentTurnRunner<'a> {
    agent: &'a Agent,
    tx: mpsc::Sender<SessionEventKind>,
    cancel: CancellationToken,
    identity: SessionIdentity,
    model: &'a dyn ModelClient,
    tool_defs: Vec<ToolDefinition>,
    entries: Vec<LogEntry>,
    stats: TurnStats,
}

pub(crate) async fn run_agent_turn(
    model: &dyn ModelClient,
    agent: &Agent,
    messages: &mut Vec<Message>,
    identity: SessionIdentity,
    tx: mpsc::Sender<SessionEventKind>,
    cancel: CancellationToken,
) -> EngineOutcome {
    AgentTurnRunner::new(agent, model, identity, tx, cancel)
        .run(messages)
        .await
}

impl<'a> AgentTurnRunner<'a> {
    fn new(
        agent: &'a Agent,
        model: &'a dyn ModelClient,
        identity: SessionIdentity,
        tx: mpsc::Sender<SessionEventKind>,
        cancel: CancellationToken,
    ) -> Self {
        let tool_defs = agent.tool_definitions();
        Self {
            agent,
            tx,
            cancel,
            identity,
            model,
            tool_defs,
            entries: Vec::new(),
            stats: TurnStats::default(),
        }
    }

    async fn run(mut self, messages: &mut Vec<Message>) -> EngineOutcome {
        let result = self.run_result(messages).await;
        EngineOutcome {
            entries: self.entries,
            result,
            stats: self.stats,
        }
    }

    async fn run_result(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<StopReason, ash_core::AshError> {
        loop {
            if self.cancel.is_cancelled() {
                return Ok(StopReason::Aborted);
            }
            debug!("calling LLM");
            if let Step::Done(reason) = self.step(messages).await? {
                return Ok(reason);
            }
        }
    }

    /// Run one model call plus any tool calls it produced.
    /// Returns `Step::More` to continue the turn or `Step::Done` to stop it.
    ///
    /// Safe failures (network errors, upstream 5xx, rate limits, truncated
    /// streams) are retried up to `MAX_RETRIES` times. A retry is only
    /// taken before any tool call was executed: the partial output is
    /// discarded and the identical request is re-issued, so retries never
    /// repeat side effects.
    async fn step(&mut self, messages: &mut Vec<Message>) -> Result<Step, ash_core::AshError> {
        let mut retries = 0u32;
        loop {
            self.prepare_context(messages).await?;

            let mut stream = self.model.stream(ModelRequest {
                model: self.agent.model().clone(),
                system: self.agent.system_prompt().map(str::to_string),
                messages: messages.clone(),
                tools: self.tool_defs.clone(),
                max_tokens: None,
            })?;
            let CollectedResponse {
                message,
                usage,
                generation_ms,
                outcome,
            } = collect_response(&mut stream, Some(&self.tx), &self.cancel, self.stats).await;

            let request_stats = TurnStats {
                usage,
                generation_ms,
            };
            self.stats = self.stats.saturating_add(request_stats);
            send_progress(&self.tx, self.stats);

            // Retry only safe, retryable failures before any side effect.
            // A truncated stream produced no terminal marker from the
            // provider; without tool calls the request is idempotent, so
            // re-issuing it is safe. Other failures retry only when the
            // transport itself is the likely culprit.
            let retry = match &outcome {
                ResponseOutcome::Failed(error) => retryable(error),
                ResponseOutcome::Finished(StopReason::Truncated) => true,
                _ => false,
            };
            match self.maybe_retry(&mut retries, retry).await {
                Some(RetryAction::Abort) => return Ok(Step::Done(StopReason::Aborted)),
                Some(RetryAction::Retry) => continue,
                None => {}
            }

            if let Some(message) = message {
                self.record_message(messages, message);
            }

            return self.finish_outcome(messages, outcome).await;
        }
    }

    /// Retry a safe, retryable model failure before any tool side effect.
    /// Cancellation during the backoff aborts the turn.
    async fn maybe_retry(&mut self, retries: &mut u32, retry: bool) -> Option<RetryAction> {
        if !retry || *retries >= MAX_RETRIES {
            return None;
        }
        let delay = retry_delay(*retries, RETRY_BASE, RETRY_MAX);
        debug!(
            retries = *retries,
            ?delay,
            "retrying model call after retryable failure"
        );
        tokio::select! {
            () = self.cancel.cancelled() => return Some(RetryAction::Abort),
            () = tokio::time::sleep(delay) => {}
        }
        *retries += 1;
        Some(RetryAction::Retry)
    }

    /// Consume the response outcome into the next step or a terminal error.
    async fn finish_outcome(
        &mut self,
        messages: &mut Vec<Message>,
        outcome: ResponseOutcome,
    ) -> Result<Step, ash_core::AshError> {
        match outcome {
            ResponseOutcome::Failed(error) => Err(error.into()),
            ResponseOutcome::Cancelled => Ok(Step::Done(StopReason::Aborted)),
            ResponseOutcome::ToolCalls(calls) => {
                self.execute_tool_calls(messages, calls).await;
                if self.cancel.is_cancelled() {
                    return Ok(Step::Done(StopReason::Aborted));
                }
                Ok(Step::More)
            }
            ResponseOutcome::Finished(reason) => Ok(Step::Done(reason)),
        }
    }

    async fn prepare_context(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<(), ash_core::AshError> {
        if let Some(pruned) = prune_tool_outputs(messages) {
            *messages = pruned;
        }
        let mut estimated_input_tokens =
            estimate_request_tokens(self.agent.system_prompt(), messages, &self.tool_defs);
        let update = if needs_compaction(estimated_input_tokens, self.agent.max_context_tokens()) {
            let (result, stats) =
                compact_context(self.agent, messages, self.model, &self.cancel).await;
            self.stats = self.stats.saturating_add(stats);
            if stats != TurnStats::default() {
                send_progress(&self.tx, self.stats);
            }
            match result? {
                Some(compacted) => {
                    estimated_input_tokens =
                        usize::try_from(compacted.update.after_tokens).unwrap_or(usize::MAX);
                    *messages = compacted.messages;
                    Some(compacted.update)
                }
                None => None,
            }
        } else {
            None
        };
        send_live(
            &self.tx,
            SessionEventKind::ContextChanged {
                tokens: sat_u64(estimated_input_tokens),
            },
        )
        .await;
        let Some(update) = update else {
            return Ok(());
        };
        self.entries
            .push(LogEntry::Checkpoint(ContextCheckpoint::from_model_context(
                messages,
            )?));
        send_live(&self.tx, SessionEventKind::ContextCompacted(update)).await;
        Ok(())
    }

    fn record_message(&mut self, messages: &mut Vec<Message>, message: Message) {
        self.entries.push(LogEntry::Message(message.clone()));
        messages.push(message);
    }

    /// Run one model response's calls concurrently against the same context.
    /// `join_all` preserves provider order for the durable tool-result messages.
    async fn execute_tool_calls(
        &mut self,
        messages: &mut Vec<Message>,
        calls: Vec<PendingToolCall>,
    ) {
        self.stats.usage.tool_calls = self
            .stats
            .usage
            .tool_calls
            .saturating_add(sat_u64(calls.len()));
        send_progress(&self.tx, self.stats);

        let results = join_all(calls.into_iter().map(|call| self.execute_tool_call(call))).await;

        for message in results {
            self.record_message(messages, message);
        }
    }

    async fn execute_tool_call(&self, call: PendingToolCall) -> Message {
        send_live(
            &self.tx,
            SessionEventKind::Live(LiveEvent::ToolStarted {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }),
        )
        .await;
        let result = limit_tool_result(self.execute_tool(&call.name, call.arguments.clone()).await);
        let (result, attachments) = match result {
            Ok(ToolOutput { text, attachments }) => (Ok(text), attachments),
            Err(error) => (Err(error.to_string()), Vec::new()),
        };
        send_live(
            &self.tx,
            SessionEventKind::Live(LiveEvent::ToolFinished {
                id: call.id.clone(),
                name: call.name,
                arguments: call.arguments,
                result: result.clone(),
            }),
        )
        .await;
        Message::tool_result(call.id, result, attachments)
    }

    async fn execute_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let Some(tool) = self.agent.tools().iter().find(|tool| tool.name() == name) else {
            return Err(ToolError::Execution(format!("unknown tool: {name}")));
        };
        let timeout = match tool.timeout() {
            ToolTimeout::Session => Some(self.agent.tool_timeout()),
            ToolTimeout::Disabled => None,
        };
        let context = ToolContext {
            identity: self.identity,
            cancellation: self.cancel.clone(),
            deadline: timeout.map(|timeout| Instant::now() + timeout),
        };
        context
            .run(tool.execute(context.clone(), arguments))
            .await?
    }
}

pub(crate) async fn compact_context(
    agent: &Agent,
    messages: &[Message],
    model: &dyn ModelClient,
    cancel: &CancellationToken,
) -> (
    Result<Option<CompactedContext>, ash_core::AshError>,
    TurnStats,
) {
    let tools = agent.tool_definitions();
    let before_tokens = estimate_request_tokens(agent.system_prompt(), messages, &tools);
    let Some(plan) = plan_compaction(messages, agent.max_context_tokens()) else {
        return (Ok(None), TurnStats::default());
    };
    let summary_messages = vec![Message::user(&plan.summary_prompt)];
    let mut stream = match model.stream(ModelRequest {
        model: agent.model().clone(),
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: summary_messages,
        tools: Vec::new(),
        max_tokens: Some(summary_output_tokens(agent.max_context_tokens())),
    }) {
        Ok(stream) => stream,
        Err(error) => return (Err(error.into()), TurnStats::default()),
    };
    let response = collect_response(&mut stream, None, cancel, TurnStats::default()).await;
    let stats = TurnStats {
        usage: response.usage,
        generation_ms: response.generation_ms,
    };
    let result = match response.outcome {
        ResponseOutcome::Cancelled => Err(ash_core::AshError::Cancelled),
        ResponseOutcome::Failed(error) => Err(error.into()),
        ResponseOutcome::ToolCalls(_) => Err(ash_core::ProtocolError::InvalidResponse(
            "compaction model unexpectedly requested a tool".to_string(),
        )
        .into()),
        ResponseOutcome::Finished(reason) if reason != StopReason::EndTurn => {
            Err(ash_core::ProtocolError::InvalidResponse(format!(
                "compaction model stopped before completing the summary: {reason}"
            ))
            .into())
        }
        ResponseOutcome::Finished(_) => {
            let summary = response
                .message
                .as_ref()
                .and_then(Message::visible_text)
                .unwrap_or_default();
            if summary.trim().is_empty() {
                Err(ash_core::ProtocolError::InvalidResponse(
                    "compaction model returned an empty summary".to_string(),
                )
                .into())
            } else {
                let compacted_messages = apply_summary(&summary, plan.tail);
                let after_tokens =
                    estimate_request_tokens(agent.system_prompt(), &compacted_messages, &tools);
                if after_tokens >= before_tokens {
                    Ok(None)
                } else {
                    Ok(Some(CompactedContext {
                        messages: compacted_messages,
                        update: ash_core::ContextUpdate {
                            before_tokens: sat_u64(before_tokens),
                            after_tokens: sat_u64(after_tokens),
                            dropped_messages: sat_u64(plan.compacted_messages),
                        },
                    }))
                }
            }
        }
    };
    (result, stats)
}

/// Send one live event to the session actor. A closed receiver means the
/// session is shutting down (or already gone): the turn keeps running to a
/// result, but its streaming preview has nowhere to go. Log instead of
/// failing the turn silently.
async fn send_live(tx: &mpsc::Sender<SessionEventKind>, kind: SessionEventKind) {
    if let Err(error) = tx.send(kind).await {
        tracing::warn!(%error, "dropping live event: session event receiver closed");
    }
}

/// Progress is an ephemeral full snapshot. Dropping one under backpressure is
/// safe because the next snapshot or the durable TurnCompleted event converges.
fn send_progress(tx: &mpsc::Sender<SessionEventKind>, stats: TurnStats) {
    match tx.try_send(SessionEventKind::TurnProgress(stats)) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!("dropping turn progress: session event receiver closed");
        }
    }
}

async fn collect_response(
    stream: &mut ModelStream,
    tx: Option<&mpsc::Sender<SessionEventKind>>,
    cancel: &CancellationToken,
    settled_stats: TurnStats,
) -> CollectedResponse {
    let mut accumulator = ResponseAccumulator::new();
    let stream_exit = loop {
        let next = tokio::select! {
            () = cancel.cancelled() => break StreamExit::Cancelled,
            next = stream.next() => next,
        };
        let Some(item) = next else {
            break StreamExit::Exhausted;
        };
        let item = match item {
            Ok(item) => item,
            Err(error) => break StreamExit::Failed(error),
        };
        if accumulator.apply(tx, item).await {
            if let Some(tx) = tx {
                accumulator.publish_progress(tx, settled_stats);
            }
        }
    };
    accumulator.finish(stream_exit)
}

/// Incremental state accumulated while reading one model stream.
struct ResponseAccumulator {
    blocks: Vec<ContentBlock>,
    thought_started_at: Option<Instant>,
    first_output_at: Option<Instant>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    open_block: Option<OpenResponseBlock>,
}

impl ResponseAccumulator {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            thought_started_at: None,
            first_output_at: None,
            stop_reason: None,
            usage: Usage::default(),
            open_block: None,
        }
    }

    /// Fold one stream item into the accumulator, forwarding live deltas.
    /// Returns whether provider-reported usage changed.
    async fn apply(
        &mut self,
        tx: Option<&mpsc::Sender<SessionEventKind>>,
        item: ModelEvent,
    ) -> bool {
        let previous_usage = self.usage;
        match item {
            ModelEvent::Text(delta) => {
                self.first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut self.blocks, &mut self.thought_started_at);
                match self.blocks.last_mut() {
                    Some(ContentBlock::Text(text)) => text.push_str(&delta),
                    _ => self.blocks.push(ContentBlock::Text(delta.clone())),
                }
                self.open_block = Some(OpenResponseBlock::Text);
                if let Some(tx) = tx {
                    send_live(tx, SessionEventKind::Live(LiveEvent::TextDelta(delta))).await;
                }
            }
            ModelEvent::Reasoning(delta) => {
                self.first_output_at.get_or_insert_with(Instant::now);
                if let Some(ContentBlock::Thought { text, .. }) = self.blocks.last_mut() {
                    text.push_str(&delta);
                } else {
                    self.thought_started_at = Some(Instant::now());
                    self.blocks.push(ContentBlock::Thought {
                        text: delta.clone(),
                        elapsed_seconds: 0,
                    });
                }
                self.open_block = Some(OpenResponseBlock::Thought);
                if let Some(tx) = tx {
                    send_live(tx, SessionEventKind::Live(LiveEvent::ReasoningDelta(delta))).await;
                }
            }
            ModelEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.first_output_at.get_or_insert_with(Instant::now);
                let block = ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                };
                finish_open_thought(&mut self.blocks, &mut self.thought_started_at);
                self.open_block = None;
                self.blocks.push(block);
            }
            ModelEvent::Usage(reported) => {
                self.usage = merge_request_usage(self.usage, reported);
            }
            ModelEvent::Stop(reason) => {
                finish_open_thought(&mut self.blocks, &mut self.thought_started_at);
                self.open_block = None;
                self.stop_reason = Some(reason);
            }
        }
        self.usage != previous_usage
    }

    fn publish_progress(&self, tx: &mpsc::Sender<SessionEventKind>, settled_stats: TurnStats) {
        let request_stats = TurnStats {
            usage: self.usage,
            generation_ms: elapsed_generation_ms(self.first_output_at),
        };
        let stats = settled_stats.saturating_add(request_stats);
        send_progress(tx, stats);
    }

    /// Finalize the accumulated blocks into a response, applying truncation
    /// and cancellation rules and extracting pending tool calls.
    fn finish(self, stream_exit: StreamExit) -> CollectedResponse {
        let Self {
            mut blocks,
            mut thought_started_at,
            first_output_at,
            stop_reason,
            usage,
            open_block,
        } = self;
        if matches!(stream_exit, StreamExit::Cancelled) {
            discard_open_block(&mut blocks, open_block, &mut thought_started_at);
        } else {
            finish_open_thought(&mut blocks, &mut thought_started_at);
        }

        // Defense in depth for truncated streams: the protocol adapters emit
        // `Stop(Truncated)` when a stream ends without a terminal marker, but any
        // stream that simply runs out without ever signalling a stop is treated
        // as truncated here rather than as a clean `EndTurn`.
        let exhausted = matches!(stream_exit, StreamExit::Exhausted);
        let truncated =
            exhausted && matches!(stop_reason.as_ref(), None | Some(StopReason::Truncated));
        if truncated {
            blocks.retain(|block| !matches!(block, ContentBlock::ToolCall { .. }));
        }
        let calls = if exhausted && !truncated {
            pending_tool_calls(&blocks)
        } else {
            Vec::new()
        };
        let outcome = response_action(stream_exit, calls, stop_reason);
        let message = (exhausted && !blocks.is_empty()).then(|| Message::assistant(blocks));

        CollectedResponse {
            message,
            usage,
            generation_ms: elapsed_generation_ms(first_output_at),
            outcome,
        }
    }
}

fn sat_u64(value: impl TryInto<u64>) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}

fn merge_request_usage(current: Usage, reported: ModelUsage) -> Usage {
    Usage {
        // Provider reports may split input/output across events or repeat a
        // cumulative snapshot. Reconcile one request before turns add calls.
        input_tokens: current.input_tokens.max(reported.input_tokens),
        output_tokens: current.output_tokens.max(reported.output_tokens),
        tool_calls: 0,
    }
}

fn pending_tool_calls(blocks: &[ContentBlock]) -> Vec<PendingToolCall> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some(PendingToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            ContentBlock::Text(_) | ContentBlock::Thought { .. } => None,
        })
        .collect()
}

fn elapsed_generation_ms(first_output_at: Option<Instant>) -> u64 {
    first_output_at.map_or(0, |started| sat_u64(started.elapsed().as_millis()))
}

/// Map a stream exit plus any accumulated tool calls to the turn outcome.
/// Priority is explicit: stream failure > cancellation > incomplete stream >
/// completed tool calls > normal stop. Tool calls are never executed unless
/// the stream ended cleanly.
fn response_action(
    stream_exit: StreamExit,
    calls: Vec<PendingToolCall>,
    stop_reason: Option<StopReason>,
) -> ResponseOutcome {
    match stream_exit {
        StreamExit::Failed(error) => ResponseOutcome::Failed(error),
        StreamExit::Cancelled => ResponseOutcome::Cancelled,
        StreamExit::Exhausted => match (stop_reason, calls) {
            (None | Some(StopReason::Truncated), _) => {
                ResponseOutcome::Finished(StopReason::Truncated)
            }
            (Some(_), calls) if !calls.is_empty() => ResponseOutcome::ToolCalls(calls),
            (Some(reason), _) => ResponseOutcome::Finished(reason),
        },
    }
}

/// Error classification for the retry path. Only transport-level failures are
/// retried: request/network errors, explicitly retryable upstream statuses,
/// and rate limits. Auth, request-shaping, and response-shaping errors are
/// never retried because re-issuing them cannot succeed.
const fn retryable(error: &ash_core::ProtocolError) -> bool {
    matches!(
        error,
        ash_core::ProtocolError::Request(_) | ash_core::ProtocolError::RateLimited
    ) || matches!(
        error,
        ash_core::ProtocolError::Upstream {
            status: 500 | 502 | 503 | 504 | 520..=524 | 529,
            ..
        }
    )
}

/// Exponential backoff for the `attempt`-th retry (0-based): `base * 2^attempt`,
/// capped at `max`.
fn retry_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    let base_ms = sat_u64(base.as_millis());
    let max_ms = sat_u64(max.as_millis());
    let millis = base_ms.saturating_mul(1u64 << attempt.min(20)).min(max_ms);
    Duration::from_millis(millis)
}

fn finish_open_thought(blocks: &mut [ContentBlock], started_at: &mut Option<Instant>) {
    let Some(started) = started_at.take() else {
        return;
    };
    if let Some(ContentBlock::Thought {
        elapsed_seconds, ..
    }) = blocks.last_mut()
    {
        *elapsed_seconds = started.elapsed().as_secs();
    }
}

fn discard_open_block(
    blocks: &mut Vec<ContentBlock>,
    open_block: Option<OpenResponseBlock>,
    thought_started_at: &mut Option<Instant>,
) {
    let matches_open = match (open_block, blocks.last()) {
        (Some(OpenResponseBlock::Thought), Some(ContentBlock::Thought { .. }))
        | (Some(OpenResponseBlock::Text), Some(ContentBlock::Text(_))) => true,
        (None | Some(OpenResponseBlock::Thought | OpenResponseBlock::Text), _) => false,
    };
    if matches_open {
        blocks.pop();
    }
    thought_started_at.take();
}

fn limit_tool_result(result: Result<ToolOutput, ToolError>) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(mut output) => {
            output.text = limit_tool_output(output.text);
            Ok(output)
        }
        Err(ToolError::Execution(output)) => Err(ToolError::Execution(limit_tool_output(output))),
        Err(error) => Err(error),
    }
}

fn limit_tool_output(output: String) -> String {
    if output.len() <= MAX_AGENT_OUTPUT_BYTES {
        return output;
    }
    let content_budget =
        MAX_AGENT_OUTPUT_BYTES.saturating_sub(AGENT_OUTPUT_TRUNCATION_NOTICE.len());
    let tail_budget = AGENT_OUTPUT_TAIL_BYTES.min(content_budget);
    let head_budget = content_budget.saturating_sub(tail_budget);
    let mut head_end = head_budget.min(output.len());
    while !output.is_char_boundary(head_end) {
        head_end = head_end.saturating_sub(1);
    }
    let mut tail_start = output.len().saturating_sub(tail_budget);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start = tail_start.saturating_add(1);
    }
    format!(
        "{}{}{}",
        &output[..head_end],
        AGENT_OUTPUT_TRUNCATION_NOTICE,
        &output[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use ash_core::{
        define_tool, MessageContent, ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream,
        ModelUsage, SessionId, Tool, ToolCallId, ToolContext, ToolError,
    };
    use futures::StreamExt;

    use super::*;

    struct MockModel {
        responses: Mutex<VecDeque<Vec<Result<ModelEvent, ash_core::ProtocolError>>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl MockModel {
        fn events(responses: impl IntoIterator<Item = Vec<ModelEvent>>) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|events| events.into_iter().map(Ok).collect())
                        .collect(),
                ),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl ModelClient for MockModel {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(response)))
        }
    }

    struct PendingModel;

    impl ModelClient for PendingModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    fn agent(tools: Vec<Arc<dyn Tool>>) -> Agent {
        Agent::new(ModelId::new("test-model"), tools)
            .with_max_context_tokens(200_000)
            .with_tool_timeout(Duration::from_secs(1))
    }

    async fn run(
        model: &dyn ModelClient,
        agent: &Agent,
        messages: &mut Vec<Message>,
        cancel: CancellationToken,
    ) -> EngineOutcome {
        let (tx, _rx) = mpsc::channel(64);
        run_agent_turn(
            model,
            agent,
            messages,
            SessionIdentity::root(SessionId::new()),
            tx,
            cancel,
        )
        .await
    }

    struct ConcurrentTool {
        barrier: tokio::sync::Barrier,
    }

    #[async_trait::async_trait]
    impl Tool for ConcurrentTool {
        fn name(&self) -> &'static str {
            "concurrent"
        }

        fn description(&self) -> &'static str {
            "concurrent"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"]
            })
        }

        async fn execute(
            &self,
            _context: ToolContext,
            arguments: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let value = arguments["value"]
                .as_str()
                .ok_or_else(|| ToolError::Execution("missing value".to_string()))?;
            self.barrier.wait().await;
            Ok(value.to_string().into())
        }
    }

    struct BlockingTool;

    #[async_trait::async_trait]
    impl Tool for BlockingTool {
        fn name(&self) -> &'static str {
            "blocking"
        }

        fn description(&self) -> &'static str {
            "blocking"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _context: ToolContext,
            _arguments: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            futures::future::pending().await
        }
    }

    struct SlowUnboundedTool;

    #[async_trait::async_trait]
    impl Tool for SlowUnboundedTool {
        fn name(&self) -> &'static str {
            "slow_unbounded"
        }

        fn description(&self) -> &'static str {
            "slow unbounded"
        }

        fn timeout(&self) -> ToolTimeout {
            ToolTimeout::Disabled
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            context: ToolContext,
            _arguments: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            assert!(context.deadline.is_none());
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok("done".into())
        }
    }

    fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ModelEvent {
        ModelEvent::ToolCall {
            id: ToolCallId::from_provider(id),
            name: name.to_string(),
            arguments,
        }
    }

    #[tokio::test]
    async fn tool_calls_run_concurrently_and_results_keep_provider_order() {
        let model = MockModel::events([
            vec![
                tool_call("first", "concurrent", serde_json::json!({"value": "first"})),
                tool_call(
                    "second",
                    "concurrent",
                    serde_json::json!({"value": "second"}),
                ),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]);
        let tool: Arc<dyn Tool> = Arc::new(ConcurrentTool {
            barrier: tokio::sync::Barrier::new(2),
        });
        let mut messages = vec![Message::user("run both")];

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            run(
                &model,
                &agent(vec![tool]),
                &mut messages,
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap();

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        assert_eq!(outcome.stats.usage.tool_calls, 2);
        let results = messages
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::ToolResult { result, .. } => result.as_ref().ok().cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results, ["first", "second"]);
    }

    #[tokio::test]
    async fn session_timeout_bounds_regular_tools() {
        let model = MockModel::events([
            vec![
                tool_call("call", "blocking", serde_json::json!({})),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]);
        let definition =
            agent(vec![Arc::new(BlockingTool)]).with_tool_timeout(Duration::from_millis(5));
        let mut messages = vec![Message::user("run")];

        let outcome = run(&model, &definition, &mut messages, CancellationToken::new()).await;

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        assert!(messages.iter().any(|message| {
            matches!(
                &message.content,
                MessageContent::ToolResult { result: Err(error), .. }
                    if error.contains("deadline exceeded")
            )
        }));
    }

    #[tokio::test]
    async fn disabled_tool_timeout_can_outlive_the_agent_limit() {
        let model = MockModel::events([
            vec![
                tool_call("call", "slow_unbounded", serde_json::json!({})),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
            vec![ModelEvent::Stop(StopReason::EndTurn)],
        ]);
        let definition =
            agent(vec![Arc::new(SlowUnboundedTool)]).with_tool_timeout(Duration::from_millis(1));
        let mut messages = vec![Message::user("run")];

        let outcome = run(&model, &definition, &mut messages, CancellationToken::new()).await;

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        assert!(messages.iter().any(|message| {
            matches!(
                &message.content,
                MessageContent::ToolResult { result: Ok(output), .. } if output == "done"
            )
        }));
    }

    #[tokio::test]
    async fn provider_usage_is_used_without_a_local_fallback() {
        let model = MockModel::events([vec![
            ModelEvent::Text("hello".to_string()),
            ModelEvent::Usage(ModelUsage {
                input_tokens: 120,
                output_tokens: 25,
            }),
            ModelEvent::Stop(StopReason::EndTurn),
        ]]);
        let definition = agent(Vec::new());
        let mut messages = vec![Message::user("question")];

        let outcome = run(&model, &definition, &mut messages, CancellationToken::new()).await;

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        assert_eq!(outcome.stats.usage.input_tokens, 120);
        assert_eq!(outcome.stats.usage.output_tokens, 25);
    }

    #[tokio::test]
    async fn one_turn_adds_protocol_usage_from_each_model_call() {
        let model = MockModel::events([
            vec![
                ModelEvent::Usage(ModelUsage {
                    input_tokens: 100,
                    output_tokens: 10,
                }),
                tool_call("call", "echo", serde_json::json!({})),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
            vec![
                ModelEvent::Usage(ModelUsage {
                    input_tokens: 130,
                    output_tokens: 0,
                }),
                ModelEvent::Text("done".to_string()),
                ModelEvent::Usage(ModelUsage {
                    input_tokens: 0,
                    output_tokens: 5,
                }),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
        ]);
        let echo = define_tool("echo", "echo", |_, ()| async { Ok("ok") }).unwrap();
        let mut messages = vec![Message::user("question")];

        let outcome = run(
            &model,
            &agent(vec![echo]),
            &mut messages,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        assert_eq!(
            outcome.stats.usage,
            Usage {
                input_tokens: 230,
                output_tokens: 15,
                tool_calls: 1,
            }
        );
    }

    #[tokio::test]
    async fn automatic_compaction_uses_the_shared_collector_and_protocol_usage() {
        let model = MockModel::events([
            vec![
                ModelEvent::Text("summary".to_string()),
                ModelEvent::Usage(ModelUsage {
                    input_tokens: 10,
                    output_tokens: 3,
                }),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
            vec![
                ModelEvent::Text("done".to_string()),
                ModelEvent::Usage(ModelUsage {
                    input_tokens: 20,
                    output_tokens: 4,
                }),
                ModelEvent::Stop(StopReason::EndTurn),
            ],
        ]);
        let requests = Arc::clone(&model.requests);
        let definition = agent(Vec::new())
            .with_system_prompt("system")
            .with_max_context_tokens(1_000);
        let mut messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("recent request"),
        ];

        let outcome = run(&model, &definition, &mut messages, CancellationToken::new()).await;

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].system.as_deref(),
            Some(COMPACTION_SYSTEM_PROMPT)
        );
        assert!(matches!(
            requests[1].messages.first().map(|message| &message.content),
            Some(MessageContent::Assistant(_))
        ));
        assert_eq!(outcome.stats.usage.input_tokens, 30);
        assert_eq!(outcome.stats.usage.output_tokens, 7);
    }

    #[tokio::test]
    async fn compaction_rejects_tool_calls() {
        let model = MockModel::events([vec![
            ModelEvent::Usage(ModelUsage {
                input_tokens: 7,
                output_tokens: 2,
            }),
            tool_call("call", "read", serde_json::json!({})),
            ModelEvent::Stop(StopReason::EndTurn),
        ]]);
        let definition = agent(Vec::new()).with_max_context_tokens(100);
        let messages = vec![
            Message::user(&format!("old {}", "x".repeat(12_000))),
            Message::assistant_text("old answer"),
            Message::user("recent"),
        ];

        let (result, stats) =
            compact_context(&definition, &messages, &model, &CancellationToken::new()).await;

        assert!(result.unwrap_err().to_string().contains("requested a tool"));
        assert_eq!(stats.usage.tool_calls, 0);
        assert_eq!(stats.usage.input_tokens, 7);
        assert_eq!(stats.usage.output_tokens, 2);
    }

    #[tokio::test]
    async fn compaction_rejects_an_empty_summary() {
        let model = MockModel::events([vec![ModelEvent::Stop(StopReason::EndTurn)]]);
        let definition = agent(Vec::new()).with_max_context_tokens(100);
        let messages = vec![
            Message::user(&format!("old {}", "x".repeat(12_000))),
            Message::assistant_text("old answer"),
            Message::user("recent"),
        ];

        let (result, _) =
            compact_context(&definition, &messages, &model, &CancellationToken::new()).await;

        assert!(result.unwrap_err().to_string().contains("empty summary"));
    }

    #[tokio::test]
    async fn compaction_rejects_a_truncated_summary() {
        let model = MockModel::events([vec![ModelEvent::Text("partial".to_string())]]);
        let definition = agent(Vec::new()).with_max_context_tokens(100);
        let messages = vec![
            Message::user(&format!("old {}", "x".repeat(12_000))),
            Message::assistant_text("old answer"),
            Message::user("recent"),
        ];

        let (result, _) =
            compact_context(&definition, &messages, &model, &CancellationToken::new()).await;

        assert!(result.unwrap_err().to_string().contains("Truncated"));
    }

    #[tokio::test]
    async fn compaction_honors_cancellation() {
        let definition = agent(Vec::new()).with_max_context_tokens(100);
        let messages = vec![
            Message::user(&format!("old {}", "x".repeat(12_000))),
            Message::assistant_text("old answer"),
            Message::user("recent"),
        ];
        let cancel = CancellationToken::new();
        cancel.cancel();

        let (result, _) = compact_context(&definition, &messages, &PendingModel, &cancel).await;

        assert!(matches!(result, Err(ash_core::AshError::Cancelled)));
    }

    #[tokio::test]
    async fn compaction_discards_a_summary_that_does_not_reduce_context() {
        let model = MockModel::events([vec![
            ModelEvent::Text("z".repeat(20_000)),
            ModelEvent::Usage(ModelUsage {
                input_tokens: 8,
                output_tokens: 5_000,
            }),
            ModelEvent::Stop(StopReason::EndTurn),
        ]]);
        let definition = agent(Vec::new()).with_max_context_tokens(100);
        let messages = vec![
            Message::user(&format!("old {}", "x".repeat(12_000))),
            Message::assistant_text("old answer"),
            Message::user("recent"),
        ];

        let (result, stats) =
            compact_context(&definition, &messages, &model, &CancellationToken::new()).await;

        assert!(result.unwrap().is_none());
        assert_eq!(stats.usage.output_tokens, 5_000);
    }

    #[tokio::test]
    async fn unfinished_stream_keeps_text_and_discards_tool_calls() {
        let mut stream: ModelStream = Box::pin(futures::stream::iter([
            Ok(ModelEvent::Text("partial".to_string())),
            Ok(tool_call("call", "read", serde_json::json!({}))),
        ]));
        let (tx, _rx) = mpsc::channel(8);

        let response = collect_response(
            &mut stream,
            Some(&tx),
            &CancellationToken::new(),
            TurnStats::default(),
        )
        .await;

        let message = response.message.unwrap();
        assert_eq!(
            message.content,
            MessageContent::Assistant(vec![ContentBlock::Text("partial".to_string())])
        );
        assert!(matches!(
            response.outcome,
            ResponseOutcome::Finished(StopReason::Truncated)
        ));
    }

    #[tokio::test]
    async fn turn_progress_is_published_when_protocol_usage_changes() {
        let mut stream: ModelStream = Box::pin(futures::stream::iter([
            Ok(ModelEvent::Text("answer".to_string())),
            Ok(ModelEvent::Usage(ModelUsage {
                input_tokens: 100,
                output_tokens: 0,
            })),
            Ok(ModelEvent::Usage(ModelUsage {
                input_tokens: 100,
                output_tokens: 0,
            })),
            Ok(ModelEvent::Usage(ModelUsage {
                input_tokens: 0,
                output_tokens: 25,
            })),
            Ok(ModelEvent::Stop(StopReason::EndTurn)),
        ]));
        let (tx, mut rx) = mpsc::channel(16);
        let settled = TurnStats {
            usage: Usage {
                input_tokens: 7,
                output_tokens: 3,
                tool_calls: 2,
            },
            generation_ms: 10,
        };

        let response =
            collect_response(&mut stream, Some(&tx), &CancellationToken::new(), settled).await;

        drop(tx);
        let mut progress = Vec::new();
        while let Some(event) = rx.recv().await {
            if let SessionEventKind::TurnProgress(stats) = event {
                progress.push(stats.usage);
            }
        }

        assert!(matches!(
            response.outcome,
            ResponseOutcome::Finished(StopReason::EndTurn)
        ));
        assert_eq!(
            progress,
            [
                Usage {
                    input_tokens: 107,
                    output_tokens: 3,
                    tool_calls: 2,
                },
                Usage {
                    input_tokens: 107,
                    output_tokens: 28,
                    tool_calls: 2,
                },
            ]
        );
    }

    #[tokio::test]
    async fn truncated_stop_keeps_text_and_discards_tool_calls() {
        let mut stream: ModelStream = Box::pin(futures::stream::iter([
            Ok(ModelEvent::Text("partial".to_string())),
            Ok(tool_call("call", "read", serde_json::json!({}))),
            Ok(ModelEvent::Stop(StopReason::Truncated)),
        ]));

        let response = collect_response(
            &mut stream,
            None,
            &CancellationToken::new(),
            TurnStats::default(),
        )
        .await;

        let message = response.message.unwrap();
        assert_eq!(
            message.content,
            MessageContent::Assistant(vec![ContentBlock::Text("partial".to_string())])
        );
        assert!(matches!(
            response.outcome,
            ResponseOutcome::Finished(StopReason::Truncated)
        ));
    }

    #[tokio::test]
    async fn cancellation_discards_the_unfinished_response() {
        let mut stream: ModelStream = Box::pin(
            futures::stream::iter([Ok(ModelEvent::Text("partial".to_string()))])
                .chain(futures::stream::pending()),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let waiting = collect_response(&mut stream, Some(&tx), &cancel, TurnStats::default());
        tokio::pin!(waiting);

        tokio::select! {
            _ = rx.recv() => cancel.cancel(),
            _ = &mut waiting => panic!("collector completed before cancellation"),
        }
        let response = waiting.await;

        assert!(response.message.is_none());
        assert!(matches!(response.outcome, ResponseOutcome::Cancelled));
    }

    #[tokio::test]
    async fn retryable_transport_failure_is_retried() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = MockModel {
            responses: Mutex::new(VecDeque::from([
                vec![Err(ash_core::ProtocolError::Request("reset".to_string()))],
                vec![Ok(ModelEvent::Stop(StopReason::EndTurn))],
            ])),
            requests: Arc::clone(&requests),
        };
        let definition = agent(Vec::new());
        let mut messages = vec![Message::user("question")];

        let outcome = run(&model, &definition, &mut messages, CancellationToken::new()).await;

        assert_eq!(outcome.result.unwrap(), StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancellation_stops_a_tool_without_an_internal_timeout() {
        let model = MockModel::events([vec![
            tool_call("call", "blocking", serde_json::json!({})),
            ModelEvent::Stop(StopReason::EndTurn),
        ]]);
        let definition = agent(vec![Arc::new(BlockingTool)]).with_tool_timeout(Duration::MAX);
        let mut messages = vec![Message::user("run")];
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = run(&model, &definition, &mut messages, cancel).await;

        assert_eq!(outcome.result.unwrap(), StopReason::Aborted);
        assert_eq!(outcome.stats.usage.tool_calls, 0);
    }

    #[test]
    fn response_action_requires_a_complete_stream_before_tool_execution() {
        let call = PendingToolCall {
            id: ToolCallId::from_provider("call"),
            name: "read".to_string(),
            arguments: serde_json::json!({}),
        };
        assert!(matches!(
            response_action(
                StreamExit::Exhausted,
                vec![call.clone()],
                Some(StopReason::EndTurn),
            ),
            ResponseOutcome::ToolCalls(_)
        ));
        assert!(matches!(
            response_action(StreamExit::Exhausted, vec![call], None),
            ResponseOutcome::Finished(StopReason::Truncated)
        ));
        assert!(matches!(
            response_action(
                StreamExit::Exhausted,
                vec![PendingToolCall {
                    id: ToolCallId::from_provider("truncated"),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                }],
                Some(StopReason::Truncated),
            ),
            ResponseOutcome::Finished(StopReason::Truncated)
        ));
        assert!(matches!(
            response_action(StreamExit::Cancelled, Vec::new(), None),
            ResponseOutcome::Cancelled
        ));
    }

    #[test]
    fn truncates_large_tool_output_without_splitting_utf8() {
        let source = "a".repeat(MAX_AGENT_OUTPUT_BYTES) + "中文🙂tail";
        let truncated = limit_tool_output(source);
        assert!(truncated.contains(AGENT_OUTPUT_TRUNCATION_NOTICE));
        assert!(truncated.ends_with("中文🙂tail"));
        assert!(truncated.len() <= MAX_AGENT_OUTPUT_BYTES + AGENT_OUTPUT_TRUNCATION_NOTICE.len());
    }

    #[test]
    fn retry_delay_grows_exponentially_and_caps_at_max() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(10);
        assert_eq!(retry_delay(0, base, max), Duration::from_secs(1));
        assert_eq!(retry_delay(3, base, max), Duration::from_secs(8));
        assert_eq!(retry_delay(9, base, max), Duration::from_secs(10));
    }

    #[test]
    fn retries_only_explicitly_allowed_upstream_statuses() {
        assert!(retryable(&ash_core::ProtocolError::RateLimited));
        assert!(retryable(&ash_core::ProtocolError::Upstream {
            status: 503,
            message: "unavailable".to_string(),
        }));
        assert!(!retryable(&ash_core::ProtocolError::Auth));
        assert!(!retryable(&ash_core::ProtocolError::Upstream {
            status: 400,
            message: "bad request".to_string(),
        }));
    }
}
