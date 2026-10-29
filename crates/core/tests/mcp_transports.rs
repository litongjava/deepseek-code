//! MCP over Streamable HTTP, HTTP+SSE and WebSocket against local fixtures.
use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::{get, post}, Json, Router};
use claude_core::mcp::{McpConnector, McpServerConfig};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The fixture server's answer to one JSON-RPC message, or None for a notification.
fn answer(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message["method"].as_str()? {
        "initialize" => json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
        "tools/list" => json!({"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}),
        "tools/call" => json!({"content":[{"type":"text","text":format!("echo: {}", message["params"]["arguments"]["text"].as_str().unwrap_or_default())}]}),
        "prompts/list" => json!({"prompts":[{"name":"review","description":"Review a PR","arguments":[{"name":"pr","required":true}]}]}),
        "prompts/get" => json!({"messages":[{"role":"user","content":{"type":"text","text":format!("Review PR {}", message["params"]["arguments"]["pr"].as_str().unwrap_or_default())}}]}),
        _ => return Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unknown"}})),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

async fn exercise(config: McpServerConfig) {
    let mut connector = McpConnector::new(config);
    connector.start().await.unwrap();
    let tools = connector.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let result = connector.call_tool("echo", json!({"text":"hi"})).await.unwrap();
    assert!(result.to_string().contains("echo: hi"), "{result}");
    let prompts = connector.list_prompts().await.unwrap();
    assert_eq!((prompts[0].name.as_str(), prompts[0].arguments[0].required), ("review", true));
    assert_eq!(connector.get_prompt("review", json!({"pr":"42"})).await.unwrap(), "Review PR 42");
    connector.stop().await.unwrap();
}

fn remote(transport: &str, url: String) -> McpServerConfig {
    McpServerConfig { id: "fixture".into(), name: "fixture".into(), enabled: true, transport: Some(transport.into()), url: Some(url), headers: [("x-token".to_owned(), "secret".to_owned())].into(), ..Default::default() }
}

#[derive(Clone, Default)]
struct HttpState {
    sessions_seen: Arc<Mutex<Vec<Option<String>>>>,
}

async fn streamable(State(state): State<HttpState>, headers: HeaderMap, Json(message): Json<Value>) -> axum::response::Response {
    assert_eq!(headers.get("x-token").unwrap(), "secret");
    state.sessions_seen.lock().unwrap().push(headers.get("mcp-session-id").map(|v| v.to_str().unwrap().to_owned()));
    let Some(reply) = answer(&message) else { return axum::http::StatusCode::ACCEPTED.into_response() };
    if message["method"] == "tools/list" {
        // Replies may come as an event stream.
        return ([("content-type", "text/event-stream")], format!("event: message\ndata: {reply}\n\n")).into_response();
    }
    ([("mcp-session-id", "session-1")], Json(reply)).into_response()
}

#[tokio::test]
async fn streamable_http_keeps_its_session_and_reads_event_stream_replies() {
    let state = HttpState::default();
    let app = Router::new().route("/mcp", post(streamable).delete(|| async { axum::http::StatusCode::OK })).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    exercise(remote("http", url)).await;
    let seen = state.sessions_seen.lock().unwrap().clone();
    assert_eq!(seen[0], None);
    assert!(seen[1..].iter().all(|id| id.as_deref() == Some("session-1")), "{seen:?}");
    server.abort();
}

type Outbox = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Value>>>>;

#[tokio::test]
async fn legacy_sse_posts_to_the_announced_endpoint_and_reads_replies_from_the_stream() {
    let outbox: Outbox = Arc::default();
    let stream_outbox = outbox.clone();
    let post_outbox = outbox.clone();
    let app = Router::new()
        .route("/sse", get(move || {
            let outbox = stream_outbox.clone();
            async move {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
                *outbox.lock().unwrap() = Some(tx);
                let first = futures::stream::once(async { Ok::<_, std::convert::Infallible>("event: endpoint\ndata: /messages?session=1\n\n".to_owned()) });
                let replies = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|reply| Ok(format!("event: message\ndata: {reply}\n\n")));
                ([("content-type", "text/event-stream")], axum::body::Body::from_stream(first.chain(replies)))
            }
        }))
        .route("/messages", post(move |Json(message): Json<Value>| {
            let outbox = post_outbox.clone();
            async move {
                if let Some(reply) = answer(&message) {
                    outbox.lock().unwrap().as_ref().unwrap().send(reply).unwrap();
                }
                axum::http::StatusCode::ACCEPTED
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sse", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    exercise(remote("sse", url)).await;
    server.abort();
}

#[tokio::test]
async fn websocket_exchanges_text_frames() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let callback = |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert_eq!(request.headers().get("x-token").unwrap(), "secret");
            response.headers_mut().insert("sec-websocket-protocol", "mcp".parse().unwrap());
            Ok(response)
        };
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
        while let Some(Ok(frame)) = socket.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = frame {
                if let Some(reply) = answer(&serde_json::from_str(&text).unwrap()) {
                    socket.send(tokio_tungstenite::tungstenite::Message::Text(reply.to_string().into())).await.unwrap();
                }
            }
        }
    });
    exercise(remote("ws", url)).await;
    server.abort();
}
