use super::*;
use crate::services::input_queue;
use base64::Engine;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=";

fn image() -> Value {
    json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}})
}

fn mixed_content() -> Value {
    json!([{"type":"text","text":"look [Image #7]"}, image(),
        {"type":"text","text":"then [Image #2]"},
        {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")},"fileName":"截图.png","size":base64::engine::general_purpose::STANDARD.decode(PNG).unwrap().len()}])
}

fn provider(base: &str, format: &str) -> ResolvedProvider {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| crate::config::OrchestrationConfig::init_global().unwrap());
    ResolvedProvider {
        provider: serde_json::from_value(
            json!({"id":"images-test","name":"images-test","base_url":base,
            "api_key":"local-test-key","api_format":format,"models":[],"enabled":true}),
        )
        .unwrap(),
        model: serde_json::from_value(
            json!({"id":"images-test","name":"images-test","enabled":true,
            "supports_vision":false,"supports_web_search":false}),
        )
        .unwrap(),
    }
}

fn executor(
    base: &str,
    format: &str,
    content: Value,
) -> (ToolLoopExecutor, mpsc::Receiver<EngineEvent>) {
    let (tx, rx) = mpsc::channel(128);
    (
        ToolLoopExecutor::new(
            provider(base, format),
            vec![json!({"role":"user","content":content})],
            None,
            128,
            tx,
            ".".into(),
            ReasoningMode::Standard,
        ),
        rx,
    )
}

fn assert_content(content: &Value, anthropic: bool) {
    assert_eq!(content[0]["text"], "look [Image #7]");
    assert_eq!(content[2]["text"], "then [Image #2]");
    for index in [1, 3] {
        assert!(content[index].get("fileName").is_none());
        assert!(content[index].get("size").is_none());
        if anthropic {
            assert_eq!(content[index]["source"]["data"], PNG);
        } else {
            assert_eq!(
                content[index]["image_url"]["url"],
                format!("data:image/png;base64,{PNG}")
            );
        }
    }
}

#[tokio::test]
async fn first_send_raw_cli_images_reach_actual_model_requests() {
    use axum::{routing::post, Json, Router};
    for format in ["openai", "anthropic"] {
        let anthropic = format == "anthropic";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sent, mut captured) = mpsc::channel(1);
        let app = Router::new().route(if anthropic { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
            let sent = sent.clone();
            async move {
                sent.send(body).await.unwrap();
                let response = if anthropic {
                    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                };
                ([("content-type", "text/event-stream")], response)
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // This is raw image_block_from_file format, not the normalized database copy.
        let (mut engine, _events) = executor(&base, format, mixed_content());
        let result = if anthropic {
            engine.execute_anthropic_loop().await
        } else {
            engine.execute_openai_loop().await
        };
        server.abort();
        assert_eq!(result.unwrap().0, "done");
        let request = captured.recv().await.unwrap();
        assert_content(&request["messages"][0]["content"], anthropic);
    }
}

#[tokio::test]
async fn protocol_normalization_and_retained_compression_preserve_images() {
    for anthropic in [false, true] {
        let (engine, _events) = executor("http://127.0.0.1:1", "openai", mixed_content());
        let messages: Vec<Value> = if anthropic {
            engine
                .build_anthropic_messages()
                .unwrap()
                .iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect()
        } else {
            engine
                .build_openai_messages()
                .unwrap()
                .iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect()
        };
        for level in [
            CompressionLevel::Light,
            CompressionLevel::Medium,
            CompressionLevel::Heavy,
            CompressionLevel::Critical,
        ] {
            let (compressed, _) = ContextCompressor::new(None)
                .compress(messages.clone(), None, level, None)
                .await;
            let retained = compressed
                .iter()
                .find(|message| message["role"] == "user")
                .unwrap();
            let content = if anthropic {
                let message: AnthropicMessage = serde_json::from_value(retained.clone()).unwrap();
                serde_json::to_value(message.content).unwrap()
            } else {
                let message: OpenAIMessage = serde_json::from_value(retained.clone()).unwrap();
                serde_json::to_value(message.content).unwrap()
            };
            assert_content(&content, anthropic);
        }
        let (engine, _events) = executor(
            "http://127.0.0.1:1",
            "openai",
            json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"broken"}}]),
        );
        assert!(engine.build_openai_messages().is_err());
        assert!(engine.build_anthropic_messages().is_err());
    }
}

#[tokio::test]
async fn light_compression_keeps_structured_assistant_and_tool_pair() {
    let messages = vec![
        json!({"role":"assistant","content":[image(), {"type":"tool_use","id":"call","name":"Read","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"result"}]}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"openai-call","type":"function","function":{"name":"Read","arguments":"{}"}}]}),
    ];
    let (compressed, _) = ContextCompressor::new(None)
        .compress(messages.clone(), None, CompressionLevel::Light, None)
        .await;
    assert_eq!(compressed, messages);
}

#[tokio::test]
async fn durable_steer_handoff_waits_for_boundary_and_pause_preserves_next_turn() {
    let temp = tempfile::tempdir().unwrap();
    let paths = crate::runtime::RuntimePaths::new(temp.path().to_owned()).unwrap();
    let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(temp.path(), "images-test", "images-test")
        .unwrap();
    let next = input_queue::enqueue(
        &store.db,
        &id,
        "next",
        json!("later"),
        input_queue::QueueMode::NextTurn,
    )
    .unwrap();
    let steer = input_queue::enqueue(
        &store.db,
        &id,
        "steer",
        json!("now"),
        input_queue::QueueMode::Steer,
    )
    .unwrap();
    let (mut engine, _events) = executor("http://127.0.0.1:1", "openai", json!("initial"));
    engine.goal_db = Some(store.db.clone());
    engine.conv_id = Some(id.clone());
    assert!(engine.deliver_queued_inputs(false).await.is_empty());
    input_queue::set_paused(&store.db, &id, true).unwrap();
    assert!(engine.deliver_queued_inputs(true).await.is_empty());
    input_queue::set_paused(&store.db, &id, false).unwrap();
    let delivered = engine.deliver_queued_inputs(true).await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].content, json!("now"));
    assert_eq!(delivered[0].origin, format!("queue-input-{}", steer.id));
    assert!(engine.deliver_queued_inputs(true).await.is_empty());
    let rows = input_queue::list(&store.db, &id).unwrap();
    assert_eq!(
        rows.iter().find(|row| row.id == next.id).unwrap().state,
        input_queue::QueueState::Queued
    );
    assert_eq!(
        rows.iter().find(|row| row.id == steer.id).unwrap().state,
        input_queue::QueueState::Handoff
    );
    assert_eq!(
        store
            .db
            .with_conn(|conn| crate::db::message_repo::get_message(conn, &delivered[0].origin))
            .unwrap()
            .unwrap()
            .unwrap()
            .content,
        "now"
    );
}

#[tokio::test]
async fn queued_images_replay_after_file_removal_without_becoming_approval_notes() {
    let temp = tempfile::tempdir().unwrap();
    let paths = crate::runtime::RuntimePaths::new(temp.path().to_path_buf()).unwrap();
    let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(temp.path(), "images-test", "images-test")
        .unwrap();
    let png = temp.path().join("shot.png");
    std::fs::write(
        &png,
        base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap(),
    )
    .unwrap();
    input_queue::save_pasted_images(
        temp.path(),
        &id,
        &[temp.path().join("missing.png"), png.clone()],
    )
    .unwrap();
    input_queue::push(&store.db, &id, "look [Image #2]").unwrap();
    let notice = format!(
        "{} [Image #2] internal",
        crate::tools::background_shell::NOTIFICATION_START
    );
    input_queue::push(&store.db, &id, &notice).unwrap();
    let (mut engine, mut events) = executor("http://127.0.0.1:1", "openai", json!("initial"));
    engine.goal_db = Some(store.db.clone());
    engine.conv_id = Some(id.clone());
    engine.search_config_path = Some(temp.path().join("config.json"));
    assert!(engine.deliver_queued_inputs(false).await.is_empty());
    let inputs = engine.deliver_queued_inputs(true).await;
    assert_eq!(inputs.len(), 2);
    assert!(engine.deliver_queued_inputs(true).await.is_empty());
    match events.recv().await.unwrap() {
        EngineEvent::QueuedInput(input) => assert_eq!(input, inputs[0]),
        event => panic!("Unexpected event: {event:?}"),
    }
    assert_eq!(inputs[0].origin, "user");
    assert_eq!(inputs[1].origin, "task_notification");
    assert_eq!(inputs[1].content, notice);
    let assistant = store.append(&id, "assistant", "").unwrap();
    store
        .db
        .with_conn(|conn| -> Result<()> {
            crate::db::message_repo::insert_tool_call(
                conn,
                "call",
                &assistant,
                "Read",
                Some("{}"),
                Some("result"),
                false,
                0,
            )?;
            crate::db::message_repo::set_tool_call_note(conn, "call", "approval remains separate")?;
            input_queue::save_delivered(conn, "call", &inputs)?;
            // Final assistant archiving must not cascade-delete streamed sidecars.
            crate::db::message_repo::insert_tool_call(
                conn,
                "call",
                &assistant,
                "Read",
                Some("{}"),
                Some("result"),
                false,
                0,
            )?;
            assert_eq!(input_queue::load_delivered(conn, "call")?, inputs);
            Ok(())
        })
        .unwrap()
        .unwrap();
    std::fs::remove_file(png).unwrap();
    drop(engine);
    drop(store);
    let reopened = crate::runtime::SessionStore::new(paths.open_database().unwrap());
    let fork = reopened.fork(&id).unwrap();
    for session in [&id, &fork] {
        for format in [
            crate::native_engine::provider_manager::ApiFormat::OpenAI,
            crate::native_engine::provider_manager::ApiFormat::Anthropic,
        ] {
            let history = reopened.model_history(session, &format).unwrap();
            let queued = history
                .iter()
                .find(|message| message["origin"] == "user")
                .unwrap();
            assert_eq!(queued["content"], inputs[0].content);
            assert!(history
                .iter()
                .any(|message| message["origin"] == "task_notification"
                    && message["content"] == notice));
            assert!(serde_json::to_string(&history)
                .unwrap()
                .contains("approval remains separate"));
        }
    }
}
