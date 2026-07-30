use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{ModelId, SessionId, Tool};

use crate::ContextPolicy;

pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 200_000;
pub const COMPACTION_TRIGGER_PERCENT: usize = 80;

#[derive(Clone)]
pub struct AgentDefinition {
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub max_context_tokens: usize,
    pub context_policy: Arc<dyn ContextPolicy>,
}

#[derive(Clone)]
pub struct AgentScope {
    pub working_dir: PathBuf,
    pub max_tool_duration: Duration,
    pub agent_path: String,
    pub root_session_id: Option<SessionId>,
}

/// Fully composed definition and invocation scope used by the execution engine.
#[derive(Clone)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub working_dir: PathBuf,
    pub max_context_tokens: usize,
    pub context_policy: Arc<dyn ContextPolicy>,
    pub max_tool_duration: Duration,
    pub agent_path: String,
    pub root_session_id: Option<SessionId>,
}

impl AgentConfig {
    pub fn from_parts(definition: AgentDefinition, scope: AgentScope) -> Self {
        Self {
            system_prompt: definition.system_prompt,
            tools: definition.tools,
            model: definition.model,
            max_turns: definition.max_turns,
            working_dir: scope.working_dir,
            max_context_tokens: definition.max_context_tokens,
            context_policy: definition.context_policy,
            max_tool_duration: scope.max_tool_duration,
            agent_path: scope.agent_path,
            root_session_id: scope.root_session_id,
        }
    }

    pub fn definition(&self) -> AgentDefinition {
        AgentDefinition {
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            model: self.model.clone(),
            max_turns: self.max_turns,
            max_context_tokens: self.max_context_tokens,
            context_policy: Arc::clone(&self.context_policy),
        }
    }

    pub fn scope(&self) -> AgentScope {
        AgentScope {
            working_dir: self.working_dir.clone(),
            max_tool_duration: self.max_tool_duration,
            agent_path: self.agent_path.clone(),
            root_session_id: self.root_session_id,
        }
    }
}

impl From<(AgentDefinition, AgentScope)> for AgentConfig {
    fn from((definition, scope): (AgentDefinition, AgentScope)) -> Self {
        Self::from_parts(definition, scope)
    }
}
