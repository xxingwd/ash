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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryInstruction {
    pub path: std::path::PathBuf,
    pub scope: std::path::PathBuf,
    pub content: String,
}

impl RepositoryInstruction {
    pub fn render(&self) -> String {
        format!(
            "Instructions from {} apply only to {} and descendants:\n{}",
            self.path.display(),
            self.scope.display(),
            self.content
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub text: String,
    pub attachments: Vec<Content>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_instructions: Vec<RepositoryInstruction>,
}

impl ToolOutput {
    pub fn with_attachments(text: impl Into<String>, attachments: Vec<Content>) -> Self {
        Self {
            text: text.into(),
            attachments,
            ..Self::default()
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
            ..Self::default()
        }
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            attachments: Vec::new(),
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn instructions(&self) -> Option<&str> {
        None
    }
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
    async fn prepare(
        &self,
        _ctx: ToolContext,
        _args: &serde_json::Value,
        _observed: &[RepositoryInstruction],
    ) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
    async fn committed(
        &self,
        _ctx: ToolContext,
        _args: &serde_json::Value,
        _output: &ToolOutput,
    ) -> Result<(), ToolError> {
        Ok(())
    }
    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ToolError>;
}

pub fn with_tool_instructions(
    tool: Arc<dyn Tool>,
    instructions: impl Into<String>,
) -> Arc<dyn Tool> {
    Arc::new(InstructedTool {
        tool,
        instructions: instructions.into(),
    })
}

struct InstructedTool {
    tool: Arc<dyn Tool>,
    instructions: String,
}

#[async_trait::async_trait]
impl Tool for InstructedTool {
    async fn prepare(
        &self,
        ctx: ToolContext,
        args: &serde_json::Value,
        observed: &[RepositoryInstruction],
    ) -> Result<Option<ToolOutput>, ToolError> {
        self.tool.prepare(ctx, args, observed).await
    }

    async fn committed(
        &self,
        ctx: ToolContext,
        args: &serde_json::Value,
        output: &ToolOutput,
    ) -> Result<(), ToolError> {
        self.tool.committed(ctx, args, output).await
    }
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn instructions(&self) -> Option<&str> {
        Some(&self.instructions)
    }

    fn timeout(&self) -> ToolTimeout {
        self.tool.timeout()
    }

    fn definition(&self) -> ToolDefinition {
        self.tool.definition()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.tool.parameters_schema()
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.tool.execute(ctx, args).await
    }
}

struct FnTool<Args, F, Fut, Output> {
    name: String,
    description: String,
    schema: serde_json::Value,
    timeout: ToolTimeout,
    execute: F,
    _phantom: std::marker::PhantomData<fn(Args) -> (Fut, Output)>,
}

/// Build a tool from a function. The function must be `Send + Sync + 'static`
/// and take one typed `Args` argument.
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
    validate_definition(name, description)?;
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

fn validate_definition(name: &str, description: &str) -> Result<(), ToolError> {
    if name.trim().is_empty() {
        return Err(ToolError::Execution(
            "tool name cannot be empty".to_string(),
        ));
    }
    if description.trim().is_empty() {
        return Err(ToolError::Execution(
            "tool description cannot be empty".to_string(),
        ));
    }
    Ok(())
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

    #[test]
    fn define_tool_rejects_empty_metadata() {
        let empty_name = define_tool(" ", "description", |_, _: ()| async {
            Ok::<_, ToolError>("ok")
        });
        let empty_description =
            define_tool("name", "\t", |_, _: ()| async { Ok::<_, ToolError>("ok") });

        assert!(matches!(
            empty_name,
            Err(ToolError::Execution(message)) if message == "tool name cannot be empty"
        ));
        assert!(matches!(
            empty_description,
            Err(ToolError::Execution(message)) if message == "tool description cannot be empty"
        ));
    }

    #[tokio::test]
    async fn instructions_wrapper_preserves_tool_contract_and_execution() {
        let tool = define_tool_with_timeout(
            "sample",
            "sample tool",
            ToolTimeout::Disabled,
            |_, fail: bool| async move {
                if fail {
                    Err(ToolError::Execution("failure".into()))
                } else {
                    Ok("output")
                }
            },
        )
        .unwrap();
        let definition = serde_json::to_value(tool.definition()).unwrap();
        let wrapped = with_tool_instructions(tool, "module rules");
        assert_eq!(wrapped.instructions(), Some("module rules"));
        assert_eq!(wrapped.timeout(), ToolTimeout::Disabled);
        assert_eq!(
            serde_json::to_value(wrapped.definition()).unwrap(),
            definition
        );
        let context = ToolContext {
            identity: SessionIdentity::root(SessionId::new()),
            cancellation: CancellationToken::new(),
            deadline: None,
        };
        assert_eq!(
            wrapped
                .execute(context.clone(), false.into())
                .await
                .unwrap()
                .text,
            "output"
        );
        assert!(matches!(wrapped.execute(context, true.into()).await,
            Err(ToolError::Execution(message)) if message == "failure"));
    }
}
