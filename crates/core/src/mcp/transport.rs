//! MCP transports, as in the reference CLI: stdio, Streamable HTTP, the older
//! HTTP+SSE transport and WebSocket. Each moves whole JSON-RPC messages, so
//! the request loop is shared.
use anyhow::{anyhow, bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) const PROTOCOL_VERSION: &str = "2025-06-18";

type WebSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) enum Channel {
    Stdio { stdin: tokio::process::ChildStdin, stdout: BufReader<tokio::process::ChildStdout> },
    /// Streamable HTTP: every message is a POST; replies arrive in its response.
    Http { client: reqwest::Client, url: String, headers: HashMap<String, String>, session: Option<String>, inbox: VecDeque<Value> },
    /// HTTP+SSE: one GET event stream carries replies; messages POST to its endpoint.
    Sse { client: reqwest::Client, endpoint: String, headers: HashMap<String, String>, inbox: tokio::sync::mpsc::UnboundedReceiver<Value>, reader: tokio::task::JoinHandle<()> },
    WebSocket(Box<WebSocket>),
}

/// Complete events in `buffer` as (event, data); the incomplete tail stays.
pub(super) fn sse_events(buffer: &mut String) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let normalized = buffer.replace("\r\n", "\n");
    let mut rest = normalized.as_str();
    while let Some(end) = rest.find("\n\n") {
        let (block, tail) = rest.split_at(end);
        rest = &tail[2..];
        let mut event = "message".to_owned();
        let mut data = Vec::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("event:") { event = value.trim().to_owned(); }
            if let Some(value) = line.strip_prefix("data:") { data.push(value.strip_prefix(' ').unwrap_or(value)); }
        }
        if !data.is_empty() { events.push((event, data.join("\n"))); }
    }
    *buffer = rest.to_owned();
    events
}

fn messages(value: Value) -> Vec<Value> {
    match value { Value::Array(batch) => batch, single => vec![single] }
}

fn request(client: &reqwest::Client, url: &str, headers: &HashMap<String, String>) -> reqwest::RequestBuilder {
    headers.iter().fold(client.post(url), |request, (key, value)| request.header(key, value))
}

impl Channel {
    pub async fn http(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        reqwest::Url::parse(url).context("MCP server URL is invalid")?;
        Ok(Self::Http { client: reqwest::Client::new(), url: url.to_owned(), headers: headers.clone(), session: None, inbox: VecDeque::new() })
    }

    pub async fn sse(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        let client = reqwest::Client::new();
        let base = reqwest::Url::parse(url).context("MCP server URL is invalid")?;
        let response = headers.iter().fold(client.get(url), |request, (key, value)| request.header(key, value))
            .header("accept", "text/event-stream").send().await?.error_for_status()?;
        let (tx, inbox) = tokio::sync::mpsc::unbounded_channel();
        let (endpoint_tx, endpoint_rx) = tokio::sync::oneshot::channel();
        let reader = tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = stream.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                for (event, data) in sse_events(&mut buffer) {
                    if event == "endpoint" {
                        if let Some(sender) = endpoint_tx.take() { let _ = sender.send(data); }
                    } else if let Ok(value) = serde_json::from_str::<Value>(&data) {
                        for message in messages(value) { let _ = tx.send(message); }
                    }
                }
            }
        });
        let endpoint = tokio::time::timeout(std::time::Duration::from_secs(10), endpoint_rx).await
            .map_err(|_| anyhow!("MCP SSE server sent no endpoint event"))??;
        let endpoint = base.join(endpoint.trim()).context("MCP SSE endpoint is invalid")?.to_string();
        Ok(Self::Sse { client, endpoint, headers: headers.clone(), inbox, reader })
    }

    pub async fn websocket(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = url.into_client_request().context("MCP WebSocket URL is invalid")?;
        request.headers_mut().insert("sec-websocket-protocol", "mcp".parse()?);
        for (key, value) in headers {
            request.headers_mut().insert(reqwest::header::HeaderName::from_bytes(key.as_bytes())?, value.parse()?);
        }
        let (socket, _) = tokio_tungstenite::connect_async(request).await.context("Cannot connect to the MCP WebSocket server")?;
        Ok(Self::WebSocket(Box::new(socket)))
    }

    pub async fn send(&mut self, message: &Value) -> Result<()> {
        let text = serde_json::to_string(message)?;
        match self {
            Self::Stdio { stdin, .. } => {
                stdin.write_all(format!("{text}\n").as_bytes()).await?;
                stdin.flush().await?;
            }
            Self::Http { client, url, headers, session, inbox } => {
                let mut post = request(client, url, headers)
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header("mcp-protocol-version", PROTOCOL_VERSION)
                    .body(text);
                if let Some(id) = session.as_deref() { post = post.header("mcp-session-id", id); }
                let response = post.send().await?.error_for_status()?;
                if let Some(id) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                    *session = Some(id.to_owned());
                }
                let event_stream = response.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|kind| kind.starts_with("text/event-stream"));
                let body = response.text().await?;
                if event_stream {
                    let mut buffer = body + "\n\n";
                    for (_, data) in sse_events(&mut buffer) {
                        if let Ok(value) = serde_json::from_str::<Value>(&data) { inbox.extend(messages(value)); }
                    }
                } else if !body.trim().is_empty() {
                    inbox.extend(messages(serde_json::from_str(&body).context("MCP server returned invalid JSON")?));
                }
            }
            Self::Sse { client, endpoint, headers, .. } => {
                request(client, endpoint, headers).header("content-type", "application/json").body(text).send().await?.error_for_status()?;
            }
            Self::WebSocket(socket) => socket.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await?,
        }
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Value> {
        match self {
            Self::Stdio { stdout, .. } => {
                let mut line = String::new();
                anyhow::ensure!(stdout.read_line(&mut line).await? != 0, "MCP stdout closed");
                Ok(serde_json::from_str(line.trim())?)
            }
            Self::Http { inbox, .. } => inbox.pop_front().context("MCP server sent no reply"),
            Self::Sse { inbox, .. } => inbox.recv().await.context("MCP SSE stream closed"),
            Self::WebSocket(socket) => loop {
                match socket.next().await.context("MCP WebSocket closed")?? {
                    tokio_tungstenite::tungstenite::Message::Text(text) => break Ok(serde_json::from_str(&text)?),
                    tokio_tungstenite::tungstenite::Message::Close(_) => bail!("MCP WebSocket closed"),
                    _ => continue,
                }
            },
        }
    }

    pub async fn close(&mut self) {
        match self {
            Self::Http { client, url, headers, session: Some(id), .. } => {
                // Streamable HTTP ends a session with DELETE; servers may decline.
                let _ = headers.iter().fold(client.delete(url.as_str()), |request, (key, value)| request.header(key, value)).header("mcp-session-id", id.as_str()).send().await;
            }
            Self::Sse { reader, .. } => reader.abort(),
            Self::WebSocket(socket) => { let _ = socket.close().await; }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_split_on_blank_lines_and_keep_the_tail() {
        let mut buffer = "event: endpoint\ndata: /messages?s=1\n\ndata: {\"id\":1}\r\n\r\ndata: {\"id\"".to_owned();
        assert_eq!(sse_events(&mut buffer), [("endpoint".to_owned(), "/messages?s=1".to_owned()), ("message".to_owned(), "{\"id\":1}".to_owned())]);
        assert_eq!(buffer, "data: {\"id\"");
    }
}
