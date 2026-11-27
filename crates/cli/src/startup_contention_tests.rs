//! An explicit binding change at startup publishes through the bounded write
//! path, so it waits for another host's writer instead of failing at the
//! pooled connection's 5 s busy_timeout.
use super::*;

use std::{thread, time::Duration};

struct Fixture {
    _root: tempfile::TempDir,
    paths: RuntimePaths,
    store: SessionStore,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store
            .create(root.path(), "old-model", "old-provider")
            .unwrap();
        Self {
            _root: root,
            paths,
            store,
            id,
        }
    }

    /// An active background job skips the durable permission-mode write, so the
    /// explicit binding publication is the first startup write that needs the
    /// writer. Without it an earlier write would absorb the contention and the
    /// binding write would never be exercised.
    fn with_active_job() -> Self {
        let fixture = Self::new();
        let job = uuid::Uuid::new_v4().to_string();
        let directory = fixture.paths.data_dir.join("tasks/background").join(&job);
        std::fs::create_dir_all(&directory).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let pid = std::process::id();
        let state = json!({
            "id": job,
            "foreground": false,
            "parent_session_id": fixture.id,
            "session_id": fixture.id,
            "title": "Contention fixture",
            "status": "running",
            "supervisor_pid": pid,
            "child_pid": pid,
            "started_ms": now,
            "heartbeat_ms": now,
            "finished_ms": null,
            "exit_code": null,
            "error": null,
            "launched": true
        });
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        fixture
    }

    fn lock_writer(&self) -> rusqlite::Connection {
        let holder = rusqlite::Connection::open_with_flags(self.paths.data_dir.join("global.db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        holder
    }

    fn hold_past_busy_timeout(&self) -> thread::JoinHandle<()> {
        let holder = self.lock_writer();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(6));
            holder.execute_batch("ROLLBACK").unwrap();
        })
    }

    /// Hold the writer until the test releases it, past any retry budget.
    fn hold_until_released(&self) -> (std::sync::mpsc::Sender<()>, thread::JoinHandle<()>) {
        let holder = self.lock_writer();
        let (release, released) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let _ = released.recv();
            holder.execute_batch("ROLLBACK").unwrap();
        });
        (release, handle)
    }

    fn selection(&self) -> Selection {
        let provider = serde_json::from_value(json!({
            "id":"fixture", "name":"Fixture", "base_url":"http://127.0.0.1:1",
            "api_key":"unused", "api_format":"openai", "enabled":true,
            "models":[{"id":"new-model","name":"Fixture","enabled":true,
                "supports_vision":false,"supports_web_search":false}]
        }))
        .unwrap();
        Selection {
            providers: vec![provider],
            provider_id: "fixture".into(),
            model: "new-model".into(),
        }
    }

    async fn try_resume(&self, cli: &Cli) -> Result<Session> {
        Session::open(cli, &self.paths, self.selection(), Some(&self.id)).await
    }

    async fn resume(&self, cli: &Cli) -> Session {
        self.try_resume(cli).await.unwrap()
    }
}

/// A resume whose `--model`/`--provider` differ from the durable selector
/// publishes a new generation; it must keep waiting past the 5 s busy_timeout.
fn explicit_change_cli() -> Cli {
    let cli = Cli::try_parse_with_mode_source([
        "dcl",
        "--print",
        "--model",
        "new-model",
        "--provider",
        "fixture",
    ])
    .unwrap();
    assert!(cli.changes_binding());
    cli
}

#[tokio::test]
async fn explicit_model_change_waits_for_writer_after_init() {
    let fixture = Fixture::with_active_job();
    let cli = explicit_change_cli();
    let holder = fixture.hold_past_busy_timeout();
    let started = std::time::Instant::now();
    let session = fixture.resume(&cli).await;
    let waited = started.elapsed();
    holder.join().unwrap();
    assert!(
        waited >= Duration::from_secs(5),
        "the binding write returned before the writer released: {waited:?}"
    );
    assert_eq!(session.binding_state().generation, 1);
    let saved = fixture.store.resume(&fixture.id).unwrap();
    assert_eq!(saved.model.as_deref(), Some("new-model"));
    assert_eq!(saved.provider.as_deref(), Some("fixture"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn explicit_model_change_reports_the_labelled_lock_error_after_the_deadline() {
    let fixture = Fixture::with_active_job();
    let cli = explicit_change_cli();
    let (release, holder) = fixture.hold_until_released();
    let started = std::time::Instant::now();
    let error = match fixture.try_resume(&cli).await {
        Ok(_) => panic!("the binding write succeeded while the writer was still held"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();
    drop(release);
    holder.join().unwrap();
    let text = format!("{error:#}");
    assert!(deepseek_code_core::db::is_contention(&error), "{text}");
    assert!(
        text.contains(deepseek_code_core::native_engine::session_binding::store::PUBLISH_LABEL),
        "{text}"
    );
    assert!(text.contains("database stayed locked"), "{text}");
    assert!(
        elapsed >= deepseek_code_core::db::STARTUP_WRITE_DEADLINE,
        "the binding write gave up before its retry budget: {elapsed:?}"
    );
    // No abandoned attempt may have published a generation or moved the selector.
    let observed = fixture.store.db.observed_binding(&fixture.id).unwrap();
    assert_eq!(observed.generation, 0);
    assert_eq!(observed.model, "old-model");
    assert_eq!(observed.provider, "old-provider");
    // The same explicit change applies once the writer is gone.
    let session = fixture.resume(&cli).await;
    assert_eq!(session.binding_state().generation, 1);
    session.close().await.unwrap();
}

#[tokio::test]
async fn title_waits_for_writer_after_init() {
    let fixture = Fixture::new();
    let cli = Cli::try_parse_with_mode_source([
        "dcl",
        "--name",
        "New title",
        "--model",
        "new-model",
        "--provider",
        "fixture",
    ])
    .unwrap();
    let mut session = fixture.resume(&cli).await;
    let holder = fixture.hold_past_busy_timeout();
    let started = std::time::Instant::now();
    crate::startup_options::apply(&cli, &mut session)
        .await
        .unwrap();
    let waited = started.elapsed();
    holder.join().unwrap();
    assert!(
        waited >= Duration::from_secs(5),
        "the title write returned before the writer released: {waited:?}"
    );
    assert_eq!(
        fixture.store.resume(&fixture.id).unwrap().title.as_deref(),
        Some("New title")
    );
    session.close().await.unwrap();
}

#[test]
fn idle_resume_releases_execution_lease_while_writer_is_held() {
    let fixture = Fixture::new();
    let holder = fixture.lock_writer();
    let (done, finished) = std::sync::mpsc::channel();
    // Keep the writer until both settlement and an independent lease probe finish.
    thread::scope(|scope| {
        scope.spawn(|| {
            let settled = settle_abandoned_turns(&fixture.paths, &fixture.store, &fixture.id);
            done.send(settled).unwrap();
        });
        let result = finished.recv_timeout(Duration::from_secs(2));
        let probe = fixture.store.db.open_session(&fixture.id).and_then(|s|s.acquire_execution());
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(result
            .expect("idle resume waited for the unrelated writer")
            .is_empty());
        assert!(probe.is_ok());
    });
}
