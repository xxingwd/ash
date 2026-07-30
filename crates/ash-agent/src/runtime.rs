use std::sync::Arc;

use ash_core::ModelClient;

use crate::{
    AgentConfig, AgentSession, ConversationRepository, JsonlConversationRepository,
    SharedConversationRepository,
};

/// Shared, provider-neutral dependencies used to create agent sessions.
#[derive(Clone)]
pub struct AgentRuntime {
    model: Arc<dyn ModelClient>,
    model_backend: Arc<str>,
    conversations: SharedConversationRepository,
}

impl AgentRuntime {
    pub fn new(model: Arc<dyn ModelClient>, model_backend: impl Into<String>) -> Self {
        Self {
            model,
            model_backend: Arc::from(model_backend.into()),
            conversations: Arc::new(JsonlConversationRepository::default()),
        }
    }

    pub fn with_conversation_repository(
        mut self,
        conversations: Arc<dyn ConversationRepository>,
    ) -> Self {
        self.conversations = conversations;
        self
    }

    pub fn create_session(&self, config: AgentConfig) -> AgentSession {
        AgentSession::new(config, self.clone())
    }

    pub fn model_client(&self) -> Arc<dyn ModelClient> {
        Arc::clone(&self.model)
    }

    pub fn model_backend(&self) -> &str {
        &self.model_backend
    }

    pub(crate) fn model(&self) -> &dyn ModelClient {
        self.model.as_ref()
    }

    pub(crate) fn conversations(&self) -> &dyn ConversationRepository {
        self.conversations.as_ref()
    }
}
