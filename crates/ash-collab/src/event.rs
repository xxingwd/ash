use ash_core::{SessionEvent, SessionId};

use crate::SubagentState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentEvent {
    pub root_id: SessionId,
    pub session_id: SessionId,
    pub name: String,
    pub kind: SubagentEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentEventKind {
    StateChanged(SubagentState),
    Session(SessionEvent),
    Removed,
}
