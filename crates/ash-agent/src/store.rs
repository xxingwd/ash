use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, SessionId, SessionSummary};

use crate::{ConversationEntry, ConversationLog};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct ConversationRevision(u64);

impl ConversationRevision {
    pub const fn initial() -> Self {
        Self(0)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    pub(crate) fn from_entry_count(count: usize) -> Self {
        Self(u64::try_from(count).unwrap_or(u64::MAX))
    }
}

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
    fn revision(&self) -> ConversationRevision;
    async fn append(
        &mut self,
        expected_revision: ConversationRevision,
        entry: ConversationEntry,
    ) -> Result<ConversationRevision, ash_core::AshError>;
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
