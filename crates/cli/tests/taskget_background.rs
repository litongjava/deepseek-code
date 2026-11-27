//! A real CLI turn against a local model fixture: the model backgrounds a
//! shell, then retrieves its output with TaskGet. The request bodies are
//! inspected so the advertised catalog and the tool result are both verified.
#[path = "../../core/tests/support/session_sql.rs"]
mod fixture_sql;
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use fixture_sql::FixtureSql;
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::process::Command;

#[path = "support/tool_loading.rs"]
mod tool_loading;

#[derive(Clone, Default)]
struct Fixture {
    long_output: bool,
    base64: bool,
    foreground: bool,
    raw_size: Option<usize>,
    requests: Arc<Mutex<Vec<Value>>>,
    bash_result: Arc<Mutex<Option<Value>>>,
    taskget_result: Arc<Mutex<Option<Value>>>,
    /// Every bounded page the paged fixture returned, in order.
    pages: Arc<Mutex<Vec<Value>>>,
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

fn last_tool_content(body: &Value) -> Option<&str> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
}

fn last_tool_result(body: &Value) -> Option<Value> {
    last_tool_content(body).and_then(|content| serde_json::from_str(content).ok())
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

const REVIEW_PAGE_LINE: &str =
    "not test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s";
const REVIEW_PAGE_COMMAND: &str = r#"node -e "process.stdout.write('not test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n'+(String.fromCharCode(34).repeat(59)+'\n').repeat(250))""#;

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let index = {
        let mut requests = fixture.requests.lock().unwrap();
        requests.push(body.clone());
        requests.len() - 1
    };
    if (fixture.foreground || fixture.base64) && body["stream"] != true {
        return Json(json!({"id":"fixture-compact","object":"chat.completion","created":0,"model":"fixture-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"Preserved the shell output."},"finish_reason":"stop"}]})).into_response();
    }
    assert_eq!(body["stream"], true);
    let tools = tool_names(&body);
    assert!(
        !tools.contains(&"TaskOutput".to_owned()),
        "TaskOutput is still advertised: {tools:?}"
    );
    // TaskGet is resident by default and available on every request.
    assert!(
        tools.contains(&"TaskGet".to_owned()),
        "TaskGet must be resident by default: {tools:?}"
    );
    let frames = match index {
        0 => tool_call_frame(
            "call-bash",
            "Bash",
            &json!({
                "command": if fixture.base64 {
                    r#"node -e "const s=Buffer.from('ABC').toString('base64');process.stdout.write([s.repeat(256),s.repeat(25600),s.repeat(256)].join('!'))""#
                } else if fixture.foreground {
                    "node -e \"console.log('i6-start');console.log('.'.repeat(7500));console.log('error: hidden in the model preview');console.log('.'.repeat(7500));console.log('warning: duplicated tail')\""
                } else if fixture.long_output {
                    REVIEW_PAGE_COMMAND
                } else {
                    "node -e \"setTimeout(()=>{console.log('bg-line');process.exit(0)},200)\""
                },
                "description": "Print a delayed background line",
                "run_in_background": !fixture.foreground
            }),
        ),
        _ if fixture.foreground && index >= 1 => text_frames("foreground output observed"),
        1 => {
            let result = last_tool_result(&body)
                .unwrap_or_else(|| panic!("Bash result missing in request {body}"));
            *fixture.bash_result.lock().unwrap() = Some(result.clone());
            assert!(
                result["task_id"].as_str().is_some(),
                "Bash task_id missing: {result}"
            );
            tool_call_frame("call-search", "ToolLoad", &json!({"names":["TaskGet"]}))
        }
        2 => {
            // TaskGet is resident by default; ToolLoad confirms it is already available.
            assert_eq!(
                last_tool_content(&body),
                Some("Already available: TaskGet."),
                "{body}"
            );
            let started = fixture
                .bash_result
                .lock()
                .unwrap()
                .clone()
                .expect("Bash result");
            let task_id = started["task_id"].as_str().expect("Bash task_id");
            let mut args = json!({"task_id":task_id,"block":true,"timeout":5000});
            if fixture.long_output {
                args["max_output_bytes"] = json!(4);
            }
            tool_call_frame("call-taskget", "TaskGet", &args)
        }
        3 if fixture.long_output => {
            let first = last_tool_result(&body).expect("first TaskGet result");
            assert_eq!(first["stdout"], "not ");
            assert!(first.get("exit_code").is_none());
            assert!(first.get("model_output_summary").is_none());
            tool_call_frame(
                "call-taskget-continued",
                "TaskGet",
                &json!({
                    "task_id": first["task_id"], "cursor": first["next_cursor"], "max_output_bytes": 14000
                }),
            )
        }
        3 if fixture.base64 => text_frames("background output observed"),
        3 => {
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
    run_background_shell(false, OptOut::None, false).await;
}

#[tokio::test]
async fn cli_long_task_get_page_appends_summary_with_original_streams_and_cursor() {
    run_background_shell(true, OptOut::None, false).await;
}

#[tokio::test]
async fn sdk_command_output_opt_out_config_set_disables_provider_summary() {
    run_background_shell(true, OptOut::ConfigSet, false).await;
}

#[tokio::test]
async fn cli_foreground_shell_summary_stays_out_of_sdk_events_and_transcript() {
    run_background_shell(false, OptOut::None, true).await;
}

#[tokio::test]
async fn cli_command_output_opt_out_honors_environment_and_data_dir_selection() {
    run_background_shell(true, OptOut::Environment, false).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptOut {
    None,
    ConfigSet,
    Environment,
}

#[tokio::test]
async fn cli_base64_model_boundary_resumes_100_kib_shell_results() {
    for foreground in [true, false] {
        run_shell(
            Fixture {
                foreground,
                base64: true,
                ..Fixture::default()
            },
            OptOut::None,
        )
        .await;
    }
}

async fn run_background_shell(long_output: bool, opt_out: OptOut, foreground: bool) {
    run_shell(
        Fixture {
            foreground,
            long_output,
            ..Fixture::default()
        },
        opt_out,
    )
    .await;
}

// The shell fixture emits standard-alphabet data. A main/I6 cut can leave a
// noncanonical fragment that still contains many copies of the original quartet.
fn check_shell_payload(view: &str, phase: &str) {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    use base64::Engine as _;
    for candidate in view.split(|c: char| !(c.is_ascii_alphanumeric() || "+/-_=".contains(c))) {
        if candidate.len() >= 1024 && candidate.contains("QUJD") {
            assert!(
                STANDARD.decode(candidate).is_err() && STANDARD_NO_PAD.decode(candidate).is_err(),
                "{phase}: canonical {}-byte candidate {:.12}…",
                candidate.len(),
                candidate
            );
        }
    }
}

async fn resume_fixture(
    data: &std::path::Path,
    workspace: &std::path::Path,
    id: &str,
    prompt: &str,
) {
    let mut resume = Command::new(env!("CARGO_BIN_EXE_dcl"));
    resume
        .current_dir(workspace)
        .args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(data)
        .arg("--path")
        .arg(workspace)
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--resume",
            id,
            prompt,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), resume.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

async fn run_shell(fixture: Fixture, opt_out: OptOut) {
    let (foreground, long_output) = (fixture.foreground, fixture.long_output);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Keep the raw historical snapshot below the automatic compaction threshold
    // so the fixture can inspect replay and then request compaction explicitly.
    let context_window = if fixture.base64 { 1_000_000 } else { 128_000 };
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/providers.json"),
        json!([{
            "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
            "api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                "context_window":context_window,"max_tokens":4096,
                "supports_web_search":false,"supports_vision":false}]
        }])
        .to_string(),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(
        std::env::var_os("CLAUDE_SDK_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
    );
    if opt_out == OptOut::ConfigSet {
        let result = Command::new(env!("CARGO_BIN_EXE_dcl"))
            .arg("--data-dir")
            .arg(&data)
            .args(["config", "set", "summarize_long_command_output", "false"])
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let file: Value =
            serde_json::from_slice(&std::fs::read(data.join("config/settings.json")).unwrap())
                .unwrap();
        assert_eq!(file["summarize_long_command_output"], false);
        cmd.args(["--input-format", "stream-json"]);
    }
    cmd.current_dir(root.path());
    cmd.args(["--print", "--output-format", "stream-json"]);
    if opt_out != OptOut::Environment {
        cmd.arg("--data-dir").arg(&data);
    }
    cmd.arg("--path")
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
    if opt_out == OptOut::Environment {
        cmd.env("DEEPSEEK_CODE_DATA_DIR", &data)
            .env("DEEPSEEK_CODE_SUMMARIZE_LONG_COMMAND_OUTPUT", "false");
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .unwrap()
        .unwrap();
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
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        if foreground {
            2
        } else if long_output {
            5
        } else {
            4
        }
    );
    if fixture.base64 {
        let live = last_tool_content(requests.last().unwrap())
            .unwrap()
            .to_owned();
        drop(requests);
        assert!(live.contains("base64 data omitted"), "{live}");
        check_shell_payload(&live, "live request");
        let (path, line) = live
            .split_once("; full text: ")
            .unwrap()
            .1
            .split_once(", line ")
            .unwrap();
        let line = line.split_once(']').unwrap().0.parse::<usize>().unwrap();
        let saved = std::fs::read_to_string(path).unwrap();
        assert!(saved
            .lines()
            .nth(line - 1)
            .unwrap()
            .contains(&"QUJD".repeat(256)));
        let note = live
            .lines()
            .find(|line| line.starts_with("Saved tool result ("))
            .unwrap();
        assert!(live.contains(note));
        let i6_path = note.split_once("): ").unwrap().1;
        assert!(std::fs::read_to_string(i6_path)
            .unwrap()
            .contains(&"QUJD".repeat(256)));

        let public_events = String::from_utf8_lossy(&output.stdout);
        assert!(public_events.contains(&"QUJD".repeat(256)));
        assert!(!public_events.contains("[base64 data omitted"));
        let paths = deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let raw = store
            .db
            .shard_sql(&id, |conn| {
                conn.query_row(
                    "SELECT output FROM tool_calls WHERE name=?1",
                    [if foreground { "Bash" } else { "TaskGet" }],
                    |row| row.get::<_, String>(0),
                )
            })
            .unwrap()
            .unwrap();
        assert!(raw.contains(&"QUJD".repeat(256)));
        assert!(!raw.contains("[base64 data omitted"));
        assert_eq!(saved, raw);
        drop(store);

        resume_fixture(&data, root.path(), &id, "Replay the fixture").await;
        let replay = fixture.requests.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            last_tool_content(&replay),
            Some(live.as_str()),
            "fresh process replay"
        );
        let start = fixture.requests.lock().unwrap().len();
        resume_fixture(&data, root.path(), &id, "/compact Keep the fixture command").await;
        let session_outputs = std::path::Path::new(path).parent().unwrap();
        let compaction_file = std::fs::read_dir(session_outputs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|file| std::fs::read(file).unwrap() == raw.as_bytes())
            .expect("the historical row was saved exactly before compaction");
        // The model receives the transcript as JSON text inside a user message.
        let quoted_path = serde_json::to_string(&compaction_file).unwrap();
        let transcript_path = &quoted_path[1..quoted_path.len() - 1];
        let requests = fixture.requests.lock().unwrap();
        let mut found_reference = false;
        for request in &requests[start..] {
            for message in request["messages"].as_array().unwrap() {
                if let Some(text) = message["content"].as_str() {
                    check_shell_payload(text, "compaction message");
                    found_reference |=
                        text.contains("[base64 data omitted") && text.contains(transcript_path);
                }
            }
        }
        assert!(
            found_reference,
            "the compaction request must name the stored row"
        );
        drop(requests);
        server.abort();
        let _ = server.await;
        return;
    }
    if foreground {
        let summary = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(summary.contains("Omitted output:"), "{summary}");
        assert!(!summary.contains("Elapsed:"), "{summary}");
        assert_eq!(summary.matches("warning: duplicated tail").count(), 1);
        assert!(
            summary.contains(r#"stdout L3 | "error: hidden in the model preview""#),
            "{summary}"
        );
        assert!(summary.len() <= 16_000);
        let public_events = String::from_utf8_lossy(&output.stdout);
        assert!(public_events.contains("warning: duplicated tail"));
        assert!(!public_events.contains("output_capture"));
        assert!(!public_events.contains("Omitted output:"));
        let paths = deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let raw = store
            .db
            .shard_sql(&id, |conn| {
                conn.query_row(
                    "SELECT output FROM tool_calls WHERE name='Bash'",
                    [],
                    |row| row.get::<_, String>(0),
                )
            })
            .unwrap()
            .unwrap();
        assert!(raw.contains("warning: duplicated tail"));
        assert!(!raw.contains("output_capture"));
        assert!(!raw.contains("Omitted output:"));
        let summary = summary.to_owned();
        drop(requests);
        for prompt in ["Replay the fixture", "/compact Keep the fixture command"] {
            resume_fixture(&data, root.path(), &id, prompt).await;
        }
        server.abort();
        let requests = fixture.requests.lock().unwrap();
        let replay = requests[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(replay, summary);
        let compact = &requests[3..];
        assert!(!compact.is_empty());
        let compact = serde_json::to_string(compact).unwrap();
        assert!(compact.contains("warning: duplicated tail"), "{compact}");
        assert!(!compact.contains("output_capture"));
        assert!(compact.contains(&serde_json::to_string(&summary).unwrap()[1..serde_json::to_string(&summary).unwrap().len() - 1]), "{compact}");
        return;
    }

    server.abort();
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

    let retrieved: Value = if long_output {
        // The JSON-escaped page exceeds the model budget. Its public tool
        // event retains the complete page and cursor.
        let public = events
            .iter()
            .rev()
            .find(|e| e["type"] == "tool_result" && e["name"] == "TaskGet")
            .unwrap();
        serde_json::from_str(public["output"].as_str().unwrap()).unwrap()
    } else {
        fixture
            .taskget_result
            .lock()
            .unwrap()
            .clone()
            .expect("TaskGet result")
    };
    assert_eq!(retrieved["status"], "completed", "{retrieved}");
    assert_eq!(retrieved["retrieval_status"], "success", "{retrieved}");
    if long_output {
        let exact = format!(
            "{REVIEW_PAGE_LINE}\n{}",
            format!("{}\n", "\"".repeat(59)).repeat(250)
        );
        assert_eq!(retrieved["stdout"], &exact[4..14004]);
        assert_eq!(retrieved["stderr"], "");
        assert_eq!(retrieved["output_mode"], "page");
        assert_eq!(retrieved["has_more"], true);
        assert!(retrieved.get("exit_code").is_none());
        assert!(retrieved["next_cursor"].is_string());
        let rendered = requests[4]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(rendered.len() <= 16_000, "{}", rendered.len());
        assert_eq!(
            rendered.contains("Omitted output:"),
            opt_out == OptOut::None
        );
        assert!(retrieved.get("model_output_summary").is_none());
    } else {
        assert_eq!(retrieved["stdout"], "bg-line\n");
        assert!(retrieved.get("model_output_summary").is_none());
    }
}

/// Drives the model through successive bounded TaskGet pages of one background
/// shell, so the real CLI protocol is exercised rather than a unit helper.
async fn respond_paged(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let index = {
        let mut requests = fixture.requests.lock().unwrap();
        requests.push(body.clone());
        requests.len() - 1
    };
    let tools = tool_names(&body);
    assert!(
        tools.contains(&"TaskGet".to_owned()),
        "TaskGet missing: {tools:?}"
    );
    let frames = if index == 0 {
        let command = fixture.raw_size.map_or_else(
            || "node -e \"process.stdout.write('alpha\\nbeta\\n');process.stderr.write('err\\n');setTimeout(()=>process.exit(0),150)\"".to_owned(),
            |size| format!("node -e \"process.stdout.write(Buffer.concat([Buffer.alloc({},113),Buffer.from([255,10])]))\"", size - 2));
        tool_call_frame(
            "call-bash",
            "Bash",
            &json!({"command":command,"run_in_background":true}),
        )
    } else if fixture.raw_size.is_some() {
        if index == 1 {
            let started = last_tool_result(&body).expect("background Bash result");
            *fixture.bash_result.lock().unwrap() = Some(started.clone());
            tool_call_frame(
                "snapshot",
                "TaskGet",
                &json!({"task_id":started["task_id"],"block":true,"timeout":5000}),
            )
        } else if index == 2 {
            let started = fixture.bash_result.lock().unwrap().clone().unwrap();
            tool_call_frame(
                "page",
                "TaskGet",
                &json!({"task_id":started["task_id"],"block":true,"timeout":5000,"max_output_bytes":13000}),
            )
        } else {
            text_frames("paired reads complete")
        }
    } else {
        let result = last_tool_result(&body)
            .unwrap_or_else(|| panic!("tool result missing in request {body}"));
        if result.get("output_mode").is_none() {
            *fixture.bash_result.lock().unwrap() = Some(result.clone());
            tool_call_frame(
                "call-get-1",
                "TaskGet",
                &json!({"task_id":result["task_id"],"block":true,"timeout":5000,"max_output_bytes":5}),
            )
        } else if result["has_more"] == true {
            fixture.pages.lock().unwrap().push(result.clone());
            tool_call_frame(
                &format!("call-get-{}", index + 1),
                "TaskGet",
                &json!({"task_id":result["task_id"],"cursor":result["next_cursor"],"max_output_bytes":5}),
            )
        } else {
            fixture.pages.lock().unwrap().push(result.clone());
            text_frames("pages read")
        }
    };
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn cli_task_get_pages_a_background_shell_to_its_exit_status() {
    paged_run(None).await;
}

#[tokio::test]
async fn cli_task_get_snapshot_and_page_share_the_raw_bash_threshold() {
    for size in [11_999, 12_000] {
        paged_run(Some(size)).await;
    }
}

async fn paged_run(raw_size: Option<usize>) {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    tool_loading::enable(&data, &["TaskGet"]);
    let fixture = Fixture {
        raw_size,
        ..Fixture::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond_paged))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/providers.json"),
        json!([{
            "id":"page-fixture","name":"Page Fixture","base_url":base,"api_key":"fixture-only",
            "api_format":"openai","enabled":true,
            "models":[{"id":"page-model","name":"Page Model","enabled":true,
                "context_window":128000,"max_tokens":4096,
                "supports_web_search":false,"supports_vision":false}]
        }])
        .to_string(),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&data)
        .arg("--path")
        .arg(root.path())
        .args([
            "--provider",
            "page-fixture",
            "--model",
            "page-model",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            &id,
            "fixture pages",
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

    let started = fixture
        .bash_result
        .lock()
        .unwrap()
        .clone()
        .expect("Bash result");
    assert_eq!(started["status"], "running", "{started}");
    assert!(started["task_id"].as_str().is_some());

    if let Some(size) = raw_size {
        let values: Vec<Value> = events
            .iter()
            .filter(|e| e["type"] == "tool_result" && e["name"] == "TaskGet")
            .map(|e| serde_json::from_str(e["output"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["task_id"], values[1]["task_id"]);
        assert_eq!(values[0]["stdout"], values[1]["stdout"]);
        assert_eq!(values[0]["stdout"].as_str().unwrap().len(), size + 2);
        let requests = fixture.requests.lock().unwrap();
        for request in &requests[2..=3] {
            let content = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert!(
                !content.contains("Omitted output:") && !content.contains("model_output_summary"),
                "complete page or aliased snapshot"
            );
        }
        return;
    }
    let pages = fixture.pages.lock().unwrap().clone();
    assert!(pages.len() > 1, "expected several pages: {pages:?}");
    let mut stdout = String::new();
    let mut stderr = String::new();
    for page in &pages {
        assert_eq!(page["output_mode"], "page", "{page}");
        assert!(page["is_error"].is_null(), "the tool must not fail: {page}");
        // The bound covers all returned log text.
        assert!(
            page["stdout"].as_str().unwrap().len() + page["stderr"].as_str().unwrap().len() <= 5,
            "{page}"
        );
        stdout.push_str(page["stdout"].as_str().unwrap());
        stderr.push_str(page["stderr"].as_str().unwrap());
    }
    assert_eq!(stdout, "alpha\nbeta\n");
    assert_eq!(stderr, "err\n");
    let last = pages.last().unwrap();
    assert_eq!(last["status"], "completed", "{last}");
    assert_eq!(last["retrieval_status"], "success", "{last}");
    assert_eq!(last["has_more"], false, "{last}");
    assert_eq!(last["output_truncated"], false, "{last}");
}
