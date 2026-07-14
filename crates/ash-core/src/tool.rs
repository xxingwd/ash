use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
pub use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, Message, ModelId, ProviderConfig, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: std::path::PathBuf,
    pub timeout: Duration,
    pub cancel: CancellationToken,
    pub session_id: SessionId,
    pub root_session_id: SessionId,
    pub agent_path: String,
    pub messages: Vec<Message>,
    pub provider: ProviderConfig,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: ModelId,
    pub max_turns: u32,
    pub max_context_tokens: Option<usize>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

impl From<std::result::Result<String, ToolError>> for ToolResult {
    fn from(result: std::result::Result<String, ToolError>) -> Self {
        match result {
            Ok(output) => Self {
                output,
                is_error: false,
            },
            Err(e) => Self {
                output: e.to_string(),
                is_error: true,
            },
        }
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
    ) -> std::result::Result<String, ToolError>;
}

pub struct FnTool<Args, F, Fut> {
    name: String,
    description: String,
    schema: serde_json::Value,
    execute: F,
    _phantom: std::marker::PhantomData<fn(Args) -> Fut>,
}

pub fn define_tool<Args, F, Fut>(name: &str, description: &str, f: F) -> Arc<dyn Tool>
where
    Args: serde::de::DeserializeOwned + Send + Sync + schemars::JsonSchema + 'static,
    F: Fn(ToolContext, Args) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = std::result::Result<String, ToolError>> + Send + 'static,
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
impl<Args, F, Fut> Tool for FnTool<Args, F, Fut>
where
    Args: serde::de::DeserializeOwned + Send + Sync + 'static,
    F: Fn(ToolContext, Args) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = std::result::Result<String, ToolError>> + Send + 'static,
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
    ) -> std::result::Result<String, ToolError> {
        let typed_args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Execution(format!("invalid arguments: {e}")))?;
        (self.execute)(ctx, typed_args).await
    }
}
