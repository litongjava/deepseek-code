use super::*;
use crate::native_engine::provider_manager::Provider;
use crate::runtime::{self, RuntimePaths, SessionStore};
use axum::{routing::post, Router};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Local SSE fixture. Only streaming requests are counted: the automatic title
/// request uses the same endpoint without `stream`, and is not a turn request.
struct Fixture {
    _temp: tempfile::TempDir,
    paths: RuntimePaths,
    store: SessionStore,
    id: String,
    engine: QueryEngine,
    streams: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

async fn fixture() -> Fixture {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(temp.path(), "fixture-model", "fixture").unwrap();

    let streams = Arc::new(AtomicUsize::new(0));
    let counter = streams.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::Json(body): axum::Json<Value>| {
            let counter = counter.clone();
            async move {
                if body["stream"] == true {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let provider: Provider = serde_json::from_value(json!({
        "id": "fixture",
        "name": "Fixture",
        "base_url": format!("http://{address}/v1"),
        "api_key": "fixture-only",
        "api_format": "openai",
        "enabled": true,
        "web_search_strategy": null,
        "models": [{
            "id": "fixture-model",
            "name": "Fixture",
            "enabled": true,
            "max_tokens": 64,
            "supports_vision": false,
            "supports_web_search": false,
        }],
    }))
    .unwrap();
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("providers.json"),
        ))),
        runtime::default_permissions(),
        Arc::new(FileCache::new()),
    );
    Fixture {
        _temp: temp,
        paths,
        store,
        id,
        engine,
        streams,
        server,
    }
}

impl Fixture {
    fn request(&self, prompt: &str) -> ChatRequest {
        ChatRequest {
            conversation_id: self.id.clone(),
            messages: vec![json!({"role":"user","content":prompt})],
            context_boundary: None,
            assistant_message_id: None,
            model: "fixture-model".into(),
            provider_id: Some("fixture".into()),
            effort: None,
            system_prompt: Some("You are a fixture.".into()),
            max_tokens: Some(64),
            max_turns: Some(1),
            response_schema: None,
            workspace_path: Some(self._temp.path().to_string_lossy().into_owned()),
            temperature: None,
            top_p: None,
            reasoning_mode: None,
        }
    }

    fn acquire_lease(&self) -> Arc<crate::services::session_messages::ExecutionLease> {
        Arc::new(
            crate::services::session_messages::ExecutionLease::acquire(
                &self.paths.data_dir,
                &self.id,
            )
            .expect("lease acquisition")
            .expect("session is a UUID"),
        )
    }

    fn active(&self) -> bool {
        crate::services::session_messages::execution_active(&self.paths.data_dir, &self.id)
            .expect("execution probe")
    }

    fn stored_messages(&self) -> i64 {
        self.store
            .db
            .with_conn(|conn| {
                Ok::<i64, anyhow::Error>(conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
                    [&self.id],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct Gate {
    entered: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
}

fn gate(engine: &QueryEngine) -> Gate {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    engine.install_executor_tail_gate(ExecutorTailGate {
        entered,
        release: release_rx,
    });
    Gate {
        entered: entered_rx,
        release,
    }
}

async fn drain_to_stop(events: &mut mpsc::Receiver<EngineEvent>) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .expect("event stream stalled before MessageStop")
            .expect("event stream closed before MessageStop");
        match event {
            EngineEvent::MessageStop { .. } => return,
            EngineEvent::Error(message) => panic!("engine error: {message}"),
            _ => {}
        }
    }
}

async fn drain_to_close(events: &mut mpsc::Receiver<EngineEvent>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while events.recv().await.is_some() {}
    })
    .await
    .expect("executor task did not close its event stream");
}

/// The executor owns the session through its whole outer task, including the
/// tail after the public `MessageStop`. Dropping both the host stream and the
/// host's lease must neither free the session nor allow an overlapping turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outer_executor_tail_keeps_ownership_after_message_stop() {
    let fixture = fixture().await;
    let gate = gate(&fixture.engine);
    let host_lease = fixture.acquire_lease();
    let mut events = fixture
        .engine
        .send_message_with_lease(fixture.request("first task"), false, host_lease.clone())
        .await
        .unwrap();
    drain_to_stop(&mut events).await;
    tokio::time::timeout(Duration::from_secs(10), gate.entered)
        .await
        .expect("executor never reached its tail gate")
        .unwrap();
    assert_eq!(fixture.streams.load(Ordering::SeqCst), 1);

    // The frontend stream and the host's own reference go away while the
    // executor is still in its tail. Ownership belongs to the engine task.
    drop(events);
    drop(host_lease);
    assert!(
        fixture.active(),
        "the session was released before executor cleanup ended"
    );
    assert!(
        crate::services::session_messages::ExecutionLease::acquire(
            &fixture.paths.data_dir,
            &fixture.id
        )
        .is_err(),
        "an independent lease was admitted while the executor tail was gated"
    );

    let error = fixture
        .engine
        .send_message(fixture.request("overlapping task"))
        .await
        .expect_err("an overlapping top-level submission must be refused");
    assert!(
        format!("{error:#}").contains("already executing"),
        "unexpected admission error: {error:#}"
    );
    assert_eq!(
        fixture.streams.load(Ordering::SeqCst),
        1,
        "the refused submission must not request the model"
    );
    assert_eq!(
        fixture.stored_messages(),
        0,
        "the refused submission must not record a message"
    );

    // A deadline does not establish completion: it reports Pending while the
    // attempt still owns the session.
    assert_eq!(
        fixture
            .engine
            .wait_for_executor(&fixture.id, Duration::from_millis(50))
            .await,
        ExecutorWait::Pending
    );
    assert!(
        fixture.active(),
        "a waiter deadline must not release the session"
    );
    assert!(
        crate::services::session_messages::ExecutionLease::acquire(
            &fixture.paths.data_dir,
            &fixture.id
        )
        .is_err(),
        "a waiter deadline must preserve exclusivity"
    );

    gate.release.send(()).unwrap();
    assert_eq!(
        fixture
            .engine
            .wait_for_executor(&fixture.id, Duration::from_secs(10))
            .await,
        ExecutorWait::Completed
    );
    assert!(
        !fixture.active(),
        "completed cleanup must release the session immediately"
    );
    crate::services::session_messages::ExecutionLease::acquire(
        &fixture.paths.data_dir,
        &fixture.id,
    )
    .expect("the session is free after the executor ends")
    .expect("a real session id");
    assert_eq!(fixture.engine.executor_attempt_count(), 0);
}

/// Cancelling an active executor must drop its lease guard and leave no
/// completion entry behind, even though cleanup never ran to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_gated_executor_releases_its_lease_and_bookkeeping() {
    let fixture = fixture().await;
    let gate = gate(&fixture.engine);
    let host_lease = fixture.acquire_lease();
    let mut events = fixture
        .engine
        .send_message_with_lease(fixture.request("cancelled task"), false, host_lease)
        .await
        .unwrap();
    drain_to_stop(&mut events).await;
    tokio::time::timeout(Duration::from_secs(10), gate.entered)
        .await
        .expect("executor never reached its tail gate")
        .unwrap();
    assert!(fixture.active());

    fixture.engine.cancel_turn(&fixture.id).await;
    assert!(
        !fixture.active(),
        "an aborted executor must drop its execution lease"
    );
    crate::services::session_messages::ExecutionLease::acquire(
        &fixture.paths.data_dir,
        &fixture.id,
    )
    .expect("the session is free after cancellation")
    .expect("a real session id");
    assert_eq!(fixture.engine.executor_attempt_count(), 0);
    assert_eq!(fixture.streams.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.stored_messages(), 0);
    // The task (and the gate receiver) no longer exists.
    let _ = gate.release.send(());
}

/// Completion bookkeeping is owned by the task, so callers that never wait do
/// not accumulate entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_bookkeeping_returns_to_baseline_without_a_waiter() {
    let fixture = fixture().await;
    let gate = gate(&fixture.engine);
    let mut events = fixture
        .engine
        .send_message_with_lease(fixture.request("unobserved task"), false, fixture.acquire_lease())
        .await
        .unwrap();
    drain_to_stop(&mut events).await;
    tokio::time::timeout(Duration::from_secs(10), gate.entered)
        .await
        .expect("executor never reached its tail gate")
        .unwrap();
    gate.release.send(()).unwrap();
    drain_to_close(&mut events).await;
    assert_eq!(
        fixture.engine.executor_attempt_count(),
        0,
        "a completed attempt must remove its own bookkeeping"
    );
    assert!(!fixture.active());
    crate::services::session_messages::ExecutionLease::acquire(
        &fixture.paths.data_dir,
        &fixture.id,
    )
    .expect("the session is free after the executor ends")
    .expect("a real session id");
}

/// An executor error before the first model request still releases the lease
/// and the completion entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_early_executor_error_still_releases_ownership() {
    let fixture = fixture().await;
    let mut events = fixture
        .engine
        .send_hook_wake(fixture.request("wake without a pending wake"), false)
        .await
        .unwrap();
    let mut saw_error = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .expect("event stream stalled before MessageStop")
            .expect("event stream closed before MessageStop");
        match event {
            EngineEvent::Error(_) => saw_error = true,
            EngineEvent::MessageStop { .. } => break,
            _ => {}
        }
    }
    assert!(saw_error, "the early failure must be reported");
    drain_to_close(&mut events).await;
    assert_eq!(
        fixture.streams.load(Ordering::SeqCst),
        0,
        "an early error must not request the model"
    );
    assert!(!fixture.active());
    crate::services::session_messages::ExecutionLease::acquire(
        &fixture.paths.data_dir,
        &fixture.id,
    )
    .expect("the session is free after the early error")
    .expect("a real session id");
    assert_eq!(fixture.engine.executor_attempt_count(), 0);
}

#[test]
fn a_stale_completion_cannot_remove_or_wait_for_a_newer_attempt() {
    let attempts = Arc::new(ExecutorAttempts::default());
    let first = attempts.begin("session", None);
    let second = attempts.begin("session", None);
    drop(first);
    assert_eq!(
        attempts.len(),
        1,
        "stale cleanup removed the newer attempt"
    );
    assert!(
        attempts.waiter("session").is_some(),
        "a stale attempt must not erase the newer waiter"
    );
    drop(second);
    assert_eq!(attempts.len(), 0);
}

#[tokio::test]
async fn a_deadline_reports_pending_and_completion_is_signalled_on_drop() {
    let attempts = Arc::new(ExecutorAttempts::default());
    let guard = attempts.begin("session", None);
    let mut waiter = attempts.waiter("session").unwrap();
    assert_eq!(
        waiter.wait(Duration::from_millis(25)).await,
        ExecutorWait::Pending
    );
    drop(guard);
    assert_eq!(
        waiter.wait(Duration::from_secs(1)).await,
        ExecutorWait::Completed
    );
    assert_eq!(attempts.len(), 0);
}

/// A started `spawn_blocking` job cannot be aborted by dropping its handle, so
/// cancelling the outer task while the mandatory assistant persistence runs must
/// not release the session or report completion. Only the real end of that job
/// may do so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_turn_keeps_ownership_until_started_persistence_ends() {
    let fixture = fixture().await;
    // Gate inside the mandatory persistence job, after it starts and before it
    // mutates the database.
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    fixture
        .engine
        .install_persistence_gate(PersistenceGate {
            entered: entered_tx,
            release: release_rx,
        });

    // The engine acquires the lease itself for a direct submission; the turn
    // must run the real success tail with `save_response`.
    let mut events = fixture
        .engine
        .send_message_with_history(fixture.request("persisted task"), true)
        .await
        .unwrap();
    drain_to_stop(&mut events).await;
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .expect("the persistence job never started")
        .unwrap();
    assert_eq!(fixture.streams.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.stored_messages(),
        0,
        "the gate must sit before the actual mutation"
    );

    // Host goes away and the outer executor task is cancelled while the
    // blocking job is still parked before its write.
    drop(events);
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.engine.cancel_turn(&fixture.id),
    )
    .await
    .expect("cancelling with the persistence gate closed must not deadlock");

    assert!(
        fixture.active(),
        "a started persistence job must keep the session owned after cancellation"
    );
    assert!(
        crate::services::session_messages::ExecutionLease::acquire(
            &fixture.paths.data_dir,
            &fixture.id
        )
        .is_err(),
        "an independent lease was admitted while persistence was in flight"
    );
    let error = fixture
        .engine
        .send_message_with_history(fixture.request("overlapping task"), true)
        .await
        .expect_err("an overlapping submission must be refused");
    assert!(
        format!("{error:#}").contains("already executing"),
        "unexpected admission error: {error:#}"
    );
    assert_eq!(
        fixture.streams.load(Ordering::SeqCst),
        1,
        "the refused submission must not request the model"
    );
    assert_eq!(
        fixture
            .engine
            .wait_for_executor(&fixture.id, Duration::from_millis(50))
            .await,
        ExecutorWait::Pending,
        "a finished outer task must not report completion while its blocking job runs"
    );
    assert_eq!(
        fixture.engine.executor_attempt_count(),
        1,
        "completion bookkeeping must survive while the blocking job runs"
    );
    assert_eq!(
        fixture.stored_messages(),
        0,
        "the gated job must not have mutated the database yet"
    );

    // Release the job: it now performs the mandatory write and ends.
    release.send(()).unwrap();
    assert_eq!(
        fixture
            .engine
            .wait_for_executor(&fixture.id, Duration::from_secs(10))
            .await,
        ExecutorWait::Completed
    );
    assert!(
        !fixture.active(),
        "completion must imply the session is free"
    );
    crate::services::session_messages::ExecutionLease::acquire(
        &fixture.paths.data_dir,
        &fixture.id,
    )
    .expect("the session is free after the persistence job ends")
    .expect("a real session id");
    assert_eq!(fixture.engine.executor_attempt_count(), 0);
    assert_eq!(
        fixture.stored_messages(),
        1,
        "the old turn's mandatory persistence must happen exactly once"
    );

    // A new admitted turn must not be followed by any further old-turn write.
    let mut next = fixture
        .engine
        .send_message_with_history(fixture.request("next task"), true)
        .await
        .expect("the next turn is admitted once the old attempt ended");
    drain_to_close(&mut next).await;
    assert_eq!(
        fixture.stored_messages(),
        2,
        "no old-turn mutation may land after a new turn was admitted"
    );
    assert_eq!(fixture.streams.load(Ordering::SeqCst), 2);
    assert!(!fixture.active());
}
