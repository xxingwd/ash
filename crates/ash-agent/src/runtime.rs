use std::sync::Arc;

use ash_core::ModelClient;

use crate::{AgentConfig, AgentSession};

/// Shared, provider-neutral dependencies used to create agent sessions.
#[derive(Clone)]
pub struct AgentRuntime {
    model: Arc<dyn ModelClient>,
    model_backend: Arc<str>,
}

impl AgentRuntime {
    pub fn new(model: Arc<dyn ModelClient>, model_backend: impl Into<String>) -> Self {
        Self {
            model,
            model_backend: Arc::from(model_backend.into()),
        }
    }

    pub fn create_session(&self, config: AgentConfig) -> AgentSession {
        AgentSession::new(config, self.clone())
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
}
