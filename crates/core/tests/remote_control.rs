use deepseek_code_core::services::remote_control::{
    mock::MockServer, Bridge, Endpoint, Inbound, Registration, Status, Timing,
};
use serde_json::{json, Value};
use std::time::Duration;

fn timing() -> Timing {
    Timing {
        poll: Duration::from_millis(30),
        keep_alive: Duration::from_millis(150),
        heartbeat: Duration::from_millis(100),
        backoff_start: Duration::from_millis(20),
        backoff_max: Duration::from_millis(40),
        give_up: Duration::from_secs(5),
    }
}

fn registration() -> Registration {
    Registration {
        machine_name: "test-host".into(),
        directory: "/work/project".into(),
        branch: Some("main".into()),
        title: "Mock session".into(),
        permission_mode: Some("default".into()),
        ..Default::default()
    }
}

async fn wait_status(bridge: &mut Bridge, expected: impl Fn(&Status) -> bool) -> Status {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = bridge.status();
            if expected(&status) {
                return status;
            }
            bridge.changed().await;
        }
    })
    .await
    .expect("status not reached")
}

fn paths(snapshot: &Value) -> Vec<String> {
    snapshot["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r["method"].as_str().unwrap(),
                r["path"].as_str().unwrap_or("")
            )
        })
        .collect()
}

#[tokio::test]
async fn bridge_relays_prompts_controls_and_events_then_tears_down() {
    let server = MockServer::start(0, Some("server-token".into()), vec![])
        .await
        .unwrap();
    let endpoint = Endpoint::new(
        &server.url,
        Some("http://localhost:4000"),
        Some("server-token".into()),
    )
    .unwrap();
    let mut bridge = Bridge::connect(endpoint, registration(), timing())
        .await
        .unwrap();
    assert!(bridge
        .url
        .starts_with("http://localhost:4000/code/session_mock_"));
    wait_status(&mut bridge, |s| *s == Status::Connected).await;
    server
        .wait_for(Duration::from_secs(5), |s| s["clients"] == 1)
        .await
        .unwrap();

    // Inbound user message becomes a prompt; echoes of our own events are ignored.
    server.send(json!({"type": "user", "uuid": "remote-1", "message": {"role": "user", "content": [{"type": "text", "text": "hello from phone"}]}}));
    match tokio::time::timeout(Duration::from_secs(5), bridge.next())
        .await
        .unwrap()
        .unwrap()
    {
        Inbound::Prompt(text) => assert_eq!(text, "hello from phone"),
        other => panic!("unexpected {other:?}"),
    }

    // Initialize is answered by the bridge; other controls go to the frontend.
    server.send(json!({"type": "control_request", "request_id": "init-1", "request": {"subtype": "initialize"}}));
    server.send(json!({"type": "control_request", "request_id": "mode-1", "request": {"subtype": "set_permission_mode", "mode": "plan"}}));
    match tokio::time::timeout(Duration::from_secs(5), bridge.next())
        .await
        .unwrap()
        .unwrap()
    {
        Inbound::Control {
            subtype,
            request,
            reply,
        } => {
            assert_eq!(subtype, "set_permission_mode");
            assert_eq!(request["mode"], "plan");
            reply.send(Ok(json!({"mode": "plan"}))).unwrap();
        }
        other => panic!("unexpected {other:?}"),
    }
    server.send(json!({"type": "control_request", "request_id": "bad-1", "request": {"subtype": "set_model", "model": "x"}}));
    match tokio::time::timeout(Duration::from_secs(5), bridge.next())
        .await
        .unwrap()
        .unwrap()
    {
        Inbound::Control { reply, .. } => reply.send(Err("unsupported".into())).unwrap(),
        other => panic!("unexpected {other:?}"),
    }

    bridge.publish_message("user", "local prompt");
    bridge.publish_message("assistant", "local answer");
    let snapshot = server
        .wait_for(Duration::from_secs(5), |s| {
            let events = s["events"].as_array().unwrap();
            events.iter().any(|e| e["type"] == "keep_alive")
                && events
                    .iter()
                    .any(|e| e["response"]["request_id"] == "bad-1")
                && events.iter().any(|e| e["type"] == "assistant")
        })
        .await
        .unwrap();
    let events = snapshot["events"].as_array().unwrap();
    let init = events
        .iter()
        .find(|e| e["response"]["request_id"] == "init-1")
        .unwrap();
    assert_eq!(init["response"]["subtype"], "success");
    let mode = events
        .iter()
        .find(|e| e["response"]["request_id"] == "mode-1")
        .unwrap();
    assert_eq!(mode["response"]["response"]["mode"], "plan");
    let bad = events
        .iter()
        .find(|e| e["response"]["request_id"] == "bad-1")
        .unwrap();
    assert_eq!(bad["response"]["subtype"], "error");
    let assistant = events.iter().find(|e| e["type"] == "assistant").unwrap();
    assert_eq!(assistant["message"]["content"][0]["text"], "local answer");
    assert_eq!(
        assistant["session_id"].as_str(),
        Some(bridge.session_id.as_str())
    );
    // Echo of a published event is not re-injected as a prompt.
    let user = events.iter().find(|e| e["type"] == "user").unwrap().clone();
    server.send(user);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), bridge.next())
            .await
            .is_err()
    );

    let session_id = bridge.session_id.clone();
    let environment_id = bridge.environment_id.clone();
    bridge.disconnect().await.unwrap();
    let snapshot = server.snapshot();
    assert_eq!(snapshot["sessions"][&session_id]["archived"], true);
    assert_eq!(
        snapshot["sessions"][&session_id]["source"],
        "remote-control"
    );
    assert_eq!(snapshot["deregistered"][0], environment_id);
    assert!(snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["type"] == "result"));
    let calls = paths(&snapshot);
    for expected in [
        "POST /v1/environments/bridge",
        "POST /v1/sessions",
        "GET /v1/environments",
        "WS /v1/session_ingress/ws",
        "/ack",
        "/heartbeat",
        "/stop",
        "/archive",
        "DELETE /v1/environments/bridge/",
    ] {
        assert!(
            calls.iter().any(|c| c.contains(expected)),
            "missing {expected} in {calls:?}"
        );
    }
    let register = snapshot["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == "/v1/environments/bridge")
        .unwrap();
    assert_eq!(register["beta"], "environments-2025-11-01");
    assert_eq!(register["body"]["machine_name"], "test-host");
    assert_eq!(register["body"]["metadata"]["worker_type"], "claude_code");
}

#[tokio::test]
async fn bridge_reconnects_after_ingress_drop_and_transient_poll_failures() {
    let server = MockServer::start(0, None, vec![]).await.unwrap();
    let endpoint = Endpoint::new(&server.url, None, None).unwrap();
    let mut bridge = Bridge::connect(endpoint, registration(), timing())
        .await
        .unwrap();
    wait_status(&mut bridge, |s| *s == Status::Connected).await;
    server
        .wait_for(Duration::from_secs(5), |s| s["clients"] == 1)
        .await
        .unwrap();
    server.fail_polls(&[503, 500]);
    assert_eq!(server.drop_clients(), 1);
    wait_status(&mut bridge, |s| *s == Status::Connected).await;
    let snapshot = server
        .wait_for(Duration::from_secs(5), |s| s["clients"] == 1)
        .await
        .unwrap();
    assert!(paths(&snapshot)
        .iter()
        .any(|c| c.contains("/bridge/reconnect")));
    server.send(json!({"type": "user", "message": {"content": "after reconnect"}}));
    match tokio::time::timeout(Duration::from_secs(5), bridge.next())
        .await
        .unwrap()
        .unwrap()
    {
        Inbound::Prompt(text) => assert_eq!(text, "after reconnect"),
        other => panic!("unexpected {other:?}"),
    }
    bridge.disconnect().await.unwrap();
}

#[tokio::test]
async fn fatal_poll_failure_and_bad_token_are_reported() {
    let server = MockServer::start(0, Some("right".into()), vec![])
        .await
        .unwrap();
    let wrong = Endpoint::new(&server.url, None, Some("wrong".into())).unwrap();
    let error = Bridge::connect(wrong, registration(), timing())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("HTTP 401"), "{error:#}");
    assert!(server.snapshot()["sessions"]
        .as_object()
        .unwrap()
        .is_empty());

    let endpoint = Endpoint::new(&server.url, None, Some("right".into())).unwrap();
    server.fail_polls(&[403]);
    let mut bridge = Bridge::connect(endpoint, registration(), timing())
        .await
        .unwrap();
    let status = wait_status(&mut bridge, |s| matches!(s, Status::Failed(_))).await;
    assert!(matches!(status, Status::Failed(m) if m.contains("HTTP 403")));
    assert!(!bridge.is_active());
    let snapshot = server.snapshot();
    assert_eq!(snapshot["deregistered"].as_array().unwrap().len(), 1);
    bridge.disconnect().await.unwrap();
}

#[tokio::test]
async fn tool_permissions_and_interrupts_are_relayed() {
    let server = MockServer::start(0, None, vec![]).await.unwrap();
    let endpoint = Endpoint::new(&server.url, None, None).unwrap();
    let mut bridge = Bridge::connect(endpoint, registration(), timing())
        .await
        .unwrap();
    wait_status(&mut bridge, |s| *s == Status::Connected).await;
    let relay = bridge.relay();
    let request_for = |id: &'static str| {
        move |s: &Value| {
            s["events"].as_array().unwrap().iter().any(|e| {
                e["type"] == "control_request"
                    && e["request_id"] == id
                    && e["request"]["subtype"] == "can_use_tool"
            })
        }
    };

    // The remote client allows the tool.
    let mut allow = relay.request_permission(
        "perm-1",
        "Write",
        &json!({"file_path": "a.txt"}),
        "toolu_1",
        "Allow Write?",
    );
    let snapshot = server
        .wait_for(Duration::from_secs(5), request_for("perm-1"))
        .await
        .unwrap();
    let request = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["request_id"] == "perm-1")
        .unwrap();
    assert_eq!(request["request"]["tool_name"], "Write");
    assert_eq!(request["request"]["input"]["file_path"], "a.txt");
    assert_eq!(request["request"]["tool_use_id"], "toolu_1");
    server.send(json!({"type": "control_response", "response": {"subtype": "success", "request_id": "perm-1", "response": {"behavior": "allow"}}}));
    assert!(
        tokio::time::timeout(Duration::from_secs(5), allow.decision())
            .await
            .unwrap()
    );
    drop(allow);

    // Deny, then an answered request is not cancelled.
    let mut deny = relay.request_permission("perm-2", "Bash", &json!({}), "toolu_2", "Allow?");
    server
        .wait_for(Duration::from_secs(5), request_for("perm-2"))
        .await
        .unwrap();
    server.send(json!({"type": "control_response", "response": {"subtype": "success", "request_id": "perm-2", "response": {"behavior": "deny", "message": "no"}}}));
    assert!(
        !tokio::time::timeout(Duration::from_secs(5), deny.decision())
            .await
            .unwrap()
    );
    drop(deny);

    // An error reply never decides; the local prompt keeps control.
    let mut errored = relay.request_permission("perm-3", "Bash", &json!({}), "toolu_3", "Allow?");
    server
        .wait_for(Duration::from_secs(5), request_for("perm-3"))
        .await
        .unwrap();
    server.send(json!({"type": "control_response", "response": {"subtype": "error", "request_id": "perm-3", "error": "unsupported"}}));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), errored.decision())
            .await
            .is_err()
    );
    drop(errored);

    // A request resolved locally is cancelled on the remote client.
    let pending = relay.request_permission("perm-4", "Bash", &json!({}), "toolu_4", "Allow?");
    server
        .wait_for(Duration::from_secs(5), request_for("perm-4"))
        .await
        .unwrap();
    drop(pending);
    let snapshot = server
        .wait_for(Duration::from_secs(5), |s| {
            s["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "control_cancel_request")
        })
        .await
        .unwrap();
    let cancelled: Vec<_> = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "control_cancel_request")
        .map(|e| e["request_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(cancelled, ["perm-4"]);

    // Interrupts reach observers created before the request only.
    server.send(json!({"type": "control_request", "request_id": "int-0", "request": {"subtype": "interrupt"}}));
    server
        .wait_for(Duration::from_secs(5), |s| {
            s["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["response"]["request_id"] == "int-0")
        })
        .await
        .unwrap();
    let mut interrupts = relay.interrupts();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), interrupts.next())
            .await
            .is_err()
    );
    server.send(json!({"type": "control_request", "request_id": "int-1", "request": {"subtype": "interrupt"}}));
    tokio::time::timeout(Duration::from_secs(5), interrupts.next())
        .await
        .unwrap();
    let snapshot = server
        .wait_for(Duration::from_secs(5), |s| {
            s["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["response"]["request_id"] == "int-1")
        })
        .await
        .unwrap();
    let reply = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["response"]["request_id"] == "int-1")
        .unwrap();
    assert_eq!(reply["response"]["subtype"], "success");
    bridge.disconnect().await.unwrap();
}
