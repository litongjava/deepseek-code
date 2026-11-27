//! Drive the interactive frontend and observe the independent stdio children.
use super::*;
#[allow(dead_code)]
#[path = "linux_terminal.rs"]
mod linux_terminal;
use linux_terminal::{Terminal, PROMPT};
use std::fs;

async fn key(tty: &mut Terminal, input: &[u8], expected: &[u8]) {
    tty.clear_output();
    tty.send(input);
    tty.wait_output(expected, true).await;
}

async fn command(tty: &mut Terminal, command: &str, expected: &[u8]) {
    tty.submit(command, false).await;
    tty.wait_output(expected, true).await;
}

// A streamed answer and command output both precede editor activation. Only
// type the next command once the terminal has re-entered bracketed paste mode.
async fn repl(tty: &mut Terminal, line: &str, result: &[u8]) {
    command(tty, line, result).await;
    let after = tty
        .transcript
        .windows(result.len())
        .position(|s| s == result)
        .unwrap()
        + result.len();
    tty.wait_output_since(after, b"\x1b[?2004h", true).await;
    tty.wait_output_since(after, PROMPT, true).await;
}

fn starts(log: &Path, id: &str) -> Vec<(u32, u32)> {
    fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["id"] == id)
        .map(|row| {
            assert_eq!(row["overlap"], false, "duplicate MCP child: {row}");
            (
                row["pid"].as_u64().unwrap() as u32,
                row["owner"].as_u64().unwrap() as u32,
            )
        })
        .collect()
}

#[tokio::test]
async fn prompt_picker_panels_and_focus_share_one_owner() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let session = SessionStore::new(paths.open_database().unwrap())
        .create(&workspace, "fixture-model", "fixture")
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), workspace.join("unused")));
    let provider = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    linux_terminal::write_provider(&paths.data_dir, &endpoint);
    deepseek_code_core::data_layout::write_config_file(
        paths.data_dir.join("config/startup-prompts.json"),
        json!({"trusted_folders":[workspace]}).to_string(),
    )
    .unwrap();

    let log = temp.path().join("children.jsonl");
    let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-owner-stdio.cjs");
    let definition =
        |id: &str, enabled| json!({"command":"node", "args":[stub, log, id], "enabled":enabled});
    let config = paths.data_dir.join("mcp-servers.json");
    fs::write(
        &config,
        json!({"mcpServers":{"aaa":definition("aaa", false)}}).to_string(),
    )
    .unwrap();
    let plugin = temp.path().join("fixture.json");
    fs::write(&plugin, json!({"id":"owner-plugin","version":"1.0.0","description":"ownership fixture","commands":{},"files":{},"mcp_servers":{"child":definition("plugin", true)}}).to_string()).unwrap();
    let context = deepseek_code_core::services::ServiceContext::open(
        paths.clone(),
        Arc::new(deepseek_code_core::services::EnvironmentCredentials),
    )
    .unwrap();
    deepseek_code_core::services::plugins::install(&context, plugin.to_str().unwrap(), None)
        .await
        .unwrap();
    deepseek_code_core::services::plugins::enable(&context, "owner-plugin", true).unwrap();

    let mut launch = Terminal::command(
        portable_pty::CommandBuilder::new(test_cli_executable()),
        &workspace,
        &paths.data_dir,
        &temp.path().join("coord"),
        true,
        false,
    );
    launch.args(["--resume", &session]);
    let mut tty = Terminal::spawn(launch);
    tty.wait_output(PROMPT, true).await;

    command(&mut tty, "/mcp", b"Manage MCP servers").await;
    key(&mut tty, b"\r", b"Enable").await;
    key(&mut tty, b"\r", b"Enabled aaa.").await;
    tty.wait_output(b"Manage MCP servers", true).await;
    key(&mut tty, b"\r", b"Reconnect").await;
    key(&mut tty, b"\x1b[B\x1b[B\r", b"Reconnected to aaa.").await;
    tty.wait_output(b"Manage MCP servers", true).await;
    key(&mut tty, b"\x1b", b"\x1b[?2004h").await;
    tty.wait_output(PROMPT, true).await;
    assert_eq!(
        starts(&log, "aaa").iter().map(|p| p.1).collect::<Vec<_>>(),
        [tty.pid(), tty.pid()]
    );

    // Edit disk after frontend startup. The prompt is expanded locally before
    // its first model turn is delegated, in this same terminal.
    let mut definitions: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    definitions["mcpServers"]["later"] = definition("later", true);
    fs::write(&config, definitions.to_string()).unwrap();
    repl(&mut tty, "/mcp__aaa__probe", b"worker result").await;
    let host = worker::current(&paths.data_dir, &session).unwrap().unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    let worker_pid = host.child_pid.unwrap();
    assert!(requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| request["messages"]
            .to_string()
            .contains("expanded owner probe")));
    repl(&mut tty, "/mcp state later", b"later  user").await;
    assert_eq!(starts(&log, "aaa").last().unwrap().1, worker_pid);

    command(&mut tty, "/tools", b"[Built-in]").await;
    key(&mut tty, b"\x1b[C", b"[Skills]").await;
    key(&mut tty, b"\x1b[C", b"[Mcp]").await;
    key(&mut tty, b" ", b"Show disabled").await;
    key(&mut tty, b"\x1b", b"\x1b[?2004h").await;
    tty.wait_output(PROMPT, true).await;
    repl(&mut tty, "/mcp state aaa", b"aaa  off").await;
    let panel_state = worker::call(&host, &worker::Request::Mcp(worker::McpOp::ListServers))
        .await
        .unwrap();
    assert!(
        matches!(panel_state.mcp, Some(worker::McpResult::Servers { servers, .. })
        if servers.iter().any(|s| s.id == "aaa" && !s.enabled && !s.running))
    );
    repl(&mut tty, "/reload-plugins", b"Reloaded 1 enabled plugins").await;
    assert_eq!(
        starts(&log, "plugin")
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        [worker_pid]
    );
    command(&mut tty, "/plugins", b"owner-plugin").await;
    key(&mut tty, b"\x1b[B ", b"Show disabled").await;
    key(&mut tty, b"\r", b"Disabled plugins").await;
    key(&mut tty, b"\x1b[B\x1b[B ", b"never used").await;
    key(&mut tty, b"\x1b", b"\x1b[?2004h").await;
    tty.wait_output(PROMPT, true).await;
    assert_eq!(
        starts(&log, "plugin")
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        [worker_pid, worker_pid]
    );

    let child = starts(&log, "plugin").last().unwrap().0;
    assert_eq!(unsafe { libc::kill(child as i32, libc::SIGKILL) }, 0);
    repl(&mut tty, "recover plugin child", b"worker result").await;
    let (_, _, events) = {
        let job = jobs::list(&paths.data_dir, Some(&session))
            .unwrap()
            .into_iter()
            .find(|job| job.title == "recover plugin child")
            .unwrap();
        jobs::events_since(&paths.data_dir, &job.id, &session, 0).unwrap()
    };
    assert_eq!(
        events
            .iter()
            .filter(|e| e["message"] == "MCP: plugin:owner-plugin:child restarted")
            .count(),
        1
    );

    repl(&mut tty, "/focus on", b"focus updated").await;
    repl(&mut tty, "local focus turn", b"worker result").await;
    let released = worker::call(&host, &worker::Request::Mcp(worker::McpOp::ListServers))
        .await
        .unwrap();
    assert!(matches!(released.mcp, Some(worker::McpResult::Unavailable)));
    repl(&mut tty, "/focus off", b"focus updated").await;
    repl(&mut tty, "back to worker", b"worker result").await;
    assert_eq!(
        starts(&log, "plugin")
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        [worker_pid, worker_pid, worker_pid, tty.pid(), worker_pid]
    );
    assert_eq!(
        starts(&log, "later")
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        [tty.pid(), worker_pid, tty.pid(), worker_pid]
    );
    let last = starts(&log, "later").last().unwrap().0;
    let mut definitions: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    definitions["mcpServers"]
        .as_object_mut()
        .unwrap()
        .remove("later");
    fs::write(&config, definitions.to_string()).unwrap();
    repl(&mut tty, "apply removal", b"worker result").await;
    assert!(worker::process_exited(last));
    assert_eq!(starts(&log, "later").len(), 4);

    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    provider.abort();
}
