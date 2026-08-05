use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, Tool, TreeId};

use crate::{ContextPolicy, ThreadKind};

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 1_000_000;
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
    pub kind: ThreadKind,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl Default for ThreadOptions {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_secs(120),
            path: "/root".to_string(),
            tree_id: None,
            kind: ThreadKind::Root,
            metadata: serde_json::Map::new(),
        }
    }
}

/// Exponential retry backoff for safe model-call retries.
/// First retry waits `base`, then doubles each attempt, capped at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoff {
    pub base: std::time::Duration,
    pub max: std::time::Duration,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            base: std::time::Duration::from_secs(1),
            max: std::time::Duration::from_secs(10),
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
    pub kind: ThreadKind,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// How many times a single model call may be retried after a safe,
    /// retryable failure (network error, upstream 5xx, rate limit, or a
    /// truncated stream). Retries only happen before any tool call has been
    /// executed, so they never repeat side effects. Between attempts the
    /// runner waits an exponential backoff (`RetryBackoff`), cancellable.
    pub max_retries: u32,
    /// Backoff schedule for the retries above.
    pub retry_backoff: RetryBackoff,
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
            kind: options.kind,
            metadata: options.metadata.clone(),
            max_retries: 5,
            retry_backoff: RetryBackoff::default(),
        }
    }
}
