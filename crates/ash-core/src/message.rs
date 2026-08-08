use derive_more::{Display, From, Into};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into)]
pub struct MessageId(Uuid);

impl MessageId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, derive_more::Display,
)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn from_provider(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ToolCallId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, Display)]
pub struct TurnId(Uuid);

impl TurnId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    From,
    Into,
    derive_more::Display,
)]
pub struct ThreadId(Uuid);

impl ThreadId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for ThreadId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, Display)]
pub struct TreeId(Uuid);

impl TreeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TreeId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<ThreadId> for TreeId {
    fn from(value: ThreadId) -> Self {
        Self(value.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, strum::EnumString, strum::EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    fn as_serialized(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }
}

impl Serialize for Role {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_serialized())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "system" => Ok(Self::System),
            other => Err(serde::de::Error::custom(format!("unknown role: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Image { media_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, enum_as_inner::EnumAsInner)]
pub enum ContentBlock {
    Text(String),
    Thought {
        text: String,
        elapsed_seconds: u64,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, enum_as_inner::EnumAsInner)]
pub enum MessageContent {
    User(Vec<Content>),
    Assistant(Vec<ContentBlock>),
    ToolResult {
        id: ToolCallId,
        result: std::result::Result<String, String>,
        attachments: Vec<Content>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: MessageContent,
}

impl Message {
    pub fn user(text: &str) -> Self {
        Self::user_content(vec![Content::Text(text.to_string())])
    }

    pub fn user_content(content: Vec<Content>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::User,
            content: MessageContent::User(content),
        }
    }

    pub fn assistant_text(text: &str) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::Assistant,
            content: MessageContent::Assistant(vec![ContentBlock::Text(text.to_string())]),
        }
    }

    pub fn system(text: &str) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::System,
            content: MessageContent::User(vec![Content::Text(text.to_string())]),
        }
    }

    pub fn is_user_turn(&self) -> bool {
        self.role == Role::User && matches!(self.content, MessageContent::User(_))
    }

    pub fn user_turn_text(&self) -> Option<String> {
        if self.is_user_turn() {
            self.content_text()
        } else {
            None
        }
    }

    pub fn content_text(&self) -> Option<String> {
        let MessageContent::User(contents) = &self.content else {
            return None;
        };
        Some(
            contents
                .iter()
                .map(Content::display_text)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

impl Content {
    pub fn display_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Image { media_type, .. } => format!("[image: {media_type}]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_turn_helpers_exclude_system_messages() {
        let user = Message::user_content(vec![
            Content::Text("inspect".to_string()),
            Content::Image {
                media_type: "image/png".to_string(),
                data: vec![1],
            },
        ]);
        let system = Message::system("rules");

        assert!(user.is_user_turn());
        assert_eq!(
            user.user_turn_text().as_deref(),
            Some("inspect\n[image: image/png]")
        );
        assert!(!system.is_user_turn());
        assert_eq!(system.user_turn_text(), None);
        assert_eq!(system.content_text().as_deref(), Some("rules"));
    }
}
