use super::*;
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use claude_core::services::{permission_authority, session_messages, ServiceContext};

#[derive(Clone)]
struct Model {
    requests: Arc<Mutex<Vec<Value>>>,
    slow_started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    boundary_resume: bool,
}

fn child_request(body: &Value) -> bool {
    body["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == "system" && message["content"].to_string().contains("Child session ID:")
    })
}

async fn model(
    State(state): State<Model>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(
        headers["authorization"],
        "Bearer fixture-secret-only-in-memory"
    );
    state.requests.lock().unwrap().push(body.clone());
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let mut tool = None;
    let child = child_request(&body);
    let content = last["content"].as_str().unwrap_or_default();
    if child && last["role"] == "user" {
        if content.contains("IDLE_MODE") {
            state.slow_started.notify_one();
            state.release.notified().await;
            tool = Some((
                "idle-mode-write",
                "Write",
                json!({"file_path":"idle.txt","content":"MODE_WRITE"}),
            ));
        } else if content.contains("IDLE_SLOW") {
            state.slow_started.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
        } else if content.contains("IDLE_FAIL") {
            return (StatusCode::BAD_REQUEST, "fixture child failure").into_response();
        } else if content.contains("IDLE_WRITE") {
            tool = Some((
                "idle-write",
                "Write",
                json!({"file_path":"idle.txt","content":"IDLE_CHILD_WRITE"}),
            ));
        } else if content.contains("IDLE_QUESTION") {
            tool = Some((
                "idle-question",
                "AskUserQuestion",
                json!({"question":"Choose child response","options":[{"label":"Rust"},{"label":"Go"}]}),
            ));
        }
    } else if !child && last["role"] == "user" {
        tool = Some((
            "initial-agent",
            "Agent",
            json!({"agent_type":"general","prompt":"Complete the initial child task","run_in_background":false}),
        ));
    } else if !child && state.boundary_resume && last["tool_call_id"] == "initial-agent" {
        let result: Value = serde_json::from_str(content).unwrap();
        tool = Some((
            "boundary-message",
            "SendMessage",
            json!({"operation":"send","to":result["session_id"],"content":"Complete one owner-turn continuation"}),
        ));
    }
    let (delta, reason) = if let Some((id, name, args)) = tool {
        (
            json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}),
            "tool_calls",
        )
    } else {
        (
            json!({"content":if child {"CHILD_COMPLETE"}else{"OWNER_COMPLETE"}}),
            "stop",
        )
    };
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})
        ),
    )
        .into_response()
}

struct Fixture {
    // Kill the copied worker before closing databases and removing its directory.
    _cleanup: Cleanup,
    workspace: std::path::PathBuf,
    executable: std::path::PathBuf,
    context: ServiceContext,
    store: SessionStore,
    owner: String,
    child: String,
    peer: String,
    host: worker::Worker,
    model: Model,
    server: tokio::task::JoinHandle<()>,
    _temp: tempfile::TempDir,
}

impl Fixture {
    async fn start(mode: &str, boundary_resume: bool) -> Self {
        Self::start_with_hooks(mode, boundary_resume, None).await
    }
    async fn start_with_hooks(mode: &str, boundary_resume: bool, settings: Option<Value>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let context = session_messages::context(&temp.path().join("data")).unwrap();
        if let Some(settings) = settings {
            std::fs::write(
                context.paths.data_dir.join("settings.json"),
                settings.to_string(),
            )
            .unwrap();
        }
        let store = SessionStore::new(context.db.clone());
        let owner = store.create(&workspace, "fixture", "cli-runtime").unwrap();
        let peer = store
            .create(&workspace, "unrelated-model", "unrelated-provider")
            .unwrap();
        permission_authority::record_user(
            &store.db,
            &owner,
            &json!("Complete the delegated task and follow-up work in this workspace"),
        )
        .unwrap();
        let model = Model {
            requests: Default::default(),
            slow_started: Default::default(),
            release: Default::default(),
            boundary_resume,
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(self::model))
            .with_state(model.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let executable = temp.path().join(if cfg!(windows) {
            "idle-worker.exe"
        } else {
            "idle-worker"
        });
        std::fs::copy(test_cli_executable(), &executable).unwrap();
        let host = worker::ensure_worker(&context.paths.data_dir, &executable, &owner, &workspace)
            .await
            .unwrap();
        let cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
        let initial = uuid::Uuid::new_v4().to_string();
        worker::call(
            &host,
            &worker::Request::Submit {
                id: initial.clone(),
                prompt: "Delegate the initial task".into(),
                arguments: vec![
                    "--data-dir".into(),
                    context.paths.data_dir.to_string_lossy().into_owned(),
                    "--resume".into(),
                    owner.clone(),
                    "--base-url".into(),
                    endpoint,
                    "--api-format".into(),
                    "openai".into(),
                    "--api-key-env".into(),
                    "CLAUDE_RUST_CHILD_API_KEY".into(),
                    "--model".into(),
                    "fixture".into(),
                    "--permission-mode".into(),
                    mode.into(),
                    "--print".into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--".into(),
                    "Delegate the initial task".into(),
                ],
                environment: vec![(
                    "CLAUDE_RUST_CHILD_API_KEY".into(),
                    "fixture-secret-only-in-memory".into(),
                )],
            },
        )
        .await
        .unwrap();
        assert_eq!(
            completed(&context.paths.data_dir, &initial).await.status,
            "completed"
        );
        let records: Vec<String> = store
            .db
            .with_conn(|conn| {
                conn.prepare("SELECT value FROM peripheral_settings WHERE key LIKE 'delegation.%'")?
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap()
            .unwrap();
        let child = records
            .iter()
            .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
            .find(|row| row["parent_session_id"] == owner)
            .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            _temp: temp,
            workspace,
            executable,
            context,
            store,
            owner,
            child,
            peer,
            host,
            model,
            server,
            _cleanup: cleanup,
        }
    }
    fn send(&self, text: &str) {
        session_messages::send(&self.context, &self.peer, &self.child, text).unwrap();
    }
    async fn next_job(&self, after: usize) -> jobs::Job {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let jobs = jobs::list(&self.context.paths.data_dir, Some(&self.owner)).unwrap();
                if jobs.len() > after {
                    return jobs.last().unwrap().clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("No visible owned-child job")
    }
    async fn stop(&self) {
        worker::shutdown(&self.context.paths.data_dir, &self.owner)
            .await
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn idle_child_permission_hook_completes_without_blocking_the_engine_or_next_question() {
    let decision = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"file_path":"hook-idle.txt","content":"HOOK_IDLE_WRITE"}}}});
    let command = if cfg!(windows) {
        format!("[Console]::Out.Write('{}')", decision)
    } else {
        format!("printf '%s' '{}'", decision)
    };
    let settings = json!({"hooks":{"PermissionRequest":[{"matcher":"Write","hooks":[{"type":"command","shell":if cfg!(windows){"powershell"}else{"sh"},"command":command}]}]}});
    let fixture = Fixture::start_with_hooks("ask", false, Some(settings)).await;
    fixture.send("IDLE_WRITE");
    let job = fixture.next_job(1).await;
    assert_eq!(
        completed(&fixture.context.paths.data_dir, &job.id)
            .await
            .status,
        "completed"
    );
    assert!(!fixture.workspace.join("idle.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("hook-idle.txt")).unwrap(),
        "HOOK_IDLE_WRITE"
    );
    assert!(
        jobs::pending_question(&fixture.context.paths.data_dir, &job.id, &fixture.owner)
            .unwrap()
            .is_none()
    );

    fixture.send("IDLE_QUESTION");
    let next = fixture.next_job(2).await;
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(question) =
                jobs::pending_question(&fixture.context.paths.data_dir, &next.id, &fixture.owner)
                    .unwrap()
            {
                break question;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(question.question.contains("Choose child response"));
    jobs::answer_question(
        &fixture.context.paths.data_dir,
        &next.id,
        &fixture.owner,
        &question.request_id,
        "2",
    )
    .unwrap();
    assert_eq!(
        completed(&fixture.context.paths.data_dir, &next.id)
            .await
            .status,
        "completed"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn owned_child_wakes_without_parent_turn_and_preserves_its_original_runtime() {
    let fixture = Fixture::start("bypass", false).await;
    let parent_history = fixture.store.history(&fixture.owner).unwrap();
    session_messages::send(
        &fixture.context,
        &fixture.owner,
        &fixture.peer,
        "Independent sessions do not inherit this owner's runtime",
    )
    .unwrap();
    permission_authority::record_user(
        &fixture.store.db,
        &fixture.owner,
        &json!("NEW_OWNER_CONSTRAINT: only write idle.txt"),
    )
    .unwrap();
    fixture.send("IDLE_WRITE: write the requested file");
    let job = fixture.next_job(1).await;
    assert_eq!(
        completed(&fixture.context.paths.data_dir, &job.id)
            .await
            .status,
        "completed"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("idle.txt")).unwrap(),
        "IDLE_CHILD_WRITE"
    );
    assert_eq!(
        fixture.store.history(&fixture.owner).unwrap(),
        parent_history
    );
    let authority =
        permission_authority::history(&fixture.store.db, &fixture.child, 100_000).unwrap();
    let authority = serde_json::to_string(&authority).unwrap();
    assert!(authority.contains("NEW_OWNER_CONSTRAINT"));
    assert!(!authority.contains("IDLE_WRITE"));
    let requests = fixture.model.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests.iter().filter(|body| !child_request(body)).count(),
        2
    );
    assert!(requests.iter().all(|body| body["model"] == "fixture"));
    drop(requests);
    assert_eq!(
        session_messages::pending_count(&fixture.context, &fixture.peer).unwrap(),
        1
    );
    let (_, _, events) =
        jobs::events_since(&fixture.context.paths.data_dir, &job.id, &fixture.owner, 0).unwrap();
    assert!(events
        .iter()
        .any(|value| value["type"] == "owned_child_wake"
            && value["child_session_id"] == fixture.child));
    assert!(events
        .iter()
        .any(|value| value["type"] == "result" && value["child_session_id"] == fixture.child));
    let notices = jobs::collect_completed(
        &fixture.store,
        &fixture.context.paths.data_dir,
        &fixture.owner,
    )
    .unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains(&format!("Source session: {}", fixture.child)));
    assert!(!notices[0].contains("stored its answer directly in this session"));
    assert_no_secret(&fixture.context.paths.data_dir);
    fixture.stop().await;
}

#[tokio::test]
async fn idle_child_live_mode_is_scoped_to_child_and_respects_owner_plan() {
    for mode in ["bypass", "plan"] {
        let fixture = Fixture::start(mode, false).await;
        fixture.send("IDLE_MODE");
        let job = fixture.next_job(1).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            fixture.model.slow_started.notified(),
        )
        .await
        .unwrap();
        let target = worker::permission_mode_target(
            &fixture.context.paths.data_dir,
            &fixture.owner,
            &job.id,
        )
        .await
        .unwrap();
        assert_eq!(target.1, fixture.child);
        if mode == "plan" {
            assert!(worker::set_permission_mode(
                &fixture.context.paths.data_dir,
                &fixture.owner,
                &job.id,
                "accept-edits"
            )
            .await
            .is_err());
        }
        worker::set_permission_mode(
            &fixture.context.paths.data_dir,
            &fixture.owner,
            &job.id,
            "plan",
        )
        .await
        .unwrap();
        fixture.model.release.notify_one();
        assert_eq!(
            completed(&fixture.context.paths.data_dir, &job.id)
                .await
                .status,
            "completed"
        );
        assert!(!fixture.workspace.join("idle.txt").exists());
        assert_eq!(
            claude_core::services::session_permission_mode::load(&fixture.store.db, &fixture.owner)
                .unwrap()
                .map(claude_core::services::session_permission_mode::name),
            Some(mode)
        );
        assert_eq!(
            claude_core::services::session_permission_mode::load(&fixture.store.db, &fixture.child)
                .unwrap()
                .map(claude_core::services::session_permission_mode::name),
            Some("plan")
        );
        let reply = worker::call(
            &fixture.host,
            &worker::Request::GetPermissionMode { job_id: job.id },
        )
        .await;
        assert!(
            reply.is_err(),
            "Finished child must not leave a live mode target"
        );
        fixture.stop().await;
    }
}

#[tokio::test]
async fn idle_child_permission_waits_for_owner_review_before_editing() {
    let fixture = Fixture::start("ask", false).await;
    fixture.send("IDLE_WRITE");
    let job = fixture.next_job(1).await;
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(question) =
                jobs::pending_question(&fixture.context.paths.data_dir, &job.id, &fixture.owner)
                    .unwrap()
            {
                return question;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(question.kind, "permission");
    assert!(question.question.contains(&fixture.child));
    assert!(!fixture.workspace.join("idle.txt").exists());
    jobs::answer_question(
        &fixture.context.paths.data_dir,
        &job.id,
        &fixture.owner,
        &question.request_id,
        "yes",
    )
    .unwrap();
    assert_eq!(
        completed(&fixture.context.paths.data_dir, &job.id)
            .await
            .status,
        "completed"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("idle.txt")).unwrap(),
        "IDLE_CHILD_WRITE"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn idle_owned_child_routes_questions_and_cancellation_through_visible_jobs() {
    let fixture = Fixture::start("ask", false).await;
    fixture.send("IDLE_QUESTION");
    let job = fixture.next_job(1).await;
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(question) =
                jobs::pending_question(&fixture.context.paths.data_dir, &job.id, &fixture.owner)
                    .unwrap()
            {
                return question;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert!(question.question.contains(&fixture.child));
    jobs::answer_question(
        &fixture.context.paths.data_dir,
        &job.id,
        &fixture.owner,
        &question.request_id,
        "1",
    )
    .unwrap();
    assert_eq!(
        completed(&fixture.context.paths.data_dir, &job.id)
            .await
            .status,
        "completed"
    );
    fixture.send("IDLE_SLOW");
    let job = fixture.next_job(2).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.model.slow_started.notified(),
    )
    .await
    .unwrap();
    jobs::cancel(&fixture.context.paths.data_dir, &job.id).unwrap();
    assert_eq!(
        completed(&fixture.context.paths.data_dir, &job.id)
            .await
            .status,
        "cancelled"
    );
    let count = fixture.model.requests.lock().unwrap().len();
    fixture.send("IDLE_WRITE: must remain pending after cancellation");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fixture.model.requests.lock().unwrap().len(), count);
    assert!(!fixture.workspace.join("idle.txt").exists());
    assert_eq!(
        session_messages::pending_count(&fixture.context, &fixture.child).unwrap(),
        1
    );
    fixture.stop().await;
}

#[tokio::test]
async fn idle_child_budget_is_shared_with_owner_turn_continuations() {
    let fixture = Fixture::start("bypass", true).await;
    assert_eq!(fixture.model.requests.lock().unwrap().len(), 5);
    for index in 0..7 {
        fixture.send(&format!("IDLE_PING_{index}"));
        let job = fixture.next_job(index + 1).await;
        assert_eq!(
            completed(&fixture.context.paths.data_dir, &job.id)
                .await
                .status,
            "completed"
        );
    }
    let count = fixture.model.requests.lock().unwrap().len();
    fixture.send("IDLE_WRITE: exceeds the shared eight-resume budget");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fixture.model.requests.lock().unwrap().len(), count);
    assert_eq!(
        jobs::list(&fixture.context.paths.data_dir, Some(&fixture.owner))
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        session_messages::pending_count(&fixture.context, &fixture.child).unwrap(),
        1
    );
    fixture.stop().await;
}

#[tokio::test]
async fn idle_child_revalidates_policy_sender_trust_and_process_residency() {
    for case in [
        "policy", "sender", "restart", "failure", "lease", "binding", "foreign",
    ] {
        let mut fixture = Fixture::start("bypass", false).await;
        let lease = if case == "lease" {
            session_messages::ExecutionLease::acquire(
                &fixture.context.paths.data_dir,
                &fixture.child,
            )
            .unwrap()
        } else {
            None
        };
        if case == "binding" {
            fixture
                .store
                .db
                .with_conn(|conn| {
                    conn.execute(
                        "UPDATE conversations SET model='foreign-model' WHERE id=?1",
                        [&fixture.child],
                    )
                })
                .unwrap()
                .unwrap();
        }
        if case == "foreign" {
            let key = format!("delegation.{}", fixture.child);
            let mut state: Value = fixture.context.setting(&key).unwrap().unwrap();
            state["parent_session_id"] = json!(fixture.peer);
            fixture.context.save_setting(&key, &state).unwrap();
        }
        if case == "policy" {
            fixture
                .context
                .save_setting(
                    &format!("cli.permissions.{}", fixture.owner),
                    &json!({"Write":"Deny"}),
                )
                .unwrap();
        }
        if case == "restart" {
            fixture.stop().await;
            fixture.host = worker::ensure_worker(
                &fixture.context.paths.data_dir,
                &fixture.executable,
                &fixture.owner,
                &fixture.workspace,
            )
            .await
            .unwrap();
            fixture
                ._cleanup
                .0
                .push(fixture.host.supervisor_pid.unwrap());
        }
        fixture.send(if case == "failure" {
            "IDLE_FAIL"
        } else {
            "IDLE_WRITE"
        });
        if case == "sender" {
            fixture
                .store
                .db
                .with_conn(|conn| {
                    conn.execute(
                        "UPDATE conversations SET archived=1 WHERE id=?1",
                        [&fixture.peer],
                    )
                })
                .unwrap()
                .unwrap();
        }
        if case != "restart" {
            let job = fixture.next_job(1).await;
            assert_eq!(
                completed(&fixture.context.paths.data_dir, &job.id)
                    .await
                    .status,
                "failed",
                "{case}"
            );
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            fixture.model.requests.lock().unwrap().len(),
            if case == "failure" { 4 } else { 3 },
            "{case}"
        );
        assert!(!fixture.workspace.join("idle.txt").exists());
        if case != "failure" {
            assert_eq!(
                session_messages::pending_count(&fixture.context, &fixture.child).unwrap(),
                1
            );
        }
        drop(lease);
        fixture.stop().await;
    }
}
