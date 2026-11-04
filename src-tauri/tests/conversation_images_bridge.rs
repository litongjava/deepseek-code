use deepseek_code_core::db::{
    message_repo,
    turn_repo::{TurnStatus, TurnTimer},
};
use deepseek_desktop_tauri_lib::{
    bridge::BridgeServer,
    runtime::{normalize_user_content, RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::time::Duration;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=";

#[test]
fn image_history_roundtrips_through_real_bridge_and_both_save_paths() {
    const CHILD: &str = "DEEPSEEK_IMAGE_BRIDGE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // A CLI worker's control channel belongs to its own session, not this
        // temporary Desktop fixture. Isolate the child without mutating host env.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "image_history_roundtrips_through_real_bridge_and_both_save_paths",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove(deepseek_code_core::services::background_sessions::CONTROL_ENV)
            .env_remove(deepseek_code_core::services::background_sessions::WORKER_ENV)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated Bridge regression failed: {status}"
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(image_history_roundtrip());
}

async fn image_history_roundtrip() {
    deepseek_desktop_tauri_lib::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(
        move |axum::Json(body): axum::Json<Value>| {
            let requests = requests.clone();
            async move {
                let hold = body["messages"].to_string().contains("HOLD_TIMING_FIXTURE");
                requests.send(body).unwrap();
                if hold { tokio::time::sleep(Duration::from_secs(60)).await; }
                ([ ("content-type", "text/event-stream") ], format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"index":0,"delta":{"content":"seen"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                ))
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_addr = listener.local_addr().unwrap();
    let model = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(dir.path().join("config.json"), json!({"providers":[{
        "id":"fixture", "name":"Fixture", "provider_type":"openai", "api_key":"test",
        "base_url":format!("http://{model_addr}"), "enabled":true, "is_default":true, "settings":{},
        "supports_web_search":false, "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
        "max_tokens":1000,"context_window":64000,"supports_vision":true,"supports_tools":false,"supports_streaming":true}]
    }]}).to_string()).unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let db = paths.open_database().unwrap();
    let store = SessionStore::new(db.clone());
    let conversation = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    let mixed = json!([
        {"type":"text","text":"[Image #3] first"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
        {"type":"text","text":"[Image #8] second"},
        {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}}
    ]);
    let normalized = normalize_user_content(&mixed).unwrap();
    let cli_id = store
        .append_user_content(&conversation, "[Image #3] first\n[Image #8] second", &mixed)
        .unwrap();
    let text_id = store
        .append(&conversation, "user", "ordinary text")
        .unwrap();
    let mut timer = TurnTimer::start(db.clone(), &conversation).unwrap();
    let assistant_id = timer.ensure_segment().unwrap();
    let turn = serde_json::to_value(timer.finish(TurnStatus::Completed).unwrap()).unwrap();

    let bridge = BridgeServer::new(dir.path().into(), db.clone());
    let key = bridge.get_api_key().to_string();
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let server = tokio::spawn(async move { bridge.start(port).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");
    let url = format!("{base}/api/conversations/{conversation}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.get(format!("{base}/health")).send().await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let get = |suffix: &str| {
        let request = client
            .get(format!("{url}{suffix}"))
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
    for suffix in ["", "/messages"] {
        let body = get(suffix).await;
        assert_eq!(body["messages"][0]["id"], cli_id);
        assert_eq!(body["messages"][0]["content"], normalized);
        assert_eq!(body["messages"][1]["content"], "ordinary text");
        assert!(body["messages"][1].get("turn").is_none());
        assert_eq!(body["messages"][2]["turn"], turn);
    }
    let saved: Value = client
        .post(&url)
        .header("x-api-key", &key)
        .json(&json!({"messages":[
            {"id":cli_id,"role":"user","content":mixed},
            {"id":text_id,"role":"user","content":"edited text"},
            {"id":assistant_id,"role":"assistant","content":"answer"},
            {"id":"tail","role":"user","content":"delete me"}
        ]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["ok"], true, "{saved}");
    let tail: Value = client
        .delete(format!("{url}/messages-tail/1"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tail["messages"][0]["content"], normalized);
    assert_eq!(
        tail["messages"][2]["turn"], turn,
        "retained messages keep host-owned timing"
    );
    db.with_conn(|conn| {
        assert_eq!(
            message_repo::get_message(conn, &cli_id)
                .unwrap()
                .unwrap()
                .content,
            "[Image #3] first\n[Image]\n[Image #8] second\n[Image]"
        );
    })
    .unwrap();

    // Read the same durable content through a separately reopened SQLite connection.
    let reopened =
        rusqlite::Connection::open(dir.path().join(deepseek_code_core::runtime::DATABASE_FILE))
            .unwrap();
    let row = message_repo::get_message(&reopened, &cli_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        message_repo::user_content_for_display(&reopened, &row).unwrap(),
        normalized
    );
    drop(reopened);

    // IDs cannot move content between conversations, and rejection rolls back earlier edits.
    let other = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    let other_id = store.append(&other, "user", "other conversation").unwrap();
    let rejected: Value = client
        .post(&url)
        .header("x-api-key", &key)
        .json(&json!({"messages":[
            {"id":text_id,"role":"user","content":"must roll back"},
            {"id":other_id,"role":"user","content":"must not overwrite"}
        ]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejected["ok"], false);
    assert_eq!(get("").await["messages"], tail["messages"]);
    assert_eq!(
        store.history(&other).unwrap()[0]["content"],
        "other conversation"
    );

    // Invalid replacement is atomic: no loss of the old images or timing.
    let invalid = json!([{"type":"image_url","image_url":{"url":"file:///not-authorized.png"}}]);
    let rejected: Value = client
        .post(&url)
        .header("x-api-key", &key)
        .json(&json!({"messages":[
            {"id":cli_id,"role":"user","content":invalid}
        ]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejected["ok"], false);
    assert_eq!(get("").await["messages"], tail["messages"]);

    // Executor startup failures must preserve their cause and durable failed timing.
    // Holding the real execution lease fails before the model can receive a request.
    let locked_session = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    let lease = deepseek_code_core::services::session_messages::ExecutionLease::acquire(
        dir.path(),
        &locked_session,
    )
    .unwrap()
    .unwrap();
    let response = client
        .post(format!("{base}/api/chat"))
        .header("x-api-key", &key)
        .json(&json!({"conversation_id":locked_session,"model":"fixture-model","provider_id":"fixture",
            "messages":[{"role":"user","content":"cannot acquire execution ownership"}]}))
        .send().await.unwrap().error_for_status().unwrap().text().await.unwrap();
    let events: Vec<Value> = response
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let errors: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "error")
        .collect();
    assert_eq!(errors.len(), 1, "{response}");
    assert!(
        errors[0]["error"]
            .as_str()
            .unwrap()
            .contains("Session is already executing in another owner"),
        "{response}"
    );
    assert!(
        !response.contains("Connection to engine lost"),
        "{response}"
    );
    assert!(
        !events.iter().any(|event| event["type"] == "message_start"),
        "{response}"
    );
    assert!(matches!(
        received.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let timing: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "turn_timing")
        .collect();
    assert_eq!(timing.len(), 2, "{response}");
    assert_eq!(timing[0]["turn"]["status"], "running");
    assert_eq!(timing[1]["turn"]["status"], "failed");
    assert_eq!(timing[0]["turn"]["turnId"], timing[1]["turn"]["turnId"]);
    assert!(timing[1]["turn"]["elapsedMs"].as_u64().is_some());
    let history: Value = client
        .get(format!(
            "{base}/api/conversations/{locked_session}/messages"
        ))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let assistants: Vec<&Value> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0]["content"], "");
    assert_eq!(assistants[0]["turn"], timing[1]["turn"]);
    drop(lease);

    // The actual /api/chat save path keeps image-only input and the model sees bytes.
    let image_only = json!([mixed[1].clone()]);
    let chat: String = client
        .post(format!("{base}/api/chat"))
        .header("x-api-key", &key)
        .json(
            &json!({"conversation_id":conversation,"model":"fixture-model","provider_id":"fixture",
            "messages":[{"role":"user","content":image_only}]}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chat.contains("seen"), "{chat}");
    let completed_events: Vec<Value> = chat
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == "turn_timing")
        .collect();
    assert_eq!(completed_events.len(), 2, "{chat}");
    assert_eq!(completed_events[0]["turn"]["status"], "running");
    assert_eq!(completed_events[1]["turn"]["status"], "completed");
    assert_eq!(
        completed_events[0]["turn"]["turnId"],
        completed_events[1]["turn"]["turnId"]
    );

    let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "user"
                && m["content"].as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "image_url")
                        .cloned()
                        .collect::<Vec<_>>()
                        == normalize_user_content(&image_only)
                            .unwrap()
                            .as_array()
                            .unwrap()
                            .clone()
                })),
        "{request}"
    );
    let history = get("/messages").await;
    let completed_segments: Vec<&Value> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["turn"]["turnId"] == completed_events[1]["turn"]["turnId"])
        .collect();
    assert_eq!(completed_segments.len(), 1);
    assert_eq!(completed_segments[0]["turn"], completed_events[1]["turn"]);

    assert!(history["messages"].as_array().unwrap().iter().any(
        |m| m["role"] == "user" && m["content"] == normalize_user_content(&image_only).unwrap()
    ));
    let invalid_chat = client.post(format!("{base}/api/chat")).header("x-api-key", &key)
        .json(&json!({"conversation_id":conversation,"model":"fixture-model","messages":[{"role":"user","content":invalid}]}))
        .send().await.unwrap();
    assert_eq!(invalid_chat.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(get("/messages").await["messages"], history["messages"]);

    // Corrupt JSON and invalid image bytes are local unavailable states, not empty history.
    for malformed in [
        "not-json",
        r#"[{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]"#,
    ] {
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE message_user_content SET content_json=?1 WHERE message_id=?2",
                rusqlite::params![malformed, cli_id],
            )
        })
        .unwrap()
        .unwrap();
        for suffix in ["", "/messages"] {
            let body = get(suffix).await;
            assert_eq!(
                body["messages"][0]["content"][1]["type"],
                "image_unavailable"
            );
            assert!(body["messages"].as_array().unwrap().len() >= 3);
        }
    }
    // A legacy stringified array with no sidecar remains intact for frontend compatibility.
    let legacy = store
        .append(&conversation, "user", &mixed.to_string())
        .unwrap();
    let history = get("/messages").await;
    assert_eq!(
        history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == legacy)
            .unwrap()["content"],
        mixed.to_string()
    );
    // Setup failure still has a complete host-owned turn and a durable empty assistant.
    let failed_conversation = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    let response = client.post(format!("{base}/api/chat")).header("x-api-key", &key)
        .json(&json!({"conversation_id":failed_conversation,"model":"missing-model","provider_id":"missing-provider",
            "messages":[{"role":"user","content":"fail before model request"}]}))
        .send().await.unwrap().error_for_status().unwrap().text().await.unwrap();
    let events: Vec<Value> = response
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let timing: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "turn_timing")
        .collect();
    assert_eq!(timing.len(), 2, "{response}");
    assert_eq!(timing[0]["turn"]["status"], "running");
    assert_eq!(timing[1]["turn"]["status"], "failed");
    assert!(timing[1]["turn"]["elapsedMs"].as_u64().is_some());
    assert!(events.iter().any(|event| event["type"] == "error"));
    let failed: Value = client
        .get(format!("{base}/api/conversations/{failed_conversation}"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let assistant = failed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], "");
    assert_eq!(assistant["turn"], timing[1]["turn"]);
    db.with_conn(|conn| {
        let saved = deepseek_code_core::db::turn_repo::get_message_turn(
            conn,
            assistant["id"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(saved.status, TurnStatus::Failed);
        assert!(saved.elapsed_ms.is_some());
    })
    .unwrap();

    // Held model execution proves both stop and transport loss finalize before reload.
    for disconnect in [false, true] {
        let session = store
            .create(dir.path(), "fixture-model", "fixture")
            .unwrap();
        let response = client
            .post(format!("{base}/api/chat"))
            .header("x-api-key", &key)
            .json(
                &json!({"conversation_id":session,"model":"fixture-model","provider_id":"fixture",
                "messages":[{"role":"user","content":"HOLD_TIMING_FIXTURE"}]}),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(request["messages"]
            .to_string()
            .contains("HOLD_TIMING_FIXTURE"));
        if disconnect {
            drop(response);
        } else {
            let stopped: Value = client
                .post(format!(
                    "{base}/api/conversations/{session}/stop-generation"
                ))
                .header("x-api-key", &key)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(stopped["settled"], true, "{stopped}");
            let events = response.text().await.unwrap();
            assert!(events.contains("\"status\":\"cancelled\""), "{events}");
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let finished = db
                    .with_conn(|conn| {
                        message_repo::get_messages_by_conversation(conn, &session)
                            .unwrap()
                            .iter()
                            .filter_map(|message| {
                                deepseek_code_core::db::turn_repo::get_message_turn(
                                    conn,
                                    &message.id,
                                )
                                .unwrap()
                            })
                            .any(|record| {
                                record.status == TurnStatus::Cancelled
                                    && record.elapsed_ms.is_some()
                            })
                    })
                    .unwrap();
                if finished {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelled turn did not settle durably");
    }

    server.abort();
    model.abort();
}
