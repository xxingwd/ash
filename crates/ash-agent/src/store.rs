use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, ThreadId, ThreadSummary};
use serde::{Deserialize, Serialize};

use crate::{LogEntry, ThreadLog};

/// Whether a thread belongs to the interactive root session or to a spawned
/// sub-agent. Sub-agent threads are hidden from the session list and cannot
/// be resumed as root sessions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    #[default]
    Root,
    Subagent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Version(u64);

impl Version {
    pub const fn initial() -> Self {
        Self(0)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub(crate) fn advance(self, count: usize) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX)),
        )
    }

    pub(crate) fn from_entry_count(count: usize) -> Self {
        Self(u64::try_from(count).unwrap_or(u64::MAX))
    }
}

#[derive(Clone, Debug)]
pub struct ThreadMetadata {
    pub thread_id: ThreadId,
    pub model_backend: String,
    pub model: ModelId,
    pub working_dir: PathBuf,
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    pub max_context_tokens: usize,
    pub max_tool_duration: Duration,
    pub kind: ThreadKind,
}

#[derive(Clone, Debug)]
pub struct StoredThread {
    pub metadata: ThreadMetadata,
    pub log: ThreadLog,
    pub version: Version,
}

/// Durable, storage-neutral boundary for thread state.
///
/// `create` and `append` persist each entry slice as one ordered version change.
/// Implementations must reject stale `expected_version` values.
#[async_trait::async_trait]
pub trait ThreadStore: Send + Sync {
    async fn create(
        &self,
        metadata: ThreadMetadata,
        entries: &[LogEntry],
    ) -> Result<Version, ash_core::AshError>;

    async fn load(&self, thread_id: ThreadId) -> Result<Option<StoredThread>, ash_core::AshError>;

    async fn append(
        &self,
        thread_id: ThreadId,
        expected_version: Version,
        entries: &[LogEntry],
    ) -> Result<Version, ash_core::AshError>;

    async fn list(
        &self,
        excluded_thread: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError>;
}

pub type SharedThreadStore = Arc<dyn ThreadStore>;

pub(crate) struct ThreadPersistence {
    store: SharedThreadStore,
    thread_id: ThreadId,
    version: Version,
    /// Entries appended through this persistence handle. The caller replays
    /// them into its in-memory log so it stays in sync without reloading the
    /// thread from disk.
    appended: Vec<LogEntry>,
}

impl ThreadPersistence {
    pub(crate) fn new(store: SharedThreadStore, thread_id: ThreadId, version: Version) -> Self {
        Self {
            store,
            thread_id,
            version,
            appended: Vec::new(),
        }
    }

    pub(crate) fn version(&self) -> Version {
        self.version
    }

    pub(crate) fn take_appended(&mut self) -> Vec<LogEntry> {
        std::mem::take(&mut self.appended)
    }

    pub(crate) async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        self.version = self
            .store
            .append(self.thread_id, self.version, entries)
            .await?;
        self.appended.extend(entries.iter().cloned());
        Ok(())
    }
}
