//! Run-local MCP servers hosted by an SDK client, without subprocess/config files.
use super::tool_executor::McpToolDefinition;
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

pub trait Host: Send + Sync {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        message: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
    fn close(&self);
}

pub fn validate_names(value: &Value) -> Result<Vec<String>> {
    let names = value.as_array().context("sdkMcpServers must be an array")?;
    ensure!(names.len() <= 64, "SDK MCP server limit is 64");
    let mut unique = HashSet::new();
    names
        .iter()
        .map(|name| {
            let name = name
                .as_str()
                .context("SDK MCP server names must be strings")?;
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                "Invalid SDK MCP server name"
            );
            ensure!(name != "ide", "SDK MCP server name ide is reserved");
            ensure!(unique.insert(name), "Duplicate SDK MCP server: {name}");
            Ok(name.to_owned())
        })
        .collect()
}

struct Tool {
    definition: McpToolDefinition,
    original: String,
    output_schema: Option<Value>,
}
struct Server {
    name: String,
    tools: Vec<Tool>,
    resources: bool,
}
struct Run {
    names: Vec<String>,
    host: Arc<dyn Host>,
    ready: tokio::sync::OnceCell<Vec<Server>>,
    closed: AtomicBool,
}
impl Run {
    async fn exchange(&self, server: &str, message: Value) -> Result<Value> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "SDK MCP run has ended"
        );
        let result =
            tokio::time::timeout(Duration::from_secs(60), self.host.exchange(server, message))
                .await
                .context("SDK MCP request timed out")??;
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "SDK MCP run has ended"
        );
        ensure!(
            serde_json::to_vec(&result)?.len() <= 8 * 1024 * 1024,
            "SDK MCP response exceeds 8 MiB"
        );
        Ok(result)
    }
    async fn request(&self, server: &str, method: &str, params: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut response = self
            .exchange(
                server,
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            )
            .await?;
        for _ in 0..64 {
            if response.get("method").is_none() {
                break;
            }
            ensure!(
                response.get("id").is_some(),
                "Unexpected SDK MCP notification response"
            );
            let reply = super::elicitation::reply(server, &response).await;
            response = self.exchange(server, reply).await?;
        }
        ensure!(
            response.is_object()
                && response["jsonrpc"] == "2.0"
                && response["id"] == id
                && response.get("method").is_none(),
            "SDK MCP response identity mismatch for {server}"
        );
        ensure!(
            response.get("result").is_some() != response.get("error").is_some(),
            "SDK MCP response requires exactly one result or error"
        );
        if let Some(error) = response.get("error") {
            ensure!(
                error["code"].is_i64() && error["message"].is_string(),
                "Malformed SDK MCP error response"
            );
            anyhow::bail!("SDK MCP {server} {method} failed: {error}");
        }
        ensure!(
            response["result"].is_object(),
            "SDK MCP {method} result must be an object"
        );
        Ok(response["result"].clone())
    }
    async fn servers(&self) -> Result<&Vec<Server>> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "SDK MCP run has ended"
        );
        self.ready.get_or_try_init(|| async {
            let mut servers = Vec::new();
            let mut all_names = HashSet::new();
            let mut schema_bytes = 0usize;
            for name in &self.names {
                let initialized = self.request(name,"initialize",json!({"protocolVersion":"2025-06-18","capabilities":{"elicitation":{}},"clientInfo":{"name":"claude-code-rust","version":env!("CARGO_PKG_VERSION")}})).await?;
                ensure!(matches!(initialized["protocolVersion"].as_str(),Some("2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25")), "Unsupported SDK MCP protocol version");
                ensure!(initialized["capabilities"].is_object(), "SDK MCP initialize requires capabilities");
                ensure!(initialized["serverInfo"]["name"].is_string() && initialized["serverInfo"]["version"].is_string(), "SDK MCP initialize requires serverInfo");
                for capability in ["tools", "resources"] {
                    ensure!(initialized["capabilities"].get(capability).is_none_or(Value::is_object), "Invalid SDK MCP capability: {capability}");
                }
                self.exchange(name,json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
                let mut tools = Vec::new();
                if initialized["capabilities"].get("tools").is_some() {
                    let mut cursor = None;
                    let mut cursors = HashSet::new();
                    loop {
                        let params = cursor.as_ref().map_or(json!({}), |value| json!({"cursor":value}));
                        let listed = self.request(name,"tools/list",params).await?;
                        for tool in listed["tools"].as_array().context("SDK MCP tools/list requires tools array")? {
                            schema_bytes += serde_json::to_vec(tool)?.len();
                            ensure!(schema_bytes <= 8 * 1024 * 1024, "SDK MCP tool catalog exceeds 8 MiB");
                            let original = tool["name"].as_str().context("MCP tool name missing")?;
                            ensure!(!original.is_empty() && original.len() <= 128 && original.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c)), "Invalid MCP tool name");
                            let full_name = format!("mcp__{name}__{original}");
                            ensure!(all_names.insert(full_name.clone()), "Duplicate SDK MCP tool: {full_name}");
                            let schema = tool.get("inputSchema").context("MCP tool inputSchema missing")?;
                            crate::structured_output::validate_schema(schema).context("Invalid MCP input schema")?;
                            ensure!(schema["type"] == "object", "MCP inputSchema must describe an object");
                            ensure!(tool.get("description").is_none_or(Value::is_string), "MCP description must be text");
                            let output_schema = tool.get("outputSchema").cloned();
                            if let Some(schema) = &output_schema { crate::structured_output::validate_schema(schema).context("Invalid MCP output schema")?; }
                            tools.push(Tool { original:original.into(), output_schema, definition:McpToolDefinition { name:full_name, description:tool["description"].as_str().unwrap_or("").into(), input_schema:schema.clone(), annotations:tool.get("annotations").cloned(), server_name:name.clone() } });
                            ensure!(all_names.len() <= 1024, "SDK MCP tool limit is 1024");
                        }
                        cursor = next_cursor(&listed)?;
                        match &cursor {
                            None => break,
                            Some(value) => ensure!(cursors.insert(value.clone()) && cursors.len() <= 32, "MCP pagination did not terminate"),
                        }
                    }
                }
                servers.push(Server { name:name.clone(), tools, resources:initialized["capabilities"].get("resources").is_some() });
            }
            Ok(servers)
        }).await
    }
}

#[derive(Default)]
pub struct Runtime {
    run: RwLock<Option<Arc<Run>>>,
}
impl Runtime {
    pub fn install(&self, names: Vec<String>, host: Arc<dyn Host>) -> Result<()> {
        validate_names(&json!(names))?;
        let mut run = self.run.write().unwrap_or_else(|e| e.into_inner());
        ensure!(run.is_none(), "SDK MCP servers are already initialized");
        *run = Some(Arc::new(Run {
            names,
            host,
            ready: tokio::sync::OnceCell::new(),
            closed: AtomicBool::new(false),
        }));
        Ok(())
    }
    fn snapshot(&self) -> Option<Arc<Run>> {
        self.run.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn names(&self) -> Vec<String> {
        self.snapshot()
            .map(|run| run.names.clone())
            .unwrap_or_default()
    }
    pub fn clear(&self) {
        if let Some(run) = self.run.write().unwrap_or_else(|e| e.into_inner()).take() {
            run.closed.store(true, Ordering::Release);
            run.host.close();
        }
    }
    pub async fn tools(&self) -> Result<Vec<McpToolDefinition>> {
        let Some(run) = self.snapshot() else {
            return Ok(Vec::new());
        };
        Ok(run
            .servers()
            .await?
            .iter()
            .flat_map(|s| s.tools.iter().map(|t| t.definition.clone()))
            .collect())
    }
    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value> {
        let run = self.snapshot().context("SDK MCP run is unavailable")?;
        for server in run.servers().await? {
            if let Some(tool) = server
                .tools
                .iter()
                .find(|tool| tool.definition.name == name)
            {
                crate::structured_output::validate_result(
                    &tool.definition.input_schema,
                    &arguments.to_string(),
                )
                .context("Invalid MCP tool arguments")?;
                let result = run
                    .request(
                        &server.name,
                        "tools/call",
                        json!({"name":tool.original,"arguments":arguments}),
                    )
                    .await?;
                validate_content(&result["content"])?;
                if let Some(flag) = result.get("isError") {
                    ensure!(flag.is_boolean(), "MCP isError must be boolean");
                }
                if let Some(value) = result.get("structuredContent") {
                    ensure!(value.is_object(), "MCP structuredContent must be an object");
                }
                if result["isError"] != true {
                    if let Some(schema) = &tool.output_schema {
                        let value = result
                            .get("structuredContent")
                            .context("MCP output schema requires structuredContent")?;
                        crate::structured_output::validate_result(schema, &value.to_string())
                            .context("MCP structured output does not match outputSchema")?;
                    }
                }
                return Ok(result);
            }
        }
        Err(anyhow!("SDK MCP tool is unavailable: {name}"))
    }
    pub async fn resource(&self, server: &str, uri: Option<&str>) -> Result<Value> {
        let run = self.snapshot().context("SDK MCP run is unavailable")?;
        let current = run
            .servers()
            .await?
            .iter()
            .find(|entry| entry.name == server)
            .context("Unknown SDK MCP resource server")?;
        ensure!(
            current.resources,
            "SDK MCP server does not advertise resources"
        );
        match uri {
            Some(uri) => {
                ensure!(
                    !uri.is_empty() && uri.len() <= 8192,
                    "Invalid MCP resource URI"
                );
                let result = run
                    .request(server, "resources/read", json!({"uri":uri}))
                    .await?;
                let contents = result["contents"]
                    .as_array()
                    .context("MCP resources/read requires contents array")?;
                ensure!(
                    contents.len() <= 1024,
                    "MCP resource contents limit exceeded"
                );
                for content in contents {
                    validate_resource(content)?;
                }
                Ok(result)
            }
            None => {
                let mut resources = Vec::new();
                let mut cursor = None;
                let mut cursors = HashSet::new();
                let mut uris = HashSet::new();
                let mut total_bytes = 0usize;
                loop {
                    let params = cursor
                        .as_ref()
                        .map_or(json!({}), |value| json!({"cursor":value}));
                    let result = run.request(server, "resources/list", params).await?;
                    for resource in result["resources"]
                        .as_array()
                        .context("MCP resources/list requires resources array")?
                    {
                        total_bytes += serde_json::to_vec(resource)?.len();
                        ensure!(
                            total_bytes <= 8 * 1024 * 1024,
                            "MCP resource catalog exceeds 8 MiB"
                        );
                        let uri = resource["uri"]
                            .as_str()
                            .context("MCP resource URI missing")?;
                        ensure!(
                            !uri.is_empty() && uri.len() <= 8192 && uris.insert(uri.to_owned()),
                            "Invalid or duplicate MCP resource URI"
                        );
                        ensure!(
                            resource["name"].is_string()
                                && resource.get("mimeType").is_none_or(Value::is_string),
                            "Invalid MCP resource metadata"
                        );
                        resources.push(resource.clone());
                        ensure!(resources.len() <= 1024, "MCP resource limit exceeded");
                    }
                    cursor = next_cursor(&result)?;
                    match &cursor {
                        None => break,
                        Some(value) => ensure!(
                            cursors.insert(value.clone()) && cursors.len() <= 32,
                            "MCP resource pagination did not terminate"
                        ),
                    }
                }
                Ok(json!({"resources":resources}))
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.clear();
    }
}

fn next_cursor(result: &Value) -> Result<Option<String>> {
    match result.get("nextCursor") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let value = value.as_str().context("MCP cursor must be a string")?;
            ensure!(
                !value.is_empty() && value.len() <= 8192,
                "Invalid MCP pagination cursor"
            );
            Ok(Some(value.to_owned()))
        }
    }
}

fn validate_resource(value: &Value) -> Result<()> {
    ensure!(
        value["uri"]
            .as_str()
            .is_some_and(|uri| !uri.is_empty() && uri.len() <= 8192),
        "MCP resource content requires URI"
    );
    ensure!(
        value.get("mimeType").is_none_or(Value::is_string),
        "MCP resource mimeType must be text"
    );
    ensure!(
        value.get("text").is_some() != value.get("blob").is_some(),
        "MCP resource requires exactly one text or blob"
    );
    if let Some(text) = value.get("text") {
        ensure!(text.is_string(), "MCP resource text must be text");
    }
    if let Some(blob) = value.get("blob") {
        validate_base64(blob)?;
    }
    Ok(())
}

fn validate_base64(value: &Value) -> Result<()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(
            value
                .as_str()
                .context("MCP binary data must be base64 text")?,
        )
        .context("Invalid MCP base64 data")?;
    Ok(())
}

fn validate_content(value: &Value) -> Result<()> {
    let content = value
        .as_array()
        .context("MCP tools/call result requires content array")?;
    ensure!(content.len() <= 1024, "MCP content block limit exceeded");
    for block in content {
        match block["type"].as_str() {
            Some("text") => ensure!(block["text"].is_string(), "MCP text block requires text"),
            Some("image" | "audio") => {
                ensure!(
                    block["mimeType"].is_string(),
                    "MCP binary block requires mimeType"
                );
                validate_base64(&block["data"])?;
            }
            Some("resource") => validate_resource(&block["resource"])?,
            Some("resource_link") => ensure!(
                block["uri"].is_string() && block["name"].is_string(),
                "MCP resource_link requires URI and name"
            ),
            _ => anyhow::bail!("Unsupported MCP content block type"),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "sdk_tests.rs"]
mod tests;
