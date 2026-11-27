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
    wait_for_editor_after(tty, result).await;
}

async fn wait_for_editor_after(tty: &mut Terminal, result: &[u8]) {
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

fn definition(log: &Path, id: &str, enabled: bool) -> Value {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    json!({"command":"/bin/sh", "args":[fixtures.join("mcp-owner-launcher.sh"),
        fixtures.join("mcp-owner-stdio.cjs"), log, id, "survive-eof"], "enabled": enabled})
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
    let definition = |id: &str, enabled| definition(&log, id, enabled);
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
    let initial_worker = worker::current(&paths.data_dir, &session).unwrap().unwrap();
    let engine = initial_worker.child_pid.unwrap();
    assert_eq!(
        starts(&log, "aaa").iter().map(|p| p.1).collect::<Vec<_>>(),
        [engine, engine]
    );

    // The worker fetches the prompt before the first chat turn in this terminal.
    let mut definitions: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    definitions["mcpServers"]["later"] = definition("later", true);
    fs::write(&config, definitions.to_string()).unwrap();
    repl(&mut tty, "/mcp__aaa__probe", b"worker result").await;
    let host = worker::current(&paths.data_dir, &session).unwrap().unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    let worker_pid = host.child_pid.unwrap();
    assert_eq!(initial_worker.id, host.id);
    assert!(requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| request["messages"]
            .to_string()
            .contains("expanded owner probe")));
    repl(&mut tty, "/doctor errors", b"No problems found").await;
    repl(&mut tty, "/mcp state later", b"later  user").await;
    assert_eq!(starts(&log, "aaa").last().unwrap().1, worker_pid);

    command(&mut tty, "/mcp", b"Manage MCP servers").await;
    key(&mut tty, b"\r", b"View tools").await;
    key(&mut tty, b"\r", b"owner witness").await;
    key(&mut tty, b"\x1b", b"Manage MCP servers").await;
    key(&mut tty, b"\x1b", b"\x1b[?2004h").await;
    tty.wait_output(PROMPT, true).await;
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
    let focused = worker::call(&host, &worker::Request::Mcp(worker::McpOp::ListServers))
        .await
        .unwrap();
    assert!(
        matches!(focused.mcp, Some(worker::McpResult::Servers { servers, .. }) if servers.iter().any(|s| s.id == "later" && s.running))
    );
    repl(&mut tty, "/focus off", b"focus updated").await;
    repl(&mut tty, "back to worker", b"worker result").await;
    assert_eq!(
        starts(&log, "plugin")
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        [worker_pid; 3]
    );
    assert_eq!(
        starts(&log, "later")
            .iter()
            .map(|p| p.1)
            .collect::<Vec<_>>(),
        [worker_pid]
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
    assert_eq!(starts(&log, "later").len(), 1);

    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    provider.abort();
}

struct OwnerFixture {
    temp: tempfile::TempDir,
    paths: RuntimePaths,
    session: String,
    requests: Arc<Mutex<Vec<Value>>>,
    held: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    provider: tokio::task::JoinHandle<()>,
}

impl OwnerFixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        fs::create_dir(&workspace).unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let session = SessionStore::new(paths.open_database().unwrap())
            .create(&workspace, "fixture-model", "fixture")
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        linux_terminal::write_provider(
            &paths.data_dir,
            &format!("http://{}", listener.local_addr().unwrap()),
        );
        let held = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post({
                    let held = held.clone();
                    let release = release.clone();
                    move |state, body: Json<Value>| {
                        let held = held.clone();
                        let release = release.clone();
                        async move {
                            if body["messages"].as_array().unwrap().last().unwrap()["content"]
                                .to_string()
                                .contains("hold owner turn")
                            {
                                held.notify_one();
                                release.notified().await;
                            }
                            respond(state, body).await
                        }
                    }
                }),
            )
            .with_state((requests.clone(), workspace.join("unused")));
        let provider = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        deepseek_code_core::data_layout::write_config_file(
            paths.data_dir.join("config/startup-prompts.json"),
            json!({"trusted_folders":[workspace]}).to_string(),
        )
        .unwrap();
        let log = temp.path().join("children.jsonl");
        fs::write(
            paths.data_dir.join("mcp-servers.json"),
            json!({"mcpServers":{"w":definition(&log, "w", true)}}).to_string(),
        )
        .unwrap();
        Self {
            temp,
            paths,
            session,
            requests,
            held,
            release,
            provider,
        }
    }

    async fn terminal(&self) -> Terminal {
        let mut command = Terminal::command(
            portable_pty::CommandBuilder::new(test_cli_executable()),
            &self.temp.path().join("work"),
            &self.paths.data_dir,
            &self.temp.path().join("coord"),
            true,
            false,
        );
        command.args(["--resume", &self.session]);
        let mut tty = Terminal::spawn(command);
        tty.wait_output(PROMPT, true).await;
        tty
    }

    fn starts(&self) -> Vec<(u32, u32)> {
        starts(&self.temp.path().join("children.jsonl"), "w")
    }

    fn worker(&self) -> worker::Worker {
        worker::current(&self.paths.data_dir, &self.session)
            .unwrap()
            .unwrap()
    }
}

impl Drop for OwnerFixture {
    fn drop(&mut self) {
        self.provider.abort();
        // Also clean up a surviving child if a regression assertion panics.
        if let Ok(log) = fs::read_to_string(self.temp.path().join("children.jsonl")) {
            for row in log
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            {
                if let Some(pid) = row["pid"].as_u64() {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

fn assert_tree_gone(log: &Path) {
    for line in fs::read_to_string(log).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        for field in ["pid", "launcher"] {
            let pid = row[field].as_u64().unwrap() as u32;
            assert!(worker::process_exited(pid), "{field} {pid} survived");
            eprintln!("MCP stop observed for {} {field} {pid}", row["id"]);
        }
    }
}

async fn crash_takeover(in_flight: bool, separate_session: bool) {
    let fixture = OwnerFixture::new().await;
    if separate_session {
        let config = fixture.paths.data_dir.join("mcp-servers.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        value["mcpServers"]["w"]["env"] = json!({"MCP_FIXTURE_SETSID":"1"});
        fs::write(config, value.to_string()).unwrap();
    }
    let mut tty = fixture.terminal().await;
    repl(&mut tty, "start worker", b"worker result").await;
    let host = fixture.worker();
    let engine = host.child_pid.unwrap() as i32;
    let supervisor = host.supervisor_pid.unwrap() as i32;
    let first = fixture.starts()[0].0 as i32;
    assert_eq!(
        unsafe { libc::getsid(first) },
        if separate_session { first } else { engine }
    );
    repl(&mut tty, "/mcp state w off", b"w  off").await;
    assert!(
        worker::process_exited(first as u32),
        "server {first} remained after disable: {:?}",
        fs::read_to_string(format!("/proc/{first}/stat"))
    );
    repl(&mut tty, "/mcp state w user", b"w  user").await;
    let old = fixture.starts()[1].0;
    if in_flight {
        repl(&mut tty, "/focus on", b"focus updated").await;
        assert_eq!(unsafe { libc::kill(engine, libc::SIGSTOP) }, 0);
        tty.submit("focus submission in flight", false).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGSTOP) }, 0);
    assert_eq!(unsafe { libc::kill(engine, libc::SIGKILL) }, 0);
    // Force the frontend to observe death before the supervisor gets a timeslice.
    let resume = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(unsafe { libc::kill(supervisor, libc::SIGCONT) }, 0);
    });
    if in_flight {
        tty.wait_output(b"\x1b[?2004h", true).await;
        tty.wait_output(PROMPT, true).await;
    }
    repl(&mut tty, "/mcp state w", b"w  user").await;
    resume.await.unwrap();
    let children = fixture.starts();
    assert_eq!(children.len(), 3);
    assert_eq!(children[2].1, fixture.worker().child_pid.unwrap());
    assert_ne!(host.id, fixture.worker().id);
    assert!(worker::process_exited(old));
    tty.submit("/exit", false).await;
    tty.wait_success().await;
    assert_tree_gone(&fixture.temp.path().join("children.jsonl"));
    for (pid, _) in children {
        assert!(worker::process_exited(pid));
    }
}

#[tokio::test]
async fn idle_worker_death_drains_launcher_children_before_restart() {
    crash_takeover(false, false).await;
}

#[tokio::test]
async fn worker_death_during_focus_submit_drains_launcher_children() {
    crash_takeover(true, false).await;
}

#[tokio::test]
async fn separate_session_server_stops_on_disable_worker_death_and_exit() {
    crash_takeover(false, true).await;
}

async fn refused_package_toggle(enabled: bool) {
    use deepseek_code_core::services::{plugins, EnvironmentCredentials, ServiceContext};
    let fixture = OwnerFixture::new().await;
    let context =
        ServiceContext::open(fixture.paths.clone(), Arc::new(EnvironmentCredentials)).unwrap();
    let plugin = fixture.temp.path().join("package.json");
    let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-owner-stdio.cjs");
    fs::write(&plugin, json!({"id":"busy-plugin","description":"busy fixture","version":"1.0.0","commands":{},"files":{},
        "mcp_servers":{"child":{"command":"node","args":[stub,fixture.temp.path().join("children.jsonl"),"plugin"]}}
    }).to_string()).unwrap();
    plugins::install(&context, plugin.to_str().unwrap(), None)
        .await
        .unwrap();
    plugins::enable(&context, "busy-plugin", enabled).unwrap();
    let mut owner = fixture.terminal().await;
    let mut panel = fixture.terminal().await;
    repl(&mut owner, "initialize worker", b"worker result").await;
    if enabled {
        command(&mut panel, "/plugins", b"busy-plugin").await;
        panel.send(b"\x1b[B");
    } else {
        command(&mut panel, "/plugins", b"Show disabled").await;
        key(&mut panel, b"\r", b"Disabled plugins").await;
        panel.send(b"\x1b[B\x1b[B");
    }
    owner.submit("hold owner turn", false).await;
    tokio::time::timeout(Duration::from_secs(10), fixture.held.notified())
        .await
        .unwrap();
    key(&mut panel, b" ", b"MCP is busy").await;
    assert_eq!(
        plugins::list(&context)
            .unwrap()
            .iter()
            .find(|p| p.id == "busy-plugin")
            .unwrap()
            .enabled,
        enabled
    );
    fixture.release.notify_one();
    owner.wait_output(b"worker result", true).await;
    owner.wait_output(b"\x1b[?2004h", true).await;
    let host = fixture.worker();
    let servers = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match worker::call(&host, &worker::Request::Mcp(worker::McpOp::ListServers))
                .await
                .unwrap()
                .mcp
            {
                Some(worker::McpResult::Servers { servers, .. }) => break servers,
                Some(worker::McpResult::Busy) => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                other => panic!("unexpected worker state: {other:?}"),
            }
        }
    })
    .await
    .expect("worker did not finish the held turn");
    assert_eq!(
        servers
            .iter()
            .any(|s| s.id == "plugin:busy-plugin:child" && s.running),
        enabled
    );
    worker::shutdown(&fixture.paths.data_dir, &fixture.session)
        .await
        .unwrap();
}

#[tokio::test]
async fn busy_panel_enable_preserves_catalog_and_runtime() {
    refused_package_toggle(false).await;
}

#[tokio::test]
async fn busy_panel_disable_preserves_catalog_and_runtime() {
    refused_package_toggle(true).await;
}

#[tokio::test]
async fn slow_remove_and_lost_reconnect_reply_apply_once() {
    use tokio::io::AsyncWriteExt;
    let fixture = OwnerFixture::new().await;
    let mut tty = fixture.terminal().await;
    repl(&mut tty, "start worker", b"worker result").await;
    let host = fixture.worker();
    let http_log = fixture.temp.path().join("http.log");
    let port_file = fixture.temp.path().join("http.port");
    let mut server = tokio::process::Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-log-stub.cjs"))
        .env("MCP_LOG", &http_log)
        .env("MCP_PORT_FILE", &port_file)
        .env("MCP_INIT_DELAY", "7000")
        .env("MCP_DELETE_DELAY", "0")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(port) = fs::read_to_string(&port_file) {
                break port;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let path = fixture.paths.data_dir.join("mcp-servers.json");
    let mut defs: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    defs["mcpServers"]["http"] =
        json!({"type":"http","url":format!("http://127.0.0.1:{port}"),"enabled":true});
    fs::write(&path, defs.to_string()).unwrap();
    defs["mcpServers"]["keep"] =
        definition(&fixture.temp.path().join("children.jsonl"), "keep", true);
    fs::write(&path, defs.to_string()).unwrap();
    repl(&mut tty, "/mcp remove w", b"Removed w").await;
    assert!(worker::process_exited(fixture.starts()[0].0));
    assert!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["mcpServers"]
            .get("w")
            .is_none()
    );
    let id = uuid::Uuid::new_v4();
    let restart = worker::Request::McpTracked {
        id,
        operation: worker::McpOp::Reconnect {
            server: "keep".into(),
        },
    };
    let mut stream = tokio::net::UnixStream::connect(&host.endpoint)
        .await
        .unwrap();
    let mut bytes = serde_json::to_vec(&restart).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    drop(stream);
    let outcome = worker::Request::McpOutcome { id };
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if matches!(
                worker::call(&host, &outcome).await.unwrap().mcp,
                Some(worker::McpResult::Message { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        worker::call(&host, &restart).await.unwrap().mcp,
        Some(worker::McpResult::Message { .. })
    ));
    let children = starts(&fixture.temp.path().join("children.jsonl"), "keep");
    assert_eq!(children.len(), 2, "one initial server and one reconnect");
    assert!(worker::process_exited(children[0].0));
    assert!(!worker::process_exited(children[1].0));
    server.kill().await.unwrap();
    tty.submit("/exit", false).await;
    tty.wait_success().await;
}

#[tokio::test]
async fn malformed_hot_edit_keeps_provider_tools_in_both_focus_modes() {
    let fixture = OwnerFixture::new().await;
    let mut tty = fixture.terminal().await;
    repl(&mut tty, "/mcp state w", b"w  user").await;
    let config = fixture.paths.data_dir.join("mcp-servers.json");
    fs::write(&config, "{").unwrap();
    repl(
        &mut tty,
        "worker with last good definitions",
        b"worker result",
    )
    .await;
    let host = fixture.worker();
    let job = jobs::list(&fixture.paths.data_dir, Some(&fixture.session))
        .unwrap()
        .into_iter()
        .find(|j| j.title == "worker with last good definitions")
        .unwrap();
    let (_, _, events) =
        jobs::events_since(&fixture.paths.data_dir, &job.id, &fixture.session, 0).unwrap();
    assert!(events.iter().any(|e| e["message"]
        .as_str()
        .is_some_and(|s| s.contains("MCP reload:"))));
    repl(&mut tty, "/focus on", b"focus updated").await;
    repl(
        &mut tty,
        "local with last good definitions",
        b"worker result",
    )
    .await;
    assert!(String::from_utf8_lossy(&tty.transcript).contains("MCP reload:"));
    assert_eq!(
        fixture.starts().iter().map(|p| p.1).collect::<Vec<_>>(),
        [host.child_pid.unwrap()]
    );
    let requests = fixture.requests.lock().unwrap();
    for prompt in [
        "worker with last good definitions",
        "local with last good definitions",
    ] {
        assert!(
            requests.iter().any(|r| {
                r["messages"].as_array().unwrap().last().unwrap()["content"]
                    .to_string()
                    .contains(prompt)
                    && r["tools"].as_array().is_some_and(|tools| {
                        tools
                            .iter()
                            .any(|t| t["function"]["name"] == "mcp__w__probe")
                    })
            }),
            "provider did not receive mcp__w__probe on {prompt}"
        );
    }
    drop(requests);
    assert_eq!(fs::read_to_string(config).unwrap(), "{");
    tty.submit("/exit", false).await;
    tty.wait_success().await;
}

#[tokio::test]
async fn two_frontends_and_a_frontend_crash_preserve_the_same_worker() {
    let fixture = OwnerFixture::new().await;
    let mut first = fixture.terminal().await;
    let mut second = fixture.terminal().await;
    repl(&mut first, "/mcp state w", b"w  user").await;
    repl(&mut second, "/mcp state w", b"w  user").await;
    assert_eq!(fixture.starts().len(), 1);
    repl(&mut second, "first worker transfer", b"worker result").await;
    let engine = fixture.worker().child_pid.unwrap();
    repl(&mut first, "/focus on", b"focus updated").await;
    repl(&mut first, "focused local turn", b"worker result").await;
    repl(&mut second, "/mcp state w", b"w  user").await;
    assert_eq!(fixture.starts().last().unwrap().1, engine);
    repl(&mut second, "/focus off", b"focus updated").await;
    repl(&mut second, "second worker transfer", b"worker result").await;
    repl(&mut first, "/mcp state w", b"w  user").await;
    let children = fixture.starts();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].1, engine);
    // Kill only the original frontend. The worker was started by that process.
    assert_eq!(unsafe { libc::kill(first.pid() as i32, libc::SIGKILL) }, 0);
    repl(&mut second, "/mcp state w", b"w  user").await;
    assert_eq!(fixture.starts(), children);
    second.submit("/exit", false).await;
    second.wait_success().await;
    assert_tree_gone(&fixture.temp.path().join("children.jsonl"));
}

#[tokio::test]
async fn focus_modes_send_live_argument_schema_and_preserve_sibling() {
    let fixture = OwnerFixture::new().await;
    let log = fixture.temp.path().join("children.jsonl");
    let path = fixture.paths.data_dir.join("mcp-servers.json");
    // An older binary may have left a first-seen contract for this session.
    let snapshots = fixture.paths.data_dir.join("cache/mcp-tool-snapshots");
    fs::create_dir_all(&snapshots).unwrap();
    fs::write(snapshots.join(format!("{}.json", fixture.session)), json!({
        "mcp__w__probe":{"name":"mcp__w__probe","description":"legacy","input_schema":{"type":"object","required":["legacy"]}}
    }).to_string()).unwrap();
    let mut tty = fixture.terminal().await;
    for (focus, field) in [("off", "alpha"), ("on", "beta"), ("off", "gamma")] {
        repl(&mut tty, &format!("/focus {focus}"), b"focus updated").await;
        let mut server = definition(&log, "w", true);
        server["env"] = json!({"PROBE_FIELD":field});
        let mut sibling = definition(&log, "sibling", true);
        sibling["env"] = json!({"PROBE_FIELD":"constant"});
        fs::write(
            &path,
            json!({"mcpServers":{"w":server,"sibling":sibling}}).to_string(),
        )
        .unwrap();
        let prompt = format!("inspect {field}");
        repl(&mut tty, &prompt, b"worker result").await;
        let requests = fixture.requests.lock().unwrap();
        let request = requests
            .iter()
            .find(|r| {
                let last = r["messages"].as_array().unwrap().last().unwrap();
                r["stream"] == true
                    && last["role"] == "user"
                    && last["content"]
                        .as_str()
                        .is_some_and(|content| prompt_text(content) == prompt)
            })
            .unwrap_or_else(|| panic!("No streamed chat request for {prompt}"));
        for (name, expected) in [
            ("mcp__w__probe", field),
            ("mcp__sibling__probe", "constant"),
        ] {
            let tool = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .unwrap();
            assert_eq!(tool["function"]["description"], expected);
            assert_eq!(
                tool["function"]["parameters"]["required"],
                json!([expected])
            );
        }
    }
    assert_eq!(starts(&log, "sibling").len(), 1);
    assert_eq!(starts(&log, "w").len(), 3);
    let image = linux_terminal::image_prompt(&fixture.temp.path().join("work"), "inspect image");
    repl(&mut tty, &image, b"worker result").await;
    assert!(fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(
            |request| request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "image_url"))
        ));
    assert_eq!(starts(&log, "w").len(), 3);
    tty.submit("/exit", false).await;
    tty.wait_success().await;
    assert_tree_gone(&log);
}

#[tokio::test]
async fn rejected_plugin_write_preserves_setting_and_next_provider_turn() {
    use deepseek_code_core::services::{plugins, EnvironmentCredentials, ServiceContext};
    let fixture = OwnerFixture::new().await;
    let log = fixture.temp.path().join("children.jsonl");
    let package = fixture.temp.path().join("package.json");
    fs::write(&package, json!({"id":"transaction","version":"1.0.0","description":"plugin toggle","commands":{},"files":{},
        "mcp_servers":{"child":definition(&log,"plugin",true)}}).to_string()).unwrap();
    let context =
        ServiceContext::open(fixture.paths.clone(), Arc::new(EnvironmentCredentials)).unwrap();
    plugins::install(&context, package.to_str().unwrap(), None)
        .await
        .unwrap();
    let mut tty = fixture.terminal().await;
    repl(&mut tty, "initialize plugin", b"worker result").await;
    let before = starts(&log, "plugin");
    let path = fixture.paths.data_dir.join("mcp-servers.json");
    fs::write(&path, "{").unwrap();
    repl(
        &mut tty,
        "/plugin disable transaction",
        b"configuration changed in another process",
    )
    .await;
    assert!(
        plugins::list(&context)
            .unwrap()
            .into_iter()
            .find(|p| p.id == "transaction")
            .unwrap()
            .enabled
    );
    repl(&mut tty, "after rejected toggle", b"worker result").await;
    let tool = deepseek_code_core::mcp::tool_executor::qualified_tool_name(
        "plugin:transaction:child",
        "probe",
    );
    assert!(fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(
            |r| r["messages"].as_array().unwrap().last().unwrap()["content"]
                .to_string()
                .contains("after rejected toggle")
                && r["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == tool)
        ));
    assert_eq!(fs::read_to_string(path).unwrap(), "{");
    assert_eq!(starts(&log, "plugin"), before);
    tty.submit("/exit", false).await;
    tty.wait_success().await;
}

#[tokio::test]
async fn advertised_restart_obeys_turn_admission() {
    let fixture = OwnerFixture::new().await;
    let mut tty = fixture.terminal().await;
    repl(&mut tty, "initialize worker", b"worker result").await;
    let original = fixture.starts();
    tty.submit("hold owner turn", false).await;
    tokio::time::timeout(Duration::from_secs(10), fixture.held.notified())
        .await
        .unwrap();
    let restart = || {
        let mut cmd = tokio::process::Command::new(test_cli_executable());
        cmd.arg("--data-dir")
            .arg(&fixture.paths.data_dir)
            .args(["mcp", "restart", "w"]);
        cmd
    };
    let refused = tokio::time::timeout(Duration::from_secs(4), restart().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("MCP is busy"),
        "{refused:?}"
    );
    assert_eq!(fixture.starts(), original);
    assert!(!worker::process_exited(original[0].0));
    tty.clear_output();
    fixture.release.notify_one();
    tty.wait_output(b"worker result", true).await;
    wait_for_editor_after(&mut tty, b"worker result").await;
    let accepted = restart().output().await.unwrap();
    assert!(accepted.status.success(), "{accepted:?}");
    assert_eq!(fixture.starts().len(), 2);
    assert!(worker::process_exited(original[0].0));
    tty.submit("/exit", false).await;
    tty.wait_success().await;
}

#[tokio::test]
async fn plain_tty_modes_render_worker_output() {
    for screen_reader in [true, false] {
        let fixture = OwnerFixture::new().await;
        let mut launch = Terminal::command(
            portable_pty::CommandBuilder::new(test_cli_executable()),
            &fixture.temp.path().join("work"),
            &fixture.paths.data_dir,
            &fixture.temp.path().join("coord"),
            true,
            screen_reader,
        );
        if !screen_reader {
            launch.env("TERM", "dumb");
        }
        launch.args(["--resume", &fixture.session]);
        let mut tty = Terminal::spawn(launch);
        tty.wait_output(b"> ", true).await;
        tty.submit("plain worker prompt", true).await;
        tty.wait_output(b"worker result", true).await;
        assert_eq!(fixture.starts()[0].1, fixture.worker().child_pid.unwrap());
        tty.wait_output(b"> ", true).await;
        tty.submit("/exit", true).await;
        tty.wait_success().await;
    }
}
