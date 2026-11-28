use deepseek_desktop_tauri_lib::{
    bridge::BridgeServer,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::time::Duration;

async fn fixture_response(
    axum::extract::State(captured): axum::extract::State<
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    >,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if headers["x-dcl-request-kind"] == "compaction" {
        captured.lock().unwrap().push(body.clone());
        if body.to_string().contains("FAIL_FOCUS") {
            return (axum::http::StatusCode::UNAUTHORIZED, "fixture denied").into_response();
        }
        let stream = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"Retain the fixture decision."},"finish_reason":"stop"}]})
        );
        return ([("content-type", "text/event-stream")], stream).into_response();
    }
    let messages = body["messages"].as_array().unwrap();
    let (delta, finish) = if messages.iter().any(|m| m["role"] == "tool") {
        (json!({"content":"turn finished"}), "stop")
    } else {
        let (shell, command) = if cfg!(windows) {
            ("powershell", "Set-Content started.txt ready; Start-Sleep -Milliseconds 3000; Set-Content late.txt unexpected")
        } else {
            (
                "sh",
                "echo ready > started.txt; sleep 3; echo unexpected > late.txt",
            )
        };
        (
            json!({"content":"partial generation evidence", "tool_calls":[{"index":0,"id":"slow-call","type":"function","function":{"name":"Bash","arguments":json!({"shell":shell,"command":command,"timeout":10000}).to_string()}}]}),
            "tool_calls",
        )
    };
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    );
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn conversation_status_context_size_and_stop_generation_routes() {
    deepseek_desktop_tauri_lib::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A local fixture model: the first request starts a slow tool, later ones finish.
    let compactions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .route(
            "/v1/chat/completions",
            axum::routing::post(fixture_response),
        )
        .with_state(compactions.clone());
    let second_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let second = axum::Router::new()
        .route(
            "/second/v1/chat/completions",
            axum::routing::post(fixture_response),
        )
        .with_state(second_requests.clone());
    let app = app.merge(second);
    let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_addr = model_listener.local_addr().unwrap();
    let model = tokio::spawn(async move {
        axum::serve(model_listener, app).await.unwrap();
    });
    let mut config = json!({"providers":[{"id":"fixture","name":"Fixture","provider_type":"openai","api_key":"test","base_url":format!("http://{model_addr}"),"enabled":true,"is_default":true,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"max_tokens":1000,"context_window":64000,"supports_vision":false,"supports_tools":true,"supports_streaming":true}]}]});
    let mut second_provider = config["providers"][0].clone();
    second_provider["id"] = json!("second/provider");
    second_provider["base_url"] = json!(format!("http://{model_addr}/second/v1"));
    second_provider["is_default"] = json!(false);
    let mut tiny_model = second_provider["models"][0].clone();
    tiny_model["id"] = json!("small-window");
    tiny_model["context_window"] = json!(256);
    second_provider["models"]
        .as_array_mut()
        .unwrap()
        .push(tiny_model);
    config["providers"]
        .as_array_mut()
        .unwrap()
        .push(second_provider.clone());
    deepseek_code_core::data_layout::write_config_file(
        dir.path().join("config/config.json"),
        config.to_string(),
    )
    .unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let db = paths.open_database().unwrap();
    let sessions = SessionStore::new(db.clone());
    let conversation = sessions
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    sessions
        .append(&conversation, "user", &"earlier question ".repeat(100))
        .unwrap();
    sessions
        .append(&conversation, "assistant", &"earlier answer ".repeat(100))
        .unwrap();

    let bridge = BridgeServer::new(dir.path().into(), db.clone());
    let key = bridge.get_api_key().to_string();
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let server = tokio::spawn(async move {
        bridge.start(port).await.unwrap();
    });
    let base = format!("http://127.0.0.1:{port}/api/conversations");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let get = |path: String| {
        let request = client
            .get(format!("{base}/{path}"))
            .header("x-api-key", &key);
        async move { request.send().await.unwrap() }
    };
    let stop = |id: String| {
        let request = client
            .post(format!("{base}/{id}/stop-generation"))
            .header("x-api-key", &key);
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };

    // Routes sit behind the same auth layer as the other conversation routes.
    let unauthenticated = client
        .get(format!("{base}/{conversation}/context-size"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let before: Value = get(format!("{conversation}/context-size"))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["limit"], 64000);
    let before_tokens = before["tokens"].as_u64().expect("tokens must be a number");
    assert!(
        before_tokens > 500,
        "persisted history must be counted: {before}"
    );
    let missing = get("00000000-0000-0000-0000-000000000000/context-size".into()).await;
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        missing.json::<Value>().await.unwrap()["error"],
        "Conversation not found"
    );

    let compact = |id: String, body: Value| {
        let req = client
            .post(format!("{base}/{id}/compact"))
            .header("x-api-key", &key)
            .json(&body);
        async move { req.send().await.unwrap() }
    };
    // Run the success and failure cases on a separate seeded conversation so
    // the active-turn checks below retain their original context-size baseline.
    let seeded = sessions
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    sessions
        .append(&seeded, "user", &"Some durable decision. ".repeat(256))
        .unwrap();
    sessions
        .append(&seeded, "assistant", "Keep the fixture decision.")
        .unwrap();
    let history = sessions.history(&seeded).unwrap();
    let typo = compact(seeded.clone(), json!({"foucs":"misspelled"})).await;
    assert_eq!(typo.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(typo.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("foucs"));
    assert!(compactions.lock().unwrap().is_empty());
    assert_eq!(sessions.history(&seeded).unwrap(), history);
    let failure = compact(seeded.clone(), json!({"focus":"FAIL_FOCUS"})).await;
    assert_eq!(failure.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(failure.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("401"));
    assert_eq!(sessions.history(&seeded).unwrap(), history);

    let success = compact(
        seeded.clone(),
        json!({"instruction":"KEEP_FOCUS", "env_base_url":"unused", "env_token":"fixture-only"}),
    )
    .await;
    let status = success.status();
    let result: Value = success.json().await.unwrap();
    assert!(status.is_success(), "{result}");
    assert_eq!(result["messagesCompacted"], 2);
    assert!(result["summary"]
        .as_str()
        .unwrap()
        .contains("Retain the fixture decision."));
    assert!(result["tokensSaved"].as_u64().unwrap() > 0);
    assert_eq!(
        deepseek_code_core::services::compaction::count(&db, &seeded)
            .unwrap()
            .proven(),
        1
    );
    assert_eq!(compactions.lock().unwrap().len(), 2);
    assert!(compactions.lock().unwrap()[1]
        .to_string()
        .contains("KEEP_FOCUS"));

    let idle: Value = get(format!("{conversation}/generation-status"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            idle["active"].clone(),
            idle["status"].clone(),
            idle["generating"].clone()
        ),
        (json!(false), json!("idle"), json!(false))
    );
    let idle: Value = get(format!("{conversation}/stream-status"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            idle["active"].clone(),
            idle["eventCount"].clone(),
            idle["generating"].clone()
        ),
        (json!(false), json!(0), json!(false))
    );
    for id in [conversation.clone(), "unknown-conversation".to_string()] {
        assert_eq!(
            stop(id).await,
            json!({"ok":true, "stopped":false, "settled":true, "paused":true})
        );
    }

    client
        .post(format!("{base}/{conversation}/warm"))
        .header("x-api-key", &key)
        .json(&json!({"permission_mode":"bypass_permissions"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut response = client
        .post(format!("http://127.0.0.1:{port}/api/chat"))
        .header("x-api-key", &key)
        .json(&json!({"conversation_id":conversation,"model":"fixture-model","provider_id":"fixture","workspace_path":dir.path(),"messages":[{"role":"user","content":"run the slow tool"}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !dir.path().join("started.txt").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // The tool process and the Bridge event collector advance independently.
    // Observe the collector's partial answer before stopping the still-running tool.
    let running = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status: Value = get(format!("{conversation}/generation-status"))
                .await
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["text"].as_str().is_some_and(|s| !s.is_empty()) || status["active"] != true {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Bridge must publish partial text while the tool is running");
    assert_eq!(
        (running["active"].clone(), running["status"].clone()),
        (json!(true), json!("generating")),
        "{running}"
    );
    assert_eq!(running["text"], "partial generation evidence");
    assert!(running["eventCount"].as_u64().unwrap() > 0);
    let busy = compact(conversation.clone(), json!({"instructions":"BUSY_FOCUS"})).await;
    assert_eq!(busy.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(compactions.lock().unwrap().len(), 2);
    let running_stream: Value = get(format!("{conversation}/stream-status"))
        .await
        .json()
        .await
        .unwrap();
    // No reattach route exists, so the UI must poll generation-status instead.
    assert_eq!(
        (
            running_stream["active"].clone(),
            running_stream["generating"].clone()
        ),
        (json!(false), json!(true))
    );
    assert_eq!(
        get(format!(
            "{}/generation-status",
            sessions
                .create(dir.path(), "fixture-model", "fixture")
                .unwrap()
        ))
        .await
        .json::<Value>()
        .await
        .unwrap()["active"],
        false
    );

    assert_eq!(
        stop(conversation.clone()).await,
        json!({"ok":true, "stopped":true, "settled":true, "paused":true})
    );
    let after: Value = get(format!("{conversation}/generation-status"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after["active"], false, "stop must settle before it returns");

    let mut body = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(chunk) = response.chunk().await.unwrap() {
            body.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .expect("the stopped turn must close its response stream");
    let terminal = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .last()
        .unwrap();
    assert_eq!(
        (terminal["type"].clone(), terminal["stop_reason"].clone()),
        (json!("message_stop"), json!("cancelled")),
        "{body}"
    );
    assert!(!body.contains("turn finished"));

    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(
        !dir.path().join("late.txt").exists(),
        "stop-generation left the tool running"
    );
    let rows = db
        .open_session(&conversation)
        .unwrap()
        .read(|r| r.messages())
        .unwrap();
    assert!(
        rows.iter()
            .any(|row| row.role == "assistant"
                && row.content.contains("partial generation evidence")),
        "stop lost the partial assistant trajectory"
    );
    let after: Value = get(format!("{conversation}/context-size"))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after["tokens"].as_u64().unwrap() > before_tokens,
        "{after} vs {before}"
    );
    assert_eq!(stop(conversation.clone()).await["stopped"], false);

    // The queued request still refers to the first Desktop chat. Publish a
    // same-model switch through the same binding transaction used by the CLI.
    let shard = db.open_session(&conversation).unwrap();
    let binding = deepseek_code_core::native_engine::session_binding::BindingSnapshot {
        generation: 0,
        observation_only: false,
        provider_id: "second/provider".into(),
        model: "fixture-model".into(),
        environment: Default::default(),
        providers: vec![deepseek_code_core::runtime::provider_from_config(
            &serde_json::from_value(second_provider).unwrap(),
        )],
    };
    shard
        .publish_provider(
            shard.observe_provider().unwrap().generation,
            &binding,
            None,
            "fixture",
            None,
        )
        .unwrap();
    // A Desktop admission carrying the stale provider can lease an existing
    // conversation, but only the original creation can write its binding.
    let (lease, _) = db
        .admit_chat_execution_with_provider(
            &conversation,
            "fixture-model",
            Some("fixture"),
            None,
            "typed",
        )
        .unwrap();
    drop(lease);
    assert_eq!(
        db.desktop_context(&conversation)
            .unwrap()
            .provider_id
            .as_deref(),
        Some("fixture")
    );
    assert_eq!(
        db.conversation_metadata(&conversation)
            .unwrap()
            .unwrap()
            .provider
            .as_deref(),
        Some("second/provider")
    );
    let abandoned = shard.write(|w| w.start_turn()).unwrap();
    let switched = compact(conversation.clone(), json!({"focus":"SWITCH_FOCUS"})).await;
    assert!(
        switched.status().is_success(),
        "{}",
        switched.text().await.unwrap()
    );
    assert_eq!(compactions.lock().unwrap().len(), 2);
    assert_eq!(second_requests.lock().unwrap().len(), 1);
    assert!(second_requests.lock().unwrap()[0]
        .to_string()
        .contains("SWITCH_FOCUS"));

    let settled = shard
        .repository()
        .observed_turn(&abandoned)
        .unwrap()
        .unwrap();
    assert_eq!(
        settled.status,
        deepseek_code_core::db::turn_repo::TurnStatus::Cancelled
    );

    // A picker edit stores the decoded provider and preserves the window override.
    let picker = db.open_session(&seeded).unwrap();
    picker
        .edit_conversation(&json!({"model":"second%2Fprovider::small-window"}))
        .unwrap();
    sessions
        .append(&seeded, "user", &"Keep the test input. ".repeat(512))
        .unwrap();
    let too_small = compact(seeded.clone(), json!({"focus":"SAME_WINDOW_TEST"})).await;
    assert_eq!(too_small.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(second_requests.lock().unwrap().len(), 1);
    picker
        .edit_conversation(&json!({"model":"second%2Fprovider::small-window[1m]"}))
        .unwrap();
    let selected = db.conversation_metadata(&seeded).unwrap().unwrap();
    assert_eq!(selected.provider.as_deref(), Some("second/provider"));
    assert_eq!(selected.model.as_deref(), Some("small-window[1m]"));
    let qualified = compact(seeded.clone(), json!({"instructions":"SAME_WINDOW_TEST"})).await;
    assert!(
        qualified.status().is_success(),
        "{}",
        qualified.text().await.unwrap()
    );
    assert_eq!(second_requests.lock().unwrap().len(), 2);
    assert_eq!(compactions.lock().unwrap().len(), 2);

    shard
        .edit_conversation(&json!({"project_id":null,"workspace_mode":"code"}))
        .unwrap();
    let invalid = compact(conversation.clone(), json!({})).await;
    let chat_request = || {
        client
            .post(format!("http://127.0.0.1:{port}/api/chat"))
            .header("x-api-key", &key)
            .json(&json!({"conversation_id":conversation,"messages":[],"model":"fixture-model"}))
    };
    let chat = chat_request().send().await.unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(chat.status(), invalid.status());
    assert_eq!(
        chat.json::<Value>().await.unwrap(),
        invalid.json::<Value>().await.unwrap()
    );
    assert_eq!(second_requests.lock().unwrap().len(), 2);
    // Exhaust the shared chat quota with invalid project requests (zero model calls).
    let mut limited = false;
    for _ in 0..=60 {
        let rejected = compact(conversation.clone(), json!({})).await;
        if rejected.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(rejected.headers()["retry-after"], "60");
            limited = true;
            break;
        }
        assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    }
    assert!(limited);
    let limited_chat = chat_request().send().await.unwrap();
    assert_eq!(
        limited_chat.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(second_requests.lock().unwrap().len(), 2);
    server.abort();
    model.abort();
}
