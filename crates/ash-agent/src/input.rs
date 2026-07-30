use ash_core::{Content, Message};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    User,
    Steering,
    FollowUp,
    Scheduled,
    Heartbeat,
    System,
    ChildAgent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentInput {
    pub trigger: Trigger,
    pub content: Vec<Content>,
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl AgentInput {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            trigger: Trigger::User,
            content: vec![Content::Text(text.into())],
            idempotency_key: None,
            metadata: serde_json::Map::new(),
        }
    }

    pub fn with_trigger(trigger: Trigger, text: impl Into<String>) -> Self {
        Self {
            trigger,
            content: vec![Content::Text(text.into())],
            idempotency_key: None,
            metadata: serde_json::Map::new(),
        }
    }

    pub fn into_message(self) -> Message {
        Message::user_content(self.content)
    }
}
