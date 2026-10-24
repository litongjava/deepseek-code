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

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "user.email=fixture@example.invalid", "-c", "user.name=fixture"])
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[tokio::test]
async fn rewind_files_previews_then_restores_the_message_checkpoint() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "data/\n").unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["add", ".gitignore"]);
        git(root.path(), &["commit", "-q", "-m", "fixture"]);
        let server = Server::start(anthropic, Mode::Approval).await;
        let mut client = Client::start(root.path(), &server, &["--replay-user-messages"]);
        let missing = answer(&mut client, "none", json!({"subtype":"rewind_files","user_message_id":"edit-1"})).await;
        assert_eq!(missing["response"]["canRewind"], false, "{missing}");
        let mut edit = user("Write the fixture file");
        edit["uuid"] = json!("edit-1");
        client.send(edit).await;
        let replay = client.frame("user").await;
        assert_eq!(replay["uuid"], "edit-1");
        assert_eq!(client.result().await["is_error"], false);
        assert_eq!(std::fs::read_to_string(root.path().join("approval.txt")).unwrap(), "ACTUAL_APPROVED_WRITE");
        let preview = answer(&mut client, "dry", json!({"subtype":"rewind_files","user_message_id":"edit-1","dry_run":true})).await;
        assert_eq!(preview["response"]["canRewind"], true, "{preview}");
        assert_eq!(preview["response"]["filesChanged"].as_array().unwrap().len(), 1, "{preview}");
        assert_eq!(preview["response"]["deletions"], 1);
        assert!(root.path().join("approval.txt").exists(), "dry run must not change files");
        let restored = answer(&mut client, "real", json!({"subtype":"rewind_files","user_message_id":"edit-1"})).await;
        assert_eq!(restored["response"]["canRewind"], true, "{restored}");
        assert!(!root.path().join("approval.txt").exists());
        let again = answer(&mut client, "again", json!({"subtype":"rewind_files","user_message_id":"edit-1"})).await;
        assert_eq!(again["response"]["canRewind"], false);
        client.finish().await;
    }
}

#[tokio::test]
async fn resource_controls_fail_explicitly_for_unknown_or_unsupported_targets() {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(false, Mode::Text).await;
    let mut client = Client::start(root.path(), &server, &[]);
    for (id, request, message) in [
        ("toggle", json!({"subtype":"mcp_toggle","serverName":"absent","enabled":true}), "MCP server not found: absent"),
        ("reconnect", json!({"subtype":"mcp_reconnect","serverName":"absent"}), "MCP server not found: absent"),
        ("set", json!({"subtype":"mcp_set_servers","servers":{}}), "mcp_set_servers is not supported"),
        ("stop", json!({"subtype":"stop_task","task_id":"absent"}), "Task not found: absent"),
        ("seed", json!({"subtype":"seed_read_state","path":"a.txt"}), "mtime must be a number"),
    ] {
        let frame = answer(&mut client, id, request).await;
        assert_eq!(frame["subtype"], "error", "{frame}");
        assert!(frame["error"].as_str().unwrap().contains(message), "{frame}");
    }
    let seeded = answer(&mut client, "seed-ok", json!({"subtype":"seed_read_state","path":"a.txt","mtime":1})).await;
    assert_eq!(seeded["subtype"], "success");
    let reloaded = answer(&mut client, "reload", json!({"subtype":"reload_plugins"})).await;
    assert_eq!(reloaded["subtype"], "success", "{reloaded}");
    for key in ["commands", "agents", "plugins", "mcpServers"] {
        assert!(reloaded["response"][key].is_array(), "{key}: {reloaded}");
    }
    client.finish().await;
}

#[tokio::test]
async fn mcp_toggle_and_reconnect_drive_a_configured_stdio_server() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("data")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elicitation.cjs");
    std::fs::write(root.path().join("data/mcp-servers.json"), json!({"mcpServers":{"fixture":{"command":"node","args":[script],"env":{"ELICITATION_RESULT":root.path().join("reply.json")},"enabled":true}}}).to_string()).unwrap();
    let server = Server::start(false, Mode::Text).await;
    let mut client = Client::start(root.path(), &server, &[]);
    let status = |frame: Value| frame["response"]["mcpServers"].as_array().unwrap().iter().find(|s| s["name"] == "fixture").cloned().unwrap();
    assert_eq!(status(answer(&mut client, "s0", json!({"subtype":"mcp_status"})).await)["status"], "connected");
    let off = answer(&mut client, "off", json!({"subtype":"mcp_toggle","serverName":"fixture","enabled":false})).await;
    assert_eq!(off["subtype"], "success", "{off}");
    assert_eq!(status(answer(&mut client, "s1", json!({"subtype":"mcp_status"})).await)["status"], "disabled");
    let reconnect = answer(&mut client, "r0", json!({"subtype":"mcp_reconnect","serverName":"fixture"})).await;
    assert!(reconnect["error"].as_str().unwrap().contains("disabled"), "{reconnect}");
    answer(&mut client, "on", json!({"subtype":"mcp_toggle","serverName":"fixture","enabled":true})).await;
    assert_eq!(status(answer(&mut client, "s2", json!({"subtype":"mcp_status"})).await)["status"], "connected");
    let reconnect = answer(&mut client, "r1", json!({"subtype":"mcp_reconnect","serverName":"fixture"})).await;
    assert_eq!(reconnect["subtype"], "success", "{reconnect}");
    assert_eq!(status(answer(&mut client, "s3", json!({"subtype":"mcp_status"})).await)["status"], "connected");
    client.finish().await;
}
