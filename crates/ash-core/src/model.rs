use derive_more::{Display, From, Into};
use serde::{Deserialize, Serialize};
use strum::EnumString;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, Display)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Display, EnumString)]
pub enum Protocol {
    #[strum(serialize = "anthropic")]
    AnthropicMessages,
    #[strum(serialize = "openai")]
    OpenaiCompletions,
    #[strum(serialize = "openai-responses")]
    OpenaiResponses,
}

impl Protocol {
    pub const fn as_cli_name(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenaiCompletions => "openai",
            Self::OpenaiResponses => "openai-responses",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub api_key: secrecy::SecretString,
    pub base_url: Option<String>,
}
