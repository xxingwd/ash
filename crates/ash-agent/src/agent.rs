use ash_core::{
    AgentToolContext, CancellationToken, ContentBlock, Event, Message, MessageContent, Role,
    SessionId, StopReason, ToolContext, ToolDefinition, ToolError, ToolOutput,
};
use ash_protocol::{create_adapter, LlmRequest, ProtocolAdapter, StreamItem};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::debug;

use crate::{session_store::SessionStore, AgentConfig};

const MAX_AGENT_OUTPUT_BYTES: usize = 64 * 1024;
const AGENT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const AGENT_OUTPUT_TRUNCATION_NOTICE: &str = "\n... tool output truncated by the agent ...\n";

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

        let mut blocks = Vec::new();
        let mut text = String::new();
        let mut stop_reason = StopReason::EndTurn;

        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Ok(StopReason::Aborted),
                next = stream.next() => next,
            };
            let Some(item) = next else {
                break;
            };

            match item.map_err(ash_core::AshError::Protocol)? {
                StreamItem::TextDelta(delta) => {
                    text.push_str(&delta);
                    let _ = tx.send(Event::TextDelta(delta)).await;
                }
                StreamItem::ThinkingDelta(delta) => {
                    let _ = tx.send(Event::Thinking(delta)).await;
                }
                StreamItem::ToolCall {
                    id,
                    name,
                    arguments,
                } => blocks.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                }),
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

        if !text.is_empty() {
            blocks.insert(0, ContentBlock::Text(text));
        }
        let calls: Vec<_> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                ContentBlock::Text(_) => None,
            })
            .collect();

        if !blocks.is_empty() {
            let message = Message {
                id: ash_core::MessageId::new(),
                role: Role::Assistant,
                content: MessageContent::Assistant(blocks),
            };
            persist_message(store.as_deref_mut(), &message).await;
            messages.push(message);
        }
        if calls.is_empty() {
            return Ok(stop_reason);
        }

        for (id, name, arguments) in calls {
            let _ = tx
                .send(Event::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                })
                .await;
            let result = limit_tool_result(
                execute_tool(
                    config,
                    messages,
                    session_id,
                    &cancel,
                    &name,
                    arguments.clone(),
                )
                .await,
            );
            if cancel.is_cancelled() {
                return Ok(StopReason::Aborted);
            }
            let (output, is_error) = match &result {
                Ok(output) => (output.text.clone(), false),
                Err(error) => (error.to_string(), true),
            };
            let _ = tx
                .send(Event::ToolCallEnd {
                    id: id.clone(),
                    name,
                    arguments,
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
                    id,
                    result,
                    attachments,
                },
            };
            persist_message(store.as_deref_mut(), &message).await;
            messages.push(message);
        }
    }

    Ok(StopReason::MaxTurns)
}

async fn persist_message(store: Option<&mut SessionStore>, message: &Message) {
    let Some(store) = store else {
        return;
    };
    if let Err(error) = store.append_message(message).await {
        tracing::warn!(
            path = %store.path().display(),
            %error,
            "failed to persist session message"
        );
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

    #[test]
    fn truncates_large_tool_output_without_splitting_utf8() {
        let output = "你".repeat(MAX_AGENT_OUTPUT_BYTES);
        let truncated = limit_tool_output(output);

        assert!(truncated.len() <= MAX_AGENT_OUTPUT_BYTES);
        assert!(truncated.contains("tool output truncated"));
        assert!(truncated.is_char_boundary(truncated.len()));
    }
}
