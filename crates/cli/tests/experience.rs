//! Project experience entries through actual CLI requests, both protocols.
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::RuntimePaths,
    services::{EnvironmentCredentials, ServiceContext},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Mock {
    protocol: &'static str,
    requests: Mutex<Vec<Value>>,
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn respond(
    State(mock): State<Arc<Mock>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    mock.requests.lock().unwrap().push(body);
    let events = if mock.protocol == "openai" {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":"recorded"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]
    } else {
        vec![
            json!({"type":"message_start","message":{"id":"msg-experience","type":"message","role":"assistant","model":"fixture","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"recorded"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ]
    };
    let mut stream = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    if mock.protocol == "openai" {
        stream.push_str("data: [DONE]\n\n");
    }
    ([("content-type", "text/event-stream")], stream)
}

fn entry(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
}

fn write_entry(workspace: &Path, relative: &str, name: &str, description: &str, body: &str) {
    let path = workspace.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, entry(name, description, body)).unwrap();
}

async fn run(
    data: &Path,
    workspace: &Path,
    base: &str,
    protocol: &str,
    resume: Option<&str>,
    prompt: &str,
) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--base-url",
            base,
            "--api-format",
            protocol,
            "--api-key-env",
            "EXPERIENCE_FIXTURE_KEY",
            "--model",
            "fixture",
            "--print",
            "--output-format",
            "stream-json",
            "--permission-mode",
            "dontAsk",
        ])
        .env("EXPERIENCE_FIXTURE_KEY", "local-only")
        .env("NO_PROXY", "127.0.0.1");
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            command.env_remove(key);
        }
    }
    if let Some(id) = resume {
        command.args(["--resume", id]);
    }
    command.args(["--", prompt]);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("CLI timeout")
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn user_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("unexpected user content: {other}"),
    }
}

fn first_user_text(request: &Value) -> String {
    let user = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    user_text(user)
}

fn last_user_text(request: &Value) -> String {
    let user = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap();
    user_text(user)
}

async fn scenario(protocol: &'static str) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("project");
    let data = root.path().join("data");
    std::fs::create_dir(&workspace).unwrap();
    write_entry(
        &workspace,
        ".agents/experience/alpha.md",
        "alpha",
        "Alpha symptom and keyword",
        "body-alpha-marker",
    );
    write_entry(
        &workspace,
        "docs/.agents/experience/beta.md",
        "beta",
        "Beta symptom and keyword",
        "body-beta-marker",
    );
    write_entry(
        &workspace,
        "docs/sub/.agents/experience/deep.md",
        "deep-entry",
        "Third level must be ignored",
        "body-deep-marker",
    );
    std::fs::write(
        workspace.join(".agents/experience/no-frontmatter.md"),
        "missing-frontmatter-marker",
    )
    .unwrap();
    std::fs::write(
        workspace.join(".agents/experience/oversized.md"),
        format!(
            "---\nname: oversized\ndescription: symptom\n---\n{}\n",
            "x".repeat(70 * 1024)
        ),
    )
    .unwrap();
    std::fs::write(
        workspace.join(".agents/experience/leaky.md"),
        entry("leaky", "api_key = sk-abcdefgh", "body"),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        workspace.join(".agents/experience/alpha.md"),
        workspace.join(".agents/experience/link.md"),
    )
    .unwrap();

    let mock = Arc::new(Mock {
        protocol,
        requests: Mutex::new(vec![]),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .route("/v1/messages", post(respond))
        .with_state(mock.clone());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));

    run(&data, &workspace, &base, protocol, None, "first turn").await;
    let first = {
        let requests = mock.requests.lock().unwrap();
        let text = first_user_text(&requests[0]);
        assert_eq!(text.matches("# Experience").count(), 1, "{text}");
        assert!(
            text.contains("- .agents/experience/alpha.md | alpha: Alpha symptom and keyword"),
            "{text}"
        );
        assert!(
            text.contains("- docs/.agents/experience/beta.md | beta: Beta symptom and keyword"),
            "{text}"
        );
        for absent in [
            "deep-entry",
            "body-alpha-marker",
            "body-beta-marker",
            "body-deep-marker",
            "missing-frontmatter-marker",
            "oversized",
            "leaky",
            "link.md",
        ] {
            assert!(!text.contains(absent), "{absent} in\n{text}");
        }
        text
    };
    let first_system = {
        let requests = mock.requests.lock().unwrap();
        requests[0]["system"].to_string()
    };
    assert!(!first_system.contains("# Experience"), "{first_system}");

    let context = ServiceContext::open(
        RuntimePaths::new(data.clone()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let id = context.db.latest_code(&workspace).unwrap().unwrap().id;

    // Resuming replays the frozen block byte for byte and does not add it twice.
    run(&data, &workspace, &base, protocol, Some(&id), "second turn").await;
    {
        let requests = mock.requests.lock().unwrap();
        let text = first_user_text(requests.last().unwrap());
        assert_eq!(text, first, "the first message changed on resume");
        assert!(!last_user_text(requests.last().unwrap()).contains("New experience"));
    }

    // A new entry is reported once; the frozen block stays unchanged.
    write_entry(
        &workspace,
        "docs/.agents/experience/gamma.md",
        "gamma",
        "Gamma symptom and keyword",
        "body-gamma-marker",
    );
    run(&data, &workspace, &base, protocol, Some(&id), "third turn").await;
    {
        let requests = mock.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let text = last_user_text(last);
        assert_eq!(first_user_text(last), first, "the frozen block changed");
        assert_eq!(text.matches("New experience:").count(), 1, "{text}");
        assert!(
            text.contains("- docs/.agents/experience/gamma.md | gamma: Gamma symptom and keyword"),
            "{text}"
        );
    }
    run(&data, &workspace, &base, protocol, Some(&id), "fourth turn").await;
    {
        let requests = mock.requests.lock().unwrap();
        let text = last_user_text(requests.last().unwrap());
        assert!(!text.contains("New experience:"), "reported twice\n{text}");
    }

    // Removing an entry produces one removal reminder.
    std::fs::remove_file(workspace.join("docs/.agents/experience/beta.md")).unwrap();
    run(&data, &workspace, &base, protocol, Some(&id), "fifth turn").await;
    {
        let requests = mock.requests.lock().unwrap();
        let text = last_user_text(requests.last().unwrap());
        assert_eq!(
            text.matches("Removed experience: beta").count(),
            1,
            "{text}"
        );
    }
    run(&data, &workspace, &base, protocol, Some(&id), "sixth turn").await;
    {
        let requests = mock.requests.lock().unwrap();
        let last = requests.last().unwrap();
        assert!(!last_user_text(last).contains("Removed experience: beta"));
        assert_eq!(first_user_text(last), first, "the frozen block changed");
    }

    // Disabling applies to new sessions only; re-enabling restores the block.
    deepseek_code_core::experience::set_enabled(&context, false).unwrap();
    run(&data, &workspace, &base, protocol, None, "disabled session").await;
    {
        let requests = mock.requests.lock().unwrap();
        let text = first_user_text(requests.last().unwrap());
        assert!(!text.contains("# Experience"), "{text}");
        assert!(!last_user_text(requests.last().unwrap()).contains("New experience"));
    }
    deepseek_code_core::experience::set_enabled(&context, true).unwrap();
    run(&data, &workspace, &base, protocol, None, "enabled session").await;
    {
        let requests = mock.requests.lock().unwrap();
        assert!(first_user_text(requests.last().unwrap()).contains("# Experience"));
    }
}

#[tokio::test]
async fn experience_cli_openai_freezes_index_and_reports_changes() {
    scenario("openai").await;
}

#[tokio::test]
async fn experience_cli_anthropic_freezes_index_and_reports_changes() {
    scenario("anthropic").await;
}
