use ash_core::{
    AgentToolContext, CancellationToken, ContentBlock, EventKind, LiveEvent, Message,
    MessageContent, ModelClient, ModelEvent, ModelRequest, ModelStream, Role, StopReason, ThreadId,
    ToolCallId, ToolContext, ToolDefinition, ToolError, ToolOutput, TurnId, Usage,
};
use futures::StreamExt;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

#[cfg(test)]
use crate::context_policy::COMPACTION_SYSTEM_PROMPT;
use crate::store::ThreadPersistence;
use crate::{
    context::{count_output_tokens, estimate_request_tokens},
    context_policy::{ContextRequest, DefaultContextPolicy},
    log::{ContextCheckpoint, LogEntry},
    AcceptedInput, Input, RunConfig,
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

enum ResponseOutcome {
    Finished(StopReason),
    ToolCalls(Vec<PendingToolCall>),
    Failed(ash_core::ProtocolError),
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

struct AgentTurnRunner<'config, 'store> {
    config: &'config RunConfig,
    tx: mpsc::Sender<EventKind>,
    cancel: CancellationToken,
    ids: ExecutionIds,
    model: &'config dyn ModelClient,
    persistence: Option<&'store mut ThreadPersistence>,
    steering: mpsc::UnboundedReceiver<Input>,
    tool_defs: Vec<ToolDefinition>,
}

#[derive(Clone, Copy)]
pub(crate) struct ExecutionIds {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}

pub(crate) struct TurnExecution {
    ids: ExecutionIds,
    tx: mpsc::Sender<EventKind>,
    cancel: CancellationToken,
    steering: mpsc::UnboundedReceiver<Input>,
}

impl ExecutionIds {
    fn new(thread_id: ThreadId, turn_id: TurnId) -> Self {
        Self { thread_id, turn_id }
    }
}

impl TurnExecution {
    pub(crate) fn new(
        thread_id: ThreadId,
        turn_id: TurnId,
        tx: mpsc::Sender<EventKind>,
        cancel: CancellationToken,
        steering: mpsc::UnboundedReceiver<Input>,
    ) -> Self {
        Self {
            ids: ExecutionIds::new(thread_id, turn_id),
            tx,
            cancel,
            steering,
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

pub(crate) async fn compact_with_adapter(
    config: &RunConfig,
    messages: &[Message],
    model: &dyn ModelClient,
    cancel: &CancellationToken,
) -> Result<Option<CompactedHistory>, ash_core::AshError> {
    let tools = config
        .tools
        .iter()
        .map(|tool| tool.definition())
        .collect::<Vec<_>>();
    let update = DefaultContextPolicy
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

pub(crate) async fn run_agent_turn_persisted(
    model: &dyn ModelClient,
    config: &RunConfig,
    messages: &mut Vec<Message>,
    execution: TurnExecution,
    persistence: &mut ThreadPersistence,
) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
    run_agent_turn_inner(model, config, messages, execution, Some(persistence)).await
}

async fn run_agent_turn_inner(
    model: &dyn ModelClient,
    config: &RunConfig,
    messages: &mut Vec<Message>,
    execution: TurnExecution,
    persistence: Option<&mut ThreadPersistence>,
) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
    AgentTurnRunner::new(config, model, persistence, execution)
        .run(messages)
        .await
}

#[cfg(test)]
async fn run_with_adapter(
    config: &RunConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<EventKind>,
    cancel: CancellationToken,
    thread_id: ThreadId,
    model: &dyn ModelClient,
    persistence: Option<&mut ThreadPersistence>,
) -> Result<(StopReason, Option<Usage>), ash_core::AshError> {
    let (_, steering) = mpsc::unbounded_channel();
    let execution = TurnExecution::new(thread_id, TurnId::new(), tx, cancel, steering);
    run_agent_turn_inner(model, config, messages, execution, persistence).await
}

impl<'config, 'store> AgentTurnRunner<'config, 'store> {
    fn new(
        config: &'config RunConfig,
        model: &'config dyn ModelClient,
        persistence: Option<&'store mut ThreadPersistence>,
        execution: TurnExecution,
    ) -> Self {
        let tool_defs = config.tools.iter().map(|tool| tool.definition()).collect();
        Self {
            config,
            tx: execution.tx,
            cancel: execution.cancel,
            ids: execution.ids,
            model,
            persistence,
            steering: execution.steering,
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
            let call_usage = usage.finalize(estimated_input_tokens, estimated_output_tokens);
            turn_usage = Some(match turn_usage {
                Some(acc) => Usage {
                    input_tokens: acc.input_tokens.saturating_add(call_usage.input_tokens),
                    output_tokens: acc.output_tokens.saturating_add(call_usage.output_tokens),
                    generation_ms: acc.generation_ms.saturating_add(call_usage.generation_ms),
                    estimated: acc.estimated || call_usage.estimated,
                },
                None => call_usage,
            });
            if let Some(message) = message {
                self.persist_message(&message).await?;
                messages.push(message);
            }

            let mut stop_reason = None;
            match outcome {
                ResponseOutcome::Finished(reason) => stop_reason = Some(reason),
                ResponseOutcome::Failed(error) => return Err(error.into()),
                ResponseOutcome::ToolCalls(calls) => {
                    self.execute_tool_calls(messages, calls).await?;
                    if self.cancel.is_cancelled() {
                        return Ok((StopReason::Aborted, turn_usage));
                    }
                }
            }
            let has_steering = self.apply_steering(messages).await?;
            if let Some(reason) = stop_reason {
                if !has_steering {
                    return Ok((reason, turn_usage));
                }
            }
        }

        Ok((StopReason::MaxTurns, turn_usage))
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
        if let Some(persistence) = self.persistence.as_deref_mut() {
            persistence
                .append(
                    &[LogEntry::Checkpoint(ContextCheckpoint::from_model_context(
                        messages,
                    )?)],
                )
                .await?;
        }
        let _ = self
            .tx
            .send(EventKind::Compacted {
                before: u64::try_from(update.before_tokens).unwrap_or(u64::MAX),
                after: u64::try_from(update.after_tokens).unwrap_or(u64::MAX),
                dropped: u64::try_from(update.dropped_messages).unwrap_or(u64::MAX),
                automatic: true,
            })
            .await;
        Ok(())
    }

    async fn persist_message(&mut self, message: &Message) -> Result<(), ash_core::AshError> {
        match self.persistence.as_deref_mut() {
            Some(persistence) => {
                persistence
                    .append(&[LogEntry::Message(message.clone())])
                    .await
            }
            None => Ok(()),
        }
    }

    async fn apply_steering(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<bool, ash_core::AshError> {
        let mut accepted = Vec::new();
        while let Ok(input) = self.steering.try_recv() {
            if input.is_empty() {
                continue;
            }
            let message = Message::user_content(input.content.clone());
            accepted.push((input, message));
        }
        if accepted.is_empty() {
            return Ok(false);
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
            persistence.append(&records).await?;
        }
        messages.extend(accepted.into_iter().map(|(_, message)| message));
        Ok(true)
    }

    async fn execute_tool_calls(
        &mut self,
        messages: &mut Vec<Message>,
        calls: Vec<PendingToolCall>,
    ) -> Result<(), ash_core::AshError> {
        for call in calls {
            let _ = self
                .tx
                .send(EventKind::Live(LiveEvent::ToolStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                }))
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
                .send(EventKind::Live(LiveEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name,
                    arguments: call.arguments,
                    output,
                    is_error,
                }))
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
            thread_id: self.ids.thread_id,
            turn_id: self.ids.turn_id,
            cancellation: self.cancel.clone(),
            deadline: Instant::now() + self.config.max_tool_duration,
            agent: AgentToolContext {
                tree_id: self
                    .config
                    .tree_id
                    .unwrap_or_else(|| self.ids.thread_id.into()),
                path: self.config.agent_path.clone(),
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
    tx: &mpsc::Sender<EventKind>,
    cancel: &CancellationToken,
    request_started: Instant,
) -> CollectedResponse {
    let mut blocks = Vec::new();
    let mut thought_started_at = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut cancelled = false;
    let mut failure = None;
    let mut usage = UsageAccumulator::default();
    let mut first_output_at = None;

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                cancelled = true;
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
                failure = Some(stream_error);
                break;
            }
        };
        match item {
            ModelEvent::Text(delta) => {
                first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut blocks, &mut thought_started_at);
                match blocks.last_mut() {
                    Some(ContentBlock::Text(text)) => text.push_str(&delta),
                    _ => blocks.push(ContentBlock::Text(delta.clone())),
                }
                let _ = tx.send(EventKind::Live(LiveEvent::TextDelta(delta))).await;
            }
            ModelEvent::Reasoning(delta) => {
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
                let _ = tx
                    .send(EventKind::Live(LiveEvent::ReasoningDelta(delta)))
                    .await;
            }
            ModelEvent::ToolCall {
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
            ModelEvent::Usage(reported) => {
                usage.record(reported.input_tokens, reported.output_tokens)
            }
            ModelEvent::Stop(reason) => stop_reason = reason,
        }
    }
    finish_open_thought(&mut blocks, &mut thought_started_at);

    let calls: Vec<PendingToolCall> = blocks
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
    let outcome = if let Some(error) = failure {
        ResponseOutcome::Failed(error)
    } else if calls.is_empty() {
        ResponseOutcome::Finished(if cancelled {
            StopReason::Aborted
        } else {
            stop_reason
        })
    } else {
        ResponseOutcome::ToolCalls(calls)
    };

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
        outcome,
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use ash_core::{
        Content, ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, Tool, ToolCallId,
        ToolContext, ToolError,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{JsonlThreadStore, SharedThreadStore, StoredThread, ThreadMetadata};

    fn metadata(config: &RunConfig, thread_id: ThreadId) -> ThreadMetadata {
        ThreadMetadata {
            thread_id,
            model_backend: "test".to_string(),
            model: config.model.clone(),
            working_dir: config.working_dir.clone(),
            system_prompt: config.system_prompt.clone(),
            max_turns: config.max_turns,
            max_context_tokens: config.max_context_tokens,
            max_tool_duration: config.max_tool_duration,
            kind: crate::ThreadKind::Root,
        }
    }

    async fn persisted_thread(
        config: &RunConfig,
        directory: &std::path::Path,
        messages: &[Message],
    ) -> (ThreadId, SharedThreadStore, ThreadPersistence) {
        let thread_id = ThreadId::new();
        let store: SharedThreadStore = Arc::new(JsonlThreadStore::new(directory));
        let records = messages
            .iter()
            .cloned()
            .map(LogEntry::Message)
            .collect::<Vec<_>>();
        store
            .create(metadata(config, thread_id), &records)
            .await
            .unwrap();
        let writer = Arc::new(tokio::sync::Mutex::new(
            store
                .open_writer(thread_id, metadata(config, thread_id))
                .await
                .unwrap(),
        ));
        let persistence = ThreadPersistence::new(writer).await;
        (thread_id, store, persistence)
    }

    async fn load_thread(store: &SharedThreadStore, thread_id: ThreadId) -> StoredThread {
        store.load(thread_id).await.unwrap().unwrap()
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
        };
        let mut messages = vec![Message::user("use a tool")];
        let directory = TempDir::new().unwrap();
        let (thread_id, store, mut persistence) =
            persisted_thread(&config, directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(32);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            thread_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.flush().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(requests.lock().unwrap()[1].messages.len(), 3);
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            messages[2].content,
            MessageContent::ToolResult { .. }
        ));
        let persisted = load_thread(&store, thread_id).await;
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
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
        let (thread_id, store, mut persistence) =
            persisted_thread(&config, directory.path(), &messages).await;
        let (tx, mut rx) = mpsc::channel(16);

        let (reason, _usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            thread_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.flush().await.unwrap();

        assert_eq!(reason, StopReason::EndTurn);
        let stored = load_thread(&store, thread_id).await;
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
        }
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(
                event,
                EventKind::Compacted {
                    automatic: true,
                    dropped: 2,
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
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
            ThreadId::new(),
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
        };
        let mut messages = vec![Message::user("question")];
        let (tx, _rx) = mpsc::channel(8);

        let (reason, usage) = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            ThreadId::new(),
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
    async fn persists_reasoning_blocks_in_thread_history() {
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let (thread_id, store, mut persistence) =
            persisted_thread(&config, directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(32);

        run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            thread_id,
            &adapter,
            Some(&mut persistence),
        )
        .await
        .unwrap();
        persistence.flush().await.unwrap();

        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [
                    ContentBlock::Thought { text, .. },
                    ContentBlock::Text(answer),
                ] if text == "inspect first" && answer == "done")
        ));
        let persisted = serde_json::to_string(&load_thread(&store, thread_id).await.log).unwrap();
        assert!(persisted.contains("inspect first"));
        assert!(persisted.contains("elapsed_seconds"));
    }

    #[tokio::test]
    async fn cancellation_preserves_partial_assistant_text() {
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
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
                ThreadId::new(),
                &PendingAdapter,
                None,
            );
            tokio::pin!(turn);

            let event = tokio::select! {
                event = rx.recv() => event,
                result = &mut turn => panic!("turn ended before cancellation: {result:?}"),
            };
            assert!(
                matches!(event, Some(EventKind::Live(LiveEvent::TextDelta(text))) if text == "partial")
            );
            cancel.cancel();
            (&mut turn).await.unwrap().0
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
        };
        let mut messages = vec![Message::user("question")];
        let directory = TempDir::new().unwrap();
        let (thread_id, store, mut persistence) =
            persisted_thread(&config, directory.path(), &messages).await;
        let (tx, _rx) = mpsc::channel(8);

        let error = run_with_adapter(
            &config,
            &mut messages,
            tx,
            CancellationToken::new(),
            thread_id,
            &FailingAdapter,
            Some(&mut persistence),
        )
        .await
        .unwrap_err();
        persistence.flush().await.unwrap();

        assert!(matches!(error, ash_core::AshError::Protocol(_)));
        assert!(matches!(
            &messages[1].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [
                    ContentBlock::Thought { text, .. },
                    ContentBlock::Text(answer),
                ] if text == "checking" && answer == "partial")
        ));
        let persisted = serde_json::to_string(&load_thread(&store, thread_id).await.log).unwrap();
        assert!(persisted.contains("checking"));
        assert!(persisted.contains("partial"));
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
            agent_path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
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
                ThreadId::new(),
                &adapter,
                None,
            );
            tokio::pin!(turn);

            loop {
                let event = tokio::select! {
                    event = rx.recv() => event,
                    result = &mut turn => panic!("turn ended before cancellation: {result:?}"),
                };
                if matches!(event, Some(EventKind::Live(LiveEvent::ToolStarted { .. }))) {
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
            Some(EventKind::Live(LiveEvent::ToolFinished {
                is_error: true,
                ..
            }))
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
