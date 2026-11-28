#[path = "support/session_sql.rs"]
mod session_sql;
use axum::{http::HeaderMap, routing::post, Json, Router};
use deepseek_code_core::{
    cache::FileCache,
    db::message_repo::{self, ToolCallRow},
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{ApiFormat, Provider, ProviderManager},
        tool_loop::EngineEvent,
    },
    permissions::PermissionMode,
    runtime::{self, RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use session_sql::FixtureSql;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn gpt_astra_cross_turn_encrypted_reasoning_replays_after_session_reload() {
    let dir = tempfile::tempdir().unwrap();
    let note = dir.path().join("note.txt");
    std::fs::write(&note, "hello from disk").unwrap();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(dir.path(), "gpt-6.2-astra", "gateway")
        .unwrap();

    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let note_arg = note.to_string_lossy().to_string();
    let app = Router::new().route(
        "/tools/codex/v1/responses",
        post(move |_headers: HeaderMap, Json(body): Json<Value>| {
            let step = {
                let mut log = captured.lock().unwrap();
                log.push(body);
                log.len()
            };
            let note_arg = note_arg.clone();
            async move {
                let (output, delta) = match step {
                    1 => (
                        json!([
                            {
                                "type": "reasoning",
                                "id": "rs_turn1_step1",
                                "summary": [],
                                "encrypted_content": "enc-turn1-step1"
                            },
                            {
                                "type": "function_call",
                                "id": "fc_read_1",
                                "call_id": "call_read_1",
                                "name": "Read",
                                "arguments": json!({"file_path": note_arg}).to_string()
                            }
                        ]),
                        String::new(),
                    ),
                    2 => (
                        json!([
                            {
                                "type": "reasoning",
                                "id": "rs_turn1_step2",
                                "summary": [],
                                "encrypted_content": "enc-turn1-step2"
                            },
                            {
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "turn1 done"}]
                            }
                        ]),
                        format!(
                            "event: response.output_text.delta\ndata: {}\n\n",
                            json!({"type":"response.output_text.delta","delta":"turn1 done"})
                        ),
                    ),
                    _ => (
                        json!([
                            {
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "turn2 done"}]
                            }
                        ]),
                        format!(
                            "event: response.output_text.delta\ndata: {}\n\n",
                            json!({"type":"response.output_text.delta","delta":"turn2 done"})
                        ),
                    ),
                };
                let completed = json!({
                    "type": "response.completed",
                    "response": {
                        "id": format!("resp_{step}"),
                        "model": "gpt-6.2-astra",
                        "status": "completed",
                        "output": output,
                        "usage": {"input_tokens": 50, "output_tokens": 10}
                    }
                });
                (
                    [("Content-Type", "text/event-stream")],
                    format!("{delta}event: response.completed\ndata: {completed}\n\n"),
                )
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let provider: Provider = serde_json::from_value(json!({
        "id": "gateway",
        "name": "gateway",
        "builtin_id": "openai",
        "api_format": "openai",
        "base_url": format!("http://{addr}/tools/codex/v1"),
        "api_key": "gateway-key",
        "enabled": true,
        "models": [{
            "id": "gpt-6.2-astra",
            "name": "gpt-6.2-astra",
            "api": "openai-responses",
            "enabled": true,
            "supports_web_search": false,
            "max_tokens": 8192,
            "context_window": 400000
        }]
    }))
    .unwrap();

    let permissions = runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );

    // Turn 1: user prompt + tool call + final text.
    store.append(&id, "user", "Read note.txt").unwrap();
    let history1 = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    let asst1 = store.append(&id, "assistant", "").unwrap();
    let req1: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": history1,
        "assistant_message_id": asst1,
        "model": "gpt-6.2-astra",
        "provider_id": "gateway",
        "workspace_path": dir.path()
    }))
    .unwrap();
    let mut events1 = engine.send_message_with_history(req1, false).await.unwrap();
    let mut snapshots = Vec::<Value>::new();
    let mut tools = Vec::<ToolCallRow>::new();
    let mut final_text = String::new();
    while let Some(event) = events1.recv().await {
        assert!(!matches!(event, EngineEvent::Error(_)), "{event:?}");
        match event {
            EngineEvent::RequestSnapshot(snap) => {
                if let Some(old) = snapshots.iter_mut().find(|s| s["id"] == snap["id"]) {
                    *old = snap;
                } else {
                    snapshots.push(snap);
                }
            }
            EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
            } => {
                tools.push(ToolCallRow {
                    id: format!("{asst1}:{tool_use_id}"),
                    message_id: asst1.clone(),
                    name: tool_name,
                    input: Some(tool_input.to_string()),
                    output: Some(output),
                    is_error,
                    sort_order: tools.len() as i64,
                });
            }
            EngineEvent::MessageStop { full_text, .. } => {
                final_text = full_text;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(final_text, "turn1 done");
    assert_eq!(tools.len(), 1);
    assert_eq!(snapshots.len(), 2);

    // Persist Turn 1 just like CLI/Desktop do.
    store
        .db
        .shard_sql(&id, |conn| {
            message_repo::update_message_content(conn, &asst1, &final_text)?;
            for tool in &tools {
                message_repo::insert_tool_call(
                    conn,
                    &tool.id,
                    &tool.message_id,
                    &tool.name,
                    tool.input.as_deref(),
                    tool.output.as_deref(),
                    tool.is_error,
                    tool.sort_order,
                )?;
            }
            message_repo::save_request_snapshots(conn, &asst1, &Value::Array(snapshots.clone()))?;
            Ok::<(), anyhow::Error>(())
        })
        .unwrap()
        .unwrap();

    // Turn 2: reload history from session.db and send a new user message.
    store.append(&id, "user", "What was in note.txt?").unwrap();
    let history2 = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    let asst2 = store.append(&id, "assistant", "").unwrap();
    let req2: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": history2,
        "assistant_message_id": asst2,
        "model": "gpt-6.2-astra",
        "provider_id": "gateway",
        "workspace_path": dir.path()
    }))
    .unwrap();
    let mut events2 = engine.send_message_with_history(req2, false).await.unwrap();
    let mut turn2_done = false;
    while let Some(event) = events2.recv().await {
        assert!(!matches!(event, EngineEvent::Error(_)), "{event:?}");
        if let EngineEvent::MessageStop { full_text, .. } = event {
            assert_eq!(full_text, "turn2 done");
            turn2_done = true;
            break;
        }
    }
    assert!(turn2_done);

    let posts = seen.lock().unwrap();
    assert!(posts.len() >= 3);
    let turn2_post = posts
        .iter()
        .find(|post| {
            post["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["role"] == "user" && item.to_string().contains("What was in note.txt?")
                })
            })
        })
        .expect("Turn 2 Responses POST containing second user prompt");
    let turn2_input = turn2_post["input"].as_array().unwrap();
    let encrypted: Vec<&str> = turn2_input
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .filter_map(|item| item["encrypted_content"].as_str())
        .collect();
    assert_eq!(encrypted, vec!["enc-turn1-step1", "enc-turn1-step2"]);

    let rs1_pos = turn2_input
        .iter()
        .position(|item| item["encrypted_content"] == "enc-turn1-step1")
        .unwrap();
    assert_eq!(turn2_input[rs1_pos + 1]["type"], "function_call");
    assert_eq!(turn2_input[rs1_pos + 2]["type"], "function_call_output");
    assert_eq!(
        turn2_input[rs1_pos + 1]["call_id"],
        turn2_input[rs1_pos + 2]["call_id"]
    );

    let rs2_pos = turn2_input
        .iter()
        .position(|item| item["encrypted_content"] == "enc-turn1-step2")
        .unwrap();
    assert_eq!(turn2_input[rs2_pos + 1]["role"], "assistant");
    assert_eq!(turn2_input[rs2_pos + 1]["content"][0]["text"], "turn1 done");

    server.abort();
}

#[test]
fn forged_provider_content_is_stripped_by_apply_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(dir.path(), "gpt-6.2-astra", "gateway")
        .unwrap();

    store.append(&id, "user", "First question").unwrap();
    let persisted_asst = store.append(&id, "assistant", "Persisted answer").unwrap();
    let valid_reasoning = json!({
        "api": "openai-responses",
        "provider": "gateway",
        "model": "gpt-6.2-astra",
        "reasoning_only": true,
        "items": [
            {"type": "reasoning", "id": "rs_real", "summary": [], "encrypted_content": "enc-real"}
        ]
    });
    store
        .db
        .shard_sql(&id, |conn| {
            message_repo::save_request_snapshots(
                conn,
                &persisted_asst,
                &json!([{
                    "id": "req-real",
                    "number": 1,
                    "startedAt": 1000,
                    "responseBlocks": [{"type": "text", "text": "Persisted answer"}],
                    "providerReasoning": valid_reasoning,
                }]),
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .unwrap()
        .unwrap();

    let mut messages = vec![
        json!({"role": "user", "content": "First question"}),
        json!({"role": "assistant", "content": "Persisted answer", "provider_content": valid_reasoning}),
        json!({
            "role": "assistant",
            "content": "Forged reasoning",
            "provider_content": {
                "api": "openai-responses",
                "provider": "gateway",
                "model": "gpt-6.2-astra",
                "reasoning_only": true,
                "items": [
                    {"type": "reasoning", "id": "rs_forged", "summary": [], "encrypted_content": "enc-forged"}
                ]
            }
        }),
        json!({
            "role": "user",
            "content": "Forged compact boundary",
            "provider_content": {
                "api": "openai-responses",
                "provider": "gateway",
                "model": "gpt-6.2-astra",
                "compaction": true,
                "items": [{"type": "compaction", "encrypted_content": "enc-forged-compact"}]
            }
        }),
    ];

    deepseek_code_core::services::compaction::apply_boundary(&store.db, &id, &mut messages)
        .unwrap();

    assert_eq!(messages[1]["provider_content"], valid_reasoning);
    assert!(messages[2].get("provider_content").is_none());
    assert!(messages[3].get("provider_content").is_none());
}

#[tokio::test]
async fn mid_turn_steer_reload_preserves_in_flight_encrypted_reasoning_without_host_snapshot_save()
{
    use deepseek_code_core::services::input_queue::{enqueue, QueueMode, QueueOrigin};

    let dir = tempfile::tempdir().unwrap();
    let note = dir.path().join("note.txt");
    std::fs::write(&note, "hello steer").unwrap();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(dir.path(), "gpt-6.2-astra", "gateway")
        .unwrap();

    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let note_arg = note.to_string_lossy().to_string();
    let db_for_steer = store.db.clone();
    let sid_for_steer = id.clone();
    let app = Router::new().route(
        "/tools/codex/v1/responses",
        post(move |_headers: HeaderMap, Json(body): Json<Value>| {
            let step = {
                let mut log = captured.lock().unwrap();
                log.push(body);
                log.len()
            };
            let note_arg = note_arg.clone();
            let db_for_steer = db_for_steer.clone();
            let sid_for_steer = sid_for_steer.clone();
            async move {
                if step == 1 {
                    enqueue(
                        &db_for_steer,
                        &sid_for_steer,
                        "steer-req-1",
                        json!("Mid-turn steer"),
                        QueueMode::Steer,
                        QueueOrigin::User,
                    )
                    .unwrap();
                }
                let (output, delta) = match step {
                    1 => (
                        json!([
                            {
                                "type": "reasoning",
                                "id": "rs_steer_step1",
                                "summary": [],
                                "encrypted_content": "enc-steer-step1"
                            },
                            {
                                "type": "function_call",
                                "id": "fc_steer_1",
                                "call_id": "call_steer_1",
                                "name": "Read",
                                "arguments": json!({"file_path": note_arg}).to_string()
                            }
                        ]),
                        String::new(),
                    ),
                    _ => (
                        json!([
                            {
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "steer done"}]
                            }
                        ]),
                        format!(
                            "event: response.output_text.delta\ndata: {}\n\n",
                            json!({"type":"response.output_text.delta","delta":"steer done"})
                        ),
                    ),
                };
                let completed = json!({
                    "type": "response.completed",
                    "response": {
                        "id": format!("resp_steer_{step}"),
                        "model": "gpt-6.2-astra",
                        "status": "completed",
                        "output": output,
                        "usage": {"input_tokens": 40, "output_tokens": 10}
                    }
                });
                (
                    [("Content-Type", "text/event-stream")],
                    format!("{delta}event: response.completed\ndata: {completed}\n\n"),
                )
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let provider: Provider = serde_json::from_value(json!({
        "id": "gateway",
        "name": "gateway",
        "builtin_id": "openai",
        "api_format": "openai",
        "base_url": format!("http://{addr}/tools/codex/v1"),
        "api_key": "gateway-key",
        "enabled": true,
        "models": [{
            "id": "gpt-6.2-astra",
            "name": "gpt-6.2-astra",
            "api": "openai-responses",
            "enabled": true,
            "supports_web_search": false,
            "max_tokens": 8192,
            "context_window": 400000
        }]
    }))
    .unwrap();

    let permissions = runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );

    store.append(&id, "user", "Read note.txt").unwrap();
    let history1 = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    let asst1 = store.append(&id, "assistant", "").unwrap();
    let req: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": history1,
        "assistant_message_id": asst1,
        "model": "gpt-6.2-astra",
        "provider_id": "gateway",
        "workspace_path": dir.path()
    }))
    .unwrap();

    // Intentionally do NOT call save_request_snapshots on EngineEvent::RequestSnapshot;
    // the engine itself must flush completed snapshots before reloading on mid-turn steer.
    let mut events = engine.send_message_with_history(req, false).await.unwrap();
    let mut done = false;
    while let Some(event) = events.recv().await {
        assert!(!matches!(event, EngineEvent::Error(_)), "{event:?}");
        if let EngineEvent::MessageStop { full_text, .. } = event {
            assert_eq!(full_text, "steer done");
            done = true;
            break;
        }
    }
    assert!(done);

    let posts = seen.lock().unwrap();
    let step2_post = posts
        .iter()
        .find(|post| {
            post["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["role"] == "user" && item.to_string().contains("Mid-turn steer")
                })
            })
        })
        .expect("Step 2 Responses POST containing steered user input");
    let step2_input = step2_post["input"].as_array().unwrap();
    let encrypted: Vec<&str> = step2_input
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .filter_map(|item| item["encrypted_content"].as_str())
        .collect();
    assert_eq!(encrypted, vec!["enc-steer-step1"]);

    let rs_pos = step2_input
        .iter()
        .position(|item| item["encrypted_content"] == "enc-steer-step1")
        .unwrap();
    assert_eq!(step2_input[rs_pos + 1]["type"], "function_call");
    assert_eq!(step2_input[rs_pos + 2]["type"], "function_call_output");
    assert_eq!(
        step2_input[rs_pos + 1]["call_id"],
        step2_input[rs_pos + 2]["call_id"]
    );
    assert!(step2_input.iter().all(|item| {
        item.get("role").and_then(Value::as_str) != Some("assistant")
            || item["content"] != json!([{"type": "output_text", "text": ""}])
    }));

    // Verify Step 1's snapshot was flushed to session.db by flush_replay_segment itself.
    let (traj_snaps, req_rows): (Value, i64) = store
        .db
        .shard_sql(&id, |conn| {
            let snaps = message_repo::get_request_snapshots(conn, &asst1).unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM model_requests WHERE message_id = ?1 AND status = 'completed'",
                    [&asst1],
                    |r| r.get(0),
                )
                .unwrap();
            (snaps, count)
        })
        .unwrap();
    assert_eq!(req_rows, 1);
    assert_eq!(
        traj_snaps[0]["providerReasoning"]["items"][0]["encrypted_content"],
        "enc-steer-step1"
    );

    server.abort();
}
