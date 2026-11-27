//! Read-only, credential-free status snapshots. No model or tool is invoked.
use crate::mcp::McpServerManager;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStatus {
    pub name: String,
    pub state: String,
    pub tools: usize,
    pub pid: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub mcp_initialized: bool,
    pub mcp_configuration: String,
    pub mcp: Vec<ToolStatus>,
    pub lsp_state: String,
    pub lsp_configured: usize,
    pub lsp_pooled: Vec<ToolStatus>,
    pub native_tools: usize,
}
#[derive(Clone)]
pub struct RuntimeSource {
    pub mcp: Option<Arc<McpServerManager>>,
    pub mcp_ready: Arc<tokio::sync::OnceCell<()>>,
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
}
pub fn label(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(180)
        .collect()
}
impl RuntimeSource {
    pub async fn snapshot(&self) -> RuntimeStatus {
        let live = self.mcp.as_ref().filter(|_| self.mcp_ready.get().is_some());
        let (configuration, mut mcp) = if let Some(manager) = live {
            let servers = manager.list_servers().await;
            (
                "loaded".to_owned(),
                servers
                    .into_iter()
                    .map(|server| ToolStatus {
                        name: label(&server.name),
                        state: if !server.enabled {
                            "disabled"
                        } else if server.error.is_some() {
                            "failed"
                        } else if server.running {
                            "manager_reports_running"
                        } else {
                            "stopped"
                        }
                        .into(),
                        tools: server.tools_count,
                        pid: server.pid,
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            configured_mcp(&self.data_dir.join("mcp-servers.json"))
        };
        mcp.sort_by(|a, b| a.name.cmp(&b.name));
        let mut report = RuntimeStatus {
            mcp_initialized: live.is_some(),
            mcp_configuration: configuration,
            mcp,
            lsp_state: "unavailable".into(),
            lsp_configured: 0,
            lsp_pooled: Vec::new(),
            native_tools: crate::tools::get_tool_definitions().len(),
        };
        if let Ok(Ok(value)) = tokio::time::timeout(
            Duration::from_millis(500),
            crate::tools::lsp_status(&self.workspace.to_string_lossy()),
        )
        .await
        {
            report.lsp_state = "observed".into();
            report.lsp_configured = value["configured"].as_array().map_or(0, Vec::len);
            report.lsp_pooled = value["servers"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, server)| ToolStatus {
                    name: format!("LSP {}", index + 1),
                    state: if server["state"] == "exited" {
                        "exited"
                    } else {
                        "process_running"
                    }
                    .into(),
                    pid: server["pid"].as_u64().and_then(|pid| pid.try_into().ok()),
                    tools: 0,
                })
                .collect();
        }
        report
    }
}
fn configured_mcp(path: &Path) -> (String, Vec<ToolStatus>) {
    let result = (|| -> anyhow::Result<Vec<ToolStatus>> {
        anyhow::ensure!(
            std::fs::metadata(path)?.len() <= 4 * 1024 * 1024,
            "Configuration too large"
        );
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let servers = value["mcpServers"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Missing mcpServers"))?;
        Ok(servers
            .iter()
            .map(|(id, server)| ToolStatus {
                name: label(server["name"].as_str().unwrap_or(id)),
                state: if server["enabled"] == false {
                    "disabled"
                } else {
                    "not_initialized"
                }
                .into(),
                tools: 0,
                pid: None,
            })
            .collect())
    })();
    match result {
        Ok(items) => ("configured_not_initialized".into(), items),
        Err(_) if !path.exists() => ("absent".into(), Vec::new()),
        Err(_) => ("unreadable".into(), Vec::new()),
    }
}

#[derive(Debug, Serialize)]
pub struct HttpReachability {
    pub origin: String,
    pub state: String,
    pub http_status: Option<u16>,
}
fn origin_url(value: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}
pub fn origin(value: &str) -> String {
    origin_url(value)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "invalid endpoint".into())
}
/// Only HTTP transport reachability: no authorization headers, cookies, redirect,
/// model request or response body. Never claims inference/account validity.
pub async fn http_reachability(endpoint: &str, timeout: Duration) -> HttpReachability {
    let mut report = HttpReachability {
        origin: origin(endpoint),
        state: "invalid_endpoint".into(),
        http_status: None,
    };
    let Some(url) = origin_url(endpoint) else {
        return report;
    };
    let client = match reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            report.state = "client_unavailable".into();
            return report;
        }
    };
    match client.head(url).send().await {
        Ok(response) => {
            report.state = "http_response_received".into();
            report.http_status = Some(response.status().as_u16());
        }
        Err(error) => {
            report.state = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connection_failed"
            } else {
                "request_failed"
            }
            .into();
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn configured_tools_remain_unchecked_and_secrets_are_not_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-servers.json");
        std::fs::write(&path,serde_json::json!({"mcpServers":{"fixture":{"command":"must-not-start","args":["arg-secret"],"env":{"KEY":"env-secret"},"url":"https://user:password@example.test/?key=query-secret","enabled":true},"disabled":{"enabled":false}}}).to_string()).unwrap();
        let mcp = Arc::new(McpServerManager::new(path));
        let source = RuntimeSource {
            mcp: Some(mcp.clone()),
            mcp_ready: Arc::new(tokio::sync::OnceCell::new()),
            workspace: dir.path().into(),
            data_dir: dir.path().into(),
        };
        let status = source.snapshot().await;
        assert!(!status.mcp_initialized);
        assert_eq!(status.mcp_configuration, "configured_not_initialized");
        assert_eq!(
            status
                .mcp
                .iter()
                .find(|entry| entry.name == "fixture")
                .unwrap()
                .state,
            "not_initialized"
        );
        assert!(mcp.list_servers().await.is_empty());
        let serialized = serde_json::to_string(&status).unwrap();
        for secret in [
            "arg-secret",
            "env-secret",
            "query-secret",
            "password",
            "must-not-start",
        ] {
            assert!(!serialized.contains(secret));
        }
        mcp.initialize().await.unwrap();
        source.mcp_ready.set(()).unwrap();
        let status = source.snapshot().await;
        assert!(status.mcp_initialized);
        assert_eq!(
            status
                .mcp
                .iter()
                .find(|entry| entry.name == "fixture")
                .unwrap()
                .state,
            "stopped"
        );
        assert!(!serde_json::to_string(&status)
            .unwrap()
            .contains("env-secret"));
        mcp.shutdown_all().await.unwrap();
    }
    #[tokio::test]
    async fn http_probe_strips_credentials_path_and_query_and_never_follows_redirects() {
        use axum::{
            extract::State,
            http::{HeaderMap, Method, StatusCode, Uri},
            routing::any,
            Router,
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        async fn endpoint(
            State(seen): State<Arc<std::sync::Mutex<Vec<(Method, Uri, HeaderMap)>>>>,
            method: Method,
            uri: Uri,
            headers: HeaderMap,
        ) -> impl axum::response::IntoResponse {
            seen.lock().unwrap().push((method, uri, headers));
            (
                StatusCode::FOUND,
                [("location", "/credential-leak-target")],
                "ignored body",
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .fallback(any(endpoint))
            .with_state(seen.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let report = http_reachability(
            &format!("http://user:password@{address}/secret-path?key=query-secret#fragment-secret"),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(report.state, "http_response_received");
        assert_eq!(report.http_status, Some(302));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Method::HEAD);
        assert_eq!(seen[0].1.to_string(), "/");
        assert!(!seen[0].2.contains_key("authorization"));
        assert!(!seen[0].2.contains_key("cookie"));
        assert!(!serde_json::to_string(&report).unwrap().contains("secret"));
        assert!(!report.origin.contains("password"));
        server.abort();
    }
    #[tokio::test]
    async fn http_probe_timeout_and_invalid_endpoint_are_distinct_from_connectivity() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let report =
            http_reachability(&format!("http://{address}"), Duration::from_millis(50)).await;
        assert_eq!(report.state, "timeout");
        assert!(report.http_status.is_none());
        server.abort();
        let report = http_reachability("file:///private-key", Duration::from_millis(50)).await;
        assert_eq!(report.state, "invalid_endpoint");
        assert_eq!(report.origin, "invalid endpoint");
    }
}
