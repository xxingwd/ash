use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ash_core::{
    AshError, CancellationToken, Conversation, Input, Item, ModelClient, ModelEvent, ModelRequest,
    ModelStream, ProtocolError, SessionEvent, Step, StopReason, ToolCall, ToolCallId, ToolContext,
    ToolError, ToolOutput, ToolTimeout, Turn, TurnActivity, TurnId, TurnResult, TurnStats,
};
use futures::{future::join_all, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

use crate::{
    context::{estimate_request_tokens, estimate_tokens, needs_compaction},
    Agent,
};

const MAX_AGENT_OUTPUT_BYTES: usize = 64 * 1024;
const AGENT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const AGENT_OUTPUT_TRUNCATION_NOTICE: &str = "\n... tool output truncated by the agent ...\n";
const MAX_RETRIES: u32 = 5;
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(10);
pub(crate) const COMPACTION_SYSTEM_PROMPT: &str = "You are an anchored context summarization assistant for coding sessions. Summarize only the supplied conversation history. Do not answer the conversation. Preserve exact technical details and respond in the conversation's language.";

#[derive(Clone, Debug)]
enum ContentBlock {
    Text(String),
    Thought {
        text: String,
        elapsed_seconds: u64,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
}

enum ModelResponse {
    Stopped {
        content: Vec<ContentBlock>,
        reason: StopReason,
        stats: TurnStats,
    },
    Truncated {
        content: Vec<ContentBlock>,
        stats: TurnStats,
    },
    Cancelled {
        stats: TurnStats,
    },
    Failed {
        error: ProtocolError,
        stats: TurnStats,
    },
}

impl ModelResponse {
    const fn stats(&self) -> TurnStats {
        match self {
            Self::Stopped { stats, .. }
            | Self::Truncated { stats, .. }
            | Self::Cancelled { stats }
            | Self::Failed { stats, .. } => *stats,
        }
    }
}

pub(crate) struct EngineOutcome {
    pub(crate) turn: Arc<Turn>,
    pub(crate) summary: Option<String>,
    pub(crate) error: Option<AshError>,
}

pub(crate) struct StepCommit {
    pub(crate) step: Arc<Step>,
    pub(crate) reply: oneshot::Sender<Result<(), AshError>>,
}

#[derive(Clone)]
pub(crate) enum StepCommitter {
    Channel(mpsc::Sender<StepCommit>),
    #[cfg(test)]
    Immediate,
}

impl StepCommitter {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<StepCommit>) {
        let (sender, receiver) = mpsc::channel(1);
        (Self::Channel(sender), receiver)
    }

    async fn commit(&self, step: Arc<Step>) -> Result<(), AshError> {
        let sender = match self {
            Self::Channel(sender) => sender,
            #[cfg(test)]
            Self::Immediate => return Ok(()),
        };
        let (reply, result) = oneshot::channel();
        sender
            .send(StepCommit { step, reply })
            .await
            .map_err(|_| ash_core::SessionError::Closed)?;
        result.await.map_err(|_| ash_core::SessionError::Closed)?
    }
}

pub(crate) struct TurnChannels {
    events: mpsc::Sender<SessionEvent>,
    committer: StepCommitter,
}

impl TurnChannels {
    pub(crate) const fn new(events: mpsc::Sender<SessionEvent>, committer: StepCommitter) -> Self {
        Self { events, committer }
    }

    #[cfg(test)]
    const fn immediate(events: mpsc::Sender<SessionEvent>) -> Self {
        Self {
            events,
            committer: StepCommitter::Immediate,
        }
    }
}

pub(crate) async fn run_turn(
    model: &dyn ModelClient,
    agent: &Agent,
    conversation: &Conversation,
    id: TurnId,
    input: Input,
    channels: TurnChannels,
    context: ToolContext,
) -> EngineOutcome {
    TurnRunner {
        model,
        agent,
        conversation,
        id,
        input: Arc::new(input),
        events: channels.events,
        committer: channels.committer,
        context,
        steps: Vec::new(),
        stats: TurnStats::default(),
        pending_summary: None,
        last_context: None,
    }
    .run()
    .await
}

struct TurnRunner<'a> {
    model: &'a dyn ModelClient,
    agent: &'a Agent,
    conversation: &'a Conversation,
    id: TurnId,
    input: Arc<Input>,
    events: mpsc::Sender<SessionEvent>,
    committer: StepCommitter,
    context: ToolContext,
    steps: Vec<Arc<Step>>,
    stats: TurnStats,
    pending_summary: Option<String>,
    last_context: Option<(u64, u64)>,
}

impl TurnRunner<'_> {
    async fn run(mut self) -> EngineOutcome {
        let (result, error) = match self.run_loop().await {
            Ok(result) => (result, None),
            Err(AshError::Cancelled) => (TurnResult::Cancelled, None),
            Err(error) => (TurnResult::Failed(error.to_string()), Some(error)),
        };
        let turn = Turn {
            id: self.id,
            input: Arc::unwrap_or_clone(self.input),
            steps: self.steps,
            result,
            stats: self.stats,
        };
        EngineOutcome {
            turn: Arc::new(turn),
            summary: self.pending_summary,
            error,
        }
    }

    async fn run_loop(&mut self) -> Result<TurnResult, AshError> {
        loop {
            if self.context.cancellation.is_cancelled() {
                return Ok(TurnResult::Cancelled);
            }
            let request = self.prepare_request().await?;

            let mut retries = 0;
            let response = loop {
                debug!(turn_id = %self.id, "calling model");
                let response = match self.model.stream(request.clone()) {
                    Ok(mut stream) => {
                        collect_response(
                            &mut stream,
                            Some((&self.events, self.id)),
                            &self.context.cancellation,
                        )
                        .await
                    }
                    Err(error) => ModelResponse::Failed {
                        error,
                        stats: TurnStats::default(),
                    },
                };
                let retry = matches!(&response, ModelResponse::Truncated { .. })
                    || matches!(&response, ModelResponse::Failed { error, .. } if retryable(error));
                self.record_stats(response.stats()).await;
                if !retry || retries >= MAX_RETRIES {
                    break response;
                }
                let delay = retry_delay(retries, RETRY_BASE, RETRY_MAX);
                send_event(&self.events, SessionEvent::Retrying { turn_id: self.id }).await;
                debug!(turn_id = %self.id, retries, ?delay, "retrying model call");
                tokio::select! {
                    biased;
                    () = self.context.cancellation.cancelled() => {
                        return Ok(TurnResult::Cancelled);
                    }
                    () = tokio::time::sleep(delay) => {}
                }
                retries += 1;
            };

            match response {
                ModelResponse::Cancelled { .. } => return Ok(TurnResult::Cancelled),
                ModelResponse::Failed { error, .. } => return Err(error.into()),
                ModelResponse::Truncated { content, .. } => {
                    self.commit_without_tools(content).await?;
                    return Ok(TurnResult::Truncated);
                }
                ModelResponse::Stopped {
                    content,
                    reason,
                    stats: _,
                } => {
                    if reason != StopReason::EndTurn || !has_tool_calls(&content) {
                        self.commit_without_tools(content).await?;
                        return Ok(TurnResult::Stopped(reason));
                    }
                    let step = self.execute_tools(content).await;
                    self.commit_step(step).await?;
                    if self.context.cancellation.is_cancelled() {
                        return Ok(TurnResult::Cancelled);
                    }
                }
            }
        }
    }

    fn request(&self) -> ModelRequest {
        ModelRequest {
            model: self.agent.model().clone(),
            system: self.agent.system_prompt().map(str::to_string),
            context: self.model_context(),
            tools: self.agent.tool_definitions(),
            max_tokens: None,
        }
    }

    fn model_context(&self) -> ash_core::ModelContext {
        self.pending_summary
            .as_ref()
            .map_or_else(
                || self.conversation.context(),
                |summary| self.conversation.context_with_summary(summary),
            )
            .with_current(self.input.clone(), self.steps.clone())
    }

    async fn prepare_request(&mut self) -> Result<ModelRequest, AshError> {
        let mut request = self.request();
        let mut tokens = request_tokens(&request);
        if self.pending_summary.is_none()
            && needs_compaction(tokens, self.agent.max_context_tokens())
        {
            if let Some((summary, stats)) = compact(
                self.model,
                self.agent,
                self.conversation,
                &self.context.cancellation,
            )
            .await?
            {
                self.record_stats(stats).await;
                let context = self
                    .conversation
                    .context_with_summary(&summary)
                    .with_current(self.input.clone(), self.steps.clone());
                let compacted_tokens =
                    estimate_request_tokens(request.system.as_deref(), &context, &request.tools);
                if compacted_tokens < tokens {
                    request.context = context;
                    tokens = compacted_tokens;
                    self.pending_summary = Some(summary);
                }
            }
        }

        self.publish_context(tokens).await;
        Ok(request)
    }

    async fn publish_context(&mut self, tokens: usize) {
        let tokens = u64::try_from(tokens).unwrap_or(u64::MAX);
        let limit = u64::try_from(self.agent.max_context_tokens()).unwrap_or(u64::MAX);
        if self.last_context != Some((tokens, limit)) {
            self.last_context = Some((tokens, limit));
            send_event(
                &self.events,
                SessionEvent::Context {
                    turn_id: self.id,
                    tokens,
                    limit,
                },
            )
            .await;
        }
    }

    async fn record_stats(&mut self, delta: TurnStats) {
        self.stats = self.stats.saturating_add(delta);
        if delta.input_tokens > 0 || delta.output_tokens > 0 {
            self.publish_activity().await;
        }
    }

    /// Emit the current activity snapshot; consumers replace their view with
    /// the complete value instead of accumulating deltas.
    async fn publish_activity(&self) {
        let activity = TurnActivity {
            stats: self.stats,
            completed_tool_calls: self.completed_tool_calls(),
        };
        send_event(
            &self.events,
            SessionEvent::Activity {
                turn_id: self.id,
                activity,
            },
        )
        .await;
    }

    fn completed_tool_calls(&self) -> u64 {
        u64::try_from(self.steps.iter().flat_map(|step| step.tool_calls()).count())
            .unwrap_or(u64::MAX)
    }

    async fn commit_without_tools(&mut self, content: Vec<ContentBlock>) -> Result<(), AshError> {
        let items = content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(Item::Text(text)),
                ContentBlock::Thought {
                    text,
                    elapsed_seconds,
                } => Some(Item::Thought {
                    text,
                    elapsed_seconds,
                }),
                ContentBlock::ToolCall { .. } => None,
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Ok(());
        }
        self.commit_step(Step { items }).await
    }

    async fn commit_step(&mut self, step: Step) -> Result<(), AshError> {
        if step.items.is_empty() {
            return Ok(());
        }
        let has_tools = step.tool_calls().next().is_some();
        let step = Arc::new(step);
        self.committer.commit(Arc::clone(&step)).await?;
        self.steps.push(step);
        if has_tools {
            self.publish_activity().await;
        }
        Ok(())
    }

    async fn execute_tools(&self, content: Vec<ContentBlock>) -> Step {
        const MAX_PARALLEL_TOOLS: usize = 8;

        let calls = content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.clone(), name.clone(), arguments.clone())),
            ContentBlock::Text(_) | ContentBlock::Thought { .. } => None,
        });
        let mut pending = calls
            .map(|(id, name, arguments)| self.execute_tool_call(id, name, arguments))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(pending.len());
        while !pending.is_empty() {
            let batch = pending.drain(..pending.len().min(MAX_PARALLEL_TOOLS));
            results.extend(join_all(batch).await);
        }
        let mut results = results.into_iter();
        let items = content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(Item::Text(text)),
                ContentBlock::Thought {
                    text,
                    elapsed_seconds,
                } => Some(Item::Thought {
                    text,
                    elapsed_seconds,
                }),
                ContentBlock::ToolCall { .. } => results.next().map(Item::ToolCall),
            })
            .collect();
        Step { items }
    }

    async fn execute_tool_call(
        &self,
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    ) -> ToolCall {
        send_event(
            &self.events,
            SessionEvent::ToolStarted {
                turn_id: self.id,
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            },
        )
        .await;
        let result = limit_tool_result(self.execute_tool(&name, arguments.clone()).await)
            .map_err(|error| error.to_string());
        send_event(
            &self.events,
            SessionEvent::ToolFinished {
                turn_id: self.id,
                id: id.clone(),
                result: result
                    .as_ref()
                    .map(|output| output.text.clone())
                    .map_err(Clone::clone),
            },
        )
        .await;
        ToolCall {
            id,
            name,
            arguments,
            result,
        }
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
            identity: self.context.identity,
            cancellation: self.context.cancellation.clone(),
            deadline: timeout.map(|timeout| Instant::now() + timeout),
        };
        context
            .run(tool.execute(context.clone(), arguments))
            .await?
    }
}

fn request_tokens(request: &ModelRequest) -> usize {
    estimate_request_tokens(request.system.as_deref(), &request.context, &request.tools)
}

pub(crate) async fn compact(
    model: &dyn ModelClient,
    agent: &Agent,
    conversation: &Conversation,
    cancellation: &CancellationToken,
) -> Result<Option<(String, TurnStats)>, AshError> {
    let Some(prompt) = conversation.compact_prompt() else {
        return Ok(None);
    };
    if estimate_tokens(COMPACTION_SYSTEM_PROMPT).saturating_add(estimate_tokens(&prompt))
        > agent.max_context_tokens()
    {
        return Err(ProtocolError::InvalidRequest(
            "conversation is too large to compact without dropping history".to_string(),
        )
        .into());
    }
    let mut stream = model.stream(ModelRequest {
        model: agent.model().clone(),
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        context: ash_core::ModelContext::default().with_current(Input::user(prompt), Vec::new()),
        tools: Vec::new(),
        max_tokens: None,
    })?;
    match collect_response(&mut stream, None, cancellation).await {
        ModelResponse::Stopped {
            content,
            reason: StopReason::EndTurn,
            stats,
        } if !has_tool_calls(&content) => {
            let summary = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) if !text.trim().is_empty() => Some(text.as_str()),
                    ContentBlock::Text(_)
                    | ContentBlock::Thought { .. }
                    | ContentBlock::ToolCall { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if summary.trim().is_empty() {
                return Err(ProtocolError::InvalidResponse(
                    "compaction model returned an empty summary".to_string(),
                )
                .into());
            }
            Ok(Some((summary, stats)))
        }
        ModelResponse::Stopped { reason, .. } => Err(ProtocolError::InvalidResponse(format!(
            "compaction model stopped before completing the summary: {reason}"
        ))
        .into()),
        ModelResponse::Truncated { .. } => Err(ProtocolError::InvalidResponse(
            "compaction model returned a truncated summary".to_string(),
        )
        .into()),
        ModelResponse::Cancelled { .. } => Err(AshError::Cancelled),
        ModelResponse::Failed { error, .. } => Err(error.into()),
    }
}

async fn collect_response(
    stream: &mut ModelStream,
    events: Option<(&mpsc::Sender<SessionEvent>, TurnId)>,
    cancellation: &CancellationToken,
) -> ModelResponse {
    let mut accumulator = ResponseAccumulator::new();
    loop {
        let next = tokio::select! {
            biased;
            () = cancellation.cancelled() => return accumulator.cancelled(),
            next = stream.next() => next,
        };
        let Some(item) = next else {
            return accumulator.finish();
        };
        match item {
            Ok(item) => accumulator.apply(events, item).await,
            Err(error) => return accumulator.failed(error),
        }
    }
}

struct ResponseAccumulator {
    content: Vec<ContentBlock>,
    thought_started_at: Option<Instant>,
    first_output_at: Option<Instant>,
    stop: Option<StopReason>,
    input_tokens: u64,
    output_tokens: u64,
}

impl ResponseAccumulator {
    fn new() -> Self {
        Self {
            content: Vec::new(),
            thought_started_at: None,
            first_output_at: None,
            stop: None,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    async fn apply(
        &mut self,
        events: Option<(&mpsc::Sender<SessionEvent>, TurnId)>,
        event: ModelEvent,
    ) {
        match event {
            ModelEvent::Text(text) => {
                self.first_output_at.get_or_insert_with(Instant::now);
                self.finish_thought();
                match self.content.last_mut() {
                    Some(ContentBlock::Text(existing)) => existing.push_str(&text),
                    _ => self.content.push(ContentBlock::Text(text.clone())),
                }
                if let Some((events, turn_id)) = events {
                    send_event(events, SessionEvent::Text { turn_id, text }).await;
                }
            }
            ModelEvent::Reasoning(text) => {
                self.first_output_at.get_or_insert_with(Instant::now);
                match self.content.last_mut() {
                    Some(ContentBlock::Thought { text: existing, .. }) => existing.push_str(&text),
                    _ => {
                        self.thought_started_at = Some(Instant::now());
                        self.content.push(ContentBlock::Thought {
                            text: text.clone(),
                            elapsed_seconds: 0,
                        });
                    }
                }
                if let Some((events, turn_id)) = events {
                    send_event(events, SessionEvent::Thought { turn_id, text }).await;
                }
            }
            ModelEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.first_output_at.get_or_insert_with(Instant::now);
                self.finish_thought();
                self.content.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            ModelEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.input_tokens = self.input_tokens.max(input_tokens);
                self.output_tokens = self.output_tokens.max(output_tokens);
            }
            ModelEvent::Stop(reason) => {
                self.finish_thought();
                self.stop = Some(reason);
            }
        }
    }

    fn finish(mut self) -> ModelResponse {
        self.finish_thought();
        let stats = self.stats();
        match self.stop {
            Some(reason) => ModelResponse::Stopped {
                content: self.content,
                reason,
                stats,
            },
            None => {
                self.content
                    .retain(|block| !matches!(block, ContentBlock::ToolCall { .. }));
                ModelResponse::Truncated {
                    content: self.content,
                    stats,
                }
            }
        }
    }

    fn cancelled(mut self) -> ModelResponse {
        self.finish_thought();
        ModelResponse::Cancelled {
            stats: self.stats(),
        }
    }

    fn failed(mut self, error: ProtocolError) -> ModelResponse {
        self.finish_thought();
        ModelResponse::Failed {
            error,
            stats: self.stats(),
        }
    }

    fn stats(&self) -> TurnStats {
        TurnStats {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            generation_ms: elapsed_generation_ms(self.first_output_at),
        }
    }

    fn finish_thought(&mut self) {
        let Some(started) = self.thought_started_at.take() else {
            return;
        };
        if let Some(ContentBlock::Thought {
            elapsed_seconds, ..
        }) = self.content.last_mut()
        {
            *elapsed_seconds = started.elapsed().as_secs();
        }
    }
}

fn has_tool_calls(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
}

async fn send_event(events: &mpsc::Sender<SessionEvent>, event: SessionEvent) {
    if let Err(error) = events.send(event).await {
        tracing::warn!(%error, "dropping session event: receiver closed");
    }
}

fn elapsed_generation_ms(first_output_at: Option<Instant>) -> u64 {
    first_output_at.map_or(0, |started| {
        started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
    })
}

const fn retryable(error: &ProtocolError) -> bool {
    matches!(
        error,
        ProtocolError::Request(_) | ProtocolError::RateLimited { .. }
    ) || matches!(
        error,
        ProtocolError::Upstream {
            status: 500 | 502 | 503 | 504 | 520..=524 | 529,
            ..
        }
    )
}

fn retry_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let max_ms = u64::try_from(max.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(base_ms.saturating_mul(1_u64 << attempt.min(20)).min(max_ms))
}

fn limit_tool_result(result: Result<ToolOutput, ToolError>) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(mut output) => {
            output.text = limit_tool_output(output.text);
            Ok(output)
        }
        Err(ToolError::Execution(output)) => Err(ToolError::Execution(limit_tool_output(output))),
        Err(ToolError::CommandFailed { status, output }) => Err(ToolError::CommandFailed {
            status,
            output: limit_tool_output(output),
        }),
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
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        },
    };

    use ash_core::{define_tool, ModelId};

    use super::*;

    struct MockModel {
        responses: StdMutex<VecDeque<Vec<Result<ModelEvent, ProtocolError>>>>,
        requests: StdMutex<Vec<ModelRequest>>,
    }

    struct PendingModel {
        started: tokio::sync::Notify,
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct ToolTestArgs {
        index: usize,
    }

    impl ModelClient for PendingModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            self.started.notify_one();
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    impl MockModel {
        fn new(events: impl IntoIterator<Item = ModelEvent>) -> Self {
            Self {
                responses: StdMutex::new(VecDeque::from([events.into_iter().map(Ok).collect()])),
                requests: StdMutex::new(Vec::new()),
            }
        }
    }

    impl ModelClient for MockModel {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(response)))
        }
    }

    fn settled_turn(input: &str) -> Arc<Turn> {
        Arc::new(Turn {
            id: TurnId::new(),
            input: Input::user(input),
            steps: Vec::new(),
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: TurnStats::default(),
        })
    }

    #[test]
    fn retry_delay_grows_and_caps() {
        assert_eq!(
            retry_delay(0, Duration::from_secs(1), Duration::from_secs(10)),
            Duration::from_secs(1)
        );
        assert_eq!(
            retry_delay(9, Duration::from_secs(1), Duration::from_secs(10)),
            Duration::from_secs(10)
        );
    }

    #[tokio::test]
    async fn retries_reset_the_preview_before_publishing_the_next_attempt() {
        let model = MockModel {
            responses: StdMutex::new(VecDeque::from([
                vec![Ok(ModelEvent::Text("discarded".into()))],
                vec![
                    Ok(ModelEvent::Text("final".into())),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ],
            ])),
            requests: StdMutex::new(Vec::new()),
        };
        let (events, mut incoming) = mpsc::channel(64);
        let outcome = run_turn(
            &model,
            &Agent::new(ModelId::new("model"), Vec::new()),
            &Conversation::new(),
            TurnId::new(),
            Input::user("retry"),
            TurnChannels::immediate(events),
            ToolContext {
                identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
                cancellation: CancellationToken::new(),
                deadline: None,
            },
        )
        .await;
        let mut preview = String::new();
        let mut resets = 0;
        while let Ok(event) = incoming.try_recv() {
            match event {
                SessionEvent::Text { text, .. } => preview.push_str(&text),
                SessionEvent::Retrying { .. } => {
                    preview.clear();
                    resets += 1;
                }
                _ => {}
            }
        }
        assert_eq!(resets, 1);
        assert_eq!(preview, "final");
        assert_eq!(outcome.turn.visible_text().as_deref(), Some("final"));
    }

    #[test]
    fn large_tool_output_keeps_valid_utf8_tail() {
        let source = "a".repeat(MAX_AGENT_OUTPUT_BYTES) + "中文tail";
        let output = limit_tool_output(source);
        assert!(output.contains(AGENT_OUTPUT_TRUNCATION_NOTICE));
        assert!(output.ends_with("中文tail"));
    }

    #[tokio::test]
    async fn truncated_response_drops_tools_but_keeps_reported_usage() {
        let mut stream: ModelStream = Box::pin(futures::stream::iter([
            Ok(ModelEvent::Text("partial".to_string())),
            Ok(ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call"),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
            }),
            Ok(ModelEvent::Usage {
                input_tokens: 12,
                output_tokens: 3,
            }),
        ]));

        let response = collect_response(&mut stream, None, &CancellationToken::new()).await;

        let ModelResponse::Truncated { content, stats } = response else {
            panic!("expected truncated response");
        };
        assert!(matches!(content.as_slice(), [ContentBlock::Text(text)] if text == "partial"));
        assert_eq!(stats.input_tokens, 12);
        assert_eq!(stats.output_tokens, 3);
    }

    #[tokio::test]
    async fn usage_is_collected_once_per_response() {
        let mut stream: ModelStream = Box::pin(futures::stream::iter([
            Ok(ModelEvent::Text("answer".to_string())),
            Ok(ModelEvent::Usage {
                input_tokens: 12,
                output_tokens: 0,
            }),
            Ok(ModelEvent::Usage {
                input_tokens: 12,
                output_tokens: 0,
            }),
            Ok(ModelEvent::Usage {
                input_tokens: 0,
                output_tokens: 3,
            }),
            Ok(ModelEvent::Stop(StopReason::EndTurn)),
        ]));
        let response = collect_response(&mut stream, None, &CancellationToken::new()).await;

        assert!(matches!(
            response,
            ModelResponse::Stopped {
                reason: StopReason::EndTurn,
                ..
            }
        ));
        assert_eq!(response.stats().input_tokens, 12);
        assert_eq!(response.stats().output_tokens, 3);
    }

    #[tokio::test]
    async fn context_is_published_for_each_model_request() {
        let model = MockModel::new([ModelEvent::Stop(StopReason::EndTurn)]);
        let agent = Agent::new(ModelId::new("model"), Vec::new()).with_max_context_tokens(1_000);
        let (events, mut receiver) = mpsc::channel(8);
        let turn_id = TurnId::new();

        let outcome = run_turn(
            &model,
            &agent,
            &Conversation::new(),
            turn_id,
            Input::user("hello"),
            TurnChannels::immediate(events.clone()),
            ToolContext {
                identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
                cancellation: CancellationToken::new(),
                deadline: None,
            },
        )
        .await;
        drop(events);

        let events = receiver.recv().await.expect("context event");
        assert!(matches!(
            events,
            SessionEvent::Context {
                turn_id: event_turn,
                tokens,
                limit: 1_000,
            } if event_turn == turn_id && tokens > 0
        ));
        assert_eq!(
            outcome.turn.result,
            TurnResult::Stopped(StopReason::EndTurn)
        );
    }

    #[tokio::test]
    async fn non_end_turn_never_executes_a_tool_call() {
        let model = MockModel::new([
            ModelEvent::Text("partial".to_string()),
            ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call"),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
            },
            ModelEvent::Stop(StopReason::Other("unknown".to_string())),
        ]);
        let agent = Agent::new(ModelId::new("model"), Vec::new());
        let (events, _) = mpsc::channel(8);

        let outcome = run_turn(
            &model,
            &agent,
            &Conversation::new(),
            TurnId::new(),
            Input::user("hello"),
            TurnChannels::immediate(events),
            ToolContext {
                identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
                cancellation: CancellationToken::new(),
                deadline: None,
            },
        )
        .await;

        assert_eq!(
            outcome.turn.result,
            TurnResult::Stopped(StopReason::Other("unknown".to_string()))
        );
        assert_eq!(outcome.turn.visible_text().as_deref(), Some("partial"));
        assert!(!outcome.turn.has_tools());
    }

    #[tokio::test]
    async fn bounds_parallel_tool_execution_without_reordering_results() {
        const TOOL_CALLS: usize = 10;
        const MAX_PARALLEL_TOOLS: usize = 8;

        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let tool_active = Arc::clone(&active);
        let tool_maximum = Arc::clone(&maximum);
        let tool = define_tool("test", "test", move |_, args: ToolTestArgs| {
            let tool_active = Arc::clone(&tool_active);
            let tool_maximum = Arc::clone(&tool_maximum);
            let index = args.index;
            async move {
                let current = tool_active.fetch_add(1, Ordering::SeqCst) + 1;
                tool_maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                tool_active.fetch_sub(1, Ordering::SeqCst);
                Ok(index.to_string())
            }
        })
        .unwrap();
        let calls = (0..TOOL_CALLS)
            .map(|index| {
                Ok(ModelEvent::ToolCall {
                    id: ToolCallId::from_provider(format!("call-{index}")),
                    name: "test".to_string(),
                    arguments: serde_json::json!({ "index": index }),
                })
            })
            .chain([Ok(ModelEvent::Stop(StopReason::EndTurn))])
            .collect::<Vec<_>>();
        let model = MockModel {
            responses: StdMutex::new(VecDeque::from([
                calls,
                vec![
                    Ok(ModelEvent::Text("complete".to_string())),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ],
            ])),
            requests: StdMutex::new(Vec::new()),
        };
        let agent = Agent::new(ModelId::new("model"), vec![tool]);
        let (events, mut event_rx) = mpsc::channel(64);
        let conversation = Conversation::new();
        let run = run_turn(
            &model,
            &agent,
            &conversation,
            TurnId::new(),
            Input::user("run tools"),
            TurnChannels::immediate(events),
            ToolContext {
                identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
                cancellation: CancellationToken::new(),
                deadline: None,
            },
        );
        let outcome = run.await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(maximum.load(Ordering::SeqCst), MAX_PARALLEL_TOOLS);
        for (index, call) in outcome.turn.tool_calls().enumerate() {
            assert!(matches!(&call.result, Ok(output) if output.text == index.to_string()));
        }
        assert_eq!(outcome.turn.tool_calls().count(), TOOL_CALLS);
        assert_eq!(outcome.turn.completed_tool_calls(), TOOL_CALLS as u64);
        let activities = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                SessionEvent::Activity { activity, .. } => Some(activity),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].completed_tool_calls, TOOL_CALLS as u64);
    }

    #[tokio::test]
    async fn activity_snapshots_match_the_final_turn_across_model_rounds() {
        let tool = define_tool("test", "test", |_, index: ToolTestArgs| async move {
            Ok(index.index.to_string())
        })
        .unwrap();
        let model = MockModel {
            responses: StdMutex::new(VecDeque::from([
                vec![
                    Ok(ModelEvent::ToolCall {
                        id: ToolCallId::from_provider("call-1"),
                        name: "test".to_string(),
                        arguments: serde_json::json!({ "index": 1 }),
                    }),
                    Ok(ModelEvent::Usage {
                        input_tokens: 10,
                        output_tokens: 4,
                    }),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ],
                vec![
                    Ok(ModelEvent::ToolCall {
                        id: ToolCallId::from_provider("call-2"),
                        name: "test".to_string(),
                        arguments: serde_json::json!({ "index": 2 }),
                    }),
                    Ok(ModelEvent::Usage {
                        input_tokens: 20,
                        output_tokens: 6,
                    }),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ],
                vec![
                    Ok(ModelEvent::Text("complete".to_string())),
                    Ok(ModelEvent::Usage {
                        input_tokens: 30,
                        output_tokens: 2,
                    }),
                    Ok(ModelEvent::Stop(StopReason::EndTurn)),
                ],
            ])),
            requests: StdMutex::new(Vec::new()),
        };
        let agent = Agent::new(ModelId::new("model"), vec![tool]);
        let (events, mut event_rx) = mpsc::channel(64);

        let outcome = run_turn(
            &model,
            &agent,
            &Conversation::new(),
            TurnId::new(),
            Input::user("run tools"),
            TurnChannels::immediate(events),
            ToolContext {
                identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
                cancellation: CancellationToken::new(),
                deadline: None,
            },
        )
        .await;

        let activities = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                SessionEvent::Activity { activity, .. } => Some(activity),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(activities.len(), 5, "one per reported response and batch");
        let final_activity = TurnActivity {
            stats: outcome.turn.stats,
            completed_tool_calls: outcome.turn.completed_tool_calls(),
        };
        assert_eq!(*activities.last().unwrap(), final_activity);
        assert_eq!(activities[0].completed_tool_calls, 0);
        assert_eq!(activities[1].completed_tool_calls, 1);
        assert_eq!(activities[3].completed_tool_calls, 2);
    }

    #[tokio::test]
    async fn compact_request_has_only_the_fixed_prompt() {
        let model = MockModel::new([
            ModelEvent::Text("summary".to_string()),
            ModelEvent::Stop(StopReason::EndTurn),
        ]);
        let tool = define_tool("read", "read", |_, ()| async { Ok("unused") }).unwrap();
        let agent = Agent::new(ModelId::new("model"), vec![tool]);
        let mut conversation = Conversation::new();
        conversation.push(settled_turn("remember this"), None);

        let result = compact(&model, &agent, &conversation, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result.map(|(summary, _)| summary),
            Some("summary".to_string())
        );
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.system.as_deref(), Some(COMPACTION_SYSTEM_PROMPT));
        assert!(request.tools.is_empty());
        assert_eq!(request.max_tokens, None);
        assert!(request.context.summary().is_none());
        assert!(request.context.turns().is_empty());
        let (input, steps) = request.context.current().unwrap();
        assert!(input.text().contains("remember this"));
        assert!(steps.is_empty());
    }

    #[tokio::test]
    async fn compact_rejects_tool_calls_even_with_end_turn() {
        let model = MockModel::new([
            ModelEvent::Text("summary".to_string()),
            ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call"),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
            },
            ModelEvent::Stop(StopReason::EndTurn),
        ]);
        let agent = Agent::new(ModelId::new("model"), Vec::new());
        let mut conversation = Conversation::new();
        conversation.push(settled_turn("remember this"), None);

        assert!(
            compact(&model, &agent, &conversation, &CancellationToken::new(),)
                .await
                .is_err()
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_during_automatic_compaction_is_not_a_failure() {
        let model = PendingModel {
            started: tokio::sync::Notify::new(),
        };
        let agent = Agent::new(ModelId::new("model"), Vec::new()).with_max_context_tokens(1_000);
        let mut conversation = Conversation::new();
        conversation.push(settled_turn(&"x".repeat(3_200)), None);
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let (events, _) = mpsc::channel(8);
        let context = ToolContext {
            identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
            cancellation,
            deadline: None,
        };

        let (outcome, ()) = tokio::join!(
            run_turn(
                &model,
                &agent,
                &conversation,
                TurnId::new(),
                Input::user("current"),
                TurnChannels::immediate(events),
                context,
            ),
            async {
                model.started.notified().await;
                cancel.cancel();
            }
        );

        assert_eq!(outcome.turn.result, TurnResult::Cancelled);
        assert!(outcome.error.is_none());
    }
}
