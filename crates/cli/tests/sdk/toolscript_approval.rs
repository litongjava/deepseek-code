//! Real model -> ToolSearch -> ToolScript -> private worker -> SDK approval.
#![cfg(unix)]
use super::*;

#[derive(Clone)]
struct ScriptFixture {
    source: String,
    background: bool,
    stop: bool,
    requests: Arc<Mutex<Vec<Value>>>,
    stop_release: Arc<tokio::sync::Notify>,
}
struct ScriptServer {
    task: tokio::task::JoinHandle<()>,
    fixture: ScriptFixture,
}
impl Drop for ScriptServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn model(State(f): State<ScriptFixture>, Json(body): Json<Value>) -> Response {
    let index = {
        let mut r = f.requests.lock().unwrap();
        r.push(body.clone());
        r.len() - 1
    };
    let call = match index {
        0 => Some(("search", "ToolSearch", json!({"query":"select:ToolScript"}))),
        1 => Some((
            "script",
            "ToolScript",
            json!({"source":f.source,"run_in_background":f.background}),
        )),
        2 if f.stop => {
            f.stop_release.notified().await;
            let result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "tool")
                .unwrap();
            let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
            Some(("stop", "TaskStop", json!({"task_id":result["task_id"]})))
        }
        _ => None,
    };
    let delta = match call {
        Some((id, name, input)) => {
            json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]})
        }
        None => json!({"content":"approval fixture complete"}),
    };
    let reason = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})
    );
    ([("content-type", "text/event-stream")], stream).into_response()
}
impl ScriptServer {
    async fn start(root: &Path, source: &str, stop: bool) -> Self {
        let fixture = ScriptFixture {
            source: source.into(),
            background: stop,
            stop,
            requests: Default::default(),
            stop_release: Default::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(model))
            .with_state(fixture.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        std::fs::create_dir_all(root.join("data")).unwrap();
        // Authorize only the outer container. Inner Write must still ask (or
        // deny headlessly); loading/allowing ToolScript grants it no authority.
        std::fs::write(
            root.join("data/settings.json"),
            json!({"permissions":{"allow":if stop {vec!["ToolScript", "TaskStop"]} else {vec!["ToolScript"]}}}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("data/providers.json"),json!([{"id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"context_window":128000,"max_tokens":4096,"supports_web_search":false,"supports_vision":false}]}]).to_string()).unwrap();
        Self { task, fixture }
    }
    fn client(&self, root: &Path) -> Client {
        Client::start_configured(
            root,
            &[
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--permission-mode",
                "ask",
            ],
        )
    }
}
const WRITE: &str = r#"try { await tools.call("Write", {file_path:"approved.txt",content:"ACTUAL_INPUT"}); return "wrote"; } catch (error) { return error.code; }"#;

fn answer(pending: &Value, allow: bool) -> Value {
    json!({"type":"control_response","response":{"subtype":"success","request_id":pending["request_id"],"response":if allow {json!({"behavior":"allow"})} else {json!({"behavior":"deny","message":"fixture denied"})}}})
}
fn origin(pending: &Value) -> (&str, &str) {
    assert_eq!(pending["request"]["subtype"], "can_use_tool", "{pending}");
    assert_eq!(pending["request"]["tool_name"], "Write", "{pending}");
    let id = pending["request"]["tool_use_id"].as_str().unwrap();
    let parent = pending["request"]["toolscript"]["parent_tool_use_id"]
        .as_str()
        .unwrap();
    assert!(id.starts_with("inv-"));
    assert!(parent.starts_with("inv-"));
    assert_ne!(id, parent);
    assert!(pending["request"]["description"]
        .as_str()
        .unwrap()
        .contains(parent));
    println!("J4_APPROVAL {pending}");
    (id, parent)
}
fn jobs(root: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(root.join("data/toolscript-jobs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}
async fn terminal(root: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let dirs = jobs(root);
            assert_eq!(dirs.len(), 1);
            let state: Value =
                serde_json::from_slice(&std::fs::read(dirs[0].join("state.json")).unwrap())
                    .unwrap();
            if ["completed", "failed", "cancelled", "interrupted"]
                .contains(&state["status"].as_str().unwrap())
            {
                println!("J4_STATE {state}");
                println!(
                    "J4_LOG {}",
                    std::fs::read_to_string(dirs[0].join("output.log")).unwrap()
                );
                return state;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}
fn rows(root: &Path) -> Vec<Value> {
    let conn = rusqlite::Connection::open(root.join("data/deepseek_code.db")).unwrap();
    let mut stmt=conn.prepare("SELECT t.id,t.name,t.input,t.output,t.is_error,o.parent_tool_call_id FROM tool_calls t LEFT JOIN tool_call_origins o ON o.tool_call_id=t.id ORDER BY t.id").unwrap();
    let rows=stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"input":r.get::<_,Option<String>>(2)?,"output":r.get::<_,Option<String>>(3)?,"is_error":r.get::<_,bool>(4)?,"parent":r.get::<_,Option<String>>(5)?}))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    for row in &rows {
        println!("J4_DB {row}");
    }
    assert_eq!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count(),
        0
    );
    rows
}
fn check_row(rows: &[Value], id: &str, parent: &str, error: bool) {
    let found: Vec<_> = rows
        .iter()
        .filter(|r| r["id"].as_str().unwrap().ends_with(&format!(":{id}")))
        .collect();
    assert_eq!(found.len(), 1, "{rows:?}");
    assert_eq!(found[0]["name"], "Write");
    assert_eq!(found[0]["is_error"], error);
    assert!(found[0]["parent"]
        .as_str()
        .unwrap()
        .ends_with(&format!(":{parent}")));
    assert!(found[0]["output"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn sdk_toolscript_approval_allow_and_deny_keep_input_and_history() {
    for allow in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let server = ScriptServer::start(root.path(), WRITE, false).await;
        let mut client = server.client(root.path());
        client.send(user("run the script")).await;
        let pending = client.frame("control_request").await;
        let (id, parent) = origin(&pending);
        assert_eq!(
            pending["request"]["input"],
            json!({"file_path":"approved.txt","content":"ACTUAL_INPUT"})
        );
        assert!(!root.path().join("approved.txt").exists());
        client.send(answer(&pending, allow)).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(client.finish().await.0.success());
        assert_eq!(terminal(root.path()).await["status"], "completed");
        assert_eq!(root.path().join("approved.txt").exists(), allow);
        if allow {
            assert_eq!(
                std::fs::read_to_string(root.path().join("approved.txt")).unwrap(),
                "ACTUAL_INPUT"
            );
        }
        let db = rows(root.path());
        check_row(&db, id, parent, !allow);
        assert_eq!(db.len(), 3);
        let result: Value = serde_json::from_slice(
            &std::fs::read(jobs(root.path())[0].join("result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["value"],
            if allow { "wrote" } else { "permission_denied" },
            "{result}"
        );
        let requests = server.fixture.requests.lock().unwrap();
        assert!(!requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "ToolScript"));
        assert!(requests[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "ToolScript"));
    }
}

#[tokio::test]
async fn sdk_toolscript_each_call_requires_its_own_approval() {
    let root = tempfile::tempdir().unwrap();
    let source = r#"const results=[]; for (const file_path of ["one.txt","two.txt"]) { try { await tools.call("Write",{file_path,content:"each"}); results.push("ok"); } catch(e) { results.push(e.code); } } return results;"#;
    let server = ScriptServer::start(root.path(), source, false).await;
    let mut client = server.client(root.path());
    client.send(user("run two writes")).await;
    let first = client.frame("control_request").await;
    let (id1, parent1) = origin(&first);
    client.send(answer(&first, true)).await;
    let second = client.frame("control_request").await;
    let (id2, parent2) = origin(&second);
    assert_ne!(first["request_id"], second["request_id"]);
    assert_ne!(id1, id2);
    assert_eq!(parent1, parent2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.txt")).unwrap(),
        "each"
    );
    assert!(!root.path().join("two.txt").exists());
    client.send(answer(&second, false)).await;
    assert_eq!(client.result().await["is_error"], false);
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    let db = rows(root.path());
    assert_eq!(db.len(), 4);
    check_row(&db, id1, parent1, false);
    check_row(&db, id2, parent2, true);
}

#[tokio::test]
async fn sdk_toolscript_eof_cancels_pending_inner_call() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, false).await;
    let mut client = server.client(root.path());
    client.send(user("wait for approval")).await;
    let pending = client.frame("control_request").await;
    let (id, parent) = origin(&pending);
    assert!(!client.finish().await.0.success());
    let state = terminal(root.path()).await;
    assert_eq!(state["status"], "cancelled");
    assert_eq!(state["calls_started"], 1);
    assert_eq!(state["calls_succeeded"], 0);
    assert!(!root.path().join("approved.txt").exists());
    let db = rows(root.path());
    check_row(&db, id, parent, true);
    let row = db.iter().find(|r| r["name"] == "Write").unwrap();
    assert!(
        row["output"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("cancel"),
        "{row}"
    );
    assert!(std::fs::read_dir(jobs(root.path())[0].join("approvals"))
        .unwrap()
        .next()
        .is_none());
}

#[tokio::test]
async fn sdk_toolscript_taskstop_invalidates_pending_approval() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, true).await;
    let mut client = server.client(root.path());
    client.send(user("start then stop script")).await;
    let pending = client.frame("control_request").await;
    let (id, parent) = origin(&pending);
    server.fixture.stop_release.notify_one();
    assert_eq!(
        client.frame("control_cancel_request").await["request_id"],
        pending["request_id"]
    );
    client.send(answer(&pending, true)).await; // A late allow must not resurrect the invocation.
    assert_eq!(client.result().await["is_error"], false);
    assert!(client.finish().await.0.success());
    let state = terminal(root.path()).await;
    assert_eq!(state["status"], "cancelled");
    assert_eq!(state["calls_started"], 1);
    assert_eq!(state["calls_succeeded"], 0);
    assert!(!root.path().join("approved.txt").exists());
    check_row(&rows(root.path()), id, parent, true);
}

#[tokio::test]
async fn sdk_toolscript_policy_refresh_while_waiting_revokes_allow() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, false).await;
    let mut client = server.client(root.path());
    client.send(user("wait for approval")).await;
    let pending = client.frame("control_request").await;
    let (id, parent) = origin(&pending);
    // The decision is delivered after a durable deny; execution must re-read it
    // even when this races the periodic 500ms refresh.
    std::fs::write(
        root.path().join("data/settings.json"),
        r#"{"permissions":{"deny":["Write"]}}"#,
    )
    .unwrap();
    client.send(answer(&pending, true)).await;
    assert_eq!(client.result().await["is_error"], false);
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    assert!(!root.path().join("approved.txt").exists());
    check_row(&rows(root.path()), id, parent, true);
}

#[tokio::test]
async fn sdk_toolscript_corrupt_policy_cancels_waiting_prompt() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, false).await;
    let mut client = server.client(root.path());
    client.send(user("wait for approval")).await;
    let pending = client.frame("control_request").await;
    let (id, parent) = origin(&pending);
    std::fs::write(
        root.path().join("data/settings.json"),
        r#"{"permissions":{"deny":7}}"#,
    )
    .unwrap();
    assert_eq!(
        client.frame("control_cancel_request").await["request_id"],
        pending["request_id"]
    );
    client.result().await;
    assert!(client.finish().await.0.success());
    let state = terminal(root.path()).await;
    assert_eq!(state["termination"], "policy_refresh_failed");
    assert!(!root.path().join("approved.txt").exists());
    check_row(&rows(root.path()), id, parent, true);
}

#[tokio::test]
async fn cli_toolscript_print_without_approval_host_denies() {
    let root = tempfile::tempdir().unwrap();
    let _server = ScriptServer::start(root.path(), WRITE, false).await;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(env!("CARGO_BIN_EXE_dcl"))
            .args(["--print", "--output-format", "stream-json", "--data-dir"])
            .arg(root.path().join("data"))
            .arg("--path")
            .arg(root.path())
            .args([
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--permission-mode",
                "ask",
                "run script",
            ])
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("approved.txt").exists());
    terminal(root.path()).await;
    let db = rows(root.path());
    let write = db.iter().find(|r| r["name"] == "Write").unwrap();
    assert_eq!(write["is_error"], true);
    assert!(write["output"]
        .as_str()
        .unwrap()
        .contains("no approval host"));
}

#[tokio::test]
async fn sdk_toolscript_persistent_allow_rule_applies_to_later_calls() {
    let root = tempfile::tempdir().unwrap();
    let source = r#"for (const file_path of ["one.txt","two.txt"]) { await tools.call("Write",{file_path,content:"remembered"}); } return "done";"#;
    let server = ScriptServer::start(root.path(), source, false).await;
    let mut client = server.client(root.path());
    client.send(user("approve with a rule")).await;
    let pending = client.frame("control_request").await;
    origin(&pending);
    let mut reply = answer(&pending, true);
    reply["response"]["response"]["updatedPermissions"] = json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]);
    client.send(reply).await;
    loop {
        let frame = client.next_frame().await;
        assert_ne!(
            frame["type"], "control_request",
            "persistent rule must avoid another prompt: {frame}"
        );
        if frame["type"] == "result" {
            assert_eq!(frame["is_error"], false);
            break;
        }
    }
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    for name in ["one.txt", "two.txt"] {
        assert_eq!(
            std::fs::read_to_string(root.path().join(name)).unwrap(),
            "remembered"
        );
    }
    let db = rows(root.path());
    assert_eq!(
        db.iter()
            .filter(|r| r["name"] == "Write" && r["is_error"] == false)
            .count(),
        2
    );
}

#[tokio::test]
async fn sdk_toolscript_approved_updated_input_is_the_recorded_input() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, false).await;
    let mut client = server.client(root.path());
    client.send(user("approve revised write")).await;
    let pending = client.frame("control_request").await;
    let (id, parent) = origin(&pending);
    let mut reply = answer(&pending, true);
    let updated = json!({"file_path":"revised.txt","content":"REVIEWED"});
    reply["response"]["response"]["updatedInput"] = updated.clone();
    client.send(reply).await;
    assert_eq!(client.result().await["is_error"], false);
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    assert!(!root.path().join("approved.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("revised.txt")).unwrap(),
        "REVIEWED"
    );
    let db = rows(root.path());
    check_row(&db, id, parent, false);
    let row = db.iter().find(|r| r["name"] == "Write").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(row["input"].as_str().unwrap()).unwrap(),
        updated
    );
}

#[tokio::test]
async fn sdk_toolscript_host_without_can_use_tool_denies_without_prompt() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, false).await;
    let mut client = server.client(root.path());
    client
        .send(initialize_request("init", json!({"canUseTool":false})))
        .await;
    assert_eq!(
        client.frame("control_response").await["response"]["subtype"],
        "success"
    );
    client.send(user("run without approval capability")).await;
    loop {
        let frame = client.next_frame().await;
        assert_ne!(frame["type"], "control_request", "{frame}");
        if frame["type"] == "result" {
            assert_eq!(frame["is_error"], false);
            break;
        }
    }
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    assert!(!root.path().join("approved.txt").exists());
    let db = rows(root.path());
    let row = db.iter().find(|r| r["name"] == "Write").unwrap();
    assert_eq!(row["is_error"], true);
    assert!(row["output"].as_str().unwrap().contains("no approval host"));
}

#[tokio::test]
async fn sdk_toolscript_super_gates_precede_the_approval_host() {
    use deepseek_code_core::{
        runtime::{RuntimePaths, SessionStore},
        super_agent::{self, crp::ToolRuntime, Role, TaskSpec},
    };
    for coder in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let _server = ScriptServer::start(root.path(), WRITE, false).await;
        let data = root.path().join("data");
        let db = RuntimePaths::new(data.clone())
            .unwrap()
            .open_database()
            .unwrap();
        let store = SessionStore::new(db.clone());
        let session = store
            .create(root.path(), "fixture-model", "fixture")
            .unwrap();
        if coder {
            let router = store
                .create(root.path(), "fixture-model", "fixture")
                .unwrap();
            super_agent::role::set(&db, &router, Role::Router).unwrap();
            let runtime = ToolRuntime {
                db: db.clone(),
                data_dir: data.clone(),
                session_id: session.clone(),
            };
            let tasks = runtime.store();
            let task = tasks
                .create_task(
                    &router,
                    TaskSpec {
                        title: "J4 gate".into(),
                        workspace: root.path().to_path_buf(),
                        targets: vec![root.path().to_string_lossy().into_owned()],
                        goal: "gate".into(),
                        requirements: vec![],
                        acceptance_criteria: vec![],
                        constraints: vec![],
                        out_of_scope: vec![],
                        target_branch: None,
                        repository_target_branches: Default::default(),
                    },
                    "j4-gate",
                )
                .unwrap();
            tasks
                .set_sessions(&task.task_id, Some(&session), None)
                .unwrap();
        } else {
            super_agent::role::set(&db, &session, Role::Router).unwrap();
        }
        let mut client = Client::start_configured(
            root.path(),
            &[
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--resume",
                &session,
            ],
        );
        client.send(user("run gated script")).await;
        loop {
            let frame = client.next_frame().await;
            assert_ne!(
                frame["type"], "control_request",
                "gate must run before approval: {frame}"
            );
            if frame["type"] == "result" {
                assert_eq!(frame["is_error"], false);
                break;
            }
        }
        assert!(client.finish().await.0.success());
        terminal(root.path()).await;
        assert!(!root.path().join("approved.txt").exists());
        let db = rows(root.path());
        let row = db.iter().find(|r| r["name"] == "Write").unwrap();
        assert_eq!(row["is_error"], true);
        assert!(
            row["output"].as_str().unwrap().contains(if coder {
                "until the host approves"
            } else {
                "cannot edit business code"
            }),
            "{row}"
        );
    }
}

#[tokio::test]
async fn sdk_toolscript_hook_ask_is_not_bypassed_by_an_allow_rule() {
    let root = tempfile::tempdir().unwrap();
    let server = ScriptServer::start(root.path(), WRITE, false).await;
    let hook =
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}});
    std::fs::write(
        root.path().join("data/settings.json"),
        json!({
            "permissions":{"allow":["ToolScript","Write"]},
            "hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{
                "type":"command","shell":"sh","command":format!("printf '%s' '{}'", hook)
            }]}]}
        })
        .to_string(),
    )
    .unwrap();
    let mut client = server.client(root.path());
    client
        .send(user("hook requires review despite the allow rule"))
        .await;
    let pending = client.frame("control_request").await;
    let (id, parent) = origin(&pending);
    assert!(!root.path().join("approved.txt").exists());
    client.send(answer(&pending, false)).await;
    assert_eq!(client.result().await["is_error"], false);
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    assert!(!root.path().join("approved.txt").exists());
    check_row(&rows(root.path()), id, parent, true);
    let result: Value =
        serde_json::from_slice(&std::fs::read(jobs(root.path())[0].join("result.json")).unwrap())
            .unwrap();
    assert_eq!(result["value"], "permission_denied");
}

#[tokio::test]
async fn sdk_toolscript_hook_still_asks_after_a_session_rule_is_committed() {
    let root = tempfile::tempdir().unwrap();
    let source = r#"const results=[]; for (const file_path of ["one.txt","two.txt"]) { try { await tools.call("Write",{file_path,content:"hook"}); results.push("ok"); } catch(e) { results.push(e.code); } } return results;"#;
    let server = ScriptServer::start(root.path(), source, false).await;
    let hook =
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}});
    std::fs::write(
        root.path().join("data/settings.json"),
        json!({
            "permissions":{"allow":["ToolScript"]},
            "hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{
                "type":"command","shell":"sh","command":format!("printf '%s' '{}'", hook)
            }]}]}
        })
        .to_string(),
    )
    .unwrap();
    let mut client = server.client(root.path());
    client.send(user("review both hook requests")).await;
    let first = client.frame("control_request").await;
    let (id1, parent1) = origin(&first);
    let mut reply = answer(&first, true);
    reply["response"]["response"]["updatedPermissions"] = json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]);
    client.send(reply).await;
    let second = client.frame("control_request").await;
    let (id2, parent2) = origin(&second);
    assert_ne!(id1, id2);
    assert_eq!(parent1, parent2);
    client.send(answer(&second, false)).await;
    assert_eq!(client.result().await["is_error"], false);
    assert!(client.finish().await.0.success());
    terminal(root.path()).await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.txt")).unwrap(),
        "hook"
    );
    assert!(!root.path().join("two.txt").exists());
    let db = rows(root.path());
    assert_eq!(db.len(), 4);
    check_row(&db, id1, parent1, false);
    check_row(&db, id2, parent2, true);
    let result: Value =
        serde_json::from_slice(&std::fs::read(jobs(root.path())[0].join("result.json")).unwrap())
            .unwrap();
    assert_eq!(result["value"], json!(["ok", "permission_denied"]));
}
