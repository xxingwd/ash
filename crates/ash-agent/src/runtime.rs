use std::{path::PathBuf, sync::Arc};

use ash_core::{ModelClient, SessionId, SessionIdentity, SessionSummary};

use crate::jsonl::JsonlSessionStore;
use crate::{Agent, Session, SessionActorState};

/// Provider-neutral dependencies and the only entry point for creating sessions.
#[derive(Clone)]
pub struct Runtime {
    model: Arc<dyn ModelClient>,
    session_store: Arc<JsonlSessionStore>,
}

impl Runtime {
    #[must_use]
    pub fn new(model: Arc<dyn ModelClient>) -> Self {
        Self {
            model,
            session_store: Arc::new(JsonlSessionStore::default()),
        }
    }

    #[must_use]
    pub fn with_session_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.session_store = Arc::new(JsonlSessionStore::new(directory));
        self
    }

    #[must_use]
    pub fn start(&self, agent: &Agent) -> Session {
        Session::spawn(SessionActorState::new(agent.clone(), self.clone()))
    }

    /// Start an empty child session of `parent`.
    #[must_use]
    pub fn start_child(&self, agent: &Agent, parent: SessionIdentity) -> Session {
        Session::spawn(SessionActorState::new_child(
            agent.clone(),
            self.clone(),
            parent,
        ))
    }

    /// Resume an existing session by id.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session store cannot be read or the stored
    /// session cannot be replayed.
    pub async fn resume(
        &self,
        agent: &Agent,
        session_id: SessionId,
    ) -> Result<Option<Session>, ash_core::AshError> {
        let mut state = SessionActorState::new(agent.clone(), self.clone());
        if !state.resume(session_id).await? {
            return Ok(None);
        }
        Ok(Some(Session::spawn(state)))
    }

    /// List resumable root sessions, optionally excluding one session.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session store cannot be read.
    pub async fn list_sessions(
        &self,
        excluded: Option<SessionId>,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        let mut sessions = self.session_store.list_roots().await?;
        if let Some(excluded) = excluded {
            sessions.retain(|summary| summary.session_id != excluded);
        }
        Ok(sessions)
    }

    /// List every session in one collaboration tree, root included.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session store cannot be read.
    pub async fn session_tree(
        &self,
        root_id: SessionId,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.session_store.tree(root_id).await
    }

    /// Delete a root session and every durable child session in its tree.
    /// Returns the number of removed sessions, or zero when the root does not
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the id belongs to a child session, any session
    /// in the tree is still open, or the store cannot complete the deletion.
    pub async fn delete_session_tree(
        &self,
        root_id: SessionId,
    ) -> Result<usize, ash_core::AshError> {
        self.session_store.delete_tree(root_id).await
    }

    pub(crate) fn model(&self) -> &dyn ModelClient {
        self.model.as_ref()
    }

    pub(crate) fn session_store_handle(&self) -> Arc<JsonlSessionStore> {
        Arc::clone(&self.session_store)
    }
}
