//! Windows acceptance through a local model and real CLI persistent worker.
#[cfg(windows)]
mod windows_tests {
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

    mod process_handles {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            pub fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
            pub fn TerminateProcess(handle: *mut c_void, code: u32) -> i32;
        }
    }
    #[derive(Default)]
    struct ExactCleanup(Vec<(u32, std::os::windows::io::OwnedHandle)>);
    impl ExactCleanup {
        fn add(&mut self, pid: u32) {
            use std::os::windows::io::FromRawHandle;
            let handle = unsafe { process_handles::OpenProcess(0x00100001, 0, pid) };
            assert!(
                !handle.is_null(),
                "OpenProcess {pid}: {}",
                std::io::Error::last_os_error()
            );
            self.0.push((pid, unsafe {
                std::os::windows::io::OwnedHandle::from_raw_handle(handle)
            }));
        }
        fn pids(&self) -> Vec<u32> {
            self.0.iter().map(|(pid, _)| *pid).collect()
        }
        fn exited(&self, pid: u32) -> bool {
            use std::os::windows::io::AsRawHandle;
            let handle = &self
                .0
                .iter()
                .find(|(p, _)| *p == pid)
                .expect("untracked PID")
                .1;
            unsafe { process_handles::WaitForSingleObject(handle.as_raw_handle(), 0) == 0 }
        }
        fn terminate(&self, pid: u32) {
            use std::os::windows::io::AsRawHandle;
            let handle = &self
                .0
                .iter()
                .find(|(p, _)| *p == pid)
                .expect("untracked PID")
                .1;
            assert_ne!(
                unsafe { process_handles::TerminateProcess(handle.as_raw_handle(), 1) },
                0
            );
        }
    }
    impl Drop for ExactCleanup {
        fn drop(&mut self) {
            for pid in self.pids() {
                if !self.exited(pid) {
                    self.terminate(pid);
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
            let mut cleanup = ExactCleanup::default();
            cleanup.add(host.supervisor_pid.unwrap());
            cleanup.add(host.child_pid.unwrap());
            println!(
                "CONPTY_WORKER session={session} engine={} supervisor={} data={}",
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
            let prompt = format!("ConPTY acceptance turn {}", self.sequence);
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
                let job = deepseek_code_core::services::background_sessions::get(&self.data, &id)
                    .unwrap();
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
            println!("CONPTY_TOOL {id} {value}");
            value
        }
        fn track(&mut self, id: &str) -> (String, u32) {
            let result = self.result(id);
            let task = result["task_id"].as_str().unwrap().to_owned();
            let pid = result["terminal_pid"].as_u64().unwrap() as u32;
            self.cleanup.add(pid);
            println!("CONPTY_ID task={task} pid={pid} identity=pinned_process_handle");
            (task, pid)
        }
        async fn wait_exited(&self, pid: u32) {
            let end = Instant::now() + Duration::from_secs(25);
            while !self.cleanup.exited(pid) {
                assert!(Instant::now() < end, "pinned process {pid} still alive");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            println!(
                "CONPTY_CLEANUP pid={pid} identity=pinned_process_handle exited=true observed={:?}",
                self.cleanup.exited(pid)
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
            for pid in self.cleanup.pids().into_iter().take(2) {
                self.wait_exited(pid).await;
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
            json!({"command":command,"shell":"powershell","interaction":"pty","run_in_background":true}),
        )
    }
    fn input(id: &'static str, data: &str) -> (&'static str, &'static str, Value) {
        (id, "TaskInput", json!({"task_id":"$start","data":data}))
    }
    fn get(id: &'static str) -> (&'static str, &'static str, Value) {
        (
            id,
            "TaskGet",
            json!({"task_id":"$start","block":true,"timeout":500}),
        )
    }
    fn stop() -> (&'static str, &'static str, Value) {
        ("stop", "TaskStop", json!({"task_id":"$start"}))
    }
    fn final_get() -> (&'static str, &'static str, Value) {
        (
            "final",
            "TaskGet",
            json!({"task_id":"$start","block":true,"timeout":20000}),
        )
    }
    fn fixture(h: &Harness) {
        std::fs::write(
            h.workspace.join("fixture.ps1"),
            include_str!("fixtures/conpty-console.ps1"),
        )
        .unwrap();
    }
    impl Harness {
        async fn read_until(&mut self, expected: &str) -> Value {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                self.turn("bypass", vec![get("poll")]).await;
                let value = self.result("poll");
                if value["stdout"].as_str().unwrap_or("").contains(expected) {
                    return value;
                }
                assert!(Instant::now() < deadline, "missing {expected}: {value}");
            }
        }
        async fn ready(&mut self) -> (String, Vec<u32>) {
            self.read_until("READY").await;
            let (task, gate) = self.track("start");
            let mut pids = vec![gate];
            for file in ["shell.pid", "descendant.pid"] {
                let pid = std::fs::read_to_string(self.workspace.join(file))
                    .unwrap()
                    .parse()
                    .unwrap();
                self.cleanup.add(pid);
                pids.push(pid);
            }
            println!(
                "CONPTY_TREE task={task} gate={} shell={} descendant={}",
                pids[0], pids[1], pids[2]
            );
            (task, pids)
        }
        fn lines(&self) -> String {
            std::fs::read_to_string(self.workspace.join("received-lines")).unwrap_or_default()
        }
    }
    #[tokio::test]
    async fn model_conpty_console_write_resize_resume_timeout_stop_tree_and_records() {
        let mut h = Harness::new().await;
        fixture(&h);
        h.turn("bypass", vec![load(), bash("start", "& .\\fixture.ps1")])
            .await;
        let (task, pids) = h.ready().await;
        let cross_process = deepseek_code_core::tools::tasks::execute_for_session(
            "TaskInput",
            json!({"task_id":task,"data":"CROSS_PROCESS\r"}),
            &h.data,
            &h.host.session_id,
        )
        .unwrap_err();
        assert!(cross_process
            .to_string()
            .contains("cross-process TaskInput is unsupported"));
        let initial = h.result("poll");
        assert!(initial["stdout"].as_str().unwrap().contains("CONSOLE_OK"));
        assert!(initial["stdout"].as_str().unwrap().contains("SIZE:24:80"));
        assert_eq!(initial["status"], "running");
        assert_eq!(initial["retrieval_status"], "timeout");
        assert!(worker::shutdown(&h.data, &h.host.session_id).await.is_err());
        h.turn(
            "bypass",
            vec![
                (
                    "resize",
                    "TaskInput",
                    json!({"task_id":task,"resize":{"rows":35,"cols":110}}),
                ),
                input("first", "first-input\r"),
            ],
        )
        .await;
        assert_eq!(h.result("resize")["resized"], true);
        let first = h.read_until("echo:first-input").await;
        assert!(first["stdout"].as_str().unwrap().contains("SIZE:35:110"));
        // A separate real worker/session cannot address this task.
        let mut other = h.other_session().await;
        other
            .turn(
                "bypass",
                vec![
                    load(),
                    (
                        "foreign",
                        "TaskInput",
                        json!({"task_id":task,"data":"FOREIGN\r"}),
                    ),
                ],
            )
            .await;
        assert!(other
            .result("foreign")
            .to_string()
            .contains("Task not found"));
        other.shutdown().await;
        // Same task, new model turn through the original persistent owner.
        h.turn("plan", vec![input("plan-input", "PLAN\r")]).await;
        assert!(h.result("plan-input").to_string().contains("error"));
        h.turn("bypass", vec![input("resume", "resume-input\r")])
            .await;
        h.read_until("echo:resume-input").await;
        assert_eq!(h.lines(), "first-input\nresume-input\n");
        h.turn("bypass", vec![stop(), final_get()]).await;
        let final_value = h.result("final");
        assert_eq!(final_value["status"], "cancelled");
        let outcome: Value = serde_json::from_str(final_value["output"].as_str().unwrap()).unwrap();
        assert_eq!(outcome["cleanup_complete"], true);
        assert_eq!(outcome["final_drain_complete"], true);
        for pid in pids {
            h.wait_exited(pid).await;
        }
        let db = rusqlite::Connection::open(h.data.join("deepseek_code.db")).unwrap();
        let mut stmt=db.prepare("SELECT tc.id,tc.name,tc.output,tc.is_error FROM tool_calls tc JOIN messages m ON m.id=tc.message_id WHERE m.conversation_id=?1 ORDER BY m.sort_order,tc.sort_order").unwrap();
        let rows: Vec<(String, String, Option<String>, bool)> = stmt
            .query_map([&h.host.session_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows.iter()
                .map(|r| &r.0)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            rows.len()
        );
        let inputs: Vec<_> = rows.iter().filter(|r| r.1 == "TaskInput").collect();
        assert_eq!(inputs.len(), 4);
        assert!(inputs
            .iter()
            .all(|r| r.0.contains(":inv-") && r.2.is_some()));
        assert_eq!(inputs.iter().filter(|r| r.3).count(), 1);
        println!("CONPTY_DB_CALLS {}", json!(rows));
        h.shutdown().await;
    }

    #[tokio::test]
    async fn model_conpty_eof_and_invalid_input_have_no_effect_then_normal_exit() {
        let mut h = Harness::new().await;
        fixture(&h);
        h.turn("bypass", vec![load(), bash("start", "& .\\fixture.ps1")])
            .await;
        let (_, pids) = h.ready().await;
        h.turn(
            "bypass",
            vec![
                (
                    "eof",
                    "TaskInput",
                    json!({"task_id":"$start","data":"BAD_EOF\r","eof":true}),
                ),
                (
                    "size",
                    "TaskInput",
                    json!({"task_id":"$start","resize":{"rows":0,"cols":100}}),
                ),
                (
                    "mixed",
                    "TaskInput",
                    json!({"task_id":"$start","data":"BAD_MIXED\r","resize":{"rows":30,"cols":90}}),
                ),
                (
                    "timeout",
                    "TaskInput",
                    json!({"task_id":"$start","data":"BAD_TIMEOUT\r","timeout":0}),
                ),
            ],
        )
        .await;
        assert!(h.result("eof").to_string().contains("No data was written"));
        for id in ["size", "mixed", "timeout"] {
            assert!(h.result(id).to_string().contains("error"));
        }
        assert_eq!(h.lines(), "");
        h.turn("bypass", vec![input("exit", "exit\r"), final_get()])
            .await;
        assert_eq!(h.lines(), "exit\n");
        let value = h.result("final");
        assert_eq!(value["status"], "completed");
        let result: Value = serde_json::from_str(value["output"].as_str().unwrap()).unwrap();
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["cleanup_complete"], true);
        for pid in pids {
            h.wait_exited(pid).await;
        }
        h.shutdown().await;
    }

    #[tokio::test]
    async fn model_conpty_plan_denies_start_without_marker_or_task() {
        let mut h = Harness::new().await;
        h.turn(
            "plan",
            vec![bash(
                "denied",
                "[IO.File]::WriteAllText((Join-Path $pwd 'DENIED'),'bad')",
            )],
        )
        .await;
        assert!(h.result("denied").to_string().contains("error"));
        assert!(!h.workspace.join("DENIED").exists());
        let tasks = deepseek_code_core::tools::tasks::execute_for_session(
            "TaskList",
            json!({}),
            &h.data,
            &h.host.session_id,
        )
        .unwrap();
        assert_eq!(tasks["tasks"].as_array().unwrap().len(), 0);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn model_conpty_owner_hard_exit_kills_gate_shell_and_descendant() {
        let mut h = Harness::new().await;
        fixture(&h);
        h.turn("bypass", vec![load(), bash("start", "& .\\fixture.ps1")])
            .await;
        let (_, pids) = h.ready().await;
        let engine = h.host.child_pid.unwrap();
        h.cleanup.terminate(engine);
        h.wait_exited(engine).await;
        // Only the exact engine is killed above. These exits must be kernel Job
        // Object cleanup, before the fixture's fallback Drop can touch any PID.
        for pid in pids {
            h.wait_exited(pid).await;
        }
        h.wait_exited(h.host.supervisor_pid.unwrap()).await;
        assert_eq!(h.lines(), "");
    }

    #[tokio::test]
    async fn model_conpty_exit_code_259_is_failed_not_permanently_running() {
        let mut h = Harness::new().await;
        h.turn(
            "bypass",
            vec![
                load(),
                bash(
                    "start",
                    "[Console]::WriteLine('EXIT_259_READY'); [void][Console]::ReadLine(); exit 259",
                ),
            ],
        )
        .await;
        // Cold PowerShell startup is not the exit-code regression. Establish
        // that the real program is running before releasing its exit, then
        // retain the same bounded TaskGet and exact terminal assertions.
        h.read_until("EXIT_259_READY").await;
        let (_, gate) = h.track("start");
        assert!(!h.cleanup.exited(gate));
        h.turn("bypass", vec![input("exit", "exit-259\r"), final_get()])
            .await;
        assert_eq!(h.result("exit")["bytes_written"], 9);
        let value = h.result("final");
        assert_eq!(value["status"], "failed");
        let result: Value = serde_json::from_str(value["output"].as_str().unwrap()).unwrap();
        assert_eq!(result["exit_code"], 259);
        assert_eq!(result["cleanup_complete"], true);
        h.wait_exited(gate).await;
        h.shutdown().await;
    }

    #[tokio::test]
    async fn model_conpty_unsupported_cmd_and_pipe_never_run_the_command() {
        let mut h = Harness::new().await;
        h.turn("bypass", vec![
            ("cmd", "Bash", json!({"command":"echo bad>CMD_RAN","shell":"cmd","interaction":"pty","run_in_background":true})),
            ("pipe", "Bash", json!({"command":"[IO.File]::WriteAllText((Join-Path $pwd 'PIPE_RAN'),'bad')","shell":"powershell","interaction":"pipe","run_in_background":true}))
        ]).await;
        assert!(h
            .result("cmd")
            .to_string()
            .contains("cmd raw command-line quoting"));
        assert!(h.result("pipe").to_string().contains("Windows named pipes"));
        assert!(!h.workspace.join("CMD_RAN").exists());
        assert!(!h.workspace.join("PIPE_RAN").exists());
        h.shutdown().await;
    }

    #[tokio::test]
    async fn model_conpty_initialization_failure_is_terminal_without_starting_command() {
        let mut h = Harness::new().await;
        let parent = h.data.join("shell-tasks").join(&h.host.session_id);
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, b"block directory creation").unwrap();
        h.turn(
            "bypass",
            vec![bash(
                "failed",
                "[IO.File]::WriteAllText((Join-Path $pwd 'STARTED'),'bad')",
            )],
        )
        .await;
        assert!(h
            .result("failed")
            .to_string()
            .contains("initialization failed"));
        assert!(!h.workspace.join("STARTED").exists());
        let tasks = deepseek_code_core::tools::tasks::execute_for_session(
            "TaskList",
            json!({}),
            &h.data,
            &h.host.session_id,
        )
        .unwrap();
        let tasks = tasks["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["status"], "failed");
        h.shutdown().await;
    }

    #[tokio::test]
    async fn model_conpty_configured_git_bash_reuses_shell_arguments_and_cwd() {
        let mut h = Harness::new().await;
        // Windows default resolves Git Bash through the normal shell settings.
        h.turn("bypass",vec![load(),("start","Bash",json!({"command":"printf 'BASH_READY\\n'; read -r line; printf 'bash:%s\\n' \"$line\"; pwd","interaction":"pty","run_in_background":true}))]).await;
        h.read_until("BASH_READY").await;
        let (_, pid) = h.track("start");
        assert!(h.result("start")["shell_path"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("bash"));
        h.turn(
            "bypass",
            vec![input("line", "git-bash-input\r"), final_get()],
        )
        .await;
        let value = h.result("final");
        assert_eq!(value["status"], "completed");
        assert!(value["stdout"]
            .as_str()
            .unwrap()
            .contains("bash:git-bash-input"));
        assert!(value["stdout"].as_str().unwrap().contains("workspace"));
        h.wait_exited(pid).await;
        h.shutdown().await;
    }
} // windows_tests

// The third CI platform has an explicit nonzero regression: no PTY side effect.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_pty_is_rejected_before_starting_a_process() {
    let temp = tempfile::tempdir().unwrap();
    let result = deepseek_code_core::tools::execute_tool_async(
        "Bash",
        serde_json::json!({"command":"touch STARTED","interaction":"pty","run_in_background":true}),
        temp.path().to_str().unwrap(),
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("macOS is not implemented"));
    assert!(!temp.path().join("STARTED").exists());
}
