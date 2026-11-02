use crate::mcp::{McpConnector, McpServerManager};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
    pub server_name: String,
}

impl McpToolDefinition {
    pub fn is_read_only(&self) -> bool {
        // Missing annotations default to read-only; explicit invalid values do not.
        match self
            .annotations
            .as_ref()
            .and_then(|value| value.get("readOnlyHint"))
        {
            None => true,
            Some(value) => value.as_bool() == Some(true),
        }
    }
}

pub struct McpToolRegistry {
    manager: Arc<McpServerManager>,
    ide: Option<Arc<crate::services::ide::IdeClient>>,
    sdk: Option<Arc<super::sdk::Runtime>>,
    #[allow(dead_code)]
    active_connectors: Arc<Mutex<HashMap<String, Arc<Mutex<McpConnector>>>>>,
}

pub(crate) struct McpExecution {
    pub value: serde_json::Value,
    pub is_tool_call: bool,
}

impl McpExecution {
    fn tool(value: serde_json::Value) -> Self { Self { value, is_tool_call: true } }
    fn resource(value: serde_json::Value) -> Self { Self { value, is_tool_call: false } }
}

/// Upstream `normalizeNameForMCP`: characters outside `[A-Za-z0-9_-]` become `_`.
fn normalize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') { c } else { '_' })
        .collect()
}

/// Configured servers expose tools as `mcp__<server>__<tool>` so permission
/// rules and hook matchers can target a server without colliding with builtins.
pub fn qualified_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", normalize_name(server), normalize_name(tool))
}

impl McpToolRegistry {
    pub fn new(manager: Arc<McpServerManager>) -> Self {
        Self {
            manager,
            ide: None,
            sdk: None,
            active_connectors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_ide(mut self, ide: Arc<crate::services::ide::IdeClient>) -> Self {
        self.ide = Some(ide);
        self
    }

    pub fn with_sdk(mut self, sdk: Arc<super::sdk::Runtime>) -> Self {
        self.sdk = Some(sdk);
        self
    }

    pub async fn get_all_tools(&self) -> Result<Vec<McpToolDefinition>> {
        let manager = &self.manager;
        let servers = manager.list_servers_full().await;
        let mut all_tools = vec![
            McpToolDefinition {
                name: "ListMcpResources".into(),
                description:
                    "List live MCP resources from an enabled running server; server_id is required."
                        .into(),
                input_schema: serde_json::json!({"type":"object","properties":{"server_id":{"type":"string"}},"required":["server_id"]}),
                annotations: None,
                server_name: "MCP resources".into(),
            },
            McpToolDefinition {
                name: "ReadMcpResource".into(),
                description: "Read actual MCP resource contents from an enabled running server."
                    .into(),
                input_schema: serde_json::json!({"type":"object","properties":{"server_id":{"type":"string"},"uri":{"type":"string"}},"required":["server_id","uri"]}),
                annotations: None,
                server_name: "MCP resources".into(),
            },
        ];

        for server in servers {
            if !server.config.enabled || !server.status.running {
                continue;
            }

            if let Some(connector_arc) = &server.connector {
                let connector = connector_arc.lock().await;
                let tools = connector.list_tools().await.unwrap_or_default();

                for tool in tools {
                    all_tools.push(McpToolDefinition {
                        name: qualified_tool_name(&server.config.id, &tool.name),
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                        annotations: tool.annotations.clone(),
                        server_name: server.config.name.clone(),
                    });
                }
            }
        }

        if let Some(ide) = &self.ide { all_tools.extend(ide.tools().await); }
        if let Some(sdk) = &self.sdk {
            let names = sdk.names();
            if !names.is_empty() {
                let ids = names.iter().map(|name|format!("sdk:{name}")).collect::<Vec<_>>().join(", ");
                for tool in all_tools.iter_mut().filter(|tool| matches!(tool.name.as_str(), "ListMcpResources" | "ReadMcpResource")) {
                    tool.description.push_str(&format!(" Run-local SDK server IDs: {ids}."));
                }
            }
            all_tools.extend(sdk.tools().await?);
        }
        let mut names = std::collections::HashSet::new();
        for tool in &all_tools {
            anyhow::ensure!(names.insert(&tool.name), "Conflicting MCP tool name: {}", tool.name);
        }
        Ok(all_tools)
    }

    pub async fn execute_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        Ok(self.execute_with_provenance(name, arguments).await?.value)
    }

    pub(crate) async fn execute_with_provenance(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpExecution> {
        if name.starts_with("mcp__ide__") {
            return self.ide.as_ref().ok_or_else(||anyhow::anyhow!("IDE is not connected"))?.call_tool(name, arguments).await.map(McpExecution::tool);
        }
        let manager = &self.manager;
        let servers = manager.list_servers_full().await;

        if let Some(sdk) = &self.sdk {
            if sdk.tools().await?.iter().any(|tool| tool.name == name) {
                return sdk.call(name, arguments).await.map(McpExecution::tool);
            }
        }

        if matches!(name, "ListMcpResources" | "ReadMcpResource") {
            let id = arguments["server_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("server_id is required"))?;
            if let Some(server_name) = id.strip_prefix("sdk:") {
                let sdk = self.sdk.as_ref().ok_or_else(|| anyhow::anyhow!("SDK MCP run unavailable"))?;
                anyhow::ensure!(!server_name.is_empty(), "SDK MCP server name missing");
                let uri = if name == "ReadMcpResource" {
                    Some(arguments["uri"].as_str().ok_or_else(|| anyhow::anyhow!("uri is required"))?)
                } else { None };
                return sdk.resource(server_name, uri).await.map(McpExecution::resource);
            }
            let server = servers
                .iter()
                .find(|server| server.config.id == id)
                .ok_or_else(|| anyhow::anyhow!("MCP server not found: {id}"))?;
            anyhow::ensure!(
                server.config.enabled && server.status.running,
                "MCP server is disabled or stopped"
            );
            let connector = server
                .connector
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("MCP connector unavailable"))?
                .lock()
                .await;
            return if name == "ListMcpResources" {
                Ok(McpExecution::resource(serde_json::to_value(connector.list_resources().await?)?))
            } else {
                let uri = arguments["uri"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("uri is required"))?;
                Ok(McpExecution::resource(serde_json::to_value(
                    connector.read_resource(uri, None).await?,
                )?))
            };
        }

        for server in servers {
            if !server.config.enabled || !server.status.running {
                continue;
            }

            if let Some(connector_arc) = &server.connector {
                let connector = connector_arc.lock().await;
                let tools = connector.list_tools().await.unwrap_or_default();

                if let Some(tool) = tools.iter().find(|t| qualified_tool_name(&server.config.id, &t.name) == name) {
                    return connector.call_tool(&tool.name, arguments).await.map(McpExecution::tool);
                }
            }
        }

        Err(anyhow::anyhow!(
            "MCP tool '{}' not found in any running server",
            name
        ))
    }

    pub async fn is_mcp_tool(&self, name: &str) -> bool {
        if name.starts_with("mcp__ide__") { return true; }
        if matches!(name, "ListMcpResources" | "ReadMcpResource") {
            return true;
        }
        match self.get_all_tools().await {
            Ok(tools) => tools.iter().any(|t| t.name == name),
            Err(_) => false,
        }
    }
}
