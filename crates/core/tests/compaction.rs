use deepseek_code_core::{
    native_engine::provider_manager::ApiFormat,
    runtime::{RuntimePaths, SessionStore},
    services::compaction::{apply_boundary, commit, prepare},
};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, SessionStore, String) {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(temp.path(), "model", "provider").unwrap();
    store.append(&id, "user", "Implement task").unwrap();
    store
        .append(&id, "assistant", "Changed file, tests pending")
        .unwrap();
    (temp, store, id)
}

#[test]
fn summary_preserves_transcript_and_projects_after_reopen_and_fork() {
    let (temp, store, id) = fixture();
    let result = commit(
        &store.db,
        prepare(&store.db, &id).unwrap(),
        "Task: run remaining tests",
    )
    .unwrap();
    assert_eq!(result.summarized_messages, 2);
    assert_eq!(store.history(&id).unwrap().len(), 3);
    store.append(&id, "user", "Continue").unwrap();
    let reopened = SessionStore::new(
        RuntimePaths::new(temp.path().join("data"))
            .unwrap()
            .open_database()
            .unwrap(),
    );
    for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
        let history = reopened.model_history(&id, &format).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0]["content"]
            .as_str()
            .unwrap()
            .contains("run remaining tests"));
        assert_eq!(history[1]["content"], "Continue");
        let fork = reopened.fork(&id).unwrap();
        assert_eq!(reopened.model_history(&fork, &format).unwrap(), history);
    }
}

#[test]
fn conflict_and_empty_summary_leave_history_untouched() {
    let (_temp, store, id) = fixture();
    let prepared = prepare(&store.db, &id).unwrap();
    store.append(&id, "user", "New work").unwrap();
    let before = store.history(&id).unwrap();
    assert!(commit(&store.db, prepared, "Obsolete summary").is_err());
    assert!(commit(&store.db, prepare(&store.db, &id).unwrap(), " ").is_err());
    assert_eq!(store.history(&id).unwrap(), before);
}

#[test]
fn desktop_boundary_is_idempotent_and_keeps_unsaved_multimodal_turn() {
    let (_temp, store, id) = fixture();
    let mut stale = store.history(&id).unwrap();
    commit(&store.db, prepare(&store.db, &id).unwrap(), "Pending tests").unwrap();
    assert!(apply_boundary(&store.db, &id, &mut stale).is_err());
    assert_eq!(stale.len(), 2);
    let mut full = store.history(&id).unwrap();
    let latest = json!({"role":"user","content":[{"type":"text","text":"Continue"},{"type":"image_url","image_url":{"url":"example"}}]});
    full.push(latest.clone());
    apply_boundary(&store.db, &id, &mut full).unwrap();
    assert_eq!(full.len(), 2);
    assert_eq!(full[1], latest);
    apply_boundary(&store.db, &id, &mut full).unwrap();
    assert_eq!(full.len(), 2);
    // Desktop may represent the summary itself as a text content block.
    full[0]["content"] = json!([{"type":"text","text":full[0]["content"]}]);
    apply_boundary(&store.db, &id, &mut full).unwrap();
}

#[test]
fn prepare_dropped_without_commit_does_not_mutate_session() {
    let (_temp, store, id) = fixture();
    let before = store.history(&id).unwrap();
    drop(prepare(&store.db, &id).unwrap());
    assert_eq!(store.history(&id).unwrap(), before);
}

#[test]
fn failed_persistence_rolls_back_summary_and_message_count() {
    let (_temp, store, id) = fixture();
    let before = store.history(&id).unwrap();
    let count = store.resume(&id).unwrap().message_count;
    let prepared = prepare(&store.db, &id).unwrap();
    store.db.with_conn(|conn| conn.execute_batch(
        "CREATE TRIGGER fail_compaction_timestamp BEFORE UPDATE OF updated_at ON conversations BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;"
    )).unwrap().unwrap();
    assert!(commit(&store.db, prepared, "Valid summary").is_err());
    assert_eq!(store.history(&id).unwrap(), before);
    assert_eq!(store.resume(&id).unwrap().message_count, count);
}
