use serde::{Deserialize, Serialize};
use std::sync::Arc;
pub use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, Content, Message, SessionId, SessionIdentity, TurnId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

#[derive(Clone)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub cancellation: CancellationToken,
    pub deadline: std::time::Instant,
    /// Read-only snapshot of the calling session, for session-aware tools.
    pub session: SessionToolContext,
}

/// Read-only invocation state used by session-aware tools.
///
/// Model credentials, model clients, prompts, and the complete tool registry are
/// intentionally not exposed here. Product-specific tools must capture those
/// capabilities when they are constructed.
#[derive(Clone)]
pub struct SessionToolContext {
    pub identity: SessionIdentity,
    pub messages: Vec<Message>,
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
        Self {
            text: text.to_string(),
            attachments: Vec::new(),
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
    ) -> std::result::Result<ToolOutput, ToolError>;
}

pub struct FnTool<Args, F, Fut, Output> {
    name: String,
    description: String,
    schema: serde_json::Value,
    execute: F,
    _phantom: std::marker::PhantomData<fn(Args) -> (Fut, Output)>,
}

/// Build a tool from a function. The function must be `Send + Sync + 'static`
/// and take one `Args` argument (see [`FnTool`]).
///
/// # Errors
///
/// Returns [`ToolError`] when `name` or `description` are empty or the
/// function's signature cannot be turned into a JSON schema.
pub fn define_tool<Args, F, Fut, Output>(
    name: &str,
    description: &str,
    f: F,
) -> Result<Arc<dyn Tool>, ToolError>
where
    Args: serde::de::DeserializeOwned + Send + Sync + schemars::JsonSchema + 'static,
    F: Fn(ToolContext, Args) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = std::result::Result<Output, ToolError>> + Send + 'static,
    Output: Into<ToolOutput> + Send + Sync + 'static,
{
    let schema = schemars::schema_for!(Args);
    let schema_value = serde_json::to_value(schema).map_err(|error| {
        ToolError::Execution(format!(
            "cannot serialize schema for tool '{name}': {error}"
        ))
    })?;

    Ok(Arc::new(FnTool {
        name: name.to_string(),
        description: description.to_string(),
        schema: schema_value,
        execute: f,
        _phantom: std::marker::PhantomData,
    }))
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
