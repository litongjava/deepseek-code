use super::*;
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Recovery {
    anthropic: bool,
    main: Arc<AtomicUsize>,
    summaries: Arc<AtomicUsize>,
}

async fn respond(State(state): State<Recovery>, Json(body): Json<Value>) -> Response {
    if body["stream"] != true {
        state.summaries.fetch_add(1, Ordering::SeqCst);
        return Json(if state.anthropic {
            json!({"id":"summary","type":"message","role":"assistant","model":"fixture-model","content":[{"type":"text","text":"valid summary"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":2}})
        } else {
            json!({"id":"summary","object":"chat.completion","created":0,"model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"valid summary"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}})
        }).into_response();
    }
    let index = state.main.fetch_add(1, Ordering::SeqCst);
    if index == 1 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"prompt is too long"}}))).into_response();
    }
    let mut frames = Vec::new();
    if state.anthropic {
        frames.push(json!({"type":"message_start","message":{"id":"response","model":"fixture-model","role":"assistant","content":[],"usage":{"input_tokens":if index==0 {11} else {7},"output_tokens":0}}}));
        if index == 0 {
            frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"denied","name":"Bash","input":{}}}));
            frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"echo fixture\"}"}}));
        } else {
            frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"next reply"}}));
        }
        frames.push(json!({"type":"content_block_stop","index":0}));
        frames.push(json!({"type":"message_delta","delta":{"stop_reason":if index==0 {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":if index==0 {2} else {1}}}));
        frames.push(json!({"type":"message_stop"}));
    } else {
        let delta = if index == 0 {
            json!({"tool_calls":[{"index":0,"id":"denied","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo fixture\"}"}}]})
        } else {
            json!({"content":"next reply"})
        };
        frames.push(json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}));
        frames.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":if index==0 {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":if index==0 {11} else {7},"completion_tokens":if index==0 {2} else {1}}}));
    }
    let stream: String = frames
        .into_iter()
        .map(|f| {
            if state.anthropic {
                format!("event: {}\ndata: {f}\n\n", f["type"].as_str().unwrap())
            } else {
                format!("data: {f}\n\n")
            }
        })
        .collect();
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn sdk_interrupt_during_recovery_preserves_usage_and_committed_summary() {
    for anthropic in [false, true] {
        for committed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let paths = RuntimePaths::new(root.path().join("data")).unwrap();
            let store = SessionStore::new(paths.open_database().unwrap());
            let id = store
                .create(root.path(), "fixture-model", "fixture")
                .unwrap();
            store.append(&id, "user", "seed task").unwrap();
            store.append(&id, "assistant", "seed answer").unwrap();
            let state = Recovery {
                anthropic,
                main: Arc::default(),
                summaries: Arc::default(),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/v1/chat/completions", post(respond))
                .route("/v1/messages", post(respond))
                .with_state(state.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            std::fs::write(paths.data_dir.join("providers.json"), json!([{
                "id":"fixture","name":"fixture","base_url":address,"api_key":"fixture-only","api_format":if anthropic {"anthropic"} else {"openai"},"enabled":true,
                "models":[{"id":"fixture-model","name":"Fixture","enabled":true,"context_window":128000,"max_tokens":4096,"supports_tools":true,"supports_web_search":false,"supports_vision":false}]
            }]).to_string()).unwrap();
            let mut client = Client::start_configured(
                root.path(),
                &[
                    "--provider",
                    "fixture",
                    "--model",
                    "fixture-model",
                    "--resume",
                    &id,
                    "--disallowed-tools",
                    "Bash",
                ],
            );
            let hook = if committed {
                "PostCompact"
            } else {
                "PreCompact"
            };
            client
                .send(initialize_request(
                    "init",
                    json!({"hooks":{hook:[{"hookCallbackIds":["compact"]}]}}),
                ))
                .await;
            assert_eq!(
                client.frame("control_response").await["response"]["subtype"],
                "success"
            );
            client.send(user("Continue with recovery")).await;
            let pending = client.frame("control_request").await;
            assert_eq!(pending["request"]["callback_id"], "compact");
            assert_eq!(pending["request"]["input"]["hook_event_name"], hook);
            client.send(json!({"type":"control_request","request_id":"interrupt-recovery","request":{"subtype":"interrupt"}})).await;
            assert_eq!(
                client.frame("control_cancel_request").await["request_id"],
                pending["request_id"]
            );
            client.frame("control_response").await;
            let result = client.result().await;
            assert_eq!(
                result["turn"]["status"], "cancelled",
                "{anthropic} {committed}: {result}"
            );
            assert_eq!(result["num_turns"], 1);
            let (input, output) = if anthropic {
                ("input_tokens", "output_tokens")
            } else {
                ("prompt_tokens", "completion_tokens")
            };
            assert_eq!(result["usage"][input], 11, "{result}");
            assert_eq!(result["usage"][output], 2, "{result}");
            assert_eq!(result["permission_denials"].as_array().unwrap().len(), 1);
            assert_eq!(
                state.main.load(Ordering::SeqCst),
                2,
                "cancelled recovery must not retry"
            );
            assert_eq!(state.summaries.load(Ordering::SeqCst) > 0, committed);
            let boundaries = store.db.with_conn(|conn| conn.query_row("SELECT count(*) FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1", [&id], |row| row.get::<_, u64>(0))).unwrap().unwrap();
            assert_eq!(boundaries, u64::from(committed));
            assert_eq!(
                deepseek_code_core::services::compaction::count(&store.db, &id)
                    .unwrap()
                    .to_string(),
                if committed { "1" } else { "0" }
            );
            let exports = paths.data_dir.join("compactions").join(&id);
            assert_eq!(
                std::fs::read_dir(exports)
                    .map(|entries| entries.count())
                    .unwrap_or(0),
                usize::from(committed)
            );
            client.send(user("Next input")).await;
            let next = client.result().await;
            assert_eq!(next["turn"]["status"], "completed", "{next}");
            assert_eq!(next["usage"][input], 7);
            let (_, duplicates) = client.finish().await;
            assert!(duplicates.is_empty());
            server.abort();
        }
    }
}
