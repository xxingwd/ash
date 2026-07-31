use std::sync::Arc;

use ash_core::{Message, StopReason, ThreadId, Tool, TurnId};

use crate::Input;

#[derive(Clone)]
pub struct TurnContext {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub inputs: Vec<Input>,
    pub messages: Vec<Message>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

#[derive(Default)]
pub struct TurnPatch {
    /// Ephemeral model context. These messages are not added to durable chat history.
    pub context: Vec<Message>,
    /// Tools available only for this turn.
    pub tools: Vec<Arc<dyn Tool>>,
}

#[derive(Clone, Debug)]
pub enum TurnStatus {
    Completed(StopReason),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub status: TurnStatus,
    pub messages: Vec<Message>,
}

/// Runtime extension point for goals, workflows, memory, and product-specific policy.
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    async fn prepare(&self, _turn: &TurnContext) -> Result<TurnPatch, ash_core::AshError> {
        Ok(TurnPatch::default())
    }

    async fn complete(
        &self,
        _turn: &TurnContext,
        _outcome: &TurnOutcome,
    ) -> Result<(), ash_core::AshError> {
        Ok(())
    }
}
