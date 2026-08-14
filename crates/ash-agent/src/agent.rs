use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, Tool, ToolDefinition, TreeId};

use crate::{ContextPolicy, SessionKind};

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 1_000_000;
pub const COMPACTION_TRIGGER_PERCENT: usize = 80;

/// Immutable behavior shared by every session that runs this agent.
#[derive(Clone)]
pub struct Agent {
    system_prompt: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    model: ModelId,
    max_turns: u32,
    max_context_tokens: usize,
    context_policy: Arc<dyn ContextPolicy>,
}

impl Agent {
    #[must_use]
    pub fn new(model: impl Into<ModelId>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            system_prompt: None,
            tools: deduplicate_tools(tools),
            model: model.into(),
            max_turns: 100,
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            context_policy: Arc::new(crate::DefaultContextPolicy),
        }
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<ModelId>) -> Self {
        self.model = model.into();
        self
    }

    #[must_use]
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    #[must_use]
    pub fn with_max_context_tokens(mut self, max_context_tokens: usize) -> Self {
        self.max_context_tokens = max_context_tokens;
        self
    }

    #[must_use]
    pub fn with_context_policy(mut self, policy: Arc<dyn ContextPolicy>) -> Self {
        self.context_policy = policy;
        self
    }

    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools = deduplicate_tools(tools);
        self
    }

    /// Add tools by name. A later tool replaces an existing tool with the same
    /// name in place, so the model and executor always share one definition.
    #[must_use]
    pub fn pushing_tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        for tool in tools {
            if let Some(existing) = self
                .tools
                .iter_mut()
                .find(|existing| existing.name() == tool.name())
            {
                *existing = tool;
            } else {
                self.tools.push(tool);
            }
        }
        self
    }

    #[must_use]
    pub fn without_tools(mut self, names: &[&str]) -> Self {
        self.tools.retain(|tool| !names.contains(&tool.name()));
        self
    }

    #[must_use]
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    #[must_use]
    pub fn model(&self) -> &ModelId {
        &self.model
    }

    #[must_use]
    pub const fn max_turns(&self) -> u32 {
        self.max_turns
    }

    #[must_use]
    pub const fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }
}

fn deduplicate_tools(tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    let mut unique: Vec<Arc<dyn Tool>> = Vec::new();
    for tool in tools {
        if let Some(existing) = unique
            .iter_mut()
            .find(|existing| existing.name() == tool.name())
        {
            *existing = tool;
        } else {
            unique.push(tool);
        }
    }
    unique
}

/// Per-session execution scope.
#[derive(Clone)]
pub struct SessionOptions {
    pub working_dir: PathBuf,
    pub tool_timeout: Duration,
    pub path: String,
    pub tree_id: Option<TreeId>,
    pub kind: SessionKind,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            tool_timeout: Duration::from_mins(2),
            path: default_agent_path(),
            tree_id: None,
            kind: SessionKind::Root,
        }
    }
}

/// The agent's root path defaults to the process working directory, matching
/// the relative `working_dir` default. Falls back to `.` only when the
/// directory cannot be resolved.
fn default_agent_path() -> String {
    std::env::current_dir().map_or_else(
        |_| ".".to_string(),
        |directory| directory.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use ash_core::define_tool;

    use super::*;

    fn named_tool(name: &str, description: &str) -> Arc<dyn Tool> {
        define_tool(name, description, |_, _: ()| async { Ok("ok") }).unwrap()
    }

    #[test]
    fn later_duplicate_tools_replace_the_existing_definition_in_place() {
        let agent = Agent::new(
            ModelId::new("model"),
            vec![named_tool("read", "old"), named_tool("bash", "bash")],
        )
        .pushing_tools([named_tool("read", "new")]);

        assert_eq!(agent.tools().len(), 2);
        assert_eq!(agent.tools()[0].name(), "read");
        assert_eq!(agent.tools()[0].description(), "new");
        assert_eq!(agent.tools()[1].name(), "bash");
    }
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
    pub kind: SessionKind,
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
    pub(crate) fn new(agent: &Agent, options: &SessionOptions) -> Self {
        Self {
            system_prompt: agent.system_prompt().map(str::to_string),
            tools: agent.tools().to_vec(),
            model: agent.model().clone(),
            max_turns: agent.max_turns(),
            working_dir: options.working_dir.clone(),
            max_context_tokens: agent.max_context_tokens(),
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
