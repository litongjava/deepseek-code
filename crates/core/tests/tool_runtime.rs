use deepseek_code_core::tools::{execute_tool, execute_tool_async};
use serde_json::json;

fn shell_input() -> serde_json::Value {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    json!({"command":"echo runtime-ok", "shell": if cfg!(windows) { "powershell" } else { "sh" }})
}

#[test]
fn sync_bash_without_runtime() {
    let cwd = tempfile::tempdir().unwrap();
    let output = execute_tool("Bash", shell_input(), cwd.path().to_str().unwrap()).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert!(output["stdout"].as_str().unwrap().contains("runtime-ok"));
}

async fn worker_tools() {
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path().to_str().unwrap();
    let input = json!({"action_type":"Wait", "duration_ms":1});
    let output = execute_tool_async("computer_use", input.clone(), cwd)
        .await
        .unwrap();
    assert_eq!(output["success"], true);
    assert_eq!(
        execute_tool("computer_use", input, cwd).unwrap()["success"],
        true
    );
    let output = execute_tool("Bash", shell_input(), cwd).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert!(output["stdout"].as_str().unwrap().contains("runtime-ok"));
    let output = execute_tool_async("Bash", shell_input(), cwd)
        .await
        .unwrap();
    assert_eq!(output["exit_code"], 0);
    assert!(execute_tool_async("computer_use", json!({}), cwd)
        .await
        .is_err());
    assert_eq!(
        execute_tool_async("computer_use", json!({"action_type":"invalid"}), cwd)
            .await
            .unwrap()["is_error"],
        true
    );
}

#[tokio::test]
async fn tools_on_current_thread_runtime() {
    worker_tools().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_on_worker_runtime() {
    tokio::spawn(worker_tools()).await.unwrap();
}
