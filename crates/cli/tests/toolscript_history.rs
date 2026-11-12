//! Model-visible ToolScript history through the actual CLI and worker processes.
//! No session/assistant/tool row is manufactured by the fixture.
#![cfg(unix)]

use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

const SOURCE: &str = r#"
await tools.call("Bash", {command: args.command});
const input = {file_path: "same.txt"};
const reads = [];
reads.push(await tools.call("Read", input));
reads.push(await tools.call("Read", input));
reads.push(...await Promise.all([tools.call("Read", input), tools.call("Read", input), tools.call("Read", input)]));
let denied = "NOT_DENIED";
try { await tools.call("Write", {file_path: "denied.txt", content: args.tag}); }
catch (error) { denied = error.code; }
return {reads, denied, tag: args.tag};
"#;

fn script_input(round: usize) -> Value {
    json!({"source": SOURCE, "args": {
        "tag": format!("ROUND_{round}"),
        "command": format!("printf ROUND_{round} > same.txt; printf MARKER_{round} > marker-{round}.txt")
    }})
}

#[derive(Clone)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    data: std::path::PathBuf,
    session: String,
    /// Independent read-only connection, before releasing each model response.
    assistants_before_response: Arc<Mutex<Vec<usize>>>,
}

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let index = {
        let mut requests = fixture.requests.lock().unwrap();
        requests.push(body);
        requests.len() - 1
    };
    let conn = rusqlite::Connection::open_with_flags(
        fixture.data.join("deepseek_code.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let assistants: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1 AND role='assistant'",
            [&fixture.session],
            |row| row.get(0),
        )
        .unwrap();
    fixture
        .assistants_before_response
        .lock()
        .unwrap()
        .push(assistants);
    let call = match index {
        0 => Some(("search", "ToolSearch", json!({"query":"select:ToolScript"}))),
        // Deliberately reuse the provider's raw id in separate model rounds.
        1 | 2 => Some(("same-provider-id", "ToolScript", script_input(index))),
        _ => None,
    };
    let frames = if let Some((id, name, input)) = call {
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":"history fixture complete"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]
    };
    let mut stream: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

fn snapshot(data: &Path, session: &str) -> Vec<Value> {
    let conn = rusqlite::Connection::open_with_flags(
        data.join("deepseek_code.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    let mut stmt = conn.prepare(
        "SELECT t.id, t.message_id, t.name, t.input, t.output, t.is_error, o.parent_tool_call_id, o.sequence \
         FROM tool_calls t JOIN messages m ON m.id=t.message_id \
         LEFT JOIN tool_call_origins o ON o.tool_call_id=t.id \
         WHERE m.conversation_id=?1 ORDER BY t.id",
    ).unwrap();
    let rows = stmt.query_map([session], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?, "assistant": row.get::<_, String>(1)?,
            "name": row.get::<_, String>(2)?, "input": row.get::<_, Option<String>>(3)?,
            "output": row.get::<_, Option<String>>(4)?, "is_error": row.get::<_, bool>(5)?,
            "parent": row.get::<_, Option<String>>(6)?, "sequence": row.get::<_, Option<i64>>(7)?
        }))
    }).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let violations: usize = conn
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .count();
    assert_eq!(violations, 0);
    rows
}

#[tokio::test]
async fn cli_toolscript_repeated_calls_keep_history_across_model_rounds_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let fixture = Fixture {
        requests: Default::default(),
        data: data.clone(),
        session: session.clone(),
        assistants_before_response: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(data.join("providers.json"), json!([{
        "id":"fixture", "name":"Fixture", "base_url":base, "api_key":"fixture-only",
        "api_format":"openai", "enabled":true,
        "models":[{"id":"fixture-model", "name":"Fixture", "enabled":true,
            "context_window":128000,"max_tokens":4096,"supports_web_search":false,"supports_vision":false}]
    }]).to_string()).unwrap();
    std::fs::write(
        data.join("settings.json"),
        json!({"permissions":{"deny":["Write"]}}).to_string(),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&data)
        .arg("--path")
        .arg(root.path())
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            &session,
            "fixture history task",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(90), cmd.output())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    let rows = snapshot(&data, &session);
    println!(
        "CLI_EXIT={:?}\nSTDERR={}\nDB_ROWS={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        json!(rows)
    );
    let mut jobs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data.join("toolscript-jobs")) {
        for entry in entries {
            let dir = entry.unwrap().path();
            let state: Value =
                serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
            let spec: Value =
                serde_json::from_slice(&std::fs::read(dir.join("spec.json")).unwrap()).unwrap();
            let result: Value =
                serde_json::from_slice(&std::fs::read(dir.join("result.json")).unwrap()).unwrap();
            let log = std::fs::read_to_string(dir.join("output.log")).unwrap();
            jobs.push(json!({"state":state,"spec":spec,"result":result,"log":log}));
        }
    }
    println!("JOBS={}", json!(jobs));
    println!(
        "ASSISTANTS_BEFORE_MODEL_RESPONSE={:?}",
        fixture.assistants_before_response.lock().unwrap()
    );
    assert!(output.status.success());
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        *fixture.assistants_before_response.lock().unwrap(),
        [1, 1, 1, 1]
    );
    for (i, request) in requests.iter().enumerate() {
        let advertised = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "ToolScript");
        assert_eq!(advertised, i > 0, "ToolSearch must load ToolScript");
    }
    let results: Vec<Value> = requests[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["loaded_tools"], json!(["ToolScript"]));
    assert_eq!(jobs.len(), 2, "both model ToolScript calls must run");
    for result in &results[1..] {
        assert_eq!(result["status"], "completed", "{result}");
    }
    let parents: Vec<_> = rows
        .iter()
        .filter(|row| row["name"] == "ToolScript")
        .collect();
    let nested: Vec<_> = rows
        .iter()
        .filter(|row| row["parent"].is_string())
        .collect();
    assert_eq!(parents.len(), 2);
    assert_eq!(nested.len(), 14); // each script: Bash + five Read + refused Write
    assert_eq!(rows.len(), 17); // plus ToolSearch and two ToolScript parents
    let ids: HashSet<_> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), rows.len());
    assert!(ids
        .iter()
        .all(|id| id.contains(":inv-") && !id.contains("same-provider-id")));
    for round in 1..=2 {
        let input = script_input(round);
        let parent = parents
            .iter()
            .find(|row| {
                serde_json::from_str::<Value>(row["input"].as_str().unwrap()).unwrap() == input
            })
            .unwrap();
        let parent_result: Value =
            serde_json::from_str(parent["output"].as_str().unwrap()).unwrap();
        assert_eq!(parent["is_error"], false);
        assert_eq!(parent_result, results[round]);
        assert_eq!(
            parent_result["result"]["value"]["tag"],
            format!("ROUND_{round}")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(format!("marker-{round}.txt"))).unwrap(),
            format!("MARKER_{round}")
        );
        assert!(!root.path().join("denied.txt").exists());
        let job = jobs
            .iter()
            .find(|job| job["spec"]["args"]["tag"] == format!("ROUND_{round}"))
            .unwrap();
        assert_eq!(job["spec"]["assistant_id"], parent["assistant"]);
        let occurrence = parent["id"].as_str().unwrap().split_once(':').unwrap().1;
        assert_eq!(job["spec"]["parent_tool_use_id"], occurrence);
        assert_eq!(job["state"]["calls_started"], 7);
        assert_eq!(job["state"]["calls_succeeded"], 6);
        assert_eq!(job["result"]["calls_failed"], 1);
        assert_eq!(job["result"]["value"]["denied"], "permission_denied");
        let log = job["log"].as_str().unwrap();
        let starts: Vec<_> = log
            .lines()
            .filter(|line| line.starts_with("start "))
            .collect();
        let dones: Vec<_> = log
            .lines()
            .filter(|line| line.starts_with("done "))
            .collect();
        let origins: Vec<_> = log
            .lines()
            .filter(|line| line.starts_with("nested "))
            .collect();
        assert_eq!((starts.len(), dones.len(), origins.len()), (7, 7, 7));
        let mut children: Vec<_> = nested
            .iter()
            .filter(|row| row["parent"] == parent["id"])
            .collect();
        children.sort_by_key(|row| row["sequence"].as_u64().unwrap());
        assert_eq!(children.len(), starts.len());
        for (index, child) in children.iter().enumerate() {
            assert_eq!(child["assistant"], parent["assistant"]);
            assert_eq!(child["sequence"], index + 1);
            let inner = child["id"].as_str().unwrap().split_once(':').unwrap().1;
            let args: Value = serde_json::from_str(child["input"].as_str().unwrap()).unwrap();
            assert!(starts.contains(
                &format!("start {inner} {} {args}", child["name"].as_str().unwrap()).as_str()
            ));
            assert!(origins.contains(
                &format!("nested {inner} parent={occurrence} seq={}", index + 1).as_str()
            ));
            assert!(log.contains(&format!(
                "done {inner} {} error={}\n{}\n",
                child["name"].as_str().unwrap(),
                child["is_error"],
                child["output"].as_str().unwrap()
            )));
            match index {
                0 => {
                    assert_eq!(child["name"], "Bash");
                    assert_eq!(args, json!({"command":input["args"]["command"]}));
                    assert_eq!(child["is_error"], false);
                }
                1..=5 => {
                    assert_eq!(child["name"], "Read");
                    assert_eq!(args, json!({"file_path":"same.txt"}));
                    assert_eq!(child["is_error"], false);
                    assert!(child["output"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("ROUND_{round}")));
                    assert!(!child["output"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("ROUND_{}", 3 - round)));
                }
                6 => {
                    assert_eq!(child["name"], "Write");
                    assert_eq!(
                        args,
                        json!({"file_path":"denied.txt","content":format!("ROUND_{round}")})
                    );
                    assert_eq!(child["is_error"], true);
                    assert!(child["output"].as_str().unwrap().contains("denied"));
                }
                _ => unreachable!(),
            }
        }
    }
    // snapshot() drops every connection; open SQLite independently again.
    let reopened = snapshot(&data, &session);
    assert_eq!(
        reopened, rows,
        "all original inputs/results/origins survive reopen"
    );
    println!(
        "REOPEN rows={} nested={} parents={} unchanged=true foreign_key_check=0",
        reopened.len(),
        nested.len(),
        parents.len()
    );
}
