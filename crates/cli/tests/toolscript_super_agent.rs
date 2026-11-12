//! Exercise Super authority through the actual private CLI worker, with a
//! durable session/task. The plan comparison also drives direct CLI calls with
//! a local model mock; the other cases need no model request.
#![cfg(unix)]

use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    super_agent::{self, crp::ToolRuntime, ApprovalStatus, ApprovalType, Role, TaskSpec},
};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

struct Fixture {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    runtime: ToolRuntime,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let data_dir = temp.path().join("data");
        let db = RuntimePaths::new(data_dir.clone())
            .unwrap()
            .open_database()
            .unwrap();
        let session_id = SessionStore::new(db.clone())
            .create(&workspace, "fixture", "fixture")
            .unwrap();
        std::fs::write(
            data_dir.join("providers.json"),
            json!([{
                "id":"fixture", "name":"fixture", "base_url":"http://127.0.0.1:1",
                "api_key":"unused", "api_format":"openai", "enabled":true,
                "models":[{"id":"fixture","name":"fixture","enabled":true,
                    "supports_vision":false,"supports_web_search":false}]
            }])
            .to_string(),
        )
        .unwrap();
        Self {
            _temp: temp,
            workspace,
            runtime: ToolRuntime {
                db,
                data_dir,
                session_id,
            },
        }
    }

    async fn worker(&self, phase: &str, source: &str) -> (Value, Value, String) {
        let r = &self.runtime;
        let task = deepseek_code_core::tools::tasks::execute_for_session_async(
            "TaskCreate",
            json!({"subject":phase}),
            &r.data_dir,
            &r.session_id,
        )
        .await
        .unwrap();
        let id = task["id"].as_str().unwrap();
        let job = r.data_dir.join("toolscript-jobs").join(id);
        std::fs::create_dir_all(&job).unwrap();
        // Deliberately permissive admission snapshot: durable Super identity
        // and durable plan mode must still restrict every nested call.
        std::fs::write(
            job.join("spec.json"),
            json!({
                "id":id,"session":r.session_id,"workspace":self.workspace,
                "assistant_id":null,"parent_tool_use_id":format!("inv-{phase}"),
                "provider_id":"fixture","model":"fixture","source":source,"args":null,
                "data_dir":r.data_dir,"settings_path":r.data_dir.join("config.json"),
                "loaded_tools":["ApplyPatch"],"reasoning":"standard","permission_mode":"bypass",
                "allowed_tools":null,"exposed_tools":null,"source_sha256":"","interactive":false
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(job.join("state.json"), json!({
            "id":id,"kind":"toolscript","session":r.session_id,"parent_tool_use_id":format!("inv-{phase}"),
            "assistant_id":null,"status":"starting","owner_pid":null,"started_ms":0,
            "finished_ms":null,"termination":null,"error":null,"calls_started":0,"calls_succeeded":0
        }).to_string()).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("DEEPSEEK_CODE_")) {
            command.env_remove(key);
        }
        let output = tokio::time::timeout(
            Duration::from_secs(45),
            command
                .env("DEEPSEEK_CODE_TOOLSCRIPT_JOB", &job)
                .current_dir(&self.workspace)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("private worker timed out")
        .unwrap();
        let state: Value =
            serde_json::from_slice(&std::fs::read(job.join("state.json")).unwrap()).unwrap();
        let result: Value = std::fs::read(job.join("result.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or(Value::Null);
        let log = std::fs::read_to_string(job.join("output.log")).unwrap_or_default();
        println!(
            "{phase}: exit={} state={state} result={result}\n{log}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        (state, result, log)
    }

    fn assert_unchanged(&self) {
        assert_eq!(
            std::fs::read_to_string(self.workspace.join("edit.txt")).unwrap(),
            "before\n"
        );
        for name in ["write.txt", "patch.txt", "bash.txt"] {
            assert!(
                !self.workspace.join(name).exists(),
                "unexpected write: {name}"
            );
        }
    }
}

// Read before Edit also proves that a useful safe nested tool works. Capture
// structured denials so the script can finish, then inspect both its result
// and the actual filesystem; a completed script alone is not security proof.
const SCRIPT: &str = r#"
await tools.call("Read", {file_path:"edit.txt"});
const calls = [
 ["Write", {file_path:"write.txt",content:"written"}],
 ["Edit", {file_path:"edit.txt",old_string:"before",new_string:"after"}],
 ["ApplyPatch", {patch:"*** Begin Patch\n*** Add File: patch.txt\n+patched\n*** End Patch"}],
 ["Bash", {command:"printf shell > bash.txt"}]
];
const results = [];
for (const [name,input] of calls) {
 try { await tools.call(name,input); results.push({name,ok:true}); }
 catch (error) { results.push({name,ok:false,code:error.code,message:error.message}); }
}
return results;
"#;

fn assert_denied(result: &Value, count: usize, reason: &str) {
    let rows = result["value"]
        .as_array()
        .expect("script must return per-tool results");
    assert_eq!(rows.len(), count);
    for row in rows {
        assert_eq!(row["ok"], false, "{row}");
        assert_eq!(row["code"], "permission_denied", "{row}");
        assert!(row["message"].as_str().unwrap().contains(reason), "{row}");
    }
}

#[tokio::test]
async fn router_private_toolscript_cannot_write_edit_or_apply_patch() {
    let f = Fixture::new();
    super_agent::role::set(&f.runtime.db, &f.runtime.session_id, Role::Router).unwrap();
    std::fs::write(f.workspace.join("edit.txt"), "before\n").unwrap();
    // Router Bash follows the existing direct-call policy; test the three
    // business edit tools which the shared Router gate unconditionally denies.
    let source = SCRIPT.replace(
        "for (const [name,input] of calls)",
        "for (const [name,input] of calls.slice(0,3))",
    );
    let (state, result, _) = f.worker("router", &source).await;
    assert_eq!(state["status"], "completed");
    assert_eq!(state["calls_started"], 4);
    assert_eq!(state["calls_succeeded"], 1);
    assert_denied(&result, 3, "cannot edit business code");
    f.assert_unchanged();
}

#[tokio::test]
async fn coder_private_toolscript_requires_current_plan_approval_for_every_write_path() {
    let f = Fixture::new();
    let r = &f.runtime;
    let router = SessionStore::new(r.db.clone())
        .create(&f.workspace, "fixture", "fixture")
        .unwrap();
    super_agent::role::set(&r.db, &router, Role::Router).unwrap();
    let store = r.store();
    let task = store
        .create_task(
            &router,
            TaskSpec {
                title: "ToolScript gate".into(),
                workspace: f.workspace.clone(),
                targets: vec![f.workspace.to_string_lossy().into_owned()],
                goal: "gate".into(),
                requirements: vec![],
                acceptance_criteria: vec![],
                constraints: vec![],
                out_of_scope: vec![],
                target_branch: None,
                repository_target_branches: Default::default(),
            },
            "create",
        )
        .unwrap();
    store
        .set_sessions(&task.task_id, Some(&r.session_id), None)
        .unwrap();
    std::fs::write(f.workspace.join("edit.txt"), "before\n").unwrap();
    for phase in ["unapproved", "stale", "approved"] {
        if phase != "unapproved" {
            let approval = store
                .request_approval(
                    phase,
                    &task.task_id,
                    super_agent::store::ApprovalDraft {
                        source_session_id: r.session_id.clone(),
                        parent_session_id: router.clone(),
                        request_type: ApprovalType::Plan,
                        request: json!({"round":task.review_round}),
                        task_spec_version: task.spec_version + u32::from(phase == "stale"),
                        permission_policy_version: task.permission_policy_version,
                    },
                )
                .unwrap();
            store
                .resolve_approval(
                    &format!("answer-{phase}"),
                    &approval.approval_id,
                    ApprovalStatus::Approved,
                    None,
                )
                .unwrap();
        }
        let (state, result, _) = f.worker(phase, SCRIPT).await;
        assert_eq!(state["status"], "completed");
        assert_eq!(state["calls_started"], 5);
        if phase == "approved" {
            assert_eq!(state["calls_succeeded"], 5);
            for row in result["value"].as_array().unwrap() {
                assert_eq!(row["ok"], true, "{row}");
            }
            assert_eq!(
                std::fs::read_to_string(f.workspace.join("edit.txt")).unwrap(),
                "after\n"
            );
            assert_eq!(
                std::fs::read_to_string(f.workspace.join("write.txt")).unwrap(),
                "written"
            );
            assert_eq!(
                std::fs::read_to_string(f.workspace.join("patch.txt")).unwrap(),
                "patched\n"
            );
            assert_eq!(
                std::fs::read_to_string(f.workspace.join("bash.txt")).unwrap(),
                "shell"
            );
        } else {
            assert_eq!(state["calls_succeeded"], 1);
            assert_denied(&result, 4, "blocked until the host approves");
            f.assert_unchanged();
        }
    }
}

#[tokio::test]
async fn ordinary_plan_mode_matches_direct_bash_and_rejects_edit_tools() {
    use axum::{response::IntoResponse, routing::post, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    const ORDINARY: &str = "printf shell > bash.txt";
    // `eval` is rejected by the existing danger filter. Even if a regression
    // executes it, the payload only writes a marker in this temporary fixture.
    const DANGEROUS: &str = "eval 'printf blocked > danger.txt'";
    assert!(!deepseek_code_core::tools::is_dangerous_command(ORDINARY));
    assert!(deepseek_code_core::tools::is_dangerous_command(DANGEROUS));
    let f = Fixture::new();
    let permissions = deepseek_code_core::runtime::default_permissions();
    deepseek_code_core::services::session_permission_mode::set(
        &f.runtime.db,
        &f.runtime.session_id,
        &permissions,
        deepseek_code_core::permissions::PermissionMode::PlanMode,
    )
    .unwrap();

    // Drive the ordinary CLI executor, not a permission-manager simulation.
    // Both direct calls and the private worker use this same durable session.
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route("/v1/chat/completions", post(move || {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        async move {
            let (delta, finish) = if first {
                (json!({"tool_calls":[
                    {"index":0,"id":"ordinary","type":"function","function":{"name":"Bash","arguments":json!({"command":ORDINARY}).to_string()}},
                    {"index":1,"id":"dangerous","type":"function","function":{"name":"Bash","arguments":json!({"command":DANGEROUS}).to_string()}}
                ]}), "tool_calls")
            } else {
                (json!({"content":"direct comparison complete"}), "stop")
            };
            let stream = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
            ([("content-type", "text/event-stream")], stream).into_response()
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Abort on scope exit, including an assertion failure.
    struct StopServer(tokio::task::AbortHandle);
    impl Drop for StopServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server_guard = StopServer(server.abort_handle());
    let provider_path = f.runtime.data_dir.join("providers.json");
    let mut providers: Value =
        serde_json::from_slice(&std::fs::read(&provider_path).unwrap()).unwrap();
    providers[0]["base_url"] = json!(base);
    std::fs::write(&provider_path, providers.to_string()).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("DEEPSEEK_CODE_")) {
        command.env_remove(key);
    }
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        command
            .args(["--print", "--output-format", "stream-json", "--data-dir"])
            .arg(&f.runtime.data_dir)
            .arg("--path")
            .arg(&f.workspace)
            .args([
                "--resume",
                &f.runtime.session_id,
                "--provider",
                "fixture",
                "--model",
                "fixture",
                "--permission-mode",
                "plan",
                "--",
                "compare direct plan Bash calls",
            ])
            .current_dir(&f.workspace)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("direct CLI timed out")
    .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "direct CLI: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let direct: Vec<(String, String, bool)> = f.runtime.db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT t.input, t.output, t.is_error FROM tool_calls t JOIN messages m ON m.id=t.message_id \
             WHERE m.conversation_id=?1 AND t.name='Bash' ORDER BY t.sort_order",
        )?;
        let rows = stmt.query_map([&f.runtime.session_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, rusqlite::Error>(rows)
    }).unwrap().unwrap();
    assert_eq!(direct.len(), 2, "{direct:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&direct[0].0).unwrap(),
        json!({"command":ORDINARY})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&direct[1].0).unwrap(),
        json!({"command":DANGEROUS})
    );
    assert!(!direct[0].2, "ordinary direct Bash failed: {direct:?}");
    assert!(direct[1].2, "dangerous direct Bash was allowed: {direct:?}");
    assert!(direct[1]
        .1
        .contains("Plan mode: dangerous shell command is not allowed"));
    let direct_content = std::fs::read_to_string(f.workspace.join("bash.txt")).unwrap();
    assert_eq!(direct_content, "shell");
    assert!(!f.workspace.join("danger.txt").exists());
    std::fs::remove_file(f.workspace.join("bash.txt")).unwrap();
    assert_eq!(
        deepseek_code_core::services::session_permission_mode::load(
            &f.runtime.db,
            &f.runtime.session_id
        )
        .unwrap(),
        Some(deepseek_code_core::permissions::PermissionMode::PlanMode)
    );

    let calls = json!([
        ["Write", {"file_path":"write.txt","content":"written"}],
        ["Edit", {"file_path":"edit.txt","old_string":"before","new_string":"after"}],
        ["ApplyPatch", {"patch":"*** Begin Patch\n*** Add File: patch.txt\n+patched\n*** End Patch"}],
        ["Bash", {"command":ORDINARY}],
        ["Bash", {"command":DANGEROUS}]
    ]);
    let source = r#"
        const calls = __CALLS__;
        const results = [];
        for (const [name,input] of calls) {
            try { const output = await tools.call(name,input); results.push({name,ok:true,output}); }
            catch (error) { results.push({name,ok:false,code:error.code,message:error.message}); }
        }
        return results;
    "#.replace("__CALLS__", &calls.to_string());
    let (state, result, _) = f.worker("plan", &source).await;
    assert_eq!(state["status"], "completed");
    assert_eq!(state["calls_started"], 5);
    assert_eq!(state["calls_succeeded"], 1);
    let rows = result["value"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert_denied(
        &json!({"value": &rows[..3]}),
        3,
        "Plan mode: write operations are not allowed",
    );
    for (index, name) in ["Write", "Edit", "ApplyPatch"].iter().enumerate() {
        assert_eq!(rows[index]["name"], *name);
    }
    for file in ["write.txt", "edit.txt", "patch.txt", "danger.txt"] {
        assert!(!f.workspace.join(file).exists(), "unexpected file: {file}");
    }
    for (nested, direct) in rows[3..].iter().zip(&direct) {
        assert_eq!(nested["name"], "Bash");
        assert_eq!(
            nested["ok"], !direct.2,
            "nested={nested}, direct={direct:?}"
        );
    }
    assert_eq!(rows[4]["code"], "permission_denied");
    assert_eq!(rows[4]["message"], direct[1].1);
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("bash.txt")).unwrap(),
        direct_content
    );
    println!("PLAN_DIRECT_COMPARISON session={} direct={direct:?} nested={} bash_content={direct_content:?}", f.runtime.session_id, result["value"]);
}

#[tokio::test]
async fn corrupt_super_authority_fails_private_worker_before_any_call() {
    let f = Fixture::new();
    f.runtime
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,'invalid-role')",
                [format!("super-agent.role.{}", f.runtime.session_id)],
            )
        })
        .unwrap()
        .unwrap();
    let (state, result, log) = f
        .worker(
            "invalid-role",
            "return await tools.call('Write',{file_path:'write.txt',content:'bad'});",
        )
        .await;
    assert_eq!(state["status"], "failed");
    assert_eq!(state["termination"], "initialization_failed");
    assert_eq!(state["calls_started"], 0);
    assert!(state["error"].as_str().unwrap().contains("Unknown role"));
    assert_eq!(result, Value::Null);
    assert!(!log.contains("start "));
    assert!(!f.workspace.join("write.txt").exists());
}
