use serde::{Deserialize, Serialize};

/// Display-oriented snapshot of a sub-agent, published for UI consumers
/// (e.g. the inline TUI's sub-agent status region below the composer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub task_name: String,
    pub agent_type: String,
    pub state: SubagentState,
    pub last_task_message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Pending,
    Running,
    Completed,
    Interrupted,
    Errored,
}

impl SubagentState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}
