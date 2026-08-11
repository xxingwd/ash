use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, Tool, ToolDefinition, TreeId};

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

/// Per-thread execution scope.
#[derive(Clone)]
pub struct ThreadOptions {
    pub working_dir: PathBuf,
    pub tool_timeout: Duration,
    pub path: String,
    pub tree_id: Option<TreeId>,
    pub kind: ThreadKind,
}

impl Default for ThreadOptions {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_mins(2),
            path: default_agent_path(),
            tree_id: None,
            kind: ThreadKind::Root,
        }
    }
}

/// The agent's root path defaults to the process working directory, matching
/// the relative `working_dir` default. Falls back to `/root` only when the
/// directory cannot be resolved (for example because it was removed).
fn default_agent_path() -> String {
    std::env::current_dir().map_or_else(
        |_| "/root".to_string(),
        |directory| directory.display().to_string(),
    )
}

/// Exponential retry backoff for safe model-call retries.
/// First retry waits `base`, then doubles each attempt, capped at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoff {
    pub base: Duration,
    pub max: Duration,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(10),
        }
    }
}

/// Private composition consumed by the model/tool execution engine.
#[derive(Clone)]
pub struct RunConfig {
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
            max_retries: 5,
            retry_backoff: RetryBackoff::default(),
        }
    }

    /// Collect tool definitions once for request sizing, context policy, and
    /// projections so every call site shares the same mapping.
    pub(crate) fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }
}
