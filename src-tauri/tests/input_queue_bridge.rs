use deepseek_code_core::services::input_queue as queue;
use deepseek_desktop_tauri_lib::{
    bridge::BridgeServer,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn durable_queue_api_conflicts_pause_and_backend_dispatch() {
    const CHILD: &str = "DEEPSEEK_QUEUE_BRIDGE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The invoking CLI worker owns a different session than this fixture.
        // Isolate its control channel without mutating the host environment.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "durable_queue_api_conflicts_pause_and_backend_dispatch",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove(deepseek_code_core::services::background_sessions::CONTROL_ENV)
            .env_remove(deepseek_code_core::services::background_sessions::WORKER_ENV)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated queue regression failed: {status}"
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(durable_queue_roundtrip());
}

async fn durable_queue_roundtrip() {
    deepseek_desktop_tauri_lib::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let model=axum::Router::new().route("/v1/chat/completions",axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
        let tx=tx.clone(); async move {
            tx.send(body).unwrap();
            ([("content-type","text/event-stream")],format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let model = tokio::spawn(async move { axum::serve(listener, model).await.unwrap() });
    deepseek_code_core::data_layout::write_config_file(dir.path().join("config/config.json"),json!({"providers":[{"id":"fixture","name":"Fixture","provider_type":"openai","api_key":"test","base_url":format!("http://{addr}"),"enabled":true,"is_default":true,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"max_tokens":1000,"context_window":64000,"supports_vision":true,"supports_tools":false,"supports_streaming":true}]}]}).to_string()).unwrap();
    let db = RuntimePaths::new(dir.path().into())
        .unwrap()
        .open_database()
        .unwrap();
    let id = SessionStore::new(db.clone())
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    let bridge = BridgeServer::new(dir.path().into(), db.clone());
    let key = bridge.get_api_key().to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let server = tokio::spawn(async move { bridge.start(port).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.get(format!("{base}/health")).send().await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let url = format!("{base}/api/conversations/{id}/queue");
    // Desktop Code submissions need a selected project before any side effects.
    let original_project = db.conversation_metadata(&id).unwrap().unwrap().project_id.unwrap();
    let shard=db.open_session(&id).unwrap();
    shard.edit_conversation(&json!({"project_id":null,"workspace_mode":"code"})).unwrap();
    let rejected = client.post(format!("{base}/api/chat")).header("x-api-key", &key)
        .json(&json!({"conversation_id":id,"messages":[{"role":"user","content":"must not persist"}],"model":"fixture-model","provider_id":"fixture"})).send().await.unwrap();
    assert_eq!(rejected.status(), 400);
    assert!(rejected.text().await.unwrap().contains("Select a project"));
    assert!(shard.read(|r|r.messages()).unwrap().is_empty());
    assert!(requests.try_recv().is_err());
    shard.edit_conversation(&json!({"project_id":original_project})).unwrap();
    queue::set_paused(&db, &id, true).unwrap();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=";
    let content = json!([
        {"type":"text","text":"queued"},
        {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{png}")},"fileName":"queued.png","size":68},
        {"type":"attachment","fileName":"notes.txt","size":3,"source":{"type":"base64","media_type":"text/plain","data":"YWJj"}}
    ]);
    let payload = json!({"requestId":"one","content":content,"mode":"next_turn"});
    let first: Value = client
        .post(&url)
        .header("x-api-key", &key)
        .json(&payload)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let duplicate: Value = client
        .post(&url)
        .header("x-api-key", &key)
        .json(&payload)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, duplicate);
    assert_eq!(first["content"], content);
    let item_url = format!("{url}/{}", first["id"]);
    // An idle session must retain the row instead of treating Steer as a new turn.
    for paused in [true, false] {
        queue::set_paused(&db, &id, paused).unwrap();
        let response = client
            .post(format!("{item_url}/steer"))
            .header("x-api-key", &key)
            .json(&json!({"revision":first["revision"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 409);
        assert!(response.text().await.unwrap().contains("active execution"));
        let retained = &queue::list(&db, &id).unwrap()[0];
        assert_eq!(retained.mode, queue::QueueMode::NextTurn);
        assert_eq!(retained.revision, first["revision"].as_i64().unwrap());
    }
    queue::set_paused(&db, &id, true).unwrap();
    assert_eq!(
        client
            .patch(&item_url)
            .header("x-api-key", &key)
            .json(&json!({"revision":99,"text":"stale"}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(client.post(&url).header("x-api-key",&key).json(&json!({"requestId":"bad","content":[{"type":"image_url","image_url":{"url":"blob:gone"}}],"mode":"next_turn"})).send().await.unwrap().status(),400);
    // A real first turn seeds the persistent execution settings. Pause is not
    // cleared by ordinary submission, and queue input does not join that turn.
    let response=client.post(format!("{base}/api/chat")).header("x-api-key",&key).json(&json!({"conversation_id":id,"messages":[{"role":"user","content":"first"}],"model":"fixture-model","provider_id":"fixture"})).send().await.unwrap();
    assert!(response.status().is_success());
    let events = response.text().await.unwrap();
    assert!(
        !events.contains("\"type\":\"error\""),
        "first turn failed before queue dispatch: {events}"
    );
    let first_request = tokio::time::timeout(Duration::from_secs(10), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!first_request["messages"].to_string().contains("queued"));
    assert_eq!(
        queue::list(&db, &id).unwrap()[0].state,
        queue::QueueState::Queued
    );
    // Contending process ownership cannot consume the admitted row.
    let lease = shard.acquire_execution().unwrap();
    client
        .post(format!("{url}/resume"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue::list(&db, &id).unwrap()[0].state,
        queue::QueueState::Queued
    );
    drop(lease);
    client
        .post(format!("{url}/resume"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(10), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(second["messages"].to_string().contains("queued"));
    assert!(second["messages"].to_string().contains(png));
    assert!(!second["messages"].to_string().contains("fileName"));
    assert!(second["messages"]
        .to_string()
        .contains("Attached file: notes.txt"));
    assert!(second["messages"].to_string().contains("abc"));
    assert!(!second["messages"].to_string().contains("YWJj"));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status: Value = client
                .get(format!("{base}/api/conversations/{id}/generation-status"))
                .header("x-api-key", &key)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["active"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        queue::list(&db, &id).unwrap()[0].state,
        queue::QueueState::Handoff
    );
    let handoff=queue::list(&db,&id).unwrap()[0].handoff_id.clone().unwrap();
    let messages=shard.read(|r|r.messages()).unwrap();
    assert_eq!(messages.iter().filter(|m|m.id==handoff).count(),1);
    assert_eq!(shard.read(|r|r.user_content(&handoff)).unwrap(),content);
    client
        .post(format!("{base}/api/conversations/{id}/stop-generation"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(queue::is_paused(&db, &id).unwrap());
    // A rejected first request must leave the ID available for a sealed first turn.
    let first_id = uuid::Uuid::new_v4().to_string();
    let first = |content: Value| {
        client.post(format!("{base}/api/chat")).header("x-api-key",&key)
        .json(&json!({"conversation_id":first_id,"messages":[{"role":"user","content":content}],"model":"fixture-model","provider_id":"fixture"}))
    };
    let invalid = first(json!({"invalid":"content"})).send().await.unwrap();
    assert_eq!(invalid.status(), 400);
    assert!(db.conversation_metadata(&first_id).unwrap().is_none());
    let content = json!([{"type":"text","text":"accepted first prompt"}]);
    let accepted = first(content.clone()).send().await.unwrap();
    assert!(accepted.status().is_success());
    let _ = accepted.bytes().await.unwrap();
    let (_, rows) = db.conversation_detail(&first_id).unwrap();
    let users = rows
        .iter()
        .filter(|row| row["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], content);
    server.abort();
    model.abort();
}
