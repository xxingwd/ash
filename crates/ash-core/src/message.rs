use derive_more::{Display, From, Into};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into)]
pub struct MessageId(Uuid);

impl MessageId {
    #[must_use]
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
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn from_provider(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
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
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(Uuid::from_u128(value))
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
pub struct SessionId(Uuid);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, Display)]
pub struct TreeId(Uuid);

impl TreeId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TreeId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<SessionId> for TreeId {
    fn from(value: SessionId) -> Self {
        Self(value.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    const fn as_serialized(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }
}

impl std::fmt::Display for Role {
    /// Same vocabulary as serde, `FromStr`, and `as_serialized`: the stable
    /// lowercase provider forms. Display and `FromStr` are mutual inverses.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_serialized())
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Image { media_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageContent {
    User(Vec<Content>),
    Assistant(Vec<ContentBlock>),
    System(Vec<Content>),
    ToolResult {
        id: ToolCallId,
        result: std::result::Result<String, String>,
        attachments: Vec<Content>,
    },
}

impl MessageContent {
    #[must_use]
    pub const fn role(&self) -> Role {
        match self {
            Self::User(_) | Self::ToolResult { .. } => Role::User,
            Self::Assistant(_) => Role::Assistant,
            Self::System(_) => Role::System,
        }
    }
}

/// Durable transcript message. `role` is derived from `content` so illegal
/// role/content pairs cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub content: MessageContent,
}

impl Message {
    #[must_use]
    pub const fn role(&self) -> Role {
        self.content.role()
    }

    #[must_use]
    pub fn user(text: &str) -> Self {
        Self::user_content(vec![Content::Text(text.to_string())])
    }

    #[must_use]
    pub fn user_content(content: Vec<Content>) -> Self {
        Self {
            id: MessageId::new(),
            content: MessageContent::User(content),
        }
    }

    #[must_use]
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            id: MessageId::new(),
            content: MessageContent::Assistant(blocks),
        }
    }

    #[must_use]
    pub fn assistant_text(text: &str) -> Self {
        Self::assistant(vec![ContentBlock::Text(text.to_string())])
    }

    #[must_use]
    pub fn system(text: &str) -> Self {
        Self::system_content(vec![Content::Text(text.to_string())])
    }

    #[must_use]
    pub fn system_content(content: Vec<Content>) -> Self {
        Self {
            id: MessageId::new(),
            content: MessageContent::System(content),
        }
    }

    #[must_use]
    pub fn tool_result(
        id: ToolCallId,
        result: std::result::Result<String, String>,
        attachments: Vec<Content>,
    ) -> Self {
        Self {
            id: MessageId::new(),
            content: MessageContent::ToolResult {
                id,
                result,
                attachments,
            },
        }
    }

    #[must_use]
    pub fn is_user_turn(&self) -> bool {
        matches!(self.content, MessageContent::User(_))
    }

    #[must_use]
    pub fn user_turn_text(&self) -> Option<String> {
        if self.is_user_turn() {
            self.content_text()
        } else {
            None
        }
    }

    pub fn content_text(&self) -> Option<String> {
        let contents = match &self.content {
            MessageContent::User(contents) | MessageContent::System(contents) => contents,
            MessageContent::Assistant(_) | MessageContent::ToolResult { .. } => return None,
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
    #[must_use]
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
        assert_eq!(system.role(), Role::System);
    }

    #[test]
    fn messages_serialize_content_without_a_role_field() {
        let json = serde_json::to_value(Message::system("rules")).unwrap();
        let message: Message = serde_json::from_value(json.clone()).unwrap();

        assert!(json.get("role").is_none());
        assert_eq!(json["content"]["System"][0]["Text"], "rules");
        assert_eq!(message.role(), Role::System);
        assert!(matches!(message.content, MessageContent::System(_)));
    }
}
