use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{ApiFormat, Provider, ProviderManager},
        token_counter::TokenCounter,
        NativeEngine,
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";

fn window() -> Value {
    json!([
        {"role":"user","content":[{"type":"input_text","text":"Finish 中文 tests"}]},
        {"type":"reasoning","id":"reasoning-id","encrypted_content":"exact signed reasoning"},
        {"type":"compaction","id":"compact-id","encrypted_content":"opaque exact content"}
    ])
}

async fn fixture(
    mode: u8,
) -> (
    tempfile::TempDir,
    SessionStore,
    String,
    NativeEngine,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    // ImageGenerate is not a default core tool; this fixture drives it directly.
    deepseek_code_core::services::customization::save(
        &paths.data_dir,
        &deepseek_code_core::services::customization::Customization {
            tool_loading: std::collections::BTreeMap::from([(
                "ImageGenerate".to_owned(),
                deepseek_code_core::services::customization::ToolLoading { enabled: true, on_demand: true },
            )]),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(dir.path(), "gpt-6-astra", "gateway").unwrap();
    store.append(&id, "user", "Finish 中文 tests").unwrap();
    store
        .append(&id, "assistant", "Tests remain pending")
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let app = Router::new().route("/tools/codex/v1/responses/compact", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let seen = captured.clone();
        async move {
            assert_eq!(headers["authorization"], "Bearer gateway-key");
            assert!(body.get("tools").is_none() && body.get("stream").is_none());
            seen.lock().unwrap().push(body);
            if mode == 2 { std::future::pending::<()>().await; }
            if mode == 1 { return (StatusCode::BAD_GATEWAY, Json(json!({"error":"incomplete"}))); }
            (StatusCode::OK, Json(json!({"object":"response.compaction","output":window(),"usage":{"input_tokens":150,"output_tokens":77}})))
        }
    }));
    let captured = seen.clone();
    let app = app.route("/tools/codex/v1/responses", post(move |Json(body): Json<Value>| {
        let step = captured.lock().unwrap().len();
        let first = step == 0;
        if mode == 4 {
            // The Responses adapter sends the same filtered list: the tool the
            // fixture enabled, never an on-demand one nobody loaded.
            let names: Vec<&str> = body["tools"].as_array().unwrap().iter().filter_map(|tool| tool["name"].as_str()).collect();
            assert!(names.contains(&"ImageGenerate"), "{names:?}");
            assert!(names.contains(&"Read"), "{names:?}");
            assert!(!names.contains(&"ListDir"), "{names:?}");
        }
        if mode == 5 {
            // Step 0 is the compaction exchange; the first turn request follows
            // it with no restored load, and the search adds the tool from the
            // next request on.
            let names: Vec<String> = body["tools"].as_array().map(|tools| tools.iter().filter_map(|tool| tool["name"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
            match step {
                1 => assert!(!names.contains(&"ListDir".to_owned()), "{names:?}"),
                _ => assert!(names.contains(&"ListDir".to_owned()), "{names:?}"),
            }
        }
        captured.lock().unwrap().push(body);
        async move {
            let call = |id: &str, name: &str, arguments: Value| json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":arguments.to_string()});
            let output = if mode == 5 {
                match step {
                    1 => json!([call("search-1", "ToolSearch", json!({"query":"ListDir"}))]),
                    2 => json!([call("list-1", "ListDir", json!({"path":"."}))]),
                    _ => json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]),
                }
            } else if mode == 4 && first {
                json!([{"type":"function_call","id":"fc-image","call_id":"call-image","name":"ImageGenerate","arguments":json!({"prompt":"Create an icon","file_path":"assets/图标.png"}).to_string()}])
            } else {
                json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}])
            };
            let data = json!({"type":"response.completed","response":{"id":"response","status":"completed","output":output,"usage":{"input_tokens":80,"output_tokens":1}}});
            let tool_step = (mode == 4 && first) || (mode == 5 && step <= 2);
            let delta = if tool_step { String::new() } else { format!("event: response.output_text.delta\ndata: {}\n\n", json!({"type":"response.output_text.delta","delta":"done"})) };
            ([("Content-Type", "text/event-stream")], format!("{delta}event: response.completed\ndata: {data}\n\n")).into_response()
        }
    }));
    let app = app.route(
        "/tools/codex/v1/images/generations",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer gateway-key");
            assert_eq!(body["model"], "gpt-image-2");
            Json(json!({"data":[{"b64_json":PNG}]}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider: Provider = serde_json::from_value(json!({"id":"gateway","name":"gateway","builtin_id":"openai","api_format":"openai","base_url":format!("http://{}/tools/codex/v1",listener.local_addr().unwrap()),"api_key":"gateway-key","enabled":true,"models":[{"id":"gpt-6-astra","name":"model","api":"openai-responses","enabled":true,"supports_web_search":false,"max_tokens":8192,"context_window":if mode == 3 {16000} else {256000}}]})).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let permissions = runtime::default_permissions();
    permissions.set_mode(deepseek_code_core::permissions::PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );
    (dir, store, id, engine, seen, server)
}

#[tokio::test]
async fn running_responses_turn_uses_native_compaction_before_local_truncation() {
    let (dir, store, id, engine, seen, server) = fixture(3).await;
    let original = "Detailed evidence ".repeat(2200);
    store.append(&id, "assistant", &original).unwrap();
    store.append(&id, "user", "Continue").unwrap();
    let request: ChatRequest = serde_json::from_value(json!({"conversation_id":id,"messages":store.model_history(&id,&ApiFormat::OpenAI).unwrap(),"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path(),"max_tokens":1000})).unwrap();
    let mut events = engine.send_message(request).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
        }
    })
    .await
    .unwrap();
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["input"].to_string().contains(&original));
    assert_eq!(
        &requests[1]["input"].as_array().unwrap()[..3],
        window().as_array().unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn model_invokes_image_tool_and_receives_persisted_actual_image() {
    let (dir, store, id, engine, seen, server) = fixture(4).await;
    let assistant_id = uuid::Uuid::new_v4().to_string();
    let request: ChatRequest = serde_json::from_value(json!({"conversation_id":id,"messages":store.model_history(&id,&ApiFormat::OpenAI).unwrap(),"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path()})).unwrap();
    let mut events = engine.send_message(request).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
            if let deepseek_code_core::native_engine::tool_loop::EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
            } = event
            {
                assert!(!is_error, "{output}");
                // Use the same host persistence entrypoint as CLI streaming results.
                store
                    .db
                    .with_conn(|conn| {
                        deepseek_code_core::db::message_repo::save_stream_tool_result(
                            conn,
                            &id,
                            &assistant_id,
                            "",
                            &deepseek_code_core::db::message_repo::ToolCallRow {
                                id: format!("{assistant_id}:{tool_use_id}"),
                                message_id: assistant_id.clone(),
                                name: tool_name,
                                input: Some(tool_input.to_string()),
                                output: Some(output),
                                is_error,
                                sort_order: 0,
                            },
                        )
                    })
                    .unwrap()
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert!(dir.path().join("assets/图标.png").exists());
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["input"].to_string().contains(PNG));
    assert!(store
        .model_history(&id, &ApiFormat::OpenAI)
        .unwrap()
        .iter()
        .any(|message| message.to_string().contains(PNG)));
    server.abort();
}

#[tokio::test]
async fn native_window_survives_reload_fork_and_actual_next_request_without_rewriting() {
    let (dir, store, id, engine, seen, server) = fixture(0).await;
    compaction::compact(
        &engine,
        &store.db,
        &id,
        "gateway",
        "gpt-6-astra",
        "Keep pending work",
    )
    .await
    .unwrap();
    let first = seen.lock().unwrap()[0].clone();
    assert_eq!(first["input"][0]["content"][0]["text"], "Finish 中文 tests");
    assert_eq!(first["instructions"], "Keep pending work");
    assert_eq!(store.history(&id).unwrap().len(), 3);
    store.append(&id, "user", "Continue").unwrap();
    let reopened = SessionStore::new(
        RuntimePaths::new(dir.path().join("data"))
            .unwrap()
            .open_database()
            .unwrap(),
    );
    let fork = reopened.fork(&id).unwrap();
    for session in [&id, &fork] {
        let history = reopened.model_history(session, &ApiFormat::OpenAI).unwrap();
        assert_eq!(history[0]["provider_content"]["items"], window());
        assert!(TokenCounter::estimate(&history) >= 77);
        let request: ChatRequest = serde_json::from_value(json!({"conversation_id":session,"messages":history,"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path(),"max_tokens":8192})).unwrap();
        let mut events = engine.send_message(request).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
            }
        })
        .await
        .unwrap();
        let actual = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(
            &actual["input"].as_array().unwrap()[..3],
            window().as_array().unwrap()
        );
        assert!(actual["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.to_string().contains("Continue")));
    }
    // A second native compact forwards the complete earlier window too.
    compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    assert_eq!(
        &seen.lock().unwrap().last().unwrap()["input"]
            .as_array()
            .unwrap()[..3],
        window().as_array().unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn failure_and_cancellation_never_commit_opaque_state() {
    for mode in [1, 2] {
        let (_dir, store, id, engine, seen, server) = fixture(mode).await;
        let before = store.history(&id).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", ""),
        )
        .await;
        if mode == 1 {
            assert!(result.unwrap().is_err());
        } else {
            assert!(result.is_err());
        }
        assert_eq!(store.history(&id).unwrap(), before);
        assert_eq!(seen.lock().unwrap().len(), 1);
        let count = store
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM peripheral_settings WHERE key LIKE 'compaction.native.%'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(count, 0);
        server.abort();
    }
}

/// A real compaction exchange replaces the history that carried a successful
/// ToolSearch, so the restored session cannot treat the summary as an
/// authorization; a fresh search loads the tool again for the next request.
#[tokio::test]
async fn a_compacted_search_record_cannot_authorize_and_a_fresh_search_restores_it() {
    let (dir, store, id, engine, seen, server) = fixture(5).await;
    std::fs::write(dir.path().join("item.txt"), "listing").unwrap();
    let assistant = uuid::Uuid::new_v4().to_string();
    store.append(&id, "assistant", "").unwrap();
    store
        .db
        .with_conn(|conn| {
            deepseek_code_core::db::message_repo::save_stream_tool_result(
                conn,
                &id,
                &assistant,
                "",
                &deepseek_code_core::db::message_repo::ToolCallRow {
                    id: format!("{assistant}:search-1"),
                    message_id: assistant.clone(),
                    name: "ToolSearch".to_owned(),
                    input: Some(json!({"query":"ListDir"}).to_string()),
                    output: Some(
                        json!({"success":true,"loaded_tools":["ListDir"],"tools":[]}).to_string(),
                    ),
                    is_error: false,
                    sort_order: 0,
                },
            )
        })
        .unwrap()
        .unwrap();
    store.append(&id, "user", "Continue").unwrap();
    let before = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    assert!(
        serde_json::to_string(&before).unwrap().contains("loaded_tools"),
        "{before:?}"
    );

    compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    let after = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    assert!(
        !serde_json::to_string(&after).unwrap().contains("loaded_tools"),
        "the compaction window must replace the search record: {after:?}"
    );

    store.append(&id, "user", "Continue").unwrap();
    let request: ChatRequest = serde_json::from_value(json!({"conversation_id":id,"messages":store.model_history(&id,&ApiFormat::OpenAI).unwrap(),"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path(),"max_tokens":8192})).unwrap();
    let mut events = engine.send_message(request).await.unwrap();
    let mut errors = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if let deepseek_code_core::native_engine::tool_loop::EngineEvent::ToolUseDone {
                tool_name,
                output,
                is_error,
                ..
            } = event
            {
                if is_error {
                    errors.push(format!("{tool_name}: {output}"));
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let requests = seen.lock().unwrap();
    // The compaction exchange, then the first turn request without the tool,
    // the request after the fresh search, and the request carrying the listing.
    assert_eq!(requests.len(), 4, "{requests:#?}");
    let names = |body: &Value| -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(!names(&requests[1]).contains(&"ListDir".to_owned()));
    assert!(names(&requests[2]).contains(&"ListDir".to_owned()));
    assert!(names(&requests[3]).contains(&"ListDir".to_owned()));
    assert!(
        requests[3].to_string().contains("item.txt"),
        "the restored search did not run the tool"
    );
    server.abort();
}
