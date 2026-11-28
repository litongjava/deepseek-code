use axum::{extract::State, http::HeaderMap, routing::post, Router};
use deepseek_code_core::db::session_store::CreateOptions;
use deepseek_desktop_tauri_lib::{
    bridge::BridgeServer,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Endpoint {
    kinds: Arc<Mutex<Vec<String>>>,
    compact_models: Arc<Mutex<Vec<String>>>,
}

// Record every POST, including requests that do not carry a recognized kind.
async fn provider(
    State(endpoint): State<Endpoint>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> ([(&'static str, &'static str); 1], String) {
    let kind = headers
        .get("x-dcl-request-kind")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    endpoint.kinds.lock().unwrap().push(kind.to_owned());
    if kind == "compaction" {
        endpoint
            .compact_models
            .lock()
            .unwrap()
            .push(body["model"].as_str().unwrap().to_owned());
    }
    let delta = json!({"choices":[{"index":0,"delta":{"content":"Keep the original decision."},"finish_reason":"stop"}]});
    (
        [("content-type", "text/event-stream")],
        format!("data: {delta}\n\ndata: [DONE]\n\n"),
    )
}

#[tokio::test]
async fn compact_uses_the_created_binding_or_latest_completed_turn() {
    let dir = tempfile::tempdir().unwrap();
    let first = Endpoint::default();
    let second = Endpoint::default();
    let calls = || {
        (
            first.kinds.lock().unwrap().clone(),
            second.kinds.lock().unwrap().clone(),
        )
    };
    let app = Router::new()
        .route(
            "/first/v1/chat/completions",
            post(provider).with_state(first.clone()),
        )
        .route(
            "/second/v1/chat/completions",
            post(provider).with_state(second.clone()),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut providers: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            json!({
                "id":id,"name":id,"provider_type":"openai","api_key":"fixture",
                "base_url":format!("http://{addr}/{id}"),"enabled":true,"is_default":id=="first",
                "settings":{},"supports_web_search":false,"models":[{"id":"shared","name":"Shared",
                "enabled":true,"max_tokens":1024,"context_window":64000,"supports_vision":false,
                "supports_tools":true,"supports_streaming":true}]
            })
        })
        .collect();
    for (provider, ids) in providers.iter_mut().zip([
        &["first::shared", "first-only", "second::shared"][..],
        &[
            "first::shared",
            "second::raw-only",
            "second::shared",
            "catalog::raw",
        ][..],
    ]) {
        for id in ids {
            let mut model = provider["models"][0].clone();
            model["id"] = json!(id);
            provider["models"].as_array_mut().unwrap().push(model);
        }
    }
    deepseek_code_core::data_layout::write_config_file(
        dir.path().join("config/config.json"),
        json!({"providers":providers}).to_string(),
    )
    .unwrap();
    let db = RuntimePaths::new(dir.path().into())
        .unwrap()
        .open_database()
        .unwrap();
    let bridge = BridgeServer::new(dir.path().into(), db.clone());
    let key = bridge.get_api_key().to_owned();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let server = tokio::spawn(async move { bridge.start(port).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");
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
    let chat = |id: &str, selected: &str, model: &str| {
        let request = client
            .post(format!("{base}/api/chat"))
            .header("x-api-key", &key)
            .json(&json!({
                "conversation_id":id,"model":model,"provider_id":selected,
                "messages":[{"role":"user","content":"Remember this decision."}]
            }));
        async move {
            let events = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(events.contains("message_stop"), "{events}");
        }
    };
    let chat_with_model = |id: &str, model: &str| {
        client
            .post(format!("{base}/api/chat"))
            .header("x-api-key", &key)
            .json(&json!({"conversation_id":id, "model":model,
                "messages":[{"role":"user","content":"Continue."}]}))
            .send()
    };
    let compact = |id: &str| {
        client
            .post(format!("{base}/api/conversations/{id}/compact"))
            .header("x-api-key", &key)
            .json(&json!({"focus":"Keep the decision"}))
            .send()
    };
    let binding = |id: &str| {
        let row = db.conversation_metadata(id).unwrap().unwrap();
        (row.provider, row.model)
    };

    let created = uuid::Uuid::new_v4().to_string();
    chat(&created, "second", "shared").await;
    assert_eq!(
        binding(&created),
        (Some("second".into()), Some("shared".into()))
    );
    // The initial chat starts a separate title request. Drain it before
    // comparing all POSTs made during compaction.
    let wait_for_title = |id: String| {
        let db = db.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                while db
                    .conversation_metadata(&id)
                    .unwrap()
                    .unwrap()
                    .title
                    .is_none()
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap()
        }
    };
    wait_for_title(created.clone()).await;
    let result = compact(&created)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(result["messagesCompacted"].as_u64().unwrap() >= 2);
    assert!(calls().0.is_empty());
    assert_eq!(calls().1, ["turn", "title", "compaction"]);

    // Prior to first-chat binding persistence, bare rows had a NULL provider.
    let legacy = uuid::Uuid::new_v4().to_string();
    let shard = db
        .create(
            &legacy,
            CreateOptions {
                title: Some("Legacy conversation".into()),
                model: Some("shared".into()),
                ..Default::default()
            },
        )
        .unwrap();
    chat(&legacy, "first", "shared").await;
    chat(&legacy, "second", "shared").await;
    assert_eq!(binding(&legacy).0, None);
    compact(&legacy).await.unwrap().error_for_status().unwrap();
    assert_eq!(calls().0, ["turn"]);
    assert_eq!(
        calls().1,
        ["turn", "title", "compaction", "turn", "compaction"]
    );
    // A model ID can itself contain a provider-looking prefix.
    let opaque = uuid::Uuid::new_v4().to_string();
    chat(&opaque, "second", "first::shared").await;
    wait_for_title(opaque.clone()).await;
    let prior = calls();
    compact(&opaque).await.unwrap().error_for_status().unwrap();
    assert_eq!(calls().0, prior.0);
    assert_eq!(&calls().1[prior.1.len()..], ["compaction"]);

    // A caller may also submit the picker value itself on a first chat.
    let picked_chat = uuid::Uuid::new_v4().to_string();
    let events = chat_with_model(&picked_chat, "se%63ond::first::shared")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(events.contains("message_stop"), "{events}");
    wait_for_title(picked_chat.clone()).await;
    assert_eq!(
        binding(&picked_chat),
        (Some("second".into()), Some("first::shared".into()))
    );

    let store = SessionStore::new(db.clone());
    let mut matrix = Vec::new();
    for model in ["shared", "first::shared"] {
        let cli = store.create(dir.path(), model, "second").unwrap();
        let rebound = store.create(dir.path(), "shared", "first").unwrap();
        let session = db.open_session(&rebound).unwrap();
        let snapshot = deepseek_code_core::native_engine::session_binding::BindingSnapshot {
            generation: 0,
            observation_only: false,
            provider_id: "second".into(),
            model: model.to_owned(),
            environment: Default::default(),
            providers: providers
                .iter()
                .map(|p| {
                    deepseek_code_core::runtime::provider_from_config(
                        &serde_json::from_value(p.clone()).unwrap(),
                    )
                })
                .collect(),
        };
        session
            .publish_provider(
                session.observe_provider().unwrap().generation,
                &snapshot,
                None,
                "cli",
                None,
            )
            .unwrap();

        let picker_key = format!("second::{model}");
        let desktop: Value = client
            .post(format!("{base}/api/conversations"))
            .header("x-api-key", &key)
            .json(&json!({"model":picker_key}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let picker = desktop["id"].as_str().unwrap().to_owned();
        let detail: Value = client
            .get(format!("{base}/api/conversations/{picker}"))
            .header("x-api-key", &key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["model"], picker_key);

        let http = if model == "shared" {
            created.clone()
        } else {
            opaque.clone()
        };
        for (writer, id) in [
            ("cli", cli),
            ("rebind", rebound),
            ("first chat", http),
            ("picker", picker),
        ] {
            assert_eq!(
                binding(&id),
                (Some("second".to_owned()), Some(model.to_owned())),
                "{writer}"
            );
            store.append(&id, "user", "Keep this decision").unwrap();
            store
                .append(&id, "assistant", "Use the selected provider.")
                .unwrap();
            let prior = calls();
            let response = compact(&id).await.unwrap();
            assert!(
                response.status().is_success(),
                "{writer}/{model}: {}",
                response.text().await.unwrap()
            );
            assert_eq!(calls().0, prior.0, "{writer}/{model}");
            assert_eq!(
                &calls().1[prior.1.len()..],
                ["compaction"],
                "{writer}/{model}"
            );
            assert_eq!(second.compact_models.lock().unwrap().last().unwrap(), model);
            matrix.push(id);
        }
    }

    for (raw, recorded, expected_model) in [
        ("se%63ond::shared", Some("second"), Some("shared")),
        ("first::shared", Some("second"), None),
        ("catalog::raw", Some("second"), Some("catalog::raw")),
        ("second::raw-only", Some("second"), Some("second::raw-only")),
        ("second::shared", Some("second"), None),
        ("first::unknown", Some("second"), None),
        ("second::shared", None, None),
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        db.create(
            &id,
            CreateOptions {
                title: Some("Legacy".into()),
                model: Some(raw.into()),
                ..Default::default()
            },
        )
        .unwrap();
        if let Some(provider) = recorded {
            chat(&id, provider, "shared").await;
        }
        assert_eq!(binding(&id), (None, Some(raw.to_owned())));
        let prior = calls();
        let response = compact(&id).await.unwrap();
        if let Some(model) = expected_model {
            assert!(
                response.status().is_success(),
                "{}",
                response.text().await.unwrap()
            );
            assert_eq!(&calls().1[prior.1.len()..], ["compaction"]);
            assert_eq!(second.compact_models.lock().unwrap().last().unwrap(), model);
        } else {
            assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
            assert!(response
                .text()
                .await
                .unwrap()
                .contains("Pick a model in Desktop"));
            assert_eq!(calls().1, prior.1);
        }
        assert_eq!(calls().0, prior.0);
    }

    let prior = calls();
    let events = chat_with_model(&opaque, "first::shared")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(events.contains("message_stop"), "{events}");
    assert_eq!(calls().0, prior.0);
    assert_eq!(&calls().1[prior.1.len()..], ["turn"]);

    // A disabled provider still claims its picker prefix. The other provider
    // serves the same literal ID and handled the latest turn.
    let disabled_prefix = uuid::Uuid::new_v4().to_string();
    db.create(
        &disabled_prefix,
        CreateOptions {
            title: Some("Disabled picker prefix".into()),
            model: Some("second::shared".into()),
            ..Default::default()
        },
    )
    .unwrap();
    chat(&disabled_prefix, "first", "shared").await;
    let before = calls();
    client
        .patch(format!("{base}/api/providers/second"))
        .header("x-api-key", &key)
        .json(&json!({"enabled":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for id in matrix.iter().chain([&legacy]) {
        let response = compact(id).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        let body: Value = response.json().await.unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("provider second is unavailable"),
            "{body}"
        );
    }

    let conflict = compact(&disabled_prefix).await.unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
    assert!(conflict
        .text()
        .await
        .unwrap()
        .contains("Pick a model in Desktop"));
    assert_eq!(calls(), before);

    let disabled_chat = chat_with_model(&opaque, "first::shared").await.unwrap();
    assert_eq!(disabled_chat.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(calls(), before);

    // Even a single enabled provider with this model cannot establish ownership.
    let empty = uuid::Uuid::new_v4().to_string();
    db.create(
        &empty,
        CreateOptions {
            model: Some("shared".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // An older provider-bearing turn also cannot stand in for a more recent
    // completed turn whose provider is unknown.
    shard
        .write(|w| {
            let turn = w.start_turn()?;
            w.finish_turn(
                &turn,
                deepseek_code_core::db::turn_repo::TurnStatus::Completed,
                1,
            )
        })
        .unwrap();
    for id in [&empty, &legacy] {
        let response = compact(id).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        let body: Value = response.json().await.unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("Pick a model in Desktop"),
            "{body}"
        );
    }
    client
        .delete(format!("{base}/api/providers/second"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let missing = compact(&created).await.unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::CONFLICT);
    assert!(missing.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("provider second is unavailable"));
    assert_eq!(calls(), before);
    // Chat still accepts a different, unqualified model from the caller.
    let events = chat_with_model(&opaque, "first-only")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(events.contains("message_stop"), "{events}");
    assert_eq!(&calls().0[before.0.len()..], ["turn"]);
    assert_eq!(calls().1, before.1);
    server.abort();
    mock.abort();
}
