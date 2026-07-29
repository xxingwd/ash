use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, ProviderConfig, SessionId, Tool};

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 200_000;
pub const COMPACTION_TRIGGER_PERCENT: usize = 80;

#[derive(Clone)]
pub struct AgentConfig {
    pub provider: ProviderConfig,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub working_dir: PathBuf,
    pub max_context_tokens: usize,
    pub max_output_tokens: Option<u32>,
    pub max_tool_duration: Duration,
    pub agent_path: String,
    pub root_session_id: Option<SessionId>,
}
