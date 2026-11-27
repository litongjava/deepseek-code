#[path = "support/session_sql.rs"]
mod session_sql;
use session_sql::FixtureSql;
// A request snapshot must describe the body it actually sent. These tests drive
// the real executor and HTTP request, then read the emitted snapshot, so the
// binding cannot be faked by writing metadata by hand.
use deepseek_code_core::{
    native_engine::{
        engine_core::ReasoningMode,
        provider_manager::{ApiFormat, ResolvedProvider},
        tool_loop::{EngineEvent, ToolLoopExecutor},
    },
    runtime::{RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

fn provider(base: &str, format: &str) -> ResolvedProvider {
    let _ = deepseek_code_core::config::OrchestrationConfig::init_global();
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "fixture", "name": "Fixture", "base_url": base,
            "api_key": "fixture-key", "api_format": format,
            "models": [], "enabled": true
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id": "fixture-model", "name": "Fixture", "enabled": true,
            "max_tokens": 4096, "context_window": 128000,
            "supports_vision": false, "supports_web_search": false
        }))
        .unwrap(),
    }
}

struct Mock {
    base: String,
    bodies: Arc<std::sync::Mutex<Vec<Value>>>,
}

async fn mock() -> Mock {
    mock_with_delay(false).await
}

async fn mock_with_delay(delay: bool) -> Mock {
    let bodies: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let captured = bodies.clone();
    let handler = move |axum::Json(body): axum::Json<Value>| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push(body);
            if delay {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let stream = concat!(
                "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                "data: [DONE]\n\n"
            );
            ([("content-type", "text/event-stream")], stream)
        }
    };
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(handler.clone()))
        .route("/v1/messages", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock { base, bodies }
}

fn store(dir: &std::path::Path) -> SessionStore {
    SessionStore::new(
        RuntimePaths::new(dir.join("data"))
            .unwrap()
            .open_database()
            .unwrap(),
    )
}

/// Run one turn and return the request snapshots the executor emitted.
async fn run(
    provider: ResolvedProvider,
    messages: Vec<Value>,
    boundary: Option<String>,
    cwd: &std::path::Path,
    store: &SessionStore,
    conversation: &str,
) -> Vec<Value> {
    let (tx, mut rx) = mpsc::channel(256);
    let mut executor = ToolLoopExecutor::new(
        provider,
        messages,
        None,
        4096,
        tx,
        cwd.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    // A DB-aware executor is what the product builds, so the old behaviour
    // (querying the latest boundary at send time) would be exercised too.
    .with_goal_state(store.db.clone())
    .with_conv_id(conversation.to_string())
    .with_context_boundary(boundary);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), executor.execute())
        .await
        .expect("the turn finished");
    assert!(outcome.is_ok(), "turn failed: {outcome:?}");
    let mut snapshots = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let EngineEvent::RequestSnapshot(snapshot) = event {
            snapshots.push(snapshot);
        }
    }
    snapshots
}

fn boundary_ids(snapshots: &[Value]) -> Vec<Option<&str>> {
    snapshots
        .iter()
        .map(|snapshot| snapshot["contextBoundary"].as_str())
        .collect()
}

fn openai(dir: &std::path::Path) -> ApiFormat {
    let _ = dir;
    ApiFormat::OpenAI
}

/// The executor keeps the boundary its messages were built from, even after a
/// later compaction makes a different boundary the stored latest one.
async fn pre_compaction_executor_keeps_its_own_boundary(format: &str) {
    let server = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let id = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    store
        .append(&id, "user", "ORIGINAL_CONTEXT_MARKER first request")
        .unwrap();
    store.append(&id, "assistant", "Working").unwrap();

    // Read the messages and their boundary together.
    let (messages, boundary) = store
        .model_history_with_boundary(&id, &openai(dir.path()))
        .unwrap();
    assert_eq!(boundary, None, "{format}: no boundary exists yet");

    // Another entry commits a compaction before this executor sends.
    let prepared = compaction::prepare(&store.db, &id).unwrap();
    compaction::commit(&store.db, prepared, "NEW_SUMMARY_MARKER summary").unwrap();

    let snapshots = run(
        provider(&server.base, format),
        messages,
        boundary,
        dir.path(),
        &store,
        &id,
    )
    .await;
    let body = server
        .bodies
        .lock()
        .unwrap()
        .pop()
        .expect("the request reached the endpoint")
        .to_string();
    assert!(body.contains("ORIGINAL_CONTEXT_MARKER"), "{format}: {body}");
    assert!(
        !body.contains("NEW_SUMMARY_MARKER"),
        "{format}: the new summary must not be sent: {body}"
    );
    assert!(!snapshots.is_empty(), "{format}: the request was recorded");
    for snapshot in &snapshots {
        assert!(
            snapshot["contextBoundary"].is_null(),
            "{format}: the old body must not be labelled with the new boundary: {snapshot}"
        );
    }
}

#[tokio::test]
async fn an_openai_executor_keeps_the_boundary_of_the_body_it_sent() {
    pre_compaction_executor_keeps_its_own_boundary("openai").await;
}

#[tokio::test]
async fn an_anthropic_executor_keeps_the_boundary_of_the_body_it_sent() {
    pre_compaction_executor_keeps_its_own_boundary("anthropic").await;
}

/// The binding follows the messages, so a transcript built from a committed
/// summary reports that summary's boundary even after a later compaction.
#[tokio::test]
async fn a_transcript_built_from_a_summary_reports_that_summary() {
    for format in ["openai", "anthropic"] {
        let server = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = store
            .create(dir.path(), "fixture-model", "fixture")
            .unwrap();
        store.append(&id, "user", "Task").unwrap();
        store.append(&id, "assistant", "Result").unwrap();
        let prepared = compaction::prepare(&store.db, &id).unwrap();
        compaction::commit(&store.db, prepared, "FIRST_SUMMARY summary").unwrap();
        store.append(&id, "user", "Next task").unwrap();
        let (messages, boundary) = store
            .model_history_with_boundary(&id, &openai(dir.path()))
            .unwrap();
        let first = boundary.expect("the transcript starts at a boundary");
        // A later compaction must not change what these messages claim.
        let prepared = compaction::prepare(&store.db, &id).unwrap();
        compaction::commit(&store.db, prepared, "SECOND_SUMMARY summary").unwrap();

        let snapshots = run(
            provider(&server.base, format),
            messages,
            Some(first.clone()),
            dir.path(),
            &store,
            &id,
        )
        .await;
        assert!(
            boundary_ids(&snapshots)
                .iter()
                .all(|id| *id == Some(first.as_str())),
            "{snapshots:?}"
        );
        let body = server.bodies.lock().unwrap().pop().unwrap().to_string();
        assert!(body.contains("FIRST_SUMMARY"));
        assert!(!body.contains("SECOND_SUMMARY"));
    }
}

/// Without a proven boundary the snapshot stays unattributed; status then
/// keeps the request out of the current context instead of guessing.
#[tokio::test]
async fn an_unattributed_executor_never_claims_the_latest_boundary() {
    for format in ["openai", "anthropic"] {
        let server = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = store
            .create(dir.path(), "fixture-model", "fixture")
            .unwrap();
        store.append(&id, "user", "Task").unwrap();
        store.append(&id, "assistant", "Result").unwrap();
        let prepared = compaction::prepare(&store.db, &id).unwrap();
        compaction::commit(&store.db, prepared, "summary").unwrap();

        let snapshots = run(
            provider(&server.base, format),
            vec![json!({"role":"user","content":"client supplied"})],
            None,
            dir.path(),
            &store,
            &id,
        )
        .await;
        assert!(!snapshots.is_empty());
        assert!(
            boundary_ids(&snapshots).iter().all(Option::is_none),
            "{snapshots:?}"
        );
    }
}

#[tokio::test]
async fn late_response_keeps_its_boundary_after_a_concurrent_commit() {
    for format in ["openai", "anthropic"] {
        let server = mock_with_delay(true).await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = store
            .create(dir.path(), "fixture-model", "fixture")
            .unwrap();
        store.append(&id, "user", "OLD_REQUEST").unwrap();
        store.append(&id, "assistant", "old answer").unwrap();
        let (messages, boundary) = store
            .model_history_with_boundary(&id, &ApiFormat::OpenAI)
            .unwrap();
        let future = run(
            provider(&server.base, format),
            messages,
            boundary,
            dir.path(),
            &store,
            &id,
        );
        tokio::pin!(future);
        tokio::select! {
            _ = &mut future => panic!("response must remain pending"),
            _ = async {
                while server.bodies.lock().unwrap().is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => {}
        }
        let prepared = compaction::prepare(&store.db, &id).unwrap();
        compaction::commit(&store.db, prepared, "NEW_BOUNDARY").unwrap();
        let snapshots = future.await;
        assert!(!snapshots.is_empty());
        assert!(boundary_ids(&snapshots).iter().all(Option::is_none));
        assert!(server.bodies.lock().unwrap()[0]
            .to_string()
            .contains("OLD_REQUEST"));
    }
}

#[tokio::test]
async fn restored_and_reset_histories_bind_the_requests_they_send() {
    for format in ["openai", "anthropic"] {
        let server = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let original = store(dir.path());
        let id = original
            .create(dir.path(), "fixture-model", "fixture")
            .unwrap();
        original.append(&id, "user", "task").unwrap();
        original.append(&id, "assistant", "answer").unwrap();
        let prepared = compaction::prepare(&original.db, &id).unwrap();
        compaction::commit(&original.db, prepared, "RESTORED_SUMMARY").unwrap();
        drop(original);
        let restored = store(dir.path());
        let (messages, boundary) = restored
            .model_history_with_boundary(&id, &ApiFormat::OpenAI)
            .unwrap();
        assert!(boundary.is_some());
        let snapshots = run(
            provider(&server.base, format),
            messages,
            boundary.clone(),
            dir.path(),
            &restored,
            &id,
        )
        .await;
        assert!(!snapshots.is_empty());
        assert!(boundary_ids(&snapshots)
            .iter()
            .all(|b| *b == boundary.as_deref()));
        restored.db.open_session(&id).unwrap().write(|w|w.replace_messages(&[])).unwrap();
        restored.append(&id, "user", "RESET_CONTEXT").unwrap();
        let (messages, boundary) = restored
            .model_history_with_boundary(&id, &ApiFormat::OpenAI)
            .unwrap();
        assert!(boundary.is_none());
        let snapshots = run(
            provider(&server.base, format),
            messages,
            boundary,
            dir.path(),
            &restored,
            &id,
        )
        .await;
        assert!(!snapshots.is_empty());
        assert!(boundary_ids(&snapshots).iter().all(Option::is_none));
        let body = server.bodies.lock().unwrap().last().unwrap().to_string();
        assert!(body.contains("RESET_CONTEXT") && !body.contains("RESTORED_SUMMARY"));
    }
}

/// Guard the production writers. History migration, import and fork copy existing
/// row flags; creating a fresh boundary must go through the transactional writer.
#[test]
fn new_memory_boundaries_have_one_writer() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let patterns = [
        r"(?i)\bis_compact_boundary\s*=\s*(?:excluded\.|\?\d+)",
        r"(?i)\bset\s+is_compact_boundary\s*=\s*(?:1|true)",
        r#"(?s)\binsert_message\([^;]{0,1000}?,\s*(?:true|boundary|is_boundary)\s*,"#,
        r#"set_prompt_origin\([^;]*?,\s*"(?:approved_plan|compaction)""#,
        r#"promptOrigin"\s*:\s*"(?:approved_plan|compaction)""#,
    ]
    .map(|pattern| regex::Regex::new(pattern).unwrap());
    let test_modules = regex::Regex::new(r"(?ms)^#\[cfg\(test\)\]\s*mod \w+\s*\{.*?^\}").unwrap();
    let mut violations = Vec::new();
    for tree in ["crates/core/src", "crates/cli/src", "src-tauri/src"] {
        for entry in walkdir::WalkDir::new(root.join(tree)) {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy();
            if path.extension().is_none_or(|ext| ext != "rs") || name.ends_with("tests.rs") {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let production_source = test_modules.replace_all(&source, "");
            let mut production = production_source.as_ref();
            let mut outside_writer = String::new();
            if path.ends_with("db/session_store/boundary.rs") {
                let (before, writer) = production.split_once("pub(crate) fn seal(").unwrap();
                let (_, after) = writer.split_once("\n}").unwrap();
                outside_writer.push_str(before);
                outside_writer.push_str(after);
                production = &outside_writer;
            }
            // A parameterized flag in a sync INSERT bypasses origin and snapshot
            // writes even if its ON CONFLICT clause never mentions the flag.
            if path.ends_with("lifecycle/sync.rs") {
                let direct = regex::Regex::new(
                    r"(?is)insert\s+(?:or\s+\w+\s+)?into\s+messages\s*\([^)]*is_compact_boundary",
                )
                .unwrap();
                assert!(
                    !direct.is_match(production)
                        && !regex::Regex::new(r"(?i)SET\s+is_compact_boundary\s*=")
                            .unwrap()
                            .is_match(production),
                    "Sync writes boundary flags directly"
                );
            }
            for pattern in &patterns {
                for hit in pattern.find_iter(production) {
                    // Native compaction seals its own row in the same transaction
                    // that inserts native_boundaries; it is a reviewed writer.
                    if path.ends_with("db/session_store/history.rs")
                        && hit
                            .as_str()
                            .to_ascii_lowercase()
                            .contains("set is_compact_boundary=1")
                    {
                        continue;
                    }
                    violations.push(format!(
                        "{} at byte {}: {}",
                        path.strip_prefix(&root).unwrap().display(),
                        hit.start(),
                        hit.as_str()
                    ));
                }
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
