use std::sync::Arc;

use ash_core::{Message, ModelClient, ThreadId, ThreadSummary};

use crate::{
    agent::RunConfig, Agent, JsonlThreadStore, SharedThreadStore, Thread, ThreadOptions,
    ThreadState, ThreadStore,
};

/// Provider-neutral dependencies and the only entry point for creating threads.
#[derive(Clone)]
pub struct Runtime {
    model: Arc<dyn ModelClient>,
    model_backend: Arc<str>,
    threads: SharedThreadStore,
}

impl Runtime {
    pub fn new(model: Arc<dyn ModelClient>, model_backend: impl Into<String>) -> Self {
        Self {
            model,
            model_backend: Arc::from(model_backend.into()),
            threads: Arc::new(JsonlThreadStore::default()),
        }
    }

    #[must_use]
    pub fn with_thread_store(mut self, threads: Arc<dyn ThreadStore>) -> Self {
        self.threads = threads;
        self
    }

    #[must_use]
    pub fn start(&self, agent: &Agent, options: &ThreadOptions) -> Thread {
        Thread::spawn(ThreadState::new(
            RunConfig::new(agent, options),
            self.clone(),
        ))
    }

    /// Start a thread seeded with the given history.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when seeding the history fails.
    pub async fn start_with_history(
        &self,
        agent: Agent,
        options: ThreadOptions,
        history: Vec<Message>,
    ) -> Result<Thread, ash_core::AshError> {
        let mut state = ThreadState::new(RunConfig::new(&agent, &options), self.clone());
        state.seed(history).await?;
        Ok(Thread::spawn(state))
    }

    /// Resume an existing thread by id.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the thread store cannot be read or the stored
    /// thread cannot be replayed.
    pub async fn resume(
        &self,
        agent: Agent,
        options: ThreadOptions,
        thread_id: ThreadId,
    ) -> Result<Option<Thread>, ash_core::AshError> {
        let mut state = ThreadState::new(RunConfig::new(&agent, &options), self.clone());
        if !state.resume(thread_id).await? {
            return Ok(None);
        }
        Ok(Some(Thread::spawn(state)))
    }

    /// List thread summaries, optionally excluding one thread.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the thread store cannot be read.
    pub async fn threads(
        &self,
        excluded: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError> {
        self.threads.list(excluded).await
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

    pub(crate) fn thread_store_handle(&self) -> SharedThreadStore {
        Arc::clone(&self.threads)
    }
}
