use super::*;
#[test]
fn precedence_matrix() {
    for cli in [false, true] {
        for env in [
            None,
            Some("1"),
            Some("true"),
            Some("0"),
            Some("false"),
            Some("invalid"),
        ] {
            for setting in [None, Some(true), Some(false)] {
                for full in [false, true] {
                    let expected = cli
                        || match env {
                            Some("1" | "true") => true,
                            Some("0" | "false") => false,
                            _ => setting.unwrap_or(full),
                        };
                    assert_eq!(
                        enabled(cli, env, setting, full),
                        expected,
                        "{cli} {env:?} {setting:?} {full}"
                    );
                }
            }
        }
    }
}

use crate::native_engine::{anthropic_client::AnthropicClient, openai_client::OpenAIClient};
use axum::{
    body::{Body, Bytes},
    http::HeaderMap,
    response::IntoResponse,
    routing::post,
    Router,
};
use std::sync::{Arc, Mutex};
type Captured = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;
const OPENAI: &str = ": raw comment\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"字节 🦀\"},\"finish_reason\":null}]}\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
const ANTHROPIC: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"test\",\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"字节 🦀\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
fn provider(base: &str, builtin: bool) -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture", "name":"fixture", "base_url":base, "api_key":"sk-secret-fixture", "api_format":"openai", "builtin_id":if builtin { Some("deepseek") } else { None }, "models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture-model","name":"fixture","enabled":true,"api":"openai-completions","supports_web_search":false})).unwrap(),
    }
}
fn options(root: &Path) -> RequestOptions {
    let mut options = RequestOptions::for_session(Some("session-test"));
    options.recording = Some(RecordingContext {
        root: root.into(),
        parent_session_id: Some("parent-test".into()),
    });
    options
}
async fn fixture(
    raw: &'static str,
    retry: bool,
    hold: bool,
) -> (String, Captured, tokio::task::JoinHandle<()>) {
    let seen: Captured = Default::default();
    let captured = seen.clone();
    let app = Router::new().fallback(post(move |headers: HeaderMap, bytes: Bytes| {
        let mut seen = captured.lock().unwrap();
        seen.push((headers, bytes.to_vec()));
        let failed = retry && seen.len() == 1;
        async move {
            if failed {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [("content-type", "application/json")],
                    "{ \"error\": \"retry\" }",
                )
                    .into_response();
            }
            let stream = async_stream::stream! {
                for chunk in raw.as_bytes().chunks(7) {
                    yield Ok::<_, std::io::Error>(chunk.to_vec());
                    tokio::task::yield_now().await;
                }
                if hold { futures::future::pending::<()>().await; }
            };
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen, server)
}
fn metas(root: &Path) -> Vec<(PathBuf, Value)> {
    let mut files = fs::read_dir(root.join("request-records/session-test"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".meta.json"))
        .collect::<Vec<_>>();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let v = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
            (p, v)
        })
        .collect()
}
fn sibling(meta: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}.{}",
        meta.to_string_lossy().trim_end_matches(".meta.json"),
        suffix
    ))
}

#[tokio::test]
async fn three_transports_record_exact_wire_bytes_and_private_files() {
    for channel in ["openai", "anthropic", "catalog"] {
        let temp = tempfile::tempdir().unwrap();
        let raw = if channel == "anthropic" {
            ANTHROPIC
        } else {
            OPENAI
        };
        let (base, seen, server) = fixture(raw, false, false).await;
        let mut p = provider(&base, channel == "catalog");
        // Harness headers are an explicit non-secret header on all three paths.
        p.provider.builtin_id = Some("deepseek".into());
        let mut opts = options(temp.path());
        opts.model_request_id = Some("trace-id".into());
        let mut stream = match channel {
            "anthropic" => AnthropicClient::new()
                .with_request_options(opts)
                .send_message_stream(&p, vec![], None, vec![], 128)
                .await
                .unwrap(),
            "catalog" => crate::native_engine::provider_runtime::stream_with_options(
                &p,
                vec![],
                None,
                vec![],
                128,
                None,
                None,
                &opts,
            )
            .await
            .unwrap(),
            _ => {
                p.provider.builtin_id = None; // custom transport; keep DeepSeek identity via a nonsecret explicit session header in separate test
                OpenAIClient::new()
                    .with_request_options(opts)
                    .send_message_stream(&p, vec![], None, vec![], 128)
                    .await
                    .unwrap()
            }
        };
        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            output.push_str(&chunk.unwrap());
        }
        assert!(output.contains("字节"));
        let rows = metas(temp.path());
        assert_eq!(rows.len(), 1);
        let (path, meta) = &rows[0];
        assert_eq!(
            fs::read(sibling(path, "request.json")).unwrap(),
            seen.lock().unwrap()[0].1
        );
        assert_eq!(fs::read(sibling(path, "sse.txt")).unwrap(), raw.as_bytes());
        let headers = fs::read_to_string(sibling(path, "headers.json")).unwrap();
        assert!(!headers.contains("sk-secret-fixture"));
        assert!(!headers.contains("authorization"));
        assert!(!headers.contains("x-api-key"));
        assert!(headers.contains("content-type"));
        let recorded: Value = serde_json::from_str(&headers).unwrap();
        let captured_headers = seen.lock().unwrap()[0].0.clone();
        let expected = captured_headers
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "authorization" | "x-api-key"))
            .count();
        assert_eq!(recorded.as_object().unwrap().len(), expected);
        for (name, value) in &captured_headers {
            if !matches!(name.as_str(), "authorization" | "x-api-key") {
                assert_eq!(
                    recorded[name.as_str()][0],
                    value.to_str().unwrap(),
                    "{channel}: {name}"
                );
            }
        }
        if channel != "openai" {
            assert!(headers.contains("x-deepseek-harness-session-id"));
        }
        assert_eq!(meta["parent_session_id"], "parent-test");
        assert_eq!(meta["http_status"], 200);
        assert_eq!(meta["stream_completed"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            for entry in fs::read_dir(path.parent().unwrap()).unwrap() {
                assert_eq!(
                    entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        server.abort();
    }
}

#[tokio::test]
async fn retries_get_two_attempts_and_raw_error_response() {
    let temp = tempfile::tempdir().unwrap();
    let (base, seen, server) = fixture(OPENAI, true, false).await;
    let mut stream = OpenAIClient::new()
        .with_request_options(options(temp.path()))
        .send_message_stream(&provider(&base, false), vec![], None, vec![], 128)
        .await
        .unwrap();
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
    let rows = metas(temp.path());
    assert_eq!(rows.len(), 2);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(rows[0].1["request_id"], rows[1].1["request_id"]);
    for (index, (path, meta)) in rows.iter().enumerate() {
        assert_eq!(meta["attempt"], index + 1);
        assert_eq!(meta["sequence"], index + 1);
        assert!(path
            .to_string_lossy()
            .contains(&format!("-a{}.", index + 1)));
        assert_eq!(
            fs::read(sibling(path, "request.json")).unwrap(),
            seen.lock().unwrap()[index].1
        );
    }
    assert_eq!(
        fs::read(sibling(&rows[0].0, "response.json")).unwrap(),
        b"{ \"error\": \"retry\" }"
    );
    server.abort();
}

#[tokio::test]
async fn cancelled_stream_keeps_partial_bytes_and_incomplete_meta() {
    const PART: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let temp = tempfile::tempdir().unwrap();
    let (base, _, server) = fixture(PART, false, true).await;
    let mut stream = OpenAIClient::new()
        .with_request_options(options(temp.path()))
        .send_message_stream(&provider(&base, false), vec![], None, vec![], 128)
        .await
        .unwrap();
    let mut output = String::new();
    while output.len() < PART.len() {
        output.push_str(
            &tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        );
    }
    let active = metas(temp.path());
    assert_eq!(active[0].1["stream_completed"], false);
    assert_eq!(
        fs::read(sibling(&active[0].0, "sse.txt")).unwrap(),
        PART.as_bytes()
    );
    drop(stream);
    let rows = metas(temp.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        fs::read(sibling(&rows[0].0, "sse.txt")).unwrap(),
        PART.as_bytes()
    );
    assert_eq!(rows[0].1["stream_completed"], false);
    assert!(rows[0].1["error"].as_str().unwrap().contains("cancelled"));
    server.abort();
}

#[tokio::test]
async fn disabled_and_disk_failure_do_not_change_output() {
    for fail in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (base, _, server) = fixture(OPENAI, false, false).await;
        let opts = if fail {
            fs::write(temp.path().join("request-records"), b"block").unwrap();
            options(temp.path())
        } else {
            RequestOptions::for_session(Some("session-test"))
        };
        let mut stream = OpenAIClient::new()
            .with_request_options(opts)
            .send_message_stream(&provider(&base, false), vec![], None, vec![], 128)
            .await
            .unwrap();
        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            output.push_str(&chunk.unwrap());
        }
        assert_eq!(output, OPENAI);
        if !fail {
            assert!(!temp.path().join("request-records").exists());
        }
        server.abort();
    }
}

#[test]
fn header_filter_and_url_remove_authentication() {
    assert!(secret_header("x-deepseek-harness-extra", "sk-hidden"));
    assert!(secret_header(
        "x-deepseek-harness-extra",
        "a1234567890123456789012345678901234"
    ));
    assert!(!secret_header(
        "x-correlation-id",
        "28bf2307-c213-4d31-a13c-94cb0d2aba8a"
    ));
    let temp = tempfile::tempdir().unwrap();
    let request = reqwest::Client::new()
        .post("http://user:pass@localhost/v1?key=secret")
        .header("Authorization", "Bearer hidden")
        .header("x-api-key", "hidden")
        .header("api-key", "hidden")
        .header("x-goog-api-key", "hidden")
        .header("Cookie", "hidden")
        .header("Proxy-Authorization", "hidden")
        .header("custom", "sk-hidden")
        .header("custom-jwt", "eyJabc.def.xyz")
        .header("opaque", "a1234567890123456789012345678901234")
        .header("x-deepseek-harness-session-id", "session-test")
        .header("x-feature", "retained")
        .body("{  \"raw\" : true }")
        .build()
        .unwrap();
    let h = headers(&request);
    assert_eq!(h.as_object().unwrap().len(), 2);
    assert_eq!(h["x-feature"][0], "retained");
    let mut record = Attempt::start(
        &options(temp.path()),
        &provider("http://localhost", false),
        "openai",
        &request,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(record.meta["url"], "http://localhost/v1");
    record.finish(None, true);
}

#[test]
fn concurrent_sequences_and_reopen_never_collide() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let root = root.clone();
            scope.spawn(move || {
                let request = reqwest::Client::new()
                    .post("http://localhost")
                    .body("{}")
                    .build()
                    .unwrap();
                let _record = Attempt::start(
                    &options(&root),
                    &provider("http://localhost", false),
                    "openai",
                    &request,
                    false,
                )
                .unwrap()
                .unwrap();
            });
        }
    });
    let rows = metas(&root);
    assert_eq!(rows.len(), 16);
    let mut numbers = rows
        .iter()
        .map(|r| r.1["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    numbers.sort();
    assert_eq!(numbers, (1..=16).collect::<Vec<_>>());
}

#[test]
fn retention_uses_latest_file_and_excludes_active_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("request-records");
    let now = SystemTime::now() + Duration::from_secs(15 * 86400);
    let mut active = None;
    for session in ["old", "new", "active", "recent-file"] {
        let (dir, lease, lock) = acquire(&root, session).unwrap();
        crate::fs::unlock_file(&lock).unwrap();
        write_new(&dir.join("data"), b"{}").unwrap();
        if session == "new" || session == "recent-file" {
            OpenOptions::new()
                .write(true)
                .open(dir.join("data"))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(now))
                .unwrap();
        }
        if session == "active" {
            active = Some(lease);
        }
    }
    let lock = root_lock(&root).unwrap();
    cleanup_locked(&root, now).unwrap();
    crate::fs::unlock_file(&lock).unwrap();
    assert!(!root.join("old").exists());
    for session in ["new", "active", "recent-file"] {
        assert!(root.join(session).exists(), "{session}");
    }
    drop(active);
}

#[tokio::test]
async fn nonstream_response_is_unformatted_bytes() {
    const RAW: &str = "{ \n \"answer\": \"原始\", \"usage\" : null }\n";
    let app = Router::new().fallback(post(|| async {
        ([("content-type", "application/json")], RAW)
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let response = send(
        reqwest::Client::new().post(&base).body("{ \"input\" : 1 }"),
        &options(temp.path()),
        &provider(&base, false),
        "openai",
        false,
    )
    .await
    .unwrap();
    assert_eq!(response.url().as_str(), format!("{base}/"));
    assert_eq!(response.bytes().await.unwrap().as_ref(), RAW.as_bytes());
    let rows = metas(temp.path());
    assert_eq!(
        fs::read(sibling(&rows[0].0, "response.json")).unwrap(),
        RAW.as_bytes()
    );
    assert_eq!(rows[0].1["stream_completed"], true);
    server.abort();
}

#[test]
fn launch_policy_is_session_scoped_and_inherited_by_explicit_parent() {
    let temp = tempfile::tempdir().unwrap();
    let paths = crate::runtime::RuntimePaths::new(temp.path().into()).unwrap();
    let db = paths.open_database().unwrap();
    db.set_request_recording_policy("parent-test", true, false);
    assert!(RequestOptions::from_db(&db, Some("parent-test"), "main")
        .recording
        .is_some());
    assert!(RequestOptions::from_db(&db, Some("unrelated"), "main")
        .recording
        .is_none());
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
            rusqlite::params!["delegation.child", r#"{"parent_session_id":"parent-test"}"#],
        )
    })
    .unwrap()
    .unwrap();
    let child = RequestOptions::from_db(&db, Some("child"), "main");
    assert_eq!(child.purpose, "subagent");
    assert_eq!(
        child.recording.unwrap().parent_session_id.as_deref(),
        Some("parent-test")
    );
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
            rusqlite::params!["delegation.grandchild", r#"{"parent_session_id":"child"}"#],
        )
    })
    .unwrap()
    .unwrap();
    let grandchild = RequestOptions::from_db(&db, Some("grandchild"), "main");
    assert_eq!(
        grandchild.recording.unwrap().parent_session_id.as_deref(),
        Some("child")
    );
    assert!(!temp.path().join("request-records").exists());
}

#[tokio::test]
async fn compatible_openai_finish_reason_without_done_is_complete() {
    const RAW: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n";
    let temp = tempfile::tempdir().unwrap();
    let (base, _, server) = fixture(RAW, false, false).await;
    let mut stream = OpenAIClient::new()
        .with_request_options(options(temp.path()))
        .send_message_stream(&provider(&base, false), vec![], None, vec![], 128)
        .await
        .unwrap();
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
    let rows = metas(temp.path());
    assert_eq!(rows[0].1["stream_completed"], true);
    assert_eq!(
        fs::read(sibling(&rows[0].0, "sse.txt")).unwrap(),
        RAW.as_bytes()
    );
    server.abort();
}

#[cfg(target_os = "linux")]
#[test]
fn disk_full_during_a_chunk_does_not_change_completion() {
    let temp = tempfile::tempdir().unwrap();
    let request = reqwest::Client::new()
        .post("http://localhost")
        .body("{}")
        .build()
        .unwrap();
    let mut record = Attempt::start(
        &options(temp.path()),
        &provider("http://localhost", false),
        "openai",
        &request,
        true,
    )
    .unwrap()
    .unwrap();
    record.meta["http_status"] = json!(200);
    // Real ENOSPC from the OS, without filling a shared filesystem.
    record.body = Some(OpenOptions::new().write(true).open("/dev/full").unwrap());
    record.chunk(b"data: [DONE]\n\n");
    record.finish(None, true);
    let rows = metas(temp.path());
    assert!(rows[0].1["recording_error"].is_string());
    assert_eq!(rows[0].1["stream_completed"], true);
    assert!(rows[0].1["error"].is_null());
}
