//! Exercise compaction through a resumed CLI and the provider's HTTP interface.
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use deepseek_code_core::{
    db::session_store::Stores,
    runtime::RuntimePaths,
    services::{memory_files::FileMemory, EnvironmentCredentials, ServiceContext},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

const QUESTION: &str = "saffron INPUT_MARKER_37";
const ORIGINAL: &str = "ADMISSION_BODY_420";
const EDITED: &str = "RETRY_BODY_780";

fn note(body: &str) -> String {
    format!("---\nname: saffron\ndescription: saffron\nmetadata:\n  type: user\n---\n{body}\n")
}

struct Fixture {
    root: tempfile::TempDir,
    context: ServiceContext,
    memory: PathBuf,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let context = ServiceContext::open(paths, Arc::new(EnvironmentCredentials)).unwrap();
        let memory = FileMemory::workspace(&context, &project).unwrap();
        memory.put("saffron.md", &note(ORIGINAL), None).unwrap();
        Self {
            root,
            context,
            memory: memory.directory,
            id: uuid::Uuid::new_v4().to_string(),
        }
    }

    async fn serve(
        &self,
        window: u32,
        respond: impl Fn(Value, bool) -> Response + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let provider = json!([{"id":"fixture","name":"Fixture","base_url":url,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"model","name":"Model","enabled":true,"context_window":window,"max_tokens":1024,"supports_web_search":false,"supports_vision":false}]}]);
        fs::write(
            self.root.path().join("data/providers.json"),
            provider.to_string(),
        )
        .unwrap();
        let respond = Arc::new(respond);
        let endpoint = move |headers: HeaderMap, Json(body): Json<Value>| {
            let compact = headers
                .get("x-dcl-request-kind")
                .is_some_and(|kind| kind == "compaction");
            std::future::ready(respond(body, compact))
        };
        let app = Router::new().route("/v1/chat/completions", post(endpoint));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })
    }

    async fn turn(&self, first: bool, prompt: &str) -> Output {
        let root = self.root.path();
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        cmd.current_dir(root.join("project"))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .arg("--data-dir")
            .arg(root.join("data"))
            .arg("--coordination-dir")
            .arg(root.join("coord"))
            .args([
                "--provider",
                "fixture",
                "--model",
                "model",
                "--print",
                "--permission-mode",
                "dont-ask",
                if first { "--session-id" } else { "--resume" },
                &self.id,
                "--",
                prompt,
            ]);
        tokio::time::timeout(Duration::from_secs(75), cmd.output())
            .await
            .unwrap()
            .unwrap()
    }
}

fn answer(text: &str) -> Response {
    let chunk = json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
    let stop = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    (
        [("content-type", "text/event-stream")],
        format!("data: {chunk}\n\ndata: {stop}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}

fn export(data: &Path, id: &str) -> Vec<Value> {
    let mut out = Vec::new();
    Stores::export_session(data, id, &mut out).unwrap();
    serde_json::Deserializer::from_slice(&out)
        .into_iter()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn admitted(rows: &[Value]) -> Option<String> {
    let mut users = rows
        .iter()
        .filter(|r| r["type"] == "message" && r["role"] == "user" && r["content"] == QUESTION);
    let id = users.next()?["id"].as_str().unwrap();
    assert!(users.next().is_none());
    Some(id.to_owned())
}

async fn overflow(fail_summary: bool) {
    let fixture = Fixture::new();
    let id = fixture.id.clone();
    let data = fixture.root.path().join("data");
    let file = fixture.memory.join("saffron.md");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture = requests.clone();
    let server = fixture.serve(128_000, move |body, compact| {
        let mut calls = capture.lock().unwrap();
        calls.push((body["messages"].to_string(), admitted(&export(&data, &id))));
        if calls.len() == 2 {
            if !fail_summary { fs::write(&file, note(EDITED)).unwrap(); }
            return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"context_length_exceeded","message":"prompt is too long"}}))).into_response();
        }
        if compact {
            if fail_summary {
                return (StatusCode::FORBIDDEN, Json(json!({"error":{"message":"summary refused"}}))).into_response();
            }
            return answer("compacted");
        }
        answer("done")
    }).await;
    let first = fixture.turn(true, "hello").await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output = fixture.turn(false, QUESTION).await;
    server.abort();
    let _ = server.await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Context limit reached"), "{stderr}");
    assert_eq!(output.status.success(), !fail_summary, "{stderr}");
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), if fail_summary { 3 } else { 4 }, "{stderr}");
    let id = calls[1]
        .1
        .clone()
        .expect("row at the overflowing generation");
    assert!(calls[1].0.contains(QUESTION) && !calls[1].0.contains(ORIGINAL));
    assert!(calls[2].0.contains("hello") && !calls[2].0.contains(QUESTION));
    assert_eq!(
        calls[2].1.as_ref(),
        calls[1].1.as_ref(),
        "admitted row during summary"
    );
    let rows = export(&fixture.root.path().join("data"), &fixture.id);
    let final_id = admitted(&rows).expect("admitted row after recovery");
    assert_eq!(final_id, id);
    let messages = rows
        .iter()
        .filter(|r| r["type"] == "message")
        .collect::<Vec<_>>();
    if fail_summary {
        assert!(stderr.contains("403"), "{stderr}");
        assert!(!messages.iter().any(|r| r["is_compact_boundary"] == true));
    } else {
        assert!(stderr.contains("2 messages summarized"), "{stderr}");
        assert_eq!(calls[3].1, calls[1].1);
        assert_eq!(calls[3].0.matches(QUESTION).count(), 1);
        assert_eq!(calls[3].0.matches(ORIGINAL).count(), 0);
        assert!(!calls[3].0.contains(EDITED));
        let at = messages.iter().position(|r| r["id"] == id).unwrap();
        assert_eq!(messages[at - 1]["is_compact_boundary"], true);
    }
}

#[tokio::test]
async fn overflow_retry_preserves_input_and_refreshes_indexes() {
    overflow(false).await;
}

#[tokio::test]
async fn overflow_summary_failure_keeps_admitted_input() {
    overflow(true).await;
}

async fn compressed_chat(large: bool, edited: bool) {
    let fixture = Fixture::new();
    let current = format!(
        "INDEX_CURRENT_37{}",
        if large {
            "x".repeat(18_500)
        } else {
            String::new()
        }
    );
    if large {
        assert_eq!(current.len(), 18_516);
    }
    let index = fixture.memory.join("MEMORY.md");
    fs::write(&index, if edited { "INDEX_OLD_37" } else { &current }).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture = requests.clone();
    let server = fixture
        .serve(12_800, move |body, _| {
            assert_eq!(body["stream"], true);
            capture.lock().unwrap().push(body["messages"].to_string());
            if large {
                answer(&"The project build finished. ".repeat(100))
            } else {
                answer(&"older conversation material. ".repeat(890))
            }
        })
        .await;
    let prior = if large { 4 } else { 1 };
    for n in 0..prior {
        let output = fixture
            .turn(n == 0, &format!("Summarize build {}.", n + 1))
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(index, &current).unwrap();
    let prompt = if large && !edited {
        // The unchanged index produces no change reminder. Supply enough input to
        // cross the compression threshold without relying on a recalled file body.
        format!("{QUESTION}{}", " detail".repeat(550))
    } else if large {
        "What does saffron mean?".to_owned()
    } else {
        format!("{QUESTION}{}", " saffron".repeat(800))
    };
    let output = fixture.turn(false, &prompt).await;
    server.abort();
    let _ = server.await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), prior + 1);
    let last = calls.last().unwrap();
    assert!(last.contains("[CRITICAL: Context Compressed]"));
    assert_eq!(last.matches(&current).count(), 1);
    assert!(!last.contains("INDEX_OLD_37"));
    assert_eq!(last.matches(ORIGINAL).count(), 0);
    assert_eq!(last.matches(&prompt).count(), 1);
}

#[tokio::test]
async fn compressed_chat_sends_one_edited_index() {
    compressed_chat(false, true).await;
}

#[tokio::test]
async fn compressed_chat_fits_large_edited_index() {
    compressed_chat(true, true).await;
}

#[tokio::test]
async fn compressed_chat_fits_large_unchanged_index() {
    compressed_chat(true, false).await;
}

#[tokio::test]
async fn post_boundary_changes_leave_the_frozen_index_intact() {
    use deepseek_code_core::services::compaction;

    let fixture = Fixture::new();
    let index = fixture.memory.join("MEMORY.md");
    fs::write(&index, "FROZEN_INDEX_C").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let server = fixture
        .serve(128_000, move |body, _| {
            captured.lock().unwrap().push(body["messages"].clone());
            answer("done")
        })
        .await;

    let first = fixture.turn(true, "hello").await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let prefix = compaction::prepare(&fixture.context.db, &fixture.id).unwrap();
    compaction::commit(&fixture.context.db, prefix, "hello and its answer").unwrap();
    for (marker, question) in [
        ("INDEX_CHANGE_D", "first saffron question"),
        ("INDEX_CHANGE_E", "second saffron question"),
    ] {
        fs::write(&index, marker).unwrap();
        let output = fixture.turn(false, question).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    server.abort();
    let _ = server.await;
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 3);
    let last = calls[2].to_string();
    assert!(last.contains("FROZEN_INDEX_C") && last.contains("INDEX_CHANGE_D"));
    assert_eq!(last.matches("INDEX_CHANGE_E").count(), 1);
    assert_eq!(last.matches(ORIGINAL).count(), 0);
    let latest = calls[2].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(latest.contains("second saffron question") && latest.contains("INDEX_CHANGE_E"));
}
