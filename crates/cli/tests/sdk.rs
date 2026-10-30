//! End-to-end SDK tests run the actual CLI process against local model servers.
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
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

fn permission_hook_settings(root: &Path, case: &str) {
    let response = if case == "no-decision" {json!({})} else {json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"file_path":"hook.txt","content":"HOOK_WINNER"},"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]}}})};
    let seconds = if case == "host" {3} else {1};
    let command = if cfg!(windows) {
        format!("$v=[Console]::In.ReadToEnd() | ConvertFrom-Json; if ($v.tool_use_id -ne 'write-1' -or $v.tool_name -ne 'Write') {{exit 7}}; [IO.File]::WriteAllText('hook-started','yes'); Start-Sleep -Seconds {seconds}; [IO.File]::WriteAllText('hook-finished','yes'); [Console]::Out.Write('{}')", response.to_string().replace('\'', "''"))
    } else { format!("read input; printf yes > hook-started; sleep {seconds}; printf yes > hook-finished; printf '%s' '{}'", response.to_string().replace('\'', "'\\''")) };
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/settings.json"), json!({"hooks":{"PermissionRequest":[{"matcher":"Write","hooks":[{"type":"command","shell":if cfg!(windows) {"powershell"} else {"sh"},"command":command}]}]}}).to_string()).unwrap();
}

#[tokio::test]
async fn permission_request_hooks_race_sdk_and_cancel_only_the_losing_request() {
    for anthropic in [false, true] {
        for winner in ["hook", "host", "no-decision"] {
            let root = tempfile::tempdir().unwrap();
            permission_hook_settings(root.path(), winner);
            let server = Server::start(anthropic, Mode::Approval).await;
            let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
            client.send(user("Write the fixture file")).await;
            let request = client.frame("control_request").await;
            assert_eq!(request["request"]["tool_use_id"], "write-1");
            assert!(!root.path().join("hook-finished").exists(), "hook must not delay SDK prompt publication");
            if winner == "hook" {
                let cancel = client.frame("control_cancel_request").await;
                assert_eq!(cancel["request_id"], request["request_id"]);
                // A late host response carries a conflicting persistent patch. It must lose.
                client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":"allow","updatedInput":{"file_path":"late.txt","content":"LATE"},"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"deny","rules":[{"toolName":"Write"}]}]}}})).await;
            } else {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let marker = if winner=="host" {"hook-started"} else {"hook-finished"};
                    while !root.path().join(marker).exists() {tokio::time::sleep(Duration::from_millis(10)).await;}
                }).await.unwrap();
                client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":"allow","updatedInput":{"file_path":"host.txt","content":"HOST_WINNER"}}}})).await;
            }
            let result = client.result().await;
            assert_eq!(result["is_error"], false, "{anthropic} {winner}: {result}");
            assert_eq!(root.path().join("hook.txt").exists(), winner=="hook");
            assert_eq!(root.path().join("host.txt").exists(), winner!="hook");
            assert!(!root.path().join("approval.txt").exists());
            assert!(!root.path().join("late.txt").exists());
            // The winning hook's session patch survives; losing hooks cannot install it.
            client.send(user("Write again")).await;
            if winner != "hook" {
                let next = client.frame("control_request").await;
                assert_ne!(next["request_id"], request["request_id"]);
                client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":next["request_id"],"response":{"behavior":"deny"}}})).await;
            }
            let next = client.result().await;
            assert_eq!(next["is_error"], false, "{next}");
            assert_eq!(root.path().join("approval.txt").exists(), winner=="hook");
            client.finish().await;
            if winner == "host" {
                tokio::time::sleep(Duration::from_millis(3200)).await;
                assert!(!root.path().join("hook-finished").exists(), "losing hook child must be killed");
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Text,
    RuntimeAgent,
    Approval,
    DelayedApproval,
    Question,
    Plan,
    Truncated,
    TruncatedThinking,
    TruncatedTool,
    ReadThenTruncated,
    ReadThenText,
    SlowText,
    Tool,
    SchemaGood,
    SchemaBad,
    SchemaRejected,
}
#[derive(Clone)]
struct Fixture {
    anthropic: bool,
    mode: Mode,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn respond(State(state): State<Fixture>, Json(body): Json<Value>) -> Response {
    let count = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(body.clone());
        requests.len()
    };
    if body["stream"].as_bool() != Some(true) {
        if state.anthropic {
            return Json(json!({"id":"fixture","type":"message","role":"assistant","content":[{"type":"text","text":"suggested-next"}],"model":"fixture","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})).into_response();
        }
        return Json(json!({"id":"fixture","object":"chat.completion","created":0,"model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"suggested-next"},"finish_reason":"stop"}]})).into_response();
    }
    if matches!(state.mode, Mode::DelayedApproval) && count == 1 { tokio::time::sleep(Duration::from_secs(2)).await; }
    if matches!(state.mode, Mode::SlowText) && count == 1 { tokio::time::sleep(Duration::from_secs(2)).await; }
    if matches!(state.mode, Mode::SchemaRejected) {
        return (
            StatusCode::BAD_REQUEST,
            "fixture: structured output unsupported",
        )
            .into_response();
    }
    let text = match state.mode {
        Mode::SchemaGood => r#"{"answer":42}"#.into(),
        Mode::SchemaBad => r#"{"answer":"wrong"}"#.into(),
        _ => format!("reply-{count}"),
    };
    let truncated = matches!(
        state.mode,
        Mode::Truncated | Mode::TruncatedTool | Mode::TruncatedThinking
    ) || matches!(state.mode, Mode::ReadThenTruncated) && count > 1;
    let tool = matches!(state.mode, Mode::RuntimeAgent) && count <= 2
        || matches!(state.mode, Mode::Plan) && count == 1
        || matches!(state.mode, Mode::Tool | Mode::TruncatedTool)
        || matches!(state.mode, Mode::ReadThenTruncated) && count == 1
        || matches!(state.mode, Mode::ReadThenText) && count == 1
        || matches!(state.mode, Mode::Approval | Mode::DelayedApproval | Mode::Question) && count % 2 == 1;
    let stream = if state.anthropic {
        let mut frames = vec![
            json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),
        ];
        if tool {
            let (id, name, args) = if matches!(state.mode, Mode::RuntimeAgent) {
                if count == 1 {
                    (
                        "agent-1",
                        "Agent",
                        json!({"agent_type":"sdk-child","prompt":"Complete the child task","run_in_background":false}),
                    )
                } else {
                    (
                        "write-1",
                        "Write",
                        json!({"file_path":"child.txt","content":"CHILD_WRITE"}),
                    )
                }
            } else if matches!(state.mode, Mode::Question) {
                (
                    "question-1",
                    "AskUserQuestion",
                    json!({"question":"Which language?","description":"Choose the implementation language","multiSelect":true,"options":[{"label":"Rust","description":"Memory safety"},{"label":"Go","description":"Fast compilation"}]}),
                )
            } else if matches!(state.mode, Mode::Plan) {
                (
                    "plan-1",
                    "ExitPlanMode",
                    json!({"plan":"APPROVED_PLAN_SENTINEL"}),
                )
            } else if matches!(state.mode, Mode::Approval | Mode::DelayedApproval) {
                (
                    "write-1",
                    "Write",
                    json!({"file_path":"approval.txt","content":"ACTUAL_APPROVED_WRITE"}),
                )
            } else {
                ("read-1", "Read", json!({"file_path":"fixture.txt"}))
            };
            frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args.to_string()}})]);
        } else if matches!(state.mode, Mode::TruncatedThinking) {
            frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"PARTIAL_THINKING"}})]);
        } else {
            frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})]);
        }
        frames.extend([json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":if truncated {"max_tokens"} else if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":4}}),json!({"type":"message_stop"})]);
        frames
            .into_iter()
            .map(|frame| {
                format!(
                    "event: {}\ndata: {frame}\n\n",
                    frame["type"].as_str().unwrap()
                )
            })
            .collect::<String>()
    } else {
        let delta = if tool {
            if matches!(state.mode, Mode::RuntimeAgent) {
                let (name, args) = if count == 1 {
                    (
                        "Agent",
                        json!({"agent_type":"sdk-child","prompt":"Complete the child task","run_in_background":false}),
                    )
                } else {
                    (
                        "Write",
                        json!({"file_path":"child.txt","content":"CHILD_WRITE"}),
                    )
                };
                json!({"tool_calls":[{"index":0,"id":if count==1 {"agent-1"}else{"write-1"},"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
            } else if matches!(state.mode, Mode::Question) {
                json!({"tool_calls":[{"index":0,"id":"question-1","type":"function","function":{"name":"AskUserQuestion","arguments":json!({"question":"Which language?","description":"Choose the implementation language","multiSelect":true,"options":[{"label":"Rust","description":"Memory safety"},{"label":"Go","description":"Fast compilation"}]}).to_string()}}]})
            } else if matches!(state.mode, Mode::Plan) {
                json!({"tool_calls":[{"index":0,"id":"plan-1","type":"function","function":{"name":"ExitPlanMode","arguments":json!({"plan":"APPROVED_PLAN_SENTINEL"}).to_string()}}]})
            } else if matches!(state.mode, Mode::Approval | Mode::DelayedApproval) {
                json!({"tool_calls":[{"index":0,"id":"write-1","type":"function","function":{"name":"Write","arguments":json!({"file_path":"approval.txt","content":"ACTUAL_APPROVED_WRITE"}).to_string()}}]})
            } else {
                json!({"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"fixture.txt\"}"}}]})
            }
        } else if matches!(state.mode, Mode::TruncatedThinking) {
            json!({"reasoning_content":"PARTIAL_THINKING"})
        } else {
            json!({"content":text})
        };
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":if truncated {"length"} else if tool {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":10,"completion_tokens":4}})
        )
    };
    ([("content-type", "text/event-stream")], stream).into_response()
}

struct Server {
    url: String,
    fixture: Fixture,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(anthropic: bool, mode: Mode) -> Self {
        let fixture = Fixture {
            anthropic,
            mode,
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                if anthropic {
                    "/v1/messages"
                } else {
                    "/v1/chat/completions"
                },
                post(respond),
            )
            .with_state(fixture.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, fixture, task }
    }
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
}
impl Client {
    async fn frame(&mut self, kind: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("CLI ended before expected SDK frame");
                let frame: Value = serde_json::from_str(&line).unwrap();
                assert!(
                    frame["type"] != "result" || kind == "result",
                    "Unexpected terminal result while waiting for {kind}: {frame}"
                );
                if frame["type"] == kind {
                    return frame;
                }
            }
        })
        .await
        .unwrap()
    }
    fn start(root: &Path, server: &Server, extra: &[&str]) -> Self {
        std::fs::write(root.join("fixture.txt"), "SDK_READ_SENTINEL").unwrap();
        let mut command = Command::new(std::env::var_os("CLAUDE_SDK_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()));
        command
            .args([
                "--data-dir",
                root.join("data").to_str().unwrap(),
                "--path",
                root.to_str().unwrap(),
                "--base-url",
                &server.url,
                "--api-format",
                if server.fixture.anthropic {
                    "anthropic"
                } else {
                    "openai"
                },
                "--api-key-env",
                "CLAUDE_SDK_FIXTURE_KEY",
                "--model",
                "fixture-model",
                "--output-format",
                "stream-json",
                "--permission-mode",
                "accept-edits",
                "--print",
                "--input-format",
                "stream-json",
            ])
            .args(extra);
        command
            .env("CLAUDE_SDK_FIXTURE_KEY", "fixture-only")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "API_BASE_URL",
            "ANTHROPIC_BASE_URL",
            "OPENAI_BASE_URL",
            "CLAUDE_MODEL",
            "RUST_LOG",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            input,
            output,
        }
    }
    async fn send(&mut self, value: Value) {
        self.raw(&format!("{value}\n")).await;
    }
    async fn raw(&mut self, text: &str) {
        let input = self.input.as_mut().unwrap();
        input.write_all(text.as_bytes()).await.unwrap();
        input.flush().await.unwrap();
    }
    async fn result(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("CLI ended before a result");
                let frame: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("Non-JSON stdout: {line}"));
                if frame["type"] == "result" {
                    return frame;
                }
            }
        })
        .await
        .expect("CLI did not emit a result while stdin stayed open")
    }
    async fn finish(self) -> (std::process::ExitStatus, Vec<Value>) {
        let (status, results, _) = self.finish_with_transcript().await;
        (status, results)
    }
    async fn finish_with_transcript(mut self) -> (std::process::ExitStatus, Vec<Value>, String) {
        self.input.take();
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut results = Vec::new();
            let mut transcript = String::new();
            while let Some(line) = self.output.next_line().await.unwrap() {
                transcript.push_str(&line);
                transcript.push('\n');
                let frame: Value = serde_json::from_str(&line).unwrap();
                if frame["type"] == "result" {
                    results.push(frame);
                }
            }
            (self.child.wait().await.unwrap(), results, transcript)
        })
        .await
        .expect("CLI failed to exit after stdin EOF")
    }
}

fn user(text: &str) -> Value {
    json!({"type":"user","message":{"role":"user","content":text}})
}

#[tokio::test]
async fn output_limit_is_incomplete_retains_partial_and_never_replays_tools() {
    for anthropic in [false, true] {
        for mode in [
            Mode::Truncated,
            Mode::TruncatedThinking,
            Mode::TruncatedTool,
            Mode::ReadThenTruncated,
        ] {
            let server = Server::start(anthropic, mode).await;
            let root = tempfile::tempdir().unwrap();
            let mut client = Client::start(root.path(), &server, &[]);
            client.send(user("Do real work")).await;
            let result = client.result().await;
            assert_eq!(result["is_error"], true, "{result}");
            assert!(
                result["error"]
                    .as_str()
                    .unwrap()
                    .contains("output token limit"),
                "{result}"
            );
            let (status, extra) = client.finish().await;
            assert!(!status.success());
            assert!(extra.is_empty());
            // Every cut-off response is continued three times before the
            // turn reports the output limit.
            let expected = if matches!(mode, Mode::ReadThenTruncated) {
                2
            } else {
                1
            } + 3;
            assert_eq!(server.fixture.requests.lock().unwrap().len(), expected);
            let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
            let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
            let id = result["session_id"].as_str().unwrap();
            let history = store
                .model_history(
                    id,
                    &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
                )
                .unwrap();
            let text = serde_json::to_string(&history).unwrap();
            assert_eq!(
                text.matches("SDK_READ_SENTINEL").count(),
                if matches!(mode, Mode::ReadThenTruncated) {
                    1
                } else {
                    0
                }
            );
            if matches!(mode, Mode::TruncatedThinking) {
                let retained = store
                    .db
                    .with_conn(|conn| {
                        deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, id)
                    })
                    .unwrap()
                    .unwrap();
                assert!(retained
                    .iter()
                    .any(|m| m.thinking.as_deref().is_some_and(|t| t.contains("PARTIAL_THINKING"))));
            } else if !matches!(mode, Mode::TruncatedTool) {
                assert!(text.contains(&format!("reply-{expected}")), "{text}");
            }
            let snapshots = store
                .db
                .with_conn(|conn| -> anyhow::Result<Vec<Value>> {
                    let messages =
                        deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, id)?;
                    Ok(messages
                        .iter()
                        .filter(|m| m.role == "assistant")
                        .map(|m| {
                            deepseek_code_core::db::message_repo::get_request_snapshots(conn, &m.id)
                                .unwrap()
                        })
                        .collect())
                })
                .unwrap()
                .unwrap();
            assert!(
                snapshots.iter().any(|s| s
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["status"] == "incomplete")),
                "{snapshots:?}"
            );
        }
    }
}

#[tokio::test]
async fn one_million_suffix_is_selection_metadata_not_api_model() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(
            root.path(),
            &server,
            &["--model", "deepseek-v4.1-flash[1m]"],
        );
        client.send(user("Check suffix")).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        client.send(json!({"type":"control_request","request_id":"model","request":{"subtype":"set_model","model":"deepseek-v4.1-flash[1m]"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client.send(user("Still selected")).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(client.finish().await.0.success());
        assert!(server
            .fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["model"] == "deepseek-v4.1-flash"));
        let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        assert_eq!(
            store
                .resume(result["session_id"].as_str().unwrap())
                .unwrap()
                .model
                .as_deref(),
            Some("deepseek-v4.1-flash[1m]")
        );
        store
            .db
            .with_conn(|conn| -> anyhow::Result<()> {
                let messages = deepseek_code_core::db::message_repo::get_messages_by_conversation(
                    conn,
                    result["session_id"].as_str().unwrap(),
                )?;
                for message in messages.iter().filter(|m| m.role == "assistant") {
                    let snapshots =
                        deepseek_code_core::db::message_repo::get_request_snapshots(conn, &message.id)?;
                    assert!(!snapshots.as_array().unwrap().is_empty());
                    for snapshot in snapshots.as_array().unwrap() {
                        assert_eq!(snapshot["contextWindow"], 1_000_000, "{snapshot}");
                    }
                }
                Ok(())
            })
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn sdk_question_answers_reach_model_and_history_without_rewriting_questions() {
    for (outcome, anthropic) in [
        ("answer", false),
        ("answer", true),
        ("changed", false),
        ("deny", false),
        ("interrupt", false),
        ("eof", false),
    ] {
        let server = Server::start(anthropic, Mode::Question).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("Ask my preference")).await;
        let request = client.frame("control_request").await;
        assert_eq!(request["request"]["tool_name"], "AskUserQuestion");
        assert_eq!(request["request"]["tool_use_id"], "question-1");
        assert_eq!(
            request["request"]["input"]["questions"][0]["multiSelect"],
            true
        );
        assert_eq!(
            request["request"]["input"]["questions"][0]["options"][0]["description"],
            "Memory safety"
        );
        assert_eq!(
            request["request"]["description"],
            "Choose the implementation language"
        );
        if outcome == "eof" {
            let (status, results) = client.finish().await;
            assert!(!status.success());
            assert_eq!(results.len(), 1);
            assert_eq!(results[0]["is_error"], true);
            continue;
        }
        if outcome == "interrupt" {
            client.send(json!({"type":"control_request","request_id":"cancel-question","request":{"subtype":"interrupt"}})).await;
            client.frame("control_response").await;
        } else {
            let mut updated = request["request"]["input"].clone();
            updated["answers"] = json!({"Which language?":"Rust 中文😀 and custom free text"});
            if outcome == "changed" {
                updated["questions"][0]["question"] = json!("Injected replacement");
            }
            client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":if outcome=="deny" {"deny"} else {"allow"},"updatedInput":updated}}})).await;
        }
        let result = client.result().await;
        assert_eq!(result["is_error"], outcome != "answer", "{result}");
        let (status, _) = client.finish().await;
        assert_eq!(status.success(), outcome == "answer");
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), if outcome == "answer" { 2 } else { 1 });
        if outcome == "answer" {
            assert!(requests[1]["messages"]
                .to_string()
                .contains("Rust 中文😀 and custom free text"));
            let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
            let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
            let history = store
                .model_history(
                    result["session_id"].as_str().unwrap(),
                    &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
                )
                .unwrap();
            assert!(serde_json::to_string(&history)
                .unwrap()
                .contains("Rust 中文😀 and custom free text"));
        }
    }
}

#[tokio::test]
async fn embedded_images_reach_both_providers_and_survive_history_and_fork() {
    const PNG:&str="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=";
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let mut config = deepseek_code_core::config::AppConfig::default();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        config.providers.push(serde_json::from_value(json!({"id":"vision-fixture","name":"Vision fixture","provider_type":"openai","base_url":server.url,"enabled":true,"is_default":false,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"fixture-model","enabled":true,"supports_vision":true,"supports_tools":true,"supports_text":true,"supports_streaming":true}]})).unwrap());
        std::fs::write(
            paths.data_dir.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"Before image"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},{"type":"text","text":"After image"}]}})).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        client.send(user("Remember that image")).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(client.finish().await.0.success());
        let requests = server.fixture.requests.lock().unwrap();
        for request in requests.iter() {
            assert!(request["messages"].to_string().contains(PNG));
            // Anthropic requests merge adjacent user messages (e.g. injected memories),
            // so locate the image turn by its text instead of assuming it starts the message.
            let (content, start) = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user" && m["content"].is_array())
                .find_map(|m| {
                    let blocks = m["content"].as_array().unwrap();
                    blocks
                        .iter()
                        .position(|b| b["text"] == "Before image")
                        .map(|i| (blocks, i))
                })
                .unwrap();
            assert_eq!(content[start + 2]["text"], "After image");
            if anthropic {
                assert_eq!(content[start + 1]["source"]["data"], PNG);
            } else {
                assert!(content[start + 1]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .ends_with(PNG));
            }
        }
        drop(requests);
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let id = result["session_id"].as_str().unwrap();
        let fork = store.fork(id).unwrap();
        let history = store
            .model_history(
                &fork,
                &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
            )
            .unwrap();
        assert!(history[0].to_string().contains(PNG));
        assert_eq!(history[0]["content"][2]["text"], "After image");
    }
}

#[tokio::test]
async fn controls_replay_and_live_permission_decisions_use_actual_engine() {
    for (allow, mutate) in [(false, false), (true, false), (true, true)] {
        let server = Server::start(false, Mode::Approval).await;
        let root = tempfile::tempdir().unwrap();
        // initialize acknowledges registration before SessionStart finishes.
        // Hold startup open in one case to exercise the busy control response.
        if !allow {
            std::fs::create_dir_all(root.path().join("data")).unwrap();
            std::fs::write(root.path().join("startup.cjs"), "const fs=require('fs');fs.writeFileSync('startup-entered','1');const timer=setInterval(()=>{if(fs.existsSync('startup-release')){clearInterval(timer);console.log('{}')}},10);").unwrap();
            std::fs::write(root.path().join("data/settings.json"), json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"node startup.cjs"}]}]}}).to_string()).unwrap();
        }
        let mut client = Client::start(
            root.path(),
            &server,
            &["--permission-mode", "ask", "--replay-user-messages"],
        );
        client.send(json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        if !allow {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !root.path().join("startup-entered").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        let mut saw_startup_busy = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                client.send(json!({"type":"control_request","request_id":"model","request":{"subtype":"set_model","model":"fixture-model"}})).await;
                let reply = client.frame("control_response").await;
                assert_eq!(reply["response"]["request_id"], "model", "{reply}");
                if reply["response"]["subtype"] == "success" { break; }
                assert_eq!(reply["response"]["error"], "Control changes require completed session startup", "{reply}");
                saw_startup_busy = true;
                if !allow { std::fs::write(root.path().join("startup-release"), "1").unwrap(); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        if !allow {
            assert!(saw_startup_busy);
        }
        let mut message = user("Write approval.txt");
        message["uuid"] = json!("retained-user-uuid");
        client.send(message).await;
        assert_eq!(client.frame("user").await["uuid"], "retained-user-uuid");
        let permission = client.frame("control_request").await;
        assert_eq!(permission["request"]["tool_name"], "Write");
        assert_eq!(permission["request"]["tool_use_id"], "write-1");
        assert!(!root.path().join("approval.txt").exists());
        let mut updated = permission["request"]["input"].clone();
        if mutate {
            updated["content"] = json!("SDK reviewed replacement");
        }
        client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":permission["request_id"],"response":{"behavior":if allow {"allow"} else {"deny"},"updatedInput":updated}}})).await;
        assert_eq!(client.result().await["is_error"], false);
        assert_eq!(root.path().join("approval.txt").exists(), allow);
        if allow {
            assert_eq!(
                std::fs::read_to_string(root.path().join("approval.txt")).unwrap(),
                if mutate {
                    "SDK reviewed replacement"
                } else {
                    "ACTUAL_APPROVED_WRITE"
                }
            );
        }
        client.send(json!({"type":"control_request","request_id":"mode","request":{"subtype":"set_permission_mode","mode":"acceptEdits"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client
            .send(user("Write with updated permission mode"))
            .await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(root.path().join("approval.txt").exists());
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn session_permission_updates_authorize_later_turn_without_another_prompt() {
    let server = Server::start(false, Mode::Approval).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
    client.send(user("Write a file")).await;
    let permission = client.frame("control_request").await;
    client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":permission["request_id"],"response":{"behavior":"allow","updatedInput":permission["request"]["input"],"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]}}})).await;
    assert_eq!(client.result().await["is_error"], false);
    std::fs::remove_file(root.path().join("approval.txt")).unwrap();
    client
        .send(user("Write the file again using the session grant"))
        .await;
    assert_eq!(client.result().await["is_error"], false);
    assert_eq!(
        std::fs::read_to_string(root.path().join("approval.txt")).unwrap(),
        "ACTUAL_APPROVED_WRITE"
    );
    assert!(client.finish().await.0.success());
}

#[tokio::test]
async fn interrupt_cancels_pending_approval_and_session_accepts_next_message() {
    let server = Server::start(false, Mode::Approval).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
    client.send(user("Write a file")).await;
    let pending = client.frame("control_request").await;
    client
        .send(
            json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}}),
        )
        .await;
    assert_eq!(
        client.frame("control_cancel_request").await["request_id"],
        pending["request_id"]
    );
    assert_eq!(
        client.frame("control_response").await["response"]["request_id"],
        "stop"
    );
    let interrupted = client.result().await;
    assert_eq!(interrupted["is_error"], true);
    assert_eq!(interrupted["subtype"], "error_during_execution", "{interrupted}");
    assert!(!root.path().join("approval.txt").exists());
    client.send(user("Continue without tools")).await;
    assert_eq!(client.result().await["result"], "reply-2");
    assert!(!client.finish().await.0.success());
}

#[tokio::test]
async fn sdk_eof_during_approval_and_unsupported_initialization_fail_explicitly() {
    let server = Server::start(false, Mode::Approval).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
    client.send(json!({"type":"control_request","request_id":"unsupported","request":{"subtype":"initialize","hooks":{"WorktreeCreate":[{"hookCallbackIds":["callback"]}]}}})).await;
    let response = client.frame("control_response").await;
    assert_eq!(response["response"]["subtype"], "error");
    assert!(response["response"]["error"]
        .as_str()
        .unwrap()
        .contains("hooks"));
    client.send(user("Write pending approval")).await;
    client.frame("control_request").await;
    let (status, results) = client.finish().await;
    assert!(!status.success());
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["is_error"], true);
    assert!(!root.path().join("approval.txt").exists());
}

#[tokio::test]
async fn streaming_process_handles_two_messages_before_eof_in_one_session() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("first")).await;
        let first = client.result().await;
        assert_eq!(first["result"], "reply-1");
        assert_eq!(first["is_error"], false);
        let mut second = user("second");
        second["session_id"] = first["session_id"].clone();
        client.send(second).await;
        let second = client.result().await;
        assert_eq!(second["result"], "reply-2");
        assert_eq!(second["session_id"], first["session_id"]);
        let (status, extra) = client.finish().await;
        assert!(status.success());
        assert!(extra.is_empty(), "Duplicate result: {extra:?}");
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1]["messages"].to_string().contains("first"));
    }
}

#[tokio::test]
async fn initial_print_prompt_runs_before_streamed_messages() {
    let server = Server::start(false, Mode::Text).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["-p", "initial prompt"]);
    let first = client.result().await;
    assert_eq!(first["result"], "reply-1");
    client.send(user("streamed follow-up")).await;
    let second = client.result().await;
    assert_eq!(second["result"], "reply-2");
    assert_eq!(second["session_id"], first["session_id"]);
    let (status, extra) = client.finish().await;
    assert!(status.success());
    assert!(extra.is_empty());
}

#[tokio::test]
async fn invalid_schema_fails_before_any_model_request() {
    for schema in [
        "invalid JSON",
        r#"{"type":42}"#,
        r#"{"$ref":"https://example.invalid/schema"}"#,
    ] {
        let server = Server::start(false, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let client = Client::start(root.path(), &server, &["--json-schema", schema]);
        let (status, results) = client.finish().await;
        assert!(!status.success());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], true);
        assert!(server.fixture.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn malformed_frame_or_wrong_session_produces_one_failure() {
    for wrong_session in [false, true] {
        let server = Server::start(false, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("valid")).await;
        let first = client.result().await;
        assert_eq!(first["is_error"], false);
        if wrong_session {
            let mut frame = user("wrong");
            frame["session_id"] = json!("not-the-active-session");
            client.send(frame).await;
        } else {
            client.raw("{malformed}\n").await;
        }
        let (status, results) = client.finish().await;
        assert!(!status.success());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], true);
        assert_eq!(server.fixture.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn max_turns_executes_tool_then_fails_once_without_another_model_request() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Tool).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--max-turns", "1"]);
        client.send(user("Read fixture.txt then answer")).await;
        let (status, results, transcript) = client.finish_with_transcript().await;
        assert!(
            transcript.contains("SDK_READ_SENTINEL"),
            "The CLI must execute the actual Read tool"
        );
        assert!(!status.success());
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0]["is_error"], true);
        assert!(results[0]["error"]
            .as_str()
            .unwrap()
            .contains("Max tool iterations"));
        assert_eq!(server.fixture.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn structured_results_are_enforced_and_provider_rejection_is_not_softened() {
    let schema=json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false}).to_string();
    for anthropic in [false, true] {
        for mode in [Mode::SchemaGood, Mode::SchemaBad, Mode::SchemaRejected] {
            let server = Server::start(anthropic, mode).await;
            let root = tempfile::tempdir().unwrap();
            let mut client = Client::start(root.path(), &server, &["--json-schema", &schema]);
            client.send(user("Return an answer")).await;
            let (status, results) = client.finish().await;
            assert_eq!(results.len(), 1, "Duplicate/missing result: {results:?}");
            if matches!(mode, Mode::SchemaGood) {
                assert!(status.success());
                assert_eq!(results[0]["structured_output"], json!({"answer":42}));
            } else {
                assert!(!status.success());
                assert_eq!(results[0]["is_error"], true);
                assert!(results[0]["structured_output"].is_null());
            }
            let requests = server.fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let actual = if anthropic {
                &requests[0]["output_config"]["format"]["schema"]
            } else {
                &requests[0]["response_format"]["json_schema"]["schema"]
            };
            assert_eq!(actual, &serde_json::from_str::<Value>(&schema).unwrap());
        }
    }
}

#[tokio::test]
async fn dont_ask_control_denies_write_without_sdk_permission_request() {
    let server = Server::start(false, Mode::Approval).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--permission-mode", "dontAsk"]);
    client.send(json!({"type":"control_request","request_id":"invalid","request":{"subtype":"set_permission_mode","mode":"auto"}})).await;
    assert_eq!(
        client.frame("control_response").await["response"]["subtype"],
        "error"
    );
    client.send(user("Write a file without approval")).await;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), client.output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_ne!(
            frame["type"], "control_request",
            "dontAsk must never request SDK approval"
        );
        if frame["type"] == "result" {
            let denials = frame["permission_denials"].as_array().unwrap();
            assert_eq!(denials.len(), 1, "{frame}");
            assert_eq!(denials[0]["tool_name"], "Write");
            assert_eq!(denials[0]["tool_use_id"], "write-1");
            break;
        }
    }
    assert!(!root.path().join("approval.txt").exists());
    client.send(json!({"type":"control_request","request_id":"mode","request":{"subtype":"set_permission_mode","mode":"acceptEdits"}})).await;
    assert_eq!(
        client.frame("control_response").await["response"]["subtype"],
        "success"
    );
    client.send(user("Write with approval from mode")).await;
    let allowed = client.result().await;
    assert_eq!(allowed["is_error"], false);
    assert_eq!(allowed["permission_denials"], json!([]));
    assert!(root.path().join("approval.txt").exists());
    assert!(client.finish().await.0.success());
}

#[tokio::test]
async fn sdk_plan_clear_boundary_survives_actual_cli_process() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Plan).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "plan"]);
        client.send(user("OLD_HISTORY_SENTINEL")).await;
        let request = client.frame("control_request").await;
        let mut updated = request["request"]["input"].clone();
        let question = updated["questions"][0]["question"]
            .as_str()
            .unwrap()
            .to_owned();
        let choice = updated["questions"][0]["options"][0]["label"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(choice, deepseek_code_core::services::plan_approval::OPTIONS[0]);
        updated["answers"] = json!({question: choice});
        client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":"allow","updatedInput":updated}}})).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        assert!(client.finish().await.0.success());
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let resumed = requests[1]["messages"].to_string();
        assert!(resumed.contains("APPROVED_PLAN_SENTINEL"));
        assert!(!resumed.contains("OLD_HISTORY_SENTINEL"));
        let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let session = result["session_id"].as_str().unwrap();
        assert!(
            deepseek_code_core::services::plan_approval::latest_boundary(&store.db, session)
                .unwrap()
                .is_some()
        );
        let history = store
            .model_history(
                session,
                &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
            )
            .unwrap();
        let history = serde_json::to_string(&history).unwrap();
        assert!(history.contains("APPROVED_PLAN_SENTINEL"));
        assert!(!history.contains("OLD_HISTORY_SENTINEL"));
    }
}

#[tokio::test]
async fn auto_extension_uses_only_explicit_classifier_and_never_executes_on_failure() {
    for outcome in ["allow", "deny", "malformed", "unavailable", "timeout"] {
        let server = Server::start(false, Mode::Approval).await;
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let observed = requests.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let observed = observed.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer isolated-classifier-fixture");
                assert_eq!(body["model"], "explicit-classifier");
                assert!(body.get("tools").is_none());
                assert!(body.to_string().contains("SDK_AUTHORITY_TEXT"));
                observed.lock().unwrap().push(body);
                if outcome == "unavailable" { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                if outcome == "timeout" { tokio::time::sleep(Duration::from_millis(300)).await; }
                let content = if outcome == "malformed" { "not a classification".into() } else { json!({"decision":if outcome=="allow"{"allow"}else{"deny"},"reason_code":if outcome=="allow"{"authorized"}else{"outside_scope"}}).to_string() };
                Json(json!({"id":"classifier","object":"chat.completion","created":0,"model":"explicit-classifier","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}})).into_response()
            }
        }));
        let classifier = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let context =
            deepseek_code_core::services::session_messages::context(&root.path().join("data")).unwrap();
        std::fs::write(root.path().join("data/providers.json"), json!([{"id":"classifier-only","name":"Fixture","base_url":endpoint,"api_key":"isolated-classifier-fixture","api_format":"openai","enabled":true,"models":[{"id":"explicit-classifier","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false}]}]).to_string()).unwrap();
        deepseek_code_core::permissions::auto_config::save(
            &context,
            &deepseek_code_core::permissions::auto_config::AutoConfig {
                enabled: true,
                classifier: Some(
                    deepseek_code_core::permissions::auto_config::ClassifierSelection::Named {
                        provider_id: "classifier-only".into(),
                        model_id: "explicit-classifier".into(),
                    },
                ),
                timeout_ms: if outcome == "timeout" { 100 } else { 2000 },
                ..Default::default()
            },
        )
        .unwrap();
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "dontAsk"]);
        client.send(json!({"type":"control_request","request_id":"auto","request":{"subtype":"set_permission_mode","mode":"auto"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client.send(json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"Write approval.txt with the requested content"},{"type":"text","text":"SDK_AUTHORITY_TEXT: only edit that requested file"}]}})).await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(20), client.output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            assert_ne!(
                frame["type"], "control_request",
                "headless auto never falls back to host approval: {frame}"
            );
            if frame["type"] == "result" {
                break;
            }
        }
        assert_eq!(
            root.path().join("approval.txt").exists(),
            outcome == "allow"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(client.finish().await.0.success());
        classifier.abort();
    }
}

fn initialize_request(id: &str, mut fields: Value) -> Value {
    fields["subtype"] = json!("initialize");
    json!({"type":"control_request","request_id":id,"request":fields})
}

#[tokio::test]
async fn sdk_initialize_applies_runtime_overrides_and_reports_real_resources() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::SchemaGood).await;
        let root = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let context = deepseek_code_core::services::ServiceContext::open(
            paths.clone(),
            Arc::new(deepseek_code_core::services::EnvironmentCredentials),
        )
        .unwrap();
        deepseek_code_core::services::agents::install(&context,serde_json::from_value(json!({"id":"sdk-fixture-agent","description":"Actual configured agent","system_prompt":"Use local fixtures","tools":["Read"]})).unwrap()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        context
            .save_setting(
                &format!("cli.display.{session}"),
                &json!({"output_style":"learning"}),
            )
            .unwrap();
        let style = root.path().join(".claude/output-styles");
        std::fs::create_dir_all(&style).unwrap();
        std::fs::write(
            style.join("fixture-style.md"),
            "Describe precise test results",
        )
        .unwrap();
        let skill = root.path().join(".claude/skills/sdk-fixture-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"),"---\nname: sdk-fixture-skill\ndescription: Actual discovered SDK fixture\n---\nInspect fixture files.").unwrap();
        let mut config = deepseek_code_core::config::AppConfig::default();
        config.providers.push(serde_json::from_value(json!({"id":"sdk-fixture","name":"SDK fixture provider","provider_type":if anthropic {"anthropic"}else{"openai"},"base_url":server.url,"enabled":true,"is_default":false,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"Fixture primary","enabled":true,"supports_vision":false,"supports_tools":true,"supports_text":true,"supports_streaming":true},{"id":"fixture-second","name":"Fixture secondary","enabled":true,"supports_vision":false,"supports_tools":true,"supports_text":true,"supports_streaming":true},{"id":"fixture-disabled","name":"Hidden","enabled":false,"supports_vision":false,"supports_tools":true,"supports_text":true,"supports_streaming":true}]})).unwrap());
        let mut other = config.providers.last().unwrap().clone();
        other.id = "other-fixture".into();
        other.name = "Other fixture provider".into();
        other.models.retain(|model| model.id == "fixture-second");
        config.providers.push(other);
        std::fs::write(
            paths.data_dir.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let schema = json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false});
        let mut client = Client::start(
            root.path(),
            &server,
            &[
                "--session-id",
                &session,
                "--system-prompt",
                "CLI_BASE_REPLACED",
                "--append-system-prompt",
                "CLI_APPEND_REPLACED",
                "--json-schema",
                r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}"#,
            ],
        );
        client.send(initialize_request("initialize",json!({"systemPrompt":"SDK_BASE_中文","appendSystemPrompt":"SDK_APPEND_MARKER","jsonSchema":schema,"hooks":{},"sdkMcpServers":[],"agents":{},"promptSuggestions":false,"agentProgressSummaries":false}))).await;
        let response = client.frame("control_response").await;
        assert_eq!(response["response"]["subtype"], "success", "{response}");
        let metadata = &response["response"]["response"];
        assert!(metadata["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "help"));
        assert!(metadata["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "sdk-fixture-skill"));
        assert!(metadata["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "sdk-fixture-agent"));
        assert!(metadata["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "general"));
        assert!(metadata["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["value"] == "sdk-fixture::fixture-second"));
        assert!(metadata["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["value"] == "other-fixture::fixture-second"));
        assert!(!metadata["models"].to_string().contains("fixture-disabled"));
        assert!(!metadata.to_string().contains("fixture-only"));
        assert_eq!(metadata["output_style"], "learning");
        assert!(metadata["available_output_styles"]
            .as_array()
            .unwrap()
            .contains(&json!("fixture-style")));
        client
            .send(initialize_request(
                "repeat",
                json!({"systemPrompt":"REJECTED_REPEAT_MARKER"}),
            ))
            .await;
        let repeat = client.frame("control_response").await;
        assert_eq!(repeat["response"]["subtype"], "error");
        assert!(
            repeat["response"]["error"]
                .as_str()
                .unwrap()
                .contains("Already initialized"),
            "{repeat}"
        );
        for prompt in ["Return the schema answer", "Return it again"] {
            client.send(user(prompt)).await;
            let result = client.result().await;
            assert_eq!(result["is_error"], false, "{result}");
            assert_eq!(result["structured_output"], json!({"answer":42}));
        }
        assert!(client.finish().await.0.success());
        {
            let requests = server.fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                let system = if anthropic {
                    request["system"].to_string()
                } else {
                    request["messages"][0].to_string()
                };
                assert!(system.contains("SDK_BASE_中文") && system.contains("SDK_APPEND_MARKER"));
                assert!(
                    !system.contains("CLI_BASE_REPLACED")
                        && !system.contains("CLI_APPEND_REPLACED")
                        && !system.contains("REJECTED_REPEAT_MARKER")
                );
                let actual = if anthropic {
                    &request["output_config"]["format"]["schema"]
                } else {
                    &request["response_format"]["json_schema"]["schema"]
                };
                assert_eq!(actual, &schema);
            }
        }
        let mut fresh = Client::start(root.path(), &server, &[]);
        fresh
            .send(user("New process has no SDK initialization overrides"))
            .await;
        assert!(fresh.result().await["structured_output"].is_null());
        fresh
            .send(initialize_request(
                "late",
                json!({"systemPrompt":"LATE_REJECTED"}),
            ))
            .await;
        let late = fresh.frame("control_response").await;
        assert_eq!(late["response"]["subtype"], "error");
        assert!(late["response"]["error"]
            .as_str()
            .unwrap()
            .contains("Already initialized"));
        assert!(fresh.finish().await.0.success());
        let requests = server.fixture.requests.lock().unwrap();
        let request = &requests[2];
        assert!(!request.to_string().contains("SDK_BASE_中文"));
        assert!(!request.to_string().contains("SDK_APPEND_MARKER"));
        assert!(request.get("response_format").is_none());
        assert!(request
            .get("output_config")
            .is_none_or(|value| value.get("format").is_none()));
    }
}

#[tokio::test]
async fn sdk_initialize_validation_is_atomic_and_does_not_consume_initialization() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(
            root.path(),
            &server,
            &[
                "--system-prompt",
                "CLI_BASE_PRESERVED",
                "--append-system-prompt",
                "CLI_APPEND_PRESERVED",
            ],
        );
        for (index,fields) in [
            json!({"systemPrompt":"INVALID_MARKER","jsonSchema":{"type":42}}),
            json!({"systemPrompt":"INVALID_MARKER","unknown":null}),
            json!({"systemPrompt":17}),
            json!({"systemPrompt":"INVALID_MARKER","hooks":{"WorktreeCreate":[{"hookCallbackIds":["callback"]}]}}),
            json!({"jsonSchema":{"$ref":"https://example.invalid/schema"}}),
        ].into_iter().enumerate() {
            client.send(initialize_request(&format!("bad-{index}"),fields)).await;
            let response=client.frame("control_response").await;
            assert_eq!(response["response"]["subtype"],"error","{response}");
        }
        assert!(server.fixture.requests.lock().unwrap().is_empty());
        client
            .send(initialize_request(
                "valid",
                json!({"appendSystemPrompt":"SDK_APPEND_ONLY"}),
            ))
            .await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client.send(user("Use the valid initialization only")).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(client.finish().await.0.success());
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let system = if anthropic {
            requests[0]["system"].to_string()
        } else {
            requests[0]["messages"][0].to_string()
        };
        assert!(system.contains("CLI_BASE_PRESERVED"));
        assert!(system.contains("SDK_APPEND_ONLY"));
        assert!(!system.contains("CLI_APPEND_PRESERVED"));
        assert!(!system.contains("INVALID_MARKER"));
        assert!(requests[0].get("response_format").is_none());
        assert!(requests[0]
            .get("output_config")
            .is_none_or(|value| value.get("format").is_none()));
    }
}

#[tokio::test]
async fn sdk_initialize_is_rejected_during_an_implicitly_initialized_turn() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Question).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("Ask which language")).await;
        let question = client.frame("control_request").await;
        client
            .send(initialize_request(
                "too-late",
                json!({"systemPrompt":"MUST_NOT_APPLY"}),
            ))
            .await;
        let rejected = client.frame("control_response").await;
        assert_eq!(rejected["response"]["subtype"], "error");
        assert!(rejected["response"]["error"]
            .as_str()
            .unwrap()
            .contains("Already initialized"));
        let mut updated = question["request"]["input"].clone();
        updated["answers"] = json!({"Which language?":"Rust"});
        client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":question["request_id"],"response":{"behavior":"allow","updatedInput":updated}}})).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(client.finish().await.0.success());
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[1].to_string().contains("MUST_NOT_APPLY"));
        assert!(requests[1]["messages"].to_string().contains("Rust"));
    }
}

#[tokio::test]
async fn typed_sdk_approval_rewrites_both_provider_histories_and_empty_input_preserves_original() {
    for anthropic in [false, true] {
        for empty in [false, true] {
            let server = Server::start(anthropic, Mode::Approval).await;
            let root = tempfile::tempdir().unwrap();
            let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
            client.send(user("Write a reviewed file")).await;
            let permission = client.frame("control_request").await;
            assert_eq!(permission["request"]["tool_name"], "Write");
            let replacement = if empty {
                json!({})
            } else {
                json!({"file_path":"replacement.txt","content":"REVIEWED_REPLACEMENT_SENTINEL"})
            };
            client.send(json!({"type":"control_response","response":{"subtype":"success",
                "request_id":permission["request_id"],"response":{"behavior":"allow","updatedInput":replacement}}})).await;
            assert_eq!(client.result().await["is_error"], false);
            assert_eq!(root.path().join("approval.txt").exists(), empty);
            assert_eq!(root.path().join("replacement.txt").exists(), !empty);
            let requests = server.fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let history = &requests[1]["messages"];
            if !empty {
                assert!(
                    history
                        .to_string()
                        .contains("REVIEWED_REPLACEMENT_SENTINEL"),
                    "{history}"
                );
                assert!(
                    !history.to_string().contains("ACTUAL_APPROVED_WRITE"),
                    "{history}"
                );
            }
            drop(requests);
            assert!(client.finish().await.0.success());
            let db = deepseek_code_core::db::DbManager::new(root.path().join("data").join(deepseek_code_core::runtime::DATABASE_FILE))
                .unwrap();
            let persisted: String = db
                .with_conn(|connection| {
                    connection.query_row(
                        "SELECT input FROM tool_calls WHERE name = 'Write'",
                        [],
                        |row| row.get(0),
                    )
                })
                .unwrap()
                .unwrap();
            let persisted: Value = serde_json::from_str(&persisted).unwrap();
            assert_eq!(
                persisted["file_path"],
                if empty {
                    "approval.txt"
                } else {
                    "replacement.txt"
                }
            );
        }
    }
}

#[tokio::test]
async fn typed_sdk_deny_interrupt_stops_both_providers_without_followup_request() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Approval).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
        client.send(user("Write a file")).await;
        let permission = client.frame("control_request").await;
        client.send(json!({"type":"control_response","response":{"subtype":"success",
            "request_id":permission["request_id"],"response":{"behavior":"deny","message":"INTERRUPT_SENTINEL","interrupt":true}}})).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], true, "{result}");
        assert!(
            result.to_string().contains("INTERRUPT_SENTINEL"),
            "{result}"
        );
        assert_eq!(server.fixture.requests.lock().unwrap().len(), 1);
        assert!(!root.path().join("approval.txt").exists());
        client.finish().await;
    }
}

#[tokio::test]
async fn invalid_sdk_replacement_never_commits_session_rules() {
    for anthropic in [false, true] {
        for outside_scope in [false, true] {
            let server = Server::start(anthropic, Mode::Approval).await;
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
            client.send(user("Write a file")).await;
            let permission = client.frame("control_request").await;
            let replacement = if outside_scope {
                json!({"file_path":outside.path().join("outside.txt"),"content":"must not write"})
            } else {
                json!({"file_path":"invalid.txt"})
            };
            client.send(json!({"type":"control_response","response":{"subtype":"success",
                "request_id":permission["request_id"],"response":{"behavior":"allow","updatedInput":replacement,
                "updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]}}})).await;
            // The rejected tool is returned to the model, which may still finish its turn normally.
            assert_eq!(client.result().await["is_error"], false);
            assert!(!root.path().join("approval.txt").exists());
            assert!(!root.path().join("invalid.txt").exists());
            assert!(!outside.path().join("outside.txt").exists());
            client.send(user("Try the original Write again")).await;
            let next = client.frame("control_request").await;
            assert_eq!(
                next["request"]["tool_name"], "Write",
                "Invalid input must not publish its allow rule"
            );
            client.send(json!({"type":"control_response","response":{"subtype":"success",
                "request_id":next["request_id"],"response":{"behavior":"deny","message":"DENIAL_REASON_SENTINEL"}}})).await;
            assert_eq!(client.result().await["is_error"], false);
            assert!(server
                .fixture
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .to_string()
                .contains("DENIAL_REASON_SENTINEL"));
            assert!(client.finish().await.0.success());
        }
    }
}

#[tokio::test]
async fn sdk_unknown_and_completed_permission_ids_cannot_execute_pending_tools() {
    for completed in [false, true] {
        let server = Server::start(false, Mode::Approval).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
        client.send(user("Write a file")).await;
        let permission = client.frame("control_request").await;
        let id = if completed {
            client
                .send(
                    json!({"type":"control_response","response":{"subtype":"success",
                "request_id":permission["request_id"],"response":{"behavior":"deny"}}}),
                )
                .await;
            client.result().await;
            permission["request_id"].clone()
        } else {
            json!("unknown-request")
        };
        client.send(json!({"type":"control_response","response":{"subtype":"success",
            "request_id":id,"response":{"behavior":"allow","updatedInput":{},
            "updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]}}})).await;
        assert!(!client.finish().await.0.success());
        assert!(!root.path().join("approval.txt").exists());
    }
}

#[tokio::test]
async fn sdk_initialize_agents_execute_with_bounded_tools_and_run_local_catalog() {
    for anthropic in [false, true] {
        for case in [
            "allow",
            "disallow",
            "inherit-deny",
            "parent",
            "plan",
            "turn-limit",
        ] {
            let disallow = matches!(case, "disallow" | "inherit-deny");
            let parent_limited = case == "parent";
            let server = Server::start(anthropic, Mode::RuntimeAgent).await;
            let root = tempfile::tempdir().unwrap();
            let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
            let context = deepseek_code_core::services::ServiceContext::open(
                paths.clone(),
                Arc::new(deepseek_code_core::services::EnvironmentCredentials),
            )
            .unwrap();
            let mut config = deepseek_code_core::config::AppConfig::default();
            config.providers.push(serde_json::from_value(json!({"id":"sdk-fixture","name":"SDK fixture provider","provider_type":if anthropic {"anthropic"}else{"openai"},"base_url":server.url,"enabled":true,"is_default":false,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"Fixture primary","enabled":true,"supports_vision":false,"supports_tools":true,"supports_text":true,"supports_streaming":true},{"id":"fixture-second","name":"Fixture secondary","enabled":true,"supports_vision":false,"supports_tools":true,"supports_text":true,"supports_streaming":true}]})).unwrap());
            std::fs::write(
                paths.data_dir.join("config.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .unwrap();
            let parent_session = if parent_limited {
                let definition: deepseek_code_core::services::agents::Definition = serde_json::from_value(json!({"id":"limited-parent","description":"Read-only parent","system_prompt":"Parent scope","tools":["Agent","Read"]})).unwrap();
                deepseek_code_core::services::agents::install(&context, definition.clone()).unwrap();
                let id = deepseek_code_core::runtime::SessionStore::new(context.db.clone())
                    .create(root.path(), "fixture-model", "sdk-fixture")
                    .unwrap();
                deepseek_code_core::services::agents::bind(&context.db, &id, &definition).unwrap();
                Some(id)
            } else {
                None
            };
            let extra: &[&str] = if parent_limited {
                &["--resume", parent_session.as_deref().unwrap()]
            } else if case == "plan" {
                &["--permission-mode", "plan"]
            } else {
                &[]
            };
            let mut client = Client::start(root.path(), &server, extra);
            let mut definition = json!({"description":"SDK_CHILD_DESCRIPTION","prompt":"SDK_CHILD_PROMPT_SENTINEL",
                "tools":["Read","Write"],"disallowedTools":if disallow {vec!["Write"]}else{vec![]},"model":if disallow {"inherit"}else{"fixture-second"},"maxTurns":if case == "turn-limit" {1}else{3}});
            if case == "inherit-deny" {
                definition.as_object_mut().unwrap().remove("tools");
            }
            if case == "allow" {
                definition["model"] = json!("sdk-fixture::fixture-second");
            }
            client
                .send(initialize_request(
                    "init",
                    json!({"agents":{"sdk-child":definition}}),
                ))
                .await;
            let initialized = client.frame("control_response").await;
            assert_eq!(
                initialized["response"]["subtype"], "success",
                "{initialized}"
            );
            assert!(initialized["response"]["response"]["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|agent| agent["name"] == "sdk-child"));
            client.send(user("Delegate the task to sdk-child")).await;
            let result = client.result().await;
            assert_eq!(result["is_error"], false, "{result}");
            assert_eq!(
                root.path().join("child.txt").exists(),
                matches!(case, "allow" | "turn-limit")
            );
            let requests = server.fixture.requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                if case == "turn-limit" { 3 } else { 4 },
                "{case}"
            );
            assert!(requests[0]["tools"]
                .to_string()
                .contains("SDK_CHILD_DESCRIPTION"));
            let child = &requests[1];
            assert!(
                child.to_string().contains("SDK_CHILD_PROMPT_SENTINEL"),
                "{child}"
            );
            let names = child["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| {
                    if anthropic {
                        tool["name"].as_str().unwrap()
                    } else {
                        tool["function"]["name"].as_str().unwrap()
                    }
                })
                .collect::<Vec<_>>();
            assert!(names.contains(&"Read"));
            assert_eq!(names.contains(&"Write"), !disallow && !parent_limited);
            assert_eq!(
                child["model"],
                if disallow {
                    "fixture-model"
                } else {
                    "fixture-second"
                }
            );
            assert_eq!(names.contains(&"Bash"), case == "inherit-deny");
            drop(requests);
            assert!(client.finish().await.0.success());
            let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
            let context = deepseek_code_core::services::ServiceContext::open(
                paths,
                Arc::new(deepseek_code_core::services::EnvironmentCredentials),
            )
            .unwrap();
            assert!(!deepseek_code_core::services::agents::list(&context)
                .unwrap()
                .iter()
                .any(|agent| agent.id == "sdk-child"));
            let bindings: Vec<String> = context.db.with_conn(|conn| {
                conn.prepare("SELECT definition FROM session_agents WHERE definition LIKE '%sdk-runtime-boundary%'")?
                    .query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()
            }).unwrap().unwrap();
            assert_eq!(bindings.len(), 1);
            let binding: Value = serde_json::from_str(&bindings[0]).unwrap();
            assert!(!bindings[0].contains("SDK_CHILD_PROMPT_SENTINEL"));
            assert_eq!(
                binding["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool == "Write"),
                !disallow && !parent_limited
            );
            let fresh = Server::start(anthropic, Mode::RuntimeAgent).await;
            let mut client = Client::start(root.path(), &fresh, &[]);
            client.send(initialize_request("fresh", json!({}))).await;
            let response = client.frame("control_response").await;
            assert!(!response["response"]["response"]["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|agent| agent["name"] == "sdk-child"));
            client.send(user("Try the previous run agent")).await;
            client.result().await;
            assert!(fresh.fixture.requests.lock().unwrap()[1]
                .to_string()
                .contains("Unknown agent type"));
            assert!(!fresh
                .fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.to_string().contains("SDK_CHILD_PROMPT_SENTINEL")));
            client.finish().await;
        }
    }
}

#[tokio::test]
async fn sdk_agent_initialization_rejects_entire_invalid_batch_before_publication() {
    for bad in [
        json!({"description":"bad","prompt":"bad","model":"unknown-model"}),
        json!({"description":"bad","prompt":"bad","tools":"Write"}),
        json!({"description":"bad","prompt":"bad","permissionMode":"bypassPermissions"}),
        json!({"description":"bad","prompt":"bad","maxTurns":0}),
        json!({"description":"bad","prompt":"bad","mcpServers":[""]}),
        json!({"description":"bad","prompt":"bad","skills":["missing-skill"]}),
        json!({"description":"bad","prompt":"bad","background":true}),
        json!({"description":"bad","prompt":"bad","memory":"global"}),
        json!({"description":"bad","prompt":"bad","effort":3}),
        json!({"description":"bad","prompt":"bad","effort":"huge"}),
        json!({"description":"bad","prompt":"bad","permissionMode":"nope"}),
        json!({"description":"bad","prompt":"bad","mcpServers":[{"inline":{"command":"x"}}]}),
    ] {
        let server = Server::start(false, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(
            root.path(),
            &server,
            &["--system-prompt", "ORIGINAL_SYSTEM"],
        );
        client.send(initialize_request("invalid",json!({"systemPrompt":"MUST_NOT_APPLY","agents":{
            "a-valid-first":{"description":"MUST_NOT_REGISTER","prompt":"MUST_NOT_APPLY"}, "z-invalid-second":bad}}))).await;
        let result = client.frame("control_response").await;
        assert_eq!(result["response"]["subtype"], "error", "{bad}: {result}");
        client.send(initialize_request("retry", json!({}))).await;
        let result = client.frame("control_response").await;
        assert_eq!(result["response"]["subtype"], "success", "{result}");
        assert!(!result.to_string().contains("MUST_NOT_REGISTER"));
        client.send(user("Check current system")).await;
        client.result().await;
        let requests = server.fixture.requests.lock().unwrap();
        assert!(requests[0].to_string().contains("ORIGINAL_SYSTEM"));
        assert!(!requests[0].to_string().contains("MUST_NOT_APPLY"));
        drop(requests);
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn sdk_agent_fields_reach_the_child_request() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::RuntimeAgent).await;
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(data.join("skills/sdk-skill")).unwrap();
        std::fs::write(
            data.join("skills/sdk-skill/SKILL.md"),
            "---
name: sdk-skill
description: fixture skill
---
SKILL_SENTINEL",
        )
        .unwrap();
        std::fs::create_dir_all(data.join("agent-memory/sdk-child")).unwrap();
        std::fs::write(
            data.join("agent-memory/sdk-child/notes.md"),
            "MEMORY_SENTINEL",
        )
        .unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client
            .send(initialize_request(
                "init",
                json!({"agents":{"sdk-child":{
                    "description":"SDK_CHILD_DESCRIPTION",
                    "prompt":"SDK_CHILD_PROMPT_SENTINEL",
                    "skills":["sdk-skill"],
                    "memory":"user",
                    "effort":"high",
                    "permissionMode":"acceptEdits",
                    "criticalSystemReminder_EXPERIMENTAL":"REMINDER_SENTINEL"
                }}}),
            ))
            .await;
        let initialized = client.frame("control_response").await;
        assert_eq!(
            initialized["response"]["subtype"], "success",
            "{initialized}"
        );
        client.send(user("Delegate the task to sdk-child")).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        let requests = server.fixture.requests.lock().unwrap();
        let child = requests[1].to_string();
        for sentinel in [
            "SDK_CHILD_PROMPT_SENTINEL",
            "SKILL_SENTINEL",
            "MEMORY_SENTINEL",
            "REMINDER_SENTINEL",
        ] {
            assert!(child.contains(sentinel), "missing {sentinel}: {child}");
        }
        if anthropic {
            assert_eq!(requests[1]["thinking"]["type"], "enabled");
        } else {
            assert_eq!(requests[1]["reasoning_effort"], "high");
        }
        drop(requests);
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn sdk_main_thread_agent_and_prompt_suggestions_apply() {
    let server = Server::start(false, Mode::Text).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &[]);
    client
        .send(initialize_request(
            "init",
            json!({
                "agents":{"main":{
                    "description":"MAIN_AGENT_DESCRIPTION",
                    "prompt":"MAIN_AGENT_SYSTEM_SENTINEL",
                    "initialPrompt":"INITIAL_PROMPT_SENTINEL"
                }},
                "agent":"main",
                "promptSuggestions":true
            }),
        ))
        .await;
    let initialized = client.frame("control_response").await;
    assert_eq!(
        initialized["response"]["subtype"], "success",
        "{initialized}"
    );
    client.send(user("hello")).await;
    let result = client.result().await;
    assert_eq!(result["is_error"], false, "{result}");
    let suggestion = client.frame("prompt_suggestion").await;
    assert_eq!(suggestion["suggestion"], "suggested-next");
    assert_eq!(suggestion["session_id"], result["session_id"]);
    {
        let requests = server.fixture.requests.lock().unwrap();
        let first = requests[0].to_string();
        assert!(first.contains("MAIN_AGENT_SYSTEM_SENTINEL"), "{first}");
        assert!(first.contains("INITIAL_PROMPT_SENTINEL"), "{first}");
    }
    assert!(client.finish().await.0.success());
}

#[tokio::test]
async fn sdk_agent_definition_unsupported_fields_are_explicit_errors() {
    let server = Server::start(false, Mode::Text).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &[]);
    for agent in [
        json!({"description":"d","prompt":"p","background":true}),
        json!({"description":"d","prompt":"p","mcpServers":[{"inline":{"command":"x"}}]}),
        json!({"description":"d","prompt":"p","effort":3}),
        json!({"description":"d","prompt":"p","memory":"global"}),
        json!({"description":"d","prompt":"p","skills":["missing-skill"]}),
    ] {
        client
            .send(initialize_request("bad", json!({"agents":{"x":agent}})))
            .await;
        let response = client.frame("control_response").await;
        assert_eq!(
            response["response"]["subtype"], "error",
            "{agent}: {response}"
        );
    }
    assert!(client.finish().await.0.success());
}

#[path = "sdk/hook_callbacks.rs"]
mod hook_callbacks;

#[path = "sdk/mcp_transport.rs"]
mod mcp_transport;

#[path = "sdk/launch_permissions.rs"]
mod launch_permissions;

#[path = "sdk/permission_updates.rs"]
mod permission_updates;

#[path = "sdk/messages.rs"]
mod messages;

#[path = "sdk/controls.rs"]
mod controls;
