use std::sync::Arc;

use ash_core::{Tool, ToolContext, ToolError, ToolOutput};
use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::serve_client;
use rmcp::service::{Peer, RoleClient};
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

pub struct McpToolAdapter {
    name: String,
    description: String,
    schema: serde_json::Value,
    peer: Peer<RoleClient>,
    tool_name: String,
}

impl McpToolAdapter {
    pub fn new(
        name: String,
        description: String,
        schema: serde_json::Value,
        peer: Peer<RoleClient>,
        tool_name: String,
    ) -> Self {
        Self {
            name,
            description,
            schema,
            peer,
            tool_name,
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
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let arguments: Option<JsonObject> = serde_json::from_value(args)
            .map_err(|e| ToolError::Execution(format!("invalid args: {e}")))?;

        let mut request = CallToolRequestParams::new(self.tool_name.clone());
        if let Some(arguments) = arguments {
            request = request.with_arguments(arguments);
        }

        let result = self
            .peer
            .call_tool(request)
            .await
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

pub struct McpManager {
    peers: Vec<(McpServerConfig, Peer<RoleClient>)>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        Self { peers: Vec::new() }
    }

    pub async fn connect(&mut self, config: McpServerConfig) -> Result<(), ash_core::AshError> {
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args);

        if let Some(env) = &config.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| ash_core::AshError::Config(format!("MCP spawn failed: {e}")))?;

        let running = serve_client((), transport)
            .await
            .map_err(|e| ash_core::AshError::Config(format!("MCP init failed: {e}")))?;

        let peer = running.peer().clone();
        self.peers.push((config, peer));
        Ok(())
    }

    pub async fn discover_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = Vec::new();

        for (config, peer) in &self.peers {
            let remote_tools = match peer.list_all_tools().await {
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
                    peer.clone(),
                    tool_info.name.to_string(),
                )) as Arc<dyn Tool>);
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
