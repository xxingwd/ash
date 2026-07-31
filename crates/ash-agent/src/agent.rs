use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, Tool, TreeId};

use crate::ContextPolicy;

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 200_000;
pub const COMPACTION_TRIGGER_PERCENT: usize = 80;

/// Immutable behavior shared by every thread that runs this agent.
#[derive(Clone)]
pub struct Agent {
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub max_context_tokens: usize,
    pub context_policy: Arc<dyn ContextPolicy>,
}

/// Per-thread execution scope. Product-specific state belongs in `metadata`.
#[derive(Clone)]
pub struct ThreadOptions {
    pub working_dir: PathBuf,
    pub tool_timeout: Duration,
    pub path: String,
    pub tree_id: Option<TreeId>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl Default for ThreadOptions {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_secs(120),
            path: "/root".to_string(),
            tree_id: None,
            metadata: serde_json::Map::new(),
        }
    }
}

/// Private composition consumed by the model/tool execution engine.
#[derive(Clone)]
pub(crate) struct RunConfig {
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub working_dir: PathBuf,
    pub max_context_tokens: usize,
    pub context_policy: Arc<dyn ContextPolicy>,
    pub max_tool_duration: Duration,
    pub agent_path: String,
    pub tree_id: Option<TreeId>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl RunConfig {
    pub(crate) fn new(agent: &Agent, options: &ThreadOptions) -> Self {
        Self {
            system_prompt: agent.system_prompt.clone(),
            tools: agent.tools.clone(),
            model: agent.model.clone(),
            max_turns: agent.max_turns,
            working_dir: options.working_dir.clone(),
            max_context_tokens: agent.max_context_tokens,
            context_policy: Arc::clone(&agent.context_policy),
            max_tool_duration: options.tool_timeout,
            agent_path: options.path.clone(),
            tree_id: options.tree_id,
            metadata: options.metadata.clone(),
        }
    }
}
