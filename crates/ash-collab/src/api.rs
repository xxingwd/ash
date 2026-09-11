use ash_core::SessionId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(crate) fn default_profile() -> String {
    "default".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentArgs {
    #[serde(default = "default_profile")]
    pub profile: String,
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageArgs {
    pub agent_id: SessionId,
    pub message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupArgs {
    pub group_id: String,
    pub agent_id: Option<SessionId>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryArgs {
    pub group_id: Option<String>,
    pub before: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitArgs {
    pub agent_id: Option<SessionId>,
    pub group_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Blueprint {
    pub agents: Vec<BlueprintAgent>,
    #[serde(default)]
    pub groups: Vec<BlueprintGroup>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintAgent {
    pub name: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintGroup {
    pub name: String,
    pub owner: Option<String>,
    pub members: Vec<String>,
    #[serde(default)]
    pub prompt: String,
}
