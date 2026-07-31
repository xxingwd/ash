use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where an input originated. Delivery policy is expressed by the Thread API.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    #[default]
    User,
    Schedule,
    Heartbeat,
    Agent,
    System,
    Custom(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub source: InputSource,
    pub content: Vec<ash_core::Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl Input {
    pub fn user(text: impl Into<String>) -> Self {
        Self::from_text(InputSource::User, text)
    }

    pub fn from_text(source: InputSource, text: impl Into<String>) -> Self {
        Self {
            source,
            content: vec![ash_core::Content::Text(text.into())],
            idempotency_key: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            || self.content.iter().all(|content| match content {
                ash_core::Content::Text(text) => text.is_empty(),
                ash_core::Content::Image { data, .. } => data.is_empty(),
            })
    }
}

impl From<String> for Input {
    fn from(value: String) -> Self {
        Self::user(value)
    }
}

impl From<&str> for Input {
    fn from(value: &str) -> Self {
        Self::user(value)
    }
}
