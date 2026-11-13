//! Real detached dcl members, local model, real temporary Git, exact PID/start identities.
#![cfg(target_os = "linux")]
use axum::{routing::post, Json, Router};
use deepseek_code_core::{
    native_engine::provider_manager::ResolvedProvider,
    runtime::{RuntimePaths, SessionStore},
    services::{
        background_sessions::{self as jobs, Job},
        EnvironmentCredentials, ServiceContext,
    },
    super_agent::{
        config::{PlanApprovalPolicy, SuperAgentConfig},
        orchestrator::Orchestrator,
        role::{self, Role},
        state::StatePolicy,
        store::TaskStore,
        watcher::watch_once,
        EventType, NewEvent, Status, TaskRecord, TaskSpec,
    },
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn stream(value: Value, end: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":value,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":end}]})
    )
}
fn tool(name: &str, input: Value) -> String {
    stream(
        json!({"tool_calls":[{"index":0,"id":"fixture-call","type":"function","function":{"name":name,"arguments":input.to_string()}}]}),
        "tool_calls",
    )
}
fn strip_reminders(mut s: String) -> String {
    while let Some(a) = s.find("<system-reminder>") {
        let Some(b) = s[a..].find("</system-reminder>") else {
            break;
        };
        s.replace_range(a..a + b + "</system-reminder>".len(), "");
    }
    s.trim().into()
}
struct Model {
    hold_plan: bool,
    hold_recovery: AtomicBool,
    plan_ready: AtomicBool,
    reviewer_ready: AtomicBool,
    commit_ready: AtomicBool,
    recoveries: AtomicUsize,
    recovery_prompt: Mutex<String>,
}
async fn model_reply(model: Arc<Model>, body: Value) -> String {
    let messages = body["messages"].as_array().unwrap();
    let (index, prompt) = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(i, m)| {
            (
                i,
                strip_reminders(m["content"].as_str().unwrap_or("").into()),
            )
        })
        .find(|(_, s)| !s.is_empty())
        .unwrap();
    let results: Vec<_> = messages[index + 1..]
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    let reviewer = messages.iter().any(|m| {
        m["role"] == "system" && m["content"].as_str().unwrap_or("").contains("/ Reviewer")
    });
    if prompt.contains("[SUPER_AGENT_RECOVERY]") {
        if results.is_empty() {
            model.recoveries.fetch_add(1, Ordering::SeqCst);
            *model.recovery_prompt.lock().unwrap() = prompt.clone();
            if reviewer || model.hold_recovery.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            let round = prompt
                .rsplit("round ")
                .next()
                .unwrap()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .unwrap();
            return tool(
                "CrpSubmit",
                json!({"kind":"implementation","payload":{"spec_version":1,"round":round,"summary":"Resumed after checking existing commit; no replay","commits":["HEAD"]}}),
            );
        }
        assert!(results.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Report recorded"));
        return stream(json!({"content":"recovery report saved"}), "stop");
    }
    if reviewer {
        model.reviewer_ready.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
    }
    if prompt.contains("was approved") {
        if results.is_empty() {
            return tool(
                "Bash",
                json!({"shell":"sh","command":"printf 'committed once\\n' > file.txt && git add file.txt && git commit -q -m watcher-once","timeout":30000}),
            );
        }
        assert!(!results.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Permission denied"));
        model.commit_ready.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
    }
    if prompt.contains("implementation plan") {
        if results.is_empty() {
            return tool(
                "CrpSubmit",
                json!({"kind":"plan","payload":{"spec_version":1,"round":0,"understanding":"one commit then verify","scope":["file.txt"],"steps":["edit","commit"],"verification":["git log"],"risks":[]}}),
            );
        }
        assert!(results.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Report recorded"));
        model.plan_ready.store(true, Ordering::SeqCst);
        if model.hold_plan {
            std::future::pending::<()>().await;
        }
        return stream(json!({"content":"plan saved"}), "stop");
    }
    panic!("unexpected model prompt: {prompt}")
}

fn process_start(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = text[text.rfind(')')? + 2..].split_whitespace().collect();
    if fields[0] == "Z" {
        return None;
    }
    fields.get(19).map(|s| s.to_string())
}
fn signal(pid: u32, start: &str, signal: &str) {
    if process_start(pid).as_deref() != Some(start) {
        return;
    }
    assert!(Command::new("kill")
        .args([format!("-{signal}"), "--".into(), pid.to_string()])
        .status()
        .unwrap()
        .success());
}
struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    router: String,
    task_id: String,
    host: Orchestrator,
    model: Arc<Model>,
    server: tokio::task::JoinHandle<()>,
    owned: Mutex<Vec<(u32, String)>>,
}
impl Fixture {
    async fn new(hold_plan: bool, hold_recovery: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let model = Arc::new(Model {
            hold_plan,
            hold_recovery: AtomicBool::new(hold_recovery),
            plan_ready: AtomicBool::new(false),
            reviewer_ready: AtomicBool::new(false),
            commit_ready: AtomicBool::new(false),
            recoveries: AtomicUsize::new(0),
            recovery_prompt: Mutex::new(String::new()),
        });
        let m = model.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let m = m.clone();
                async move {
                    (
                        [("content-type", "text/event-stream")],
                        model_reply(m, body).await,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        std::fs::write(data.join("config.json"),serde_json::to_vec(&json!({"providers":[{"id":"fixture","name":"Fixture","provider_type":"openai","api_key":null,"base_url":format!("http://{addr}"),"models":[{"id":"fixture","name":"Fixture","enabled":true,"max_tokens":4096,"supports_tools":true,"supports_streaming":true,"context_window":128000}],"enabled":true,"is_default":true,"settings":{},"supports_web_search":false}],"default_provider_id":"fixture"})).unwrap()).unwrap();
        let context = ServiceContext::open(
            RuntimePaths::new(data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let router = SessionStore::new(context.db.clone())
            .create(&repo, "fixture", "fixture")
            .unwrap();
        role::set(&context.db, &router, Role::Router).unwrap();
        let provider=ResolvedProvider {provider:serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":format!("http://{addr}"),"api_key":"fixture-key","api_format":"openai","models":[],"enabled":true})).unwrap(),model:serde_json::from_value(json!({"id":"fixture","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap()};
        let mut config = SuperAgentConfig::default();
        config.approvals.plan = if hold_plan {
            PlanApprovalPolicy::Manual
        } else {
            PlanApprovalPolicy::Auto
        };
        config.watcher.retry_backoff_seconds = 0;
        let task = TaskStore::from_context(&context)
            .create_task(
                &router,
                TaskSpec {
                    title: "Watcher fixture".into(),
                    workspace: repo.clone(),
                    targets: vec![repo.to_string_lossy().into_owned()],
                    goal: "Commit once and report".into(),
                    requirements: vec![],
                    acceptance_criteria: vec![],
                    constraints: vec![],
                    out_of_scope: vec![],
                    target_branch: None,
                    repository_target_branches: Default::default(),
                },
                "watcher-fixture",
            )
            .unwrap();
        let host = Orchestrator::new(
            context,
            config,
            provider,
            "bypass".into(),
            Some(PathBuf::from(env!("CARGO_BIN_EXE_dcl"))),
        );
        let f = Self {
            _temp: temp,
            data,
            router,
            task_id: task.task_id,
            host,
            model,
            server,
            owned: Mutex::new(vec![]),
        };
        f.host.tick(&f.router).await.unwrap();
        f.wait_flag(&f.model.plan_ready).await;
        if hold_plan {
            f.host.tick(&f.router).await.unwrap();
        } else {
            let first = f.job();
            f.wait_stopped(&first.id).await;
            let result = f.host.tick(&f.router).await.unwrap();
            assert!(result.blocked.is_empty(), "{result:?}");
            f.wait_flag(&f.model.commit_ready).await;
            // The mock receiving a tool result is not a database durability
            // barrier. Kill only after the completed output is persisted.
            for attempt in 0..400 {
                let session = f.task().coder.session_id.unwrap();
                let saved: bool = f.host.context.db.with_conn(|c| c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 AND t.name='Bash' AND t.output IS NOT NULL AND t.is_error=0)",
                    [&session], |r| r.get(0),
                )).unwrap().unwrap();
                if saved {
                    break;
                }
                assert!(attempt < 399, "completed Bash output was not persisted");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(
                git(
                    &f.task().repositories[0].worktree_path,
                    &["log", "-1", "--format=%s"]
                ),
                "watcher-once"
            );
        }
        f.job();
        f
    }
    fn store(&self) -> TaskStore {
        TaskStore::from_context(&self.host.context)
    }
    fn task(&self) -> TaskRecord {
        self.store().get_task(&self.task_id).unwrap().unwrap()
    }
    fn job(&self) -> Job {
        self.member_job(false)
    }
    fn member_job(&self, reviewer: bool) -> Job {
        let task = self.task();
        let session = if reviewer {
            task.reviewer.session_id
        } else {
            task.coder.session_id
        }
        .unwrap();
        let owner = deepseek_code_core::native_engine::child_session::owner_record(
            &self.host.context,
            &session,
        )
        .unwrap()
        .unwrap();
        let job = jobs::get(&self.data, owner.job_id.as_deref().unwrap()).unwrap();
        assert_eq!(job.parent_session_id, self.router);
        for pid in job.child_pid.into_iter().chain(job.supervisor_pid) {
            if let Some(start) = process_start(pid) {
                let mut owned = self.owned.lock().unwrap();
                if !owned.iter().any(|v| v.0 == pid && v.1 == start) {
                    owned.push((pid, start));
                }
            }
        }
        job
    }
    async fn wait_flag(&self, flag: &AtomicBool) {
        for _ in 0..400 {
            self.job();
            if self.task().reviewer.session_id.is_some() {
                self.member_job(true);
            }
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let job = self.job();
        let dir = self.data.join("background-tasks").join(&job.id);
        panic!(
            "mock barrier timed out: {job:?}; stderr: {}; output: {}",
            std::fs::read_to_string(dir.join("stderr.log")).unwrap_or_default(),
            std::fs::read_to_string(dir.join("output.jsonl")).unwrap_or_default()
        );
    }
    async fn wait_stopped(&self, id: &str) {
        for _ in 0..400 {
            let job = jobs::get(&self.data, id).unwrap();
            if matches!(job.status.as_str(), "completed" | "failed" | "cancelled")
                && job
                    .child_pid
                    .is_some_and(deepseek_code_core::services::session_worker::process_exited)
                && job
                    .supervisor_pid
                    .is_some_and(deepseek_code_core::services::session_worker::process_exited)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job did not stop: {:?}", jobs::get(&self.data, id));
    }
    async fn kill_child(&self) -> Job {
        let job = self.job();
        let pid = job.child_pid.unwrap();
        let start = process_start(pid).unwrap();
        signal(pid, &start, "KILL");
        self.wait_stopped(&job.id).await;
        job
    }
    fn count(&self, kind: EventType) -> usize {
        self.store()
            .events(&self.task_id)
            .unwrap()
            .iter()
            .filter(|e| e.event_type == kind)
            .count()
    }
    fn reopened(&self) -> Orchestrator {
        Orchestrator::new(
            ServiceContext::open(
                RuntimePaths::new(self.data.clone()).unwrap(),
                Arc::new(EnvironmentCredentials),
            )
            .unwrap(),
            self.host.config.clone(),
            self.host.provider.clone(),
            "bypass".into(),
            self.host.executable_override.clone(),
        )
    }
    fn pause(&self) {
        self.store()
            .apply_event(
                &self.task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type: EventType::TaskPaused,
                    causation_id: None,
                    operation_id: "fixture.pause".into(),
                    payload: json!({}),
                },
                &StatePolicy::default(),
            )
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let mut owned = self.owned.lock().unwrap().clone();
        // A failed assertion can run Drop immediately after spawn, before the
        // next barrier captured the child's PID. Cooperatively cancel all jobs
        // of this fixture's unique Router and keep observing until they settle.
        for _ in 0..80 {
            let jobs = jobs::list(&self.data, Some(&self.router)).unwrap_or_default();
            let mut settled = true;
            for job in jobs {
                assert_eq!(job.parent_session_id, self.router);
                for pid in job.child_pid.into_iter().chain(job.supervisor_pid) {
                    if let Some(start) = process_start(pid) {
                        if !owned.iter().any(|v| v.0 == pid && v.1 == start) {
                            owned.push((pid, start));
                        }
                    }
                }
                for (pid, start) in &owned {
                    signal(*pid, start, "CONT");
                }
                if !matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
                    settled = false;
                    // Also works for a paused supervisor's stale heartbeat.
                    let _ = std::fs::write(
                        self.data
                            .join("background-tasks")
                            .join(&job.id)
                            .join("cancel"),
                        b"cancel",
                    );
                }
            }
            if settled
                && owned
                    .iter()
                    .all(|(p, s)| process_start(*p).as_ref() != Some(s))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for (pid, start) in &owned {
            signal(*pid, start, "TERM");
        }
        for _ in 0..40 {
            if owned
                .iter()
                .all(|(p, s)| process_start(*p).as_ref() != Some(s))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for (pid, start) in &owned {
            signal(*pid, start, "KILL");
        }
        self.server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_recovers_killed_coder_once_without_repeating_commit() {
    let f = Fixture::new(false, false).await;
    let session = f.task().coder.session_id.unwrap();
    let repo = f.task().repositories.remove(0);
    let head = git(&repo.worktree_path, &["rev-parse", "HEAD"]);
    let count = git(&repo.worktree_path, &["rev-list", "--count", "HEAD"]);
    f.kill_child().await;
    let result = f.host.tick_watched(&f.router).await.unwrap();
    assert_eq!(result.launched.len(), 1, "{result:?}");
    assert!(result.blocked.is_empty(), "{result:?}");
    let next = f.job();
    f.wait_stopped(&next.id).await;
    for host in [&f.host, &f.reopened()] {
        let r = watch_once(host, &f.router).await.unwrap();
        assert!(r.resumed.is_empty(), "{r:?}");
    }
    assert_eq!(f.task().coder.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(f.count(EventType::SessionInterrupted), 1);
    assert_eq!(f.count(EventType::SessionResumed), 1);
    assert_eq!(git(&repo.worktree_path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&repo.worktree_path, &["rev-list", "--count", "HEAD"]),
        count
    );
    assert!(f.model.recovery_prompt.lock().unwrap().contains(&head));
    assert_eq!(f.model.recoveries.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_does_not_resume_killed_member_waiting_for_plan_approval() {
    let f = Fixture::new(true, false).await;
    assert_eq!(f.task().status, Status::WaitingUser);
    assert!(!f.store().approvals(&f.task_id).unwrap().is_empty());
    f.kill_child().await;
    let r = watch_once(&f.host, &f.router).await.unwrap();
    assert!(r.resumed.is_empty());
    assert_eq!(f.count(EventType::SessionInterrupted), 0);
    assert_eq!(f.count(EventType::SessionResumed), 0);
    assert_eq!(f.task().status, Status::WaitingUser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_does_not_resume_a_paused_task() {
    let f = Fixture::new(false, false).await;
    f.pause();
    f.kill_child().await;
    let r = f.host.tick_watched(&f.router).await.unwrap();
    assert!(r.launched.is_empty());
    assert_eq!(f.task().status, Status::Paused);
    assert_eq!(f.count(EventType::SessionResumed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_watcher_hosts_resume_one_interruption_once() {
    // The same test executable supplies an independent OS Router host. Only
    // the parent creates a fixture; the contender reads its explicit envelope.
    if let Ok(path) = std::env::var("DCL_WATCHER_CONTENDER_CONFIG") {
        let envelope: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let context = ServiceContext::open(
            RuntimePaths::new(PathBuf::from(envelope["data"].as_str().unwrap())).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let host = Orchestrator::new(
            context,
            serde_json::from_value(envelope["config"].clone()).unwrap(),
            ResolvedProvider {
                provider: serde_json::from_value(envelope["provider"].clone()).unwrap(),
                model: serde_json::from_value(envelope["model"].clone()).unwrap(),
            },
            "bypass".into(),
            Some(PathBuf::from(env!("CARGO_BIN_EXE_dcl"))),
        );
        let result = watch_once(&host, envelope["router"].as_str().unwrap())
            .await
            .unwrap();
        std::fs::write(envelope["result"].as_str().unwrap(), serde_json::to_vec(&json!({
            "pid":std::process::id(), "resumed":result.resumed, "deferred":result.deferred_tasks,
        })).unwrap()).unwrap();
        return;
    }
    let f = Fixture::new(false, false).await;
    f.kill_child().await;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let host_a = f.reopened().with_gate(Arc::new(move |stage| {
        if stage == "after-reservation" {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        Ok(())
    }));
    let router = f.router.clone();
    let a = tokio::spawn(async move { watch_once(&host_a, &router).await.unwrap() });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let envelope = f._temp.path().join("contender.json");
    let result_path = f._temp.path().join("contender-result.json");
    std::fs::write(
        &envelope,
        serde_json::to_vec(&json!({
            "data":f.data, "router":f.router, "config":f.host.config,
            "provider":f.host.provider.provider, "model":f.host.provider.model,
            "result":result_path,
        }))
        .unwrap(),
    )
    .unwrap();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "two_watcher_hosts_resume_one_interruption_once",
                "--nocapture",
            ])
            .env("DCL_WATCHER_CONTENDER_CONFIG", envelope)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "contender failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let b: Value = serde_json::from_slice(&std::fs::read(result_path).unwrap()).unwrap();
    assert_ne!(b["pid"].as_u64().unwrap(), std::process::id() as u64);
    assert!(b["resumed"].as_array().unwrap().is_empty());
    assert!(b["deferred"]
        .as_array()
        .unwrap()
        .contains(&json!(f.task_id)));
    release_tx.send(()).unwrap();
    assert_eq!(a.await.unwrap().resumed.len(), 1);
    let job = f.job();
    f.wait_stopped(&job.id).await;
    assert_eq!(f.count(EventType::SessionResumed), 1);
    assert_eq!(f.count(EventType::SessionInterrupted), 1);
    assert_eq!(f.model.recoveries.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_recovery_limit_blocks_and_notifies_router_once() {
    let mut f = Fixture::new(false, true).await;
    f.host.config.watcher.max_recovery_attempts = 1;
    f.kill_child().await;
    let first = watch_once(&f.host, &f.router).await.unwrap();
    assert_eq!(first.resumed.len(), 1, "{first:?}; task: {:?}", f.task());
    for _ in 0..200 {
        f.job();
        if f.model.recoveries.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(f.model.recoveries.load(Ordering::SeqCst), 1);
    f.kill_child().await;
    let r = watch_once(&f.host, &f.router).await.unwrap();
    assert_eq!(r.blocked.len(), 1);
    assert!(r.blocked[0].contains("max_recovery_attempts"));
    assert_eq!(f.task().status, Status::Blocked);
    watch_once(&f.reopened(), &f.router).await.unwrap();
    assert_eq!(f.count(EventType::TaskBlocked), 1);
    assert_eq!(f.count(EventType::SessionResumed), 1);
    let notices:i64=f.host.context.db.with_conn(|c|c.query_row("SELECT count(*) FROM session_inbox WHERE recipient=?1 AND content LIKE 'Super Agent Watcher blocked task%'",[&f.router],|r|r.get(0))).unwrap().unwrap();
    assert_eq!(notices, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_stops_stale_old_instance_before_starting_a_replacement() {
    let mut f = Fixture::new(false, false).await;
    f.host.config.watcher.default_progress_timeout_seconds = 1;
    let job = f.job();
    let supervisor = job.supervisor_pid.unwrap();
    let start = process_start(supervisor).unwrap();
    signal(supervisor, &start, "STOP");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let r = watch_once(&f.host, &f.router).await.unwrap();
    assert!(r.resumed.is_empty());
    assert_eq!(f.count(EventType::SessionResumed), 0);
    assert_eq!(f.job().id, job.id);
    assert!(process_start(job.child_pid.unwrap()).is_some());
    assert!(f
        .data
        .join("background-tasks")
        .join(&job.id)
        .join("cancel")
        .exists());
    signal(supervisor, &start, "CONT");
    f.wait_stopped(&job.id).await;
    assert_eq!(
        watch_once(&f.reopened(), &f.router)
            .await
            .unwrap()
            .resumed
            .len(),
        1
    );
    let next = f.job();
    assert_ne!(next.id, job.id);
    f.wait_stopped(&next.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_time_budget_and_backoff_survive_host_reopen() {
    let mut f = Fixture::new(false, false).await;
    f.host.config.watcher.retry_backoff_seconds = 2;
    f.host.config.watcher.recovery_budget_seconds = 1;
    f.kill_child().await;
    assert!(watch_once(&f.host, &f.router)
        .await
        .unwrap()
        .resumed
        .is_empty());
    assert_eq!(f.count(EventType::SessionInterrupted), 1);
    assert!(watch_once(&f.reopened(), &f.router)
        .await
        .unwrap()
        .resumed
        .is_empty());
    assert_eq!(f.count(EventType::SessionInterrupted), 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let r = watch_once(&f.reopened(), &f.router).await.unwrap();
    assert!(r.blocked[0].contains("time budget"));
    assert_eq!(f.count(EventType::SessionResumed), 0);
    assert_eq!(f.task().status, Status::Blocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_adopts_spawn_before_resumed_event_after_host_reopen() {
    let f = Fixture::new(false, false).await;
    let repo = f.task().repositories.remove(0);
    let head = git(&repo.worktree_path, &["rev-parse", "HEAD"]);
    f.kill_child().await;
    let host = f.reopened().with_gate(Arc::new(|stage| {
        anyhow::ensure!(stage != "after-spawn", "simulated host crash after spawn");
        Ok(())
    }));
    let r = watch_once(&host, &f.router).await.unwrap();
    assert!(r.resumed.is_empty());
    assert_eq!(f.count(EventType::SessionInterrupted), 1);
    assert_eq!(f.count(EventType::SessionResumed), 0);
    let job = f.job();
    f.wait_stopped(&job.id).await;
    let r = f.reopened().tick_watched(&f.router).await.unwrap();
    assert_eq!(r.launched.len(), 1, "{r:?}");
    assert!(r.blocked.is_empty(), "{r:?}");
    assert_eq!(f.job().id, job.id);
    assert_eq!(f.count(EventType::SessionResumed), 1);
    assert_eq!(f.model.recoveries.load(Ordering::SeqCst), 1);
    assert_eq!(git(&repo.worktree_path, &["rev-parse", "HEAD"]), head);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_recovers_reviewer_in_the_same_session_and_workspace() {
    let f = Fixture::new(false, false).await;
    f.kill_child().await;
    assert_eq!(
        watch_once(&f.host, &f.router).await.unwrap().resumed.len(),
        1
    );
    f.wait_stopped(&f.job().id).await;
    let r = f.host.tick_watched(&f.router).await.unwrap();
    assert!(r.blocked.is_empty(), "{r:?}");
    f.wait_flag(&f.model.reviewer_ready).await;
    let session = f.task().reviewer.session_id.unwrap();
    let old = f.member_job(true);
    let pid = old.child_pid.unwrap();
    signal(pid, &process_start(pid).unwrap(), "KILL");
    f.wait_stopped(&old.id).await;
    let r = watch_once(&f.reopened(), &f.router).await.unwrap();
    assert_eq!(r.resumed.len(), 1, "{r:?}");
    assert!(r.resumed[0].contains("reviewer"));
    for attempt in 0..200 {
        f.member_job(true);
        if f.model.recoveries.load(Ordering::SeqCst) == 2 {
            break;
        }
        assert!(attempt < 199, "Reviewer recovery never reached the mock");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let new = f.member_job(true);
    assert_ne!(old.id, new.id);
    assert_eq!(new.session_id, session);
    assert_eq!(
        jobs::launch_of(&f.data, &old.id).unwrap().unwrap().cwd,
        jobs::launch_of(&f.data, &new.id).unwrap().unwrap().cwd
    );
    assert_eq!(
        f.task().reviewer.session_id.as_deref(),
        Some(session.as_str())
    );
    assert!(watch_once(&f.reopened(), &f.router)
        .await
        .unwrap()
        .resumed
        .is_empty());
    assert_eq!(f.count(EventType::SessionResumed), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_cannot_resume_while_another_session_claim_is_held() {
    let f = Fixture::new(false, false).await;
    f.kill_child().await;
    let session = f.task().coder.session_id.unwrap();
    let claim = f
        .data
        .join("background-tasks")
        .join(format!("session-{session}.claim"));
    std::fs::write(&claim, "fixture-existing-owner").unwrap();
    let old = f.job().id;
    let r = watch_once(&f.host, &f.router).await.unwrap();
    assert!(r.resumed.is_empty());
    assert_eq!(f.job().id, old);
    assert_eq!(f.count(EventType::SessionResumed), 0);
    assert_eq!(
        std::fs::read_to_string(&claim).unwrap(),
        "fixture-existing-owner"
    );
    std::fs::remove_file(&claim).unwrap();
    // Simulate a host crash while holding its own probe. The next host may
    // release exactly this operation's claim under the Router lifetime lock.
    let interrupted = f
        .store()
        .events(&f.task_id)
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == EventType::SessionInterrupted)
        .unwrap();
    let operation = interrupted
        .operation_id
        .strip_suffix(".interrupted")
        .unwrap();
    std::fs::write(&claim, operation).unwrap();

    assert_eq!(
        watch_once(&f.reopened(), &f.router)
            .await
            .unwrap()
            .resumed
            .len(),
        1
    );
    f.wait_stopped(&f.job().id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_blocks_unknown_side_effect_after_observing_real_commit() {
    let f = Fixture::new(false, false).await;
    let session = f.task().coder.session_id.unwrap();
    let repo = f.task().repositories.remove(0);
    let head = git(&repo.worktree_path, &["rev-parse", "HEAD"]);
    let old = f.kill_child().await;
    // Simulate a crash before the completed Bash output reached the transcript.
    // Git already contains its commit; replaying the compound command is unsafe.
    let changed = f.host.context.db.with_conn(|c| c.execute(
        "UPDATE tool_calls SET output=NULL WHERE name='Bash' AND message_id IN (SELECT id FROM messages WHERE conversation_id=?1)",
        [&session],
    )).unwrap().unwrap();
    assert_eq!(changed, 1);
    let r = watch_once(&f.host, &f.router).await.unwrap();
    assert!(r.resumed.is_empty());
    assert_eq!(r.blocked.len(), 1);
    assert!(r.blocked[0].contains("unknown outcome"));
    assert!(r.blocked[0].contains(&head));
    assert_eq!(f.task().status, Status::Blocked);
    assert_eq!(f.job().id, old.id);
    assert_eq!(git(&repo.worktree_path, &["rev-parse", "HEAD"]), head);
    assert_eq!(f.model.recoveries.load(Ordering::SeqCst), 0);
}
