//! Auto-compaction before a resumed --print turn, against built-in provider fixtures.
#[path = "../../core/tests/support/session_sql.rs"]
mod fixture_sql;
use fixture_sql::FixtureSql;
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    db::message_repo::get_messages_by_conversation,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const SUMMARY: &str = "{\"kept\":\"中文🙂\"}\n\n{\"next\":\"continue\"}";
const PENDING: &str = "Resume the pending task";
const REPLY: &str = "Resumed task completed.";
const PREFIX: &str = "synthetic-history-start ";

struct Request {
    fragment: bool,
    turn: bool,
    budget: u64,
    has_summary: bool,
    has_original: bool,
}

#[derive(Clone, Default)]
struct Fixture {
    malformed_first_fragment: bool,
    reject_turn: bool,
    requests: Arc<Mutex<Vec<Request>>>,
}

fn frame(delta: Value, finish: Value) -> String {
    let event = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    format!("data: {event}\n\n")
}

async fn reply(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> (
    axum::http::StatusCode,
    [(&'static str, &'static str); 1],
    String,
) {
    let text = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|msg| msg["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let fragment = text.contains("Summarize this consecutive fragment");
    let turn = text.contains(PENDING);
    let malformed = {
        let mut requests = fixture.requests.lock().unwrap();
        let malformed = fixture.malformed_first_fragment && fragment && requests.is_empty();
        requests.push(Request {
            fragment,
            turn,
            budget: body["max_tokens"].as_u64().unwrap(),
            has_original: text.contains(PREFIX),
            has_summary: text.contains(SUMMARY),
        });
        malformed
    };
    if turn && fixture.reject_turn {
        return (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            [("content-type", "text/plain")],
            "context limit".into(),
        );
    }
    let data = if malformed {
        // Valid partial text, then two JSON values inside a single data payload.
        frame(json!({"content":"discarded partial"}), Value::Null) + "data: {}\ndata: {}\n\n"
    } else {
        let answer = if turn { REPLY } else { SUMMARY };
        let (first, rest) = answer.split_at(1);
        frame(json!({"role":"assistant"}), Value::Null)
            + &frame(json!({"content":first}), Value::Null)
            + &frame(json!({"content":rest}), Value::Null)
            + &frame(json!({}), json!("stop"))
            + "data: [DONE]\n\n"
    };
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/event-stream")],
        data,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Success,
    Fits,
    TooLarge,
    Rejected,
}

async fn scenario(case: Scenario) {
    let window: u32 = if case == Scenario::TooLarge {
        750_000
    } else {
        1_000_000
    };
    let (provider, model) = ("opencode-go", "deepseek-v4.1-flash");
    let state = Fixture {
        malformed_first_fragment: case != Scenario::Success,
        reject_turn: case == Scenario::Rejected,
        ..Fixture::default()
    };
    let requests = state.requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let fixture = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(fixture.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(fixture.path(), model, provider).unwrap();
    // The quote-heavy transcript reproduces the 16-fragment plan at the incident threshold.
    let original = format!(
        "{PREFIX}{}{}",
        "evidence\"".repeat(150_000),
        "evidence ".repeat(123_000)
    );
    let original_id = store.append(&session, "user", &original).unwrap();
    store.append(&session, "assistant", "Prior reply.").unwrap();
    deepseek_code_core::data_layout::write_config_file(
        paths.data_dir.join("config/providers.json"),
        json!([{
            "id":provider, "builtin_id":provider, "name":"Local fixture",
            "base_url":base, "api_key":"test-only", "api_format":"openai", "enabled":true,
            "models":[{"id":model, "name":"Fixture", "api":"openai-completions", "enabled":true, "context_window":window,
                "max_tokens":32_000, "supports_web_search":false}]
        }])
        .to_string(),
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("DEEPSEEK_CODE") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .env("DEEPSEEK_CODE_DATA_DIR", &paths.data_dir)
        .arg("--path")
        .arg(fixture.path())
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--resume",
            &session,
            PENDING,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut child_stdout = child.stdout.take().unwrap();
    let mut child_stderr = child.stderr.take().unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let collected = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::try_join!(
            child.wait(),
            child_stdout.read_to_end(&mut stdout),
            child_stderr.read_to_end(&mut stderr),
        )
    })
    .await;
    let status = match collected {
        Ok(Ok((status, _, _))) => status,
        failure => {
            let _ = child.start_kill();
            child.wait().await.expect("reap the dcl fixture");
            panic!("{failure:?}\n{}", String::from_utf8_lossy(&stderr));
        }
    };
    server.abort();
    let stdout = String::from_utf8(stdout).unwrap();
    let stderr = String::from_utf8(stderr).unwrap();
    let result = stdout
        .lines()
        .rev()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["type"] == "result")
        .unwrap();
    let requests = requests.lock().unwrap();
    let rows = store
        .db
        .shard_sql(&session, |db| get_messages_by_conversation(db, &session))
        .unwrap()
        .unwrap();
    assert!(
        stderr.contains(&format!("threshold {}", window * 80 / 100)),
        "{stderr}"
    );
    assert!(requests.iter().all(|request| request.budget == 32_000));
    assert_eq!(
        rows.iter()
            .find(|row| row.id == original_id)
            .unwrap()
            .content,
        original
    );
    let boundaries: Vec<_> = rows.iter().filter(|row| row.is_compact_boundary).collect();
    let turns: Vec<_> = requests.iter().filter(|request| request.turn).collect();
    assert!(requests[0].fragment && requests[0].has_original);
    if case == Scenario::Rejected {
        assert!(!status.success(), "{result}");
        let error = result["error"].as_str().unwrap();
        for detail in [
            "Automatic compaction failed",
            "provider rejected",
            "/compact",
            "Invalid provider SSE JSON",
        ] {
            assert!(error.contains(detail), "{error}");
        }
        assert_eq!(requests.len(), 2);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].has_original && boundaries.is_empty());
    } else if case == Scenario::Fits {
        assert!(status.success(), "{result}");
        assert_eq!(result["result"], REPLY);
        assert_eq!(requests.len(), 2);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].has_original && !turns[0].has_summary);
        assert!(boundaries.is_empty());
        assert!(
            stdout.contains("Automatic compaction failed")
                || stderr.contains("Automatic compaction failed")
        );
    } else if case == Scenario::TooLarge {
        assert!(!status.success(), "{result}");
        assert_eq!(result["subtype"], "error_during_execution");
        let error = result["error"].as_str().unwrap();
        for detail in [
            "fragment 1/16",
            "0 of 7 recovery generations",
            "original history retained",
            "Invalid provider SSE JSON",
            "trailing characters at line 2 column 1",
        ] {
            assert!(error.contains(detail), "{error}");
        }
        assert_eq!(requests.len(), 1);
        assert!(boundaries.is_empty());
        assert!(turns.is_empty());
    } else {
        assert!(status.success(), "{result}\n{stderr}");
        assert_eq!(result["result"], REPLY);
        assert_eq!(boundaries.len(), 1);
        assert!(boundaries[0].content.ends_with(SUMMARY));
        assert_eq!(turns.len(), 1);
        assert!(turns[0].has_summary && !turns[0].has_original);
    }
}

#[tokio::test]
async fn coalesced_sse_frames_preserve_the_complete_plain_text_summary() {
    scenario(Scenario::Success).await;
}

#[tokio::test]
async fn malformed_provider_frame_stops_after_one_fragment_post_and_retains_history() {
    scenario(Scenario::TooLarge).await;
}

#[tokio::test]
async fn failed_pre_turn_summary_sends_the_original_request_when_it_fits() {
    scenario(Scenario::Fits).await;
}

#[tokio::test]
async fn provider_context_rejection_keeps_the_pre_turn_compaction_failure() {
    scenario(Scenario::Rejected).await;
}

fn write_responses_fixture(paths: &RuntimePaths, base: &str) {
    deepseek_code_core::data_layout::write_config_file(
        paths.data_dir.join("config/providers.json"),
        json!([{"id":"local","name":"Local","builtin_id":"openai","api_format":"openai",
            "base_url":base,"api_key":"fixture-only","enabled":true,
            "models":[{"id":"gpt-fixture","name":"Fixture","api":"openai-responses","enabled":true,
                "context_window":64000,"max_tokens":1000,"supports_web_search":false}]}])
        .to_string(),
    )
    .unwrap();
}

async fn resumed_query(
    paths: &RuntimePaths,
    workspace: &std::path::Path,
    id: &str,
    prompt: &str,
) -> std::process::Output {
    let mut cli = Command::new(env!("CARGO_BIN_EXE_dcl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("DEEPSEEK_CODE") {
            cli.env_remove(key);
        }
    }
    tokio::time::timeout(
        Duration::from_secs(30),
        cli.env("DEEPSEEK_CODE_DATA_DIR", &paths.data_dir)
            .arg("--path")
            .arg(workspace)
            .args(["--resume", id, "query", "-p", prompt])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

/// Drive the same slash command a user runs in a resumed CLI session.
async fn manual_native_compaction(persistent: bool) {
    use axum::{http::StatusCode, response::IntoResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let native = Arc::new(AtomicUsize::new(0));
    let summary = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = native.clone();
    let fallbacks = summary.clone();
    let app = Router::new().fallback(post(move |uri: axum::http::Uri| {
        let attempt = if uri.path().ends_with("/responses/compact") {
            requests.fetch_add(1, Ordering::SeqCst)
        } else {
            assert!(uri.path().ends_with("/responses"));
            fallbacks.fetch_add(1, Ordering::SeqCst);
            0
        };
        async move {
            if persistent || attempt == 0 {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "0")],
                    "busy",
                )
                    .into_response();
            }
            Json(json!({"object":"response.compaction","output":[
                {"type":"compaction","id":"compact-test","encrypted_content":"fixture-context"}
            ]}))
            .into_response()
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(temp.path(), "gpt-fixture", "local").unwrap();
    store.append(&id, "user", "Read the fixture").unwrap();
    store
        .append(&id, "assistant", "The fixture is ready")
        .unwrap();
    let before = store.history(&id).unwrap();
    write_responses_fixture(&paths, &base);
    let output = resumed_query(&paths, temp.path(), &id, "/compact Keep the fixture").await;
    let diagnostic = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let attempts = deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1;
    assert_eq!(output.status.success(), !persistent, "{diagnostic}");
    if persistent {
        assert!(
            diagnostic.contains("429") && diagnostic.contains("summary compaction failed"),
            "{diagnostic}"
        );
        assert_eq!(native.load(Ordering::SeqCst), attempts);
        assert_eq!(summary.load(Ordering::SeqCst), attempts);
        assert_eq!(store.history(&id).unwrap(), before);
    } else {
        assert_eq!(native.load(Ordering::SeqCst), 2, "{diagnostic}");
        assert_eq!(summary.load(Ordering::SeqCst), 0);
        assert_eq!(store.history(&id).unwrap().len(), before.len() + 1);
    }
    assert_eq!(
        deepseek_code_core::services::compaction::count(&store.db, &id)
            .unwrap()
            .proven(),
        u64::from(!persistent)
    );
    server.abort();
}

#[tokio::test]
async fn manual_compact_retries_429_and_commits_once() {
    manual_native_compaction(false).await;
}

#[tokio::test]
async fn manual_compact_persistent_429_reports_failure_and_preserves_history() {
    manual_native_compaction(true).await;
}

#[tokio::test]
async fn manual_fallback_replays_the_same_twelve_read_results_as_native() {
    use axum::{http::StatusCode, response::IntoResponse};
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new().fallback(post(move |uri: axum::http::Uri, Json(body): Json<Value>| {
        let native = uri.path().ends_with("/responses/compact");
        capture.lock().unwrap().push((native, body["input"].clone()));
        async move {
            if native {
                return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "busy").into_response();
            }
            let event = json!({"type":"response.completed","response":{"id":"local-response","status":"completed",
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fixture summary"}]}],
                "usage":{"input_tokens":100,"output_tokens":5}}});
            let delta = json!({"type":"response.output_text.delta","delta":"fixture summary"});
            ([("content-type","text/event-stream")],format!("event: response.output_text.delta\ndata: {delta}\n\nevent: response.completed\ndata: {event}\n\n")).into_response()
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(temp.path(), "gpt-fixture", "local").unwrap();
    write_responses_fixture(&paths, &base);
    let seed = resumed_query(&paths, temp.path(), &id, "Inspect the fixture").await;
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );
    requests.lock().unwrap().clear();

    store
        .db
        .shard_sql(&id, |db| -> anyhow::Result<()> {
            use deepseek_code_core::db::message_repo::{insert_tool_call, update_message_content};
            let assistant = get_messages_by_conversation(db, &id)?
                .into_iter()
                .rev()
                .find(|r| r.role == "assistant")
                .unwrap();
            update_message_content(db, &assistant.id, &"a".repeat(103_500))?;
            for index in 0..12 {
                let output = if index == 0 {
                    format!("REVIEW_OLD_TOOL {}", "x".repeat(1100))
                } else {
                    format!("READ_RESULT_{index}")
                };
                insert_tool_call(
                    db,
                    &format!("review_read_{index}"),
                    &assistant.id,
                    "Read",
                    Some("{}"),
                    Some(&output),
                    false,
                    index,
                )?;
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
    let compact = resumed_query(&paths, temp.path(), &id, "/compact review-focus").await;
    assert!(
        compact.status.success(),
        "{}",
        String::from_utf8_lossy(&compact.stderr)
    );
    let requests = requests.lock().unwrap();
    let (summary, native) = requests.split_last().unwrap();
    let budget = deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1;
    assert_eq!(native.len(), budget);
    assert!(!summary.0);
    let (instruction, history) = summary.1.as_array().unwrap().split_last().unwrap();
    assert!(instruction.to_string().contains("review-focus"));
    for (is_native, input) in native {
        assert!(*is_native);
        assert!(
            input.as_array().unwrap().as_slice() == history,
            "native and summary serialized different histories"
        );
    }
    let outputs: Vec<_> = history
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 12);
    assert_eq!(
        outputs[0]["output"],
        deepseek_code_core::services::compaction::microcompact::CLEARED_TOOL_RESULT
    );
    assert_eq!(outputs[11]["output"], "READ_RESULT_11");
    assert_eq!(
        deepseek_code_core::services::compaction::count(&store.db, &id)
            .unwrap()
            .proven(),
        1
    );
    server.abort();
}
