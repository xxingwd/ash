use derive_more::Display;
use enum_as_inner::EnumAsInner;
use serde::{Deserialize, Serialize};

use crate::message::{AgentId, Message, MessageId, SessionId, ToolCallId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPoint {
    pub message_id: MessageId,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, EnumAsInner)]
pub enum Event {
    TextDelta(String),
    ToolCallStart {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolCallEnd {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
        output: String,
        is_error: bool,
    },
    Thinking(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        generation_ms: u64,
        estimated: bool,
    },
    AgentStarted {
        session_id: SessionId,
    },
    AgentFinished {
        reason: StopReason,
    },
    SessionRestored {
        model: String,
        protocol: String,
        working_dir: std::path::PathBuf,
        messages: Vec<Message>,
    },
    SessionsListed {
        sessions: Vec<SessionSummary>,
    },
    ForkPointsListed {
        points: Vec<ForkPoint>,
    },
    SessionForked {
        model: String,
        protocol: String,
        working_dir: std::path::PathBuf,
        messages: Vec<Message>,
        prompt: String,
    },
    TurnRolledBack {
        prompt: String,
    },
    ContextCompacted {
        before_tokens: u64,
        after_tokens: u64,
        dropped_messages: u64,
        automatic: bool,
    },
    ChildSpawned {
        agent_id: AgentId,
        task: String,
    },
    ChildCompleted {
        agent_id: AgentId,
        summary: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurns,
    Aborted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_events_require_complete_payloads() {
        let id = ToolCallId::new();
        assert!(serde_json::from_value::<Event>(serde_json::json!({
            "ToolCallStart": {
                "id": id,
                "name": "read"
            }
        }))
        .is_err());
        assert!(serde_json::from_value::<Event>(serde_json::json!({
            "ContextCompacted": {
                "before_tokens": 100,
                "after_tokens": 50,
                "dropped_messages": 4
            }
        }))
        .is_err());
    }
}
