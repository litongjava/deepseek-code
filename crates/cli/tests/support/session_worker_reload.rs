use super::*;

/// A plugin whose MCP executable is missing must not block chat. Its start
/// failure is announced once, and the next turn does not retry it.
#[tokio::test]
async fn plugin_mcp_start_failure_is_announced_once_and_turns_continue() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let command = temp.path().join("missing-plugin-mcp");
    let source = temp.path().join("broken-plugin.json");
    std::fs::write(
        &source,
        json!({
            "id": "broken",
            "version": "1.0.0",
            "description": "fixture whose MCP executable does not exist",
            "commands": {},
            "files": {},
            "mcp_servers": {"missing": {"command": command.to_string_lossy()}},
        })
        .to_string(),
    )
    .unwrap();
    let context = deepseek_code_core::services::ServiceContext::open(
        paths.clone(),
        Arc::new(deepseek_code_core::services::EnvironmentCredentials),
    )
    .unwrap();
    deepseek_code_core::services::plugins::install(&context, source.to_str().unwrap(), None)
        .await
        .unwrap();
    deepseek_code_core::services::plugins::enable(&context, "broken", true).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let requests = requests.clone();
            move |Json(body): Json<Value>| {
                let requests = requests.clone();
                async move {
                    requests.lock().unwrap().push(body);
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = temp.path().join(if cfg!(windows) {
        "plugin-mcp.exe"
    } else {
        "plugin-mcp"
    });
    std::fs::copy(test_cli_executable(), &executable).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    let mut failures = Vec::new();
    for turn in 0..2 {
        let id = submit(
            &host,
            &endpoint,
            &format!("plugin turn {turn}"),
            "fixture",
            128,
        )
        .await;
        let job = completed(&paths.data_dir, &id).await;
        assert_eq!(job.status, "completed", "turn {turn}: {:?}", job.error);
        let (_, _, events) = jobs::events_since(&paths.data_dir, &id, &session, 0).unwrap();
        failures.extend(
            events
                .iter()
                .filter(|event| event["type"] == "notice")
                .filter_map(|event| event["message"].as_str())
                .filter(|message| message.starts_with("MCP: plugin:broken:missing failed to start"))
                .map(str::to_owned),
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            turn + 1,
            "turn {turn} did not reach the provider"
        );
    }
    assert_eq!(
        failures.len(),
        1,
        "a failed plugin MCP start must be announced once: {failures:?}"
    );
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn worker_owns_mcp_state_and_stops_removed_servers_once() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();

    // An HTTP MCP stub logs `start` when the worker first initializes it and
    // `stop` when the manager ends the session after the config changes.
    let log = temp.path().join("mcp-lifecycle.log");
    let port_file = temp.path().join("mcp.port");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-log-stub.cjs");
    let mut stub = tokio::process::Command::new("node")
        .arg(&script)
        .env("MCP_LOG", &log)
        .env("MCP_PORT_FILE", &port_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&port_file) {
                if let Ok(port) = text.trim().parse::<u16>() {
                    break port;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the MCP stub did not report its port");
    let config = paths.data_dir.join("mcp-servers.json");
    let configure = |servers: Value| std::fs::write(&config, servers.to_string()).unwrap();
    configure(json!({"mcpServers":{"stub":{
        "type":"http", "url":format!("http://127.0.0.1:{port}/mcp"), "enabled":true
    }}}));

    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), workspace.join("unused-lsp")));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = temp.path().join(if cfg!(windows) {
        "mcp-owner.exe"
    } else {
        "mcp-owner"
    });
    std::fs::copy(test_cli_executable(), &executable).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);

    let job = submit(&host, &endpoint, "stub turn one", "fixture", 128).await;
    assert_eq!(
        completed(&paths.data_dir, &job).await.status,
        "completed"
    );
    // The same request `/mcp state <name>` sends: it must see the server the
    // worker started, not a frontend manager that never initialized it.
    let reply = worker::call(
        &host,
        &worker::Request::Mcp(worker::McpOp::State {
            server: Some("stub".into()),
            state: None,
        }),
    )
    .await
    .unwrap();
    match reply.mcp.expect("MCP result") {
        worker::McpResult::State { text, .. } => assert_eq!(text, "stub  user"),
        other => panic!("unexpected MCP result: {other:?}"),
    }

    // Removing the definition and running another turn must stop the child
    // the worker started, exactly once.
    configure(json!({"mcpServers":{}}));
    let job = submit(&host, &endpoint, "stub turn two", "fixture", 128).await;
    assert_eq!(
        completed(&paths.data_dir, &job).await.status,
        "completed"
    );
    let lifecycle = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        lifecycle.lines().collect::<Vec<_>>(),
        ["start", "stop"],
        "{lifecycle}"
    );

    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    stub.kill().await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn running_session_reloads_extensions_on_the_next_request() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let mcp = paths.data_dir.join("mcp-servers.json");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/live-mcp.cjs");
    let configure = |revision, enabled| {
        std::fs::write(
            &mcp,
            json!({"mcpServers":{"live":{"command":"node", "args":[fixture],
            "env":{"REVISION":revision}, "enabled":enabled}}})
            .to_string(),
        )
        .unwrap();
    };
    std::fs::write(&mcp, r#"{"mcpServers":{}}"#).unwrap();
    let skill = workspace.join(".claude/skills/live-skill/SKILL.md");
    let write_skill = |body| {
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            format!("---\nname: live-skill\ndescription: local fixture\n---\n{body}\n"),
        )
        .unwrap();
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), workspace.clone()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let exe = temp.path().join(if cfg!(windows) {
        "reload.exe"
    } else {
        "reload"
    });
    std::fs::copy(test_cli_executable(), &exe).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &exe, &session, &workspace)
        .await
        .unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    let mut first_user = None;
    for turn in 0..5 {
        match turn {
            1 => {
                configure("initial schema", true);
                write_skill("initial instructions");
            }
            2 => {
                configure("replacement schema", true);
                write_skill("edited instructions");
            }
            3 => {
                std::fs::write(
                    paths.data_dir.join("mcp-states.json"),
                    r#"{"servers":{"live":"on-demand"}}"#,
                )
                .unwrap();
            }
            4 => {
                configure("replacement schema", false);
                std::fs::write(
                    paths.data_dir.join("skill-states.json"),
                    r#"{"skills":{"live-skill":"off"}}"#,
                )
                .unwrap();
            }
            _ => {}
        }
        let job = submit(
            &host,
            &endpoint,
            &format!("check extension {turn}"),
            "fixture",
            128,
        )
        .await;
        let result = completed(&paths.data_dir, &job).await;
        assert_eq!(
            result.status, "completed",
            "turn {turn}: {:?}",
            result.error
        );
        let (_, _, events) = jobs::events_since(&paths.data_dir, &job, &session, 0).unwrap();
        let notices: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "notice")
            .filter_map(|e| e["message"].as_str())
            .collect();
        if let Some(expected) = match turn {
            1 => Some("MCP: live started"),
            2 => Some("MCP: live restarted"),
            4 => Some("MCP: live disabled"),
            _ => None,
        } {
            assert!(
                notices.contains(&expected) && notices.contains(&"Skills reloaded"),
                "turn {turn}: {notices:?}"
            );
        }
        let observed = requests.lock().unwrap();
        assert_eq!(observed.len(), turn + 1);
        let request = &observed[turn];
        let messages = request["messages"].as_array().unwrap();
        let users: Vec<_> = messages.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(
            first_user.get_or_insert_with(|| users[0].clone()),
            users[0],
            "first user message changed on turn {turn}"
        );
        let latest = users.last().unwrap()["content"].as_str().unwrap();
        let tool = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| &t["function"])
            .find(|f| f["name"] == "mcp__live__probe");
        match turn {
            1 | 2 => {
                assert!(
                    latest.contains(if turn == 1 {
                        "New MCP tools: mcp__live__probe"
                    } else {
                        "Updated MCP tools: mcp__live__probe"
                    }),
                    "{latest}"
                );
                assert_eq!(
                    tool.expect("resident probe")["description"],
                    if turn == 1 {
                        "initial schema"
                    } else {
                        "replacement schema"
                    }
                );
                assert!(
                    latest.contains(if turn == 1 {
                        "New skills: live-skill"
                    } else {
                        "Updated skills: live-skill"
                    }),
                    "{latest}"
                );
            }
            3 => {
                assert!(tool.is_none());
                assert!(
                    latest.contains("Now loadable: mcp__live__probe"),
                    "{latest}"
                );
            }
            4 => {
                assert!(tool.is_none());
                assert!(
                    latest.contains("Removed skills: live-skill")
                        && latest.contains("No longer available: mcp__live__probe"),
                    "{latest}"
                );
            }
            _ => assert!(tool.is_none()),
        }
    }
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    server.abort();
    let _ = server.await;
}
