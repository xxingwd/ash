use serde::{Deserialize, Serialize};

/// Display-oriented snapshot of a sub-agent, published for UI consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub name: String,
    pub profile: String,
    pub state: SubagentState,
    pub last_message: String,
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
