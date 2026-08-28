use std::sync::Arc;

use derive_more::Display;
use serde::{Deserialize, Serialize};

use crate::{ToolCallId, Turn, TurnId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Started(TurnId),
    Text {
        turn_id: TurnId,
        text: String,
    },
    Thought {
        turn_id: TurnId,
        text: String,
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
    Finished(Arc<Turn>),
    Discarded {
        turn_id: TurnId,
        error: Option<String>,
    },
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
