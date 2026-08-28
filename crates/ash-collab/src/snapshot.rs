use ash_core::{SessionId, Usage};
use serde::{Deserialize, Serialize};

/// Host-facing projection of the agents owned by one root session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentTreeSnapshot {
    pub root_id: SessionId,
    pub agents: Vec<SubagentSnapshot>,
}

/// Display-oriented snapshot of one child agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub name: String,
    pub state: SubagentState,
    /// Settled protocol usage plus the active turn's latest progress snapshot.
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Idle,
    Running,
}

impl SubagentState {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }
}
