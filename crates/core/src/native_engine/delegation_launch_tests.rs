use super::*;
use crate::native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider};

fn executor(api_key: &str) -> ToolLoopExecutor {
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":api_key,"api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (events, _receiver) = tokio::sync::mpsc::channel(8);
    ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        events,
        ".".into(),
        ReasoningMode::Standard,
    )
}

#[test]
fn delegated_child_pins_its_provider_and_does_not_forward_auto_route() {
    let executor = executor("memory-only-secret");
    let (arguments, environment) = executor
        .child_cli_launch(std::path::Path::new("data"))
        .unwrap();
    // The child resolves its provider and model from these arguments.
    assert!(arguments
        .windows(2)
        .any(|pair| pair[0] == "--provider" && pair[1] == "fixture"));
    assert!(arguments
        .windows(2)
        .any(|pair| pair[0] == "--model" && pair[1] == "fixture"));
    let names: Vec<&str> = environment.iter().map(|(key, _)| key.as_str()).collect();
    // The explicit snapshot and credential are forwarded...
    assert!(names.contains(&crate::services::background_sessions::WORKER_MODEL_ENV));
    assert!(names.contains(&"DEEPSEEK_CODE_CHILD_API_KEY"));
    // ...but the parent's routing decision is not: the child stays on the
    // provider this delegation resolved for the request.
    assert!(!names.contains(&crate::services::background_sessions::WORKER_AUTO_ROUTE_ENV));
}
