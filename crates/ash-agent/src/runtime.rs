use std::sync::Arc;

use ash_core::{Message, ModelClient, ThreadId, ThreadSummary, TurnId};
use serde::{Deserialize, Serialize};

use crate::{
    agent::RunConfig, Agent, Extension, JsonlThreadStore, SharedThreadStore, Thread, ThreadOptions,
    ThreadState, ThreadStore, TurnContext, TurnOutcome, TurnPatch,
};

/// A routed event emitted by a thread. `sequence` is monotonic within one thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub thread_id: ThreadId,
    pub turn_id: Option<TurnId>,
    pub sequence: u64,
    pub timestamp: String,
    pub kind: ash_core::EventKind,
}

/// Provider-neutral dependencies and the only entry point for creating threads.
#[derive(Clone)]
pub struct Runtime {
    model: Arc<dyn ModelClient>,
    model_backend: Arc<str>,
    threads: SharedThreadStore,
    extensions: Arc<Vec<Arc<dyn Extension>>>,
}

impl Runtime {
    pub fn new(model: Arc<dyn ModelClient>, model_backend: impl Into<String>) -> Self {
        Self {
            model,
            model_backend: Arc::from(model_backend.into()),
            threads: Arc::new(JsonlThreadStore::default()),
            extensions: Arc::new(Vec::new()),
        }
    }

    pub fn with_thread_store(mut self, threads: Arc<dyn ThreadStore>) -> Self {
        self.threads = threads;
        self
    }

    pub fn with_extension(mut self, extension: Arc<dyn Extension>) -> Self {
        Arc::make_mut(&mut self.extensions).push(extension);
        self
    }

    pub fn start(&self, agent: Agent, options: ThreadOptions) -> Thread {
        Thread::spawn(ThreadState::new(
            RunConfig::new(&agent, &options),
            self.clone(),
        ))
    }

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

    pub async fn threads(
        &self,
        excluded: Option<ThreadId>,
    ) -> Result<Vec<ThreadSummary>, ash_core::AshError> {
        self.threads.list(excluded).await
    }

    pub fn model_client(&self) -> Arc<dyn ModelClient> {
        Arc::clone(&self.model)
    }

    pub fn model_backend(&self) -> &str {
        &self.model_backend
    }

    pub(crate) fn model(&self) -> &dyn ModelClient {
        self.model.as_ref()
    }

    pub(crate) fn thread_store_handle(&self) -> SharedThreadStore {
        Arc::clone(&self.threads)
    }

    pub(crate) async fn prepare_turn(
        &self,
        turn: &TurnContext,
    ) -> Result<TurnPatch, ash_core::AshError> {
        let mut combined = TurnPatch::default();
        for extension in self.extensions.iter() {
            let patch = extension.prepare(turn).await?;
            combined.context.extend(patch.context);
            combined.tools.extend(patch.tools);
        }
        Ok(combined)
    }

    pub(crate) async fn complete_turn(
        &self,
        turn: &TurnContext,
        outcome: &TurnOutcome,
    ) -> Result<(), ash_core::AshError> {
        let mut first_error = None;
        for extension in self.extensions.iter() {
            if let Err(error) = extension.complete(turn, outcome).await {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}
