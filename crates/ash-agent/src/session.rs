use std::path::PathBuf;

use ash_core::{
    CancellationToken, Content, Event, ForkPoint, Message, MessageContent, MessageId, SessionId,
    SessionSummary, StopReason,
};
use tokio::sync::mpsc;

use crate::agent::{compact_with_adapter, run_agent_turn_persisted};
use crate::context::estimate_request_tokens;
use crate::{
    AgentConfig, AgentRuntime, ContextCheckpoint, ConversationEntry, ConversationLog,
    ConversationMetadata, ConversationStore,
};

pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub model: String,
    pub protocol: String,
    pub working_dir: PathBuf,
}

pub struct ForkedSession {
    pub messages: Vec<Message>,
    pub model: String,
    pub protocol: String,
    pub working_dir: PathBuf,
    pub prompt: String,
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
    runtime: AgentRuntime,
    log: ConversationLog,
    store: Box<dyn ConversationStore>,
}

impl AgentSession {
    pub(crate) fn new(config: AgentConfig, runtime: AgentRuntime) -> Self {
        let id = SessionId::new();
        let store = runtime
            .conversations()
            .create(conversation_metadata(&config, &runtime, id));
        Self {
            id,
            config,
            runtime,
            log: ConversationLog::new(),
            store,
        }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn messages(&self) -> Vec<Message> {
        self.log.messages()
    }

    pub fn reset(&mut self) {
        self.id = SessionId::new();
        self.log = ConversationLog::new();
        self.store = self.runtime.conversations().create(conversation_metadata(
            &self.config,
            &self.runtime,
            self.id,
        ));
    }

    pub async fn resumable_sessions(&self) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.runtime.conversations().list(Some(self.id)).await
    }

    pub async fn resume(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<ResumedSession>, ash_core::AshError> {
        let Some(store) = self.runtime.conversations().open(session_id).await? else {
            return Ok(None);
        };
        self.restore(store).await.map(Some)
    }

    pub fn fork_points(&self) -> Vec<ForkPoint> {
        self.log
            .messages()
            .iter()
            .rev()
            .filter_map(|message| {
                user_prompt(message).map(|prompt| ForkPoint {
                    message_id: message.id,
                    prompt,
                })
            })
            .collect()
    }

    pub async fn fork_at(
        &mut self,
        message_id: MessageId,
    ) -> Result<Option<ForkedSession>, ash_core::AshError> {
        let Some((turn_start, prompt)) =
            self.log
                .messages()
                .iter()
                .enumerate()
                .find_map(|(index, message)| {
                    (message.id == message_id)
                        .then(|| user_prompt(message).map(|prompt| (index, prompt)))
                        .flatten()
                })
        else {
            return Ok(None);
        };

        let messages = self.log.messages()[..turn_start].to_vec();
        let id = SessionId::new();
        let mut store = self.runtime.conversations().create(conversation_metadata(
            &self.config,
            &self.runtime,
            id,
        ));
        for message in &messages {
            store
                .append(ConversationEntry::Message(message.clone()))
                .await?;
        }

        self.id = id;
        self.log = ConversationLog::from_messages(messages.clone());
        self.store = store;

        Ok(Some(ForkedSession {
            messages,
            model: self.config.model.as_str().to_string(),
            protocol: self.runtime.model_backend().to_string(),
            working_dir: self.config.working_dir.clone(),
            prompt,
        }))
    }

    async fn restore(
        &mut self,
        store: Box<dyn ConversationStore>,
    ) -> Result<ResumedSession, ash_core::AshError> {
        // Saved sessions replay history; the current runtime configuration stays indivisible.
        self.id = store.session_id();
        self.log = store.load().await?;
        self.store = store;

        Ok(ResumedSession {
            messages: self.log.messages(),
            model: self.config.model.as_str().to_string(),
            protocol: self.runtime.model_backend().to_string(),
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
        if let Err(error) = self
            .store
            .append(ConversationEntry::Message(user_message.clone()))
            .await
        {
            let _ = events.send(Event::Error(error.to_string())).await;
            let _ = events
                .send(Event::AgentFinished {
                    reason: StopReason::Aborted,
                })
                .await;
            return Err(error);
        }
        self.log
            .push(ConversationEntry::Message(user_message.clone()));
        let mut model_context = self.log.model_context();
        let result = run_agent_turn_persisted(
            self.runtime.model(),
            &self.config,
            &mut model_context,
            events,
            cancel,
            self.id,
            self.store.as_mut(),
        )
        .await;
        match self.store.load().await {
            Ok(log) => self.log = log,
            Err(error) if result.is_ok() => return Err(error),
            Err(_) => {}
        }
        result
    }

    pub async fn rollback_last_turn(&mut self) -> Result<Option<String>, ash_core::AshError> {
        let messages = self.log.messages();
        let Some((turn_start, prompt)) = last_user_turn(&messages) else {
            return Ok(None);
        };
        self.store.append(ConversationEntry::TurnRolledBack).await?;
        self.log = self.store.load().await?;
        debug_assert_eq!(self.log.messages().len(), turn_start);
        Ok(Some(prompt))
    }

    pub async fn compact(&mut self) -> Result<ContextCompaction, ash_core::AshError> {
        let model = self.runtime.model_client();
        self.compact_using(model.as_ref(), &CancellationToken::new())
            .await
    }

    async fn compact_using(
        &mut self,
        model: &dyn ash_core::ModelClient,
        cancel: &CancellationToken,
    ) -> Result<ContextCompaction, ash_core::AshError> {
        let tools = self
            .config
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let model_context = self.log.model_context();
        let before_tokens =
            estimate_request_tokens(self.config.system_prompt.as_deref(), &model_context, &tools);
        let Some(compacted) =
            compact_with_adapter(&self.config, &model_context, model, cancel).await?
        else {
            return Ok(ContextCompaction {
                before_tokens,
                after_tokens: before_tokens,
                dropped_messages: 0,
            });
        };
        let checkpoint = ContextCheckpoint::from_model_context(&compacted.messages)?;
        self.store
            .append(ConversationEntry::ContextCheckpoint(checkpoint.clone()))
            .await?;
        self.log
            .push(ConversationEntry::ContextCheckpoint(checkpoint));
        Ok(ContextCompaction {
            before_tokens: compacted.before_tokens,
            after_tokens: compacted.after_tokens,
            dropped_messages: compacted.dropped_messages,
        })
    }
}

fn conversation_metadata(
    config: &AgentConfig,
    runtime: &AgentRuntime,
    session_id: SessionId,
) -> ConversationMetadata {
    ConversationMetadata {
        session_id,
        model_backend: runtime.model_backend().to_string(),
        model: config.model.clone(),
        working_dir: config.working_dir.clone(),
        system_prompt: config.system_prompt.clone(),
        max_turns: config.max_turns,
        max_context_tokens: config.max_context_tokens,
        max_tool_duration: config.max_tool_duration,
    }
}

fn last_user_turn(messages: &[Message]) -> Option<(usize, String)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| user_prompt(message).map(|prompt| (index, prompt)))
}

fn user_prompt(message: &Message) -> Option<String> {
    let MessageContent::User(contents) = &message.content else {
        return None;
    };
    Some(
        contents
            .iter()
            .map(|content| match content {
                Content::Text(text) => text.clone(),
                Content::Image { media_type, .. } => format!("[image: {media_type}]"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use ash_core::{
        ContentBlock, MessageContent, MessageId, ModelClient as ProtocolAdapter, ModelId,
        ModelRequest as LlmRequest, ModelStream as ProtocolStream, ModelStreamEvent as StreamItem,
        Role, ToolCallId,
    };
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
            system_prompt: Some("current prompt".to_string()),
            tools: Vec::new(),
            model: ModelId::new("current-model"),
            max_turns: 10,
            working_dir,
            max_context_tokens: 1000,
            max_tool_duration: Duration::from_secs(5),
            agent_path: "/root".to_string(),
            root_session_id: None,
        }
    }

    fn runtime() -> AgentRuntime {
        AgentRuntime::new(
            Arc::new(MockAdapter {
                responses: Mutex::new(VecDeque::new()),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            "test",
        )
    }

    fn runtime_in(directory: &std::path::Path) -> AgentRuntime {
        runtime().with_conversation_repository(Arc::new(crate::JsonlConversationRepository::new(
            directory,
        )))
    }

    async fn session_with_messages(
        config: AgentConfig,
        runtime: AgentRuntime,
        messages: &[Message],
    ) -> AgentSession {
        let mut session = AgentSession::new(config, runtime);
        for message in messages {
            session
                .store
                .append(ConversationEntry::Message(message.clone()))
                .await
                .unwrap();
            session
                .log
                .push(ConversationEntry::Message(message.clone()));
        }
        session
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
    fn fork_points_list_real_user_prompts_newest_first() {
        let tool_id = ToolCallId::from_provider("call");
        let first = Message::user("first");
        let second = Message::user("second\nline");
        let mut session = AgentSession::new(config(PathBuf::from(".")), runtime());
        session.log = ConversationLog::from_messages(vec![
            first.clone(),
            Message::assistant_text("answer"),
            Message {
                id: MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: tool_id,
                    result: Ok("result".to_string()),
                    attachments: Vec::new(),
                },
            },
            second.clone(),
        ]);

        assert_eq!(
            session.fork_points(),
            vec![
                ForkPoint {
                    message_id: second.id,
                    prompt: "second\nline".to_string(),
                },
                ForkPoint {
                    message_id: first.id,
                    prompt: "first".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn fork_creates_a_new_session_before_the_selected_prompt() {
        let directory = TempDir::new().unwrap();
        let config = config(directory.path().to_path_buf());
        let runtime = runtime_in(directory.path());
        let first = Message::user("first");
        let answer = Message::assistant_text("first answer");
        let expected_ids = [first.id, answer.id];
        let selected = Message::user("try another direction");
        let later = Message::assistant_text("second answer");
        let messages = vec![first.clone(), answer.clone(), selected.clone(), later];
        let mut session = session_with_messages(config, runtime.clone(), &messages).await;
        let original_id = session.id();

        let forked = session.fork_at(selected.id).await.unwrap().unwrap();

        assert_ne!(session.id(), original_id);
        assert_eq!(session.messages().len(), 2);
        assert_eq!(
            session
                .messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(
            forked
                .messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(forked.prompt, "try another direction");
        let original = runtime
            .conversations()
            .open(original_id)
            .await
            .unwrap()
            .unwrap()
            .load()
            .await
            .unwrap();
        assert_eq!(original.messages().len(), 4);
        let stored_fork = session.store.load().await.unwrap();
        assert_eq!(
            stored_fork
                .messages()
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
    }

    #[tokio::test]
    async fn resume_keeps_the_current_runtime_configuration() {
        let directory = TempDir::new().unwrap();
        let current_dir = directory.path().join("current");
        tokio::fs::create_dir(&current_dir).await.unwrap();
        let runtime = runtime_in(directory.path());
        let mut session = AgentSession::new(config(current_dir.clone()), runtime.clone());
        let saved_id = SessionId::new();
        let saved_message = Message::user("saved question");
        let mut saved = runtime.conversations().create(conversation_metadata(
            &config(directory.path().join("old")),
            &runtime,
            saved_id,
        ));
        saved
            .append(ConversationEntry::Message(saved_message))
            .await
            .unwrap();

        let restored = session.resume(saved_id).await.unwrap().unwrap();

        assert_eq!(session.id(), saved_id);
        assert_eq!(session.config.model.as_str(), "current-model");
        assert_eq!(session.runtime.model_backend(), "test");
        assert_eq!(
            session.config.system_prompt.as_deref(),
            Some("current prompt")
        );
        assert_eq!(session.config.working_dir, current_dir);
        assert_eq!(restored.model, "current-model");
        assert_eq!(restored.protocol, "test");
        assert_eq!(restored.messages.len(), 1);
    }

    #[tokio::test]
    async fn rollback_keeps_memory_when_persistence_fails() {
        let directory = TempDir::new().unwrap();
        let mut session = AgentSession::new(config(directory.path().to_path_buf()), runtime());
        let message = Message::user("unpersisted");
        session.log.push(ConversationEntry::Message(message));

        let result = session.rollback_last_turn().await;

        assert!(result.is_err());
        assert_eq!(session.log.messages().len(), 1);
        assert_eq!(session.log.model_context().len(), 1);
    }

    #[tokio::test]
    async fn submit_keeps_memory_clean_when_persistence_fails() {
        let directory = TempDir::new().unwrap();
        let blocked_parent = directory.path().join("not-a-directory");
        tokio::fs::write(&blocked_parent, b"file").await.unwrap();
        let config = config(directory.path().to_path_buf());
        let runtime = runtime_in(&blocked_parent);
        let mut session = AgentSession::new(config, runtime);
        let (events, mut received) = mpsc::channel(4);

        let result = session
            .submit("unpersisted", events, CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(session.log.messages().is_empty());
        assert!(session.log.model_context().is_empty());
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
        let runtime = runtime_in(directory.path());
        let messages = vec![
            Message::user(&format!("old request {}", "x".repeat(10_000))),
            Message::assistant_text("old answer"),
            Message::user("middle request"),
            Message::assistant_text("middle answer"),
            Message::user("recent request"),
            Message::assistant_text("recent answer"),
        ];
        let mut session = session_with_messages(config, runtime, &messages).await;
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
        assert_eq!(session.log.messages().len(), 6);
        assert_eq!(session.log.model_context().len(), 5);
        assert!(result.after_tokens < result.before_tokens);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].tools.is_empty());
        }
        let stored = session.store.load().await.unwrap();
        assert_eq!(stored.messages().len(), 6);
        assert_eq!(stored.model_context().len(), 5);
        assert!(matches!(
            &stored.messages()[0].content,
            MessageContent::User(contents)
                if matches!(contents.as_slice(), [Content::Text(text)] if text.starts_with("old request"))
        ));
    }
}
