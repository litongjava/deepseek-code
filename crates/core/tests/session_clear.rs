use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::agents,
};

#[test]
fn clear_preserves_source_and_project_settings_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let source = store
        .create(temp.path(), "chosen-model", "chosen-provider")
        .unwrap();
    let definition = agents::builtins()
        .into_iter()
        .find(|agent| agent.id == "plan")
        .unwrap();
    agents::bind(&store.db, &source, &definition).unwrap();
    store
        .append(&source, "user", "Keep this conversation available")
        .unwrap();
    let original = store.resume(&source).unwrap();
    let next = store.clear(&source).unwrap();
    assert_ne!(next, source);
    drop(store);

    let store = SessionStore::new(paths.open_database().unwrap());
    let fresh = store.resume(&next).unwrap();
    assert_eq!(fresh.project_id, original.project_id);
    assert_eq!(fresh.workspace_path, original.workspace_path);
    assert_eq!(fresh.model, original.model);
    assert_eq!(fresh.provider, original.provider);
    assert_eq!(fresh.message_count, 0);
    assert!(store.history(&next).unwrap().is_empty());
    assert_eq!(
        store.history(&source).unwrap()[0]["content"],
        "Keep this conversation available"
    );
    assert_eq!(
        agents::for_session(&store.db, &next).unwrap(),
        Some(definition)
    );
    store.append(&next, "user", "New task").unwrap();
    assert_eq!(store.history(&source).unwrap().len(), 1);
    assert_eq!(
        store.resume(&source).unwrap().updated_at,
        original.updated_at
    );
}

#[test]
fn clear_rejects_missing_and_archived_sessions_without_creating_rows() {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let source = store.create(temp.path(), "model", "provider").unwrap();
    store
        .db
        .with_conn(|conn| {
            deepseek_code_core::db::conversation_repo::archive_conversation(conn, &source, true).unwrap();
        })
        .unwrap();
    assert!(store.clear("missing").is_err());
    assert!(store.clear(&source).is_err());
    let count: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
        })
        .unwrap()
        .unwrap();
    assert_eq!(count, 1);
}
