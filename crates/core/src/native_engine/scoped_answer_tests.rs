use super::*;
use crate::permissions::PermissionMode;
use crate::native_engine::permission_response as responses;

fn fixture() -> (tempfile::TempDir, QueryEngine) {
    let temp = tempfile::tempdir().unwrap();
    let context = crate::services::session_messages::context(temp.path()).unwrap();
    let engine = QueryEngine::new(
        Arc::new(Mutex::new(ProviderManager::in_memory(
            vec![],
            temp.path().join("config.json"),
        ))),
        context.db,
        temp.path().to_path_buf(),
        crate::runtime::default_permissions(),
        Arc::new(FileCache::new()),
    );
    (temp, engine)
}

#[tokio::test]
async fn scoped_question_rejects_wrong_owner_tool_endpoint_duplicate_and_cancelled() {
    let (_temp, engine) = fixture();
    let id = uuid::Uuid::new_v4().to_string();
    let (sender, receiver) = oneshot::channel();
    engine
        .answer_waiters
        .lock()
        .await
        .insert(id.clone(), sender);
    responses::register_owner(
        &id,
        "child",
        "tool",
        "question",
        serde_json::json!({"question":"Proceed?"}),
        None,
        None,
        None,
    );
    for (session, tool, permission) in [
        ("parent", "tool", false),
        ("child", "other", false),
        ("child", "tool", true),
    ] {
        assert!(engine
            .resume_scoped_answer(session, &id, tool, "yes".into(), permission)
            .await
            .is_err());
        assert!(engine.answer_waiters.lock().await.contains_key(&id));
    }
    assert!(engine.pending_questions("parent").await.is_empty());
    assert_eq!(engine.pending_questions("child").await.len(), 1);
    assert_eq!(
        engine
            .resume_scoped_answer("child", &id, "tool", "yes".into(), false)
            .await
            .unwrap()["status"],
        "submitted"
    );
    assert_eq!(receiver.await.unwrap(), "yes");
    assert!(engine
        .resume_scoped_answer("child", &id, "tool", "yes".into(), false)
        .await
        .is_err());
    responses::remove_owner(&id);
    let (sender, receiver) = oneshot::channel();
    engine
        .answer_waiters
        .lock()
        .await
        .insert(id.clone(), sender);
    responses::register_owner(
        &id,
        "child",
        "tool",
        "question",
        Value::Null,
        None,
        None,
        None,
    );
    drop(receiver);
    assert!(engine.pending_questions("child").await.is_empty());
    assert!(engine
        .resume_scoped_answer("child", &id, "tool", "yes".into(), false)
        .await
        .is_err());
    responses::remove_owner(&id);
}

#[tokio::test]
async fn scoped_plan_validation_preserves_waiter_and_acknowledges_only_committed_outcome() {
    let (temp, engine) = fixture();
    let id = uuid::Uuid::new_v4().to_string();
    let request = crate::services::plan_approval::ApprovalRequest::new(
        "child",
        &id,
        "tool",
        "# Plan",
        &temp.path().join("plan.md"),
        false,
    );
    let manager = crate::runtime::default_permissions();
    manager.set_mode(PermissionMode::PlanMode);
    crate::services::plans::save(temp.path(), "child", &request.plan).unwrap();
    let (sender, receiver) = oneshot::channel();
    engine
        .answer_waiters
        .lock()
        .await
        .insert(id.clone(), sender);
    responses::register_owner(
        &id,
        "child",
        "tool",
        "plan",
        Value::Null,
        Some(request.clone()),
        Some(manager),
        Some(temp.path().to_path_buf()),
    );
    let mut answer = serde_json::json!({"type":"plan_approval_response","version":1,"sessionId":"child","requestId":id,"toolUseId":"tool","planVersion":"stale","action":"approve","mode":"ask","context":"keep"});
    assert!(engine
        .resume_scoped_answer("child", &id, "tool", answer.to_string(), false)
        .await
        .is_err());
    assert!(engine.answer_waiters.lock().await.contains_key(&id));
    answer["planVersion"] = serde_json::json!(request.plan_version);
    let answer = answer.to_string();
    let pending = engine.resume_scoped_answer("child", &id, "tool", answer, false);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut pending)
            .await
            .is_err()
    );
    assert!(receiver.await.is_ok());
    responses::finish_answer(&id, Ok(serde_json::json!({"approved":true,"mode":"ask"})));
    let result = pending.await.unwrap();
    assert_eq!(result["status"], "applied");
    assert_eq!(result["result"]["mode"], "ask");
    responses::remove_owner(&id);
}
