use std::sync::Arc;

use ash_core::{SessionId, SessionSummary};
use serde::{Deserialize, Serialize};

use crate::{LogEntry, SessionLog};

/// Whether a session belongs to the interactive root session or to a spawned
/// sub-agent. Sub-agent sessions are hidden from the session list and cannot
/// be resumed as root sessions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Root,
    Subagent,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionMetadata {
    pub session_id: SessionId,
    pub kind: SessionKind,
}

#[derive(Clone, Debug)]
pub struct StoredSession {
    pub metadata: SessionMetadata,
    pub log: SessionLog,
}

pub struct OpenedSession {
    pub session: StoredSession,
    pub writer: Box<dyn SessionAppender>,
}

impl std::fmt::Debug for OpenedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenedSession")
            .field("session", &self.session)
            .field("writer", &"<session appender>")
            .finish()
    }
}

/// Exclusively owned append capability for one durable session.
///
/// A backend can retain a lock, transaction, or remote lease in this handle
/// without exposing its concrete writer type to the runtime.
#[async_trait::async_trait]
pub trait SessionAppender: Send {
    async fn append(&mut self, entries: &[LogEntry]) -> Result<(), ash_core::AshError>;
}

/// Durable, storage-neutral boundary for session state.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(
        &self,
        metadata: SessionMetadata,
        entries: &[LogEntry],
    ) -> Result<(), ash_core::AshError>;

    async fn load(
        &self,
        session_id: SessionId,
    ) -> Result<Option<StoredSession>, ash_core::AshError>;

    /// Load one session and keep its exclusively locked append handle open.
    async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<Option<OpenedSession>, ash_core::AshError>;

    async fn list(
        &self,
        excluded_session: Option<SessionId>,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError>;

    /// Open an append-only writer for a session. The runtime's hot write path
    /// holds one exclusively locked writer per active session.
    async fn open_writer(
        &self,
        metadata: SessionMetadata,
    ) -> Result<Box<dyn SessionAppender>, ash_core::AshError>;
}

pub type SharedSessionStore = Arc<dyn SessionStore>;

pub struct SessionPersistence {
    writer: Arc<tokio::sync::Mutex<Box<dyn SessionAppender>>>,
    /// Entries buffered in memory and not yet written to disk. Flushed at
    /// commit points (`TurnEnd`, rollback, checkpoint) so one turn costs two
    /// writes: the accepted inputs, then all messages plus the turn end.
    pending: Vec<LogEntry>,
}

impl SessionPersistence {
    pub(crate) fn new(writer: Arc<tokio::sync::Mutex<Box<dyn SessionAppender>>>) -> Self {
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
    pub(crate) const fn pending_len(&self) -> usize {
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
