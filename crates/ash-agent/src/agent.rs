use std::{sync::Arc, time::Duration};

use ash_core::{ModelId, Tool, ToolDefinition};

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 1_000_000;
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_mins(2);
pub const COMPACTION_TRIGGER_PERCENT: usize = 80;

/// Immutable behavior shared by every session that runs this agent.
#[derive(Clone)]
pub struct Agent {
    system_prompt: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    model: ModelId,
    max_context_tokens: usize,
    tool_timeout: Duration,
}

impl Agent {
    #[must_use]
    pub fn new(model: impl Into<ModelId>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            system_prompt: None,
            tools: deduplicate_tools(tools),
            model: model.into(),
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
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
    pub const fn with_max_context_tokens(mut self, max_context_tokens: usize) -> Self {
        self.max_context_tokens = max_context_tokens;
        self
    }

    #[must_use]
    pub const fn with_tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = timeout;
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
        tools
            .into_iter()
            .for_each(|tool| upsert_tool(&mut self.tools, tool));
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
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    #[must_use]
    pub const fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    #[must_use]
    pub const fn tool_timeout(&self) -> Duration {
        self.tool_timeout
    }

    pub(crate) fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }
}

fn deduplicate_tools(tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    tools.into_iter().fold(Vec::new(), |mut unique, tool| {
        upsert_tool(&mut unique, tool);
        unique
    })
}

fn upsert_tool(tools: &mut Vec<Arc<dyn Tool>>, tool: Arc<dyn Tool>) {
    if let Some(existing) = tools
        .iter_mut()
        .find(|existing| existing.name() == tool.name())
    {
        *existing = tool;
    } else {
        tools.push(tool);
    }
}

#[cfg(test)]
mod tests {
    use ash_core::define_tool;

    use super::*;

    fn named_tool(name: &str, description: &str) -> Arc<dyn Tool> {
        define_tool(name, description, |_, ()| async { Ok("ok") }).unwrap()
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
