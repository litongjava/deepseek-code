//! Linux PTY acceptance through local model HTTP, real CLI persistent workers,
//! ordinary tool dispatch and durable per-call records. Never starts pty_owner directly.
#![cfg(target_os = "linux")]
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::session_worker as worker,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Model {
    actions: VecDeque<(String, String, Value)>,
    results: BTreeMap<String, Value>,
    tasks: BTreeMap<String, String>,
    requests: Vec<Value>,
}
type Fixture = Arc<Mutex<Model>>;
async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut f = fixture.lock().unwrap();
    for message in body["messages"].as_array().unwrap() {
        if message["role"] == "tool" {
            let id = message["tool_call_id"].as_str().unwrap().to_owned();
            let text = message["content"].as_str().unwrap_or("");
            let value: Value = serde_json::from_str(text).unwrap_or(json!({"error":text}));
            if let Some(task) = value["task_id"].as_str() {
                f.tasks.insert(id.clone(), task.into());
            }
            f.results.insert(id, value);
        }
    }
    f.requests.push(body);
    let frames = if let Some((id, name, mut args)) = f.actions.pop_front() {
        if let Some(source) = args["task_id"].as_str().and_then(|s| s.strip_prefix('$')) {
            args["task_id"] = json!(f
                .tasks
                .get(source)
                .unwrap_or_else(|| panic!("missing task {source}: {:?}", f.results)));
        }
        json!([
            {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":null}]},
            {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}
        ])
    } else {
        json!([
            {"choices":[{"index":0,"delta":{"content":"PTY fixture turn complete"},"finish_reason":null}]},
            {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}
        ])
    };
    let mut stream = String::new();
    for frame in frames.as_array().unwrap() {
        stream.push_str(&format!("data: {frame}\n\n"));
    }
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}
fn identity(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let parts: Vec<_> = stat.rsplit(')').next()?.split_whitespace().collect();
    Some((parts[0].chars().next()?, parts[19].parse().ok()?))
}
fn exited(pid: u32, tick: u64) -> bool {
    identity(pid).is_none_or(|(state, t)| t != tick || state == 'Z')
}
struct ExactCleanup(Vec<(u32, u64)>);
impl ExactCleanup {
    fn add(&mut self, pid: u32) {
        if let Some((_, tick)) = identity(pid) {
            self.0.push((pid, tick));
        }
    }
}
impl Drop for ExactCleanup {
    fn drop(&mut self) {
        for (pid, tick) in &self.0 {
            if !exited(*pid, *tick) {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", "--", &pid.to_string()])
                    .status();
            }
        }
    }
}
struct Harness {
    _temp: tempfile::TempDir,
    data: PathBuf,
    workspace: PathBuf,
    host: worker::Worker,
    endpoint: String,
    model: Fixture,
    cleanup: ExactCleanup,
    sequence: usize,
}
impl Harness {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        Self::at(temp, data, workspace).await
    }
    async fn other_session(&self) -> Self {
        Self::at(
            tempfile::tempdir().unwrap(),
            self.data.clone(),
            self.workspace.clone(),
        )
        .await
    }
    async fn at(temp: tempfile::TempDir, data: PathBuf, workspace: PathBuf) -> Self {
        let paths = RuntimePaths::new(data.clone()).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
        let model = Fixture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(respond))
            .with_state(model.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let host = worker::ensure_worker(
            &data,
            &PathBuf::from(env!("CARGO_BIN_EXE_dcl")),
            &session,
            &workspace,
        )
        .await
        .unwrap();
        let mut cleanup = ExactCleanup(vec![]);
        cleanup.add(host.supervisor_pid.unwrap());
        cleanup.add(host.child_pid.unwrap());
        println!(
            "PTY_WORKER session={session} engine={} supervisor={} data={}",
            host.child_pid.unwrap(),
            host.supervisor_pid.unwrap(),
            data.display()
        );
        Self {
            _temp: temp,
            data,
            workspace,
            host,
            endpoint,
            model,
            cleanup,
            sequence: 0,
        }
    }
    async fn turn(&mut self, mode: &str, actions: Vec<(&str, &str, Value)>) {
        self.sequence += 1;
        {
            let mut f = self.model.lock().unwrap();
            assert!(f.actions.is_empty());
            f.actions.extend(
                actions
                    .into_iter()
                    .map(|(id, name, args)| (id.into(), name.into(), args)),
            );
        }
        let prompt = format!("PTY acceptance turn {}", self.sequence);
        let id = uuid::Uuid::new_v4().to_string();
        let arguments = vec![
            "--data-dir".into(),
            self.data.display().to_string(),
            "--resume".into(),
            self.host.session_id.clone(),
            "--base-url".into(),
            self.endpoint.clone(),
            "--api-format".into(),
            "openai".into(),
            "--api-key-env".into(),
            "DEEPSEEK_CODE_CHILD_API_KEY".into(),
            "--model".into(),
            "fixture".into(),
            "--max-tokens".into(),
            "4096".into(),
            "--permission-mode".into(),
            mode.into(),
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--".into(),
            prompt.clone(),
        ];
        worker::call(
            &self.host,
            &worker::Request::Submit {
                id: id.clone(),
                prompt,
                arguments,
                environment: vec![(
                    "DEEPSEEK_CODE_CHILD_API_KEY".into(),
                    "local-fixture-only".into(),
                )],
            },
        )
        .await
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let job =
                deepseek_code_core::services::background_sessions::get(&self.data, &id).unwrap();
            if job.finished_ms.is_some() {
                assert_eq!(job.status, "completed", "{job:?}");
                break;
            }
            assert!(Instant::now() < deadline, "turn timed out: {job:?}");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(self.model.lock().unwrap().actions.is_empty());
    }
    fn result(&self, id: &str) -> Value {
        let value = self
            .model
            .lock()
            .unwrap()
            .results
            .get(id)
            .unwrap_or_else(|| panic!("no result {id}"))
            .clone();
        println!("PTY_TOOL {id} {value}");
        value
    }
    fn track(&mut self, id: &str) -> (String, u32, u64) {
        let result = self.result(id);
        let task = result["task_id"].as_str().unwrap().to_owned();
        let pid = result["terminal_pid"].as_u64().unwrap() as u32;
        let tick = identity(pid).unwrap().1;
        self.cleanup.add(pid);
        println!("PTY_ID task={task} pid={pid} start_tick={tick}");
        (task, pid, tick)
    }
    async fn wait_exited(&self, pid: u32, tick: u64) {
        let end = Instant::now() + Duration::from_secs(25);
        while !exited(pid, tick) {
            assert!(Instant::now() < end, "pid {pid}/{tick} still alive");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        println!(
            "PTY_CLEANUP pid={pid} start_tick={tick} exited=true observed={:?}",
            identity(pid)
        );
    }
    async fn shutdown(&self) {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            match worker::shutdown(&self.data, &self.host.session_id).await {
                Ok(()) => break,
                Err(e) => {
                    assert!(Instant::now() < end, "shutdown: {e:#}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        for (pid, tick) in self.cleanup.0.iter().take(2) {
            self.wait_exited(*pid, *tick).await;
        }
    }
}
fn load() -> (&'static str, &'static str, Value) {
    (
        "load",
        "ToolSearch",
        json!({"query":"select:TaskInput,TaskGet,TaskStop,TaskUpdate"}),
    )
}
fn bash(id: &'static str, command: &str) -> (&'static str, &'static str, Value) {
    (
        id,
        "Bash",
        json!({"command":command,"run_in_background":true,"interaction":"pty"}),
    )
}
fn input(id: &'static str, data: &str) -> (&'static str, &'static str, Value) {
    (id, "TaskInput", json!({"task_id":"$start","data":data}))
}
fn get(id: &'static str) -> (&'static str, &'static str, Value) {
    (
        id,
        "TaskGet",
        json!({"task_id":"$start","block":true,"timeout":150}),
    )
}
fn stop() -> (&'static str, &'static str, Value) {
    ("stop", "TaskStop", json!({"task_id":"$start"}))
}
fn settings(h: &Harness, value: Value) {
    let dir = h.workspace.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), value.to_string()).unwrap();
}

#[tokio::test]
async fn model_pty_worker_resize_resume_timeout_paging_stop_and_records() {
    let mut h = Harness::new().await;
    // A configured executable proves argv/environment/cwd come from Bash's
    // actual command builder, not an independent hard-coded shell launch.
    let wrapper = h.workspace.join("configured-bash");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nexport PTY_CONFIGURED=selected\nexec /bin/bash \"$@\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.data.join("settings.json"),
        json!({"shell":{"bashPath":wrapper}}).to_string(),
    )
    .unwrap();
    settings(
        &h,
        json!({"hooks":{
            "PreToolUse":[{"matcher":"TaskInput","hooks":[{"type":"command","shell":"sh","command":"cat >> hook-events.jsonl"}]}],
            "PostToolUse":[{"matcher":"TaskInput","hooks":[{"type":"command","shell":"sh","command":"cat >> hook-events.jsonl"}]}]
        }}),
    );
    h.turn("bypass",vec![load(),bash("start","test -t 0 && test -t 1 && test -t 2 && test -r /dev/tty || exit 9; printf 'TTY_READY:%s:%s:%s\\n' \"$PTY_CONFIGURED\" \"$PWD\" \"${DEEPSEEK_CODE_BACKGROUND_CONTROL-unset}\"; stty size; while IFS= read -r line; do printf '%s\\n' \"$line\" >> received-lines; printf 'echo:%s\\n' \"$line\"; stty size; done"),get("initial")]).await;
    {
        let model = h.model.lock().unwrap();
        let find = |name: &str| {
            model
                .requests
                .iter()
                .flat_map(|r| r["tools"].as_array().unwrap())
                .find(|t| t["function"]["name"] == name)
                .unwrap()["function"]["parameters"]
                .clone()
        };
        let bash = find("Bash");
        assert_eq!(
            bash["properties"]["interaction"]["enum"],
            json!(["pipe", "pty"])
        );
        assert_eq!(bash["properties"]["rows"]["default"], 24);
        assert_eq!(bash["properties"]["cols"]["maximum"], 1000);
        assert_eq!(
            find("TaskInput")["properties"]["resize"]["required"],
            json!(["rows", "cols"])
        );
    }
    let (task, pid, tick) = h.track("start");
    let initial = h.result("initial");
    assert!(
        initial["stdout"]
            .as_str()
            .unwrap()
            .contains("TTY_READY:selected:"),
        "{initial}"
    );
    assert!(initial["stdout"]
        .as_str()
        .unwrap()
        .contains(h.workspace.to_str().unwrap()));
    assert!(initial["stdout"].as_str().unwrap().contains(":unset"));
    assert!(initial["stdout"].as_str().unwrap().contains("24 80"));
    assert_eq!(initial["status"], "running");
    assert!(!exited(pid, tick));
    let refusal = worker::shutdown(&h.data, &h.host.session_id)
        .await
        .unwrap_err();
    assert!(refusal.to_string().contains("background shell tasks"));
    h.turn(
        "bypass",
        vec![
            (
                "resize",
                "TaskInput",
                json!({"task_id":"$start","resize":{"rows":40,"cols":120}}),
            ),
            input("first", "first-input\n"),
            get("first-get"),
            (
                "bad-mixed",
                "TaskInput",
                json!({"task_id":"$start","data":"BAD_MIXED\n","resize":{"rows":0,"cols":80}}),
            ),
            (
                "bad-timeout",
                "TaskInput",
                json!({"task_id":"$start","data":"BAD_TIMEOUT\n","timeout":0}),
            ),
            (
                "bad-kind",
                "TaskUpdate",
                json!({"task_id":"$start","metadata":{"interaction":"pipe"}}),
            ),
        ],
    )
    .await;
    assert_eq!(h.result("resize")["rows"], 40);
    let first = h.result("first-get");
    assert!(first["stdout"]
        .as_str()
        .unwrap()
        .contains("echo:first-input"));
    assert!(first["stdout"].as_str().unwrap().contains("40 120"));
    for id in ["bad-mixed", "bad-timeout", "bad-kind"] {
        assert!(h.result(id).to_string().contains("error"), "{id}");
    }
    // Another real model/CLI worker cannot address the owner session's task.
    let mut other = h.other_session().await;
    other
        .turn(
            "bypass",
            vec![
                load(),
                (
                    "foreign",
                    "TaskInput",
                    json!({"task_id":task,"data":"FOREIGN_MARKER\n"}),
                ),
            ],
        )
        .await;
    assert!(other
        .result("foreign")
        .to_string()
        .contains("Task not found"));
    other.shutdown().await;
    // A new resume turn on the same worker exercises permissions before writes.
    h.turn(
        "plan",
        vec![
            input("plan-input", "PLAN_MARKER\n"),
            bash("plan-start", "touch PLAN_START_MARKER"),
        ],
    )
    .await;
    assert!(h.result("plan-input").to_string().contains("error"));
    assert!(h.result("plan-start").to_string().contains("error"));
    assert!(!h.workspace.join("PLAN_START_MARKER").exists());
    assert_eq!(
        std::fs::read_to_string(h.workspace.join("received-lines")).unwrap(),
        "first-input\n"
    );
    h.turn(
        "bypass",
        vec![
            input("resume", "resume-input\n"),
            get("resume-get"),
            stop(),
            (
                "final",
                "TaskGet",
                json!({"task_id":"$start","block":true,"timeout":15000}),
            ),
        ],
    )
    .await;
    h.wait_exited(pid, tick).await;
    let endpoint = h.result("start")["endpoint"].as_str().unwrap().to_owned();
    assert!(
        !std::path::Path::new(&endpoint).exists(),
        "live endpoint after stop: {endpoint}"
    );
    println!("PTY_ENDPOINT_REMOVED {endpoint}");
    let final_result = h.result("final");
    assert_eq!(final_result["status"], "cancelled");
    let text = final_result["stdout"].as_str().unwrap();
    assert!(text.contains("echo:resume-input"));
    assert_eq!(
        std::fs::read_to_string(h.workspace.join("received-lines")).unwrap(),
        "first-input\nresume-input\n"
    );
    for absent in ["BAD_MIXED", "BAD_TIMEOUT", "FOREIGN_MARKER", "PLAN_MARKER"] {
        assert!(!text.contains(absent), "{text}");
    }
    // Model reads all bounded pages after the true terminal state, exactly once.
    let mut pages = String::new();
    let mut cursor = Value::Null;
    for index in 0..100 {
        let id = format!("page-{index}");
        let mut args = json!({"task_id":task,"max_output_bytes":17});
        if !cursor.is_null() {
            args["cursor"] = cursor.clone();
        }
        h.turn("bypass", vec![(&id, "TaskGet", args)]).await;
        let page = h.result(&id);
        assert_eq!(page["status"], "cancelled");
        let chunk = page["stdout"].as_str().unwrap();
        assert!(chunk.len() <= 17);
        pages.push_str(chunk);
        if page["has_more"] == false {
            break;
        }
        cursor = page["next_cursor"].clone();
    }
    assert_eq!(pages, text);
    let db = rusqlite::Connection::open(h.data.join("deepseek_code.db")).unwrap();
    let mut stmt=db.prepare("SELECT tc.id,tc.name,tc.input,tc.output,tc.is_error FROM tool_calls tc JOIN messages m ON m.id=tc.message_id WHERE m.conversation_id=?1 ORDER BY m.sort_order,tc.sort_order").unwrap();
    let rows: Vec<(String, String, String, String, i64)> = stmt
        .query_map([&h.host.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let ids: std::collections::HashSet<_> = rows.iter().map(|r| &r.0).collect();
    assert_eq!(ids.len(), rows.len());
    let inputs: Vec<_> = rows.iter().filter(|r| r.1 == "TaskInput").collect();
    assert_eq!(inputs.len(), 6);
    assert!(inputs.iter().all(|r| r.0.contains(":inv-")));
    println!("PTY_DB_CALLS {}", json!(rows));
    let journal = std::fs::read_to_string(h.workspace.join("hook-events.jsonl")).unwrap();
    assert!(journal.contains("PreToolUse") && journal.contains("PostToolUse"));
    for row in inputs.iter().filter(|r| r.4 == 0) {
        assert!(
            journal.contains(row.0.split(':').next_back().unwrap()),
            "missing hook {}",
            row.0
        );
    }
    println!("PTY_HOOKS {journal}");
    let terminal: Value = serde_json::from_str(
        &std::fs::read_to_string(
            h.data
                .join("shell-tasks")
                .join(&h.host.session_id)
                .join(&task)
                .join("terminal-state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(terminal["cleanup_complete"], true);
    println!("PTY_TERMINAL {terminal}");
    h.shutdown().await;
}

#[tokio::test]
async fn model_pty_eof_is_not_a_close_and_normal_exit_is_real() {
    let mut h = Harness::new().await;
    h.turn("bypass",vec![load(),bash("start","stty -echo; echo EOF_READY; if IFS= read -r line; then echo unexpected; else echo VEOF_OBSERVED; fi; IFS= read -r line; printf 'AFTER_EOF:%s\\n' \"$line\"; exit 7"),get("ready")]).await;
    let (_, pid, tick) = h.track("start");
    assert!(h.result("ready")["stdout"]
        .as_str()
        .unwrap()
        .contains("EOF_READY"));
    h.turn(
        "bypass",
        vec![
            ("eof", "TaskInput", json!({"task_id":"$start","eof":true})),
            get("veof"),
        ],
    )
    .await;
    assert!(h.result("veof")["stdout"]
        .as_str()
        .unwrap()
        .contains("VEOF_OBSERVED"));
    assert!(!exited(pid, tick));
    h.turn(
        "bypass",
        vec![
            input("after", "still-writable\n"),
            (
                "exited",
                "TaskGet",
                json!({"task_id":"$start","block":true,"timeout":15000}),
            ),
        ],
    )
    .await;
    h.wait_exited(pid, tick).await;
    let done = h.result("exited");
    assert_eq!(done["status"], "failed");
    assert!(done.to_string().contains("AFTER_EOF:still-writable"));
    let failed: Value = serde_json::from_str(done["output"].as_str().unwrap()).unwrap();
    assert_eq!(failed["exit_code"], 7);
    assert_eq!(failed["cancelled"], false);
    assert_eq!(failed["cleanup_complete"], true);
    let mut normal = bash("success", "stty size; printf NORMAL_EXIT; exit 0");
    normal.2["rows"] = json!(37);
    normal.2["cols"] = json!(99);
    h.turn(
        "bypass",
        vec![
            normal,
            (
                "success-get",
                "TaskGet",
                json!({"task_id":"$success","block":true,"timeout":15000}),
            ),
        ],
    )
    .await;
    let success = h.result("success-get");
    assert_eq!(success["status"], "completed");
    assert_eq!(success["stdout"], "37 99\r\nNORMAL_EXIT");
    let outcome: Value = serde_json::from_str(success["output"].as_str().unwrap()).unwrap();
    assert_eq!(outcome["exit_code"], 0);
    assert_eq!(outcome["cleanup_complete"], true);
    let launched = h.result("success");
    let task = launched["task_id"].as_str().unwrap();
    let pid = launched["terminal_pid"].as_u64().unwrap() as u32;
    let identity_path = h
        .data
        .join("shell-tasks")
        .join(&h.host.session_id)
        .join(task)
        .join("session.json");
    let recorded: Value =
        serde_json::from_str(&std::fs::read_to_string(identity_path).unwrap()).unwrap();
    assert_eq!(recorded["terminal_pid"], pid);
    let tick = recorded["leader_start_tick"].as_u64().unwrap();
    println!("PTY_ID task={task} pid={pid} start_tick={tick}");
    h.wait_exited(pid, tick).await;
    assert!(!std::path::Path::new(launched["endpoint"].as_str().unwrap()).exists());
    h.shutdown().await;
}

#[tokio::test]
async fn model_pty_denials_and_invalid_launches_have_no_effects() {
    let mut h = Harness::new().await;
    settings(
        &h,
        json!({"hooks":{"PreToolUse":[{"matcher":"TaskInput","hooks":[{"type":"command","shell":"sh","command":"cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"PTY_FIXTURE_DENIED\"}}'"}]}]}}),
    );
    h.turn("bypass",vec![load(),bash("start","stty -echo; echo DENY_READY; while IFS= read -r line; do printf '%s' \"$line\" >> marker; done"),get("ready")]).await;
    let (_, pid, tick) = h.track("start");
    h.turn(
        "bypass",
        vec![
            input("denied", "MUTATION\n"),
            get("unchanged"),
            stop(),
            (
                "stopped",
                "TaskGet",
                json!({"task_id":"$start","block":true,"timeout":15000}),
            ),
        ],
    )
    .await;
    assert!(h
        .result("denied")
        .to_string()
        .contains("PTY_FIXTURE_DENIED"));
    assert!(!h.workspace.join("marker").exists());
    h.wait_exited(pid, tick).await;
    let bad = json!({"command":"touch INVALID_MARKER","interaction":"pty","run_in_background":true,"rows":0});
    h.turn(
        "bypass",
        vec![
            ("invalid", "Bash", bad),
            (
                "foreground",
                "Bash",
                json!({"command":"touch INVALID_MARKER","interaction":"pty"}),
            ),
        ],
    )
    .await;
    assert!(h.result("invalid").to_string().contains("error"));
    assert!(h.result("foreground").to_string().contains("error"));
    assert!(!h.workspace.join("INVALID_MARKER").exists());
    // The execution gate can start while the configured executable's exec
    // fails. TaskGet must expose the real failure and completed cleanup.
    let broken = h.workspace.join("broken-bash");
    std::fs::write(&broken, "#!/no/such/interpreter\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.data.join("settings.json"),
        json!({"shell":{"bashPath":broken}}).to_string(),
    )
    .unwrap();
    h.turn(
        "bypass",
        vec![
            bash("spawn-failure", "touch INVALID_MARKER"),
            (
                "exec-failure-get",
                "TaskGet",
                json!({"task_id":"$spawn-failure","block":true,"timeout":15000}),
            ),
        ],
    )
    .await;
    let failed_start = h.result("spawn-failure");
    let failed_pid = failed_start["terminal_pid"].as_u64().unwrap() as u32;
    let failed_task = failed_start["task_id"].as_str().unwrap();
    let state_path = h
        .data
        .join("shell-tasks")
        .join(&h.host.session_id)
        .join(failed_task);
    let session_identity: Value =
        serde_json::from_str(&std::fs::read_to_string(state_path.join("session.json")).unwrap())
            .unwrap();
    let failed_tick = session_identity["leader_start_tick"].as_u64().unwrap();
    h.wait_exited(failed_pid, failed_tick).await;
    let failed = h.result("exec-failure-get");
    assert_eq!(failed["status"], "failed");
    let outcome: Value = serde_json::from_str(failed["output"].as_str().unwrap()).unwrap();
    assert_eq!(outcome["cleanup_complete"], true);
    assert_eq!(outcome["success"], false);
    assert!(!h.workspace.join("INVALID_MARKER").exists());
    let list = deepseek_code_core::tools::tasks::execute_for_session(
        "TaskList",
        json!({}),
        &h.data,
        &h.host.session_id,
    )
    .unwrap();
    println!("PTY_FAILURE_TASKS {list}");
    let tasks = list["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks.iter().filter(|t| t["status"] == "failed").count(), 1);
    for task in tasks {
        let dir = h
            .data
            .join("shell-tasks")
            .join(&h.host.session_id)
            .join(task["id"].as_str().unwrap());
        if let Ok(text) = std::fs::read_to_string(dir.join("terminal.json")) {
            let terminal: Value = serde_json::from_str(&text).unwrap();
            if let Some(endpoint) = terminal["endpoint"].as_str() {
                assert!(!std::path::Path::new(endpoint).exists());
            }
        }
    }
    h.shutdown().await;
}

#[tokio::test]
async fn model_pty_initialization_failure_leaves_no_running_task_or_child() {
    let mut h = Harness::new().await;
    std::fs::write(h.data.join("shell-tasks"), b"not a directory").unwrap();
    h.turn(
        "bypass",
        vec![load(), bash("init-failure", "touch INIT_MARKER")],
    )
    .await;
    let failed = h.result("init-failure");
    assert!(
        failed.to_string().contains("Creating the task directory"),
        "{failed}"
    );
    assert!(!h.workspace.join("INIT_MARKER").exists());
    let list = deepseek_code_core::tools::tasks::execute_for_session(
        "TaskList",
        json!({}),
        &h.data,
        &h.host.session_id,
    )
    .unwrap();
    let tasks = list["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["status"], "failed");
    let output: Value = serde_json::from_str(tasks[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(output["startup_failed"], true);
    println!("PTY_INIT_FAILURE {list}");
    h.shutdown().await;
}

#[tokio::test]
async fn model_pty_sandbox_and_explicit_deny_reject_before_start() {
    for (label, policy, expected) in [
        ("sandbox", json!({"sandbox":{"enabled":true}}), "sandbox"),
        ("deny", json!({"permissions":{"deny":["Bash"]}}), "den"),
    ] {
        let mut h = Harness::new().await;
        settings(&h, policy);
        h.turn("bypass", vec![bash(label, "touch FORBIDDEN_MARKER")])
            .await;
        let result = h.result(label);
        assert!(
            result.to_string().to_lowercase().contains(expected),
            "{result}"
        );
        assert!(result.get("task_id").is_none(), "{result}");
        assert!(!h.workspace.join("FORBIDDEN_MARKER").exists());
        assert!(!h.data.join("shell-tasks").exists());
        h.shutdown().await;
    }
}
