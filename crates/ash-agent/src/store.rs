use std::sync::Arc;

use ash_core::{SessionId, SessionIdentity, SessionSummary};

use crate::{LogEntry, SessionLog};

#[derive(Clone, Debug)]
pub struct StoredSession {
    pub identity: SessionIdentity,
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
    /// Open a brand-new session and keep its exclusively locked append handle
    /// open. The runtime's hot write path holds one writer per active session.
    async fn open_new(
        &self,
        identity: SessionIdentity,
    ) -> Result<Box<dyn SessionAppender>, ash_core::AshError>;

    /// Load one session and keep its exclusively locked append handle open.
    async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<Option<OpenedSession>, ash_core::AshError>;

    /// Non-locking read of one session. Used for introspection and tests when
    /// another live writer already holds the exclusive lock.
    async fn load(
        &self,
        session_id: SessionId,
    ) -> Result<Option<StoredSession>, ash_core::AshError>;

    /// List root sessions, newest first.
    async fn list_roots(&self) -> Result<Vec<SessionSummary>, ash_core::AshError>;

    /// List every session in one collaboration tree, root included, newest
    /// first. Children are durable history: they are discoverable here but
    /// never resumable as root sessions.
    async fn tree(&self, root_id: SessionId) -> Result<Vec<SessionSummary>, ash_core::AshError>;

    /// Delete one session tree and return the number of removed sessions.
    /// The id must identify a root session; zero is returned when it does not
    /// exist. Implementations must reject deletion while any session in the
    /// tree is open for writing; unreadable sessions are skipped.
    async fn delete_tree(&self, root_id: SessionId) -> Result<usize, ash_core::AshError>;
}

pub(crate) type SharedSessionStore = Arc<dyn SessionStore>;

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
