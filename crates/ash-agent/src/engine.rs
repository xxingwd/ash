use ash_core::{
    CancellationToken, ContentBlock, LiveEvent, Message, ModelClient, ModelEvent, ModelRequest,
    ModelStream, SessionEventKind, SessionId, SessionToolContext, StopReason, ToolCallId,
    ToolContext, ToolDefinition, ToolError, ToolOutput, TurnId, Usage,
};
use futures::StreamExt;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

use crate::agent::RetryBackoff;
use crate::store::SessionPersistence;
use crate::{
    context::count_output_tokens,
    context_policy::{ContextRequest, DefaultContextPolicy},
    log::{ContextCheckpoint, LogEntry},
    AcceptedInput, Input, RunConfig,
};

const MAX_AGENT_OUTPUT_BYTES: usize = 64 * 1024;
const AGENT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const AGENT_OUTPUT_TRUNCATION_NOTICE: &str = "\n... tool output truncated by the agent ...\n";

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
    /// Rolled back and ready to re-issue the request.
    Retry,
    /// Cancelled during the backoff; the turn must abort.
    Abort,
}

struct CollectedResponse {
    message: Option<Message>,
    usage: UsageAccumulator,
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

pub struct CompactedHistory {
    pub(crate) messages: Vec<Message>,
    pub(crate) before_tokens: usize,
    pub(crate) after_tokens: usize,
    pub(crate) dropped_messages: usize,
}

#[derive(Default)]
struct UsageAccumulator {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    generation_ms: u64,
}

struct AgentTurnRunner<'config, 'store> {
    config: &'config RunConfig,
    tx: mpsc::Sender<SessionEventKind>,
    cancel: CancellationToken,
    ids: ExecutionIds,
    model: &'config dyn ModelClient,
    persistence: Option<&'store mut SessionPersistence>,
    steering: mpsc::UnboundedReceiver<Input>,
    ephemeral_context: Vec<Message>,
    tool_defs: Vec<ToolDefinition>,
}

#[derive(Clone, Copy)]
pub struct ExecutionIds {
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

pub struct TurnExecution {
    ids: ExecutionIds,
    tx: mpsc::Sender<SessionEventKind>,
    cancel: CancellationToken,
    steering: mpsc::UnboundedReceiver<Input>,
    ephemeral_context: Vec<Message>,
}

impl ExecutionIds {
    const fn new(session_id: SessionId, turn_id: TurnId) -> Self {
        Self {
            session_id,
            turn_id,
        }
    }
}

impl TurnExecution {
    pub(crate) const fn new(
        session_id: SessionId,
        turn_id: TurnId,
        tx: mpsc::Sender<SessionEventKind>,
        cancel: CancellationToken,
        steering: mpsc::UnboundedReceiver<Input>,
        ephemeral_context: Vec<Message>,
    ) -> Self {
        Self {
            ids: ExecutionIds::new(session_id, turn_id),
            tx,
            cancel,
            steering,
            ephemeral_context,
        }
    }
}

impl UsageAccumulator {
    fn record(&mut self, input_tokens: u64, output_tokens: u64) {
        if input_tokens > 0 {
            self.input_tokens = Some(
                self.input_tokens
                    .map_or(input_tokens, |current| current.max(input_tokens)),
            );
        }
        if output_tokens > 0 {
            self.output_tokens = Some(
                self.output_tokens
                    .map_or(output_tokens, |current| current.max(output_tokens)),
            );
        }
    }

    fn finalize(self, input_tokens: u64, output_tokens: u64) -> Usage {
        let estimated =
            self.input_tokens.is_none() || (self.output_tokens.is_none() && output_tokens > 0);
        Usage {
            input_tokens: self.input_tokens.unwrap_or(input_tokens),
            output_tokens: self.output_tokens.unwrap_or(output_tokens),
            generation_ms: self.generation_ms,
            estimated,
        }
    }
}

pub async fn compact_with_adapter(
    config: &RunConfig,
    messages: &[Message],
    model: &dyn ModelClient,
    cancel: &CancellationToken,
) -> Result<Option<CompactedHistory>, ash_core::AshError> {
    let tools = config.tool_definitions();
    let update = DefaultContextPolicy
        .compact(
            ContextRequest {
                model: config.model.clone(),
                system_prompt: config.system_prompt.clone(),
                messages: messages.to_vec(),
                ephemeral_context: Vec::new(),
                tools,
                max_context_tokens: config.max_context_tokens,
            },
            model,
            cancel,
        )
        .await?;
    Ok(update.map(|compacted| CompactedHistory {
        messages: compacted.messages,
        before_tokens: compacted.update.before_tokens,
        after_tokens: compacted.update.after_tokens,
        dropped_messages: compacted.update.dropped_messages,
    }))
}

pub async fn run_agent_turn_persisted(
    model: &dyn ModelClient,
    config: &RunConfig,
    messages: &mut Vec<Message>,
    execution: TurnExecution,
    persistence: &mut SessionPersistence,
) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
    run_agent_turn_inner(model, config, messages, execution, Some(persistence)).await
}

async fn run_agent_turn_inner(
    model: &dyn ModelClient,
    config: &RunConfig,
    messages: &mut Vec<Message>,
    execution: TurnExecution,
    persistence: Option<&mut SessionPersistence>,
) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
    AgentTurnRunner::new(config, model, persistence, execution)
        .run(messages)
        .await
}

impl<'config, 'store> AgentTurnRunner<'config, 'store> {
    fn new(
        config: &'config RunConfig,
        model: &'config dyn ModelClient,
        persistence: Option<&'store mut SessionPersistence>,
        execution: TurnExecution,
    ) -> Self {
        let tool_defs = config.tool_definitions();
        Self {
            config,
            tx: execution.tx,
            cancel: execution.cancel,
            ids: execution.ids,
            model,
            persistence,
            steering: execution.steering,
            ephemeral_context: execution.ephemeral_context,
            tool_defs,
        }
    }

    async fn run(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
        let mut turn_usage: Option<Usage> = None;
        for turn in 0..self.config.max_turns {
            if self.cancel.is_cancelled() {
                return Ok((StopReason::Aborted, turn_usage));
            }
            debug!(turn = turn + 1, "calling LLM");
            if let Step::Done(reason) = self.step(messages, &mut turn_usage).await? {
                return Ok((reason, turn_usage));
            }
        }

        Ok((StopReason::MaxTurns, turn_usage))
    }

    /// Run one model call plus its follow-up work (tool execution, steering).
    /// Returns `Step::More` to continue the turn or `Step::Done` to stop it.
    ///
    /// Safe failures (network errors, upstream 5xx, rate limits, truncated
    /// streams) are retried up to `config.max_retries` times. A retry is only
    /// taken before any tool call was executed: the partial output is
    /// discarded (both from memory and from the staged log) and the identical
    /// request is re-issued, so retries never repeat side effects.
    async fn step(
        &mut self,
        messages: &mut Vec<Message>,
        turn_usage: &mut Option<Usage>,
    ) -> Result<Step, ash_core::AshError> {
        let mut retries = 0u32;
        loop {
            let estimated_input_tokens = self.prepare_context(messages).await?;
            // Context preparation may stage a durable checkpoint. A retry must
            // retain it and discard only records produced by the model attempt.
            let pending_base = self.persistence.as_ref().map(|p| p.pending_len());
            // Snapshot after context preparation (compaction may have
            // reshaped `messages`); truncating to this length on retry
            // removes only the partial assistant message pushed below.
            let rollback_messages_len = messages.len();

            let request_messages = self.request_messages(messages);
            let request_started = Instant::now();
            let mut stream = self.model.stream(ModelRequest {
                model: self.config.model.clone(),
                system: self.config.system_prompt.clone(),
                messages: request_messages,
                tools: self.tool_defs.clone(),
                max_tokens: None,
            })?;
            let CollectedResponse {
                message,
                usage,
                outcome,
            } = collect_response(&mut stream, &self.tx, &self.cancel, request_started).await;

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
            match self
                .maybe_retry(
                    messages,
                    pending_base,
                    rollback_messages_len,
                    &mut retries,
                    retry,
                )
                .await
            {
                Some(RetryAction::Abort) => return Ok(Step::Done(StopReason::Aborted)),
                Some(RetryAction::Retry) => continue,
                None => {}
            }

            let estimated_output_tokens = message
                .as_ref()
                .map(count_output_tokens)
                .and_then(|tokens| u64::try_from(tokens).ok())
                .unwrap_or(0);
            let call_usage = usage.finalize(
                u64::try_from(estimated_input_tokens).unwrap_or(u64::MAX),
                estimated_output_tokens,
            );
            merge_turn_usage(turn_usage, call_usage);
            if let Some(message) = message {
                self.persist_message(&message)?;
                messages.push(message);
            }

            return self.finish_outcome(messages, outcome).await;
        }
    }

    /// Retry a safe, retryable model failure before any tool side effect. On
    /// retry the partial output is discarded from both memory and the staged
    /// log, and the identical request is re-issued. Cancellation during the
    /// backoff aborts the turn.
    async fn maybe_retry(
        &mut self,
        messages: &mut Vec<Message>,
        pending_base: Option<usize>,
        rollback_messages_len: usize,
        retries: &mut u32,
        retry: bool,
    ) -> Option<RetryAction> {
        if !retry || *retries >= self.config.max_retries {
            return None;
        }
        let delay = retry_delay(*retries, self.config.retry_backoff);
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
        if let (Some(base), Some(persistence)) = (pending_base, self.persistence.as_deref_mut()) {
            persistence.rollback_to(base);
        }
        messages.truncate(rollback_messages_len);
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
            ResponseOutcome::Cancelled => {
                // Drain steering so queued input is not lost, then abort.
                let _ = self.apply_steering(messages);
                Ok(Step::Done(StopReason::Aborted))
            }
            ResponseOutcome::ToolCalls(calls) => {
                self.execute_tool_calls(messages, calls).await?;
                if self.cancel.is_cancelled() {
                    return Ok(Step::Done(StopReason::Aborted));
                }
                let _ = self.apply_steering(messages);
                Ok(Step::More)
            }
            ResponseOutcome::Finished(reason) => {
                if self.apply_steering(messages) {
                    return Ok(Step::More);
                }
                Ok(Step::Done(reason))
            }
        }
    }

    async fn prepare_context(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<usize, ash_core::AshError> {
        let prepared = self
            .config
            .context_policy
            .prepare(
                ContextRequest {
                    model: self.config.model.clone(),
                    system_prompt: self.config.system_prompt.clone(),
                    messages: std::mem::take(messages),
                    ephemeral_context: std::mem::take(&mut self.ephemeral_context),
                    tools: self.tool_defs.clone(),
                    max_context_tokens: self.config.max_context_tokens,
                },
                self.model,
                &self.cancel,
            )
            .await?;
        let estimated_input_tokens = prepared.estimated_input_tokens;
        let update = prepared.update;
        *messages = prepared.messages;
        self.ephemeral_context = prepared.ephemeral_context;
        let Some(update) = update else {
            return Ok(estimated_input_tokens);
        };
        if let Some(persistence) = self.persistence.as_deref_mut() {
            persistence.stage(
                &[LogEntry::Checkpoint(ContextCheckpoint::from_model_context(
                    messages,
                )?)],
            );
        }
        let _ = self
            .tx
            .send(SessionEventKind::ContextCompacted {
                before: u64::try_from(update.before_tokens).unwrap_or(u64::MAX),
                after: u64::try_from(update.after_tokens).unwrap_or(u64::MAX),
                dropped: u64::try_from(update.dropped_messages).unwrap_or(u64::MAX),
            })
            .await;
        Ok(estimated_input_tokens)
    }

    fn persist_message(&mut self, message: &Message) -> Result<(), ash_core::AshError> {
        self.persistence
            .as_deref_mut()
            .map_or(Ok(()), |persistence| {
                persistence.stage(&[LogEntry::Message(message.clone())]);
                Ok(())
            })
    }

    fn apply_steering(&mut self, messages: &mut Vec<Message>) -> bool {
        let mut accepted = Vec::new();
        while let Ok(input) = self.steering.try_recv() {
            if input.is_empty() {
                continue;
            }
            let message = Message::user_content(input.content.clone());
            accepted.push((input, message));
        }
        if accepted.is_empty() {
            return false;
        }
        if let Some(persistence) = self.persistence.as_deref_mut() {
            let records = accepted
                .iter()
                .map(|(input, message)| {
                    LogEntry::Input(AcceptedInput {
                        turn_id: self.ids.turn_id,
                        input: input.clone(),
                        message: message.clone(),
                    })
                })
                .collect::<Vec<_>>();
            persistence.stage(&records);
        }
        messages.extend(accepted.into_iter().map(|(_, message)| message));
        true
    }

    async fn execute_tool_calls(
        &mut self,
        messages: &mut Vec<Message>,
        calls: Vec<PendingToolCall>,
    ) -> Result<(), ash_core::AshError> {
        for call in calls {
            send_live(
                &self.tx,
                SessionEventKind::Live(LiveEvent::ToolStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                }),
            )
            .await;
            let result = limit_tool_result(
                self.execute_tool(messages, &call.name, call.arguments.clone())
                    .await,
            );
            let (output, is_error, result, attachments) = match result {
                Ok(output) => {
                    let ToolOutput { text, attachments } = output;
                    (text.clone(), false, Ok(text), attachments)
                }
                Err(error) => {
                    let text = error.to_string();
                    (text.clone(), true, Err(text), Vec::new())
                }
            };
            send_live(
                &self.tx,
                SessionEventKind::Live(LiveEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name,
                    arguments: call.arguments,
                    output,
                    is_error,
                }),
            )
            .await;
            let message = Message::tool_result(call.id, result, attachments);
            self.persist_message(&message)?;
            messages.push(message);
        }
        Ok(())
    }

    async fn execute_tool(
        &self,
        messages: &[Message],
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let Some(tool) = self.config.tools.iter().find(|tool| tool.name() == name) else {
            return Err(ToolError::Execution(format!("unknown tool: {name}")));
        };
        let context = ToolContext {
            session_id: self.ids.session_id,
            turn_id: self.ids.turn_id,
            cancellation: self.cancel.clone(),
            deadline: Instant::now() + self.config.max_tool_duration,
            session: SessionToolContext {
                identity: self.config.identity.clone(),
                messages: self.request_messages(messages),
            },
        };

        tokio::select! {
            () = self.cancel.cancelled() => Err(ToolError::Cancelled),
            result = tokio::time::timeout(
                self.config.max_tool_duration,
                tool.execute(context, arguments),
            ) => result.unwrap_or(Err(ToolError::Timeout(self.config.max_tool_duration)))
        }
    }

    fn request_messages(&self, messages: &[Message]) -> Vec<Message> {
        let mut request = Vec::with_capacity(messages.len() + self.ephemeral_context.len());
        request.extend_from_slice(messages);
        request.extend_from_slice(&self.ephemeral_context);
        request
    }
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

async fn collect_response(
    stream: &mut ModelStream,
    tx: &mpsc::Sender<SessionEventKind>,
    cancel: &CancellationToken,
    request_started: Instant,
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
        accumulator.apply(tx, item).await;
    };
    accumulator.finish(stream_exit, request_started)
}

/// Incremental state accumulated while reading one model stream.
struct ResponseAccumulator {
    blocks: Vec<ContentBlock>,
    thought_started_at: Option<Instant>,
    first_output_at: Option<Instant>,
    stop_reason: StopReason,
    saw_stop: bool,
    usage: UsageAccumulator,
    open_block: Option<OpenResponseBlock>,
}

impl ResponseAccumulator {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            thought_started_at: None,
            first_output_at: None,
            stop_reason: StopReason::EndTurn,
            saw_stop: false,
            usage: UsageAccumulator::default(),
            open_block: None,
        }
    }

    /// Fold one stream item into the accumulator, forwarding live deltas.
    async fn apply(&mut self, tx: &mpsc::Sender<SessionEventKind>, item: ModelEvent) {
        match item {
            ModelEvent::Text(delta) => {
                self.first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut self.blocks, &mut self.thought_started_at);
                match self.blocks.last_mut() {
                    Some(ContentBlock::Text(text)) => text.push_str(&delta),
                    _ => self.blocks.push(ContentBlock::Text(delta.clone())),
                }
                self.open_block = Some(OpenResponseBlock::Text);
                send_live(tx, SessionEventKind::Live(LiveEvent::TextDelta(delta))).await;
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
                send_live(tx, SessionEventKind::Live(LiveEvent::ReasoningDelta(delta))).await;
            }
            ModelEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut self.blocks, &mut self.thought_started_at);
                self.open_block = None;
                self.blocks.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            ModelEvent::Usage(reported) => self
                .usage
                .record(reported.input_tokens, reported.output_tokens),
            ModelEvent::Stop(reason) => {
                finish_open_thought(&mut self.blocks, &mut self.thought_started_at);
                self.open_block = None;
                self.saw_stop = true;
                self.stop_reason = reason;
            }
        }
    }

    /// Finalize the accumulated blocks into a response, applying truncation
    /// and cancellation rules and extracting pending tool calls.
    fn finish(self, stream_exit: StreamExit, request_started: Instant) -> CollectedResponse {
        let Self {
            mut blocks,
            mut thought_started_at,
            first_output_at,
            mut stop_reason,
            saw_stop,
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
        if matches!(stream_exit, StreamExit::Exhausted) && !saw_stop {
            stop_reason = StopReason::Truncated;
        }

        let calls = pending_tool_calls(&blocks);
        let message = (!blocks.is_empty()).then(|| Message::assistant(blocks));
        let outcome = response_action(stream_exit, calls, stop_reason);

        CollectedResponse {
            message,
            usage: UsageAccumulator {
                generation_ms: elapsed_generation_ms(first_output_at, request_started),
                ..usage
            },
            outcome,
        }
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

fn elapsed_generation_ms(first_output_at: Option<Instant>, request_started: Instant) -> u64 {
    u64::try_from(
        first_output_at
            .unwrap_or(request_started)
            .elapsed()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Map a stream exit plus any accumulated tool calls to the turn outcome.
/// Priority is explicit: stream failure > pending tool calls > cancellation >
/// normal stop. A cancellation that raced with tool calls still executes the
/// calls (the model already asked for them); the call results are what the
/// next step runs on.
fn response_action(
    stream_exit: StreamExit,
    calls: Vec<PendingToolCall>,
    stop_reason: StopReason,
) -> ResponseOutcome {
    match (stream_exit, calls.is_empty()) {
        (StreamExit::Failed(error), _) => ResponseOutcome::Failed(error),
        (_, false) => ResponseOutcome::ToolCalls(calls),
        (StreamExit::Cancelled, true) => ResponseOutcome::Cancelled,
        (StreamExit::Exhausted, true) => ResponseOutcome::Finished(stop_reason),
    }
}

/// Error classification for the retry path. Only transport-level failures are
/// retried: request/network errors, upstream 5xx, and rate limits. Auth,
/// request-shaping, and response-shaping errors are never retried because
/// re-issuing them cannot succeed.
const fn retryable(error: &ash_core::ProtocolError) -> bool {
    matches!(
        error,
        ash_core::ProtocolError::Request(_) | ash_core::ProtocolError::RateLimited
    ) || matches!(
        error,
        ash_core::ProtocolError::Upstream { status, .. } if *status >= 500
    )
}

/// Exponential backoff for the `attempt`-th retry (0-based): `base * 2^attempt`,
/// capped at `max`.
fn retry_delay(attempt: u32, backoff: RetryBackoff) -> std::time::Duration {
    let base_ms = u64::try_from(backoff.base.as_millis()).unwrap_or(u64::MAX);
    let max_ms = u64::try_from(backoff.max.as_millis()).unwrap_or(u64::MAX);
    let millis = base_ms.saturating_mul(1u64 << attempt.min(20)).min(max_ms);
    std::time::Duration::from_millis(millis)
}

/// Merge a single model call's usage into the running turn total. The API
/// reports the full model input (system prompt, tools, and history) per
/// request, so later calls in a turn already include earlier ones: take the
/// latest snapshot instead of summing deltas. Output tokens are per-call
/// increments and do accumulate across the turn.
fn merge_turn_usage(turn_usage: &mut Option<Usage>, call_usage: Usage) {
    *turn_usage = Some(turn_usage.map_or(call_usage, |acc| Usage {
        input_tokens: acc.input_tokens.max(call_usage.input_tokens),
        output_tokens: acc.output_tokens.saturating_add(call_usage.output_tokens),
        generation_ms: acc.generation_ms.saturating_add(call_usage.generation_ms),
        estimated: acc.estimated || call_usage.estimated,
    }));
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
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use ash_core::{
        Content, MessageContent, ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, Tool,
        ToolCallId, ToolContext, ToolError,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::context_policy::COMPACTION_SYSTEM_PROMPT;
    use crate::{JsonlSessionStore, SharedSessionStore, StoredSession};
    use ash_core::SessionIdentity;

    async fn run_with_adapter(
        config: &RunConfig,
        messages: &mut Vec<Message>,
        tx: mpsc::Sender<SessionEventKind>,
        cancel: CancellationToken,
        session_id: SessionId,
        model: &dyn ModelClient,
        persistence: Option<&mut SessionPersistence>,
    ) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
        let (_, steering) = mpsc::unbounded_channel();
        let execution =
            TurnExecution::new(session_id, TurnId::new(), tx, cancel, steering, Vec::new());
        run_agent_turn_inner(model, config, messages, execution, persistence).await
    }

    fn metadata(session_id: SessionId) -> SessionIdentity {
        SessionIdentity::root(session_id)
    }

    async fn persisted_session(
        directory: &std::path::Path,
        messages: &[Message],
    ) -> (SessionId, SharedSessionStore, SessionPersistence) {
        let session_id = SessionId::new();
        let store: SharedSessionStore = Arc::new(JsonlSessionStore::new(directory));
        let records = messages
            .iter()
            .cloned()
            .map(LogEntry::Message)
            .collect::<Vec<_>>();
        store.create(metadata(session_id), &records).await.unwrap();
        let opened = store.open(session_id).await.unwrap().unwrap();
        let writer = Arc::new(tokio::sync::Mutex::new(opened.writer));
        let persistence = SessionPersistence::new(writer);
        (session_id, store, persistence)
    }

    async fn load_session(store: &SharedSessionStore, session_id: SessionId) -> StoredSession {
        store.load(session_id).await.unwrap().unwrap()
    }

    struct MockAdapter {
        responses: Mutex<VecDeque<Vec<ModelEvent>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl ModelClient for MockAdapter {
        fn stream(&self, req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(req);
            let items = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(items.into_iter().map(Ok))))
        }
    }

    /// A client whose stream fails immediately with a fixed protocol error.
    struct FailingAdapter(ash_core::ProtocolError);

    impl ModelClient for FailingAdapter {
        fn stream(&self, _req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
            let error = self.0.clone();
            Ok(Box::pin(futures::stream::iter(vec![Err(error)])))
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> &'static str {
            "echo"
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
            arguments["value"]
                .as_str()
                .map(ToOwned::to_owned)
                .map(Into::into)
                .ok_or_else(|| ToolError::Execution("missing value".into()))
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

    #[tokio::test]
    async fn preserves_full_history_across_tool_turns() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![
                    ModelEvent::ToolCall {
                        id: ToolCallId::from_provider("call_1"),
                        name: "echo".into(),
                        arguments: serde_json::json!({"value": "hello"}),
                    },
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
                vec![
                    ModelEvent::Text("done".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: vec![Arc::new(EchoTool)],
            model: ModelId::new("test-model"),
            max_turns: 4,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("use a tool")];
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(32);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            session_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.commit().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap()[1].messages.len(), 3);
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            messages[2].content,
            MessageContent::ToolResult { .. }
        ));
        let persisted = load_session(&store, session_id).await;
        assert_eq!(persisted.log.messages().len(), 4);
        assert!(matches!(
            persisted.log.messages()[2].content,
            MessageContent::ToolResult { .. }
        ));
    }

    #[tokio::test]
    async fn automatically_compacts_at_eighty_percent_before_the_model_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![
                    ModelEvent::Text("condensed facts".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
                vec![
                    ModelEvent::Text("done".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 1_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("recent one"),
            Message::assistant_text("answer one"),
            Message::user("recent two"),
            Message::assistant_text("answer two"),
        ];
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, mut rx) = mpsc::channel(16);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            session_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.commit().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        let stored = load_session(&store, session_id).await;
        assert_eq!(stored.log.messages().len(), 7);
        assert_eq!(stored.log.model_context().len(), 6);
        assert!(matches!(
            &stored.log.messages()[0].content,
            MessageContent::User(contents)
                if matches!(contents.as_slice(), [Content::Text(text)] if text.starts_with("old request"))
        ));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].tools.is_empty());
            assert_eq!(
                requests[0].system.as_deref(),
                Some(COMPACTION_SYSTEM_PROMPT)
            );
            assert_eq!(requests[1].messages.len(), 5);
            assert!(matches!(
                &requests[1].messages[0].content,
                MessageContent::Assistant(blocks)
                    if matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text.contains("<context-summary>"))
            ));
            drop(requests);
        }
        assert!(std::iter::from_fn(|| rx.try_recv().ok())
            .any(|event| matches!(event, SessionEventKind::ContextCompacted { dropped: 2, .. })));
    }

    #[tokio::test]
    async fn retry_after_automatic_compaction_keeps_the_checkpoint() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![
                    ModelEvent::Text("condensed facts".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
                vec![ModelEvent::Text("partial".into())],
                vec![
                    ModelEvent::Text("complete".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 1_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 1,
            retry_backoff: RetryBackoff {
                base: Duration::from_millis(1),
                max: Duration::from_millis(10),
            },
        };
        let mut messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("recent one"),
            Message::assistant_text("answer one"),
            Message::user("recent two"),
            Message::assistant_text("answer two"),
        ];
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(16);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            session_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.commit().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap().len(), 3);
        let stored = load_session(&store, session_id).await;
        assert_eq!(stored.log.model_context(), messages);
        assert_eq!(stored.log.messages().len(), 7);
        let persisted = serde_json::to_string(&stored.log).unwrap();
        assert!(persisted.contains("checkpoint"));
        assert!(!persisted.contains("partial"));
    }

    #[tokio::test]
    async fn automatic_compaction_never_persists_ephemeral_context() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![
                    ModelEvent::Text("condensed facts".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
                vec![
                    ModelEvent::Text("done".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 1_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("recent one"),
            Message::assistant_text("answer one"),
            Message::user("recent two"),
            Message::assistant_text("answer two"),
        ];
        let ephemeral = Message::system("turn-only extension context");
        let ephemeral_id = ephemeral.id;
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(16);
        let (_, steering) = mpsc::unbounded_channel();
        let execution = TurnExecution::new(
            session_id,
            TurnId::new(),
            tx,
            CancellationToken::new(),
            steering,
            vec![ephemeral],
        );

        let (reason, _) = run_agent_turn_inner(
            &adapter,
            &config,
            &mut messages,
            execution,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.commit().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(!requests[0]
                .messages
                .iter()
                .any(|message| message.id == ephemeral_id));
            assert!(requests[1]
                .messages
                .iter()
                .any(|message| message.id == ephemeral_id));
            drop(requests);
        }
        let stored = load_session(&store, session_id).await;
        assert!(!stored
            .log
            .model_context()
            .iter()
            .any(|message| message.id == ephemeral_id));
    }

    #[tokio::test]
    async fn automatic_compaction_rejects_a_stream_without_a_clean_stop() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![ModelEvent::Text(
                "partial summary".into(),
            )]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 1_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("recent"),
        ];
        let (tx, _rx) = mpsc::channel(4);

        let error = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("without a terminal marker"));
    }

    #[tokio::test]
    async fn prunes_large_old_tool_outputs_before_full_compaction() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                ModelEvent::Text("done".into()),
                ModelEvent::Stop(StopReason::EndTurn),
            ]])),
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 120_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = Vec::new();
        for turn in 0..7 {
            let call = ToolCallId::from_provider(format!("call-{turn}"));
            messages.push(Message::user(&format!("request {turn}")));
            messages.push(Message::assistant(vec![ContentBlock::ToolCall {
                id: call.clone(),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
            }]));
            messages.push(Message::tool_result(
                call,
                Ok("x".repeat(64_000)),
                Vec::new(),
            ));
            messages.push(Message::assistant_text(&format!("answer {turn}")));
        }
        let (tx, _rx) = mpsc::channel(16);

        run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].system.as_deref(), Some("system"));
        assert_eq!(
            requests[0]
                .messages
                .iter()
                .filter(|message| matches!(
                    &message.content,
                    MessageContent::ToolResult {
                        result: Ok(output),
                        ..
                    } if output == "[Old tool output cleared to reduce context]"
                ))
                .count(),
            3
        );
        drop(requests);
    }

    #[tokio::test]
    async fn aggregates_usage_for_each_model_call() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                ModelEvent::Usage(Usage {
                    input_tokens: 120,
                    output_tokens: 0,
                    generation_ms: 0,
                    estimated: false,
                }),
                ModelEvent::Reasoning("checking".into()),
                ModelEvent::Usage(Usage {
                    input_tokens: 0,
                    output_tokens: 25,
                    generation_ms: 0,
                    estimated: false,
                }),
                ModelEvent::Stop(StopReason::EndTurn),
            ]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("question")];
        let (tx, _rx) = mpsc::channel(8);

        let (reason, usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 25);
        assert!(!usage.estimated);
    }

    #[tokio::test]
    async fn multi_call_turn_keeps_the_latest_input_snapshot_and_sums_output() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![
                    ModelEvent::Usage(Usage {
                        input_tokens: 100,
                        output_tokens: 10,
                        generation_ms: 0,
                        estimated: false,
                    }),
                    ModelEvent::ToolCall {
                        id: ToolCallId::from_provider("call_1"),
                        name: "echo".into(),
                        arguments: serde_json::json!({"value": "hello"}),
                    },
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
                vec![
                    ModelEvent::Usage(Usage {
                        input_tokens: 130,
                        output_tokens: 5,
                        generation_ms: 0,
                        estimated: false,
                    }),
                    ModelEvent::Text("done".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: vec![Arc::new(EchoTool)],
            model: ModelId::new("test-model"),
            max_turns: 4,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("use a tool")];
        let (tx, _rx) = mpsc::channel(8);

        let (reason, usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        let usage = usage.unwrap();
        // The API reports the full input per request: the second call (130)
        // already includes the first call's context, so the turn keeps the
        // latest snapshot instead of summing to 230. Output is incremental
        // and sums to 15.
        assert_eq!(usage.input_tokens, 130);
        assert_eq!(usage.output_tokens, 15);
        assert!(!usage.estimated);
    }

    #[test]
    fn fills_missing_provider_usage_with_estimates() {
        let usage = UsageAccumulator::default().finalize(120, 25);

        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 25);
        assert!(usage.estimated);
    }

    #[test]
    fn combines_split_provider_usage_without_marking_it_estimated() {
        let mut usage = UsageAccumulator::default();
        usage.record(120, 0);
        usage.record(0, 25);

        let usage = usage.finalize(999, 999);

        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 25);
        assert!(!usage.estimated);
    }

    #[tokio::test]
    async fn reasoning_time_counts_towards_the_generation_window() {
        struct ReasoningThenTextAdapter;

        #[async_trait::async_trait]
        impl ModelClient for ReasoningThenTextAdapter {
            fn stream(&self, _req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
                // A 30ms thinking phase followed by a text burst. The
                // generation clock must start at the first reasoning delta:
                // provider `output_tokens` includes thinking tokens, so an
                // excluded thinking phase would inflate the tok/s rate.
                let stream = futures::stream::unfold(0u8, |step| async move {
                    match step {
                        0 => Some((Ok(ModelEvent::Reasoning("thinking".into())), 1)),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            Some((Ok(ModelEvent::Text("answer".into())), 2))
                        }
                        2 => Some((Ok(ModelEvent::Stop(StopReason::EndTurn)), 3)),
                        _ => None,
                    }
                });
                Ok(Box::pin(stream))
            }
        }

        let adapter = ReasoningThenTextAdapter;
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("question")];
        let (tx, _rx) = mpsc::channel(8);

        let (reason, usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        let usage = usage.unwrap();
        // The 30ms thinking phase must be part of the generation window;
        // starting the clock at the trailing text burst would measure ~0ms
        // and report an absurd tok/s.
        assert!(
            usage.generation_ms >= 20,
            "generation_ms = {}",
            usage.generation_ms
        );
    }

    #[tokio::test]
    async fn persists_reasoning_blocks_in_session_history() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                ModelEvent::Reasoning("inspect first".into()),
                ModelEvent::Text("done".into()),
                ModelEvent::Stop(StopReason::EndTurn),
            ]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(32);

        run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            session_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.commit().await.unwrap();

        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [
                    ContentBlock::Thought { text, .. },
                    ContentBlock::Text(answer),
                ] if text == "inspect first" && answer == "done")
        ));
        let persisted = serde_json::to_string(&load_session(&store, session_id).await.log).unwrap();
        assert!(persisted.contains("inspect first"));
        assert!(persisted.contains("elapsed_seconds"));
    }

    #[tokio::test]
    async fn cancellation_discards_partial_assistant_text() {
        struct PendingAdapter;

        impl ModelClient for PendingAdapter {
            fn stream(&self, _req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
                Ok(Box::pin(
                    futures::stream::iter([Ok(ModelEvent::Text("partial".into()))])
                        .chain(futures::stream::pending()),
                ))
            }
        }

        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("question")];
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let reason = {
            let turn = run_with_adapter(
                &config,
                &mut messages,
                tx,
                cancel.clone(),
                SessionId::new(),
                &PendingAdapter,
                None,
            );
            tokio::pin!(turn);

            let event = tokio::select! {
                event = rx.recv() => event,
                result = &mut turn => panic!("turn ended before cancellation: {result:?}"),
            };
            assert!(
                matches!(event, Some(SessionEventKind::Live(LiveEvent::TextDelta(text))) if text == "partial")
            );
            cancel.cancel();
            (&mut turn).await.unwrap().0
        };
        assert_eq!(reason, StopReason::Aborted);

        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0].content,
            MessageContent::User(content)
                if content.as_slice() == [ash_core::Content::Text("question".into())]
        ));
    }

    #[tokio::test]
    async fn cancellation_keeps_completed_blocks_before_the_open_block() {
        let mut stream: ModelStream = Box::pin(
            futures::stream::iter([
                Ok(ModelEvent::Reasoning("completed thought".into())),
                Ok(ModelEvent::Text("partial answer".into())),
            ])
            .chain(futures::stream::pending()),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let response = collect_response(&mut stream, &tx, &cancel, Instant::now());
        tokio::pin!(response);

        for expected in ["completed thought", "partial answer"] {
            let event = tokio::select! {
                event = rx.recv() => event,
                result = &mut response => panic!("response ended before cancellation: {}", result.message.is_some()),
            };
            assert!(matches!(
                            event,
                            Some(SessionEventKind::Live(LiveEvent::ReasoningDelta(text) |
            LiveEvent::TextDelta(text))) if text == expected
                        ));
        }
        cancel.cancel();
        let collected = response.await;

        assert!(matches!(collected.outcome, ResponseOutcome::Cancelled));
        assert!(matches!(
            collected.message.map(|message| message.content),
            Some(MessageContent::Assistant(blocks))
                if matches!(blocks.as_slice(), [ContentBlock::Thought { text, .. }] if text == "completed thought")
        ));
    }

    #[tokio::test]
    async fn protocol_errors_preserve_partial_assistant_content() {
        struct FailingAdapter;

        impl ModelClient for FailingAdapter {
            fn stream(&self, _req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
                Ok(Box::pin(futures::stream::iter([
                    Ok(ModelEvent::Reasoning("checking".into())),
                    Ok(ModelEvent::Text("partial".into())),
                    Err(ash_core::ProtocolError::InvalidResponse(
                        "stream ended badly".into(),
                    )),
                ])))
            }
        }

        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(8);

        let error = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            session_id,
            &FailingAdapter,
            Some(&mut persistence),
        )
        .await
        .unwrap_err();
        persistence.commit().await.unwrap();

        assert!(matches!(error, ash_core::AshError::Protocol(_)));
        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [
                    ContentBlock::Thought { text, .. },
                    ContentBlock::Text(answer),
                ] if text == "checking" && answer == "partial")
        ));
        let persisted = serde_json::to_string(&load_session(&store, session_id).await.log).unwrap();
        assert!(persisted.contains("checking"));
        assert!(persisted.contains("partial"));
    }

    #[tokio::test]
    async fn truncated_stream_is_retried_and_partial_output_discarded() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            // First call: the provider cuts the stream mid-response (text
            // without any `Stop`). Second call: clean finish.
            responses: Mutex::new(VecDeque::from([
                vec![ModelEvent::Text("partial thought".into())],
                vec![
                    ModelEvent::Text("complete".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 1,
            retry_backoff: RetryBackoff {
                base: Duration::from_millis(1),
                max: Duration::from_millis(10),
            },
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let (session_id, store, mut persistence) =
            persisted_session(directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(8);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            session_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.commit().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap().len(), 2);
        // The partial output must not survive the retry, neither in memory
        // nor in the staged/committed log.
        assert!(messages.iter().all(|message| {
            !matches!(
                &message.content,
                MessageContent::Assistant(blocks)
                    if blocks.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.contains("partial")))
            )
        }));
        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "complete")
        ));
        let persisted = serde_json::to_string(&load_session(&store, session_id).await.log).unwrap();
        assert!(!persisted.contains("partial thought"));
        assert!(persisted.contains("complete"));
    }

    #[tokio::test]
    async fn truncated_stream_exhausts_retries_and_reports_truncated() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![ModelEvent::Text("still cut".into())],
                vec![ModelEvent::Text("cut again".into())],
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 1,
            retry_backoff: RetryBackoff {
                base: Duration::from_millis(1),
                max: Duration::from_millis(10),
            },
        };
        let mut messages = vec![Message::user("question")];
        let (tx, _rx) = mpsc::channel(8);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(reason, StopReason::Truncated);
        // The final (also truncated) attempt is kept so the user sees what
        // the provider returned before the retry budget ran out.
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "cut again")
        ));
    }

    #[tokio::test]
    async fn transport_errors_are_retried_but_shaping_errors_are_not() {
        // Rate limit: retried, then succeeds.
        struct RateLimitThenOk {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }
        impl ModelClient for RateLimitThenOk {
            fn stream(&self, req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
                self.requests.lock().unwrap().push(req);
                let calls = self.requests.lock().unwrap().len();
                let items: Vec<Result<ModelEvent, ash_core::ProtocolError>> = if calls == 1 {
                    vec![Err(ash_core::ProtocolError::RateLimited)]
                } else {
                    vec![
                        Ok(ModelEvent::Text("ok".into())),
                        Ok(ModelEvent::Stop(StopReason::EndTurn)),
                    ]
                };
                Ok(Box::pin(futures::stream::iter(items)))
            }
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = RateLimitThenOk {
            requests: requests.clone(),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 1,
            retry_backoff: RetryBackoff {
                base: Duration::from_millis(1),
                max: Duration::from_millis(10),
            },
        };
        let mut messages = vec![Message::user("question")];
        let (tx, _rx) = mpsc::channel(8);
        let (reason, _) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap();
        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap().len(), 2);

        // Invalid response (a shaping error): never retried.
        let adapter = FailingAdapter(ash_core::ProtocolError::InvalidResponse("bad json".into()));
        let (tx, _rx) = mpsc::channel(8);
        let error = run_with_adapter(
            &config,
            &mut vec![Message::user("question")],
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ash_core::AshError::Protocol(_)));
    }

    #[tokio::test]
    async fn cancellation_completes_an_active_tool_call() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                ModelEvent::ToolCall {
                    id: ToolCallId::from_provider("call_1"),
                    name: "blocking".into(),
                    arguments: serde_json::json!({}),
                },
                ModelEvent::Stop(StopReason::EndTurn),
            ]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = RunConfig {
            system_prompt: None,
            tools: vec![Arc::new(BlockingTool)],
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(30),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 0,
            retry_backoff: RetryBackoff::default(),
        };
        let mut messages = vec![Message::user("use a tool")];
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let reason = {
            let turn = run_with_adapter(
                &config,
                &mut messages,
                tx,
                cancel.clone(),
                SessionId::new(),
                &adapter,
                None,
            );
            tokio::pin!(turn);

            loop {
                let event = tokio::select! {
                    event = rx.recv() => event,
                    result = &mut turn => panic!("turn ended before cancellation: {result:?}"),
                };
                if matches!(
                    event,
                    Some(SessionEventKind::Live(LiveEvent::ToolStarted { .. }))
                ) {
                    break;
                }
            }
            cancel.cancel();
            (&mut turn).await.unwrap().0
        };

        assert_eq!(reason, StopReason::Aborted);
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[2].content,
            MessageContent::ToolResult { result: Err(error), .. } if error == "cancelled"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(SessionEventKind::Live(LiveEvent::ToolFinished {
                is_error: true,
                ..
            }))
        ));
    }

    #[test]
    fn response_action_prioritizes_failure_then_tool_calls_then_cancellation() {
        let call = PendingToolCall {
            id: ToolCallId::from_provider("call_1"),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        };
        let failed = || StreamExit::Failed(ash_core::ProtocolError::InvalidResponse("boom".into()));
        let stop = StopReason::EndTurn;

        assert!(matches!(
            response_action(StreamExit::Exhausted, Vec::new(), stop.clone()),
            ResponseOutcome::Finished(StopReason::EndTurn)
        ));
        assert!(matches!(
            response_action(StreamExit::Cancelled, Vec::new(), stop.clone()),
            ResponseOutcome::Cancelled
        ));
        // Tool calls beat cancellation: the model already asked for them.
        assert!(matches!(
            response_action(StreamExit::Cancelled, vec![call.clone()], stop.clone()),
            ResponseOutcome::ToolCalls(_)
        ));
        // Stream failure beats everything, even pending tool calls.
        assert!(matches!(
            response_action(failed(), vec![call], stop.clone()),
            ResponseOutcome::Failed(_)
        ));
        assert!(matches!(
            response_action(failed(), Vec::new(), stop),
            ResponseOutcome::Failed(_)
        ));
    }

    #[test]
    fn truncates_large_tool_output_without_splitting_utf8() {
        let output = "你".repeat(MAX_AGENT_OUTPUT_BYTES);
        let truncated = limit_tool_output(output);

        assert!(truncated.len() <= MAX_AGENT_OUTPUT_BYTES);
        assert!(truncated.contains("tool output truncated"));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn limits_tool_output_text() {
        let output = ToolOutput::from("x".repeat(MAX_AGENT_OUTPUT_BYTES + 1));

        let limited = limit_tool_result(Ok(output)).unwrap();

        assert!(limited.text.contains("tool output truncated"));
    }

    #[test]
    fn retry_delay_grows_exponentially_and_caps_at_max() {
        let backoff = RetryBackoff {
            base: Duration::from_secs(1),
            max: Duration::from_secs(10),
        };
        assert_eq!(retry_delay(0, backoff), Duration::from_secs(1));
        assert_eq!(retry_delay(1, backoff), Duration::from_secs(2));
        assert_eq!(retry_delay(2, backoff), Duration::from_secs(4));
        assert_eq!(retry_delay(3, backoff), Duration::from_secs(8));
        // 16s would exceed the cap; clamped to 10s.
        assert_eq!(retry_delay(4, backoff), Duration::from_secs(10));
        assert_eq!(retry_delay(9, backoff), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn cancellation_during_retry_backoff_aborts_the_turn() {
        // Every call truncates; with a long backoff the cancellation lands
        // while the runner is waiting between attempts.
        struct AlwaysTruncated(Arc<Mutex<Vec<ModelRequest>>>);
        impl ModelClient for AlwaysTruncated {
            fn stream(&self, req: ModelRequest) -> Result<ModelStream, ash_core::ProtocolError> {
                self.0.lock().unwrap().push(req);
                Ok(Box::pin(futures::stream::iter(vec![Ok(ModelEvent::Text(
                    "cut".into(),
                ))])))
            }
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = AlwaysTruncated(requests.clone());
        let config = RunConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::DefaultContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            identity: SessionIdentity::root(SessionId::new()),
            max_retries: 5,
            retry_backoff: RetryBackoff {
                base: Duration::from_secs(1),
                max: Duration::from_secs(10),
            },
        };
        let mut messages = vec![Message::user("question")];
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let turn = run_with_adapter(
            &config,
            &mut messages,
            tx,
            cancel.clone(),
            SessionId::new(),
            &adapter,
            None,
        );
        tokio::pin!(turn);

        // Let the first truncated call settle and enter the 1s backoff, then
        // cancel. The wait must abort instead of retrying. Poll the turn
        // concurrently so the model call can actually run.
        let wait_for_first_call = async {
            loop {
                if !requests.lock().unwrap().is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            () = wait_for_first_call => {}
            result = &mut turn => panic!("turn ended before the first call: {result:?}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let (reason, _) = (&mut turn).await.unwrap();

        assert_eq!(reason, StopReason::Aborted);
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
