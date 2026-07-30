use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, SessionId, SessionSummary};

use crate::{ConversationEntry, ConversationLog};

#[derive(Clone, Debug)]
pub struct ConversationMetadata {
    pub session_id: SessionId,
    pub model_backend: String,
    pub model: ModelId,
    pub working_dir: PathBuf,
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    pub max_context_tokens: usize,
    pub max_tool_duration: Duration,
}

#[async_trait::async_trait]
pub trait ConversationStore: Send {
    fn session_id(&self) -> SessionId;
    async fn append(&mut self, entry: ConversationEntry) -> Result<(), ash_core::AshError>;
    async fn load(&self) -> Result<ConversationLog, ash_core::AshError>;
}

#[async_trait::async_trait]
pub trait ConversationRepository: Send + Sync {
    fn create(&self, metadata: ConversationMetadata) -> Box<dyn ConversationStore>;

    async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<Option<Box<dyn ConversationStore>>, ash_core::AshError>;

    async fn list(
        &self,
        excluded_session: Option<SessionId>,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError>;
}

pub type SharedConversationRepository = Arc<dyn ConversationRepository>;
