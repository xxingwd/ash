/// Presentation-only snapshot of a child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentView {
    pub name: String,
    pub profile: String,
    pub state: SubagentViewState,
    pub last_task: String,
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
