use super::*;
use crate::native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider};
use crate::services::session_messages as mail;
use axum::{extract::State, routing::post, Json, Router};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Endpoint {
    requests: Mutex<Vec<Value>>,
    block: AtomicBool,
    json_mode: AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
async fn endpoint() -> (String, Arc<Endpoint>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Endpoint::default());
    let app=Router::new().route("/v1/chat/completions",post(|State(state):State<Arc<Endpoint>>,Json(body):Json<Value>|async move {
        state.requests.lock().unwrap().push(body);
        if state.block.swap(false,Ordering::SeqCst) {state.started.notify_one();state.release.notified().await;}
        ([("content-type","text/event-stream")],format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":if state.json_mode.load(Ordering::Relaxed){"{\"ok\":true}"}else{"child complete"}},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})))
    })).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state, server)
}
fn owner(
    context: &ServiceContext,
    id: &str,
    cwd: &std::path::Path,
    url: &str,
    registry: OwnedChildren,
) -> ToolLoopExecutor {
    let provider=ResolvedProvider {
        provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":url,"api_key":"memory-only-resume-secret","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(500);
    ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"owner next turn"})],
        None,
        128,
        tx,
        cwd.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id(id.into())
    .with_search_config_path(context.paths.data_dir.join("settings.json"))
    .with_owned_children(registry)
}
fn fixture() -> (
    tempfile::TempDir,
    ServiceContext,
    SessionStore,
    String,
    std::path::PathBuf,
) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    let context = mail::context(&temp.path().join("data")).unwrap();
    // Agent and SendMessage are not default core tools, so this module loads
    // them explicitly.
    crate::services::customization::save(
        &context.paths.data_dir,
        &crate::services::customization::Customization {
            tool_loading: ["Agent", "SendMessage"]
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        crate::services::customization::ToolLoading {
                            enabled: true,
                            on_demand: true,
                        },
                    )
                })
                .collect(),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let store = SessionStore::new(context.db.clone());
    let parent = store.create(&cwd, "fixture", "fixture").unwrap();
    (temp, context, store, parent, cwd)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_agent_executes_from_parent_on_default_worker_stack() {
    let (_temp, context, store, parent, cwd) = fixture();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture = requests.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let capture = capture.clone();
            async move {
                let mut requests = capture.lock().unwrap();
                requests.push(body);
                let call = (requests.len() == 1).then(|| {
                    (
                        "foreground-agent",
                        "Agent",
                        json!({
                            "description":"exercise foreground child",
                            "prompt":"CHILD_DEFAULT_STACK",
                            "run_in_background":false
                        }),
                    )
                });
                (
                    [("content-type", "text/event-stream")],
                    plan_child_stream("openai", call),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let registry: OwnedChildren = Default::default();
    let mut executor = owner(&context, &parent, &cwd, &url, registry.clone());
    let (tx, mut rx) = tokio::sync::mpsc::channel(500);
    executor.event_tx = tx;
    let events = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    // Spawn the real parent loop, rather than calling execute_delegated on the
    // test thread: the original overflow required the nested parent poll stack.
    let execution = tokio::spawn(async move { executor.execute().await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), execution)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.0, "CHILD_IMPLEMENTED");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "parent, child, parent continuation");
    assert!(requests[1].to_string().contains("CHILD_DEFAULT_STACK"));
    assert!(requests[2].to_string().contains("CHILD_IMPLEMENTED"));
    drop(requests);
    let retained = registry.lock().unwrap();
    assert_eq!(retained.len(), 1);
    let (child, entry) = retained.iter().next().unwrap();
    assert!(entry.executor.execution_lease.is_none());
    assert!(store.history(child).unwrap().iter().any(|m| {
        m["role"] == "assistant"
            && m["content"]
                .as_str()
                .unwrap_or_default()
                .contains("CHILD_IMPLEMENTED")
    }));
    assert!(
        mail::ExecutionLease::acquire(&context.paths.data_dir, child)
            .unwrap()
            .is_some()
    );
    drop(retained);
    server.abort();
    events.abort();
}

#[tokio::test]
async fn foreground_agent_accepts_model_effort_and_worktree_without_backgrounding() {
    let (_temp, context, _store, parent, cwd) = fixture();
    for args in [
        vec!["init"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let (url, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let mut executor = owner(&context, &parent, &cwd, &url, registry.clone());
    let mut alternate = executor.provider.model.clone();
    alternate.id = "alternate".into();
    executor.provider.provider.models.push(alternate);
    let result = executor.execute_delegated(&json!({"prompt":"inspect","run_in_background":false,"model":"alternate","effort":"high","isolation":"worktree"})).await.unwrap();
    let child = result["session_id"].as_str().unwrap();
    assert_eq!(result["status"], "completed");
    assert_ne!(result["backgrounded"], true);
    let retained = registry.lock().unwrap();
    let progress = context
        .setting::<Value>(&format!("delegation.{child}"))
        .unwrap()
        .unwrap();
    assert_eq!(progress["preview"], "child complete");
    let child_executor = &retained[child].executor;
    assert_eq!(child_executor.provider.model.id, "alternate");
    assert_eq!(
        child_executor.provider.model.effort.as_deref(),
        Some("high")
    );
    assert_ne!(
        std::path::Path::new(&child_executor.workspace_cwd)
            .canonicalize()
            .unwrap(),
        cwd.canonicalize().unwrap()
    );
    assert_eq!(executor.provider.model.id, "fixture");
    assert_eq!(api.requests.lock().unwrap()[0]["model"], "alternate");
    drop(retained);
    executor
        .resume_owned_child(child, Some("inspect again"), false)
        .await
        .unwrap();
    assert_eq!(api.requests.lock().unwrap().len(), 2);
    context
        .db
        .with_conn(|conn| {
            conn.execute(
                "UPDATE conversation_worktrees SET source_path=?1 WHERE conversation_id=?2",
                rusqlite::params![context.paths.data_dir.to_string_lossy(), child],
            )
        })
        .unwrap()
        .unwrap();
    assert!(executor
        .resume_owned_child(child, Some("reject changed binding"), false)
        .await
        .is_err());
    assert!(executor
        .execute_delegated(&json!({"prompt":"inspect","model":"missing"}))
        .await
        .unwrap_err()
        .to_string()
        .contains("Unknown model"));
    server.abort();
}

#[tokio::test]
async fn explicit_resume_sendmessage_and_safe_boundary_keep_original_child_provider() {
    let (_temp, context, store, parent, cwd) = fixture();
    let (a, api_a, server_a) = endpoint().await;
    let (b, api_b, server_b) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let first = owner(&context, &parent, &cwd, &a, registry.clone());
    let result = first
        .execute_delegated(&json!({"prompt":"original child"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap().to_owned();
    drop(first);
    {
        let retained = registry.lock().unwrap();
        let entry = retained.get(&child).unwrap();
        assert!(entry.executor.messages.is_empty());
        assert!(entry.executor.message_inbox.is_none());
        assert!(entry.executor.execution_lease.is_none());
    }
    let mut next = owner(&context, &parent, &cwd, &b, registry.clone());
    let resumed = next
        .execute_delegated(&json!({"resume":child,"prompt":"explicit continuation"}))
        .await
        .unwrap();
    assert_eq!(resumed["session_id"], child);
    assert_eq!(resumed["resumed_in_owner_turn"], true);
    let (_, output, failed) = next
        .execute_tool_call(
            "SendMessage",
            &json!({"operation":"send","to":child,"content":"MESSAGE_WAKE_MARKER"}),
            "send-owned",
        )
        .await;
    assert!(!failed, "{output}");
    assert!(output.contains("resumed_in_owner_turn"), "{output}");
    assert_eq!(
        mail::inbox(&context, &child, false).unwrap()[0].state,
        "context_committed"
    );
    let peer = store.fork(&parent).unwrap();
    mail::send(&context, &peer, &child, "BOUNDARY_WAKE_MARKER").unwrap();
    next.execute().await.unwrap();
    assert_eq!(api_a.requests.lock().unwrap().len(), 4);
    assert_eq!(api_b.requests.lock().unwrap().len(), 1);
    let requests = api_a.requests.lock().unwrap();
    assert!(requests[1].to_string().contains("original child"));
    assert!(requests[1].to_string().contains("explicit continuation"));
    assert!(requests[2].to_string().contains("MESSAGE_WAKE_MARKER"));
    assert!(requests[3].to_string().contains("BOUNDARY_WAKE_MARKER"));
    drop(requests);
    assert_eq!(
        store
            .history(&child)
            .unwrap()
            .iter()
            .filter(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("BOUNDARY_WAKE_MARKER")))
            .count(),
        1
    );
    let resumed: Value = context
        .setting(&format!("delegation.{child}"))
        .unwrap()
        .unwrap();
    assert_eq!(resumed["status"], "completed");
    server_a.abort();
    server_b.abort();
}
#[tokio::test]
async fn resume_cancellation_releases_ownership_and_never_automatically_restarts() {
    let (_temp, context, _store, parent, cwd) = fixture();
    let (a, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let first = owner(&context, &parent, &cwd, &a, registry.clone());
    let result = first
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap().to_owned();
    drop(first);
    api.block.store(true, Ordering::SeqCst);
    let next = owner(&context, &parent, &cwd, &a, registry.clone());
    let child_copy = child.clone();
    let task = tokio::spawn(async move {
        next.execute_delegated(&json!({"resume":child_copy,"prompt":"wait"}))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), api.started.notified())
        .await
        .unwrap();
    let other = owner(&context, &parent, &cwd, &a, registry.clone());
    assert!(other
        .resume_owned_child(&child, Some("duplicate"), false)
        .await
        .is_err());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let saved: Value = context
        .setting(&format!("delegation.{child}"))
        .unwrap()
        .unwrap();
    assert_eq!(saved["status"], "cancelled");
    assert!(!mail::receiver_active(&context.paths.data_dir, &child).unwrap());
    drop(mail::ExecutionLease::acquire(&context.paths.data_dir, &child).unwrap());
    mail::send(&context, &parent, &child, "must remain queued").unwrap();
    other.resume_pending_owned_children().await;
    assert!(other
        .resume_owned_child(&child, Some("explicit does not rearm cancellation"), false)
        .await
        .is_err());
    assert_eq!(api.requests.lock().unwrap().len(), 2);
    assert_eq!(mail::pending_count(&context, &child).unwrap(), 1);
    api.release.notify_one();
    server.abort();
}
#[tokio::test]
async fn foreign_owner_changed_policy_changed_trust_and_lost_configuration_make_no_request() {
    let (_temp, context, store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let first = owner(&context, &parent, &cwd, &url, registry.clone());
    let result = first
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap().to_owned();
    let foreign = store.fork(&parent).unwrap();
    assert!(owner(&context, &foreign, &cwd, &url, registry.clone())
        .resume_owned_child(&child, Some("foreign"), false)
        .await
        .is_err());
    let mut changed = owner(&context, &parent, &cwd, &url, registry.clone());
    changed.allowed_tools = Some(vec!["Read".into()]);
    assert!(changed
        .resume_owned_child(&child, Some("policy changed"), false)
        .await
        .is_err());
    let missing = owner(&context, &parent, &cwd, &url, Default::default());
    assert!(missing
        .resume_owned_child(&child, Some("lost"), false)
        .await
        .unwrap_err()
        .to_string()
        .contains("needs_resume"));
    context.db.with_conn(|conn|conn.execute("UPDATE project_folders SET trusted=0 WHERE project_id=(SELECT project_id FROM conversations WHERE id=?1)",[&child])).unwrap().unwrap();
    assert!(first
        .resume_owned_child(&child, Some("trust revoked"), false)
        .await
        .is_err());
    assert_eq!(api.requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn typed_research_stages_are_not_registered_for_automatic_resume() {
    let (_temp, context, _store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    api.json_mode.store(true, Ordering::Relaxed);
    let registry: OwnedChildren = Default::default();
    let executor = owner(&context, &parent, &cwd, &url, registry.clone());
    let result=executor.execute_delegated_typed(&json!({"prompt":"typed phase"}),Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false})),Some(vec!["Read".into()])).await.unwrap();
    assert_eq!(result["resume_available"], false);
    assert!(registry.lock().unwrap().is_empty());
    assert!(executor
        .resume_owned_child(
            result["session_id"].as_str().unwrap(),
            Some("must not restart fixed phase"),
            false
        )
        .await
        .is_err());
    assert_eq!(api.requests.lock().unwrap().len(), 1);
    fn no_secret(path: &std::path::Path) {
        for item in std::fs::read_dir(path).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                no_secret(&path);
            } else {
                assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap())
                    .contains("memory-only-resume-secret"));
            }
        }
    }
    no_secret(&context.paths.data_dir);
    server.abort();
}

#[tokio::test]
async fn unchanged_mode_keeps_resume_but_mode_away_and_back_invalidates_snapshot() {
    use crate::permissions::PermissionMode;
    let (_temp, context, _store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let permissions = crate::runtime::default_permissions();
    permissions.set_mode(PermissionMode::AskPermissions);
    let executor =
        owner(&context, &parent, &cwd, &url, registry).with_permission_manager(permissions.clone());
    let result = executor
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap();
    permissions.set_mode(PermissionMode::AskPermissions);
    executor
        .resume_owned_child(child, Some("same mode is stable"), false)
        .await
        .unwrap();
    permissions.set_mode(PermissionMode::PlanMode);
    permissions.set_mode(PermissionMode::AskPermissions);
    assert!(executor
        .resume_owned_child(child, Some("mode changed while idle"), false)
        .await
        .unwrap_err()
        .to_string()
        .contains("policy changed"));
    assert_eq!(api.requests.lock().unwrap().len(), 2);
    server.abort();
}

#[tokio::test]
async fn launch_rule_changes_block_retained_child_resume_without_consuming_its_inbox() {
    use crate::permissions::{
        rules::PermissionAction, PermissionContext, PermissionMode, PermissionResult,
    };
    let (_temp, context, store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let permissions = crate::runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let original = HashMap::from([("Write".into(), PermissionAction::Deny)]);
    permissions
        .set_launch_rules(&parent, original.clone())
        .unwrap();
    let executor = owner(&context, &parent, &cwd, &url, registry.clone())
        .with_permission_manager(permissions.clone());
    let result = executor
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap();
    // Reapplying identical launch arguments permits an ordinary next-turn resume.
    permissions.set_launch_rules(&parent, original).unwrap();
    executor
        .resume_owned_child(child, Some("unchanged launch"), false)
        .await
        .unwrap();
    let history_len = store.history(child).unwrap().len();
    mail::send(
        &context,
        &parent,
        child,
        "must stay pending after launch change",
    )
    .unwrap();
    for replacement in [
        HashMap::new(),
        HashMap::from([("Write".into(), PermissionAction::Allow)]),
    ] {
        permissions.set_launch_rules(&parent, replacement).unwrap();
        let error = executor
            .resume_owned_child(child, Some("new launch"), false)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Owner policy changed"),
            "{error:#}"
        );
        executor.resume_pending_owned_children().await;
        assert_eq!(api.requests.lock().unwrap().len(), 2);
        assert_eq!(store.history(child).unwrap().len(), history_len);
        assert_eq!(mail::pending_count(&context, child).unwrap(), 1);
        assert!(!mail::receiver_active(&context.paths.data_dir, child).unwrap());
        let retained = registry.lock().unwrap();
        let manager = retained
            .get(child)
            .unwrap()
            .executor
            .permission_manager
            .as_ref()
            .unwrap();
        assert!(matches!(
            manager.check_permission(&PermissionContext {
                mcp_read_only: false,
                tool_name: "Write".into(),
                tool_input: json!({"file_path":"blocked.txt","content":"blocked"}),
                conversation_id: child.into(),
                user_id: None,
                workspace_path: Some(cwd.to_string_lossy().into_owned()),
            }),
            PermissionResult::Denied(_)
        ));
    }
    server.abort();
}

#[test]
fn private_session_execution_lease_rejects_other_engine_until_drop() {
    let (_temp, context, _store, parent, _cwd) = fixture();
    context
        .db
        .with_conn(|conn| {
            conn.execute(
                "UPDATE conversations SET workspace_mode='chat',project_id=NULL WHERE id=?1",
                [&parent],
            )
        })
        .unwrap()
        .unwrap();
    let lease = mail::ExecutionLease::acquire(&context.paths.data_dir, &parent)
        .unwrap()
        .unwrap();
    assert!(mail::ExecutionLease::acquire(&context.paths.data_dir, &parent).is_err());
    drop(lease);
    assert!(
        mail::ExecutionLease::acquire(&context.paths.data_dir, &parent)
            .unwrap()
            .is_some()
    );
}

fn plan_child_stream(format: &str, call: Option<(&str, &str, Value)>) -> String {
    let events = if format == "openai" {
        let delta = match call {
            Some((id, name, input)) => {
                json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]})
            }
            None => json!({"content":"CHILD_IMPLEMENTED"}),
        };
        let reason = if delta.get("tool_calls").is_some() {
            "tool_calls"
        } else {
            "stop"
        };
        vec![json!({"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})]
    } else {
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"child-plan","model":"fixture","usage":{"input_tokens":1}}}),
        ];
        let reason = if let Some((id, name, input)) = call {
            events.extend([
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
                json!({"type":"content_block_stop","index":0}),
            ]);
            "tool_use"
        } else {
            events.extend([
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"CHILD_IMPLEMENTED"}}),
                json!({"type":"content_block_stop","index":0}),
            ]);
            "end_turn"
        };
        events.extend([json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":1}}),json!({"type":"message_stop"})]);
        events
    };
    let mut stream = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    if format == "openai" {
        stream.push_str("data: [DONE]\n\n");
    }
    stream
}

#[tokio::test]
async fn child_plan_review_returns_without_approval_or_implementation_and_resumes_cleanly() {
    use crate::native_engine::provider_manager::ApiFormat;
    use crate::permissions::PermissionMode;
    for format in ["openai", "anthropic"] {
        let (_temp, context, store, parent, cwd) = fixture();
        store
            .append(&parent, "user", "PARENT_CONTEXT_MUST_SURVIVE")
            .unwrap();
        let source = cwd.join("read-before-plan.txt");
        std::fs::write(&source, "OLD_CHILD_TOOL_CONTENT").unwrap();
        let destination = cwd.join("implemented-after-plan.txt");
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let capture = requests.clone();
        let written = destination.clone();
        let route = if format == "openai" {
            "/v1/chat/completions"
        } else {
            "/v1/messages"
        };
        let app = Router::new().route(
            route,
            post(move |Json(body): Json<Value>| {
                let capture = capture.clone();
                let source = source.clone();
                let written = written.clone();
                async move {
                    let index = {
                        let mut rows = capture.lock().unwrap();
                        let index = rows.len();
                        rows.push(body);
                        index
                    };
                    let call = match index {
                        0 => Some(("before-clear", "Read", json!({"file_path":source}))),
                        1 => Some((
                            "approve-plan",
                            "ExitPlanMode",
                            json!({"plan":"# APPROVED_CHILD_PLAN\nWrite the implementation."}),
                        )),
                        2 => Some((
                            "after-clear",
                            "Write",
                            json!({"file_path":written,"content":"approved implementation"}),
                        )),
                        _ => None,
                    };
                    (
                        [("content-type", "text/event-stream")],
                        plan_child_stream(format, call),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let registry: OwnedChildren = Default::default();
        let permissions = crate::runtime::default_permissions();
        permissions.set_mode(PermissionMode::PlanMode);
        let parent_mode = permissions.mode_snapshot();
        let mut executor = owner(&context, &parent, &cwd, &url, registry.clone())
            .with_permission_manager(permissions.clone());
        executor.provider.provider.api_format = if format == "openai" {
            ApiFormat::OpenAI
        } else {
            ApiFormat::Anthropic
        };
        let (events, mut receiver) = tokio::sync::mpsc::channel(500);
        executor.event_tx = events;
        let waiters = executor.answer_waiters.clone();
        let reader = tokio::spawn(async move {
            let mut plan_approvals = 0;
            let mut leaked_resets = 0;
            while let Some(event) = receiver.recv().await {
                match event {
                    EngineEvent::AskUser { request_id, .. } => {
                        plan_approvals += 1;
                        waiters
                            .lock()
                            .await
                            .remove(&request_id)
                            .unwrap()
                            .send("1".into())
                            .unwrap();
                    }
                    EngineEvent::ToolPermission { request_id, .. } => {
                        waiters
                            .lock()
                            .await
                            .remove(&request_id)
                            .unwrap()
                            .send("yes".into())
                            .unwrap();
                    }
                    EngineEvent::ContextReset { .. } => leaked_resets += 1,
                    _ => {}
                }
            }
            (plan_approvals, leaked_resets)
        });
        let created = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            executor.execute_delegated(&json!({"prompt":"OLD_CHILD_PROMPT_MUST_DISAPPEAR"})),
        )
        .await
        .unwrap()
        .unwrap();
        let child = created["session_id"].as_str().unwrap().to_owned();
        assert!(
            !destination.exists(),
            "Review must not authorize implementation"
        );
        assert_eq!(permissions.mode_snapshot(), parent_mode);
        for session in [&parent, &child] {
            assert!(
                crate::services::plan_approval::latest_boundary(&context.db, session)
                    .unwrap()
                    .is_none()
            );
        }
        let history = store
            .model_history(&child, &executor.provider.provider.api_format)
            .unwrap();
        let history = serde_json::to_string(&history).unwrap();
        assert!(history.contains("APPROVED_CHILD_PLAN"));
        assert!(history.contains("OLD_CHILD_TOOL_CONTENT"));
        assert_eq!(
            registry.lock().unwrap()[&child]
                .executor
                .permission_manager
                .as_ref()
                .unwrap()
                .get_mode(),
            PermissionMode::PlanMode
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            executor.resume_owned_child(&child, Some("RESUME_REVIEW_CHILD"), false),
        )
        .await
        .unwrap()
        .unwrap();
        drop(executor);
        assert_eq!(
            reader.await.unwrap(),
            (0, 0),
            "Child review must not prompt or reset the parent"
        );
        assert!(requests.lock().unwrap().last().unwrap()["messages"]
            .to_string()
            .contains("RESUME_REVIEW_CHILD"));
        server.abort();
    }
}
