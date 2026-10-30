use deepseek_code_core::{
    db::message_repo,
    native_engine::provider_manager::ApiFormat,
    runtime::{RuntimePaths, SessionStore},
};

#[test]
fn approval_notes_follow_their_result_as_user_text_after_reopen_and_fork() {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(temp.path(), "model", "provider").unwrap();
    store.append(&id, "user", "Run the log").unwrap();
    let message = store.append(&id, "assistant", "Done").unwrap();
    let call = format!("{message}:call-1");
    store
        .db
        .with_conn(|conn| -> anyhow::Result<()> {
            message_repo::insert_tool_call(conn, &call, &message, "Bash", Some(r#"{"command":"git log -3"}"#), Some("abc init"), false, 0)?;
            message_repo::set_tool_call_note(conn, &call, "use -5 instead")?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    let reopened = SessionStore::new(paths.open_database().unwrap());
    let fork = reopened.fork(&id).unwrap();
    for session in [&id, &fork] {
        let anthropic = reopened.model_history(session, &ApiFormat::Anthropic).unwrap();
        let results = anthropic[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["content"], "abc init", "the result itself is unchanged");
        assert_eq!(results[1], serde_json::json!({"type": "text", "text": "use -5 instead"}));

        let openai = reopened.model_history(session, &ApiFormat::OpenAI).unwrap();
        assert_eq!(openai[2]["role"], "tool");
        assert_eq!(openai[3], serde_json::json!({"role": "user", "content": "use -5 instead"}));
    }
}
