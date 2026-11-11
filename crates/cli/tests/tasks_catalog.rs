//! Real CLI + local model mock: the four task built-ins are discoverable only
//! through ToolSearch (comma and whitespace selectors) and then usable, while
//! the retired `plan_mode` and `TaskOutput` never appear in a request.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

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

fn tool_parameters<'a>(body: &'a Value, name: &str) -> &'a Value {
    let tool = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["function"]["name"] == name || tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from the request: {:?}", tool_names(body)));
    if tool["function"]["parameters"].is_object() {
        &tool["function"]["parameters"]
    } else {
        &tool["parameters"]
    }
}

fn tool_results(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| serde_json::from_str(content).ok())
        .collect()
}

fn assert_retired_absent(body: &Value) {
    let names = tool_names(body);
    for name in ["plan_mode", "TaskOutput"] {
        assert!(
            !names.iter().any(|tool| tool == name),
            "{name} must not be advertised: {names:?}"
        );
    }
    // Enter/Exit stay advertised; the duplicate entry does not replace them.
    for name in ["EnterPlanMode", "ExitPlanMode"] {
        assert!(
            names.iter().any(|tool| tool == name),
            "{name} must stay advertised: {names:?}"
        );
    }
}

fn assert_task_schemas(body: &Value) {
    assert_retired_absent(body);
    let names = tool_names(body);
    for name in ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"] {
        assert_eq!(
            names.iter().filter(|tool| tool.as_str() == name).count(),
            1,
            "{name} must appear exactly once: {names:?}"
        );
    }
    assert_eq!(
        tool_parameters(body, "TaskGet")["properties"]["block"]["type"],
        "boolean"
    );
    assert_eq!(
        tool_parameters(body, "TaskGet")["properties"]["timeout"]["maximum"],
        600_000
    );
    assert_eq!(
        tool_parameters(body, "TaskUpdate")["properties"]["setBlockedBy"]["type"],
        "array"
    );
    assert_eq!(
        tool_parameters(body, "TaskUpdate")["properties"]["setBlockedBy"]["items"]["type"],
        "string"
    );
    assert_eq!(
        tool_parameters(body, "TaskCreate")["properties"]["subject"]["type"],
        "string"
    );
    assert_eq!(tool_parameters(body, "TaskList")["type"], "object");
}

fn calls_frame(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}})
        })
        .collect();
    vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":tool_calls},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
}

fn text_frames(text: &str) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]
}

fn stream(frames: Vec<Value>) -> axum::response::Response {
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

fn provider_config(base: &str) -> Value {
    json!([{
        "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
        "api_format":"openai","enabled":true,
        "models":[{
            "id":"fixture-model","name":"fixture-model","enabled":true,
            "context_window":128000,"max_tokens":4096,
            "supports_web_search":false,"supports_vision":false
        }]
    }])
}

/// The four task tools are on demand, so only ToolSearch can expose them.
fn write_customization(data: &std::path::Path) {
    let switch = json!({"enabled":false,"on_demand":true});
    let customization = json!({"tool_loading":{
        "TaskCreate":switch,"TaskGet":switch,"TaskUpdate":switch,"TaskList":switch
    }});
    std::fs::write(data.join("customization.json"), customization.to_string()).unwrap();
}

#[derive(Clone)]
struct FixtureState {
    requests: Arc<Mutex<Vec<Value>>>,
    /// The first handler assertion that failed. The driver polls this so a
    /// fixture mistake stops the owned CLI immediately instead of turning
    /// into product retries and a long timeout.
    failures: Arc<Mutex<Vec<String>>>,
}

struct CliFixture {
    root: tempfile::TempDir,
    data: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
    state: FixtureState,
}

async fn start_fixture() -> (CliFixture, FixtureState) {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let fixture = FixtureState {
        requests: Arc::new(Mutex::new(vec![])),
        failures: Arc::new(Mutex::new(vec![])),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    write_customization(&data);
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        CliFixture {
            root,
            data,
            server,
            state: fixture.clone(),
        },
        fixture,
    )
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
        })
        .unwrap_or_else(|| "fixture assertion failed".to_owned())
}

async fn respond(
    State(state): State<FixtureState>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    state.requests.lock().unwrap().push(body.clone());
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scripted_response(&body)));
    match outcome {
        Ok(frames) => stream(frames),
        Err(payload) => {
            let message = panic_message(payload);
            state.failures.lock().unwrap().push(message.clone());
            // Answer once with an explicit error instead of letting the panic
            // surface as a dropped connection and endless product retries.
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":{"message":message}})),
            )
                .into_response()
        }
    }
}

fn scripted_response(body: &Value) -> Vec<Value> {
    assert_eq!(body["stream"], true);
    let results = tool_results(body);
    match results.len() {
        // Nothing loaded yet: the task tools must not be in this request.
        0 => {
            assert_retired_absent(body);
            let names = tool_names(&body);
            for name in ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"] {
                assert!(
                    !names.iter().any(|tool| tool == name),
                    "{name} must start on demand: {names:?}"
                );
            }
            assert!(names.iter().any(|tool| tool == "ToolSearch"));
            calls_frame(&[
                (
                    "search-comma",
                    "ToolSearch",
                    json!({"query":"TaskCreate,TaskUpdate"}),
                ),
                (
                    "search-space",
                    "ToolSearch",
                    json!({"query":"TaskGet TaskList"}),
                ),
            ])
        }
        // Both searches finished: every task tool is now in the request.
        2 => {
            assert_task_schemas(body);
            for (index, expected) in [
                (0, vec!["TaskCreate", "TaskUpdate"]),
                (1, vec!["TaskGet", "TaskList"]),
            ] {
                assert_eq!(results[index]["success"], true, "{}", results[index]);
                let loaded: Vec<&str> = results[index]["loaded_tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|name| name.as_str().unwrap())
                    .collect();
                assert_eq!(loaded, expected, "{}", results[index]);
            }
            calls_frame(&[
                (
                    "create-root",
                    "TaskCreate",
                    json!({"subject":"Root goal","description":"Root goal body"}),
                ),
                (
                    "create-step",
                    "TaskCreate",
                    json!({"subject":"First step","description":"First step body"}),
                ),
            ])
        }
        // Root and step exist: add the dependency and record output.
        4 => {
            assert_task_schemas(body);
            let root = results[2]["id"].as_str().expect("root task");
            let step = results[3]["id"].as_str().expect("step task");
            calls_frame(&[
                (
                    "link-step",
                    "TaskUpdate",
                    json!({"id":step,"setBlockedBy":[root]}),
                ),
                (
                    "root-output",
                    "TaskUpdate",
                    json!({"id":root,"status":"in_progress","output":"root-output-marker"}),
                ),
            ])
        }
        // Read both rows back through the normal task tools.
        6 => {
            assert_task_schemas(body);
            let root = results[2]["id"].as_str().expect("root task");
            calls_frame(&[
                ("list", "TaskList", json!({})),
                ("get", "TaskGet", json!({"id":root})),
            ])
        }
        _ => {
            assert_retired_absent(body);
            text_frames("task catalog fixture complete")
        }
    }
}

async fn run_cli(fixture: &CliFixture, session: &str) -> Result<std::process::Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&fixture.data)
        .arg("--path")
        .arg(fixture.root.path())
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            session,
            "exercise task tools",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting dcl");
    let started = std::time::Instant::now();
    loop {
        if let Some(message) = fixture.state.failures.lock().unwrap().first().cloned() {
            let _ = child.start_kill();
            let output = child
                .wait_with_output()
                .await
                .map_err(|error| error.to_string())?;
            let requests = fixture.state.requests.lock().unwrap();
            fixture.server.abort();
            return Err(format!(
                "fixture assertion failed after {} request(s): {message}\nCLI stderr:\n{}",
                requests.len(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .await
                .map_err(|error| error.to_string())?;
            fixture.server.abort();
            return Ok(output);
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.start_kill();
            let _ = child.wait_with_output().await;
            fixture.server.abort();
            return Err("CLI fixture timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn task_tools_load_through_comma_and_space_search_then_run_for_real() {
    let (fixture, state) = start_fixture().await;
    let session = uuid::Uuid::new_v4().to_string();
    let output = run_cli(&fixture, &session)
        .await
        .unwrap_or_else(|message| panic!("{message}"));
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
    let result: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(result.len(), 1, "{events:?}");
    assert_eq!(result[0]["is_error"], false, "{}", result[0]);

    let requests = state.requests.lock().unwrap();
    assert_eq!(requests.len(), 5, "one request per scripted turn");

    // Read the persisted graph through the same session task store.
    let rows = deepseek_code_core::tools::tasks::execute_for_session(
        "TaskList",
        json!({}),
        &fixture.data,
        &session,
    )
    .unwrap();
    let rows = rows["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let root = rows
        .iter()
        .find(|task| task["subject"] == "Root goal")
        .expect("root task");
    let step = rows
        .iter()
        .find(|task| task["subject"] == "First step")
        .expect("step task");
    assert_eq!(root["status"], "in_progress", "{root}");
    assert_eq!(root["output"], "root-output-marker", "{root}");
    assert_eq!(step["blockedBy"], json!([root["id"]]), "{step}");
    assert_eq!(root["blocks"], json!([step["id"]]), "{root}");
    let read_back = deepseek_code_core::tools::tasks::execute_for_session(
        "TaskGet",
        json!({"id":root["id"]}),
        &fixture.data,
        &session,
    )
    .unwrap();
    assert_eq!(read_back["output"], "root-output-marker");
    assert!(fixture
        .data
        .join("tasks")
        .join(format!("{session}.sqlite3"))
        .is_file());
}
