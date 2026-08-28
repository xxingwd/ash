use ash_core::{SessionId, Usage};

/// Presentation-only snapshot of a child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentView {
    pub root_id: SessionId,
    pub name: String,
    pub state: SubagentViewState,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentViewState {
    Idle,
    Running,
}

impl SubagentViewState {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }
}
