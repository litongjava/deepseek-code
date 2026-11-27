use super::*;
use crate::{
    db::{conversation_repo, session_store::Projects},
    native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider},
};
use axum::{extract::State, routing::post, Json, Router};
use std::sync::Mutex;

#[tokio::test]
async fn desktop_child_preserves_mode_and_reads_parent_private_files() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    for mode in ["chat", "work", "study"] {
        let temp = tempfile::tempdir().unwrap();
        let context = ServiceContext::open(
            RuntimePaths::new(temp.path().join("data")).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let parent = uuid::Uuid::new_v4().to_string();
        context
            .db
            .create(
                &parent,
                crate::db::session_store::CreateOptions {
                    model: Some("fixture".into()),
                    provider: Some("fixture".into()),
                    workspace_mode: Some(mode.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let private_root = context.paths.data_dir.join("data/workspaces");
        let cwd = context
            .db
            .engine_workspace(&private_root, &parent, None)
            .unwrap()
            .0;
        std::fs::write(cwd.join("parent-notes.txt"), "PARENT_PRIVATE_CONTENT").unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app=Router::new().route("/v1/chat/completions",post(|State(requests):State<Arc<Mutex<Vec<Value>>>>,Json(body):Json<Value>| async move {
            let done=body["messages"].as_array().unwrap().iter().any(|message|message["role"]=="tool");
            requests.lock().unwrap().push(body);
            let delta=if done {json!({"content":"Child read parent notes"})} else {json!({"tool_calls":[{"index":0,"id":"read-parent","type":"function","function":{"name":"Read","arguments":json!({"file_path":"parent-notes.txt"}).to_string()}}]})};
            ([("content-type","text/event-stream")],format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":if done {"stop"}else{"tool_calls"}}]})))
        })).with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider=ResolvedProvider {
            provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":endpoint,"api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (events, _receiver) = tokio::sync::mpsc::channel(128);
        let executor = ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            events,
            cwd.to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_conv_id(parent.clone())
        .with_search_config_path(context.paths.layout().config_file("settings.json"));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            executor.execute_delegated(
                &json!({"prompt":"Read parent-notes.txt","agent_type":"general"}),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["status"], "completed");
        let child = result["session_id"].as_str().unwrap();
        let row = context
            .db
            .conversation_metadata(child)
            .unwrap()
            .unwrap();
        assert_eq!(row.workspace_mode, mode);
        assert!(row.project_id.is_none());
        let resolved = context.db.engine_workspace(&private_root, child, None)
            .unwrap()
            .0;
        assert_eq!(resolved, cwd);
        assert!(requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("PARENT_PRIVATE_CONTENT"));
        assert!(context
            .db
            .projects()
            .unwrap()
            .is_empty());
        assert!(!private_root.join(child).exists());
        // A later parent archive must invalidate the child's inherited authority.
        context.db.open_session(&parent).unwrap().set_archived(true).unwrap();
        assert!(context.db.engine_workspace(&private_root, child, None)
            .is_err());
        server.abort();
    }
}

#[test]
fn private_bindings_reject_cycles_mode_changes_and_runtime_directory_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let context = ServiceContext::open(
        RuntimePaths::new(temp.path().join("data")).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    context.db.create(&parent, Default::default()).unwrap();
    let root = context.paths.data_dir.join("data/workspaces");
    let cwd = context.db.engine_workspace(&root, &parent, None).unwrap().0;
    let store = SessionStore::new(context.db.clone());
    assert!(store
        .derive_owned_child(&parent, &root, temp.path())
        .is_err());
    let child = store.derive_owned_child(&parent, &root, &cwd).unwrap();
    context
        .db
        .fixture_directory(|conn| conversation_repo::set_workspace_mode(conn, &parent, "work"))
        .unwrap()
        .unwrap();
    assert!(context.db.engine_workspace(&root, &child, None)
        .is_err());
    context.db.fixture_directory(|conn| -> Result<()> {
        conversation_repo::set_workspace_mode(conn,&parent,"chat")?;
        conn.execute("INSERT INTO conversation_private_workspace_bindings(child_id,parent_id,parent_incarnation) SELECT ?1,id,incarnation FROM conversations WHERE id=?2",rusqlite::params![parent,child])?;Ok(())
    }).unwrap().unwrap();
    assert!(context.db.engine_workspace(&root, &child, None)
        .is_err());
}

#[test]
fn existing_directory_retains_private_files_after_owner_and_child_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    {
        let db = paths.open_database().unwrap();
        db.create(&parent, Default::default()).unwrap();
    }
    let db = paths.open_database().unwrap();
    let root = paths.data_dir.join("data/workspaces");
    let cwd = db.engine_workspace(&root, &parent, None).unwrap().0;
    std::fs::write(cwd.join("keep.txt"), "keep").unwrap();
    let child = SessionStore::new(db.clone())
        .derive_owned_child(&parent, &root, &cwd)
        .unwrap();
    db.open_session(&parent).unwrap().delete().unwrap();
    assert!(db.engine_workspace(&root, &child, None).is_err());
    db.open_session(&child).unwrap().delete().unwrap();
    let rows = db.fixture_directory(|conn| conn.query_row(
        "SELECT COUNT(*) FROM conversation_private_workspace_bindings WHERE child_id=?1",
        [&child], |r| r.get::<_, i64>(0)
    )).unwrap().unwrap();
    assert_eq!(rows, 0);
    assert_eq!(
        std::fs::read_to_string(cwd.join("keep.txt")).unwrap(),
        "keep"
    );
}
