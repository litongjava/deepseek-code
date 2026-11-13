//! Real CLI + local model mock + a fake `gh` on PATH.
//!
//! Covers the executor wiring that unit tests cannot: the `GitHub` built-in is
//! discoverable only through ToolSearch, a read runs through the normal tool
//! pipeline against a fake binary, hooks and durable tool-call rows carry their
//! own invocation identity, and a mutation is blocked by Plan mode or a deny
//! rule without ever starting `gh`. No call reaches GitHub.
#![cfg(unix)]

use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// Discover the tool, then run one real read.
    Read,
    /// Discover the tool, then attempt a mutation that must be refused.
    Mutation,
}

#[derive(Clone)]
struct Fixture {
    scenario: Scenario,
    requests: Arc<Mutex<Vec<Value>>>,
    failures: Arc<Mutex<Vec<String>>>,
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

/// Tool results in order, resolved to the call's name via the assistant turn.
/// The second element is the raw result text: a permission denial is a plain
/// message, not a JSON object, and must not be mistaken for a missing result.
fn tool_results(body: &Value) -> Vec<(String, String)> {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let mut names = std::collections::HashMap::new();
    for message in &messages {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str())
            {
                names.insert(id.to_owned(), name.to_owned());
            }
        }
    }
    messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| {
            let id = message["tool_call_id"].as_str()?;
            let name = names.get(id)?.clone();
            Some((
                name,
                message["content"].as_str().unwrap_or_default().to_owned(),
            ))
        })
        .collect()
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

async fn respond(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    state.requests.lock().unwrap().push(body.clone());
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scripted(&state, &body)));
    match outcome {
        Ok(frames) => stream(frames),
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                })
                .unwrap_or_else(|| "fixture assertion failed".to_owned());
            state.failures.lock().unwrap().push(message.clone());
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":{"message":message}})),
            )
                .into_response()
        }
    }
}

fn scripted(state: &Fixture, body: &Value) -> Vec<Value> {
    let results = tool_results(body);
    match results.len() {
        // Nothing ran yet: the tool must be on demand, never preloaded.
        0 => {
            let names = tool_names(body);
            assert!(
                !names.iter().any(|name| name == "GitHub"),
                "GitHub must start on demand: {names:?}"
            );
            assert!(names.iter().any(|name| name == "ToolSearch"));
            calls_frame(&[("search-1", "ToolSearch", json!({"query":"select:GitHub"}))])
        }
        // Discovery finished: assert the schema contract, then act on the
        // scenario. Loading the tool is not authorization to write.
        1 => {
            assert_eq!(results[0].0, "ToolSearch", "{results:?}");
            let search: Value = serde_json::from_str(&results[0].1).unwrap_or_else(|error| {
                panic!("ToolSearch result is not JSON ({error}): {}", results[0].1)
            });
            assert_eq!(search["success"], true, "{search}");
            let loaded: Vec<&str> = search["loaded_tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            assert_eq!(loaded, ["GitHub"], "{search}");
            let schema = tool_parameters(body, "GitHub");
            let operations: Vec<&str> = schema["properties"]["operation"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(
                operations,
                [
                    "repo_view",
                    "issue_list",
                    "issue_view",
                    "issue_create",
                    "issue_comment",
                    "pr_list",
                    "pr_view",
                    "pr_create",
                    "pr_comment",
                    "pr_merge",
                ],
                "{schema}"
            );
            for forbidden in ["token", "url", "host", "confirm", "command", "args"] {
                assert!(
                    schema["properties"].get(forbidden).is_none(),
                    "{forbidden} must not be exposed: {schema}"
                );
            }
            assert_eq!(schema["additionalProperties"], false, "{schema}");
            match state.scenario {
                Scenario::Read => calls_frame(&[(
                    "gh-call",
                    "GitHub",
                    json!({"operation":"repo_view","repo":"octo/demo"}),
                )]),
                Scenario::Mutation => calls_frame(&[(
                    "gh-mutation",
                    "GitHub",
                    json!({
                        "operation":"issue_create",
                        "repo":"octo/demo",
                        "title":"Must not be created",
                        "body":"a body that must never leave this machine"
                    }),
                )]),
            }
        }
        // The call ran and returned. A read is normalized JSON; a blocked
        // mutation is a refusal the model can see.
        2 => {
            let (name, result) = &results[1];
            assert_eq!(name, "GitHub");
            match state.scenario {
                Scenario::Read => {
                    let result: Value = serde_json::from_str(result).unwrap_or_else(|error| {
                        panic!("the read result is not JSON ({error}): {result}")
                    });
                    assert_eq!(result["is_error"], false, "{result}");
                    assert_eq!(result["operation"], "repo_view");
                    assert_eq!(result["repository"]["name_with_owner"], "octo/demo");
                    assert_eq!(result["repository"]["owner"], "octo");
                }
                Scenario::Mutation => {
                    // A refused call returns a plain explanation instead of a
                    // tool result, and it must never look like a success.
                    let text = result.to_lowercase();
                    assert!(
                        text.contains("denied")
                            || text.contains("not allowed")
                            || text.contains("disallowed")
                            || text.contains("plan mode"),
                        "the refusal was not explained: {result}"
                    );
                    assert!(!text.contains("\"created\":true"), "{result}");
                }
            }
            // Every tool result keeps the id the model gave that call.
            let ids: Vec<&str> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "tool")
                .filter_map(|message| message["tool_call_id"].as_str())
                .collect();
            let expected = if state.scenario == Scenario::Read {
                "gh-call"
            } else {
                "gh-mutation"
            };
            assert_eq!(ids, ["search-1", expected], "{ids:?}");
            text_frames("github fixture complete")
        }
        _ => panic!("unexpected tool results: {results:?}"),
    }
}

/// A fake `gh` that records every argv and emits a repository object.
fn fake_gh(root: &std::path::Path) -> std::path::PathBuf {
    let bin = root.join("ghbin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("gh");
    let log = root.join("gh-argv.log");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{log}'\ncat > /dev/null\nprintf '%s' '{payload}'\n",
            log = log.display(),
            payload = r#"{"nameWithOwner":"octo/demo","name":"demo","owner":{"login":"octo"},"description":"Fake repo","url":"https://github.com/octo/demo","defaultBranchRef":{"name":"main"},"isPrivate":false,"isFork":false,"stargazerCount":3,"visibility":"PUBLIC","pushedAt":"2026-01-01T00:00:00Z","hasIssuesEnabled":true}"#,
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&program).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&program, permissions).unwrap();
    bin
}

fn hook_journal(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[derive(Debug)]
struct StoredCall {
    id: String,
    input: String,
    output: String,
    is_error: i64,
}

/// The durable tool-call rows for one tool in one session.
fn stored_calls(data: &std::path::Path, session: &str, tool: &str) -> Vec<StoredCall> {
    let conn = rusqlite::Connection::open(data.join("deepseek_code.db")).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT tc.id, tc.input, tc.output, tc.is_error FROM tool_calls tc \
             JOIN messages m ON m.id = tc.message_id \
             WHERE m.conversation_id = ?1 AND tc.name = ?2 \
             ORDER BY m.sort_order, tc.sort_order",
        )
        .unwrap();
    stmt.query_map([session, tool], |row| {
        Ok(StoredCall {
            id: row.get(0)?,
            input: row.get(1)?,
            output: row.get(2)?,
            is_error: row.get(3)?,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

struct CliRun {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs the CLI while **draining both pipes concurrently**. Waiting for exit
/// before reading would deadlock as soon as stream-json output fills the pipe
/// buffer, so each pipe is read by its own task from the start.
async fn run_cli(
    data: &std::path::Path,
    root: &std::path::Path,
    session: &str,
    path: &std::path::Path,
    mode: &[&str],
    prompt: &str,
    extra: &[&str],
    fixture: &Fixture,
) -> Result<CliRun, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(data)
        .arg("--path")
        .arg(root)
        .args(["--provider", "fixture", "--model", "fixture-model"])
        .args(mode)
        .args(extra)
        .args(["--session-id", session, prompt])
        .env("PATH", path)
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
    let mut stdout = child.stdout.take().expect("CLI stdout");
    let mut stderr = child.stderr.take().expect("CLI stderr");
    let out_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer).await;
        buffer
    });
    let err_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer).await;
        buffer
    });
    let started = std::time::Instant::now();
    let outcome: Result<ExitStatus, String> = loop {
        if let Some(message) = fixture.failures.lock().unwrap().first().cloned() {
            let _ = child.start_kill();
            let _ = child.wait().await;
            break Err(message);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.start_kill();
                break Err(error.to_string());
            }
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            break Err("CLI fixture timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    match outcome {
        Ok(status) => Ok(CliRun {
            status,
            stdout,
            stderr,
        }),
        Err(message) => Err(format!(
            "fixture failed: {message}\nCLI stdout:\n{}\nCLI stderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )),
    }
}

/// One fixture: data dir, workspace, model server and a PATH that finds the
/// fake `gh` before anything else.
struct Harness {
    root: tempfile::TempDir,
    data: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
    fixture: Fixture,
    path: String,
}

async fn harness(scenario: Scenario) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let fixture = Fixture {
        scenario,
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
    std::fs::write(
        data.join("settings.json"),
        json!({"hooks":{
            "PreToolUse":[{"matcher":"GitHub","hooks":[{"type":"command","shell":"sh","command":"cat >> github-hooks.jsonl"}]}],
            "PostToolUse":[{"matcher":"GitHub","hooks":[{"type":"command","shell":"sh","command":"cat >> github-hooks.jsonl"}]}]
        }})
        .to_string(),
    )
    .unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ghbin = fake_gh(root.path());
    let path = format!(
        "{}:{}",
        ghbin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Harness {
        root,
        data,
        server,
        fixture,
        path,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn github_read_runs_through_the_executor_with_hooks_and_history() {
    let harness = harness(Scenario::Read).await;
    let session = uuid::Uuid::new_v4().to_string();
    let run = run_cli(
        &harness.data,
        harness.root.path(),
        &session,
        std::path::Path::new(&harness.path),
        &["--permission-mode", "bypassPermissions"],
        "read the repository",
        &[],
        &harness.fixture,
    )
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // The fake gh really ran, with an argument vector (no shell).
    let argv = std::fs::read_to_string(harness.root.path().join("gh-argv.log")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(argv[0], "repo", "{argv:?}");
    assert_eq!(argv[1], "view", "{argv:?}");
    assert_eq!(argv[2], "octo/demo", "{argv:?}");
    assert_eq!(argv[3], "--json", "{argv:?}");

    // Hooks fired for the tool with its own invocation identity.
    let journal = hook_journal(&harness.root.path().join("github-hooks.jsonl"));
    assert_eq!(journal.len(), 2, "{journal:?}");
    assert_eq!(journal[0]["hook_event_name"], "PreToolUse", "{journal:?}");
    assert_eq!(journal[1]["hook_event_name"], "PostToolUse", "{journal:?}");
    for entry in &journal {
        assert_eq!(entry["tool_name"], "GitHub", "{entry}");
        assert_eq!(entry["tool_input"]["operation"], "repo_view", "{entry}");
    }
    let pre_id = journal[0]["tool_use_id"].as_str().unwrap();
    assert_eq!(pre_id, journal[1]["tool_use_id"], "{journal:?}");
    assert!(
        pre_id.starts_with("inv-"),
        "the hook saw a provider id instead of the occurrence id: {pre_id}"
    );
    assert_ne!(pre_id, "gh-call", "{journal:?}");

    // The durable tool-call row is independent of the model's own call id and
    // matches the id the hooks saw.
    let rows = stored_calls(&harness.data, &session, "GitHub");
    assert_eq!(rows.len(), 1, "{rows:?}");
    // The stored id is the conversation id plus the invocation id; the
    // invocation part is what the hooks saw.
    assert!(
        rows[0].id.ends_with(pre_id),
        "stored id {} does not end with the hook id {pre_id}",
        rows[0].id
    );
    assert_ne!(rows[0].id, "gh-call", "{rows:?}");
    assert_eq!(rows[0].is_error, 0, "{rows:?}");
    assert!(
        rows[0].input.contains("repo_view") && rows[0].input.contains("octo/demo"),
        "{rows:?}"
    );
    assert!(
        rows[0].output.contains("octo/demo"),
        "the stored output is not the normalized result: {rows:?}"
    );

    assert_eq!(harness.fixture.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn plan_mode_blocks_a_github_mutation_without_running_gh() {
    let harness = harness(Scenario::Mutation).await;
    let session = uuid::Uuid::new_v4().to_string();
    let run = run_cli(
        &harness.data,
        harness.root.path(),
        &session,
        std::path::Path::new(&harness.path),
        &["--permission-mode", "plan"],
        "create an issue",
        &[],
        &harness.fixture,
    )
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Plan mode never started the CLI: no argv file, so no request could have
    // been made.
    assert!(
        !harness.root.path().join("gh-argv.log").exists(),
        "Plan mode ran gh for a mutation"
    );
    // The refusal is durable and marked as an error.
    let rows = stored_calls(&harness.data, &session, "GitHub");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].is_error, 1, "{rows:?}");
    assert!(rows[0].input.contains("issue_create"), "{rows:?}");
}

#[tokio::test]
async fn a_deny_rule_blocks_a_github_mutation_even_after_the_tool_loads() {
    let harness = harness(Scenario::Mutation).await;
    let session = uuid::Uuid::new_v4().to_string();
    let run = run_cli(
        &harness.data,
        harness.root.path(),
        &session,
        std::path::Path::new(&harness.path),
        &["--permission-mode", "ask"],
        "create an issue",
        &["--disallowed-tools", "GitHub"],
        &harness.fixture,
    )
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    // The tool was still discoverable (loading is not authorization), and the
    // deny rule stopped it before any process existed.
    assert!(
        !harness.root.path().join("gh-argv.log").exists(),
        "a denied tool ran gh"
    );
    let rows = stored_calls(&harness.data, &session, "GitHub");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].is_error, 1, "{rows:?}");
}
