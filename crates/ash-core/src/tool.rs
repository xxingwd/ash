use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc};
pub use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, Content, SessionIdentity};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolTimeout {
    /// Use the timeout configured for the calling session.
    #[default]
    Session,
    /// Run until completion or cancellation.
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

#[derive(Clone)]
pub struct ToolContext {
    pub identity: SessionIdentity,
    pub cancellation: CancellationToken,
    pub deadline: Option<std::time::Instant>,
}

impl ToolContext {
    /// Return the deadline supplied to a timeout-bound tool.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the tool disabled the session timeout.
    pub fn require_deadline(&self) -> Result<std::time::Instant, ToolError> {
        self.deadline.ok_or_else(|| {
            ToolError::Execution("tool requires a bounded execution deadline".to_string())
        })
    }

    /// Await one tool operation under this context's cancellation and deadline.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Cancelled`] or [`ToolError::DeadlineExceeded`]
    /// before the operation completes.
    pub async fn run<T>(&self, operation: impl Future<Output = T>) -> Result<T, ToolError> {
        tokio::pin!(operation);
        match self.deadline {
            Some(deadline) => tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(ToolError::Cancelled),
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    Err(ToolError::DeadlineExceeded)
                }
                result = &mut operation => Ok(result),
            },
            None => tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(ToolError::Cancelled),
                result = &mut operation => Ok(result),
            },
        }
    }
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
    fn timeout(&self) -> ToolTimeout {
        ToolTimeout::Session
    }
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
    timeout: ToolTimeout,
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
    define_tool_with_timeout(name, description, ToolTimeout::Session, f)
}

/// Build a tool with an explicit execution timeout policy.
///
/// # Errors
///
/// Returns [`ToolError`] under the same conditions as [`define_tool`].
pub fn define_tool_with_timeout<Args, F, Fut, Output>(
    name: &str,
    description: &str,
    timeout: ToolTimeout,
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
        timeout,
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

    fn timeout(&self) -> ToolTimeout {
        self.timeout
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    #[tokio::test]
    async fn cancellation_wins_when_every_tool_branch_is_ready() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let context = ToolContext {
            identity: SessionIdentity::root(SessionId::new()),
            cancellation,
            deadline: Some(std::time::Instant::now()),
        };

        let result = context.run(std::future::ready(())).await;

        assert!(matches!(result, Err(ToolError::Cancelled)));
    }
}
