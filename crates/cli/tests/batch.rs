//! Real detached CLI supervisors and workers against a local model fixture.
use axum::{extract::State, routing::post, Router};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::batch::{self, Proposal, Unit, WorkerLaunch},
};
use serde_json::json;
use std::{
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Fixture {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    delay: Duration,
}
async fn respond(State(state): State<Fixture>) -> impl axum::response::IntoResponse {
    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak.fetch_max(active, Ordering::SeqCst);
    state.calls.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(state.delay).await;
    state.active.fetch_sub(1, Ordering::SeqCst);
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"No changes needed. PR: none."},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    );
    ([("content-type", "text/event-stream")], body)
}
fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
async fn exercise(cancel: bool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    std::fs::write(repo.join("README.md"), "fixture").unwrap();
    git(&repo, &["add", "README.md"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
    let root = dir.path().join("data");
    let paths = RuntimePaths::new(root.clone()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let parent = store.create(&repo, "fixture-model", "cli-runtime").unwrap();
    let denied = r#"{"Bash":"Deny"}"#;
    store
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
                [format!("cli.permissions.{parent}"), denied.into()],
            )
        })
        .unwrap()
        .unwrap();
    let proposal = Proposal {
        summary: "Independent changes".into(),
        verification: "Check assigned file".into(),
        units: (0..5)
            .map(|i| Unit {
                title: format!("Task {i}"),
                paths: vec![format!("module-{i}")],
                instructions: "Inspect and report".into(),
            })
            .collect(),
    };
    let plan = batch::create(&root, &parent, &repo, "Inspect modules", proposal).unwrap();
    let fixture = Fixture {
        active: Arc::new(AtomicUsize::new(0)),
        peak: Arc::new(AtomicUsize::new(0)),
        calls: Arc::new(AtomicUsize::new(0)),
        delay: Duration::from_millis(if cancel { 5000 } else { 450 }),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let launch = WorkerLaunch {
        executable: env!("CARGO_BIN_EXE_dcl").into(),
        arguments: vec![
            "--data-dir".into(),
            root.to_string_lossy().into_owned(),
            "--base-url".into(),
            url,
            "--api-format".into(),
            "openai".into(),
            "--api-key-env".into(),
            "BATCH_FIXTURE_KEY".into(),
            "--model".into(),
            "fixture-model".into(),
        ],
        model: "fixture-model".into(),
        provider: "cli-runtime".into(),
        permission_mode: "ask".into(),
    };
    batch::start(
        &root,
        &plan.id,
        launch,
        2,
        false,
        &[("BATCH_FIXTURE_KEY".into(), "fixture-not-secret".into())],
    )
    .unwrap();
    let started = Instant::now();
    let mut cancelled = false;
    let result = loop {
        let state = batch::get(&root, &plan.id).unwrap();
        if cancel && !cancelled && fixture.calls.load(Ordering::SeqCst) > 0 {
            batch::cancel(&root, &plan.id).unwrap();
            cancelled = true;
        }
        if !matches!(state.status.as_str(), "running" | "unresponsive") {
            break state;
        }
        if started.elapsed() > Duration::from_secs(40) {
            let _ = batch::cancel(&root, &plan.id);
            panic!("batch timed out: {state:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    if result.status == "failed" {
        for worker in &result.workers {
            if let Some(id) = &worker.job_id {
                eprintln!(
                    "WORKER LOG: {:?}",
                    deepseek_code_core::services::background_sessions::output(&root, id)
                );
            }
        }
    }
    assert_eq!(
        result.status,
        if cancel { "cancelled" } else { "completed" },
        "{result:?}"
    );
    assert!(fixture.peak.load(Ordering::SeqCst) <= 2);
    assert!(!result.publish);
    assert_eq!(result.permission_mode, "ask");
    assert!(result
        .workers
        .iter()
        .all(|w| w.pr.is_none() && w.commit.is_none()));
    if cancel {
        assert!(result.workers.iter().filter(|w| w.job_id.is_some()).count() <= 2);
    } else {
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 5);
        assert_eq!(fixture.peak.load(Ordering::SeqCst), 2);
        assert!(result
            .workers
            .iter()
            .all(|w| w.status == "no_changes" && w.worktree.join("README.md").exists()));
    }
    for worker in &result.workers {
        if let Some(session) = &worker.session_id {
            let value: String = store
                .db
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT value FROM peripheral_settings WHERE key=?1",
                        [format!("cli.permissions.{session}")],
                        |row| row.get(0),
                    )
                })
                .unwrap()
                .unwrap();
            assert_eq!(value, denied);
        }
    }
    assert!(!repo.join("module-0").exists());
    git(&repo, &["diff", "--exit-code"]);
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_workers_obey_concurrency_and_preserve_permissions() {
    exercise(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_stops_workers_and_keeps_queued_work_unstarted() {
    exercise(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_failures_are_reported_and_the_batch_fails() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    std::fs::write(repo.join("README.md"), "fixture").unwrap();
    git(&repo, &["add", "README.md"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
    let root = dir.path().join("data");
    let paths = RuntimePaths::new(root.clone()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let parent = store.create(&repo, "fixture-model", "cli-runtime").unwrap();
    let plan = batch::create(
        &root,
        &parent,
        &repo,
        "Fail workers",
        Proposal {
            summary: "Independent changes".into(),
            verification: "Check assigned file".into(),
            units: (0..5)
                .map(|i| Unit {
                    title: format!("Task {i}"),
                    paths: vec![format!("module-{i}")],
                    instructions: "Inspect and report".into(),
                })
                .collect(),
        },
    )
    .unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "fixture model failure",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let launch = WorkerLaunch {
        executable: env!("CARGO_BIN_EXE_dcl").into(),
        arguments: vec![
            "--data-dir".into(),
            root.to_string_lossy().into_owned(),
            "--base-url".into(),
            url,
            "--api-format".into(),
            "openai".into(),
            "--api-key-env".into(),
            "BATCH_FIXTURE_KEY".into(),
            "--model".into(),
            "fixture-model".into(),
        ],
        model: "fixture-model".into(),
        provider: "cli-runtime".into(),
        permission_mode: "ask".into(),
    };
    batch::start(
        &root,
        &plan.id,
        launch,
        2,
        false,
        &[("BATCH_FIXTURE_KEY".into(), "fixture-not-secret".into())],
    )
    .unwrap();
    let started = Instant::now();
    let result = loop {
        let state = batch::get(&root, &plan.id).unwrap();
        if !matches!(state.status.as_str(), "running" | "unresponsive") {
            break state;
        }
        if started.elapsed() > Duration::from_secs(90) {
            let _ = batch::cancel(&root, &plan.id);
            panic!("batch timed out: {state:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(result.status, "failed", "{result:?}");
    assert!(result
        .workers
        .iter()
        .any(|worker| worker.status == "failed"));
    for worker in &result.workers {
        if let Some(job) = &worker.job_id {
            let job = deepseek_code_core::services::background_sessions::get(&root, job).unwrap();
            assert_ne!(job.status, "running");
        }
    }
    server.abort();
}
