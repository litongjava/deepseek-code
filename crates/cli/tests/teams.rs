//! Standalone CLI creates, spawns and disbands a team without Desktop.
#[path = "support/tool_loading.rs"]
mod tool_loading;
#[path = "support/user_message.rs"]
mod user_message;

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const ALPHA_PROMPT: &str = "alpha work";
const BETA_PROMPT: &str = "beta work";

struct Fixture {
    requests: Mutex<Vec<Value>>,
    parent_calls: AtomicUsize,
}

fn text(value: &Value) -> String {
    match &value["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_child_request(request: &Value) -> bool {
    // The task is the first line of the user's own text. Context reminders open
    // the first message and recall can append more.
    request["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["role"] == "user" && {
                let prompt = user_message::user_text(&message["content"]);
                matches!(prompt.lines().next(), Some(ALPHA_PROMPT | BETA_PROMPT))
            }
        })
    })
}

#[test]
fn child_request_marker_survives_appended_context() {
    let reminder = "<system-reminder>\nRetrieved for possible relevance\n</system-reminder>";
    for content in [
        json!(ALPHA_PROMPT),
        json!(format!("{BETA_PROMPT}\n\n{reminder}")),
        json!([
            {"type":"text","text":ALPHA_PROMPT},
            {"type":"text","text":reminder}
        ]),
    ] {
        let request = json!({"messages":[{"role":"user","content":content}]});
        assert!(is_child_request(&request), "{request}");
    }

    let leader_request = json!({"messages":[
        {"role":"user","content":format!("start a team\n\n{ALPHA_PROMPT}")},
        {"role":"assistant","content":ALPHA_PROMPT},
        {"role":"user","content":format!("{BETA_PROMPT} update")}
    ]});
    assert!(!is_child_request(&leader_request), "{leader_request}");
}

fn stream(tools: Vec<(&str, &str, Value)>) -> String {
    let tool_mode = !tools.is_empty();
    let delta = if tool_mode {
        json!({"tool_calls":tools.into_iter().enumerate().map(|(index,(id,name,input))|json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect::<Vec<_>>()})
    } else {
        json!({"content":"finished"})
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":if tool_mode{"tool_calls"}else{"stop"}}]})
    )
}

async fn respond(
    State(fixture): State<Arc<Fixture>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    fixture.requests.lock().unwrap().push(body.clone());
    let tools = if is_child_request(&body) {
        Vec::new()
    } else {
        match fixture.parent_calls.fetch_add(1, Ordering::SeqCst) {
            0 => vec![(
                "create",
                "TeamCreate",
                json!({"team_name":"cli-team","description":"standalone"}),
            )],
            1 => vec![(
                "spawn",
                "TeamSpawn",
                json!({"team_name":"cli-team","members":[
                    {"name":"alpha","prompt":ALPHA_PROMPT},
                    {"name":"beta","prompt":BETA_PROMPT}
                ]}),
            )],
            2 => vec![(
                "message",
                "SendMessage",
                json!({"operation":"send","to":"alpha","content":"second look"}),
            )],
            3 => vec![("delete", "TeamDelete", json!({}))],
            _ => Vec::new(),
        }
    };
    ([("content-type", "text/event-stream")], stream(tools))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_creates_spawns_and_disbands_a_team_standalone() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    tool_loading::enable(
        &data,
        &["TeamCreate", "TeamSpawn", "TeamDelete", "SendMessage"],
    );
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let fixture = Arc::new(Fixture {
        requests: Mutex::new(Vec::new()),
        parent_calls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let id = uuid::Uuid::new_v4().to_string();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(&project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("TEAMS_FIXTURE_KEY", "unused")
        .args([
            "--agent-teams",
            "--data-dir",
            data.to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "TEAMS_FIXTURE_KEY",
            "--model",
            "fixture",
            "--print",
            "--session-id",
            &id,
            "--allowedTools",
            "TeamCreate",
            "TeamSpawn",
            "TeamDelete",
            "SendMessage",
            "--",
            "start a team",
        ]);
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
        .await
        .unwrap()
        .unwrap();
    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("finished"), "stdout:\n{stdout}");

    let requests = fixture.requests.lock().unwrap();
    let joined = requests
        .iter()
        .map(|request| request.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("TeamCreate"));
    assert!(joined.contains("TeamSpawn"));
    assert!(joined.contains("TeamDelete"));
    assert!(joined.contains("SendMessage"));
    let children = requests
        .iter()
        .filter(|request| is_child_request(request))
        .count();
    let tail = requests
        .iter()
        .map(|request| {
            request["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] != "system")
                        .map(|message| {
                            let value = text(message);
                            let cut = 160.min(value.len());
                            format!(
                                "{}:{}",
                                message["role"],
                                &value[..value.floor_char_boundary(cut)]
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    assert!(children >= 3, "{tail:?}");
    drop(requests);

    let stores = deepseek_code_core::db::session_store::Stores::open(&data).unwrap();
    let mut directives = 0;
    for entry in stores.global().list_sessions(Default::default()).unwrap() {
        let shard = stores.open_session(&entry.conversation.id).unwrap();
        let (rows, _) = shard
            .read(|r| {
                r.model_history(
                    &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
                )
            })
            .unwrap();
        for row in rows.iter().filter(|row| row["role"] == "user") {
            if [ALPHA_PROMPT, BETA_PROMPT].contains(&row["content"].as_str().unwrap_or("")) {
                assert_eq!(row["origin"], "delegation");
                directives += 1;
            }
        }
    }
    assert_eq!(directives, 2);

    // A successful TeamDelete removes the persisted team directory.
    let team_path = data.join("data/teams").join("cli-team").join("config.json");
    assert!(
        !data.join("data/teams").join("cli-team").exists(),
        "team file: {:?}",
        std::fs::read_to_string(team_path).unwrap_or_default()
    );
    server.abort();
}
