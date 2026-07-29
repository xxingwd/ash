use std::path::PathBuf;

use ash_core::{
    CancellationToken, Content, Event, Message, MessageContent, MessageId, SessionId,
    SessionSummary, StopReason,
};
use ash_protocol::{create_adapter, ProtocolAdapter};
use tokio::sync::mpsc;

use crate::agent::{compact_with_adapter, run_agent_turn_persisted};
use crate::context::estimate_request_tokens;
use crate::session_store::{SessionStore, StoredSession};
use crate::AgentConfig;

pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub model: String,
    pub protocol: String,
    pub working_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextCompaction {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub dropped_messages: usize,
}

pub struct AgentSession {
    id: SessionId,
    config: AgentConfig,
    messages: Vec<Message>,
    model_messages: Vec<Message>,
    store: SessionStore,
}

impl AgentSession {
    pub fn new(config: AgentConfig) -> Self {
        let id = SessionId::new();
        let store = SessionStore::new(&config, id);
        Self {
            id,
            config,
            messages: Vec::new(),
            model_messages: Vec::new(),
            store,
        }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn reset(&mut self) {
        self.id = SessionId::new();
        self.messages.clear();
        self.model_messages.clear();
        self.store = SessionStore::new(&self.config, self.id);
    }

    pub async fn resumable_sessions(&self) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        SessionStore::summaries_except(self.store.path()).await
    }

    pub async fn resume(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<ResumedSession>, ash_core::AshError> {
        let Some(stored) = SessionStore::find(session_id).await? else {
            return Ok(None);
        };
        self.restore(stored).await.map(Some)
    }

    async fn restore(
        &mut self,
        stored: StoredSession,
    ) -> Result<ResumedSession, ash_core::AshError> {
        // Saved sessions replay history; the current runtime configuration stays indivisible.
        let store = SessionStore::resume(&stored).await?;
        self.id = stored.metadata.session_id;
        self.messages = stored.messages;
        self.model_messages = stored.model_messages;
        self.store = store;

        Ok(ResumedSession {
            messages: self.messages.clone(),
            model: self.config.model.as_str().to_string(),
            protocol: self.config.provider.protocol.as_cli_name().to_string(),
            working_dir: self.config.working_dir.clone(),
        })
    }

    pub async fn submit(
        &mut self,
        input: impl Into<String>,
        events: mpsc::Sender<Event>,
        cancel: CancellationToken,
    ) -> Result<StopReason, ash_core::AshError> {
        let input = input.into();
        let user_message = Message::user(&input);
        let user_message_id = user_message.id;
        if let Err(error) = self.store.append_message(&user_message).await {
            let _ = events.send(Event::Error(error.to_string())).await;
            let _ = events
                .send(Event::AgentFinished {
                    reason: StopReason::Aborted,
                })
                .await;
            return Err(error);
        }
        self.messages.push(user_message.clone());
        self.model_messages.push(user_message);
        let result = run_agent_turn_persisted(
            &self.config,
            &mut self.model_messages,
            events,
            cancel,
            self.id,
            &mut self.store,
        )
        .await;
        let sync_result =
            sync_model_turn_output(&mut self.messages, &self.model_messages, user_message_id);
        match result {
            Err(error) => Err(error),
            Ok(reason) => {
                sync_result?;
                Ok(reason)
            }
        }
    }

    pub async fn rollback_last_turn(&mut self) -> Result<Option<String>, ash_core::AshError> {
        let Some((turn_start, prompt)) = last_user_turn(&self.messages) else {
            return Ok(None);
        };
        self.store.append_rollback().await?;
        let stored = self.store.load().await?;
        self.messages = stored.messages;
        self.model_messages = stored.model_messages;
        debug_assert_eq!(self.messages.len(), turn_start);
        Ok(Some(prompt))
    }

    pub async fn compact(&mut self) -> Result<ContextCompaction, ash_core::AshError> {
        let adapter = create_adapter(self.config.provider.clone());
        self.compact_using(adapter.as_ref(), &CancellationToken::new())
            .await
    }

    async fn compact_using(
        &mut self,
        adapter: &dyn ProtocolAdapter,
        cancel: &CancellationToken,
    ) -> Result<ContextCompaction, ash_core::AshError> {
        let tools = self
            .config
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let before_tokens = estimate_request_tokens(
            self.config.system_prompt.as_deref(),
            &self.model_messages,
            &tools,
        );
        let Some(compacted) =
            compact_with_adapter(&self.config, &self.model_messages, adapter, cancel).await?
        else {
            return Ok(ContextCompaction {
                before_tokens,
                after_tokens: before_tokens,
                dropped_messages: 0,
            });
        };
        self.store.append_compaction(&compacted.messages).await?;
        self.model_messages = compacted.messages;
        Ok(ContextCompaction {
            before_tokens: compacted.before_tokens,
            after_tokens: compacted.after_tokens,
            dropped_messages: compacted.dropped_messages,
        })
    }
}

fn sync_model_turn_output(
    messages: &mut Vec<Message>,
    model_messages: &[Message],
    user_message_id: MessageId,
) -> Result<(), ash_core::AshError> {
    let turn_start = model_messages
        .iter()
        .position(|message| message.id == user_message_id)
        .ok_or_else(|| {
            ash_core::AshError::Config(
                "compacted model context lost the active user message".to_string(),
            )
        })?;
    messages.extend_from_slice(&model_messages[turn_start + 1..]);
    Ok(())
}

fn last_user_turn(messages: &[Message]) -> Option<(usize, String)> {
    let (index, contents) = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match &message.content {
            MessageContent::User(contents) => Some((index, contents)),
            MessageContent::Assistant(_) | MessageContent::ToolResult { .. } => None,
        })?;
    let prompt = contents
        .iter()
        .map(|content| match content {
            Content::Text(text) => text.clone(),
            Content::Image { media_type, .. } => format!("[image: {media_type}]"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((index, prompt))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use ash_core::{
        ContentBlock, MessageContent, MessageId, ModelId, Protocol, ProviderConfig, Role,
        ToolCallId,
    };
    use ash_protocol::{LlmRequest, ProtocolStream, StreamItem};
    use secrecy::SecretString;
    use tempfile::TempDir;

    use super::*;

    struct MockAdapter {
        responses: Mutex<VecDeque<Vec<StreamItem>>>,
        requests: Arc<Mutex<Vec<LlmRequest>>>,
    }

    impl ProtocolAdapter for MockAdapter {
        fn stream(&self, request: LlmRequest) -> Result<ProtocolStream, ash_core::ProtocolError> {
            self.requests.lock().unwrap().push(request);
            let items = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(items.into_iter().map(Ok))))
        }
    }

    fn config(working_dir: PathBuf) -> AgentConfig {
        AgentConfig {
            provider: ProviderConfig {
                protocol: Protocol::OpenaiResponses,
                api_key: SecretString::from("test"),
                base_url: None,
            },
            system_prompt: Some("current prompt".to_string()),
            tools: Vec::new(),
            model: ModelId::new("current-model"),
            max_turns: 10,
            working_dir,
            max_context_tokens: 1000,
            max_output_tokens: Some(200),
            max_tool_duration: Duration::from_secs(5),
            agent_path: "/root".to_string(),
            root_session_id: None,
        }
    }

    #[test]
    fn finds_the_latest_real_user_turn_after_tool_results() {
        let tool_id = ToolCallId::from_provider("call");
        let messages = vec![
            Message::user("first"),
            Message::assistant_text("first answer"),
            Message::user("second"),
            Message {
                id: MessageId::new(),
                role: Role::Assistant,
                content: MessageContent::Assistant(vec![ContentBlock::ToolCall {
                    id: tool_id.clone(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                }]),
            },
            Message {
                id: MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: tool_id,
                    result: Ok("done".to_string()),
                    attachments: Vec::new(),
                },
            },
        ];

        let (turn_start, prompt) = last_user_turn(&messages).unwrap();
        assert_eq!(turn_start, 2);
        assert_eq!(prompt, "second");
    }

    #[test]
    fn syncs_only_new_turn_output_back_to_full_history() {
        let current = Message::user("current request");
        let mut messages = vec![
            Message::user("original title"),
            Message::assistant_text("old answer"),
            current.clone(),
        ];
        let model_messages = vec![
            Message::assistant_text("<context-summary>\nold facts\n</context-summary>"),
            current.clone(),
            Message::assistant_text("new answer"),
        ];

        sync_model_turn_output(&mut messages, &model_messages, current.id).unwrap();

        assert_eq!(messages.len(), 4);
        assert!(matches!(
            &messages[0].content,
            MessageContent::User(contents)
                if matches!(contents.as_slice(), [Content::Text(text)] if text == "original title")
        ));
        assert!(matches!(
            &messages[3].content,
            MessageContent::Assistant(blocks)
                if matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "new answer")
        ));
    }

    #[tokio::test]
    async fn resume_keeps_the_current_runtime_configuration() {
        let directory = TempDir::new().unwrap();
        let current_dir = directory.path().join("current");
        tokio::fs::create_dir(&current_dir).await.unwrap();
        let mut session = AgentSession::new(config(current_dir.clone()));
        let saved_path = directory.path().join("saved.jsonl");
        tokio::fs::write(&saved_path, b"\n").await.unwrap();
        let saved_id = SessionId::new();
        let saved_message = Message::user("saved question");
        let stored = StoredSession {
            path: saved_path,
            metadata: crate::session_store::SessionMetadata {
                format_version: 1,
                session_id: saved_id,
                created_at: "2026-01-01T00:00:00.000Z".to_string(),
                protocol: "invalid-old-protocol".to_string(),
                model: "old-model".to_string(),
                working_dir: directory.path().join("missing-old-directory"),
                system_prompt: Some("old prompt".to_string()),
                max_turns: 1,
                max_context_tokens: 64_000,
                max_output_tokens: None,
                tool_timeout_ms: 1,
            },
            messages: vec![saved_message.clone()],
            model_messages: vec![saved_message],
        };

        let restored = session.restore(stored).await.unwrap();

        assert_eq!(session.id(), saved_id);
        assert_eq!(session.config.model.as_str(), "current-model");
        assert!(matches!(
            session.config.provider.protocol,
            Protocol::OpenaiResponses
        ));
        assert_eq!(
            session.config.system_prompt.as_deref(),
            Some("current prompt")
        );
        assert_eq!(session.config.working_dir, current_dir);
        assert_eq!(restored.model, "current-model");
        assert_eq!(restored.protocol, "openai-responses");
        assert_eq!(restored.messages.len(), 1);
    }

    #[tokio::test]
    async fn rollback_keeps_memory_when_persistence_fails() {
        let directory = TempDir::new().unwrap();
        let mut session = AgentSession::new(config(directory.path().to_path_buf()));
        let message = Message::user("unpersisted");
        session.messages.push(message.clone());
        session.model_messages.push(message);

        let result = session.rollback_last_turn().await;

        assert!(result.is_err());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.model_messages.len(), 1);
    }

    #[tokio::test]
    async fn submit_keeps_memory_clean_when_persistence_fails() {
        let directory = TempDir::new().unwrap();
        let blocked_parent = directory.path().join("not-a-directory");
        tokio::fs::write(&blocked_parent, b"file").await.unwrap();
        let config = config(directory.path().to_path_buf());
        let id = SessionId::new();
        let store = SessionStore::new_in(&config, id, &blocked_parent);
        let mut session = AgentSession {
            id,
            config,
            messages: Vec::new(),
            model_messages: Vec::new(),
            store,
        };
        let (events, mut received) = mpsc::channel(4);

        let result = session
            .submit("unpersisted", events, CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(session.messages.is_empty());
        assert!(session.model_messages.is_empty());
        assert!(matches!(received.recv().await, Some(Event::Error(_))));
        assert!(matches!(
            received.recv().await,
            Some(Event::AgentFinished {
                reason: StopReason::Aborted
            })
        ));
    }

    #[tokio::test]
    async fn manual_compaction_preserves_full_history_and_updates_model_context() {
        let directory = TempDir::new().unwrap();
        let config = config(directory.path().to_path_buf());
        let id = SessionId::new();
        let mut store = SessionStore::new_in(&config, id, directory.path());
        let messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("middle request"),
            Message::assistant_text("middle answer"),
            Message::user("recent request"),
            Message::assistant_text("recent answer"),
        ];
        for message in &messages {
            store.append_message(message).await.unwrap();
        }
        let mut session = AgentSession {
            id,
            config,
            messages: messages.clone(),
            model_messages: messages,
            store,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = MockAdapter {
            responses: Mutex::new(VecDeque::from([vec![
                StreamItem::TextDelta("condensed facts".to_string()),
                StreamItem::Stop(StopReason::EndTurn),
            ]])),
            requests: requests.clone(),
        };

        let result = session
            .compact_using(&adapter, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.dropped_messages, 2);
        assert_eq!(session.messages.len(), 6);
        assert_eq!(session.model_messages.len(), 5);
        assert!(result.after_tokens < result.before_tokens);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].tools.is_empty());
        }
        let contents = tokio::fs::read_to_string(session.store.path())
            .await
            .unwrap();
        assert!(contents.contains("old request"));
        assert!(contents.contains("condensed facts"));
        assert!(contents.contains("recent request"));
        let stored = session.store.load().await.unwrap();
        assert_eq!(stored.messages.len(), 6);
        assert_eq!(stored.model_messages.len(), 5);
        assert!(matches!(
            &stored.messages[0].content,
            MessageContent::User(contents)
                if matches!(contents.as_slice(), [Content::Text(text)] if text.starts_with("old request"))
        ));
    }
}
