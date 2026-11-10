use super::*;

/// Some OpenAI-compatible gateways send `function.name` in the first tool-call
/// delta and an empty string in every argument delta afterwards. The name must
/// survive those empty deltas or the call is dispatched as an unknown tool.
#[tokio::test]
async fn empty_name_deltas_keep_the_accumulated_tool_name() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let captured = captured.clone();
            async move {
                let first = captured.fetch_add(1, Ordering::SeqCst) == 0;
                let body = if first {
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"Read","arguments":""}}]},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":null,"type":null,"function":{"name":"","arguments":"{\"file_path\":\"fixture.txt\"}"}}]},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
                    )
                } else {
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})
                    )
                };
                ([("content-type", "text/event-stream")], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixture.txt"), "actual tool fixture").unwrap();
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"name-delta-test","name":"name-delta-test","base_url":format!("http://{addr}"),"api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, mut rx) = mpsc::channel(128);
    let mut executor = ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"Read fixture.txt then answer"})],
        None,
        128,
        tx,
        dir.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_max_turns(Some(4))
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), executor.execute_openai_loop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(result.0, "done");

    let mut started_name = None;
    let mut completed = None;
    while let Ok(event) = rx.try_recv() {
        match event {
            EngineEvent::ToolUseStart { tool_name, .. } => started_name = Some(tool_name),
            EngineEvent::ToolUseDone {
                output, is_error, ..
            } => completed = Some((output, is_error)),
            EngineEvent::Error(message) => panic!("unexpected engine error: {message}"),
            _ => {}
        }
    }
    assert_eq!(started_name.as_deref(), Some("Read"));
    let (output, is_error) = completed.expect("the tool must run");
    assert!(!is_error, "{output}");
    assert!(output.contains("actual tool fixture"), "{output}");
    server.abort();
}
