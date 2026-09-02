use ash_core::{SessionEvent, SessionId, TurnId, TurnStats};

/// Presentation-only snapshot of a child agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentView {
    pub root_id: SessionId,
    pub session_id: SessionId,
    pub name: String,
    pub state: SubagentViewState,
    pub stats: TurnStats,
    pub context_tokens: Option<u64>,
    pub context_limit: Option<u64>,
    pub tool_calls: usize,
    pub active_turn: Option<TurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentUpdate {
    pub root_id: SessionId,
    pub session_id: SessionId,
    pub name: String,
    pub kind: SubagentUpdateKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentUpdateKind {
    StateChanged(SubagentViewState),
    Session(SessionEvent),
    Removed,
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
