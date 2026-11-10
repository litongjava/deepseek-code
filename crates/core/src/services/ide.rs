//! Client for the editor extension's loopback MCP endpoint. This is separate
//! from the historical Desktop bridge and never starts or edits an extension.
use anyhow::{bail, ensure, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

const LIMIT: usize = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const PREFIX: &str = "mcp__ide__";

pub fn file_uri(path: &Path) -> Result<String> {
    Ok(reqwest::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("Invalid file URI"))?
        .to_string())
}

#[derive(Clone, Serialize)]
pub struct Editor {
    pub port: u16,
    pub name: String,
    pub transport: String,
    pub workspace_folders: Vec<PathBuf>,
    pub matches_workspace: bool,
    #[serde(skip)]
    token: Option<String>,
}

pub fn lock_directory() -> Result<PathBuf> {
    Ok(std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or(
            dirs::home_dir()
                .context("Home directory unavailable")?
                .join(".claude"),
        )
        .join("ide"))
}

pub fn discover(directory: &Path, workspace: &Path) -> Result<Vec<Editor>> {
    if !directory.exists() {
        return Ok(vec![]);
    }
    let workspace = workspace.canonicalize()?;
    let mut editors = Vec::new();
    for entry in std::fs::read_dir(directory)?.take(256) {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("lock") {
            continue;
        }
        let Some(port) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p > 0)
        else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > 65536 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (folders, name, transport, token) = match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                let folders = value["workspaceFolders"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Vec<_>>();
                let transport = value["transport"].as_str().unwrap_or("sse");
                if !matches!(transport, "ws" | "sse") {
                    continue;
                }
                (
                    folders,
                    value["ideName"]
                        .as_str()
                        .unwrap_or("IDE")
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(100)
                        .collect(),
                    transport.to_owned(),
                    value["authToken"].as_str().map(str::to_owned),
                )
            }
            Err(_) if !text.trim_start().starts_with('{') => (
                text.lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect(),
                "IDE".into(),
                "sse".into(),
                None,
            ),
            Err(_) => continue,
        };
        let matches_workspace = folders.iter().any(|folder| {
            folder
                .canonicalize()
                .is_ok_and(|folder| workspace.starts_with(folder))
        });
        editors.push(Editor {
            port,
            name,
            transport,
            workspace_folders: folders,
            matches_workspace,
            token,
        });
    }
    editors.sort_by_key(|e| e.port);
    Ok(editors)
}

#[derive(Default, Clone, Serialize)]
pub struct Status {
    pub connected: bool,
    pub editor: Option<Editor>,
    pub selection: Option<Value>,
    pub error: Option<String>,
}

enum Command {
    Request(u64, &'static str, Value, oneshot::Sender<Result<Value>>),
    Notify(&'static str, Value),
    Cancel(u64),
}

struct Connection {
    send: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
    status: Arc<RwLock<Status>>,
    tools: Vec<crate::mcp::tool_executor::McpToolDefinition>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
pub struct IdeClient {
    connection: Mutex<Option<Connection>>,
    next_id: AtomicU64,
}

impl IdeClient {
    pub async fn status(&self) -> Status {
        match self.connection.lock().await.as_ref() {
            Some(connection) => connection.status.read().await.clone(),
            None => Status::default(),
        }
    }

    pub async fn disconnect(&self) {
        self.connection.lock().await.take();
    }

    pub async fn connect(&self, editor: Editor) -> Result<()> {
        ensure!(
            editor.matches_workspace,
            "Editor workspace does not contain this session's workspace"
        );
        let wire = tokio::time::timeout(TIMEOUT, Wire::connect(&editor))
            .await
            .context("IDE connection timed out")??;
        let (send, receive) = mpsc::channel(64);
        let status = Arc::new(RwLock::new(Status {
            connected: true,
            editor: Some(editor),
            ..Status::default()
        }));
        let task = tokio::spawn(serve(wire, receive, status.clone()));
        let mut candidate = Connection {
            send,
            task,
            status,
            tools: vec![],
        };
        let initialized = self.request_on(&candidate, "initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"deepseek-code","version":env!("CARGO_PKG_VERSION")}})).await?;
        ensure!(
            initialized["protocolVersion"].is_string(),
            "IDE returned an invalid initialize response"
        );
        candidate
            .send
            .send(Command::Notify("notifications/initialized", json!({})))
            .await?;
        candidate
            .send
            .send(Command::Notify(
                "ide_connected",
                json!({"pid":std::process::id()}),
            ))
            .await?;
        let mut cursor = None;
        for page in 0..16 {
            let result = self
                .request_on(
                    &candidate,
                    "tools/list",
                    match &cursor {
                        Some(cursor) => json!({"cursor":cursor}),
                        None => json!({}),
                    },
                )
                .await?;
            let tools = result["tools"]
                .as_array()
                .context("IDE tools/list did not return tools")?;
            for tool in tools {
                let name = tool["name"].as_str().context("IDE tool has no name")?;
                ensure!(
                    !name.is_empty()
                        && name.len() <= 100
                        && name
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
                    "Invalid IDE tool name"
                );
                ensure!(tool["inputSchema"].is_object(), "IDE tool schema missing");
                let name = format!("{PREFIX}{name}");
                ensure!(
                    !candidate.tools.iter().any(|t| t.name == name),
                    "Duplicate IDE tool name"
                );
                candidate
                    .tools
                    .push(crate::mcp::tool_executor::McpToolDefinition {
                        name,
                        description: tool["description"]
                            .as_str()
                            .unwrap_or("Editor extension tool")
                            .chars()
                            .take(8192)
                            .collect(),
                        input_schema: tool["inputSchema"].clone(),
                        annotations: tool.get("annotations").cloned(),
                        server_name: "ide".into(),
                    });
            }
            ensure!(
                candidate.tools.len() <= 256,
                "IDE advertised too many tools"
            );
            cursor = result["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
            ensure!(page < 15, "IDE tool pagination exceeded limit");
        }
        *self.connection.lock().await = Some(candidate);
        Ok(())
    }

    async fn request_on(
        &self,
        connection: &Connection,
        method: &'static str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (send, reply) = oneshot::channel();
        let request = async {
            connection
                .send
                .send(Command::Request(id, method, params, send))
                .await
                .context("IDE connection closed")?;
            reply.await.context("IDE disconnected before response")?
        };
        match tokio::time::timeout(TIMEOUT, request).await {
            Ok(value) => value,
            Err(_) => {
                let _ = connection.send.try_send(Command::Cancel(id));
                bail!("IDE {method} timed out");
            }
        }
    }

    pub async fn tools(&self) -> Vec<crate::mcp::tool_executor::McpToolDefinition> {
        match self.connection.lock().await.as_ref() {
            Some(connection) if connection.status.read().await.connected => {
                connection.tools.clone()
            }
            _ => vec![],
        }
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let connection = self.connection.lock().await;
        let connection = connection.as_ref().context("IDE is not connected")?;
        let tool = connection
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .with_context(|| format!("IDE tool unavailable: {name}"))?;
        let schema =
            jsonschema::validator_for(&tool.input_schema).context("Invalid IDE tool schema")?;
        schema
            .validate(&arguments)
            .map_err(|error| anyhow::anyhow!("Invalid IDE tool arguments: {error}"))?;
        let raw = name
            .strip_prefix(PREFIX)
            .context("Invalid IDE tool namespace")?;
        let result = self
            .request_on(
                connection,
                "tools/call",
                json!({"name":raw,"arguments":arguments}),
            )
            .await?;
        ensure!(
            result["isError"] != true,
            "IDE tool failed: {}",
            result["content"]
        );
        Ok(result)
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
enum Wire {
    Ws(Socket),
    Sse {
        client: reqwest::Client,
        endpoint: reqwest::Url,
        incoming: mpsc::Receiver<Result<Value>>,
        reader: ReaderGuard,
    },
}
struct ReaderGuard(tokio::task::JoinHandle<()>);
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Wire {
    async fn connect(editor: &Editor) -> Result<Self> {
        if editor.transport == "ws" {
            let mut request = format!("ws://127.0.0.1:{}", editor.port).into_client_request()?;
            request
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "mcp".parse()?);
            if let Some(token) = &editor.token {
                request.headers_mut().insert(
                    "X-Claude-Code-Ide-Authorization",
                    token.parse().context("Invalid IDE authorization header")?,
                );
            }
            let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                max_message_size: Some(LIMIT),
                max_frame_size: Some(LIMIT),
                ..Default::default()
            };
            let (socket, _) =
                tokio_tungstenite::connect_async_with_config(request, Some(config), true)
                    .await
                    .context("IDE WebSocket handshake failed")?;
            return Ok(Self::Ws(socket));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = &editor.token {
            headers.insert(
                "X-Claude-Code-Ide-Authorization",
                token.parse().context("Invalid IDE authorization header")?,
            );
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(TIMEOUT)
            .default_headers(headers)
            .build()?;
        let url = reqwest::Url::parse(&format!("http://127.0.0.1:{}/sse", editor.port))?;
        let response = client
            .get(url.clone())
            .header("Accept", "text/event-stream")
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream")),
            "IDE did not return an SSE stream"
        );
        let (send, mut incoming) = mpsc::channel(64);
        let reader = ReaderGuard(tokio::spawn(async move {
            let result = read_sse(response, &send).await;
            if let Err(error) = result {
                let _ = send.send(Err(error)).await;
            }
        }));
        let endpoint = incoming
            .recv()
            .await
            .context("IDE SSE ended before endpoint")??;
        let endpoint = url.join(
            endpoint["endpoint"]
                .as_str()
                .context("IDE SSE must announce endpoint first")?,
        )?;
        ensure!(
            endpoint.origin() == url.origin()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.fragment().is_none(),
            "IDE SSE endpoint must stay on the same loopback origin"
        );
        Ok(Self::Sse {
            client,
            endpoint,
            incoming,
            reader,
        })
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let text = serde_json::to_string(&message)?;
        ensure!(text.len() <= LIMIT, "IDE request exceeds 4 MiB");
        match self {
            Self::Ws(socket) => socket.send(Message::Text(text)).await?,
            Self::Sse {
                client, endpoint, ..
            } => {
                // Do not include the endpoint's session query string in errors.
                let response = client
                    .post(endpoint.clone())
                    .header("Content-Type", "application/json")
                    .body(text)
                    .timeout(TIMEOUT)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("IDE SSE post failed"))?;
                ensure!(
                    response.status().is_success(),
                    "IDE SSE post returned {}",
                    response.status()
                );
            }
        }
        Ok(())
    }

    async fn receive(&mut self) -> Result<Value> {
        match self {
            Self::Ws(socket) => loop {
                match socket.next().await.context("IDE disconnected")?? {
                    Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                    Message::Binary(bytes) => return Ok(serde_json::from_slice(&bytes)?),
                    Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await?,
                    Message::Close(_) => bail!("IDE disconnected"),
                    _ => {}
                }
            },
            Self::Sse {
                incoming, reader, ..
            } => {
                let _ = &reader;
                incoming.recv().await.context("IDE SSE disconnected")?
            }
        }
    }
}

async fn read_sse(response: reqwest::Response, send: &mpsc::Sender<Result<Value>>) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut event = String::new();
    let mut data = String::new();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk?);
        ensure!(
            buffer.len() + data.len() <= LIMIT,
            "IDE SSE event exceeds 4 MiB"
        );
        while let Some(end) = buffer.iter().position(|c| *c == b'\n') {
            let line = String::from_utf8(buffer.drain(..=end).collect())?;
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !data.is_empty() {
                    let data = data.trim_end_matches('\n');
                    let message = if event == "endpoint" {
                        json!({"endpoint":data})
                    } else {
                        serde_json::from_str(data)?
                    };
                    send.send(Ok(message))
                        .await
                        .context("IDE receiver closed")?;
                }
                event.clear();
                data.clear();
            } else if let Some(value) = line.strip_prefix("event:") {
                event = value.trim_start().into();
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                data.push('\n');
            }
        }
    }
    bail!("IDE SSE stream ended")
}

async fn serve(mut wire: Wire, mut commands: mpsc::Receiver<Command>, status: Arc<RwLock<Status>>) {
    let mut pending = HashMap::<u64, oneshot::Sender<Result<Value>>>::new();
    let mut sweep = tokio::time::interval(Duration::from_millis(250));
    let result: Result<()> = async {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Request(id, method, params, reply)) => {
                        pending.insert(id, reply);
                        wire.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
                    },
                    Some(Command::Notify(method, params)) => wire.send(json!({"jsonrpc":"2.0","method":method,"params":params})).await?,
                    Some(Command::Cancel(id)) => { if pending.remove(&id).is_some() { wire.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})).await?; } },
                    None => break,
                },
                incoming = wire.receive() => {
                    let message = incoming?;
                    if let Some(method) = message["method"].as_str() {
                        if message.get("id").is_some() {
                            let response = if method == "ping" {json!({"jsonrpc":"2.0","id":message["id"],"result":{}})} else {json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"Unsupported client method"}})};
                            wire.send(response).await?;
                        } else if method == "selection_changed" {
                            let params = &message["params"];
                            let mut state = status.write().await;
                            state.selection = if params.is_object() && serde_json::to_vec(params)?.len() <= 128 * 1024 {Some(params.clone())} else {None};
                        }
                    } else if let Some(reply) = message["id"].as_u64().and_then(|id|pending.remove(&id)) {
                        let result = if message.get("error").is_some() {Err(anyhow::anyhow!("IDE RPC error: {}",message["error"]))} else {message.get("result").cloned().context("IDE response has no result")};
                        let _ = reply.send(result);
                    }
                },
                _ = sweep.tick() => {
                    let abandoned: Vec<_> = pending.iter().filter(|(_,reply)|reply.is_closed()).map(|(id,_)|*id).collect();
                    for id in abandoned { pending.remove(&id); wire.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})).await?; }
                },
            }
        }
        Ok(())
    }.await;
    let mut state = status.write().await;
    state.connected = false;
    state.selection = None;
    state.error = result.err().map(|error| error.to_string());
    for (_, reply) in pending {
        let _ = reply.send(Err(anyhow::anyhow!("IDE connection closed")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::{
        accept_hdr_async,
        tungstenite::handshake::server::{Request, Response},
    };

    #[test]
    fn discovery_filters_paths_and_never_serializes_authentication() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let locks = temp.path().join("locks");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&locks).unwrap();
        std::fs::write(locks.join("12345.lock"),json!({"workspaceFolders":[project],"transport":"ws","authToken":"local-fixture-only","ideName":"VS Code"}).to_string()).unwrap();
        std::fs::write(locks.join("0.lock"), "{}").unwrap();
        std::fs::write(locks.join("broken.lock"), "{}").unwrap();
        std::fs::write(locks.join("12346.lock"), "{broken").unwrap();
        let editors = discover(&locks, &project).unwrap();
        assert_eq!(editors.len(), 1);
        assert!(editors[0].matches_workspace);
        assert!(!serde_json::to_string(&editors)
            .unwrap()
            .contains("local-fixture-only"));
        assert!(!discover(&locks, temp.path()).unwrap()[0].matches_workspace);
    }

    async fn fixture() -> (Editor, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket =
                accept_hdr_async(stream, |request: &Request, mut response: Response| {
                    assert_eq!(
                        request.headers()["X-Claude-Code-Ide-Authorization"],
                        "local-fixture-only"
                    );
                    assert_eq!(request.headers()["Sec-WebSocket-Protocol"], "mcp");
                    response
                        .headers_mut()
                        .insert("Sec-WebSocket-Protocol", "mcp".parse().unwrap());
                    Ok(response)
                })
                .await
                .unwrap();
            let mut seen = vec![];
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let message: Value = serde_json::from_str(&text).unwrap();
                seen.push(message.clone());
                let result = match message["method"].as_str().unwrap_or("") {
                    "initialize" => {
                        json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"getDiagnostics","inputSchema":{"type":"object"}},{"name":"wait","inputSchema":{"type":"object"}}]})
                    }
                    "tools/call" if message["params"]["name"] == "wait" => continue,
                    "tools/call" if message["params"]["arguments"]["fail"] == true => {
                        json!({"isError":true,"content":[{"type":"text","text":"fixture failure"}]})
                    }
                    "tools/call" => json!({"content":[{"type":"text","text":"真实诊断"}]}),
                    "notifications/cancelled" => break,
                    _ => continue,
                };
                socket
                    .send(Message::Text(
                        json!({"jsonrpc":"2.0","id":message["id"],"result":result}).to_string(),
                    ))
                    .await
                    .unwrap();
                if message["method"] == "tools/list" {
                    socket.send(Message::Text(json!({"jsonrpc":"2.0","method":"selection_changed","params":{"filePath":"src/main.rs","text":"selected text","selection":{"start":{"line":0,"character":0},"end":{"line":0,"character":13}}}}).to_string())).await.unwrap();
                }
            }
            seen
        });
        (
            Editor {
                port,
                name: "fixture".into(),
                transport: "ws".into(),
                workspace_folders: vec![],
                matches_workspace: true,
                token: Some("local-fixture-only".into()),
            },
            task,
        )
    }

    #[tokio::test]
    async fn websocket_auth_tools_notifications_errors_and_cancellation_are_real() {
        let (editor, server) = fixture().await;
        let client = IdeClient::default();
        client.connect(editor).await.unwrap();
        assert_eq!(client.tools().await[0].name, "mcp__ide__getDiagnostics");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if client.status().await.selection.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            client.status().await.selection.unwrap()["text"],
            "selected text"
        );
        let response = client
            .call_tool("mcp__ide__getDiagnostics", json!({}))
            .await
            .unwrap();
        assert_eq!(response["content"][0]["text"], "真实诊断");
        assert!(client
            .call_tool("mcp__ide__getDiagnostics", json!({"fail":true}))
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture failure"));
        assert!(client.call_tool("Read", json!({})).await.is_err());
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            client.call_tool("mcp__ide__wait", json!({}))
        )
        .await
        .is_err());
        let seen = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(seen
            .iter()
            .any(|message| message["method"] == "notifications/cancelled"));
        assert!(seen
            .iter()
            .any(|message| message["method"] == "ide_connected"));
        client.disconnect().await;
        assert!(!client.status().await.connected);
    }

    #[tokio::test]
    async fn failed_reconnect_retains_working_connection_and_wrong_workspace_is_rejected() {
        let (editor, server) = fixture().await;
        let client = IdeClient::default();
        client.connect(editor.clone()).await.unwrap();
        let mut wrong = editor;
        wrong.matches_workspace = false;
        assert!(client.connect(wrong).await.is_err());
        assert!(client.status().await.connected);
        client
            .call_tool("mcp__ide__getDiagnostics", json!({}))
            .await
            .unwrap();
        client.disconnect().await;
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sse_round_trip_and_origin_restriction() {
        use axum::{
            extract::{Json, State},
            response::sse::{Event, Sse},
            routing::{get, post},
            Router,
        };
        type Send = mpsc::Sender<std::result::Result<Event, std::convert::Infallible>>;
        let (send, receive) = mpsc::channel(32);
        let receiver = Arc::new(Mutex::new(Some(receive)));
        let app = Router::new().route("/sse",get({let receiver = receiver.clone(); move || {let receiver=receiver.clone(); async move {
            Sse::new(tokio_stream::wrappers::ReceiverStream::new(receiver.lock().await.take().unwrap()))
        }}})).route("/messages",post(|State(send):State<Send>, Json(message):Json<Value>|async move {
            let result = match message["method"].as_str() {
                Some("initialize") => json!({"protocolVersion":"2024-11-05"}),
                Some("tools/list") => json!({"tools":[{"name":"getDiagnostics","inputSchema":{"type":"object"}}]}),
                Some("tools/call") => json!({"content":[{"type":"text","text":"SSE diagnostics"}]}),
                _ => return axum::http::StatusCode::ACCEPTED,
            };
            send.send(Ok(Event::default().data(json!({"jsonrpc":"2.0","id":message["id"],"result":result}).to_string()))).await.unwrap();
            axum::http::StatusCode::ACCEPTED
        })).with_state(send.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        send.send(Ok(Event::default()
            .event("endpoint")
            .data("/messages?sessionId=fixture")))
            .await
            .unwrap();
        let client = IdeClient::default();
        client
            .connect(Editor {
                port,
                name: "SSE fixture".into(),
                transport: "sse".into(),
                workspace_folders: vec![],
                matches_workspace: true,
                token: None,
            })
            .await
            .unwrap();
        assert_eq!(
            client
                .call_tool("mcp__ide__getDiagnostics", json!({}))
                .await
                .unwrap()["content"][0]["text"],
            "SSE diagnostics"
        );
        client.disconnect().await;
        server.abort();

        let app = Router::new().route(
            "/sse",
            get(|| async {
                Sse::new(futures::stream::iter([Ok::<_, std::convert::Infallible>(
                    Event::default()
                        .event("endpoint")
                        .data("https://example.com/collect"),
                )]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = client
            .connect(Editor {
                port,
                name: "invalid".into(),
                transport: "sse".into(),
                workspace_folders: vec![],
                matches_workspace: true,
                token: None,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("same loopback origin"));
        server.abort();
    }
}
