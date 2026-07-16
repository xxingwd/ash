use derive_more::Display;
use enum_as_inner::EnumAsInner;
use serde::{Deserialize, Serialize};

use crate::message::{AgentId, Message, SessionId, ToolCallId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, EnumAsInner)]
pub enum Event {
    TextDelta(String),
    ToolCallStart {
        id: ToolCallId,
        name: String,
        #[serde(default)]
        arguments: serde_json::Value,
    },
    ToolCallEnd {
        id: ToolCallId,
        output: String,
        is_error: bool,
    },
    Thinking(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    AgentStarted {
        session_id: SessionId,
    },
    AgentFinished {
        reason: StopReason,
    },
    SessionRestored {
        session_id: SessionId,
        path: std::path::PathBuf,
        title: String,
        model: String,
        protocol: String,
        working_dir: std::path::PathBuf,
        messages: Vec<Message>,
    },
    SessionsListed {
        sessions: Vec<SessionSummary>,
    },
    TurnRolledBack {
        messages: Vec<Message>,
        prompt: String,
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
    fn old_tool_start_events_default_missing_arguments() {
        let id = ToolCallId::new();
        let event: Event = serde_json::from_value(serde_json::json!({
            "ToolCallStart": {
                "id": id,
                "name": "read"
            }
        }))
        .expect("legacy event should deserialize");

        let Event::ToolCallStart { arguments, .. } = event else {
            panic!("expected tool call start");
        };
        assert_eq!(arguments, serde_json::Value::Null);
    }
}
