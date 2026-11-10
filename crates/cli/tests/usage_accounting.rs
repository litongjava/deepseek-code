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

#[derive(Clone)]
struct Fixture {
    requests: Arc<AtomicUsize>,
    anthropic: bool,
    known: bool,
}
async fn respond(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if body["stream"] != true {
        return Json(json!({})).into_response();
    }
    let i = state.requests.fetch_add(1, Ordering::SeqCst);
    let (input, output) = match i {
        0 => (11, 2),
        1 => (17, 3),
        2 => (23, 5),
        _ => (7, 1),
    };
    let tool = i < 2;
    let mut frames = Vec::new();
    if state.anthropic {
        let mut start = json!({"id":format!("response-{i}"),"type":"message","role":"assistant","model":"fixture-model","content":[]});
        if state.known {
            start["usage"] =
                json!({"input_tokens":input,"output_tokens":0,"cache_read_input_tokens":4});
        }
        frames.push(json!({"type":"message_start","message":start}));
        if tool {
            frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("tool-{i}"),"name":"Bash","input":{}}}));
            frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"echo fixture\"}"}}));
        } else {
            frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
            frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}));
        }
        frames.push(json!({"type":"content_block_stop","index":0}));
        let mut end = json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}}});
        if state.known {
            end["usage"] = json!({"output_tokens":output});
        }
        frames.push(end.clone());
        frames.push(end); // Cumulative reports for one HTTP request.
        if state.known {
            frames.push(json!({"type":"message_delta","delta":{},"usage":{"input_tokens":null,"output_tokens":null}}));
        }
        frames.push(json!({"type":"message_stop"}));
    } else {
        let delta = if tool {
            json!({"tool_calls":[{"index":0,"id":format!("tool-{i}"),"type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo fixture\"}"}}]})
        } else {
            json!({"content":"done"})
        };
        frames.push(json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}));
        if state.known {
            frames.push(json!({"choices":[],"usage":{"prompt_tokens":input,"completion_tokens":1,"total_tokens":input+1}}));
        }
        frames.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":if tool {"tool_calls"} else {"stop"}}]}));
        if state.known {
            let usage = json!({"choices":[],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output,"prompt_tokens_details":{"cached_tokens":4}}});
            frames.push(usage.clone());
            frames.push(usage);
            frames.push(
                json!({"choices":[],"usage":{"prompt_tokens":null,"completion_tokens":null,
                "prompt_tokens_details":{"cached_tokens":null}}}),
            );
        }
    }
    let mut stream: String = frames
        .into_iter()
        .map(|f| format!("data: {f}\n\n"))
        .collect();
    if !state.anthropic {
        stream.push_str("data: [DONE]\n\n");
    }
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn distinct_requests_accumulate_once_without_compaction_and_new_turns_start_empty() {
    for (anthropic, known) in [(false, true), (true, true), (false, false)] {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let fixture = Fixture {
            requests: Arc::default(),
            anthropic,
            known,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                if anthropic {
                    "/v1/messages"
                } else {
                    "/v1/chat/completions"
                },
                post(respond),
            )
            .with_state(fixture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        std::fs::write(data.join("providers.json"),json!([{"id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only","api_format":if anthropic {"anthropic"} else {"openai"},"enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,"context_window":128000,"max_tokens":4096,"supports_web_search":false,"supports_vision":false}]}]).to_string()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        for followup in [false, true] {
            let mut cmd = Command::new(
                std::env::var_os("CLAUDE_SDK_TEST_EXE")
                    .unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
            );
            cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
                .arg(&data)
                .arg("--path")
                .arg(root.path())
                .args([
                    "--provider",
                    "fixture",
                    "--model",
                    "fixture-model",
                    "--disallowed-tools",
                    "Bash",
                    if followup { "--resume" } else { "--session-id" },
                    &id,
                    "fixture task",
                ])
                .stdin(Stdio::null())
                .kill_on_drop(true);
            for (key, _) in std::env::vars() {
                if key.starts_with("DEEPSEEK_CODE") {
                    cmd.env_remove(key);
                }
            }
            let output = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| l.starts_with('{'))
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let results: Vec<_> = events.iter().filter(|e| e["type"] == "result").collect();
            assert_eq!(results.len(), 1);
            let result = results[0];
            let usage = &result["usage"];
            if known {
                assert_eq!(
                    usage[if anthropic {
                        "input_tokens"
                    } else {
                        "prompt_tokens"
                    }],
                    if followup { 7 } else { 51 },
                    "{result}"
                );
                assert_eq!(
                    usage[if anthropic {
                        "output_tokens"
                    } else {
                        "completion_tokens"
                    }],
                    if followup { 1 } else { 10 },
                    "{result}"
                );
                if !anthropic {
                    assert_eq!(usage["total_tokens"], if followup { 8 } else { 61 });
                }
                assert_eq!(
                    if anthropic {
                        &usage["cache_read_input_tokens"]
                    } else {
                        &usage["prompt_tokens_details"]["cached_tokens"]
                    },
                    if followup { 4 } else { 12 }
                );
            } else {
                assert_eq!(usage, &json!({}), "unknown usage must remain unreported");
            }
            assert_eq!(result["num_turns"], if followup { 1 } else { 3 });
            assert_eq!(
                result["permission_denials"].as_array().unwrap().len(),
                if followup { 0 } else { 2 }
            );
        }
        assert_eq!(fixture.requests.load(Ordering::SeqCst), 4);
        server.abort();
    }
}
