use deepseek_desktop_tauri_lib::{
    bridge::BridgeServer,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test]
async fn conversation_status_context_size_and_stop_generation_routes() {
    deepseek_desktop_tauri_lib::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A local fixture model: the first request starts a slow tool, later ones finish.
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
        let messages = body["messages"].as_array().unwrap();
        let (delta, finish) = if messages.iter().any(|m| m["role"] == "tool") {
            (json!({"content":"turn finished"}), "stop")
        } else {
            let (shell, command) = if cfg!(windows) {
                ("powershell", "Set-Content started.txt ready; Start-Sleep -Milliseconds 3000; Set-Content late.txt unexpected")
            } else {
                ("sh", "echo ready > started.txt; sleep 3; echo unexpected > late.txt")
            };
            (json!({"content":"partial generation evidence", "tool_calls":[{"index":0,"id":"slow-call","type":"function","function":{"name":"Bash","arguments":json!({"shell":shell,"command":command,"timeout":10000}).to_string()}}]}), "tool_calls")
        };
        let stream = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}), json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
        ([("content-type", "text/event-stream")], stream)
    }));
    let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_addr = model_listener.local_addr().unwrap();
    let model = tokio::spawn(async move {
        axum::serve(model_listener, app).await.unwrap();
    });
    let config = json!({"providers":[{"id":"fixture","name":"Fixture","provider_type":"openai","api_key":"test","base_url":format!("http://{model_addr}"),"enabled":true,"is_default":true,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"max_tokens":1000,"context_window":64000,"supports_vision":false,"supports_tools":true,"supports_streaming":true}]}]});
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
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
            json!({"ok":true, "stopped":false, "settled":true})
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

    let running: Value = get(format!("{conversation}/generation-status"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (running["active"].clone(), running["status"].clone()),
        (json!(true), json!("generating")),
        "{running}"
    );
    assert_eq!(running["text"], "partial generation evidence");
    assert!(running["eventCount"].as_u64().unwrap() > 0);
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
        json!({"ok":true, "stopped":true, "settled":true})
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
        .with_conn(|conn| {
            deepseek_desktop_tauri_lib::db::message_repo::get_messages_by_conversation(
                conn,
                &conversation,
            )
        })
        .unwrap()
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

    server.abort();
    model.abort();
}
