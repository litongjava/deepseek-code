use super::*;

#[tokio::test]
async fn builtin_errors_are_failures_in_serial_and_parallel_paths() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file.txt"), "original").unwrap();
    let provider=ResolvedProvider {provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap()};
    let (events, _receiver) = mpsc::channel(64);
    let cwd = temp.path().to_string_lossy().into_owned();
    let shell = if cfg!(windows) { "powershell" } else { "bash" };
    for (name, input, failure) in [
        ("Bash", json!({"command":"exit 7","shell":shell}), true),
        ("Bash", json!({"command":"echo ok","shell":shell}), false),
        (
            "Edit",
            json!({"file_path":"file.txt","old_string":"absent","new_string":"changed"}),
            true,
        ),
        ("Read", json!({"file_path":"file.txt"}), false),
    ] {
        let result =
            execute_builtin_tool(name, input.clone(), &cwd, &provider, &events, None, false).await;
        assert_eq!(result.is_err(), failure, "{name}: {result:?}");
        let mut executor = ToolLoopExecutor::new(
            provider.clone(),
            vec![],
            None,
            128,
            events.clone(),
            cwd.clone(),
            ReasoningMode::Standard,
        );
        let call = PendingToolCall { id: "fixture-call".into(), name: name.into(), input, concurrent: false };
        let result = executor.execute_tool_batch(std::slice::from_ref(&call)).await.remove(0);
        assert_eq!(result.2, failure, "{name}: {result:?}");
        if name == "Bash" && failure {
            assert!(result.1.contains("exit_code"));
        }
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "original"
    );
    for timeout in [json!(0), json!(-1), json!("120"), json!(600_001)] {
        assert!(crate::tools::execute_tool_async(
            "Bash",
            json!({"command":"echo must-not-run","timeout":timeout}),
            &cwd
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn ui_file_change_stays_in_events_but_not_model_results() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let provider=ResolvedProvider {provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap()};
    let (events, _receiver) = mpsc::channel(64);
    let mut executor = ToolLoopExecutor::new(
        provider.clone(),
        vec![],
        None,
        128,
        events.clone(),
        temp.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    );
    let (_, output, failed) = executor
        .execute_tool_call("Write", &json!({"file_path":"new.txt","content":"hello"}), "write")
        .await;
    let result = (failed, output);
    assert!(!result.0, "{}", result.1);
    // The loop sends this full output in ToolUseDone; only the model copy is trimmed.
    assert!(serde_json::from_str::<Value>(&result.1)
        .unwrap()
        .get("file_change")
        .is_some());
    assert!(!executor
        .retained_tool_output("Write", &result.1)
        .contains("file_change"));
    assert!(!compact_tool_output("Write", &result.1).contains("file_change"));
    assert_eq!(
        model_tool_output("mcp__fixture__Write", &result.1),
        result.1
    );
}
