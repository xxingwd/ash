use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
pub use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, Content, Message, ModelId, ProviderConfig, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

#[derive(Clone)]
pub struct ToolContext {
    /// Workspace boundary shared by every built-in tool.
    pub working_dir: std::path::PathBuf,
    /// Framework safety cap; a tool may impose a stricter per-call limit.
    pub max_duration: Duration,
    /// Context needed only by agent-aware extension tools.
    pub agent: AgentToolContext,
}

/// Additional state exposed only to tools that manage child agents.
#[derive(Clone)]
pub struct AgentToolContext {
    pub root_session_id: SessionId,
    pub agent_path: String,
    pub messages: Vec<Message>,
    pub provider: ProviderConfig,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub max_input_tokens: usize,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    pub attachments: Vec<Content>,
}

impl ToolOutput {
    pub fn with_attachments(text: impl Into<String>, attachments: Vec<Content>) -> Self {
        Self {
            text: text.into(),
            attachments,
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
        }
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters_schema: self.parameters_schema(),
        }
    }
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ToolError>;
}

pub struct FnTool<Args, F, Fut, Output> {
    name: String,
    description: String,
    schema: serde_json::Value,
    execute: F,
    _phantom: std::marker::PhantomData<fn(Args) -> (Fut, Output)>,
}

pub fn define_tool<Args, F, Fut, Output>(name: &str, description: &str, f: F) -> Arc<dyn Tool>
where
    Args: serde::de::DeserializeOwned + Send + Sync + schemars::JsonSchema + 'static,
    F: Fn(ToolContext, Args) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = std::result::Result<Output, ToolError>> + Send + 'static,
    Output: Into<ToolOutput> + Send + Sync + 'static,
{
    let schema = schemars::schema_for!(Args);
    let schema_value = serde_json::to_value(schema).unwrap_or_default();

    Arc::new(FnTool {
        name: name.to_string(),
        description: description.to_string(),
        schema: schema_value,
        execute: f,
        _phantom: std::marker::PhantomData,
    })
}

#[async_trait::async_trait]
impl<Args, F, Fut, Output> Tool for FnTool<Args, F, Fut, Output>
where
    Args: serde::de::DeserializeOwned + Send + Sync + 'static,
    F: Fn(ToolContext, Args) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = std::result::Result<Output, ToolError>> + Send + 'static,
    Output: Into<ToolOutput> + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let typed_args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Execution(format!("invalid arguments: {e}")))?;
        (self.execute)(ctx, typed_args).await.map(Into::into)
    }
}
