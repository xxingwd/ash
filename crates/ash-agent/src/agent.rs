use ash_core::{
    AgentToolContext, CancellationToken, ContentBlock, Event, Message, MessageContent, Role,
    SessionId, StopReason, ToolCallId, ToolContext, ToolDefinition, ToolError, ToolOutput,
};
use ash_protocol::{create_adapter, LlmRequest, ProtocolAdapter, ProtocolStream, StreamItem};
use futures::StreamExt;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::debug;

use crate::{session_store::SessionStore, AgentConfig};

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
    calls: Vec<PendingToolCall>,
    stop_reason: StopReason,
    cancelled: bool,
    error: Option<ash_core::ProtocolError>,
}

pub async fn run_agent_loop(
    config: AgentConfig,
    mut messages: Vec<Message>,
    tx: mpsc::Sender<Event>,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn(
        &config,
        &mut messages,
        tx,
        CancellationToken::new(),
        SessionId::new(),
    )
    .await
}

pub async fn run_agent_turn(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn_inner(config, messages, tx, cancel, session_id, None).await
}

pub(crate) async fn run_agent_turn_persisted(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
    store: &mut SessionStore,
) -> Result<StopReason, ash_core::AshError> {
    run_agent_turn_inner(config, messages, tx, cancel, session_id, Some(store)).await
}

async fn run_agent_turn_inner(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
    store: Option<&mut SessionStore>,
) -> Result<StopReason, ash_core::AshError> {
    let _ = tx.send(Event::AgentStarted { session_id }).await;
    let adapter = create_adapter(config.provider.clone());
    let result = run_with_adapter(
        config,
        messages,
        tx.clone(),
        cancel,
        session_id,
        adapter.as_ref(),
        store,
    )
    .await;

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

async fn run_with_adapter(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    session_id: SessionId,
    adapter: &dyn ProtocolAdapter,
    mut store: Option<&mut SessionStore>,
) -> Result<StopReason, ash_core::AshError> {
    let tool_defs: Vec<ToolDefinition> =
        config.tools.iter().map(|tool| tool.definition()).collect();

    for turn in 0..config.max_turns {
        if cancel.is_cancelled() {
            return Ok(StopReason::Aborted);
        }
        debug!(turn = turn + 1, "calling LLM");

        let request_messages = if let Some(max_tokens) = config.max_context_tokens {
            let bpe = crate::context::get_bpe_for_model(config.model.as_str());
            crate::context::compress_if_needed(messages.clone(), &bpe, max_tokens)
        } else {
            messages.clone()
        };
        let mut stream = adapter.stream(LlmRequest {
            model: config.model.clone(),
            system: config.system_prompt.clone(),
            messages: request_messages,
            tools: tool_defs.clone(),
            max_tokens: config.max_output_tokens,
        })?;
        let CollectedResponse {
            message,
            calls,
            stop_reason,
            cancelled,
            error,
        } = collect_response(&mut stream, &tx, &cancel).await;
        if let Some(message) = message {
            persist_message(store.as_deref_mut(), &message).await?;
            messages.push(message);
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        if calls.is_empty() {
            return if cancelled {
                Ok(StopReason::Aborted)
            } else {
                Ok(stop_reason)
            };
        }
        execute_tool_calls(
            config, messages, session_id, &cancel, &tx, &mut store, calls,
        )
        .await?;
        if cancelled || cancel.is_cancelled() {
            return Ok(StopReason::Aborted);
        }
    }

    Ok(StopReason::MaxTurns)
}

async fn collect_response(
    stream: &mut ProtocolStream,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> CollectedResponse {
    let mut blocks = Vec::new();
    let mut thought_started_at = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut cancelled = false;
    let mut error = None;

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
                error = Some(stream_error);
                break;
            }
        };
        match item {
            StreamItem::TextDelta(delta) => {
                finish_open_thought(&mut blocks, &mut thought_started_at);
                match blocks.last_mut() {
                    Some(ContentBlock::Text(text)) => text.push_str(&delta),
                    _ => blocks.push(ContentBlock::Text(delta.clone())),
                }
                let _ = tx.send(Event::TextDelta(delta)).await;
            }
            StreamItem::ThinkingDelta(delta) => {
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
            StreamItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                finish_open_thought(&mut blocks, &mut thought_started_at);
                blocks.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            StreamItem::Usage {
                input_tokens,
                output_tokens,
            } => {
                let _ = tx
                    .send(Event::Usage {
                        input_tokens,
                        output_tokens,
                    })
                    .await;
            }
            StreamItem::Stop(reason) => stop_reason = reason,
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
        calls,
        stop_reason,
        cancelled,
        error,
    }
}

async fn execute_tool_calls(
    config: &AgentConfig,
    messages: &mut Vec<Message>,
    session_id: SessionId,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<Event>,
    store: &mut Option<&mut SessionStore>,
    calls: Vec<PendingToolCall>,
) -> Result<(), ash_core::AshError> {
    for call in calls {
        let _ = tx
            .send(Event::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .await;
        let result = limit_tool_result(
            execute_tool(
                config,
                messages,
                session_id,
                cancel,
                &call.name,
                call.arguments.clone(),
            )
            .await,
        );
        let (output, is_error) = match &result {
            Ok(output) => (output.text.clone(), false),
            Err(error) => (error.to_string(), true),
        };
        let _ = tx
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
        persist_message(store.as_deref_mut(), &message).await?;
        messages.push(message);
    }
    Ok(())
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

async fn persist_message(
    store: Option<&mut SessionStore>,
    message: &Message,
) -> Result<(), ash_core::AshError> {
    match store {
        Some(store) => store.append_message(message).await,
        None => Ok(()),
    }
}

async fn execute_tool(
    config: &AgentConfig,
    messages: &[Message],
    session_id: SessionId,
    cancel: &CancellationToken,
    name: &str,
    arguments: serde_json::Value,
) -> Result<ToolOutput, ToolError> {
    let Some(tool) = config.tools.iter().find(|tool| tool.name() == name) else {
        return Err(ToolError::Execution(format!("unknown tool: {name}")));
    };
    let context = ToolContext {
        working_dir: config.working_dir.clone(),
        max_duration: config.max_tool_duration,
        agent: AgentToolContext {
            root_session_id: config.root_session_id.unwrap_or(session_id),
            agent_path: config.agent_path.clone(),
            messages: messages.to_vec(),
            provider: config.provider.clone(),
            system_prompt: config.system_prompt.clone(),
            tools: config.tools.clone(),
            model: config.model.clone(),
            max_turns: config.max_turns,
            max_context_tokens: config.max_context_tokens,
            max_output_tokens: config.max_output_tokens,
        },
    };

    tokio::select! {
        _ = cancel.cancelled() => Err(ToolError::Cancelled),
        result = tokio::time::timeout(config.max_tool_duration, tool.execute(context, arguments)) => {
            result.unwrap_or(Err(ToolError::Timeout(config.max_tool_duration)))
        }
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
    pub fn run(config: AgentConfig, messages: Vec<Message>) -> impl futures::Stream<Item = Event> {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let _ = run_agent_loop(config, messages, tx).await;
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

    use ash_core::{ModelId, Protocol, ProviderConfig, Tool, ToolCallId, ToolContext, ToolError};
    use ash_protocol::{ProtocolStream, StreamItem};
    use tempfile::TempDir;

    use super::*;

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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: None,
            tools: vec![Arc::new(EchoTool)],
            model: ModelId::new("test-model"),
            max_turns: 4,
            working_dir: PathBuf::from("."),
            max_context_tokens: None,
            max_output_tokens: None,
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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: None,
            max_output_tokens: None,
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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: None,
            max_output_tokens: None,
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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: None,
            max_output_tokens: None,
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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: None,
            tools: vec![Arc::new(BlockingTool)],
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: None,
            max_output_tokens: None,
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

            let event = tokio::select! {
                event = rx.recv() => event,
                result = &mut turn => panic!("turn ended before cancellation: {result:?}"),
            };
            assert!(matches!(event, Some(Event::ToolCallStart { .. })));
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
