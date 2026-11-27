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
    requests: Arc<Mutex<Vec<Request>>>,
}

fn frame(delta: Value, finish: Value) -> String {
    let event = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    format!("data: {event}\n\n")
}

async fn reply(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> ([(&'static str, &'static str); 1], String) {
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
    ([("content-type", "text/event-stream")], data)
}

async fn scenario(malformed_first_fragment: bool) {
    let (provider, model) = ("opencode-go", "deepseek-v4.1-flash");
    let state = Fixture {
        malformed_first_fragment,
        ..Fixture::default()
    };
    let requests = state.requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
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
            "models":[{"id":model, "name":"Fixture", "api":"openai-completions", "enabled":true, "context_window":1_000_000,
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
    assert!(stderr.contains("threshold 800000"), "{stderr}");
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
    if malformed_first_fragment {
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
    scenario(false).await;
}

#[tokio::test]
async fn malformed_provider_frame_stops_after_one_fragment_post_and_retains_history() {
    scenario(true).await;
}
