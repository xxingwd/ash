use std::sync::Arc;

use ash_core::{Message, ModelClient, SessionId, SessionSummary};

use crate::{
    agent::RunConfig, Agent, JsonlSessionStore, Session, SessionOptions, SessionState,
    SessionStore, SharedSessionStore,
};

/// Provider-neutral dependencies and the only entry point for creating sessions.
#[derive(Clone)]
pub struct Runtime {
    model: Arc<dyn ModelClient>,
    model_backend: Arc<str>,
    sessions: SharedSessionStore,
}

impl Runtime {
    pub fn new(model: Arc<dyn ModelClient>, model_backend: impl Into<String>) -> Self {
        Self {
            model,
            model_backend: Arc::from(model_backend.into()),
            sessions: Arc::new(JsonlSessionStore::default()),
        }
    }

    #[must_use]
    pub fn with_session_store(mut self, sessions: Arc<dyn SessionStore>) -> Self {
        self.sessions = sessions;
        self
    }

    #[must_use]
    pub fn start(&self, agent: &Agent, options: &SessionOptions) -> Session {
        Session::spawn(SessionState::new(
            RunConfig::new(agent, options),
            self.clone(),
        ))
    }

    /// Start a session seeded with the given history.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when seeding the history fails.
    pub async fn start_with_history(
        &self,
        agent: &Agent,
        options: &SessionOptions,
        history: Vec<Message>,
    ) -> Result<Session, ash_core::AshError> {
        let mut state = SessionState::new(RunConfig::new(agent, options), self.clone());
        state.seed(history).await?;
        Ok(Session::spawn(state))
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
        options: &SessionOptions,
        session_id: SessionId,
    ) -> Result<Option<Session>, ash_core::AshError> {
        let mut state = SessionState::new(RunConfig::new(agent, options), self.clone());
        if !state.resume(session_id).await? {
            return Ok(None);
        }
        Ok(Some(Session::spawn(state)))
    }

    /// List session summaries, optionally excluding one session.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the session store cannot be read.
    pub async fn sessions(
        &self,
        excluded: Option<SessionId>,
    ) -> Result<Vec<SessionSummary>, ash_core::AshError> {
        self.sessions.list(excluded).await
    }

    #[must_use]
    pub fn model_client(&self) -> Arc<dyn ModelClient> {
        Arc::clone(&self.model)
    }

    #[must_use]
    pub fn model_backend(&self) -> &str {
        &self.model_backend
    }

    pub(crate) fn model(&self) -> &dyn ModelClient {
        self.model.as_ref()
    }

    pub(crate) fn session_store_handle(&self) -> SharedSessionStore {
        Arc::clone(&self.sessions)
    }
}
