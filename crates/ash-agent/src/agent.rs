use std::{sync::Arc, time::Duration};

use ash_core::{AshError, ModelId, RepositoryInstruction, Tool, ToolDefinition, ToolError};

use crate::{Profile, PromptContext};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentSnapshot {
    pub profile: Option<Profile>,
    system_prompt: Option<String>,
    prompt_context: Option<PromptContext>,
    tools: Vec<String>,
    tool_instructions: std::collections::BTreeMap<String, String>,
    observed_instructions: Vec<RepositoryInstruction>,
}

impl AgentSnapshot {
    pub fn without_tools(mut self, names: &[&str]) -> Self {
        self.tools.retain(|name| !names.contains(&name.as_str()));
        self.tool_instructions
            .retain(|name, _| !names.contains(&name.as_str()));
        self
    }
}

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 1_000_000;
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_mins(2);
pub const COMPACTION_TRIGGER_PERCENT: usize = 80;

/// Immutable behavior shared by every session that runs this agent.
#[derive(Clone)]
pub struct Agent {
    catalog: Vec<Arc<dyn Tool>>,
    observed_instructions: Vec<RepositoryInstruction>,
    system_prompt: Option<String>,
    profile: Option<Profile>,
    prompt_context: Option<PromptContext>,
    tools: Vec<Arc<dyn Tool>>,
    model: ModelId,
    max_context_tokens: usize,
    tool_timeout: Duration,
}

impl Agent {
    #[must_use]
    pub fn new(model: impl Into<ModelId>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            catalog: deduplicate_tools(tools.clone()),
            observed_instructions: Vec::new(),
            system_prompt: None,
            profile: None,
            prompt_context: None,
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

    pub fn with_profile(mut self, profile: Profile) -> Result<Self, AshError> {
        self.tools = profile.select_tools(&self.tools)?;
        self.profile = Some(profile);
        Ok(self)
    }

    pub fn with_prompt_context(mut self, mut context: PromptContext) -> Self {
        self.observed_instructions = context.take_repository();
        self.prompt_context = Some(context);
        self
    }

    pub fn profile(&self) -> Option<&Profile> {
        self.profile.as_ref()
    }

    pub fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            profile: self.profile.clone(),
            system_prompt: self.system_prompt.clone(),
            prompt_context: self.prompt_context.clone(),
            tools: self
                .tools
                .iter()
                .map(|tool| tool.name().to_string())
                .collect(),
            tool_instructions: self
                .tools
                .iter()
                .filter_map(|tool| {
                    tool.instructions()
                        .map(|text| (tool.name().to_string(), text.to_string()))
                })
                .collect(),
            observed_instructions: self.observed_instructions.clone(),
        }
    }

    pub fn restore(&self, snapshot: &AgentSnapshot) -> Result<Self, AshError> {
        let mut agent = self.clone();
        agent.profile = snapshot.profile.clone();
        agent.system_prompt = snapshot.system_prompt.clone();
        agent.prompt_context = snapshot.prompt_context.clone();
        agent.observed_instructions = snapshot.observed_instructions.clone();
        agent.tools = snapshot
            .tools
            .iter()
            .map(|name| {
                self.catalog
                    .iter()
                    .find(|tool| tool.name() == name)
                    .cloned()
                    .map(|tool| match snapshot.tool_instructions.get(name) {
                        Some(text) => ash_core::with_tool_instructions(tool, text.clone()),
                        None => tool,
                    })
                    .ok_or_else(|| AshError::Config(format!("cannot restore missing tool: {name}")))
            })
            .collect::<Result<_, _>>()?;
        Ok(agent)
    }

    pub(crate) fn observed_instructions(&self) -> &[RepositoryInstruction] {
        &self.observed_instructions
    }

    pub(crate) fn apply_output(&mut self, output: &ash_core::ToolOutput) -> Result<(), ToolError> {
        let additions = output
            .installed_tools
            .iter()
            .map(|name| {
                self.catalog
                    .iter()
                    .find(|tool| tool.name() == name)
                    .cloned()
                    .ok_or_else(|| {
                        ToolError::Execution(format!("unregistered capability tool: {name}"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        for tool in additions {
            if !self
                .tools
                .iter()
                .any(|installed| installed.name() == tool.name())
            {
                self.tools.push(tool);
            }
        }
        for instruction in &output.observed_instructions {
            if let Some(existing) = self
                .observed_instructions
                .iter_mut()
                .find(|existing| existing.path == instruction.path)
            {
                *existing = instruction.clone();
            } else {
                self.observed_instructions.push(instruction.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn apply_step(&mut self, step: &ash_core::Step) -> Result<(), ToolError> {
        for call in step.tool_calls() {
            if let Ok(output) = &call.result {
                self.apply_output(output)?;
            }
        }
        Ok(())
    }

    pub fn installing_tools(
        mut self,
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> Result<Self, ToolError> {
        let incoming = tools.into_iter().collect::<Vec<_>>();
        let mut names = self
            .tools
            .iter()
            .map(|tool| tool.name())
            .collect::<std::collections::HashSet<_>>();
        for tool in &incoming {
            if !names.insert(tool.name()) {
                return Err(ToolError::Execution(format!(
                    "tool already installed: {}",
                    tool.name()
                )));
            }
        }
        for tool in &incoming {
            if let Some(existing) = self
                .catalog
                .iter()
                .find(|existing| existing.name() == tool.name())
            {
                if !Arc::ptr_eq(existing, tool) {
                    return Err(ToolError::Execution(format!(
                        "conflicting tool implementation: {}",
                        tool.name()
                    )));
                }
            } else {
                self.catalog.push(tool.clone());
            }
        }
        self.tools.extend(incoming);
        Ok(self)
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
        for tool in &self.tools {
            upsert_tool(&mut self.catalog, tool.clone());
        }
        self
    }

    /// Add tools by name. A later tool replaces an existing tool with the same
    /// name in place, so the model and executor always share one definition.
    #[must_use]
    pub fn pushing_tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        for tool in tools {
            upsert_tool(&mut self.catalog, tool.clone());
            upsert_tool(&mut self.tools, tool);
        }
        self
    }

    #[must_use]
    pub fn without_tools(mut self, names: &[&str]) -> Self {
        self.tools.retain(|tool| !names.contains(&tool.name()));
        self
    }

    pub fn restrict_tools(mut self, names: &[&str]) -> Self {
        self.tools.retain(|tool| !names.contains(&tool.name()));
        self.catalog.retain(|tool| !names.contains(&tool.name()));
        self
    }

    #[must_use]
    pub fn system_prompt(&self) -> Option<String> {
        let explicit = self
            .system_prompt
            .as_deref()
            .into_iter()
            .chain(self.profile.as_ref().map(Profile::instructions));
        let environment = self.prompt_context.as_ref().map(PromptContext::environment);
        let tools = self.tools.iter().filter_map(|tool| tool.instructions());
        let sections = explicit
            .chain(environment)
            .chain(tools)
            .map(str::trim)
            .filter(|section| !section.is_empty())
            .map(str::to_owned)
            .chain(
                self.observed_instructions
                    .iter()
                    .map(RepositoryInstruction::render),
            )
            .collect::<Vec<_>>();
        (!sections.is_empty()).then(|| sections.join("\n\n"))
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
    use ash_core::{define_tool, with_tool_instructions};

    use super::*;

    #[test]
    fn restricted_tools_cannot_return_through_capability_outputs_or_restore() {
        let base = Agent::new(
            ModelId::new("test"),
            vec![named_tool("read", "read"), named_tool("agent", "create")],
        );
        let snapshot = base.snapshot();
        let mut worker = base.restrict_tools(&["agent"]);
        let output = ash_core::ToolOutput {
            installed_tools: vec!["agent".into()],
            ..ash_core::ToolOutput::from("skill requests agent creation")
        };
        assert!(worker.apply_output(&output).is_err());
        assert_eq!(worker.tools().len(), 1);
        assert!(worker.restore(&snapshot).is_err());
        let stripped = snapshot.without_tools(&["agent"]);
        assert_eq!(worker.restore(&stripped).unwrap().tools().len(), 1);
    }

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

    #[test]
    fn tool_instructions_follow_installation_replacement_and_removal() {
        let base =
            Agent::new(ash_core::ModelId::new("model"), Vec::new()).with_system_prompt("explicit");
        let installed = base
            .clone()
            .installing_tools([
                with_tool_instructions(named_tool("first", "first"), "first rules"),
                with_tool_instructions(named_tool("second", "second"), "second rules"),
            ])
            .unwrap();
        assert_eq!(
            installed.system_prompt().as_deref(),
            Some("explicit\n\nfirst rules\n\nsecond rules")
        );
        assert_eq!(base.system_prompt().as_deref(), Some("explicit"));
        let replaced = installed.pushing_tools([with_tool_instructions(
            named_tool("first", "replacement"),
            "new rules",
        )]);
        assert_eq!(
            replaced.system_prompt().as_deref(),
            Some("explicit\n\nnew rules\n\nsecond rules")
        );
        assert_eq!(
            replaced
                .without_tools(&["first", "second"])
                .system_prompt()
                .as_deref(),
            Some("explicit")
        );
    }

    #[test]
    fn strict_installation_rejects_existing_and_incoming_duplicates() {
        let base = Agent::new(
            ash_core::ModelId::new("model"),
            vec![named_tool("read", "original")],
        );
        assert!(base
            .clone()
            .installing_tools([named_tool("read", "other")])
            .is_err());
        assert!(base
            .clone()
            .installing_tools([named_tool("new", "first"), named_tool("new", "second"),])
            .is_err());
        assert_eq!(base.tools().len(), 1);
        assert_eq!(base.tools()[0].description(), "original");
    }

    #[test]
    fn profile_selection_is_independent_between_definitions() {
        let base = Agent::new(
            ash_core::ModelId::new("model"),
            vec![named_tool("read", "read"), named_tool("write", "write")],
        );
        let profile = Profile::parse(
            "inspect",
            "---\ndescription: Inspect\ntools: read\n---\ninspect rules",
        )
        .unwrap();
        let inspect = base.clone().with_profile(profile).unwrap();
        let default = base
            .with_profile(Profile::builtin("default").unwrap())
            .unwrap();
        assert_eq!(inspect.tools().len(), 1);
        assert_eq!(inspect.system_prompt().as_deref(), Some("inspect rules"));
        assert_eq!(default.tools().len(), 2);
        assert!(!default.system_prompt().unwrap().contains("inspect rules"));
    }

    #[test]
    fn repository_rules_replace_the_same_source_without_duplicate_versions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("AGENTS.md");
        std::fs::write(&path, "original rules").unwrap();
        let mut agent = Agent::new(ModelId::new("test"), Vec::new())
            .with_prompt_context(PromptContext::load(directory.path()).unwrap());
        assert_eq!(agent.observed_instructions().len(), 1);
        let mut instruction = agent.observed_instructions()[0].clone();
        instruction.content = "updated rules".into();
        let output = ash_core::ToolOutput {
            observed_instructions: vec![instruction],
            ..Default::default()
        };
        agent.apply_output(&output).unwrap();
        agent.apply_output(&output).unwrap();
        assert_eq!(agent.observed_instructions().len(), 1);
        let prompt = agent.system_prompt().unwrap();
        assert!(!prompt.contains("original rules"));
        assert_eq!(prompt.matches("updated rules").count(), 1);
        let restored = agent.restore(&agent.snapshot()).unwrap();
        assert_eq!(restored.system_prompt(), agent.system_prompt());
    }
}
