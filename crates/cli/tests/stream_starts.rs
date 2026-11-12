//! One `message_start` per logical model response.
//!
//! OpenAI-compatible providers commonly repeat `model` on every frame and
//! `role: "assistant"` in every delta. Those repeats describe one response, so
//! they must not create extra starts, extra tool-loop rounds, or an inflated
//! SDK `num_turns`. Anthropic's native stream also gets exactly one start per
//! request even though its `message_start` frame is handled separately.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Protocol {
    OpenAi,
    Anthropic,
}

#[derive(Clone)]
struct Fixture {
    requests: Arc<AtomicUsize>,
    protocol: Protocol,
    /// The first HTTP attempt is a 200 stream with an empty handshake. The send
    /// layer withholds it and retries inside the same logical response.
    empty_first: bool,
}

/// Complete OpenAI-compatible stream, terminated like a real provider.
fn sse(frames: impl IntoIterator<Item = Value>) -> String {
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    stream
}

/// `model` belongs at the event root; only the role is repeated in deltas.
fn openai_chunk(delta: Value, finish: Value) -> Value {
    json!({
        "model": "fixture-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

/// A handshake with no content: `role` only, zero completion tokens. This is
/// the shape the send layer withholds before retrying.
fn openai_empty_handshake() -> String {
    let mut stream = String::new();
    stream.push_str(&format!(
        "data: {}\n\n",
        json!({
            "model": "fixture-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 0, "total_tokens": 4}
        })
    ));
    stream
}

/// First response: the model asks for a real `Read`, then ends the round.
fn openai_tool_turn() -> String {
    sse([
        openai_chunk(
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": "read-1",
                    "type": "function",
                    "function": {
                        "name": "Read",
                        "arguments": "{\"file_path\":\"fixture.txt\"}",
                    },
                }],
            }),
            Value::Null,
        ),
        // Repeated role deltas are part of the same response.
        openai_chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        openai_chunk(json!({"role": "assistant"}), Value::Null),
        openai_chunk(json!({"role": "assistant"}), json!("tool_calls")),
    ])
}

/// Second response: a stream of text chunks, every one repeating the role.
fn openai_text_turn() -> String {
    let mut frames = vec![
        openai_chunk(
            json!({"role": "assistant", "content": "final"}),
            Value::Null,
        ),
        openai_chunk(json!({"role": "assistant", "content": " "}), Value::Null),
        openai_chunk(
            json!({"role": "assistant", "content": "answer"}),
            Value::Null,
        ),
    ];
    for index in 0..20 {
        frames.push(openai_chunk(
            json!({"role": "assistant", "content": format!(" streamed{index}")}),
            Value::Null,
        ));
    }
    frames.push(json!({
        "model": "fixture-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 30, "completion_tokens": 12, "total_tokens": 42}
    }));
    sse(frames)
}

fn anthropic_frame(frame: Value) -> String {
    format!("data: {frame}\n\n")
}

fn anthropic_tool_turn() -> String {
    [
        json!({"type":"message_start","message":{"id":"msg-1","type":"message","role":"assistant","model":"fixture-model","content":[],"usage":{"input_tokens":18,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private reasoning"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"read-1","name":"Read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"fixture.txt\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(anthropic_frame)
    .collect()
}

fn anthropic_text_turn() -> String {
    let mut frames = vec![
        json!({"type":"message_start","message":{"id":"msg-2","type":"message","role":"assistant","model":"fixture-model","content":[],"usage":{"input_tokens":40,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    ];
    for part in ["final", " ", "answer"] {
        frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":part}}));
    }
    // An Anthropic response also repeats no model field here; the extra deltas
    // still belong to the same logical response.
    for index in 0..10 {
        frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":format!(" chunk{index}")}}));
    }
    frames.push(json!({"type":"content_block_stop","index":0}));
    frames.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":6}}));
    frames.push(json!({"type":"message_stop"}));
    frames.into_iter().map(anthropic_frame).collect()
}

async fn respond(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    assert_eq!(body["stream"], true, "the CLI must ask for streaming");
    let index = state.requests.fetch_add(1, Ordering::SeqCst);
    let stream = match state.protocol {
        Protocol::OpenAi if state.empty_first && index == 0 => openai_empty_handshake(),
        Protocol::OpenAi => {
            let logical = if state.empty_first { index - 1 } else { index };
            if logical == 0 {
                openai_tool_turn()
            } else {
                openai_text_turn()
            }
        }
        // Anthropic never retries in these fixtures; each request is one response.
        Protocol::Anthropic if index == 0 => anthropic_tool_turn(),
        Protocol::Anthropic => anthropic_text_turn(),
    };
    ([("content-type", "text/event-stream")], stream).into_response()
}

struct Run {
    events: Vec<Value>,
    stderr: String,
    requests: usize,
}

async fn run(protocol: Protocol, empty_first: bool) -> Run {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("project");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(workspace.join("config")).unwrap();
    std::fs::write(workspace.join("fixture.txt"), "fixture contents\n").unwrap();
    // Shorten only the configured retry wait so the empty handshake can retry.
    std::fs::write(
        workspace.join("config/orchestration.toml"),
        include_str!("../../../src-tauri/config/orchestration.toml")
            .replace("retry_base_backoff_ms = 2000", "retry_base_backoff_ms = 10"),
    )
    .unwrap();
    let fixture = Fixture {
        requests: Arc::default(),
        protocol,
        empty_first,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let route = if protocol == Protocol::Anthropic {
        "/v1/messages"
    } else {
        "/v1/chat/completions"
    };
    let app = Router::new()
        .route(route, post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(
        data.join("providers.json"),
        json!([{
            "id": "fixture", "name": "Fixture", "base_url": base,
            "api_key": "fixture-only",
            "api_format": if protocol == Protocol::Anthropic { "anthropic" } else { "openai" },
            "enabled": true,
            "models": [{
                "id": "fixture-model", "name": "Fixture", "enabled": true,
                "context_window": 128000, "max_tokens": 4096,
                "supports_web_search": false, "supports_vision": false
            }]
        }])
        .to_string(),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&data)
        .arg("--path")
        .arg(&workspace)
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--session-id",
            &id,
            "--permission-mode",
            "ask",
            "read the fixture and answer",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .expect("the CLI must finish")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "the CLI failed: {stderr}\nstdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests = fixture.requests.load(Ordering::SeqCst);
    server.abort();
    Run {
        events,
        stderr,
        requests,
    }
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|event| event["type"] == kind).count()
}

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["delta"].as_str())
        .collect()
}

/// The exact accounting the supervisor's run lost: duplicate starts inflated
/// `num_turns` far past the number of provider responses.
fn assert_one_start_per_response(run: &Run, logical: u64) {
    assert_eq!(
        count(&run.events, "message_start"),
        logical as usize,
        "each logical response needs exactly one start: {}",
        run.stderr
    );
    let results: Vec<_> = run
        .events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "exactly one final SDK result");
    assert_eq!(
        results[0]["num_turns"], logical,
        "starts must not inflate round accounting: {}",
        results[0]
    );
    assert_eq!(results[0]["is_error"], false, "{}", results[0]);
}

fn assert_read_executed_once(run: &Run) {
    let starts: Vec<_> = run
        .events
        .iter()
        .filter(|event| event["type"] == "tool_start")
        .collect();
    assert_eq!(starts.len(), 1, "the model asked for one tool");
    assert_eq!(starts[0]["name"], "Read");
    let results: Vec<_> = run
        .events
        .iter()
        .filter(|event| event["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 1, "the tool ran exactly once");
    assert_eq!(
        results[0]["is_error"], false,
        "input: {}",
        starts[0]["input"]
    );
    assert!(
        results[0]["output"]
            .as_str()
            .is_some_and(|output| output.contains("fixture contents")),
        "the fixture file was actually read: {}",
        results[0]
    );
}

#[tokio::test]
async fn openai_repeated_role_chunks_emit_one_start_per_response() {
    let run = run(Protocol::OpenAi, false).await;
    assert_eq!(run.requests, 2, "two real HTTP requests");
    assert_one_start_per_response(&run, 2);
    assert_read_executed_once(&run);
    assert!(
        text_of(&run.events).contains("final answer"),
        "text deltas were lost: {:?}",
        text_of(&run.events)
    );
    assert!(
        count(&run.events, "usage") > 0,
        "usage was not reported: {}",
        run.stderr
    );
}

#[tokio::test]
async fn openai_empty_handshake_retry_keeps_one_start_for_the_response() {
    let run = run(Protocol::OpenAi, true).await;
    assert_eq!(
        run.requests, 3,
        "an empty handshake is retried without becoming a second response"
    );
    assert_one_start_per_response(&run, 2);
    assert_read_executed_once(&run);
    assert!(text_of(&run.events).contains("final answer"));
    assert!(
        count(&run.events, "usage") > 0,
        "usage reporting is preserved through the retry"
    );
}

#[tokio::test]
async fn anthropic_tool_loop_emits_one_start_per_response_and_keeps_deltas() {
    let run = run(Protocol::Anthropic, false).await;
    assert_eq!(run.requests, 2, "two real HTTP requests");
    assert_one_start_per_response(&run, 2);
    assert_read_executed_once(&run);
    let thinking: String = run
        .events
        .iter()
        .filter(|event| event["type"] == "thinking")
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(
        thinking, "private reasoning",
        "thinking deltas were lost or duplicated"
    );
    assert!(
        text_of(&run.events).contains("final answer"),
        "text deltas were lost: {:?}",
        text_of(&run.events)
    );
    assert!(count(&run.events, "usage") > 0, "usage was not reported");
}

fn responses_frame(event: Value) -> String {
    format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().unwrap_or_default()
    )
}

/// The built-in Responses adapter is a separate transport, so its start must
/// also be one per response rather than one per adapter chunk.
#[tokio::test]
async fn responses_transport_emits_one_start_per_response() {
    use deepseek_code_core::runtime::RuntimePaths;
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    std::fs::write(root.path().join("fixture.txt"), "fixture contents\n").unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                assert_eq!(body["stream"].as_bool(), Some(true), "the CLI streams");
                let step = seen.fetch_add(1, Ordering::SeqCst);
                let item = json!({
                    "type": "function_call", "id": "fc-read-1", "call_id": "read-1",
                    "name": "Read", "arguments": "{\"file_path\":\"fixture.txt\"}"
                });
                let stream: String = if step == 0 {
                    [
                        json!({"type":"response.output_item.done","item":item}),
                        json!({"type":"response.completed","response":{
                            "id":"r1","model":"fixture-model","status":"completed",
                            "output":[item],
                            "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}
                        }}),
                    ]
                    .into_iter()
                    .map(responses_frame)
                    .collect()
                } else {
                    [
                        json!({"type":"response.output_text.delta","delta":"final answer"}),
                        json!({"type":"response.completed","response":{
                            "id":"r2","model":"fixture-model","status":"completed",
                            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final answer"}]}],
                            "usage":{"input_tokens":40,"output_tokens":6,"total_tokens":46}
                        }}),
                    ]
                    .into_iter()
                    .map(responses_frame)
                    .collect()
                };
                ([("content-type", "text/event-stream")], stream).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Declared in config.json: a base-url override would clear the built-in id
    // the Responses transport needs.
    let mut config = deepseek_code_core::config::AppConfig::default();
    config.providers.push(
        serde_json::from_value(json!({
            "id":"responses-fixture","name":"Responses fixture","provider_type":"openai",
            "api_key":"fixture-only","base_url":url,"enabled":true,"is_default":true,
            "settings":{"builtinId":"openai"},"supports_web_search":false,
            "models":[{"id":"fixture-model","name":"fixture-model","enabled":true,"api":"openai-responses",
                "supports_vision":false,"supports_tools":true,"supports_text":true,"supports_streaming":true}]
        }))
        .unwrap(),
    );
    std::fs::write(
        paths.data_dir.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&paths.data_dir)
        .arg("--path")
        .arg(root.path())
        .args([
            "--session-id",
            &uuid::Uuid::new_v4().to_string(),
            "--permission-mode",
            "ask",
            "read the fixture and answer",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .expect("the CLI must finish")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "the CLI failed: {stderr}\nstdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    server.abort();
    assert_eq!(requests.load(Ordering::SeqCst), 2, "two Responses requests");
    let run = Run {
        events,
        stderr,
        requests: 2,
    };
    assert_one_start_per_response(&run, 2);
    assert_read_executed_once(&run);
    assert!(
        text_of(&run.events).contains("final answer"),
        "text deltas were lost: {:?}",
        text_of(&run.events)
    );
}
