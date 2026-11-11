//! A real CLI turn against a local model fixture: the model backgrounds a
//! shell, then retrieves its output with TaskGet. The request bodies are
//! inspected so the advertised catalog and the tool result are both verified.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::process::Command;

#[derive(Clone, Default)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    bash_result: Arc<Mutex<Option<Value>>>,
    taskget_result: Arc<Mutex<Option<Value>>>,
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool["function"]["name"]
                .as_str()
                .or_else(|| tool["name"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn last_tool_result(body: &Value) -> Option<Value> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .and_then(|content| serde_json::from_str(content).ok())
}

fn tool_call_frame(id: &str, name: &str, arguments: &Value) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
}

fn text_frames(text: &str) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]
}

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let index = {
        let mut requests = fixture.requests.lock().unwrap();
        requests.push(body.clone());
        requests.len() - 1
    };
    assert_eq!(body["stream"], true);
    let tools = tool_names(&body);
    assert!(
        tools.contains(&"TaskGet".to_owned()),
        "TaskGet missing: {tools:?}"
    );
    assert!(
        !tools.contains(&"TaskOutput".to_owned()),
        "TaskOutput is still advertised: {tools:?}"
    );
    let frames = match index {
        0 => tool_call_frame(
            "call-bash",
            "Bash",
            &json!({
                "command": "node -e \"setTimeout(()=>{console.log('bg-line');process.exit(0)},200)\"",
                "description": "Print a delayed background line",
                "run_in_background": true
            }),
        ),
        1 => {
            let result = last_tool_result(&body)
                .unwrap_or_else(|| panic!("Bash result missing in request {body}"));
            *fixture.bash_result.lock().unwrap() = Some(result.clone());
            let task_id = result["task_id"]
                .as_str()
                .unwrap_or_else(|| panic!("Bash task_id missing: {result}"));
            tool_call_frame(
                "call-taskget",
                "TaskGet",
                &json!({"task_id":task_id,"block":true,"timeout":5000}),
            )
        }
        2 => {
            let result = last_tool_result(&body)
                .unwrap_or_else(|| panic!("TaskGet result missing in request {body}"));
            *fixture.taskget_result.lock().unwrap() = Some(result.clone());
            text_frames("background output observed")
        }
        _ => text_frames("background output observed"),
    };
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn cli_background_bash_then_task_get_returns_output_and_exposes_no_task_output() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let fixture = Fixture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(
        data.join("providers.json"),
        json!([{
            "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
            "api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                "context_window":128000,"max_tokens":4096,
                "supports_web_search":false,"supports_vision":false}]
        }])
        .to_string(),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(
        std::env::var_os("CLAUDE_SDK_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
    );
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&data)
        .arg("--path")
        .arg(root.path())
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            &id,
            "fixture task",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "{events:?}");
    assert_eq!(results[0]["is_error"], false, "{}", results[0]);
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);

    let started = fixture
        .bash_result
        .lock()
        .unwrap()
        .clone()
        .expect("Bash result");
    assert_eq!(started["status"], "running");
    assert!(started["task_id"].as_str().is_some());
    assert!(started["stdout_file"].as_str().is_some());
    assert!(started["stderr_file"].as_str().is_some());

    let retrieved = fixture
        .taskget_result
        .lock()
        .unwrap()
        .clone()
        .expect("TaskGet result");
    assert_eq!(retrieved["status"], "completed", "{retrieved}");
    assert_eq!(retrieved["retrieval_status"], "success", "{retrieved}");
    assert!(
        retrieved["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("bg-line"),
        "TaskGet did not return the background output: {retrieved}"
    );
}
