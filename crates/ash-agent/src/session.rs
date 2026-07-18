use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use ash_core::{
    CancellationToken, Content, Event, Message, MessageContent, ModelId, Protocol, SessionId,
    SessionSummary, StopReason,
};
use tokio::sync::mpsc;
use tracing::warn;

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
        let protocol = Protocol::from_str(&stored.metadata.protocol).map_err(|error| {
            ash_core::AshError::Config(format!(
                "invalid protocol '{}' in {}: {error}",
                stored.metadata.protocol,
                stored.path.display()
            ))
        })?;
        if !stored.metadata.working_dir.is_dir() {
            return Err(ash_core::AshError::Config(format!(
                "saved working directory no longer exists: {}",
                stored.metadata.working_dir.display()
            )));
        }
        let store = SessionStore::resume(&stored).await?;
        self.id = stored.metadata.session_id;
        self.config.provider.protocol = protocol;
        self.config.model = ModelId::new(&stored.metadata.model);
        self.config
            .working_dir
            .clone_from(&stored.metadata.working_dir);
        self.config
            .system_prompt
            .clone_from(&stored.metadata.system_prompt);
        self.config.max_turns = stored.metadata.max_turns;
        self.config.max_context_tokens = stored.metadata.max_context_tokens;
        self.config.max_output_tokens = stored.metadata.max_output_tokens;
        self.config.max_tool_duration = Duration::from_millis(stored.metadata.tool_timeout_ms);
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
        persist_or_warn(
            self.store.append_message(&user_message).await,
            self.store.path(),
        );
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

    pub async fn rollback_last_turn(&mut self) -> Option<String> {
        let (turn_start, prompt) = last_user_turn(&self.messages)?;
        self.messages.truncate(turn_start);
        persist_or_warn(
            self.store.append_turn_rolled_back(1).await,
            self.store.path(),
        );
        Some(prompt)
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

fn persist_or_warn(result: Result<(), ash_core::AshError>, path: &std::path::Path) {
    if let Err(error) = result {
        warn!(path = %path.display(), %error, "failed to persist session history");
    }
}

#[cfg(test)]
mod tests {
    use ash_core::{ContentBlock, MessageContent, MessageId, Role, ToolCallId};

    use super::*;

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
}
