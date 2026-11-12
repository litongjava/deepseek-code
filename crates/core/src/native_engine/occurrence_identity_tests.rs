//! Every distinct execution keeps its own identity even when a provider
//! repeats a raw tool call id across responses or within one response.
use super::*;

fn provider(base: &str, anthropic: bool) -> ResolvedProvider {
    crate::config::OrchestrationConfig::init_global().unwrap();
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"occurrence-test","name":"occurrence-test","base_url":base,
            "api_key":"fixture-only","api_format":if anthropic {"anthropic"} else {"openai"},"enabled":true,"models":[]})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","context_window":8000,"enabled":true,"supports_web_search":false})).unwrap(),
    }
}

#[derive(Debug, Clone)]
struct Observed {
    name: String,
    id: String,
    input: Value,
}

async fn run_fixture(anthropic: bool, responses: Vec<Vec<Value>>) -> (Vec<Observed>, Vec<Value>) {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    std::fs::write(&first, "FIRST_RESULT\n").unwrap();
    std::fs::write(&second, "SECOND_RESULT\n").unwrap();
    let first = first.to_string_lossy().into_owned();
    let second = second.to_string_lossy().into_owned();
    let responses = responses
        .into_iter()
        .map(|events| {
            events
                .into_iter()
                .map(|event| {
                    let text = event
                        .to_string()
                        .replace("__FIRST__", &first)
                        .replace("__SECOND__", &second);
                    serde_json::from_str::<Value>(&text).unwrap()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let attempt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responses = Arc::new(responses);
    let app = axum::Router::new().fallback(axum::routing::post(
        move |axum::Json(body): axum::Json<Value>| {
            let captured = captured.clone();
            let attempt = attempt.clone();
            let responses = responses.clone();
            async move {
                let index = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                captured.lock().unwrap().push(body);
                let events = responses
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| responses.last().cloned().unwrap_or_default());
                let mut stream = events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>();
                if !anthropic {
                    stream.push_str("data: [DONE]\n\n");
                }
                ([("content-type", "text/event-stream")], stream)
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut dones = Vec::<Observed>::new();
        while let Some(event) = rx.recv().await {
            if let EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                ..
            } = event
            {
                dones.push(Observed {
                    name: tool_name,
                    id: tool_use_id,
                    input: tool_input,
                });
            }
        }
        dones
    });
    let mut engine = ToolLoopExecutor::new(
        provider(&base, anthropic),
        vec![json!({"role":"user","content":"read both fixtures"})],
        None,
        256,
        tx,
        dir.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    );
    let result = engine.execute().await.unwrap();
    assert!(result.0.contains("DONE"), "{:?}", result.0);
    drop(engine);
    let mut dones = drain.await.unwrap();
    server.abort();
    for row in &mut dones {
        if let Some(path) = row.input["file_path"].as_str() {
            if path == first {
                row.input["file_path"] = json!("first");
            } else if path == second {
                row.input["file_path"] = json!("second");
            }
        }
    }
    let requests = requests.lock().unwrap().clone();
    (dones, requests)
}

fn openai_tool_call(id: &str, name: &str, input: Value) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
}

fn openai_two_tool_calls(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, input))| {
            json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})
        })
        .collect();
    vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":tool_calls},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
}

fn openai_text(text: &str) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]
}

fn anthropic_tool_use(id: &str, name: &str, input: Value) -> Vec<Value> {
    vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":input}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
    ]
}

fn anthropic_two_tool_uses(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let mut events = Vec::new();
    for (index, (id, name, input)) in calls.iter().enumerate() {
        events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":input}}));
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}));
    events
}

fn anthropic_text(text: &str) -> Vec<Value> {
    vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
    ]
}

fn tool_pairs(request: &Value, anthropic: bool, raw: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for message in request["messages"].as_array().into_iter().flatten() {
        if anthropic {
            for block in message["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use" {
                    pairs.push((
                        block["id"].as_str().unwrap_or_default().to_owned(),
                        block["name"].as_str().unwrap_or_default().to_owned(),
                    ));
                }
            }
        } else {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                pairs.push((
                    call["id"].as_str().unwrap_or_default().to_owned(),
                    call["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }
        }
    }
    pairs.retain(|(id, _)| id == raw || id.starts_with(&format!("{raw}__dup")));
    pairs
}

fn result_ids(request: &Value, anthropic: bool) -> Vec<String> {
    if anthropic {
        request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .filter(|block| block["type"] == "tool_result")
            .map(|block| block["tool_use_id"].as_str().unwrap_or_default().to_owned())
            .collect()
    } else {
        request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| message["role"] == "tool")
            .map(|message| {
                message["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }
}

#[tokio::test]
async fn reused_raw_id_across_responses_keeps_two_occurrences_and_pairs_protocol() {
    for anthropic in [false, true] {
        let responses = if anthropic {
            vec![
                anthropic_tool_use("same", "Read", json!({"file_path":"__FIRST__"})),
                anthropic_tool_use("same", "Read", json!({"file_path":"__SECOND__"})),
                anthropic_text("DONE"),
            ]
        } else {
            vec![
                openai_tool_call("same", "Read", json!({"file_path":"__FIRST__"})),
                openai_tool_call("same", "Read", json!({"file_path":"__SECOND__"})),
                openai_text("DONE"),
            ]
        };
        let (dones, requests) = run_fixture(anthropic, responses).await;
        assert_eq!(dones.len(), 2, "anthropic={anthropic}");
        assert_ne!(dones[0].id, dones[1].id, "anthropic={anthropic}");
        let mut inputs: Vec<_> = dones
            .iter()
            .map(|row| {
                row.input["file_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        inputs.sort();
        assert_eq!(inputs, vec!["first", "second"], "anthropic={anthropic}");
        // The provider keeps the raw id on both sides of each pair in both
        // rounds; the second round carries both completed pairs.
        let first_round = tool_pairs(&requests[1], anthropic, "same");
        assert!(
            !first_round.is_empty(),
            "anthropic={anthropic} {}",
            requests[1]
        );
        assert!(first_round.iter().all(|(id, _)| id == "same"));
        let third = &requests[2];
        let raw_ids = tool_pairs(third, anthropic, "same");
        assert_eq!(raw_ids.len(), 2, "anthropic={anthropic} {third}");
        assert!(raw_ids.iter().all(|(id, _)| id == "same"));
        let result_ids = result_ids(third, anthropic);
        assert_eq!(result_ids.len(), 2, "anthropic={anthropic} {third}");
        assert!(result_ids.iter().all(|id| id == "same"));
    }
}

#[tokio::test]
async fn reused_raw_id_for_a_different_tool_is_a_new_occurrence() {
    for anthropic in [false, true] {
        let responses = if anthropic {
            vec![
                anthropic_tool_use("same", "Read", json!({"file_path":"__FIRST__"})),
                anthropic_tool_use("same", "Grep", json!({"pattern":"FIRST_RESULT","path":"."})),
                anthropic_text("DONE"),
            ]
        } else {
            vec![
                openai_tool_call("same", "Read", json!({"file_path":"__FIRST__"})),
                openai_tool_call("same", "Grep", json!({"pattern":"FIRST_RESULT","path":"."})),
                openai_text("DONE"),
            ]
        };
        let (dones, _) = run_fixture(anthropic, responses).await;
        assert_eq!(dones.len(), 2, "anthropic={anthropic}");
        assert_ne!(dones[0].id, dones[1].id, "anthropic={anthropic}");
        let mut names: Vec<_> = dones.iter().map(|row| row.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["Grep", "Read"], "anthropic={anthropic}");
    }
}

#[tokio::test]
async fn duplicate_raw_ids_in_one_response_are_canonicalized_and_paired() {
    for anthropic in [false, true] {
        let calls = [
            ("dup", "Read", json!({"file_path":"__FIRST__"})),
            ("dup", "Read", json!({"file_path":"__SECOND__"})),
        ];
        let responses = if anthropic {
            vec![anthropic_two_tool_uses(&calls), anthropic_text("DONE")]
        } else {
            vec![openai_two_tool_calls(&calls), openai_text("DONE")]
        };
        let (dones, requests) = run_fixture(anthropic, responses).await;
        assert_eq!(dones.len(), 2, "anthropic={anthropic}");
        assert_ne!(dones[0].id, dones[1].id, "anthropic={anthropic}");
        let mut inputs: Vec<_> = dones
            .iter()
            .map(|row| {
                row.input["file_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        inputs.sort();
        assert_eq!(inputs, vec!["first", "second"], "anthropic={anthropic}");
        // Both sides of the outgoing protocol use the canonical ids, so no
        // result is dropped or crossed even though the provider repeated one.
        let second = &requests[1];
        let pairs = tool_pairs(second, anthropic, "dup");
        assert_eq!(pairs.len(), 2, "anthropic={anthropic} {second}");
        let ids: Vec<_> = pairs.iter().map(|(id, _)| id.clone()).collect();
        assert!(ids.contains(&"dup".to_owned()), "{ids:?}");
        assert!(ids.iter().any(|id| id == "dup__dup1"), "{ids:?}");
        let results = result_ids(second, anthropic);
        assert_eq!(results.len(), 2, "anthropic={anthropic} {second}");
        assert!(results.contains(&"dup".to_owned()), "{results:?}");
        assert!(results.iter().any(|id| id == "dup__dup1"), "{results:?}");
    }
}

#[test]
fn each_executor_and_each_call_gets_a_distinct_scope() {
    let (tx, _rx) = mpsc::channel(1);
    let mut first = ToolLoopExecutor::new(
        provider("http://127.0.0.1:1", false),
        Vec::new(),
        None,
        128,
        tx.clone(),
        ".".into(),
        ReasoningMode::Standard,
    );
    let mut second = ToolLoopExecutor::new(
        provider("http://127.0.0.1:1", false),
        Vec::new(),
        None,
        128,
        tx,
        ".".into(),
        ReasoningMode::Standard,
    );
    let a = first.next_invocation_id();
    let b = first.next_invocation_id();
    let c = second.next_invocation_id();
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
}

#[tokio::test]
async fn permission_owners_for_repeated_raw_ids_are_distinct() {
    use crate::native_engine::permission_response::{self, PermissionResponse};
    let temp = tempfile::tempdir().unwrap();
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
    let (tx, _rx) = mpsc::channel(8);
    let executor = ToolLoopExecutor::new(
        provider("http://127.0.0.1:1", false),
        Vec::new(),
        None,
        128,
        tx,
        temp.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("owner-fixture".into())
    .with_permission_manager(manager)
    .with_permission_host(true);
    let mailbox = executor.permission_mailbox.clone();
    let inspector = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let owners = permission_response::pending_owners("owner-fixture");
        assert_eq!(owners.len(), 2, "each occurrence owns one request");
        let ids: std::collections::HashSet<_> = owners
            .iter()
            .map(|(_, owner)| owner.tool_use_id.clone())
            .collect();
        assert_eq!(
            ids,
            ["inv-scope-1".to_owned(), "inv-scope-2".to_owned()]
                .into_iter()
                .collect()
        );
        assert!(owners
            .iter()
            .all(|(_, owner)| owner.session_id == "owner-fixture"));
        for (request_id, _) in &owners {
            permission_response::respond(
                &mailbox,
                request_id,
                PermissionResponse::Deny {
                    message: "fixture denial".into(),
                    interrupt: false,
                },
            )
            .await
            .unwrap();
        }
    });
    let first_input = json!({"command":"printf A"});
    let second_input = json!({"command":"printf B"});
    let first = executor.request_tool_approval_with("Bash", &first_input, "inv-scope-1");
    let second = executor.request_tool_approval_with("Bash", &second_input, "inv-scope-2");
    let (a, b) = tokio::join!(first, second);
    inspector.await.unwrap();
    assert!(a.is_err() && b.is_err());
}
