use ash_core::{
    AgentToolContext, CancellationToken, ContentBlock, Event, Message, MessageContent, ModelClient,
    ModelRequest, ModelStream, ModelStreamEvent, Role, RunId, SessionId, StopReason, ToolCallId,
    ToolContext, ToolDefinition, ToolError, ToolOutput, TurnId,
};
use futures::StreamExt;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::debug;

#[cfg(test)]
use crate::context_policy::COMPACTION_SYSTEM_PROMPT;
use crate::{
    context::{count_output_tokens, estimate_request_tokens},
    context_policy::{CodingContextPolicy, ContextRequest},
    conversation::{ContextCheckpoint, ConversationEntry},
    AgentConfig, ConversationStore,
};

const MAX_AGENT_OUTPUT_BYTES: usize = 64 * 1024;
const AGENT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const AGENT_OUTPUT_TRUNCATION_NOTICE: &str = "\n... tool output truncated by the agent ...\n";

struct PendingToolCall {
    id: ToolCallId,
    name: String,
    arguments: serde_json::Value,
}

struct CollectedResponse {
    message: Option<Message>,
    usage: UsageAccumulator,
    outcome: ResponseOutcome,
}

enum StreamTermination {
    Completed(StopReason),
    Cancelled,
    Failed(ash_core::ProtocolError),
}

enum ResponseOutcome {
    Finished(StopReason),
    ToolCalls {
        calls: Vec<PendingToolCall>,
        after_tools: AfterToolCalls,
    },
    Failed(ash_core::ProtocolError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfterToolCalls {
    Continue,
    Abort,
}

pub(crate) struct CompactedHistory {
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

struct FinalUsage {
    input_tokens: u64,
    output_tokens: u64,
    generation_ms: u64,
    estimated: bool,
}

struct AgentTurnRunner<'config, 'store> {
    config: &'config AgentConfig,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    ids: ExecutionIds,
    model: &'config dyn ModelClient,
    store: Option<&'store mut dyn ConversationStore>,
    tool_defs: Vec<ToolDefinition>,
}

#[derive(Clone, Copy)]
pub(crate) struct ExecutionIds {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
}

impl ExecutionIds {
    fn for_session(session_id: SessionId) -> Self {
        Self {
            session_id,
            run_id: RunId::new(),
            turn_id: TurnId::new(),
        }
    }
}

impl StreamTermination {
    fn resolve(self, calls: Vec<PendingToolCall>) -> ResponseOutcome {
        match (self, calls.is_empty()) {
            (Self::Completed(reason), true) => ResponseOutcome::Finished(reason),
            (Self::Completed(_), false) => ResponseOutcome::ToolCalls {
                calls,
                after_tools: AfterToolCalls::Continue,
            },
            (Self::Cancelled, true) => ResponseOutcome::Finished(StopReason::Aborted),
            (Self::Cancelled, false) => ResponseOutcome::ToolCalls {
                calls,
                after_tools: AfterToolCalls::Abort,
            },
            (Self::Failed(error), _) => ResponseOutcome::Failed(error),
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

    fn finalize(self, input_tokens: u64, output_tokens: u64) -> FinalUsage {
        let estimated =
            self.input_tokens.is_none() || (self.output_tokens.is_none() && output_tokens > 0);
        FinalUsage {
            input_tokens: self.input_tokens.unwrap_or(input_tokens),
            output_tokens: self.output_tokens.unwrap_or(output_tokens),
            generation_ms: self.generation_ms,
            estimated,
        }
    }
}

pub(crate) async fn compact_with_adapter(
    config: &AgentConfig,
    messages: &[Message],
    model: &dyn ModelClient,
    cancel: &CancellationToken,
) -> Result<Option<CompactedHistory>, ash_core::AshError> {
    let tools = config
        .tools
        .iter()
        .map(|tool| tool.definition())
        .collect::<Vec<_>>();
    let update = CodingContextPolicy
        .compact(
            ContextRequest {
                model: config.model.clone(),
                system_prompt: config.system_prompt.clone(),
                messages: messages.to_vec(),
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

pub async fn run_agent_loop(
    model: std::sync::Arc<dyn ModelClient>,
    config: AgentConfig,
    mut messages: Vec<Message>,
    tx: mpsc::Sender<Event>,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn(
        model.as_ref(),
        &config,
        &mut messages,
        tx,
        CancellationToken::new(),
        SessionId::new(),
    )
    .await
}

pub async fn run_agent_turn(
    model: &dyn ModelClient,
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn_identified(
        model,
        config,
        messages,
        tx,
        cancel,
        ExecutionIds::for_session(session_id),
    )
    .await
}

pub(crate) async fn run_agent_turn_identified(
    model: &dyn ModelClient,
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    ids: ExecutionIds,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn_inner(model, ids, config, messages, tx, cancel, None).await
}

pub(crate) async fn run_agent_turn_persisted(
    model: &dyn ModelClient,
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
    store: &mut dyn ConversationStore,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn_inner(
        model,
        ExecutionIds::for_session(session_id),
        config,
        messages,
        tx,
        cancel,
        Some(store),
    )
    .await
}

async fn run_agent_turn_inner(
    model: &dyn ModelClient,
    ids: ExecutionIds,
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    store: Option<&mut dyn ConversationStore>,
) -> Result<StopReason, ash_core::AshError> {
    let _ = tx
        .send(Event::AgentStarted {
            session_id: ids.session_id,
        })
        .await;
    let result =
        run_with_identifiers(config, messages, tx.clone(), cancel, ids, model, store).await;

    match result {
        Ok(reason) => {
            let _ = tx
                .send(Event::AgentFinished {
                    reason: reason.clone(),
                })
                .await;
            Ok(reason)
        }
        Err(error) => {
            let _ = tx.send(Event::Error(error.to_string())).await;
            let _ = tx
                .send(Event::AgentFinished {
                    reason: StopReason::Aborted,
                })
                .await;
            Err(error)
        }
    }
}

#[cfg(test)]
async fn run_with_adapter(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
    model: &dyn ModelClient,
    store: Option<&mut dyn ConversationStore>,
) -> Result<StopReason, ash_core::AshError> {
    run_with_identifiers(
        config,
        messages,
        tx,
        cancel,
        ExecutionIds::for_session(session_id),
        model,
        store,
    )
    .await
}

async fn run_with_identifiers(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    ids: ExecutionIds,
    model: &dyn ModelClient,
    store: Option<&mut dyn ConversationStore>,
) -> Result<StopReason, ash_core::AshError> {
    AgentTurnRunner::new(config, tx, cancel, ids, model, store)
        .run(messages)
        .await
}

impl<'config, 'store> AgentTurnRunner<'config, 'store> {
    fn new(
        config: &'config AgentConfig,
        tx: mpsc::Sender<Event>,
        cancel: CancellationToken,
        ids: ExecutionIds,
        model: &'config dyn ModelClient,
        store: Option<&'store mut dyn ConversationStore>,
    ) -> Self {
        let tool_defs = config.tools.iter().map(|tool| tool.definition()).collect();
        Self {
            config,
            tx,
            cancel,
            ids,
            model,
            store,
            tool_defs,
        }
    }

    async fn run(&mut self, messages: &mut Vec<Message>) -> Result<StopReason, ash_core::AshError> {
        for turn in 0..self.config.max_turns {
            if self.cancel.is_cancelled() {
                return Ok(StopReason::Aborted);
            }
            debug!(turn = turn + 1, "calling LLM");

            self.prepare_context(messages).await?;

            let request_messages = messages.clone();
            let estimated_input_tokens = u64::try_from(estimate_request_tokens(
                self.config.system_prompt.as_deref(),
                &request_messages,
                &self.tool_defs,
            ))
            .unwrap_or(u64::MAX);
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
            let estimated_output_tokens = message
                .as_ref()
                .map(count_output_tokens)
                .and_then(|tokens| u64::try_from(tokens).ok())
                .unwrap_or(0);
            self.emit_usage(usage.finalize(estimated_input_tokens, estimated_output_tokens))
                .await;
            if let Some(message) = message {
                self.persist_message(&message).await?;
                messages.push(message);
            }

            match outcome {
                ResponseOutcome::Finished(reason) => return Ok(reason),
                ResponseOutcome::Failed(error) => return Err(error.into()),
                ResponseOutcome::ToolCalls { calls, after_tools } => {
                    self.execute_tool_calls(messages, calls).await?;
                    if after_tools == AfterToolCalls::Abort || self.cancel.is_cancelled() {
                        return Ok(StopReason::Aborted);
                    }
                }
            }
        }

        Ok(StopReason::MaxTurns)
    }

    async fn prepare_context(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<(), ash_core::AshError> {
        let prepared = self
            .config
            .context_policy
            .prepare(
                ContextRequest {
                    model: self.config.model.clone(),
                    system_prompt: self.config.system_prompt.clone(),
                    messages: std::mem::take(messages),
                    tools: self.tool_defs.clone(),
                    max_context_tokens: self.config.max_context_tokens,
                },
                self.model,
                &self.cancel,
            )
            .await?;
        let update = prepared.update;
        *messages = prepared.messages;
        let Some(update) = update else {
            return Ok(());
        };
        if let Some(store) = self.store.as_deref_mut() {
            let revision = store.revision();
            store
                .append(
                    revision,
                    ConversationEntry::ContextCheckpoint(ContextCheckpoint::from_model_context(
                        messages,
                    )?),
                )
                .await?;
        }
        let _ = self
            .tx
            .send(Event::ContextCompacted {
                before_tokens: u64::try_from(update.before_tokens).unwrap_or(u64::MAX),
                after_tokens: u64::try_from(update.after_tokens).unwrap_or(u64::MAX),
                dropped_messages: u64::try_from(update.dropped_messages).unwrap_or(u64::MAX),
                automatic: true,
            })
            .await;
        Ok(())
    }

    async fn emit_usage(&mut self, usage: FinalUsage) {
        let _ = self
            .tx
            .send(Event::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                generation_ms: usage.generation_ms,
                estimated: usage.estimated,
            })
            .await;
    }

    async fn persist_message(&mut self, message: &Message) -> Result<(), ash_core::AshError> {
        match self.store.as_deref_mut() {
            Some(store) => {
                let revision = store.revision();
                store
                    .append(revision, ConversationEntry::Message(message.clone()))
                    .await
                    .map(|_| ())
            }
            None => Ok(()),
        }
    }

    async fn execute_tool_calls(
        &mut self,
        messages: &mut Vec<Message>,
        calls: Vec<PendingToolCall>,
    ) -> Result<(), ash_core::AshError> {
        for call in calls {
            let _ = self
                .tx
                .send(Event::ToolCallStart {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await;
            let result = limit_tool_result(
                self.execute_tool(messages, &call.name, call.arguments.clone())
                    .await,
            );
            let (output, is_error) = match &result {
                Ok(output) => (output.text.clone(), false),
                Err(error) => (error.to_string(), true),
            };
            let _ = self
                .tx
                .send(Event::ToolCallEnd {
                    id: call.id.clone(),
                    name: call.name,
                    arguments: call.arguments,
                    output,
                    is_error,
                })
                .await;
            let (result, attachments) = match result {
                Ok(output) => (Ok(output.text), output.attachments),
                Err(error) => (Err(error.to_string()), Vec::new()),
            };
            let message = Message {
                id: ash_core::MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: call.id,
                    result,
                    attachments,
                },
            };
            self.persist_message(&message).await?;
            messages.push(message);
        }
        Ok(())
    }

    async fn execute_tool(
        &mut self,
        messages: &[Message],
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let Some(tool) = self.config.tools.iter().find(|tool| tool.name() == name) else {
            return Err(ToolError::Execution(format!("unknown tool: {name}")));
        };
        let context = ToolContext {
            session_id: self.ids.session_id,
            run_id: self.ids.run_id,
            turn_id: self.ids.turn_id,
            cancellation: self.cancel.clone(),
            deadline: Instant::now() + self.config.max_tool_duration,
            agent: AgentToolContext {
                root_session_id: self.config.root_session_id.unwrap_or(self.ids.session_id),
                agent_path: self.config.agent_path.clone(),
                messages: messages.to_vec(),
            },
        };

        tokio::select! {
            _ = self.cancel.cancelled() => Err(ToolError::Cancelled),
            result = tokio::time::timeout(
                self.config.max_tool_duration,
                tool.execute(context, arguments),
            ) => result.unwrap_or(Err(ToolError::Timeout(self.config.max_tool_duration)))
        }
    }
}

async fn collect_response(
    stream: &mut ModelStream,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
    request_started: Instant,
) -> CollectedResponse {
    let mut blocks = Vec::new();
    let mut thought_started_at = None;
    let mut termination = StreamTermination::Completed(StopReason::EndTurn);
    let mut usage = UsageAccumulator::default();
    let mut first_output_at = None;

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                termination = StreamTermination::Cancelled;
                None
            },
            next = stream.next() => next,
        };
        let Some(item) = next else {
            break;
        };
        let item = match item {
            Ok(item) => item,
            Err(stream_error) => {
                termination = StreamTermination::Failed(stream_error);
                break;
            }
        };
        match item {
            ModelStreamEvent::TextDelta(delta) => {
                first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut blocks, &mut thought_started_at);
                match blocks.last_mut() {
                    Some(ContentBlock::Text(text)) => text.push_str(&delta),
                    _ => blocks.push(ContentBlock::Text(delta.clone())),
                }
                let _ = tx.send(Event::TextDelta(delta)).await;
            }
            ModelStreamEvent::ThinkingDelta(delta) => {
                first_output_at.get_or_insert_with(Instant::now);
                match blocks.last_mut() {
                    Some(ContentBlock::Thought { text, .. }) => text.push_str(&delta),
                    _ => {
                        thought_started_at = Some(Instant::now());
                        blocks.push(ContentBlock::Thought {
                            text: delta.clone(),
                            elapsed_seconds: 0,
                        });
                    }
                }
                let _ = tx.send(Event::Thinking(delta)).await;
            }
            ModelStreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut blocks, &mut thought_started_at);
                blocks.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            ModelStreamEvent::Usage {
                input_tokens,
                output_tokens,
            } => usage.record(input_tokens, output_tokens),
            ModelStreamEvent::Stop(reason) => termination = StreamTermination::Completed(reason),
        }
    }
    finish_open_thought(&mut blocks, &mut thought_started_at);

    let calls = blocks
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
        .collect();
    let message = (!blocks.is_empty()).then(|| Message {
        id: ash_core::MessageId::new(),
        role: Role::Assistant,
        content: MessageContent::Assistant(blocks),
    });
    CollectedResponse {
        message,
        usage: UsageAccumulator {
            generation_ms: u64::try_from(
                first_output_at
                    .unwrap_or(request_started)
                    .elapsed()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            ..usage
        },
        outcome: termination.resolve(calls),
    }
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

pub struct Agent;

impl Agent {
    pub fn run(
        model: std::sync::Arc<dyn ModelClient>,
        config: AgentConfig,
        messages: Vec<Message>,
    ) -> impl futures::Stream<Item = Event> {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let _ = run_agent_loop(model, config, messages, tx).await;
        });
        ReceiverStream::new(rx)
    }
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
        Content, ModelClient as ProtocolAdapter, ModelId, ModelRequest as LlmRequest,
        ModelStream as ProtocolStream, ModelStreamEvent as StreamItem, Tool, ToolCallId,
        ToolContext, ToolError,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::session_store::SessionStore;

    struct MockAdapter {
        responses: Mutex<VecDeque<Vec<StreamItem>>>,
        requests: Arc<Mutex<Vec<LlmRequest>>>,
    }

    impl ProtocolAdapter for MockAdapter {
        fn stream(&self, req: LlmRequest) -> Result<ProtocolStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(req);
            let items = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(items.into_iter().map(Ok))))
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
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
        fn name(&self) -> &str {
            "blocking"
        }

        fn description(&self) -> &str {
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
                    StreamItem::ToolCall {
                        id: ToolCallId::from_provider("call_1"),
                        name: "echo".into(),
                        arguments: serde_json::json!({"value": "hello"}),
                    },
                    StreamItem::Stop(StopReason::EndTurn),
                ],
                vec![
                    StreamItem::TextDelta("done".into()),
                    StreamItem::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = AgentConfig {
            system_prompt: None,
            tools: vec![Arc::new(EchoTool)],
            model: ModelId::new("test-model"),
            max_turns: 4,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
        };
        let mut messages = vec![Message::user("use a tool")];
        let directory = TempDir::new().unwrap();
        let mut store = SessionStore::new_in(&config, SessionId::new(), directory.path());
        store.append_message(&messages[0]).await.unwrap();
        let (tx, _rx) = mpsc::channel(32);

        let reason = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            Some(&mut store),
        )
        .await
        .unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap()[1].messages.len(), 3);
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            messages[2].content,
            MessageContent::ToolResult { .. }
        ));
        let persisted = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(persisted.contains("assistant_message"));
        assert!(persisted.contains("tool_result"));
    }

    #[tokio::test]
    async fn automatically_compacts_at_eighty_percent_before_the_model_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([
                vec![
                    StreamItem::TextDelta("condensed facts".into()),
                    StreamItem::Stop(StopReason::EndTurn),
                ],
                vec![
                    StreamItem::TextDelta("done".into()),
                    StreamItem::Stop(StopReason::EndTurn),
                ],
            ])),
            requests: requests.clone(),
        };
        let config = AgentConfig {
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 1_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
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
        let mut store = SessionStore::new_in(&config, SessionId::new(), directory.path());
        for message in &messages {
            store.append_message(message).await.unwrap();
        }
        let (tx, mut rx) = mpsc::channel(16);

        let reason = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            Some(&mut store),
        )
        .await
        .unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        let stored = store.load().await.unwrap();
        assert_eq!(stored.messages().len(), 7);
        assert_eq!(stored.model_context().len(), 6);
        assert!(matches!(
            &stored.messages()[0].content,
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
        }
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(
                event,
                Event::ContextCompacted {
                    automatic: true,
                    dropped_messages: 2,
                    ..
                }
            ))
        );
    }

    #[tokio::test]
    async fn prunes_large_old_tool_outputs_before_full_compaction() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                StreamItem::TextDelta("done".into()),
                StreamItem::Stop(StopReason::EndTurn),
            ]])),
            requests: requests.clone(),
        };
        let config = AgentConfig {
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 120_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
        };
        let mut messages = Vec::new();
        for turn in 0..7 {
            let call = ToolCallId::from_provider(format!("call-{turn}"));
            messages.push(Message::user(&format!("request {turn}")));
            messages.push(Message {
                id: ash_core::MessageId::new(),
                role: Role::Assistant,
                content: MessageContent::Assistant(vec![ContentBlock::ToolCall {
                    id: call.clone(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                }]),
            });
            messages.push(Message {
                id: ash_core::MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: call,
                    result: Ok("x".repeat(64_000)),
                    attachments: Vec::new(),
                },
            });
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
    }

    #[tokio::test]
    async fn aggregates_usage_for_each_model_call() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                StreamItem::Usage {
                    input_tokens: 120,
                    output_tokens: 0,
                },
                StreamItem::ThinkingDelta("checking".into()),
                StreamItem::Usage {
                    input_tokens: 0,
                    output_tokens: 25,
                },
                StreamItem::Stop(StopReason::EndTurn),
            ]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = AgentConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
        };
        let mut messages = vec![Message::user("question")];
        let (tx, mut rx) = mpsc::channel(8);

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

        let usage = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|event| {
            if let Event::Usage {
                input_tokens,
                output_tokens,
                generation_ms,
                estimated,
            } = event
            {
                Some((input_tokens, output_tokens, generation_ms, estimated))
            } else {
                None
            }
        });

        assert!(matches!(usage, Some((120, 25, _, false))));
        assert!(rx.try_recv().is_err());
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
    async fn persists_reasoning_blocks_in_session_history() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                StreamItem::ThinkingDelta("inspect first".into()),
                StreamItem::TextDelta("done".into()),
                StreamItem::Stop(StopReason::EndTurn),
            ]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = AgentConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let mut store = SessionStore::new_in(&config, SessionId::new(), directory.path());
        store.append_message(&messages[0]).await.unwrap();
        let (tx, _rx) = mpsc::channel(32);

        run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &adapter,
            Some(&mut store),
        )
        .await
        .unwrap();

        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [
                    ContentBlock::Thought { text, .. },
                    ContentBlock::Text(answer),
                ] if text == "inspect first" && answer == "done")
        ));
        let persisted = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(persisted.contains("inspect first"));
        assert!(persisted.contains("elapsed_seconds"));
    }

    #[tokio::test]
    async fn cancellation_preserves_partial_assistant_text() {
        struct PendingAdapter;

        impl ProtocolAdapter for PendingAdapter {
            fn stream(&self, _req: LlmRequest) -> Result<ProtocolStream, ash_core::ProtocolError> {
                Ok(Box::pin(
                    futures::stream::iter([Ok(StreamItem::TextDelta("partial".into()))])
                        .chain(futures::stream::pending()),
                ))
            }
        }

        let config = AgentConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
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
            assert!(matches!(event, Some(Event::TextDelta(text)) if text == "partial"));
            cancel.cancel();
            (&mut turn).await.unwrap()
        };
        assert_eq!(reason, StopReason::Aborted);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "partial")
        ));
    }

    #[tokio::test]
    async fn protocol_errors_preserve_partial_assistant_content() {
        struct FailingAdapter;

        impl ProtocolAdapter for FailingAdapter {
            fn stream(&self, _req: LlmRequest) -> Result<ProtocolStream, ash_core::ProtocolError> {
                Ok(Box::pin(futures::stream::iter([
                    Ok(StreamItem::ThinkingDelta("checking".into())),
                    Ok(StreamItem::TextDelta("partial".into())),
                    Err(ash_core::ProtocolError::InvalidResponse(
                        "stream ended badly".into(),
                    )),
                ])))
            }
        }

        let config = AgentConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let mut store = SessionStore::new_in(&config, SessionId::new(), directory.path());
        store.append_message(&messages[0]).await.unwrap();
        let (tx, _rx) = mpsc::channel(8);

        let error = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            SessionId::new(),
            &FailingAdapter,
            Some(&mut store),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ash_core::AshError::Protocol(_)));
        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [
                    ContentBlock::Thought { text, .. },
                    ContentBlock::Text(answer),
                ] if text == "checking" && answer == "partial")
        ));
        let persisted = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(persisted.contains("checking"));
        assert!(persisted.contains("partial"));
    }

    #[tokio::test]
    async fn cancellation_completes_an_active_tool_call() {
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                StreamItem::ToolCall {
                    id: ToolCallId::from_provider("call_1"),
                    name: "blocking".into(),
                    arguments: serde_json::json!({}),
                },
                StreamItem::Stop(StopReason::EndTurn),
            ]])),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let config = AgentConfig {
            system_prompt: None,
            tools: vec![Arc::new(BlockingTool)],
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 200_000,
            context_policy: Arc::new(crate::CodingContextPolicy),
            max_tool_duration: Duration::from_secs(30),
            agent_path: "/root".to_string(),
            root_session_id: None,
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
                if matches!(event, Some(Event::ToolCallStart { .. })) {
                    break;
                }
            }
            cancel.cancel();
            (&mut turn).await.unwrap()
        };

        assert_eq!(reason, StopReason::Aborted);
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[2].content,
            MessageContent::ToolResult { result: Err(error), .. } if error == "cancelled"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(Event::ToolCallEnd { is_error: true, .. })
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
}
