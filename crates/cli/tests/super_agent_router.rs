//! End-to-end stage 3 through production paths.
//!
//! A real `dcl` router turn calls CrpCreate; the host tick prepares the task and
//! launches a real `dcl` Coder, whose mock model calls CrpSubmit(plan); the host
//! requests approval; a second real `dcl` router turn answers it with CrpAnswer;
//! the tick resumes the same Coder. Only a local mock endpoint is used.
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{EnvironmentCredentials, ServiceContext},
    super_agent::{
        config::SuperAgentConfig, crp::ToolRuntime, orchestrator::Orchestrator, role,
        ApprovalStatus, Phase, Role, Status,
    },
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn stream(content: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":content,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    )
}

fn tool_call(id: &str, name: &str, input: Value) -> Value {
    json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]})
}

fn system_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn tool_results_this_turn(body: &Value) -> Vec<String> {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last_user = messages
        .iter()
        .rposition(|message| message["role"] == "user")
        .unwrap_or(0);
    messages[last_user.saturating_add(1)..]
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect()
}

fn has_tool_result_this_turn(body: &Value) -> bool {
    !tool_results_this_turn(body).is_empty()
}


async fn dcl(data: &Path, session: &str, prompt: &str) -> String {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--resume",
            session,
            "--print",
            "--output-format",
            "text",
            "--permission-mode",
            "accept-edits",
            "--data-dir",
            data.to_str().unwrap(),
            "--",
            prompt,
        ])
        .env("DEEPSEEK_CODE_CHILD_API_KEY", "fixture-key")
        .env_remove("DEEPSEEK_CODE_WORKER_MODEL")
        .env_remove("DEEPSEEK_CODE_WORKER_AUTO_ROUTE")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "dcl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_role_creates_launches_and_approves_a_real_task() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("file.txt"), "main\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    git(&repo, &["switch", "-q", "-c", "develop"]);
    std::fs::write(repo.join("file.txt"), "develop\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "develop"]);
    git(&repo, &["switch", "-q", "main"]);
    let repo = repo.canonicalize().unwrap();
    let project = temp.path().canonicalize().unwrap();

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = seen.clone();
    let spec = json!({
        "title": "Real router task",
        "workspace": project.to_string_lossy(),
        "targets": [repo.to_string_lossy()],
        "goal": "Update the fixture file",
        "requirements": ["Keep the repository valid"],
        "acceptance_criteria": [{"id":"AC-001","description":"file.txt changed on the task branch"}],
        "constraints": [],
        "out_of_scope": [],
        "target_branch": "develop"
    });
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let spec = spec.clone();
            let captured = captured.clone();
            async move {
                let system = system_text(&body);
                let prompt = last_user(&body);
                let after_tool = has_tool_result_this_turn(&body);
                let tool_results = tool_results_this_turn(&body);
                let mode = if system.contains("Super Agent Router") {
                    "router"
                } else if system.contains("/ Coder") {
                    "coder"
                } else {
                    "other"
                };
                captured
                    .lock()
                    .unwrap()
                    .push(format!("{mode}:{prompt}:after_tool={after_tool}"));
                if mode == "coder" && prompt.contains("was approved") && !tool_results.is_empty() {
                    captured
                        .lock()
                        .unwrap()
                        .push(format!("approved-tool-result={}", tool_results.join("|")));
                }
                let response = match mode {
                    "router" if !after_tool && prompt.starts_with("APPROVE_PLAN:") => {
                        let approval = prompt.trim_start_matches("APPROVE_PLAN:").trim();
                        stream(
                            tool_call(
                                "answer-1",
                                "CrpAnswer",
                                json!({"approval_id": approval, "decision": "approved"}),
                            ),
                            "tool_calls",
                        )
                    }
                    "router" if !after_tool => stream(
                        tool_call("create-1", "CrpCreate", json!({"spec": spec})),
                        "tool_calls",
                    ),
                    "coder"
                        if prompt.contains("produce an implementation plan")
                            && !prompt.contains("was approved")
                            && tool_results.is_empty() =>
                    {
                        // This must be denied before the host approves the plan.
                        stream(
                            tool_call(
                                "blocked-bash",
                                "Bash",
                                json!({"command":"touch blocked-before-approval.txt"}),
                            ),
                            "tool_calls",
                        )
                    }
                    "coder"
                        if prompt.contains("produce an implementation plan")
                            && !prompt.contains("was approved")
                            && tool_results.len() == 1 =>
                    {
                        stream(
                            tool_call(
                                "plan-1",
                                "CrpSubmit",
                                json!({
                                    "kind": "plan",
                                    "payload": {
                                        "spec_version": 1,
                                        "round": 0,
                                        "understanding": "Fixture",
                                        "scope": ["file.txt"],
                                        "steps": ["change the file"],
                                        "verification": ["git diff"],
                                        "risks": []
                                    }
                                }),
                            ),
                            "tool_calls",
                        )
                    }
                    "coder" if prompt.contains("was approved") && tool_results.is_empty() => {
                        // The host approved the plan; this read-only shell call
                        // proves the plan gate released without needing a prompt.
                        stream(
                            tool_call("approved-bash", "Bash", json!({"command":"true"})),
                            "tool_calls",
                        )
                    }
                    "coder" if prompt.contains("was approved") => {
                        stream(json!({"content":"implementation acknowledged"}), "stop")
                    }
                    _ => stream(json!({"content":"done"}), "stop"),
                };
                ([("content-type", "text/event-stream")], response)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    std::fs::write(
        data.join("config.json"),
        serde_json::to_vec(&json!({
            "providers":[{
                "id":"fixture","name":"Fixture","provider_type":"openai","api_key":"fixture-key",
                "base_url":format!("http://{address}"),
                "models":[{
                    "id":"router-model","name":"router-model","enabled":true,"max_tokens":4096,
                    "supports_tools":true,"supports_streaming":true,
                    "context_window":128000,"cost_per_1k_input":null,"cost_per_1k_output":null
                }],
                "enabled":true,"is_default":true,"settings":{},
                "supports_web_search":false,"web_search_strategy":null,
                "web_search_tested_at":null,"web_search_test_reason":null
            }],
            "default_provider_id":"fixture"
        }))
        .unwrap(),
    )
    .unwrap();

    let context = ServiceContext::open(
        RuntimePaths::new(data.clone()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let sessions = SessionStore::new(context.db.clone());
    let router = sessions
        .create(&project, "router-model", "fixture")
        .unwrap();
    role::set(&context.db, &router, Role::Router).unwrap();

    // 1. A real router turn creates the task through the model-facing tool.
    let first = dcl(&data, &router, "Create one task for the fixture repository").await;
    assert!(!first.is_empty());
    let tasks = context
        .db
        .with_conn(|conn| {
            let mut statement = conn.prepare("SELECT task_id,router_session_id FROM sa_tasks")?;
            let mut rows = Vec::new();
            let mut mapped = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            while let Some(row) = mapped.next() {
                rows.push(row?);
            }
            Ok::<_, anyhow::Error>(rows)
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "router turn did not create a task: {tasks:?}"
    );
    assert_eq!(tasks[0].1, router);
    let task_id = tasks[0].0.clone();
    let store = deepseek_code_core::super_agent::TaskStore::new(context.db.clone(), data.clone());

    // 2. The host tick prepares the environment and starts a real Coder.
    let provider = deepseek_code_core::native_engine::provider_manager::ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id":"fixture","name":"Fixture","base_url":format!("http://{address}"),
            "api_key":"fixture-key","api_format":"openai","models":[],"enabled":true
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id":"router-model","name":"router-model","enabled":true,
            "supports_vision":false,"supports_web_search":false
        }))
        .unwrap(),
    };
    let orchestrator = Orchestrator::new(
        context.clone(),
        SuperAgentConfig::default(),
        provider,
        "bypass".into(),
        Some(PathBuf::from(env!("CARGO_BIN_EXE_dcl"))),
    );
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    wait_for_jobs(&data, &router, 1, &seen).await;
    let coder = store
        .get_task(&task_id)
        .unwrap()
        .unwrap()
        .coder
        .session_id
        .expect("coder session");

    // The Coder's own CLI turn submitted the plan through CrpSubmit.
    let reports = store.reports(&task_id).unwrap();
    assert!(
        reports
            .iter()
            .any(|report| report.kind == deepseek_code_core::super_agent::ReportKind::Plan),
        "coder did not submit a plan: {reports:?}; requests={:#?}",
        seen.lock().unwrap()
    );

    // 3. The tick turns the plan into a manual approval.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.approvals.len(), 1, "{report:?}");
    let approval = store.approvals(&task_id).unwrap().remove(0);
    assert_eq!(approval.status, ApprovalStatus::Pending);
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.status, Status::WaitingUser);
    let phase_before = task.phase;

    // 4. A real router turn answers the existing approval.
    dcl(
        &data,
        &router,
        &format!("APPROVE_PLAN:{}", approval.approval_id),
    )
    .await;
    assert_eq!(
        store
            .approvals(&task_id)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.approval_id == approval.approval_id)
            .unwrap()
            .status,
        ApprovalStatus::Approved
    );

    // 5. The tick resumes the same Coder, which runs in the task workspace.
    let report = orchestrator.tick(&router).await.unwrap();
    assert!(
        report
            .launched
            .iter()
            .any(|line| line.contains("(resumed)")),
        "{report:?}"
    );
    wait_for_jobs(&data, &router, 2, &seen).await;
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
    assert_eq!(task.phase, Phase::Implementing);
    assert_ne!(phase_before, task.phase);
    let coder_row = context
        .db
        .with_conn(|conn| deepseek_code_core::db::conversation_repo::get_conversation(conn, &coder))
        .unwrap()
        .unwrap()
        .unwrap();
    let coder_workspace = PathBuf::from(coder_row.workspace_path.unwrap());
    assert!(coder_workspace.join("file.txt").is_file());
    assert!(
        !coder_workspace.join("blocked-before-approval.txt").exists(),
        "a Coder shell command before plan approval must not run"
    );
    let approved_results = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.starts_with("approved-tool-result="))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !approved_results.is_empty(),
        "the approved Coder shell call produced no result: {:#?}",
        seen.lock().unwrap()
    );
    assert!(
        approved_results
            .iter()
            .all(|line| !line.contains("blocked until the host approves")),
        "the plan gate did not release after approval: {approved_results:#?}"
    );
    assert!(
        !project.join("file.txt").exists(),
        "the child must not write into the parent workspace"
    );
    let jobs =
        deepseek_code_core::services::background_sessions::list(&data, Some(&router)).unwrap();
    assert_eq!(
        jobs.iter()
            .filter(|job| job.session_id == coder)
            .filter(|job| matches!(job.status.as_str(), "starting" | "running" | "unresponsive"))
            .count(),
        0
    );
    // The router was told about the approval from its own session context.
    let runtime = ToolRuntime {
        db: context.db.clone(),
        data_dir: data.clone(),
        session_id: router.clone(),
    };
    let shown = deepseek_code_core::super_agent::crp::execute(
        &runtime,
        "CrpShow",
        json!({"task_id": task_id}),
        "show",
    )
    .unwrap();
    assert_eq!(shown["phase"], "IMPLEMENTING");
    server.abort();
}

async fn wait_for_jobs(
    data: &Path,
    router: &str,
    expected: usize,
    seen: &Arc<Mutex<Vec<String>>>,
) {
    for _ in 0..400 {
        if let Ok(jobs) =
            deepseek_code_core::services::background_sessions::list(data, Some(router))
        {
            let running = jobs
                .iter()
                .filter(|job| {
                    matches!(job.status.as_str(), "starting" | "running" | "unresponsive")
                })
                .count();
            if jobs.len() >= expected && running == 0 {
                let failed: Vec<_> = jobs
                    .iter()
                    .filter(|job| job.status != "completed")
                    .collect();
                assert!(
                    failed.is_empty(),
                    "child job did not complete: {:#?}",
                    failed
                        .iter()
                        .map(|job| (job.id.clone(), job.status.clone(), job.error.clone()))
                        .collect::<Vec<_>>()
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let jobs = deepseek_code_core::services::background_sessions::list(data, Some(router))
        .unwrap_or_default();
    let transcript = jobs
        .iter()
        .find(|job| matches!(job.status.as_str(), "starting" | "running" | "unresponsive"))
        .and_then(|job| {
            let context = deepseek_code_core::services::session_messages::context(data).ok()?;
            let rows = context
                .db
                .with_conn(|conn| deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, &job.session_id))
                .ok()?
                .ok()?;
            let calls = context
                .db
                .with_conn(|conn| -> anyhow::Result<Vec<(String, Option<String>, Option<String>, bool)>> {
                    let rows = deepseek_code_core::db::message_repo::get_messages_by_conversation(
                        conn,
                        &job.session_id,
                    )?;
                    let mut calls = Vec::new();
                    for row in rows {
                        for call in deepseek_code_core::db::message_repo::get_tool_calls_for_message(
                            conn, &row.id,
                        )? {
                            calls.push((call.name, call.input, call.output, call.is_error));
                        }
                    }
                    Ok(calls)
                })
                .ok()?
                .ok()?;
            Some((
                rows.into_iter()
                    .rev()
                    .take(8)
                    .map(|row| (row.role, row.content))
                    .collect::<Vec<_>>(),
                calls,
            ))
        });
    panic!(
        "child jobs did not finish; jobs={:#?}; requests={:#?}; transcript={:#?}",
        jobs.iter()
            .map(|job| (job.session_id.clone(), job.status.clone(), job.error.clone()))
            .collect::<Vec<_>>(),
        seen.lock().unwrap(),
        transcript
    );
}
