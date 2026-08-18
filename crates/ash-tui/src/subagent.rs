/// Presentation-only snapshot of a child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentView {
    pub task_path: String,
    pub agent_type: String,
    pub state: SubagentViewState,
    pub last_task_message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentViewState {
    Running,
    Completed,
    Interrupted,
    Errored,
}

impl SubagentViewState {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }
}
