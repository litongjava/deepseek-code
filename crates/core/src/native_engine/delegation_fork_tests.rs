//! Fork input handling: `subagent_type: "fork"` becomes the `fork` agent type
//! and rejects options that a copy of the current session cannot take.
use super::{is_fork, normalize_agent_input, validate_fork};
use serde_json::json;

#[test]
fn subagent_type_fork_survives_normalization() {
    let input = normalize_agent_input(
        &json!({"prompt":"p","description":"d","subagent_type":"fork"}),
        true,
    );
    assert_eq!(input["agent_type"], "fork");
    assert_eq!(input["run_in_background"], true);
    assert!(is_fork(&input));
    assert!(!is_fork(&json!({"agent_type":"general"})));
}

#[test]
fn fork_rejects_foreground_model_schema_and_tools() {
    let foreground = validate_fork(
        &json!({"agent_type":"fork","run_in_background":false}),
        false,
    )
    .unwrap_err();
    assert_eq!(
        foreground.to_string(),
        "A fork runs in the background; omit run_in_background or set it to true"
    );
    let model =
        validate_fork(&json!({"agent_type":"fork","model":"other-model"}), false).unwrap_err();
    assert_eq!(
        model.to_string(),
        "A fork uses this session's model; omit model"
    );
    let typed = validate_fork(&json!({"agent_type":"fork"}), true).unwrap_err();
    assert_eq!(
        typed.to_string(),
        "A fork cannot take an output schema or a tool list"
    );
}

#[test]
fn fork_accepts_background_and_empty_model() {
    validate_fork(&json!({"agent_type":"fork"}), false).unwrap();
    validate_fork(
        &json!({"agent_type":"fork","run_in_background":true,"model":""}),
        false,
    )
    .unwrap();
}
