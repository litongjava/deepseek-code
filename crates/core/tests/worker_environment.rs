//! A child process started by the worker is a separate session.
//!
//! The worker keeps its job and control directory in host-private environment
//! variables. A shell command, or a nested CLI or test runner launched from one,
//! must not inherit them: otherwise the child claims the parent's control
//! directory and is rejected by its own ownership check. These tests set the
//! variables in the test process itself, which is why this target holds a
//! single test.
//!
//! Set `DCL_ACCEPTANCE_COMMAND` to also run a command that reads the variables
//! itself (for example the `native_compaction` test binary) through the same
//! shell tool; it must succeed without seeing them.
use deepseek_code_core::{
    config::OrchestrationConfig,
    services::background_sessions::{
        CONTROL_ENV, WORKER_AUTO_ROUTE_ENV, WORKER_ENV, WORKER_MODEL_ENV,
    },
    tools::execute_tool,
};
use serde_json::json;

#[test]
fn a_shell_command_never_inherits_the_worker_control_variables() {
    OrchestrationConfig::init_global().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path().to_str().unwrap();
    std::env::set_var(CONTROL_ENV, "fixture-control");
    std::env::set_var(WORKER_ENV, "fixture-job");
    std::env::set_var(WORKER_MODEL_ENV, r#"{"id":"parent-model"}"#);
    std::env::set_var(WORKER_AUTO_ROUTE_ENV, "true");
    std::env::set_var("DCL_KEEP_ENV", "kept-value");
    let command = if cfg!(windows) {
        r#"echo "[$env:DEEPSEEK_CODE_BACKGROUND_CONTROL][$env:DEEPSEEK_CODE_BACKGROUND_JOB][$env:DEEPSEEK_CODE_WORKER_MODEL][$env:DEEPSEEK_CODE_WORKER_AUTO_ROUTE][$env:DCL_KEEP_ENV]""#
    } else {
        r#"printf '[%s][%s][%s][%s][%s]' "$DEEPSEEK_CODE_BACKGROUND_CONTROL" "$DEEPSEEK_CODE_BACKGROUND_JOB" "$DEEPSEEK_CODE_WORKER_MODEL" "$DEEPSEEK_CODE_WORKER_AUTO_ROUTE" "$DCL_KEEP_ENV""#
    };
    let output = execute_tool(
        "Bash",
        json!({
            "command": command,
            "shell": if cfg!(windows) { "powershell" } else { "sh" },
        }),
        cwd,
    )
    .unwrap();
    assert_eq!(output["exit_code"], 0, "{output}");
    let stdout = output["stdout"].as_str().unwrap().trim().to_owned();
    // Host-private variables, including the parent's model snapshot, arrive
    // empty; an ordinary user variable is still inherited.
    assert_eq!(stdout, "[][][][][kept-value]", "{output}");

    if let Some(command) = std::env::var_os("DCL_ACCEPTANCE_COMMAND") {
        let output = execute_tool(
            "Bash",
            json!({
                "command": command.to_string_lossy(),
                "shell": if cfg!(windows) { "powershell" } else { "sh" },
            }),
            cwd,
        )
        .unwrap();
        std::env::remove_var(CONTROL_ENV);
        std::env::remove_var(WORKER_ENV);
        std::env::remove_var(WORKER_MODEL_ENV);
        std::env::remove_var(WORKER_AUTO_ROUTE_ENV);
        std::env::remove_var("DCL_KEEP_ENV");
        let stdout = output["stdout"].as_str().unwrap_or_default();
        let stderr = output["stderr"].as_str().unwrap_or_default();
        assert_eq!(
            output["exit_code"], 0,
            "the acceptance command must not see the worker control environment:\n{stdout}\n{stderr}"
        );
        return;
    }
    std::env::remove_var(CONTROL_ENV);
    std::env::remove_var(WORKER_ENV);
    std::env::remove_var(WORKER_MODEL_ENV);
    std::env::remove_var(WORKER_AUTO_ROUTE_ENV);
    std::env::remove_var("DCL_KEEP_ENV");
}
