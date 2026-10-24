//! Status, settings and queue control requests, idle and during a turn.
use super::*;

fn control(id: &str, request: Value) -> Value {
    json!({"type":"control_request","request_id":id,"request":request})
}

async fn answer(client: &mut Client, id: &str, request: Value) -> Value {
    client.send(control(id, request)).await;
    let frame = client.frame("control_response").await;
    assert_eq!(frame["response"]["request_id"], id, "{frame}");
    frame["response"].clone()
}

#[tokio::test]
async fn idle_status_settings_and_thinking_controls_report_real_state() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        std::fs::write(root.path().join("data/settings.json"), json!({"theme":"dark","permissions":{"allow":["Read"]}}).to_string()).unwrap();
        let server = Server::start(anthropic, Mode::Text).await;
        let mut client = Client::start(root.path(), &server, &[]);
        let status = answer(&mut client, "mcp", json!({"subtype":"mcp_status"})).await;
        assert_eq!(status["subtype"], "success");
        assert!(status["response"]["mcpServers"].is_array());
        let usage = answer(&mut client, "usage-0", json!({"subtype":"get_context_usage"})).await;
        assert_eq!(usage["response"]["measured"], false, "{usage}");
        client.send(user("hello")).await;
        assert_eq!(client.result().await["is_error"], false);
        let usage = answer(&mut client, "usage-1", json!({"subtype":"get_context_usage"})).await;
        assert_eq!(usage["response"]["measured"], true, "{usage}");
        assert!(usage["response"]["totalTokens"].as_u64().unwrap() > 0, "{usage}");
        assert!(usage["response"]["categories"].as_array().unwrap().iter().any(|c| c["name"] == "Messages"));

        let bad = answer(&mut client, "flags-bad", json!({"subtype":"apply_flag_settings","settings":{"theme":"light"}})).await;
        assert_eq!(bad["subtype"], "error");
        assert!(bad["error"].as_str().unwrap().contains("Unsupported flag setting: theme"));
        let applied = answer(&mut client, "flags", json!({"subtype":"apply_flag_settings","settings":{"effortLevel":"high","permissions":{"defaultMode":"plan"}}})).await;
        assert_eq!(applied["subtype"], "success", "{applied}");
        let settings = answer(&mut client, "settings", json!({"subtype":"get_settings"})).await["response"].clone();
        assert_eq!(settings["effective"]["theme"], "dark");
        assert_eq!(settings["effective"]["permissions"]["allow"], json!(["Read"]));
        assert_eq!(settings["effective"]["permissions"]["defaultMode"], "plan");
        let sources = settings["sources"].as_array().unwrap().iter().map(|s| s["source"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(sources, ["userSettings", "projectSettings", "localSettings", "flagSettings"]);
        assert_eq!(settings["applied"]["model"], "fixture-model");
        assert_eq!(settings["applied"]["effort"], "high");

        let thinking = answer(&mut client, "think", json!({"subtype":"set_max_thinking_tokens","max_thinking_tokens":2048})).await;
        assert_eq!(thinking["subtype"], "success");
        let settings = answer(&mut client, "settings-2", json!({"subtype":"get_settings"})).await;
        assert_eq!(settings["response"]["applied"]["effort"], "low");
        let invalid = answer(&mut client, "think-bad", json!({"subtype":"set_max_thinking_tokens","max_thinking_tokens":"lots"})).await;
        assert_eq!(invalid["subtype"], "error");
        let cancel = answer(&mut client, "cancel-idle", json!({"subtype":"cancel_async_message","message_uuid":"missing"})).await;
        assert_eq!(cancel["response"]["cancelled"], false);
        let (status, results) = client.finish().await;
        assert!(status.success());
        assert!(results.is_empty(), "{results:?}");
    }
}

#[tokio::test]
async fn running_turn_answers_queries_and_cancels_only_queued_messages() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(anthropic, Mode::SlowText).await;
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("first")).await;
        client.frame("message_start").await;
        let mut queued = user("QUEUED_SECOND_SENTINEL");
        queued["uuid"] = json!("queued-2");
        client.send(queued).await;
        let mut kept = user("QUEUED_THIRD_SENTINEL");
        kept["uuid"] = json!("queued-3");
        client.send(kept).await;
        let status = answer(&mut client, "mcp", json!({"subtype":"mcp_status"})).await;
        assert_eq!(status["subtype"], "success", "{status}");
        let settings = answer(&mut client, "settings", json!({"subtype":"get_settings"})).await;
        assert_eq!(settings["subtype"], "success", "{settings}");
        let mode = answer(&mut client, "mode", json!({"subtype":"set_permission_mode","mode":"acceptEdits"})).await;
        assert_eq!(mode["subtype"], "success", "{mode}");
        let model = answer(&mut client, "model", json!({"subtype":"set_model","model":"fixture-model"})).await;
        assert_eq!(model["subtype"], "error");
        let cancel = answer(&mut client, "cancel", json!({"subtype":"cancel_async_message","message_uuid":"queued-2"})).await;
        assert_eq!(cancel["response"]["cancelled"], true, "{cancel}");
        let again = answer(&mut client, "cancel-again", json!({"subtype":"cancel_async_message","message_uuid":"queued-2"})).await;
        assert_eq!(again["response"]["cancelled"], false);
        let (status, results) = client.finish().await;
        assert!(status.success());
        assert_eq!(results.len(), 2, "the cancelled message never runs: {results:?}");
        assert!(results.iter().all(|result| result["is_error"] == false), "{results:?}");
        let prompts = server.fixture.requests.lock().unwrap().iter().filter(|body| body["stream"] == true).map(|body| body.to_string()).collect::<Vec<_>>();
        assert!(prompts.iter().all(|body| !body.contains("QUEUED_SECOND_SENTINEL")), "{prompts:?}");
        assert!(prompts.iter().any(|body| body.contains("QUEUED_THIRD_SENTINEL")));
    }
}
