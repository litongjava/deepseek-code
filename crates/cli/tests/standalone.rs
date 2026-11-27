//! Real executable + local model fixture. No Desktop, paid API or user config.
#[path = "../../core/tests/support/session_sql.rs"]
mod fixture_sql;
use fixture_sql::FixtureSql;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

#[derive(Clone)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    tool: Option<String>,
    fail: bool,
}

async fn respond(
    State(state): State<Fixture>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.requests.lock().unwrap().push(body.clone());
    if body["stream"] == true && body["messages"].to_string().contains("TIMED_TURN_FIXTURE") {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if state.fail {
        // A recognized JSON error field, because arbitrary plain text is no
        // longer reproduced in diagnostics: the body could echo the request.
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {"type": "invalid_request_error", "message": "fixture failure"}
            })),
        )
            .into_response();
    }
    if body["stream"] == true
        && headers
            .get("x-dcl-request-kind")
            .is_some_and(|kind| kind == "compaction")
    {
        return chat_stream(
            json!({"content":"SUMMARY_SENTINEL: preserve task and run tests"}),
            "stop",
        );
    }
    if body["stream"] != true {
        return Json(json!({"id":"summary", "object":"chat.completion", "created":0,
            "model":"fixture-model", "choices":[{"index":0,"message":{"role":"assistant","content":"SUMMARY_SENTINEL: preserve task and run tests"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}})).into_response();
    }
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let (delta, finish) = if let Some(tool) =
        state.tool.as_deref().filter(|_| last["role"] != "tool")
    {
        let args = if tool == "Read" {
            json!({"file_path":"fixture.txt"})
        } else {
            json!({"file_path":"created.txt","content":"written by shared core"})
        };
        (
            json!({"tool_calls":[{"index":0,"id":"call-fixture","type":"function","function":{"name":tool,"arguments":args.to_string()}}]}),
            "tool_calls",
        )
    } else {
        let content = if last["role"] == "tool" {
            format!("tool completed: {}", last["content"].as_str().unwrap())
        } else {
            "fixture response 中文".to_string()
        };
        (json!({"content":content}), "stop")
    };
    chat_stream(delta, finish)
}

async fn server(tool: Option<&str>, fail: bool) -> (String, Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        requests: Arc::new(Mutex::new(vec![])),
        tool: tool.map(String::from),
        fail,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, fixture, task)
}

fn command(data: &Path, project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command.args([
        "--data-dir",
        data.to_str().unwrap(),
        "--path",
        project.to_str().unwrap(),
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "API_BASE_URL",
        "OPENAI_BASE_URL",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_MODEL",
        "RUST_LOG",
    ] {
        command.env_remove(key);
    }
    command.env("CLAUDE_FIXTURE_KEY", "fixture-only-secret");
    command
}

async fn execute(mut command: Command) -> Output {
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("CLI hung")
        .unwrap()
}

#[tokio::test]
async fn builtin_provider_and_shared_tools_run_without_node_on_path() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(temp.path().join("fixture.txt"),"native-only fixture").unwrap();
    let (url, fixture, task) = server(Some("Read"), false).await;
    let providers = json!([{"id":"builtin","name":"Built-in DeepSeek","base_url":format!("{url}/v1"),"api_key":"fixture-only-secret","api_format":"openai","builtin_id":"deepseek","enabled":true,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false,"max_tokens":8192,"api":"openai-completions"}]}]);
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/providers.json"),
        providers.to_string(),
    )
    .unwrap();
    let mut cli = command(&data, temp.path());
    cli.env("PATH", "").args([
        "--provider",
        "builtin",
        "--model",
        "fixture-model",
        "--api-key-env",
        "CLAUDE_FIXTURE_KEY",
        "--output-format",
        "json",
        "--permission-mode",
        "accept-edits",
        "query",
        "-p",
        "Read the fixture",
    ]);
    let output = execute(cli).await;
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let output: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output["result"].as_str().unwrap().contains("native-only fixture"));
    assert_durable_turn(&data, &output, "completed");
    assert_eq!(fixture.requests.lock().unwrap().len(),2);
    task.abort();
}

fn assert_durable_turn(data: &Path, result: &Value, status: &str) {
    let turn = &result["turn"];
    assert_eq!(turn["status"], status);
    assert!(turn["elapsedMs"].as_u64().is_some());
    assert!(turn["startedAt"].as_str().is_some());
    assert!(turn["endedAt"].as_str().is_some());
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_owned()).unwrap();
    let db = paths.open_database().unwrap();
    db.shard_sql(result["session_id"].as_str().unwrap(), |conn| {
        let messages = deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, result["session_id"].as_str().unwrap()).unwrap();
        let saved = messages.iter().filter_map(|message| deepseek_code_core::db::turn_repo::get_message_turn(conn, &message.id).unwrap()).find(|record| record.turn_id == turn["turnId"].as_str().unwrap()).unwrap();
        assert_eq!(serde_json::to_value(saved).unwrap(), *turn);
    }).unwrap();
}

#[tokio::test]
async fn whole_turn_timing_spans_multiple_model_requests_and_tool_execution() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::write(temp.path().join("fixture.txt"), "timed tool result").unwrap();
    let (url, fixture, task) = server(Some("Read"), false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["--permission-mode", "accept-edits", "query", "-p", "TIMED_TURN_FIXTURE read fixture"]);
    let output = execute(cli).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    assert!(result["result"].as_str().unwrap().contains("timed tool result"));
    assert!(result["turn"]["elapsedMs"].as_u64().unwrap() >= 100);
    assert_durable_turn(&data, &result, "completed");
    task.abort();
}

fn query(data: &Path, project: &Path, url: &str) -> Command {
    let mut command = command(data, project);
    command.args([
        "--model",
        "fixture-model",
        "--api-format",
        "openai",
        "--base-url",
        url,
        "--api-key-env",
        "CLAUDE_FIXTURE_KEY",
        "--output-format",
        "json",
    ]);
    command
}

#[tokio::test]
async fn agent_alias_custom_definition_and_readonly_limit_use_shared_tool_loop() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let (url, fixture, task) = server(Some("Write"), false).await;
    let mut cli = query(&data,temp.path(),&url);
    cli.args(["--permission-mode","bypass","agent","--agent-type","claude-code-guide","--prompt","Inspect project"]);
    let output = execute(cli).await;
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    assert!(!temp.path().join("created.txt").exists());
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    {
        let requests = fixture.requests.lock().unwrap();
        assert!(requests[0]["messages"][0]["content"].as_str().unwrap().contains("Agent role (guide)"));
        assert!(!requests[0]["tools"].as_array().unwrap().iter().any(|t|t["function"]["name"]=="Write"));
        assert!(requests[1]["messages"].as_array().unwrap().iter().any(|m|m["role"]=="tool" && m["content"].as_str().unwrap().contains("not allowed")));
    }
    let mut cli = query(&data,temp.path(),&url);
    cli.args(["--resume",first["session_id"].as_str().unwrap(),"--permission-mode","bypass","query","-p","Continue inspection"]);
    assert!(execute(cli).await.status.success());
    assert!(!temp.path().join("created.txt").exists());
    let source = temp.path().join("agent.json");
    std::fs::write(&source,json!({"id":"custom","description":"Custom reader","system_prompt":"Report CUSTOM_SENTINEL.","tools":["Read"]}).to_string()).unwrap();
    let mut cli = command(&data,temp.path()); cli.args(["agents","import",source.to_str().unwrap()]);
    assert!(execute(cli).await.status.success());
    let mut cli = query(&data,temp.path(),&url); cli.args(["agent","--agent-type","custom","--prompt","Inspect"]);
    assert!(execute(cli).await.status.success());
    assert!(!temp.path().join("created.txt").exists());
    let mut cli = query(&data,temp.path(),&url); cli.args(["agent","--agent-type","missing","--prompt","Inspect"]);
    assert!(!execute(cli).await.status.success());
    task.abort();
}

#[tokio::test]
async fn single_query_and_resume_share_sqlite_without_desktop_or_saved_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let (url, fixture, task) = server(None, false).await;
    let mut cli = query(&data, &project, &url);
    cli.args(["query", "-p", "first prompt"]);
    let first = execute(cli).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["result"], "fixture response 中文");
    assert_durable_turn(&data, &first, "completed");
    assert_eq!(first["is_error"], false);
    assert!(!data.join("config/providers.json").exists());
    assert!(!data.join("config/config.json").exists());
    let mut cli = query(&data, &project, &url);
    cli.args([
        "--resume",
        first["session_id"].as_str().unwrap(),
        "query",
        "-p",
        "second prompt",
    ]);
    let second = execute(cli).await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    // Loaded user/project instructions may prefix the original prompt.
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|text|
            text == "first prompt" || text.ends_with("\n\nfirst prompt"))));
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"] == "fixture response 中文"));
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Code mode:")));
    task.abort();
}

#[tokio::test]
async fn native_read_and_write_tools_operate_in_selected_project() {
    for tool in ["Read", "Write"] {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::write(temp.path().join("fixture.txt"), "source content 中文").unwrap();
        let (url, fixture, task) = server(Some(tool), false).await;
        let mut cli = query(&data, temp.path(), &url);
        cli.args([
            "--permission-mode",
            "accept-edits",
            "query",
            "-p",
            "perform task",
        ]);
        let output = execute(cli).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e|panic!("{e}; stderr: {}",String::from_utf8_lossy(&output.stderr)));
        assert!(result["result"]
            .as_str()
            .unwrap()
            .contains("tool completed"));
        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
        if tool == "Read" {
            assert!(result["result"]
                .as_str()
                .unwrap()
                .contains("source content 中文"));
        } else {
            assert_eq!(
                std::fs::read_to_string(temp.path().join("created.txt")).unwrap(),
                "written by shared core"
            );
        }
        let session_id = result["session_id"].as_str().unwrap();
        let mut resumed = query(&data, temp.path(), &url);
        resumed.args([
            "--resume",
            session_id,
            "--permission-mode",
            "accept-edits",
            "query",
            "-p",
            "continue",
        ]);
        let resumed = execute(resumed).await;
        assert!(
            resumed.status.success(),
            "{}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        let requests = fixture.requests.lock().unwrap();
        let history = requests[2]["messages"].as_array().unwrap();
        assert!(history
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"].as_str().is_some()));
        assert!(history
            .iter()
            .any(|m| m["role"] == "assistant" && m["tool_calls"].is_array()));
        task.abort();
    }
}

#[tokio::test]
async fn noninteractive_approval_and_api_errors_fail_without_hanging() {
    for fail in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let (url, _, task) = server(Some("Write"), fail).await;
        let mut cli = query(&data, temp.path(), &url);
        cli.args(["query", "-p", "perform task"]);
        let output = execute(cli).await;
        assert!(!temp.path().join("created.txt").exists());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e|panic!("{e}; stderr: {}",String::from_utf8_lossy(&output.stderr)));
        if fail {
            assert!(!output.status.success());
            assert_eq!(result["is_error"], true);
            let error = result["error"].as_str().unwrap();
            assert!(error.contains("invalid_request_error"), "{error}");
            assert!(!error.contains("fixture failure"), "{error}");
            assert_durable_turn(&data, &result, "failed");
        } else {
            // With no approval host the call is denied and the model finishes the turn.
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(result["is_error"], false);
            let text = result["result"].as_str().unwrap();
            assert!(text.contains("Permission to use Write was not granted"), "{text}");
        }
        task.abort();
    }
}

#[tokio::test]
async fn piped_input_stream_json_and_legacy_import_are_scriptable() {
    use tokio::io::AsyncWriteExt;
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let (url, fixture, task) = server(None, false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["--output-format", "stream-json", "--print"])
        .stdin(Stdio::piped());
    let mut child = cli.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all("piped 中文".as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(events.iter().any(|v| v["type"] == "text"));
    assert_eq!(events.last().unwrap()["type"], "result");
    let requests = fixture.requests.lock().unwrap();
    let first_user = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap();
    assert!(first_user.ends_with("piped 中文"));
    assert!(first_user.contains(
        "<system-reminder>\nStandalone DeepSeek Code terminal: no Desktop window or Bridge server"
    ));
    task.abort();
    let old = temp.path().join("settings.json");
    let original = json!({"model":"deepseek-chat","api":{"base_url":"https://api.deepseek.com","api_key":"legacy-only-secret","max_tokens":4096}}).to_string();
    std::fs::write(&old, &original).unwrap();
    let mut cli = command(&data, temp.path());
    cli.args(["config", "import-legacy", old.to_str().unwrap()]);
    let imported = execute(cli).await;
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(std::fs::read_to_string(old).unwrap(), original);
    let mut cli = command(&data, temp.path());
    cli.args(["config", "show"]);
    let shown = execute(cli).await;
    assert!(shown.status.success());
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(!shown.contains("legacy-only-secret"));
    assert!(shown.contains("[redacted]"));
}

#[tokio::test]
async fn empty_stdin_and_malformed_config_fail_without_overwriting() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let output = execute(command(&data, temp.path())).await;
    assert!(!output.status.success());
    // Startup diagnostics may create logs, but no session or configuration is saved.
    assert!(!data.join("deepseek_code.db").exists());
    assert!(!data.join("config/config.json").exists());
    std::fs::create_dir_all(&data).unwrap();
    deepseek_code_core::data_layout::write_config_file(data.join("config/config.json"), "not json")
        .unwrap();
    let mut cli = command(&data, temp.path());
    cli.args(["config", "set", "model", "sonnet"]);
    assert!(!execute(cli).await.status.success());
    assert_eq!(
        std::fs::read_to_string(data.join("config/config.json")).unwrap(),
        "not json"
    );
}

#[tokio::test]
async fn legacy_agent_import_preserves_allowlist_in_real_shared_engine() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("old-agent.json");
    let raw = json!({"agent_type":"Custom","name":"Old Reviewer","description":"Read only","when_to_use":"Review","tools":["file_read"],"model":"old-model","system_prompt":"LEGACY_SENTINEL","source":"user","base_dir":"unused-old-path"}).to_string();
    std::fs::write(&source, &raw).unwrap();
    let mut cli = command(&data, temp.path());
    cli.args([
        "agents",
        "import-legacy",
        source.to_str().unwrap(),
        "--id",
        "LegacyReader",
        "--preview",
    ]);
    let preview = execute(cli).await;
    assert!(preview.status.success());
    let mut cli = command(&data, temp.path());
    cli.args(["agents", "list"]);
    assert!(!String::from_utf8_lossy(&execute(cli).await.stdout).contains("LegacyReader"));
    for _ in 0..2 {
        let mut cli = command(&data, temp.path());
        cli.args([
            "agents",
            "import-legacy",
            source.to_str().unwrap(),
            "--id",
            "LegacyReader",
        ]);
        assert!(execute(cli).await.status.success());
    }
    let (url, fixture, task) = server(Some("Write"), false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args([
        "--permission-mode",
        "bypass",
        "agent",
        "--agent-type",
        "LegacyReader",
        "--prompt",
        "Review",
    ]);
    let output = execute(cli).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("created.txt").exists());
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests[0]["model"], "fixture-model");
    assert!(requests[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("LEGACY_SENTINEL"));
    assert!(!requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "Write"));
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("not allowed")));
    assert_eq!(std::fs::read_to_string(source).unwrap(), raw);
    drop(requests);
    task.abort();
    std::fs::write(temp.path().join("fixture.txt"), "READ_ALLOWED_SENTINEL").unwrap();
    let (url, _, task) = server(Some("Read"), false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args([
        "agent",
        "--agent-type",
        "LegacyReader",
        "--prompt",
        "Read fixture",
    ]);
    let output = execute(cli).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["result"]
        .as_str()
        .unwrap()
        .contains("READ_ALLOWED_SENTINEL"));
    // Default reads need no approval; an explicit ask still applies to imported agents.
    let id = result["session_id"].as_str().unwrap();
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap();
    let db = paths.open_database().unwrap();
    let rules = std::collections::HashMap::from([(
        "Read",
        deepseek_code_core::permissions::rules::PermissionAction::Confirm,
    )]);
    db.shard_sql(&id, |conn| {
        conn.execute(
            "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
            [
                "cli.permissions".to_owned(),
                serde_json::to_string(&rules).unwrap(),
            ],
        )
    })
    .unwrap()
    .unwrap();
    let mut cli = query(&data, temp.path(), &url);
    cli.args([
        "--resume",
        id,
        "query",
        "-p",
        "Read fixture with explicit approval rule",
    ]);
    let output = execute(cli).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let text = result["result"].as_str().unwrap();
    assert!(text.contains("Permission to use Read was not granted"), "{text}");
    assert!(!text.contains("READ_ALLOWED_SENTINEL"), "{text}");
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["--permission-mode", "accept-edits", "agent", "--agent-type", "LegacyReader", "--prompt", "Read fixture"]);
    let output = execute(cli).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("READ_ALLOWED_SENTINEL"));
    task.abort();
}

#[tokio::test]
async fn continue_fork_and_prompt_file_use_shared_session_without_changing_source() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let (url, fixture, task) = server(None, false).await;
    let mut first = query(&data, temp.path(), &url);
    first.args(["query", "-p", "original"]);
    let first = execute(first).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let prompt_file = temp.path().join("system.txt");
    std::fs::write(&prompt_file, "SYSTEM_FILE_SENTINEL 中文").unwrap();
    let mut next = query(&data, temp.path(), &url);
    next.args([
        "--continue",
        "--fork-session",
        "--system-prompt-file",
        prompt_file.to_str().unwrap(),
        "--append-system-prompt",
        "APPEND_SENTINEL",
        "query",
        "-p",
        "follow up",
    ]);
    let next = execute(next).await;
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    let next: Value = serde_json::from_slice(&next.stdout).unwrap();
    assert_ne!(first["session_id"], next["session_id"]);
    {
        let requests = fixture.requests.lock().unwrap();
        let body = requests.last().unwrap().to_string();
        assert!(body.contains("SYSTEM_FILE_SENTINEL"));
        assert!(body.contains("APPEND_SENTINEL"));
        assert!(body.contains("original"));
        assert!(body.contains("follow up"));
    }
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data).unwrap();
    let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
    assert_eq!(
        store
            .history(first["session_id"].as_str().unwrap())
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .history(next["session_id"].as_str().unwrap())
            .unwrap()
            .len(),
        4
    );
    task.abort();
}

#[tokio::test]
async fn compact_command_generates_summary_and_resume_uses_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let (url, fixture, task) = server(None, false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["query", "-p", "ORIGINAL_SENTINEL"]);
    let first = execute(cli).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let id = first["session_id"].as_str().unwrap();
    let mut cli = query(&data, temp.path(), &url);
    cli.args([
        "sessions",
        "compact",
        id,
        "--instructions",
        "Preserve pending tests",
    ]);
    let compacted = execute(cli).await;
    assert!(
        compacted.status.success(),
        "{}",
        String::from_utf8_lossy(&compacted.stderr)
    );
    let compacted: Value = serde_json::from_slice(&compacted.stdout).unwrap();
    assert_eq!(compacted["summarized_messages"], 2);
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["--resume", id, "query", "-p", "Continue after summary"]);
    let resumed = execute(cli).await;
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].to_string().contains("ORIGINAL_SENTINEL"));
    assert!(requests[2].to_string().contains("SUMMARY_SENTINEL"));
    assert!(!requests[2].to_string().contains("ORIGINAL_SENTINEL"));
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data).unwrap();
    let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
    assert_eq!(store.history(id).unwrap().len(), 5);
    task.abort();
}


#[tokio::test]
async fn a_queued_request_compacts_before_the_next_prompt_with_the_keep_text() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let (url, fixture, task) = server(None, false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["query", "-p", "ORIGINAL_SENTINEL"]);
    let first = execute(cli).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let id = first["session_id"].as_str().unwrap();
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap();
    let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
    deepseek_code_core::services::compaction_requests::request(
        &store.db,
        id,
        "KEEP_SENTINEL: preserve the pending tests",
        50,
        100,
    )
    .unwrap();
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["--resume", id, "query", "-p", "follow up"]);
    let second = execute(cli).await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1].to_string().contains("KEEP_SENTINEL"),
            "{}",
            requests[1]
        );
        let history = requests[2]["messages"].to_string();
        assert!(history.contains("SUMMARY_SENTINEL"), "{history}");
        assert!(!history.contains("ORIGINAL_SENTINEL"), "{history}");
    }
    assert!(store.db.open_session(id).unwrap().delivery_owner().unwrap().claim_compaction()
        .unwrap()
        .is_none());
    let boundaries: i64 = store
        .db
        .shard_sql(id, |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1",
                [id],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(boundaries, 1);
    task.abort();
}

#[tokio::test]
async fn manual_compact_clears_a_queued_request() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let (url, fixture, task) = server(None, false).await;
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["query", "-p", "ORIGINAL_SENTINEL"]);
    let first = execute(cli).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let id = first["session_id"].as_str().unwrap();
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap();
    let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
    deepseek_code_core::services::compaction_requests::request(
        &store.db,
        id,
        "KEEP_SENTINEL: preserve the pending tests",
        50,
        100,
    )
    .unwrap();
    let mut cli = query(&data, temp.path(), &url);
    cli.args(["--resume", id, "query", "-p", "/compact"]);
    let compacted = execute(cli).await;
    assert!(
        compacted.status.success(),
        "{}",
        String::from_utf8_lossy(&compacted.stderr)
    );
    assert!(store.db.open_session(id).unwrap().delivery_owner().unwrap().claim_compaction()
        .unwrap()
        .is_none());
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    task.abort();
}


#[tokio::test]
async fn session_lifecycle_hooks_run_once_per_frontend_and_resume_context_reaches_model() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(temp.path().join("session-hook.cjs"), r#"const fs=require('fs');let text='';process.stdin.on('data',x=>text+=x);process.stdin.on('end',()=>{const v=JSON.parse(text);fs.appendFileSync('session-events.jsonl',JSON.stringify(v)+'\n');if(v.hook_event_name==='SessionStart')console.log('LIFECYCLE_CONTEXT:'+v.source);});"#).unwrap();
    deepseek_code_core::data_layout::write_config_file(data.join("config/settings.json"),json!({"hooks":{
        "SessionStart":[{"matcher":"startup|resume","hooks":[{"type":"command","command":"node session-hook.cjs"}]}],
        "SessionEnd":[{"matcher":"other","hooks":[{"type":"command","command":"node session-hook.cjs"}]}]
    }}).to_string()).unwrap();
    let (url,fixture,server)=server(None,false).await;
    let mut cli=query(&data,temp.path(),&url);
    cli.args(["query","-p","hello"]);
    let output=execute(cli).await;
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let output:Value=serde_json::from_slice(&output.stdout).unwrap();
    let session=output["session_id"].as_str().unwrap();
    let mut cli=query(&data,temp.path(),&url);
    cli.args(["--resume",session,"query","-p","continue"]);
    let output=execute(cli).await;
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let events:Vec<Value>=std::fs::read_to_string(temp.path().join("session-events.jsonl")).unwrap().lines().map(|line|serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.len(),4);
    assert_eq!(events[0]["source"],"startup");
    assert_eq!(events[2]["source"],"resume");
    assert_eq!(events[1]["hook_event_name"],"SessionEnd");
    assert_eq!(events[3]["session_id"],session);
    let requests=fixture.requests.lock().unwrap();
    assert!(requests[0]["messages"][0]["content"].as_str().unwrap().contains("LIFECYCLE_CONTEXT:startup"));
    assert!(requests[1]["messages"][0]["content"].as_str().unwrap().contains("LIFECYCLE_CONTEXT:resume"));
    assert!(!requests[1]["messages"][0]["content"].as_str().unwrap().contains("LIFECYCLE_CONTEXT:startup"));
    server.abort();
}

const CONTEXT_SEARCH_FACT: &str = "x19-qrx-441";
const CONTEXT_SEARCH_REPLY: &str = "Recovered archive ID x19-qrx-441 from the saved Read result.";

async fn context_search_provider(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let step = {
        let mut requests = requests.lock().unwrap();
        requests.push(body.clone());
        requests.len()
    };
    if step == 3 {
        assert_eq!(headers["x-dcl-request-kind"], "compaction");
        let summary = json!({"role":"assistant","content":"COMPACT_FIXTURE: A file was read."});
        if body["stream"] == true {
            return chat_stream(summary, "stop");
        }
        return Json(json!({
            "id":"fixture-summary","object":"chat.completion","created":0,"model":"fixture-model",
            "choices":[{"index":0,"message":summary,"finish_reason":"stop"}]
        })).into_response();
    }
    let call = |id: &str, name: &str, args: Value| json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
    let messages = body["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    let (delta, finish) = match step {
        1 => (
            call("fixture-read", "Read", json!({"file_path":"proof.txt"})),
            "tool_calls",
        ),
        2 => (json!({"content":"The fixture artifact was read."}), "stop"),
        4 => (
            call(
                "fixture-load",
                "ToolLoad",
                json!({"names":["ContextSearch"]}),
            ),
            "tool_calls",
        ),
        5 => (
            call(
                "fixture-search",
                "ContextSearch",
                json!({"query":"archive id:","range":"before","role":"tool","tool_name":"Read","limit":1}),
            ),
            "tool_calls",
        ),
        6 => {
            let result: Value =
                serde_json::from_str(last["content"].as_str().unwrap_or("")).unwrap_or_default();
            let text = if last["role"] == "tool"
                && result["matches"][0]["excerpt"]
                    .as_str()
                    .is_some_and(|excerpt| excerpt.contains(CONTEXT_SEARCH_FACT))
            {
                CONTEXT_SEARCH_REPLY
            } else {
                "The search result did not contain the archive ID."
            };
            (json!({"content":text}), "stop")
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":"Unexpected context search fixture request"}})),
            )
                .into_response()
        }
    };
    chat_stream(delta, finish)
}

fn chat_stream(delta: Value, finish: &str) -> Response {
    let start = json!({"id":"fixture","model":"fixture-model","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let end = json!({"id":"fixture","model":"fixture-model","choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":12,"completion_tokens":8}});
    (
        [("content-type", "text/event-stream")],
        format!("data: {start}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}

async fn assert_cli_context_search(sdk: bool) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::write(
        temp.path().join("proof.txt"),
        format!("archive id: {CONTEXT_SEARCH_FACT}\n"),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(context_search_provider))
        .with_state(requests.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mut initial = query(&data, temp.path(), &url);
    initial.args(["--print", "Read proof.txt to learn the archive ID"]);
    let first = execute(initial).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["result"], "The fixture artifact was read.");
    std::fs::remove_file(temp.path().join("proof.txt")).unwrap();
    let id = first["session_id"].as_str().unwrap();

    let mut compact = query(&data, temp.path(), &url);
    compact.args(["sessions", "compact", id]);
    let compacted = execute(compact).await;
    assert!(
        compacted.status.success(),
        "{}",
        String::from_utf8_lossy(&compacted.stderr)
    );
    let compacted: Value = serde_json::from_slice(&compacted.stdout).unwrap();
    let boundary = compacted["message_id"].as_str().unwrap();

    let mut resume = query(&data, temp.path(), &url);
    resume.args(["--resume", id, "--print"]);
    let prompt = "Recover the exact archive ID from the earlier Read.";
    let result: Value = if sdk {
        resume
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ])
            .stdin(Stdio::piped());
        let mut child = resume.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = json!({"type":"user","message":{"role":"user","content":prompt}});
        stdin
            .write_all(format!("{input}\n").as_bytes())
            .await
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                let frame: Value = serde_json::from_str(&line).unwrap();
                if frame["type"] == "result" {
                    return Some(frame);
                }
            }
            None
        })
        .await
        .expect("SDK result timeout");
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        result.unwrap_or_else(|| panic!("SDK result frame missing: {stderr}"))
    } else {
        resume.arg(prompt);
        let output = execute(resume).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    assert_eq!(result["result"], CONTEXT_SEARCH_REPLY);
    server.abort();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    let fresh = requests[0]["messages"].to_string();
    assert!(fresh.contains("- ContextSearch:"), "{fresh}");
    assert!(!fresh.contains("After compaction, load ContextSearch"));
    assert!(
        !requests[0]["messages"][0]["content"]
            .to_string()
            .contains("Tools you can load with ToolLoad"),
        "the list must stay out of the system prompt: {}",
        requests[0]["messages"][0]["content"]
    );
    assert!(requests[1].to_string().contains(CONTEXT_SEARCH_FACT));
    assert!(requests[2].to_string().contains(CONTEXT_SEARCH_FACT));
    for body in &requests[3..5] {
        assert!(body["messages"].to_string().contains("COMPACT_FIXTURE"));
        assert!(!body.to_string().contains(CONTEXT_SEARCH_FACT));
    }
    // The first message is frozen, so the same list appears after compaction
    // and nothing is appended to the system prompt.
    assert!(requests[3]["messages"]
        .to_string()
        .contains("- ContextSearch:"));
    assert!(!requests[3]["messages"]
        .to_string()
        .contains("After compaction, load ContextSearch with ToolLoad"));
    let tool = |index: usize| {
        requests[index]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == "ContextSearch")
    };
    assert!(tool(3).is_none());
    assert_eq!(
        tool(4).unwrap()["function"]["parameters"]["required"],
        json!(["query"])
    );
    let messages = requests[5]["messages"].as_array().unwrap();
    let final_tool = messages.iter().rev().find(|m| m["role"] == "tool").unwrap();
    let value: Value = serde_json::from_str(final_tool["content"].as_str().unwrap()).unwrap();
    assert_eq!(value["boundary_id"], boundary);
    let hit = &value["matches"][0];
    assert_eq!(hit["role"], "tool");
    assert_eq!(hit["tool_name"], "Read");
    assert!(hit["excerpt"]
        .as_str()
        .unwrap()
        .contains(CONTEXT_SEARCH_FACT));

    let db = deepseek_code_core::runtime::RuntimePaths::new(data)
        .unwrap()
        .open_database()
        .unwrap();
    db.shard_sql(id, |conn| {
        use deepseek_code_core::db::{message_repo, turn_repo};
        let message_id = hit["message_id"].as_str().unwrap();
        let original = message_repo::get_message(conn, message_id)
            .unwrap()
            .unwrap();
        assert_eq!(original.conversation_id, id);
        assert_eq!(original.created_at, hit["timestamp"].as_str().unwrap());
        assert_eq!(
            turn_repo::get_message_turn(conn, message_id)
                .unwrap()
                .unwrap()
                .turn_id,
            hit["turn_id"].as_str().unwrap()
        );
        let tools = message_repo::get_tool_calls_for_message(conn, message_id).unwrap();
        assert!(tools
            .iter()
            .any(|tool| tool.id == hit["tool_call_id"].as_str().unwrap()
                && tool
                    .output
                    .as_deref()
                    .unwrap_or("")
                    .contains(CONTEXT_SEARCH_FACT)));
    })
    .unwrap();
}

#[tokio::test]
async fn context_search_recovers_a_rare_fact_after_compaction_in_print() {
    assert_cli_context_search(false).await;
}

#[tokio::test]
async fn context_search_recovers_a_rare_fact_after_compaction_in_sdk() {
    assert_cli_context_search(true).await;
}

#[tokio::test]
async fn context_search_resume_preserves_legacy_ties_across_a_compaction_boundary() {
    // On Windows dirs::home_dir() uses the OS profile even if USERPROFILE is set.
    // Create the fixture directly below it so all project ancestors below home are ours.
    #[cfg(windows)]
    let temp = tempfile::tempdir_in(dirs::home_dir().expect("Windows profile directory")).unwrap();
    #[cfg(not(windows))]
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let project = home.join("project");
    let data = home.join("data");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&data).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    {
        let stores=deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap().open_database().unwrap();
        deepseek_code_core::runtime::SessionStore::new(stores.clone())
            .create_with_id(&project, "fixture-model", "cli-runtime", &session)
            .unwrap();
        stores.shard_sql(&session, |db| {
        for (id, role, content, boundary) in [
            ("z-old", "user", "legacy-marker original", false),
            ("m-boundary", "user", "Stored summary", true),
            (
                "a-reply",
                "assistant",
                "legacy-marker retained reply",
                false,
            ),
        ] {
            db.execute("INSERT INTO messages(id,conversation_id,role,content,created_at,is_compact_boundary,sort_order)
                VALUES(?1,?2,?3,?4,'2026-01-01T00:00:00Z',?5,50)", rusqlite::params![id,session,role,content,boundary]).unwrap();
        }
        let mut replay = db
            .prepare("SELECT id FROM messages WHERE conversation_id=? ORDER BY sort_order")
            .unwrap();
        let ids = replay
            .query_map([&session], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, ["z-old", "m-boundary", "a-reply"]);
        db.execute("UPDATE manifest SET message_count=3,content_revision=content_revision+1",[]).unwrap();
        }).unwrap();
    }

    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                let mut seen = captured.lock().unwrap();
                seen.push(body);
                let (name, args) = match seen.len() {
                    1 => ("ToolLoad", json!({"names":["ContextSearch"]})),
                    2 => (
                        "ContextSearch",
                        json!({"query":"legacy-marker","range":"before"}),
                    ),
                    3 => (
                        "ContextSearch",
                        json!({"query":"legacy-marker","range":"after","role":"assistant"}),
                    ),
                    _ => return chat_stream(json!({"content":"finished"}), "stop"),
                };
                let delta = json!({"tool_calls":[{"index":0,"id":format!("legacy-{}",seen.len()),
                "type":"function","function":{"name":name,"arguments":args.to_string()}}]});
                chat_stream(delta, "tool_calls")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cli = query(&data, &project, &url);
    cli.env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"));
    cli.args([
        "--resume",
        &session,
        "--print",
        "Continue after the summary",
    ]);
    let output = execute(cli).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.abort();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let wire: Vec<_> = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| (&m["role"], &m["content"]))
        .collect();
    assert_eq!(wire.len(), 3, "{wire:?}");
    // The resumed session's first message keeps the frozen session context
    // before the compacted summary.
    assert_eq!(wire[0].0, &json!("user"));
    let first = wire[0].1.as_str().unwrap();
    assert!(first.ends_with("Stored summary"), "{first}");
    assert!(first.contains("<system-reminder>\n# Working environment\n"), "{first}");
    assert!(first.contains("<system-reminder>\nToday's date is "), "{first}");
    assert_eq!(
        wire[1],
        (&json!("assistant"), &json!("legacy-marker retained reply"))
    );
    assert_eq!(
        wire[2],
        (&json!("user"), &json!("Continue after the summary"))
    );
    for (body, expected) in [(&requests[2], "z-old"), (&requests[3], "a-reply")] {
        let tool = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(tool["role"], "tool");
        let result: Value = serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
        assert_eq!(result["boundary_id"], "m-boundary");
        let hits = result["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{result}");
        assert_eq!(hits[0]["message_id"], expected);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn redirected_output_config_confirmation_reports_enospc() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let status = command(&data, root.path())
        .args(["config", "set", "tools.Read.enabled", "true"])
        .stderr(full)
        .status()
        .await
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let saved: Value =
        serde_json::from_slice(&std::fs::read(data.join("config/customization.json")).unwrap())
            .unwrap();
    assert_eq!(saved["tool_loading"]["Read"]["enabled"], true);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cold_sessions_list_reads_the_directory_without_opening_history() -> anyhow::Result<()> {
    use std::{
        ffi::CString,
        io::Read,
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
    };

    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("session.json");
    let id = uuid::Uuid::new_v4().to_string();
    let at = "2026-11-20T00:00:00Z";
    std::fs::write(
        &source,
        json!({
            "id": id, "name": "Directory fixture", "created_at": at, "updated_at": at,
            "messages": [{"role":"user", "content":"first question", "timestamp":at}]
        })
        .to_string(),
    )?;
    let mut import = command(&data, temp.path());
    import
        .current_dir(temp.path())
        .args(["sessions", "import-legacy"])
        .arg(source);
    let result = execute(import).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let shard = data.join("data/sessions").join(&id).join("session.db");
    let path = CString::new(shard.as_os_str().as_bytes())?;
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    anyhow::ensure!(fd >= 0, std::io::Error::last_os_error());
    let mut watch = unsafe { std::fs::File::from_raw_fd(fd) };
    anyhow::ensure!(
        unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN) } >= 0,
        std::io::Error::last_os_error()
    );
    let mut list = command(&data, temp.path());
    list.args(["sessions", "list"]);
    let result = execute(list).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&result.stdout)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id);
    assert_eq!(rows[0]["title"], "Directory fixture");
    let mut buf = [0; 1024];
    assert_eq!(
        watch.read(&mut buf).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(std::fs::File::open(shard)?);
    assert!(watch.read(&mut buf)? >= std::mem::size_of::<libc::inotify_event>());
    Ok(())
}
