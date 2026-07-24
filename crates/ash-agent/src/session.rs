use std::path::PathBuf;

use ash_core::{
    CancellationToken, Content, Event, Message, MessageContent, SessionId, SessionSummary,
    StopReason,
};
use tokio::sync::mpsc;

use crate::agent::run_agent_turn_persisted;
use crate::session_store::{SessionStore, StoredSession};
use crate::AgentConfig;

pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub model: String,
    pub protocol: String,
    pub working_dir: PathBuf,
}

pub struct AgentSession {
    id: SessionId,
    config: AgentConfig,
    messages: Vec<Message>,
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
        if let Err(error) = self.store.append_message(&user_message).await {
            let _ = events.send(Event::Error(error.to_string())).await;
            let _ = events
                .send(Event::AgentFinished {
                    reason: StopReason::Aborted,
                })
                .await;
            return Err(error);
        }
        self.messages.push(user_message);
        run_agent_turn_persisted(
            &self.config,
            &mut self.messages,
            events,
            cancel,
            self.id,
            &mut self.store,
        )
        .await
    }

    pub async fn rollback_last_turn(&mut self) -> Result<Option<String>, ash_core::AshError> {
        let Some((turn_start, prompt)) = last_user_turn(&self.messages) else {
            return Ok(None);
        };
        self.store.truncate_last_turn().await?;
        self.messages.truncate(turn_start);
        Ok(Some(prompt))
    }
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
    use std::time::Duration;

    use ash_core::{
        ContentBlock, MessageContent, MessageId, ModelId, Protocol, ProviderConfig, Role,
        ToolCallId,
    };
    use secrecy::SecretString;
    use tempfile::TempDir;

    use super::*;

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
            max_context_tokens: Some(1000),
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

    #[tokio::test]
    async fn resume_keeps_the_current_runtime_configuration() {
        let directory = TempDir::new().unwrap();
        let current_dir = directory.path().join("current");
        tokio::fs::create_dir(&current_dir).await.unwrap();
        let mut session = AgentSession::new(config(current_dir.clone()));
        let saved_path = directory.path().join("saved.jsonl");
        tokio::fs::write(&saved_path, b"\n").await.unwrap();
        let saved_id = SessionId::new();
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
                max_context_tokens: None,
                max_output_tokens: None,
                tool_timeout_ms: 1,
            },
            messages: vec![Message::user("saved question")],
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
        session.messages.push(Message::user("unpersisted"));

        let result = session.rollback_last_turn().await;

        assert!(result.is_err());
        assert_eq!(session.messages.len(), 1);
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
            store,
        };
        let (events, mut received) = mpsc::channel(4);

        let result = session
            .submit("unpersisted", events, CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(session.messages.is_empty());
        assert!(matches!(received.recv().await, Some(Event::Error(_))));
        assert!(matches!(
            received.recv().await,
            Some(Event::AgentFinished {
                reason: StopReason::Aborted
            })
        ));
    }
}
