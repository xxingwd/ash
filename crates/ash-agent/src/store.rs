use std::sync::Arc;

use ash_core::{ThreadId, ThreadSummary};
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

impl ThreadKind {
    const METADATA_KEY: &'static str = "kind";
    const SUBAGENT_VALUE: &'static str = "subagent";

    pub fn from_metadata(metadata: &serde_json::Map<String, serde_json::Value>) -> Self {
        match metadata
            .get(Self::METADATA_KEY)
            .and_then(serde_json::Value::as_str)
        {
            Some(Self::SUBAGENT_VALUE) => Self::Subagent,
            _ => Self::Root,
        }
    }

    pub fn write_metadata(self, metadata: &mut serde_json::Map<String, serde_json::Value>) {
        match self {
            Self::Root => {
                metadata.remove(Self::METADATA_KEY);
            }
            Self::Subagent => {
                metadata.insert(
                    Self::METADATA_KEY.to_string(),
                    serde_json::json!(Self::SUBAGENT_VALUE),
                );
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ThreadMetadata {
    pub thread_id: ThreadId,
    pub kind: ThreadKind,
}

#[derive(Clone, Debug)]
pub struct StoredThread {
    pub metadata: ThreadMetadata,
    pub log: ThreadLog,
}

#[derive(Debug)]
pub struct OpenedThread {
    pub thread: StoredThread,
    pub writer: ThreadWriter,
}

/// Durable, storage-neutral boundary for thread state.
#[async_trait::async_trait]
pub trait ThreadStore: Send + Sync {
    async fn create(
        &self,
        metadata: ThreadMetadata,
        entries: &[LogEntry],
    ) -> Result<(), ash_core::AshError>;

    async fn load(&self, thread_id: ThreadId) -> Result<Option<StoredThread>, ash_core::AshError>;

    async fn append(
        &self,
        thread_id: ThreadId,
        entries: &[LogEntry],
    ) -> Result<(), ash_core::AshError>;

    /// Load one thread and keep its exclusively locked append handle open.
    async fn open(&self, thread_id: ThreadId) -> Result<Option<OpenedThread>, ash_core::AshError>;

    async fn list(
        &self,
        excluded_thread: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError>;

    /// Open an append-only writer for a thread. The runtime's hot write path
    /// holds one exclusively locked writer per active thread.
    /// The default implementation is unsupported; storage backends that
    /// cannot keep an open handle may fall back to `append` per call.
    async fn open_writer(
        &self,
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
    /// Entries buffered in memory and not yet written to disk. Flushed at
    /// commit points (`TurnEnd`, rollback, checkpoint) so one turn costs two
    /// writes: the accepted inputs, then all messages plus the turn end.
    pending: Vec<LogEntry>,
    /// Successfully flushed entries waiting to be moved into the in-memory log.
    flushed: Vec<LogEntry>,
}

impl ThreadPersistence {
    pub(crate) fn new(writer: Arc<tokio::sync::Mutex<ThreadWriter>>) -> Self {
        Self {
            writer,
            pending: Vec::new(),
            flushed: Vec::new(),
        }
    }

    pub(crate) fn take_flushed(&mut self) -> Vec<LogEntry> {
        std::mem::take(&mut self.flushed)
    }

    /// Buffer entries without touching the disk. Callers see them only via
    /// `take_flushed` after `flush`; the in-memory log is the working truth
    /// and the file catches up at the next commit point.
    pub(crate) fn append(&mut self, entries: &[LogEntry]) {
        self.pending.extend(entries.iter().cloned());
    }

    /// Write all buffered entries in one write+flush. On a failed write the
    /// buffered entries are kept so the caller can retry.
    pub(crate) async fn flush(&mut self) -> Result<(), ash_core::AshError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        if let Err(error) = self.writer.lock().await.append(&pending).await {
            self.pending = pending;
            return Err(error);
        }
        self.flushed.extend(pending);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_kind_owns_its_metadata_encoding() {
        let mut metadata = serde_json::Map::new();

        ThreadKind::Subagent.write_metadata(&mut metadata);
        assert_eq!(ThreadKind::from_metadata(&metadata), ThreadKind::Subagent);

        ThreadKind::Root.write_metadata(&mut metadata);
        assert_eq!(ThreadKind::from_metadata(&metadata), ThreadKind::Root);
        assert!(metadata.is_empty());
    }
}
