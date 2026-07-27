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

use crate::{
    context::{
        apply_summary, count_output_tokens, estimate_request_tokens, needs_compaction,
        plan_compaction, prune_tool_outputs, summary_output_tokens,
    },
    session_store::SessionStore,
    AgentConfig,
};

const MAX_AGENT_OUTPUT_BYTES: usize = 64 * 1024;
const AGENT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const AGENT_OUTPUT_TRUNCATION_NOTICE: &str = "\n... tool output truncated by the agent ...\n";
const COMPACTION_SYSTEM_PROMPT: &str = "You are an anchored context summarization assistant for coding sessions. Summarize only the supplied conversation history. Do not answer the conversation. Preserve exact technical details and respond in the conversation's language.";

struct PendingToolCall {
    id: ToolCallId,
    name: String,
    arguments: serde_json::Value,
}

struct CollectedResponse {
    message: Option<Message>,
    calls: Vec<PendingToolCall>,
    usage: UsageMetrics,
    stop_reason: StopReason,
    cancelled: bool,
    error: Option<ash_core::ProtocolError>,
}

pub(crate) struct CompactedHistory {
    pub(crate) messages: Vec<Message>,
    pub(crate) before_tokens: usize,
    pub(crate) after_tokens: usize,
    pub(crate) dropped_messages: usize,
}

#[derive(Default)]
struct UsageMetrics {
    input_tokens: u64,
    output_tokens: u64,
    generation_ms: u64,
    estimated: bool,
}

impl UsageMetrics {
    fn record(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = self.input_tokens.max(input_tokens);
        self.output_tokens = self.output_tokens.max(output_tokens);
    }

    fn fill_missing(&mut self, input_tokens: u64, output_tokens: u64) {
        if self.input_tokens == 0 {
            self.input_tokens = input_tokens;
            self.estimated = true;
        }
        if self.output_tokens == 0 && output_tokens > 0 {
            self.output_tokens = output_tokens;
            self.estimated = true;
        }
    }
}

pub(crate) async fn compact_with_adapter(
    config: &AgentConfig,
    messages: &[Message],
    adapter: &dyn ProtocolAdapter,
    cancel: &CancellationToken,
) -> Result<Option<CompactedHistory>, ash_core::AshError> {
    let tools = config
        .tools
        .iter()
        .map(|tool| tool.definition())
        .collect::<Vec<_>>();
    let before_tokens = estimate_request_tokens(config.system_prompt.as_deref(), messages, &tools);
    let Some(plan) = plan_compaction(messages, config.max_input_tokens) else {
        return Ok(None);
    };
    let mut stream = adapter.stream(LlmRequest {
        model: config.model.clone(),
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::user(&plan.summary_prompt)],
        tools: Vec::new(),
        max_tokens: Some(summary_output_tokens(config.max_input_tokens)),
    })?;
    let summary = collect_compaction_summary(&mut stream, cancel).await?;
    let compacted = apply_summary(&summary, plan.tail);
    let after_tokens = estimate_request_tokens(config.system_prompt.as_deref(), &compacted, &tools);
    if after_tokens >= before_tokens {
        return Ok(None);
    }
    Ok(Some(CompactedHistory {
        messages: compacted,
        before_tokens,
        after_tokens,
        dropped_messages: plan.compacted_messages,
    }))
}

async fn collect_compaction_summary(
    stream: &mut ProtocolStream,
    cancel: &CancellationToken,
) -> Result<String, ash_core::AshError> {
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
            StreamItem::TextDelta(text) => summary.push_str(&text),
            StreamItem::ThinkingDelta(_) | StreamItem::Usage { .. } | StreamItem::Stop(_) => {}
            StreamItem::ToolCall { .. } => {
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
    Ok(summary.to_string())
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

        if let Some(pruned) = prune_tool_outputs(messages) {
            *messages = pruned;
        }

        let estimated_context =
            estimate_request_tokens(config.system_prompt.as_deref(), messages, &tool_defs);
        if needs_compaction(estimated_context, config.max_input_tokens) {
            if let Some(compacted) =
                compact_with_adapter(config, messages, adapter, &cancel).await?
            {
                if let Some(store) = store.as_deref_mut() {
                    store.append_compaction(&compacted.messages).await?;
                }
                *messages = compacted.messages;
                let _ = tx
                    .send(Event::ContextCompacted {
                        before_tokens: u64::try_from(compacted.before_tokens).unwrap_or(u64::MAX),
                        after_tokens: u64::try_from(compacted.after_tokens).unwrap_or(u64::MAX),
                        dropped_messages: u64::try_from(compacted.dropped_messages)
                            .unwrap_or(u64::MAX),
                        automatic: true,
                    })
                    .await;
            }
        }

        let request_messages = messages.clone();
        let estimated_input_tokens = u64::try_from(estimate_request_tokens(
            config.system_prompt.as_deref(),
            &request_messages,
            &tool_defs,
        ))
        .unwrap_or(u64::MAX);
        let request_started = Instant::now();
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
            usage,
            stop_reason,
            cancelled,
            error,
        } = collect_response(&mut stream, &tx, &cancel, request_started).await;
        let estimated_output_tokens = message
            .as_ref()
            .map(count_output_tokens)
            .and_then(|tokens| u64::try_from(tokens).ok())
            .unwrap_or(0);
        let mut usage = usage;
        usage.fill_missing(estimated_input_tokens, estimated_output_tokens);
        let _ = tx
            .send(Event::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                generation_ms: usage.generation_ms,
                estimated: usage.estimated,
            })
            .await;
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
    request_started: Instant,
) -> CollectedResponse {
    let mut blocks = Vec::new();
    let mut thought_started_at = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut cancelled = false;
    let mut error = None;
    let mut usage = UsageMetrics::default();
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
                error = Some(stream_error);
                break;
            }
        };
        match item {
            StreamItem::TextDelta(delta) => {
                first_output_at.get_or_insert_with(Instant::now);
                finish_open_thought(&mut blocks, &mut thought_started_at);
                match blocks.last_mut() {
                    Some(ContentBlock::Text(text)) => text.push_str(&delta),
                    _ => blocks.push(ContentBlock::Text(delta.clone())),
                }
                let _ = tx.send(Event::TextDelta(delta)).await;
            }
            StreamItem::ThinkingDelta(delta) => {
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
            StreamItem::ToolCall {
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
            StreamItem::Usage {
                input_tokens,
                output_tokens,
            } => usage.record(input_tokens, output_tokens),
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
        usage: UsageMetrics {
            generation_ms: u64::try_from(
                first_output_at
                    .unwrap_or(request_started)
                    .elapsed()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            ..usage
        },
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
            max_input_tokens: config.max_input_tokens,
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

    use ash_core::{
        Content, ModelId, Protocol, ProviderConfig, Tool, ToolCallId, ToolContext, ToolError,
    };
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
            max_input_tokens: 200_000,
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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_input_tokens: 1_000,
            max_output_tokens: None,
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
        assert_eq!(stored.messages.len(), 7);
        assert_eq!(stored.model_messages.len(), 6);
        assert!(matches!(
            &stored.messages[0].content,
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
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: Some("system".to_string()),
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_input_tokens: 120_000,
            max_output_tokens: None,
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
            provider: ProviderConfig {
                protocol: Protocol::AnthropicMessages,
                api_key: "test".into(),
                base_url: None,
            },
            system_prompt: None,
            tools: Vec::new(),
            model: ModelId::new("test-model"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_input_tokens: 200_000,
            max_output_tokens: None,
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
        let mut usage = UsageMetrics::default();

        usage.fill_missing(120, 25);

        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 25);
        assert!(usage.estimated);
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
            max_input_tokens: 200_000,
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
            max_input_tokens: 200_000,
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
            max_input_tokens: 200_000,
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
            max_input_tokens: 200_000,
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
