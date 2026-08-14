use std::{future::Future, sync::Arc, time::Instant};

use ash_core::{Tool, ToolContext, ToolError, ToolOutput};
use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::serve_client;
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
}

struct McpToolAdapter {
    name: String,
    description: String,
    schema: serde_json::Value,
    connection: Arc<McpConnection>,
}

impl McpToolAdapter {
    const fn new(
        name: String,
        description: String,
        schema: serde_json::Value,
        connection: Arc<McpConnection>,
    ) -> Self {
        Self {
            name,
            description,
            schema,
            connection,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpToolAdapter {
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
    ) -> Result<ToolOutput, ToolError> {
        let arguments: Option<JsonObject> = serde_json::from_value(args)
            .map_err(|e| ToolError::Execution(format!("invalid args: {e}")))?;

        let mut request = CallToolRequestParams::new(self.name.clone());
        if let Some(arguments) = arguments {
            request = request.with_arguments(arguments);
        }

        let result = await_tool_call(
            &ctx.cancellation,
            ctx.deadline,
            self.connection.peer.call_tool(request),
        )
        .await?
        .map_err(|e| ToolError::Execution(format!("MCP call failed: {e}")))?;

        let mut output = String::new();
        for item in &result.content {
            if let Some(text_block) = item.as_text() {
                output.push_str(&text_block.text);
            } else {
                output.push_str("[non-text content]");
            }
        }

        if result.is_error.unwrap_or(false) {
            Err(ToolError::Execution(output))
        } else {
            Ok(output.into())
        }
    }
}

async fn await_tool_call<T>(
    cancellation: &ash_core::CancellationToken,
    deadline: Instant,
    call: impl Future<Output = T>,
) -> Result<T, ToolError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Cancelled),
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            Err(ToolError::DeadlineExceeded)
        }
        result = call => Ok(result),
    }
}

struct McpConnection {
    peer: Peer<RoleClient>,
    _service: RunningService<RoleClient, ()>,
}

impl McpConnection {
    fn new(service: RunningService<RoleClient, ()>) -> Self {
        Self {
            peer: service.peer().clone(),
            _service: service,
        }
    }
}

pub struct McpManager {
    connections: Vec<(McpServerConfig, Arc<McpConnection>)>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            connections: Vec::new(),
        }
    }

    /// Connect to an MCP server and keep the connection in this pool.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when the server process cannot be started or the
    /// handshake fails.
    pub async fn connect(&mut self, config: McpServerConfig) -> Result<(), ash_core::AshError> {
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args);

        if let Some(env) = &config.env {
            cmd.envs(env);
        }

        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| ash_core::AshError::Config(format!("MCP spawn failed: {e}")))?;

        let running = serve_client((), transport)
            .await
            .map_err(|e| ash_core::AshError::Config(format!("MCP init failed: {e}")))?;

        self.connections
            .push((config, Arc::new(McpConnection::new(running))));
        Ok(())
    }

    pub async fn discover_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

        for (config, connection) in &self.connections {
            let remote_tools = match connection.peer.list_all_tools().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("failed to list tools from {}: {e}", config.name);
                    continue;
                }
            };

            for tool_info in remote_tools {
                let schema = tool_info.schema_as_json_value();

                debug!(
                    "discovered MCP tool: {} from {}",
                    tool_info.name, config.name
                );

                tools.push(Arc::new(McpToolAdapter::new(
                    tool_info.name.to_string(),
                    tool_info.description.unwrap_or_default().to_string(),
                    schema,
                    Arc::clone(connection),
                )));
            }
        }

        tools
    }
}

pub async fn load_mcp_tools(configs: &[McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    let mut manager = McpManager::new();

    for config in configs {
        if let Err(e) = manager.connect(config.clone()).await {
            warn!("failed to connect MCP server {}: {e}", config.name);
        }
    }

    manager.discover_tools().await
}

#[cfg(test)]
mod tests {
    use ash_core::{
        CancellationToken, SessionId, SessionIdentity, SessionToolContext, ToolContext, TurnId,
    };
    use rmcp::{
        model::{
            CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
            ServerCapabilities, ServerInfo, Tool as McpTool,
        },
        service::{RequestContext, RoleServer},
        ServerHandler, ServiceExt,
    };

    use super::*;

    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, rmcp::ErrorData> {
            Ok(ListToolsResult {
                tools: vec![McpTool::new(
                    "echo",
                    "Echo a fixed response",
                    JsonObject::new(),
                )],
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            Ok(CallToolResult::success(vec![ContentBlock::text("connected")]).into())
        }
    }

    fn tool_context() -> ToolContext {
        ToolContext {
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
            session: SessionToolContext {
                identity: SessionIdentity::root(SessionId::new()),
                messages: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn tool_calls_observe_cancellation() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = await_tool_call(
            &cancellation,
            Instant::now() + std::time::Duration::from_secs(1),
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(result, Err(ToolError::Cancelled)));
    }

    #[tokio::test]
    async fn tool_calls_observe_deadlines() {
        let result = await_tool_call(
            &CancellationToken::new(),
            Instant::now(),
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(result, Err(ToolError::DeadlineExceeded)));
    }

    #[tokio::test]
    async fn discovered_tools_keep_the_mcp_connection_alive() {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let service = EchoServer.serve(server_transport).await.unwrap();
            service.waiting().await.unwrap();
        });
        let running = serve_client((), client_transport).await.unwrap();
        let manager = McpManager {
            connections: vec![(
                McpServerConfig {
                    name: "test".to_string(),
                    command: String::new(),
                    args: Vec::new(),
                    env: None,
                },
                Arc::new(McpConnection::new(running)),
            )],
        };

        let tools = manager.discover_tools().await;
        drop(manager);
        let output = tools[0]
            .execute(tool_context(), serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(output.text, "connected");
        drop(tools);
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}
