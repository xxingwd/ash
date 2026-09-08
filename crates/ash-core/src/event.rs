use std::sync::Arc;

use derive_more::Display;
use serde::{Deserialize, Serialize};

use crate::{Step, ToolCallId, Turn, TurnId, TurnStats};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Started(TurnId),
    Retrying {
        turn_id: TurnId,
    },
    Text {
        turn_id: TurnId,
        text: String,
    },
    Thought {
        turn_id: TurnId,
        text: String,
    },
    Activity {
        turn_id: TurnId,
        activity: TurnActivity,
    },
    Context {
        turn_id: TurnId,
        tokens: u64,
        limit: u64,
    },
    ToolStarted {
        turn_id: TurnId,
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished {
        turn_id: TurnId,
        id: ToolCallId,
        result: Result<String, String>,
    },
    StepCommitted {
        turn_id: TurnId,
        index: usize,
        step: Arc<Step>,
    },
    Finished {
        turn: Arc<Turn>,
        summary: Option<String>,
    },
    Discarded {
        turn_id: TurnId,
        error: Option<String>,
    },
}

/// Transient snapshot of one running turn: accumulated provider statistics
/// plus the count of completed tool calls. Consumers replace their view with
/// each snapshot; the settled `Turn` remains canonical.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnActivity {
    pub stats: TurnStats,
    pub completed_tool_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: crate::SessionId,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    #[display("Other({_0})")]
    Other(String),
}
