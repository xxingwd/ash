use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, ThreadId, ThreadSummary};
use serde::{Deserialize, Serialize};

use crate::{jsonl::ThreadWriter, LogEntry, ThreadLog};

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

    /// Open an append-only writer for a thread. The runtime's hot write path
    /// holds one writer per active thread so each append is a single
    /// write+flush instead of a directory scan plus full-file re-read.
    /// The default implementation is unsupported; storage backends that
    /// cannot keep an open handle may fall back to `append` per call.
    async fn open_writer(
        &self,
        _thread_id: ThreadId,
        _metadata: ThreadMetadata,
    ) -> Result<ThreadWriter, ash_core::AshError> {
        Err(ash_core::AshError::Config(
            "this thread store does not support open writers".to_string(),
        ))
    }
}

pub type SharedThreadStore = Arc<dyn ThreadStore>;

pub(crate) struct ThreadPersistence {
    writer: Arc<tokio::sync::Mutex<ThreadWriter>>,
    version: Version,
    /// Entries buffered in memory and not yet written to disk. Flushed at
    /// commit points (`TurnEnd`, rollback, checkpoint) so one turn costs two
    /// writes: the accepted inputs, then all messages plus the turn end.
    pending: Vec<LogEntry>,
    /// Entries appended through this persistence handle. The caller replays
    /// them into its in-memory log so it stays in sync without reloading the
    /// thread from disk.
    appended: Vec<LogEntry>,
}

impl ThreadPersistence {
    pub(crate) async fn new(writer: Arc<tokio::sync::Mutex<ThreadWriter>>) -> Self {
        let version = writer.lock().await.revision();
        Self {
            writer,
            version,
            pending: Vec::new(),
            appended: Vec::new(),
        }
    }

    pub(crate) fn version(&self) -> Version {
        self.version
    }

    pub(crate) fn take_appended(&mut self) -> Vec<LogEntry> {
        std::mem::take(&mut self.appended)
    }

    /// Buffer entries without touching the disk. Callers see them only via
    /// `take_appended` after `flush`; the in-memory log is the working truth
    /// and the file catches up at the next commit point.
    pub(crate) async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError> {
        self.pending.extend(entries.iter().cloned());
        self.appended.extend(entries.iter().cloned());
        Ok(())
    }

    /// Write all buffered entries in one write+flush and advance the version.
    pub(crate) async fn flush(&mut self) -> Result<(), ash_core::AshError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        self.version = self.writer.lock().await.append(&pending).await?;
        Ok(())
    }
}
