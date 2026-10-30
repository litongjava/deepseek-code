//! Real supervisor/host IPC with a local model endpoint and an actual MCP child.
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{background_sessions as jobs, session_worker as worker},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Cleanup(Vec<u32>);

fn test_cli_executable() -> std::path::PathBuf {
    std::env::var_os("CLAUDE_SESSION_WORKER_TEST_EXE")
        .map(Into::into)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into())
}

#[path = "support/session_worker_idle.rs"]
mod idle;

#[tokio::test]
async fn live_mode_change_applies_to_tool_after_inflight_model_request() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = Router::new().route("/v1/chat/completions", post({
        let started=started.clone(); let release=release.clone(); let calls=calls.clone();
        move || { let started=started.clone(); let release=release.clone(); let calls=calls.clone(); async move {
            let first = calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==0;
            let (delta, reason) = if first {
                started.notify_one(); release.notified().await;
                (json!({"tool_calls":[{"index":0,"id":"mode-write","type":"function","function":{"name":"Write","arguments":"{\"path\":\"must-not-exist.txt\",\"content\":\"blocked\"}"}}]}), "tool_calls")
            } else { (json!({"content":"finished"}), "stop") };
            ([("content-type","text/event-stream")], format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})))
        }}
    }));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("http://{}",listener.local_addr().unwrap());
    let server=tokio::spawn(async move { axum::serve(listener,app).await.unwrap() });
    let executable=temp.path().join(if cfg!(windows){"live-mode.exe"}else{"live-mode"});
    std::fs::copy(test_cli_executable(),&executable).unwrap();
    let host=worker::ensure_worker(&paths.data_dir,&executable,&session,&workspace).await.unwrap();
    let _cleanup=Cleanup(vec![host.supervisor_pid.unwrap()]);
    let id=submit(&host,&endpoint,"check mode","fixture",128).await;
    tokio::time::timeout(Duration::from_secs(10),started.notified()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2),worker::set_permission_mode(&paths.data_dir,&session,&id,"plan")).await.unwrap().unwrap();
    release.notify_one();
    assert_eq!(completed(&paths.data_dir,&id).await.status,"completed");
    assert!(!workspace.join("must-not-exist.txt").exists());
    let (_,_,events)=jobs::events_since(&paths.data_dir,&id,&session,0).unwrap();
    assert!(events.iter().any(|event|event["type"]=="tool_result" && event["id"]=="mode-write" && event["is_error"]==true));
    worker::shutdown(&paths.data_dir,&session).await.unwrap();
    server.abort();
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.0 {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill.exe")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .creation_flags(0x08000000)
                    .output();
            }
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output();
            }
        }
    }
}

/// The user's own text: context reminders (instructions, recalled memories)
/// may come before or after it in the same message.
fn prompt_text(content: &str) -> String {
    let mut text = content.to_owned();
    while let (Some(start), Some(end)) = (text.find("<system-reminder>"), text.find("</system-reminder>")) {
        if end < start { break; }
        text.replace_range(start..end + "</system-reminder>".len(), "");
    }
    text.trim().to_owned()
}

async fn respond(
    State((requests, lsp)): State<(Arc<Mutex<Vec<Value>>>, std::path::PathBuf)>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let slow = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .is_some_and(|s| prompt_text(s).contains("slow"));
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let lsp_turn = last["role"] == "user"
        && last["content"]
            .as_str()
            .is_some_and(|s| matches!(prompt_text(s).as_str(), "first turn" | "second turn"));
    requests.lock().unwrap().push(body);
    if lsp_turn {
        let arguments=json!({"server":"node","args":[lsp],"filePath":"fixture.rs","operation":"hover","line":1,"character":1}).to_string();
        return (
            [("content-type", "text/event-stream")],
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"lsp-fixture","type":"function","function":{"name":"LSP","arguments":arguments}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
            ),
        );
    }
    if slow {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"worker result"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        ),
    )
}

async fn completed(root: &Path, id: &str) -> jobs::Job {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let job = jobs::get(root, id).unwrap();
            if job.finished_ms.is_some() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("job did not finish")
}

async fn submit(
    host: &worker::Worker,
    endpoint: &str,
    prompt: &str,
    model: &str,
    max_tokens: u32,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let arguments = vec![
        "--data-dir".into(),
        host.root.to_string_lossy().into_owned(),
        "--resume".into(),
        host.session_id.clone(),
        "--base-url".into(),
        format!("{endpoint}/v1"),
        "--api-format".into(),
        "openai".into(),
        "--api-key-env".into(),
        "CLAUDE_RUST_CHILD_API_KEY".into(),
        "--model".into(),
        model.into(),
        "--max-tokens".into(),
        max_tokens.to_string(),
        "--permission-mode".into(),
        "bypass".into(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--".into(),
        prompt.into(),
    ];
    let mut environment = vec![(
        "CLAUDE_RUST_CHILD_API_KEY".into(),
        "fixture-secret-only-in-memory".into(),
    )];
    if let Some(base) =
        deepseek_code_core::native_engine::provider_manager::one_million_context_base(model)
    {
        environment.push(("CLAUDE_RUST_WORKER_MODEL".into(),json!({"id":base,"name":base,"enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":1_000_000,"max_tokens":max_tokens}).to_string()));
    }
    worker::call(
        host,
        &worker::Request::Submit {
            id: id.clone(),
            prompt: prompt.into(),
            arguments,
            environment,
        },
    )
    .await
    .unwrap();
    id
}

fn assert_no_secret(path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_secret(&path);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains("fixture-secret-only-in-memory"),
                "secret persisted in {}",
                path.display()
            );
        }
    }
}

async fn scope_respond(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let calls = if last["role"] == "user" {
        last["content"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(&prompt_text(s)).ok())
    } else {
        None
    };
    requests.lock().unwrap().push(body);
    let (delta, finish) = if let Some(calls) = calls {
        let calls: Vec<_> = calls.as_array().unwrap().iter().enumerate().map(|(i, call)| {
            json!({"index":i,"id":format!("scope-{i}"),"type":"function","function":{"name":call["name"],"arguments":call["input"].to_string()}})
        }).collect();
        (json!({"tool_calls": calls}), "tool_calls")
    } else {
        (json!({"content":"scope complete"}), "stop")
    };
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
        ),
    )
}

#[tokio::test]
async fn added_directory_survives_worker_resume_without_granting_other_sessions() {
    // Windows temp is inside AppData, an independently permitted root. Keep the
    // fixture outside it so denial assertions actually exercise the allowlist.
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = temp.path().join("workspace");
    let allowed = temp.path().join("allowed");
    let denied = temp.path().join("allowed-neighbor");
    for path in [&workspace, &allowed, &denied] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(allowed.join("source.txt"), "permitted-source").unwrap();
    std::fs::write(denied.join("source.txt"), "private-neighbor").unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store
        .create(&workspace, "fixture-one", "cli-runtime")
        .unwrap();
    let other = store
        .create(&workspace, "fixture-one", "cli-runtime")
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(scope_respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = temp.path().join(if cfg!(windows) {
        "scope-worker.exe"
    } else {
        "scope-worker"
    });
    std::fs::copy(test_cli_executable(), &executable).unwrap();
    let output = tokio::process::Command::new(&executable)
        .current_dir(&workspace)
        .args([
            "--data-dir",
            paths.data_dir.to_str().unwrap(),
            "--resume",
            &session,
            "--add-dir",
            allowed.to_str().unwrap(),
            "--base-url",
            &format!("{endpoint}/v1"),
            "--api-format",
            "openai",
            "--api-key-env",
            "CLAUDE_RUST_CHILD_API_KEY",
            "--model",
            "fixture-one",
            "--permission-mode",
            "bypass",
            "--print",
            "--",
            "setup",
        ])
        .env("CLAUDE_RUST_CHILD_API_KEY", "fixture-secret-only-in-memory")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut cleanup = Cleanup(Vec::new());
    for (id, granted) in [(&session, true), (&other, false)] {
        let host = worker::ensure_worker(&paths.data_dir, &executable, id, &workspace)
            .await
            .unwrap();
        cleanup.0.push(host.supervisor_pid.unwrap());
        let calls = json!([
            {"name":"Read","input":{"file_path":allowed.join("source.txt")}},
            {"name":"Write","input":{"file_path":allowed.join(if granted {"written.txt"} else {"forbidden.txt"}),"content":"persisted"}},
            {"name":"Read","input":{"file_path":denied.join("source.txt")}}
        ]);
        let job = submit(&host, &endpoint, &calls.to_string(), "fixture-one", 128).await;
        assert_eq!(completed(&paths.data_dir, &job).await.status, "completed");
        let observed = requests.lock().unwrap().last().unwrap().clone();
        let results: Vec<_> = observed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        assert_eq!(results.len(), 3, "{observed}");
        let event_log = std::fs::read_to_string(
            paths
                .data_dir
                .join("background-tasks")
                .join(&job)
                .join("output.jsonl"),
        )
        .unwrap();
        let completions: Vec<serde_json::Value> = event_log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["type"] == "tool_result")
            .collect();
        assert_eq!(
            completions.len(),
            3,
            "Each executed or denied tool has one completion"
        );
        let ids: std::collections::HashSet<_> = completions
            .iter()
            .map(|event| event["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            results[0]["content"]
                .to_string()
                .contains("permitted-source"),
            granted,
            "{results:?}"
        );
        assert!(
            !results[2]["content"]
                .to_string()
                .contains("private-neighbor"),
            "{results:?}"
        );
        if granted {
            let before = requests.lock().unwrap().len();
            let child_calls = json!([
                {"name":"Read","input":{"file_path":allowed.join("source.txt")}},
                {"name":"Read","input":{"file_path":denied.join("source.txt")}}
            ]);
            let delegate = json!([{"name":"Agent","input":{"prompt":child_calls.to_string(),"agent_type":"general","run_in_background":false}}]);
            let job = submit(&host, &endpoint, &delegate.to_string(), "fixture-one", 128).await;
            assert_eq!(completed(&paths.data_dir, &job).await.status, "completed");
            let requests = requests.lock().unwrap();
            let child = requests[before..]
                .iter()
                .find(|request| {
                    request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|m| m["role"] == "tool")
                        .count()
                        == 2
                })
                .expect("delegated agent must execute its own tool calls");
            let results: Vec<_> = child["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .collect();
            assert!(results[0]["content"]
                .to_string()
                .contains("permitted-source"));
            assert!(!results[1]["content"]
                .to_string()
                .contains("private-neighbor"));
        }
        worker::shutdown(&paths.data_dir, id).await.unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(allowed.join("written.txt")).unwrap(),
        "persisted"
    );
    assert!(!allowed.join("forbidden.txt").exists());
    server.abort();
}

#[tokio::test]
async fn persistent_engine_reuses_mcp_refreshes_settings_and_recovers_after_crash_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store
        .create(&workspace, "fixture-one", "cli-runtime")
        .unwrap();
    let hook_service = deepseek_code_core::services::ServiceContext {
        paths: paths.clone(), db: store.db.clone(), credentials: Arc::new(deepseek_code_core::services::EnvironmentCredentials),
    };
    std::fs::write(workspace.join("lifecycle.cjs"), "require('fs').appendFileSync('lifecycle-count','start\\n'); console.log('WORKER_HOOK_CONTEXT');").unwrap();
    std::fs::write(paths.data_dir.join("settings.json"), json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"node lifecycle.cjs"}]}]}}).to_string()).unwrap();
    deepseek_code_core::services::session_hooks::start(&hook_service, &workspace, &session, "startup", "fixture-one").await.unwrap();
    let marker = temp.path().join("starts.txt");
    let script = temp.path().join("mcp.cjs");
    std::fs::write(&script,r#"require('fs').appendFileSync(process.env.START_MARKER,'start\n');require('readline').createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);if(r.id!==undefined)process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result:{protocolVersion:'2024-11-05',capabilities:{},serverInfo:{name:'fixture',version:'1'},tools:[],resources:[]}})+'\n');});"#).unwrap();
    std::fs::write(paths.data_dir.join("mcp-servers.json"),serde_json::to_vec(&json!({"mcpServers":{"fixture":{"command":"node","args":[script],"enabled":true,"env":{"START_MARKER":marker}}}})).unwrap()).unwrap();
    let lsp = temp.path().join("lsp.cjs");
    let lsp_marker = temp.path().join("lsp-starts.txt");
    std::fs::write(workspace.join("fixture.rs"), "fn main() {}\n").unwrap();
    let lsp_script = r#"require('fs').appendFileSync(MARKER,'start\n');let buffer=Buffer.alloc(0);process.stdin.on('data',chunk=>{buffer=Buffer.concat([buffer,chunk]);while(true){let end=buffer.indexOf('\r\n\r\n');if(end<0)return;let length=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);if(buffer.length<end+4+length)return;let m=JSON.parse(buffer.subarray(end+4,end+4+length));buffer=buffer.subarray(end+4+length);if(m.id){let result=m.method==='initialize'?{capabilities:{}}:m.method==='shutdown'?null:{contents:'persistent hover'};let body=JSON.stringify({jsonrpc:'2.0',id:m.id,result});process.stdout.write('Content-Length: '+Buffer.byteLength(body)+'\r\n\r\n'+body);}if(m.method==='exit')process.exit(0);}});"#;
    std::fs::write(
        &lsp,
        lsp_script.replace("MARKER", &serde_json::to_string(&lsp_marker).unwrap()),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), lsp));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable_copy = temp.path().join(if cfg!(windows) {
        "worker-fixture.exe"
    } else {
        "worker-fixture"
    });
    std::fs::copy(test_cli_executable(), &executable_copy).unwrap();
    let executable = executable_copy.as_path();
    let host = worker::ensure_worker(&paths.data_dir, executable, &session, &workspace)
        .await
        .unwrap();
    let initial_status=worker::runtime_status(&paths.data_dir,&session).await;
    assert_eq!(initial_status.state,"reachable");
    assert!(!initial_status.tools.unwrap().mcp_initialized);
    assert!(!marker.exists(),"status must not launch MCP");
    let mut cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    for (prompt, model, tokens) in [
        ("first turn", "fixture-one", 64),
        ("second turn", "fixture-two[1m]", 96),
    ] {
        let id = submit(&host, &endpoint, prompt, model, tokens).await;
        let job = completed(&paths.data_dir, &id).await;
        assert_eq!(job.status, "completed", "{:?}", job.error);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "start\n");
        let observed = worker::ensure_worker(&paths.data_dir, executable, &session, &workspace)
            .await
            .unwrap();
        assert_eq!(observed.child_pid, host.child_pid);
    }
    {
        let requests = requests.lock().unwrap();
        assert_eq!(std::fs::read_to_string(workspace.join("lifecycle-count")).unwrap(), "start\n");
        assert!(requests[0]["messages"][0]["content"].as_str().unwrap().contains("WORKER_HOOK_CONTEXT"));
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[2]["model"], "fixture-two");
        assert_eq!(requests[2]["max_tokens"], 96);
        assert!(requests[2]["messages"].to_string().contains("first turn"));
    }
    assert_eq!(
        std::fs::read_to_string(&lsp_marker).unwrap(),
        "start\n",
        "two tool turns reuse one language server"
    );
    let tool_status=worker::runtime_status(&paths.data_dir,&session).await;
    assert_eq!(tool_status.state,"reachable");
    let tool_status=tool_status.tools.unwrap();
    assert!(tool_status.mcp_initialized);
    assert!(tool_status.mcp.iter().any(|server|server.name=="fixture" && server.state=="manager_reports_running"));
    assert_eq!(tool_status.lsp_pooled.len(),1);
    assert!(!serde_json::to_string(&tool_status).unwrap().contains("START_MARKER"));
    let slow = submit(&host, &endpoint, "slow cancellation", "fixture-two", 96).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while requests.lock().unwrap().len() < 5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let active_status=worker::runtime_status(&paths.data_dir,&session).await;
    assert_eq!(active_status.state,"reachable", "runtime status must not wait for the active turn's Session mutex");
    assert!(active_status.tools.unwrap().mcp_initialized);
    tokio::time::timeout(Duration::from_secs(2), worker::set_permission_mode(&paths.data_dir, &session, &slow, "plan")).await.unwrap().unwrap();
    assert_eq!(deepseek_code_core::services::session_permission_mode::load(&store.db, &session).unwrap(), Some(deepseek_code_core::permissions::PermissionMode::PlanMode));
    assert_eq!(worker::permission_mode(&paths.data_dir, &session, &slow).await.unwrap(), "plan");
    worker::set_permission_mode(&paths.data_dir, &session, &slow, "accept-edits").await.unwrap();
    // Query after an older mode event has been queued must return live authority.
    assert_eq!(worker::permission_mode(&paths.data_dir, &session, &slow).await.unwrap(), "accept-edits");
    worker::set_permission_mode(&paths.data_dir, &session, &slow, "dontAsk").await.unwrap();
    assert_eq!(worker::permission_mode(&paths.data_dir, &session, &slow).await.unwrap(), "dont-ask");
    assert_eq!(deepseek_code_core::services::session_permission_mode::load(&store.db, &session).unwrap(), Some(deepseek_code_core::permissions::PermissionMode::DontAsk));
    assert!(worker::set_permission_mode(&paths.data_dir, &session, &slow, "auto").await.is_err());
    assert_eq!(worker::permission_mode(&paths.data_dir, &session, &slow).await.unwrap(), "dont-ask");
    assert!(worker::set_permission_mode(&paths.data_dir, &session, &slow, "bypass").await.is_err());
    assert!(worker::call(&host, &worker::Request::SetPermissionMode { job_id: uuid::Uuid::new_v4().to_string(), mode:"ask".into() }).await.is_err());
    jobs::cancel(&paths.data_dir, &slow).unwrap();
    assert_eq!(completed(&paths.data_dir, &slow).await.status, "cancelled");
    assert!(worker::set_permission_mode(&paths.data_dir, &session, &slow, "ask").await.is_err());
    let next = submit(&host, &endpoint, "after cancellation", "fixture-two", 96).await;
    assert_eq!(completed(&paths.data_dir, &next).await.status, "completed");
    let observed = worker::read_worker(&worker::directory(&host)).unwrap();
    assert_eq!(
        observed.child_pid, host.child_pid,
        "cooperative cancellation must preserve the engine"
    );
    let crash = submit(&host, &endpoint, "slow crash", "fixture-two", 96).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while requests.lock().unwrap().len() < 7 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("taskkill.exe")
            .args(["/PID", &host.child_pid.unwrap().to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-KILL", &host.child_pid.unwrap().to_string()])
            .output()
            .unwrap();
    }
    assert_eq!(completed(&paths.data_dir, &crash).await.status, "failed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let recovered = worker::ensure_worker(&paths.data_dir, executable, &session, &workspace)
        .await
        .unwrap();
    cleanup.0.push(recovered.supervisor_pid.unwrap());
    assert_ne!(recovered.child_pid, host.child_pid);
    let last = submit(
        &recovered,
        &endpoint,
        "explicit recovery",
        "fixture-two",
        96,
    )
    .await;
    assert_eq!(completed(&paths.data_dir, &last).await.status, "completed");
    assert_eq!(
        requests.lock().unwrap().len(),
        8,
        "no replay after cancellation or crash"
    );
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_no_secret(&paths.data_dir.join("session-workers"));
    assert_no_secret(&paths.data_dir.join("background-tasks"));
    server.abort();
}

#[path = "support/session_worker_launch_rules.rs"]
mod launch_rules;
