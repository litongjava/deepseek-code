//! A real local OpenAI Responses protocol mock. Repeated raw provider call ids
//! must stay distinct executions while the outgoing `function_call` /
//! `function_call_output` items remain a valid pair.
use super::*;

fn responses_provider(base: &str) -> ResolvedProvider {
    crate::config::OrchestrationConfig::init_global().unwrap();
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "openai-fixture", "name": "openai-fixture",
            "base_url": format!("{base}/v1"), "api_key": "fixture-only",
            "api_format": "openai", "builtin_id": "openai", "enabled": true, "models": []
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "context_window": 8000, "enabled": true,
            "api": "openai-responses", "supports_web_search": false, "max_tokens": 256
        }))
        .unwrap(),
    }
}

#[derive(Debug, Clone)]
struct Done {
    name: String,
    id: String,
    input: Value,
    output: String,
    is_error: bool,
}

/// One Responses turn that calls `name` with `input` under the raw `call_id`.
fn call_turn(call_id: &str, name: &str, input: Value) -> Vec<Value> {
    let item = json!({
        "type": "function_call", "id": format!("fc-{call_id}"), "call_id": call_id,
        "name": name, "arguments": input.to_string()
    });
    vec![
        json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":call_id,"name":name}}),
        json!({"type":"response.output_item.done","item":item.clone()}),
        json!({"type":"response.completed","response":{"id":"resp","model":"fixture","status":"completed",
            "output":[item],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
    ]
}

/// One Responses turn with two calls in the same response.
fn two_call_turn(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let items: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, name, input))| {
            json!({"type":"function_call","id":format!("fc-{index}"),"call_id":call_id,
                "name":name,"arguments":input.to_string()})
        })
        .collect();
    let mut events = Vec::new();
    for item in &items {
        events.push(json!({"type":"response.output_item.done","item":item.clone()}));
    }
    events.push(
        json!({"type":"response.completed","response":{"id":"resp","model":"fixture",
        "status":"completed","output":items,
        "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
    );
    events
}

fn text_turn(text: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.completed","response":{"id":"resp","model":"fixture","status":"completed",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],
            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
    ]
}

/// Capture every Responses request body and reply with the next scripted turn.
async fn run_responses(
    responses: Vec<Vec<Value>>,
    ask_permissions: bool,
) -> (Vec<Done>, Vec<Value>) {
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
                        .replace("__SECOND__", &second)
                        .replace("__MISSING__", &format!("{first}.missing"));
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
                let stream = events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>();
                ([("content-type", "text/event-stream")], stream)
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut dones = Vec::<Done>::new();
        while let Some(event) = rx.recv().await {
            if let EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
            } = event
            {
                dones.push(Done {
                    name: tool_name,
                    id: tool_use_id,
                    input: tool_input,
                    output,
                    is_error,
                });
            }
        }
        dones
    });
    let mut engine = ToolLoopExecutor::new(
        responses_provider(&base),
        vec![json!({"role":"user","content":"use the fixture calls"})],
        None,
        256,
        tx,
        dir.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_permission_host(false);
    if ask_permissions {
        let manager = crate::runtime::default_permissions();
        manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
        engine = engine.with_permission_manager(manager);
    }
    let result = engine.execute().await.unwrap();
    assert!(result.0.contains("DONE"), "{:?}", result.0);
    drop(engine);
    let mut dones = drain.await.unwrap();
    server.abort();
    for done in &mut dones {
        if let Some(path) = done.input["file_path"].as_str() {
            if path == first {
                done.input["file_path"] = json!("first");
            } else if path == second {
                done.input["file_path"] = json!("second");
            }
        }
    }
    let requests = requests.lock().unwrap().clone();
    (dones, requests)
}

/// Every `function_call` must have exactly one `function_call_output` with the
/// same id, and no output may be orphaned.
fn assert_valid_pairs(body: &Value) {
    let mut calls = std::collections::BTreeMap::<String, usize>::new();
    let mut outputs = std::collections::BTreeMap::<String, usize>::new();
    for item in body["input"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("function_call") => {
                *calls
                    .entry(item["call_id"].as_str().unwrap_or_default().to_owned())
                    .or_default() += 1;
            }
            Some("function_call_output") => {
                *outputs
                    .entry(item["call_id"].as_str().unwrap_or_default().to_owned())
                    .or_default() += 1;
            }
            _ => {}
        }
    }
    assert_eq!(calls, outputs, "unpaired Responses items: {body}");
    for (id, count) in &calls {
        assert_eq!(
            outputs.get(id),
            Some(count),
            "call {id} is not paired once per execution: {body}"
        );
    }
}

fn call_ids(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call")
        .map(|item| item["call_id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn output_ids(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["call_id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The durable rows a real CLI/Desktop turn would save: keyed by the occurrence
/// identity, never by the provider's reused raw id.
fn persist(dones: &[Done]) -> Vec<(String, String, String, String, bool)> {
    let temp = tempfile::tempdir().unwrap();
    let context = crate::services::session_messages::context(temp.path()).unwrap();
    let store = crate::runtime::SessionStore::new(context.db.clone());
    let session = store.create(temp.path(), "fixture", "fixture").unwrap();
    let assistant = store.append(&session, "assistant", "").unwrap();
    context
        .db
        .with_conn(|conn| -> anyhow::Result<()> {
            for (index, done) in dones.iter().enumerate() {
                crate::db::message_repo::insert_tool_call(
                    conn,
                    &format!("{assistant}:{}", done.id),
                    &assistant,
                    &done.name,
                    Some(&done.input.to_string()),
                    Some(&done.output),
                    done.is_error,
                    index as i64,
                )?;
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
    let rows = context
        .db
        .with_conn(|conn| crate::db::message_repo::get_tool_calls_for_message(conn, &assistant))
        .unwrap()
        .unwrap();
    rows.into_iter()
        .map(|row| {
            (
                row.id,
                row.name,
                row.input.unwrap_or_default(),
                row.output.unwrap_or_default(),
                row.is_error,
            )
        })
        .collect()
}

#[tokio::test]
async fn responses_reused_raw_ids_keep_distinct_executions_and_valid_pairs() {
    let responses = vec![
        call_turn("same", "Read", json!({"file_path":"__FIRST__"})),
        call_turn("same", "Read", json!({"file_path":"__SECOND__"})),
        text_turn("DONE"),
    ];
    let (dones, requests) = run_responses(responses, false).await;
    assert_eq!(dones.len(), 2);
    assert_ne!(dones[0].id, dones[1].id);
    let mut paths: Vec<_> = dones
        .iter()
        .map(|done| done.input["file_path"].as_str().unwrap().to_owned())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["first", "second"]);
    // The first follow-up carries one pair; the last carries both turns.
    assert_valid_pairs(&requests[1]);
    assert_eq!(call_ids(&requests[1]), vec!["same"]);
    assert_eq!(output_ids(&requests[1]), vec!["same"]);
    assert_valid_pairs(&requests[2]);
    assert_eq!(call_ids(&requests[2]), vec!["same", "same"]);
    assert_eq!(output_ids(&requests[2]), vec!["same", "same"]);
    let rows = persist(&dones);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].2, r#"{"file_path":"first"}"#);
    assert_eq!(rows[1].2, r#"{"file_path":"second"}"#);
    assert!(rows[0].3.contains("FIRST_RESULT"));
    assert!(rows[1].3.contains("SECOND_RESULT"));
}

#[tokio::test]
async fn responses_reused_raw_id_for_a_different_tool_is_a_new_execution() {
    let responses = vec![
        call_turn("same", "Read", json!({"file_path":"__FIRST__"})),
        call_turn("same", "Grep", json!({"pattern":"FIRST_RESULT","path":"."})),
        text_turn("DONE"),
    ];
    let (dones, requests) = run_responses(responses, false).await;
    assert_eq!(dones.len(), 2);
    assert_ne!(dones[0].id, dones[1].id);
    let mut names: Vec<_> = dones.iter().map(|done| done.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["Grep", "Read"]);
    assert_valid_pairs(&requests[2]);
    assert_eq!(call_ids(&requests[2]), vec!["same", "same"]);
    let rows = persist(&dones);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.1 == "Grep"));
    assert!(rows.iter().any(|row| row.1 == "Read"));
}

#[tokio::test]
async fn responses_identical_duplicate_raw_ids_in_one_response_are_canonicalized() {
    // Two calls in the same response, same tool and identical arguments, and
    // the provider repeats one raw id. Responses replays its native items, so
    // the assistant items must be rewritten too or one result would be orphaned.
    let input = json!({"file_path":"__FIRST__"});
    let responses = vec![
        two_call_turn(&[("dup", "Read", input.clone()), ("dup", "Read", input)]),
        text_turn("DONE"),
    ];
    let (dones, requests) = run_responses(responses, false).await;
    assert_eq!(dones.len(), 2);
    assert_ne!(dones[0].id, dones[1].id);
    assert_eq!(dones[0].input, dones[1].input);
    assert_eq!(dones[0].output, dones[1].output);
    let follow_up = &requests[1];
    assert_valid_pairs(follow_up);
    let calls = call_ids(follow_up);
    assert_eq!(calls.len(), 2);
    assert!(calls.contains(&"dup".to_owned()), "{calls:?}");
    assert!(calls.iter().any(|id| id == "dup__dup1"), "{calls:?}");
    let mut outputs = output_ids(follow_up);
    outputs.sort();
    let mut expected = calls.clone();
    expected.sort();
    assert_eq!(outputs, expected, "{follow_up}");
    let rows = persist(&dones);
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].0, rows[1].0);
    assert_eq!(rows[0].2, rows[1].2);
    assert_eq!(rows[0].3, rows[1].3);
}

#[tokio::test]
async fn responses_denial_and_failure_keep_outcomes_and_pairing() {
    let responses = vec![
        // Outside the workspace, so permission denies it without a host.
        call_turn(
            "same",
            "Write",
            json!({"file_path":"../outside.txt","content":"must not execute"}),
        ),
        // A missing file is an ordinary tool failure.
        call_turn("same", "Read", json!({"file_path":"__MISSING__"})),
        text_turn("DONE"),
    ];
    let (dones, requests) = run_responses(responses, true).await;
    assert_eq!(dones.len(), 2);
    assert_ne!(dones[0].id, dones[1].id);
    assert!(dones.iter().all(|done| done.is_error), "{dones:?}");
    assert!(dones.iter().all(|done| !done.output.is_empty()));
    let denied = dones[0].output.to_lowercase();
    assert!(
        denied.contains("denied") || denied.contains("not granted"),
        "{:?}",
        dones[0]
    );
    assert_valid_pairs(&requests[2]);
    let rows = persist(&dones);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.4));
    assert!(rows[0].2.contains("outside.txt"));
    assert!(rows[1].2.contains("missing"));
}
