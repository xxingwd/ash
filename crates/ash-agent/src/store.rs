use std::sync::Arc;

use ash_core::{ThreadId, ThreadSummary};
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

pub struct OpenedThread {
    pub thread: StoredThread,
    pub writer: Box<dyn ThreadAppender>,
}

impl std::fmt::Debug for OpenedThread {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenedThread")
            .field("thread", &self.thread)
            .field("writer", &"<thread appender>")
            .finish()
    }
}

/// Exclusively owned append capability for one durable thread.
///
/// A backend can retain a lock, transaction, or remote lease in this handle
/// without exposing its concrete writer type to the runtime.
#[async_trait::async_trait]
pub trait ThreadAppender: Send {
    async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError>;
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

    /// Load one thread and keep its exclusively locked append handle open.
    async fn open(&self, thread_id: ThreadId) -> Result<Option<OpenedThread>, ash_core::AshError>;

    async fn list(
        &self,
        excluded_thread: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError>;

    /// Open an append-only writer for a thread. The runtime's hot write path
    /// holds one exclusively locked writer per active thread.
    async fn open_writer(
        &self,
        metadata: ThreadMetadata,
    ) -> Result<Box<dyn ThreadAppender>, ash_core::AshError>;
}

pub type SharedThreadStore = Arc<dyn ThreadStore>;

pub(crate) struct ThreadPersistence {
    writer: Arc<tokio::sync::Mutex<Box<dyn ThreadAppender>>>,
    /// Entries buffered in memory and not yet written to disk. Flushed at
    /// commit points (`TurnEnd`, rollback, checkpoint) so one turn costs two
    /// writes: the accepted inputs, then all messages plus the turn end.
    pending: Vec<LogEntry>,
}

impl ThreadPersistence {
    pub(crate) fn new(writer: Arc<tokio::sync::Mutex<Box<dyn ThreadAppender>>>) -> Self {
        Self {
            writer,
            pending: Vec::new(),
        }
    }

    /// Buffer entries without touching the disk. Callers see them only via
    /// `commit` after a successful write; the in-memory log is the working truth
    /// and the file catches up at the next commit point.
    pub(crate) fn stage(&mut self, entries: &[LogEntry]) {
        self.pending.extend(entries.iter().cloned());
    }

    pub(crate) fn pending(&self) -> &[LogEntry] {
        &self.pending
    }

    /// Number of buffered entries. Used to snapshot the buffer before a model
    /// call so a retry can discard partial output without touching the disk.
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Drop buffered entries back to `len` (a value previously returned by
    /// `pending_len`). Only safe before `commit`; used by the retry path to
    /// discard a partial assistant message so it never reaches disk.
    pub(crate) fn rollback_to(&mut self, len: usize) {
        self.pending.truncate(len);
    }

    /// Write all buffered entries in one write+flush and return the committed
    /// entries for replay into memory. On failure the buffer is kept so the
    /// caller can retry.
    pub(crate) async fn commit(&mut self) -> Result<Vec<LogEntry>, ash_core::AshError> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let pending = std::mem::take(&mut self.pending);
        if let Err(error) = self.writer.lock().await.append(&pending).await {
            self.pending = pending;
            return Err(error);
        }
        Ok(pending)
    }
}
