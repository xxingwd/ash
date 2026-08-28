use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ash_core::{
    AshError, CancellationToken, Conversation, Input, Item, ModelClient, ModelEvent, ModelRequest,
    ModelStream, ProtocolError, SessionEvent, Step, StopReason, ToolCall, ToolCallId, ToolContext,
    ToolError, ToolOutput, ToolTimeout, Turn, TurnId, TurnResult, TurnStats,
};
use futures::{future::join_all, StreamExt};
use tokio::sync::mpsc;
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

pub(crate) async fn run_turn(
    model: &dyn ModelClient,
    agent: &Agent,
    conversation: &Conversation,
    id: TurnId,
    input: Input,
    events: mpsc::Sender<SessionEvent>,
    context: ToolContext,
) -> EngineOutcome {
    TurnRunner {
        model,
        agent,
        conversation,
        id,
        input,
        events,
        context,
        steps: Vec::new(),
        stats: TurnStats::default(),
        pending_summary: None,
    }
    .run()
    .await
}

struct TurnRunner<'a> {
    model: &'a dyn ModelClient,
    agent: &'a Agent,
    conversation: &'a Conversation,
    id: TurnId,
    input: Input,
    events: mpsc::Sender<SessionEvent>,
    context: ToolContext,
    steps: Vec<Step>,
    stats: TurnStats,
    pending_summary: Option<String>,
}

impl TurnRunner<'_> {
    async fn run(mut self) -> EngineOutcome {
        let (result, error) = self.run_loop().await;
        EngineOutcome {
            turn: Arc::new(Turn {
                id: self.id,
                input: self.input,
                steps: self.steps,
                result,
                stats: self.stats,
            }),
            summary: self.pending_summary,
            error,
        }
    }

    async fn run_loop(&mut self) -> (TurnResult, Option<AshError>) {
        loop {
            if self.context.cancellation.is_cancelled() {
                return (TurnResult::Cancelled, None);
            }
            if let Err(error) = self.prepare_context().await {
                return match error {
                    AshError::Cancelled => (TurnResult::Cancelled, None),
                    error => (TurnResult::Failed(error.to_string()), Some(error)),
                };
            }

            let mut retries = 0;
            let response = loop {
                debug!(turn_id = %self.id, "calling model");
                let response = match self.model.stream(self.request()) {
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
                self.stats = self.stats.saturating_add(response.stats());
                if !retry || retries >= MAX_RETRIES {
                    break response;
                }
                let delay = retry_delay(retries, RETRY_BASE, RETRY_MAX);
                debug!(turn_id = %self.id, retries, ?delay, "retrying model call");
                tokio::select! {
                    biased;
                    () = self.context.cancellation.cancelled() => {
                        return (TurnResult::Cancelled, None);
                    }
                    () = tokio::time::sleep(delay) => {}
                }
                retries += 1;
            };

            match response {
                ModelResponse::Cancelled { .. } => return (TurnResult::Cancelled, None),
                ModelResponse::Failed { error, .. } => {
                    let error: AshError = error.into();
                    return (TurnResult::Failed(error.to_string()), Some(error));
                }
                ModelResponse::Truncated { content, .. } => {
                    self.push_without_tools(content);
                    return (TurnResult::Truncated, None);
                }
                ModelResponse::Stopped {
                    content,
                    reason,
                    stats: _,
                } => {
                    if reason != StopReason::EndTurn || !has_tool_calls(&content) {
                        self.push_without_tools(content);
                        return (TurnResult::Stopped(reason), None);
                    }
                    let step = self.execute_tools(content).await;
                    if !step.items.is_empty() {
                        self.steps.push(step);
                    }
                    if self.context.cancellation.is_cancelled() {
                        return (TurnResult::Cancelled, None);
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

    async fn prepare_context(&mut self) -> Result<(), AshError> {
        if self.pending_summary.is_some() {
            return Ok(());
        }
        let tools = self.agent.tool_definitions();
        let before =
            estimate_request_tokens(self.agent.system_prompt(), &self.model_context(), &tools);
        if !needs_compaction(before, self.agent.max_context_tokens()) {
            return Ok(());
        }
        let Some((summary, stats)) = compact(
            self.model,
            self.agent,
            self.conversation,
            &self.context.cancellation,
        )
        .await?
        else {
            return Ok(());
        };
        self.stats = self.stats.saturating_add(stats);
        let after = estimate_request_tokens(
            self.agent.system_prompt(),
            &self
                .conversation
                .context_with_summary(&summary)
                .with_current(self.input.clone(), self.steps.clone()),
            &tools,
        );
        if after < before {
            self.pending_summary = Some(summary);
        }
        Ok(())
    }

    fn push_without_tools(&mut self, content: Vec<ContentBlock>) {
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
        if !items.is_empty() {
            self.steps.push(Step { items });
        }
    }

    async fn execute_tools(&self, content: Vec<ContentBlock>) -> Step {
        let calls = content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.clone(), name.clone(), arguments.clone())),
            ContentBlock::Text(_) | ContentBlock::Thought { .. } => None,
        });
        let results = join_all(
            calls.map(|(id, name, arguments)| self.execute_tool_call(id, name, arguments)),
        )
        .await;
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
        ProtocolError::Request(_) | ProtocolError::RateLimited
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
    use std::sync::Mutex as StdMutex;

    use ash_core::{define_tool, ModelId};

    use super::*;

    struct MockModel {
        response: StdMutex<Option<Vec<Result<ModelEvent, ProtocolError>>>>,
        requests: StdMutex<Vec<ModelRequest>>,
    }

    struct PendingModel {
        started: tokio::sync::Notify,
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
                response: StdMutex::new(Some(events.into_iter().map(Ok).collect())),
                requests: StdMutex::new(Vec::new()),
            }
        }
    }

    impl ModelClient for MockModel {
        fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let response = self.response.lock().unwrap().take().unwrap();
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
            events,
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
                events,
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
