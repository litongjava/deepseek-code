//! Real terminal selections and headless denial against a loopback-only model and page.
use super::*;
use axum::routing::get;
use deepseek_code_core::{permissions::http_grants, services::customization};

#[derive(Clone)]
struct Fetch {
    origin: String,
    hits: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
}

async fn chat(State(f): State<Fetch>, Json(body): Json<Value>) -> axum::response::Response {
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let (delta, finish) = if last["role"] == "tool" {
        f.completed.fetch_add(1, Ordering::SeqCst);
        (json!({"content":"HTTP_CASE_DONE"}), "stop")
    } else if body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| t["function"]["name"] == "WebFetch"))
    {
        let args = json!({"url":format!("{}/page", f.origin),"prompt":"What is the title?"});
        (
            json!({"tool_calls":[{"index":0,"id":"fetch-case","type":"function","function":{"name":"WebFetch","arguments":args.to_string()}}]}),
            "tool_calls",
        )
    } else {
        (json!({"content":"Fixture title"}), "stop")
    };
    let frames = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    );
    ([("content-type", "text/event-stream")], frames).into_response()
}

async fn exercise(key: Option<&[u8]>, hits: usize, scope: Option<&str>) {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let project = root.path().join("workspace");
    std::fs::create_dir_all(&project).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fetch {
        origin: format!("http://{}", listener.local_addr().unwrap()),
        hits: Default::default(),
        completed: Default::default(),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route(
            "/page",
            get(|State(f): State<Fetch>| async move {
                f.hits.fetch_add(1, Ordering::SeqCst);
                "Fixture title"
            }),
        )
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    write_fixture_config(&data, &project, &fixture.origin);
    let mut cfg = customization::load(&data).unwrap();
    cfg.tool_loading.insert(
        "WebFetch".into(),
        customization::ToolLoading {
            enabled: true,
            on_demand: false,
        },
    );
    customization::save(&data, &cfg, &[]).unwrap();
    if let Some(key) = key {
        let mut pty = PtySession::spawn(&project, &data, &[]);
        let timeout = Duration::from_secs(30);
        pty.wait_for(
            timeout,
            "idle",
            |r| r.iter().any(|s| s.contains("Tip:")),
            |_| {},
        )
        .await;
        pty.send(b"Fetch fixture");
        pty.wait_for(
            timeout,
            "typed draft",
            |r| r[ROWS as usize - 3].contains("Fetch fixture"),
            |_| {},
        )
        .await;
        pty.send(b"\r");
        let menu = pty
            .wait_for(
                timeout,
                "HTTP scope menu",
                |r| r.iter().any(|s| s.contains("Allow plaintext HTTP")),
                |_| {},
            )
            .await
            .join("\n");
        for label in [
            &fixture.origin,
            "Allow once",
            "Allow for this session",
            "Allow for this project",
            "Allow for this user",
            "Deny",
            "local or private network",
        ] {
            assert!(menu.contains(label), "{menu}");
        }
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
        pty.send(key);
        pty.wait_for(
            timeout,
            "HTTP result",
            |r| r.iter().any(|s| s.contains("HTTP_CASE_DONE")),
            |_| {},
        )
        .await;
        pty.send(b"/exit\r");
    } else {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        cmd.current_dir(&project)
            .arg("--data-dir")
            .arg(&data)
            .args([
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--permission-mode",
                "ask",
                "-p",
                "Fetch fixture",
            ])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("HTTP_CASE_DONE"));
    }
    assert!(fixture.completed.load(Ordering::SeqCst) > 0);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), hits);
    let grants = http_grants::list(&data).unwrap();
    assert_eq!(grants.len(), usize::from(scope.is_some()));
    if let Some(scope) = scope {
        assert_eq!(grants[0].scope, scope);
        assert_eq!(grants[0].origin, fixture.origin);
    }
    server.abort();
}

#[tokio::test]
async fn terminal_all_four_http_scopes() {
    for (key, scope) in [
        (b"1", None),
        (b"2", Some("session")),
        (b"3", Some("project")),
        (b"4", Some("user")),
    ] {
        exercise(Some(key), 1, scope).await;
    }
}

#[tokio::test]
async fn terminal_deny_and_escape_send_no_http() {
    for key in [b"5", b"\x1b"] {
        exercise(Some(key), 0, None).await;
    }
}

#[tokio::test]
async fn noninteractive_sends_no_http() {
    exercise(None, 0, None).await;
}
