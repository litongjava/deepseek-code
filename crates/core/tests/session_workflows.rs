use claude_core::{
    db::{conversation_repo, message_repo},
    native_engine::provider_manager::ApiFormat,
    runtime::{RuntimePaths, SessionStore},
};

#[test]
fn continue_is_scoped_to_directory_and_skips_unavailable_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    assert!(store.latest(&a).unwrap().is_none());
    let first = store.create(&a, "model", "provider").unwrap();
    let older = store.create(&a, "model", "provider").unwrap();
    let archived = store.create(&a, "model", "provider").unwrap();
    let chat = store.create(&a, "model", "provider").unwrap();
    let other = store.create(&b, "model", "provider").unwrap();
    for id in [&first, &older, &archived, &chat, &other] {
        store.append(id, "user", "hello").unwrap();
    }
    // A newer session with no messages is not one to continue.
    let empty = store.create(&a, "model", "provider").unwrap();
    store
        .db
        .with_conn(|conn| {
            conversation_repo::update_conversation_timestamp(conn, &empty, "2999-01-01T00:00:00Z")
                .unwrap();
            conversation_repo::update_conversation_timestamp(conn, &older, "2000-01-01T00:00:00Z")
                .unwrap();
            conversation_repo::archive_conversation(conn, &archived, true).unwrap();
            conversation_repo::set_workspace_mode(conn, &chat, "chat").unwrap();
        })
        .unwrap();
    assert_eq!(store.latest(&a.join(".")).unwrap().unwrap().id, first);
    assert_eq!(store.latest(&b).unwrap().unwrap().id, other);
    assert_eq!(
        std::fs::canonicalize(store.workspace(&first, &paths.data_dir).unwrap()).unwrap(),
        std::fs::canonicalize(&a).unwrap()
    );
    store.db.with_conn(|conn| {
        conn.execute("UPDATE project_folders SET trusted = 0 WHERE project_id = (SELECT project_id FROM conversations WHERE id = ?1)", [&first]).unwrap();
    }).unwrap();
    assert!(store.latest(&a).unwrap().is_none());
    assert!(store.fork(&first).is_err());
    assert!(store.latest(&a.join("missing")).is_err());
}

#[test]
fn fork_preserves_tool_history_and_leaves_source_unchanged_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let (source, fork);
    {
        let store = SessionStore::new(paths.open_database().unwrap());
        source = store.create(temp.path(), "model", "provider").unwrap();
        let restricted = claude_core::services::agents::Definition {
            id: "read-only".into(),
            description: "Read-only investigation".into(),
            system_prompt: "Inspect files without making changes".into(),
            tools: Some(vec!["Read".into()]),
            legacy: None,
        };
        claude_core::services::agents::bind(&store.db, &source, &restricted).unwrap();
        store.append(&source, "user", "Read file").unwrap();
        let answer = store.append(&source, "assistant", "File contents").unwrap();
        store
            .db
            .with_conn(|conn| {
                message_repo::insert_tool_call(
                    conn,
                    "tool",
                    &answer,
                    "Read",
                    Some("{\"path\":\"file\"}"),
                    Some("contents"),
                    false,
                    0,
                )
                .unwrap();
            })
            .unwrap();
        fork = store.fork(&source).unwrap();
        assert_ne!(source, fork);
        store.append(&fork, "user", "Next step").unwrap();
    }
    let store = SessionStore::new(paths.open_database().unwrap());
    let source_agent = claude_core::services::agents::for_session(&store.db, &source)
        .unwrap()
        .unwrap();
    let fork_agent = claude_core::services::agents::for_session(&store.db, &fork)
        .unwrap()
        .unwrap();
    assert_eq!(source_agent, fork_agent);
    assert_eq!(fork_agent.tools, Some(vec!["Read".into()]));
    assert_eq!(
        fork_agent.system_prompt,
        "Inspect files without making changes"
    );
    assert_eq!(store.history(&source).unwrap().len(), 2);
    assert_eq!(store.history(&fork).unwrap().len(), 3);
    let metadata = store.resume(&fork).unwrap();
    assert_eq!(metadata.message_count, 3);
    assert_eq!(metadata.model.as_deref(), Some("model"));
    assert_eq!(metadata.provider.as_deref(), Some("provider"));
    for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
        let history = store.model_history(&fork, &format).unwrap();
        assert_eq!(history.len(), 5);
        match format {
            ApiFormat::OpenAI => assert_eq!(
                history[1]["tool_calls"][0]["id"],
                history[2]["tool_call_id"]
            ),
            ApiFormat::Anthropic => assert_eq!(
                history[1]["content"][0]["id"],
                history[2]["content"][0]["tool_use_id"]
            ),
        }
    }
    assert!(store.fork("missing").is_err());
}
