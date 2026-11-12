//! Executor bridge integration tests: real QuickJS, real builtin tools, the
//! shared event→row writer and real SQLite reopen/replay.

use super::*;
use crate::native_engine::provider_manager::ApiFormat;
use crate::permissions::PermissionMode;
use crate::services::tool_history::{apply_event, save, ToolOrigins};
use crate::toolscript::{Cancellation, ToolScriptOutcome};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn fixture_provider() -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}

struct Harness {
    executor: ToolLoopExecutor,
    manager: Arc<PermissionManager>,
    events: Arc<Mutex<Vec<EngineEvent>>>,
    _drain: tokio::task::JoinHandle<()>,
}

fn harness(root: &Path, mode: PermissionMode) -> Harness {
    harness_with_hooks(root, mode, None)
}

fn harness_with_hooks(
    root: &Path,
    mode: PermissionMode,
    hooks: Option<crate::services::tool_hooks::HookRunner>,
) -> Harness {
    let (tx, mut rx) = mpsc::channel(1024);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let drain = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            sink.lock().unwrap().push(event);
        }
    });
    let manager = crate::runtime::default_permissions();
    manager.set_mode(mode);
    let mut executor = ToolLoopExecutor::new(
        fixture_provider(),
        vec![],
        None,
        256,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("toolscript-fixture".into())
    .with_permission_manager(manager.clone())
    .with_search_config_path(root.join("settings.json"));
    if let Some(hooks) = hooks {
        executor = executor.with_hooks(hooks);
    }
    Harness {
        executor,
        manager,
        events,
        _drain: drain,
    }
}

#[derive(Debug, Clone)]
struct Done {
    id: String,
    name: String,
    input: Value,
    output: String,
    is_error: bool,
}

#[derive(Debug)]
struct Observed {
    starts: Vec<(String, String)>,
    origins: Vec<(String, String, u64)>,
    dones: Vec<Done>,
}

fn observed(events: &[EngineEvent]) -> Observed {
    let mut result = Observed {
        starts: Vec::new(),
        origins: Vec::new(),
        dones: Vec::new(),
    };
    for event in events {
        match event {
            EngineEvent::ToolUseStart {
                tool_use_id,
                tool_name,
                ..
            } => result.starts.push((tool_use_id.clone(), tool_name.clone())),
            EngineEvent::NestedToolCall {
                tool_use_id,
                parent_tool_use_id,
                sequence,
            } => result
                .origins
                .push((tool_use_id.clone(), parent_tool_use_id.clone(), *sequence)),
            EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
            } => result.dones.push(Done {
                id: tool_use_id.clone(),
                name: tool_name.clone(),
                input: tool_input.clone(),
                output: output.clone(),
                is_error: *is_error,
            }),
            _ => {}
        }
    }
    result
}

fn take_events(harness: &Harness) -> Vec<EngineEvent> {
    std::mem::take(&mut *harness.events.lock().unwrap())
}

/// Persists through the shared writer the CLI uses, then returns ids.
fn persist(root: &Path, parent: &str, events: &[EngineEvent]) -> (String, String) {
    let context = crate::services::session_messages::context(&root.join("data")).unwrap();
    let store = crate::runtime::SessionStore::new(context.db.clone());
    let session = store.create(root, "fixture", "fixture").unwrap();
    let assistant = store.append(&session, "assistant", "").unwrap();
    let mut rows = Vec::new();
    let mut origins = ToolOrigins::new();
    let mut arguments = std::collections::HashMap::new();
    for event in events {
        apply_event(&mut rows, &mut origins, &mut arguments, event);
    }
    context
        .db
        .with_conn(|conn| -> anyhow::Result<()> {
            crate::db::message_repo::insert_tool_call(
                conn,
                &format!("{assistant}:{parent}"),
                &assistant,
                "ToolScript",
                Some("{}"),
                Some("script finished"),
                false,
                0,
            )?;
            save(conn, &session, &assistant, "", &rows, &origins)?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    (session, assistant)
}

async fn run(
    harness: &mut Harness,
    source: &str,
    args: Value,
    parent: &str,
) -> Result<ToolScriptOutcome> {
    harness
        .executor
        .run_toolscript(source, &args, parent, &Cancellation::new())
        .await
}

#[tokio::test]
async fn sequential_calls_execute_real_tools_and_persist_after_reopen() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(root.join("b.txt"), "one\ntwo\n").unwrap();
    let mut h = harness(root, PermissionMode::BypassPermissions);

    let outcome = run(
        &mut h,
        r#"
        const a = await tools.call("Read", { file_path: "a.txt" });
        const b = await tools.call("Read", { file_path: "b.txt" });
        const e = await tools.call("Edit", { file_path: "b.txt", old_string: "one", new_string: "ONE" });
        const s = await tools.call("Bash", { command: "printf beta > c.txt" });
        return { a: a.includes("alpha"), b: b.includes("two"), e: e.length > 0, s };
        "#,
        json!(null),
        "parent-script",
    )
    .await
    .expect("script should succeed");

    assert_eq!(outcome.calls_started, 4);
    assert_eq!(outcome.calls_succeeded, 4);
    assert_eq!(outcome.abandoned_calls, 0);
    assert_eq!(outcome.value["a"], json!(true));
    assert_eq!(outcome.value["b"], json!(true));
    assert_eq!(outcome.value["e"], json!(true));
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "ONE\ntwo\n"
    );
    assert_eq!(std::fs::read_to_string(root.join("c.txt")).unwrap(), "beta");

    let events = take_events(&h);
    let seen = observed(&events);
    // The nested identity is emitted before any row-bearing event, so a crash
    // cannot leave a row that reopen would treat as a provider call.
    let mut origin_at = std::collections::HashMap::new();
    let mut start_at = std::collections::HashMap::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            EngineEvent::NestedToolCall { tool_use_id, .. } => {
                origin_at.insert(tool_use_id.clone(), index);
            }
            EngineEvent::ToolUseStart { tool_use_id, .. } => {
                start_at.insert(tool_use_id.clone(), index);
            }
            _ => {}
        }
    }
    for (id, start) in &start_at {
        assert!(
            origin_at.get(id).is_some_and(|origin| origin < start),
            "origin must precede the row-bearing start for {id}"
        );
    }
    assert_eq!(seen.starts.len(), 4, "{seen:?}");
    assert_eq!(seen.dones.len(), 4, "{seen:?}");
    assert_eq!(seen.origins.len(), 4, "{seen:?}");
    let ids: std::collections::HashSet<_> = seen.starts.iter().map(|(id, _)| id).collect();
    assert_eq!(ids.len(), 4);
    assert!(ids.iter().all(|id| id.starts_with("inv-")));
    assert_eq!(
        seen.origins
            .iter()
            .map(|(_, parent, _)| parent.as_str())
            .collect::<std::collections::HashSet<_>>(),
        ["parent-script"].into_iter().collect()
    );
    assert_eq!(
        seen.origins
            .iter()
            .map(|(_, _, seq)| *seq)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    // Start is emitted before execution; Done carries the output.
    assert!(seen.dones.iter().all(|done| !done.is_error));
    assert!(seen.dones[0].output.contains("alpha"));
    assert_eq!(seen.dones[3].name, "Bash");
    assert!(!seen.dones[3].is_error);

    let (session, assistant) = persist(root, "parent-script", &events);
    let reopened = crate::services::session_messages::context(&root.join("data")).unwrap();
    let store = crate::runtime::SessionStore::new(reopened.db.clone());
    reopened
        .db
        .with_conn(|conn| {
            let rows = crate::db::message_repo::get_tool_calls_for_message(conn, &assistant)?;
            let origins = crate::db::message_repo::get_tool_origins_for_message(conn, &assistant)?;
            assert_eq!(rows.len(), 5, "parent + four nested rows");
            assert_eq!(origins.len(), 4);
            assert_eq!(
                rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
                ["ToolScript", "Read", "Read", "Edit", "Bash"]
            );
            assert!(rows.iter().all(|row| row.output.is_some()));
            Ok::<(), anyhow::Error>(())
        })
        .unwrap()
        .unwrap();

    // Provider replay keeps only the parent pair and never fabricates the
    // nested occurrences.
    for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
        let history = store.model_history(&session, &format).unwrap();
        let text = serde_json::to_string(&history).unwrap();
        assert!(text.contains("parent-script"), "{format:?}: {text}");
        for id in &ids {
            assert!(
                !text.contains(id.as_str()),
                "{format:?} leaked {id}: {text}"
            );
        }
    }
}

#[tokio::test]
async fn repeated_calls_and_replayed_events_keep_distinct_rows() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("same.txt"), "identical\n").unwrap();
    let mut h = harness(root, PermissionMode::BypassPermissions);

    run(
        &mut h,
        r#"
        const first = await tools.call("Read", { file_path: "same.txt" });
        const second = await tools.call("Read", { file_path: "same.txt" });
        return { first, second };
        "#,
        json!(null),
        "parent-repeat",
    )
    .await
    .expect("script should succeed");

    let events = take_events(&h);
    let seen = observed(&events);
    assert_eq!(seen.dones.len(), 2);
    assert_ne!(seen.dones[0].id, seen.dones[1].id);

    // Replaying the same completion must not add a row.
    let (_session, assistant) = persist(root, "parent-repeat", &events);
    let context = crate::services::session_messages::context(&root.join("data")).unwrap();
    let mut rows = Vec::new();
    let mut origins = ToolOrigins::new();
    let mut arguments = std::collections::HashMap::new();
    for event in &events {
        apply_event(&mut rows, &mut origins, &mut arguments, event);
    }
    for event in &events {
        apply_event(&mut rows, &mut origins, &mut arguments, event);
    }
    context
        .db
        .with_conn(|conn| {
            save(conn, "fixture", &assistant, "", &rows, &origins)?;
            let rows = crate::db::message_repo::get_tool_calls_for_message(conn, &assistant)?;
            assert_eq!(rows.len(), 3, "parent + two calls, no duplicate");
            Ok::<(), anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn plan_mode_denies_edit_without_a_file_effect() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("plan.txt"), "original\n").unwrap();
    let mut h = harness(root, PermissionMode::PlanMode);

    let outcome = run(
        &mut h,
        r#"
        try {
            await tools.call("Edit", { file_path: "plan.txt", old_string: "original", new_string: "changed" });
            return "unexpected";
        } catch (error) {
            return error.code + ":" + error.message;
        }
        "#,
        json!(null),
        "parent-plan",
    )
    .await
    .expect("script catches the denial");

    let text = outcome.value.as_str().unwrap();
    assert!(text.contains("permission_denied"), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("plan.txt")).unwrap(),
        "original\n"
    );
    let seen = observed(&take_events(&h));
    assert_eq!(seen.starts.len(), 1);
    assert_eq!(seen.dones.len(), 1);
    assert!(seen.dones[0].is_error);
}

#[tokio::test]
async fn unloaded_and_lifecycle_tools_are_refused_before_side_effects() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut h = harness(root, PermissionMode::BypassPermissions);

    // Simulate a request snapshot that did not carry Read.
    h.executor.exposed_tools = Some(std::collections::HashSet::new());
    let outcome = run(
        &mut h,
        r#"
        const codes = [];
        for (const [name, args] of [
            ["Read", { file_path: "a.txt" }],
            ["Agent", { prompt: "x" }],
            ["Workflow", { script: "return 1;" }],
            ["Bash", { command: "touch sentinel.txt", run_in_background: true }],
        ]) {
            try { await tools.call(name, args); codes.push("ran"); }
            catch (error) { codes.push(error.code); }
        }
        return codes;
        "#,
        json!(null),
        "parent-refuse",
    )
    .await
    .expect("the script catches every refusal");

    let codes = outcome.value.as_array().unwrap();
    assert_eq!(codes[0], json!("tool_unavailable"));
    assert_eq!(codes[1], json!("unsupported_tool"));
    assert_eq!(codes[2], json!("unsupported_tool"));
    assert_eq!(codes[3], json!("unsupported_tool"));
    assert!(!root.join("sentinel.txt").exists());
    let seen = observed(&take_events(&h));
    assert_eq!(seen.starts.len(), 4, "every attempt is visible");
    assert_eq!(seen.dones.len(), 4);
    assert!(seen.dones.iter().all(|done| done.is_error));
}

#[cfg(unix)]
#[tokio::test]
async fn pre_hook_blocks_and_rewrites_nested_input() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    let block = json!({"decision":"block","reason":"blocked by fixture hook"});
    let rewrite = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"file_path":"safe.txt","content":"HOOKED"}}});
    let command = |value: &Value| {
        format!(
            "printf '%s' '{}'",
            value.to_string().replace('\'', "'\"'\"'")
        )
    };
    std::fs::write(
        root.join(".claude/settings.json"),
        json!({"hooks":{
            "PreToolUse":[
                {"matcher":"^Write$","hooks":[{"type":"command","shell":"sh","command":command(&rewrite)}]},
                {"matcher":"^Edit$","hooks":[{"type":"command","shell":"sh","command":command(&block)}]}
            ]
        }})
        .to_string(),
    )
    .unwrap();
    let hooks = crate::services::tool_hooks::HookRunner::load(&root.join("data"), root).unwrap();
    let mut h = harness_with_hooks(root, PermissionMode::BypassPermissions, Some(hooks));

    let outcome = run(
        &mut h,
        r#"
        const written = await tools.call("Write", { file_path: "original.txt", content: "ORIGINAL" });
        let editCode;
        try { await tools.call("Edit", { file_path: "original.txt", old_string: "ORIGINAL", new_string: "NO" }); editCode = "ran"; }
        catch (error) { editCode = error.code; }
        return { written, editCode };
        "#,
        json!(null),
        "parent-hook",
    )
    .await
    .expect("the script catches the hook block");

    assert_eq!(outcome.value["editCode"], json!("tool_failed"));
    assert!(!root.join("original.txt").exists(), "blocked edit ran");
    assert_eq!(
        std::fs::read_to_string(root.join("safe.txt")).unwrap(),
        "HOOKED"
    );
    let seen = observed(&take_events(&h));
    let write = seen.dones.iter().find(|done| done.name == "Write").unwrap();
    assert!(!write.is_error);
    // The Done event carries the post-hook effective input.
    assert_eq!(write.input["file_path"], json!("safe.txt"));
    let edit = seen.dones.iter().find(|done| done.name == "Edit").unwrap();
    assert!(edit.is_error);
    assert!(edit.output.contains("blocked by fixture hook"));
}

#[tokio::test]
async fn live_policy_changes_apply_between_script_calls() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("policy.txt"), "before\n").unwrap();
    let mut h = harness(root, PermissionMode::BypassPermissions);

    let first = run(
        &mut h,
        r#"
        await tools.call("Read", { file_path: "policy.txt" });
        return await tools.call("Edit", { file_path: "policy.txt", old_string: "before", new_string: "after" });
        "#,
        json!(null),
        "parent-policy-1",
    )
    .await
    .expect("first script succeeds");
    assert_eq!(
        std::fs::read_to_string(root.join("policy.txt")).unwrap(),
        "after\n"
    );
    assert!(first.value.as_str().unwrap().contains("Successfully"));

    h.manager.set_mode(PermissionMode::PlanMode);
    let second = run(
        &mut h,
        r#"
        await tools.call("Read", { file_path: "policy.txt" });
        try { await tools.call("Edit", { file_path: "policy.txt", old_string: "after", new_string: "blocked" }); return "ran"; }
        catch (error) { return error.code; }
        "#,
        json!(null),
        "parent-policy-2",
    )
    .await
    .expect("second script catches the denial");
    assert_eq!(second.value, json!("permission_denied"));
    assert_eq!(
        std::fs::read_to_string(root.join("policy.txt")).unwrap(),
        "after\n"
    );
}

#[tokio::test]
async fn script_error_propagates_and_history_keeps_the_terminal_outcome() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    let mut h = harness(root, PermissionMode::BypassPermissions);

    let error = run(
        &mut h,
        r#"
        await tools.call("Read", { file_path: "a.txt" });
        throw new Error("script blew up");
        "#,
        json!(null),
        "parent-error",
    )
    .await
    .expect_err("the script throw must propagate");
    assert!(format!("{error}").contains("script blew up"), "{error}");

    let seen = observed(&take_events(&h));
    assert_eq!(seen.dones.len(), 1);
    assert!(!seen.dones[0].is_error);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_during_bash_terminates_the_exact_owned_processes() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut h = harness(root, PermissionMode::BypassPermissions);
    let cancellation = Cancellation::new();
    let token = cancellation.clone();
    let root_for_barrier = root.to_path_buf();

    // The command publishes the shell PID and its background child, then
    // sleeps past a real start barrier and only afterwards writes the file.
    let barrier = async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let (Some(root_pid), Some(child_pid)) = (
                read_pid(&root_for_barrier.join("root.pid")),
                read_pid(&root_for_barrier.join("child.pid")),
            ) {
                let root_start = proc_start(root_pid).expect("shell must be alive");
                let child_start = proc_start(child_pid).expect("child must be alive");
                // Take cleanup ownership the moment both identities are known,
                // before any fallible assertion or cancellation.
                let cleanup = KillExact(vec![(root_pid, root_start), (child_pid, child_start)]);
                token.cancel();
                return (cleanup, root_pid, root_start, child_pid, child_start);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the Bash fixture never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let run = h.executor.run_toolscript(
        r#"return await tools.call("Bash", { command: "echo $$ > root.pid; sleep 2 & echo $! > child.pid; wait; touch late.txt" });"#,
        &json!(null),
        "parent-cancel",
        &cancellation,
    );
    let (result, (_cleanup, root_pid, root_start, child_pid, child_start)) =
        tokio::join!(run, barrier);
    let error = result.expect_err("cancelled script must fail");
    assert!(format!("{error}").contains("cancelled"), "{error}");

    // The guard was created at observation time; it signals only exact
    // PID+start-time identities, never a pattern or a group name.

    // Drop signals SIGKILL to the owned process group but does not await the
    // OS; wait for both exact identities (PID + start time) to disappear.
    assert!(
        wait_process_gone(root_pid, root_start, Duration::from_secs(5)).await,
        "the shell outlived the bridge"
    );
    assert!(
        wait_process_gone(child_pid, child_start, Duration::from_secs(5)).await,
        "the background child outlived the bridge"
    );

    // Horizon longer than the command's own 2 s sleep: a leaked child would
    // have created the file by now.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !root.join("late.txt").exists(),
        "late command effect escaped after cleanup"
    );
    let seen = observed(&take_events(&h));
    assert_eq!(seen.dones.len(), 1, "{seen:?}");
    assert!(seen.dones[0].is_error);
}

/// Well after any unawaited call could take effect, proves the call was never
/// dispatched. This is the honest truth for a script that settles without
/// another await: the unawaited host future is dropped before the driver can
/// start it. In-flight cleanup is covered by the explicit-cancellation test.
#[tokio::test]
async fn settling_script_abandons_an_unawaited_call_before_dispatch() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut h = harness(root, PermissionMode::BypassPermissions);

    let outcome = run(
        &mut h,
        r#"
        tools.call("Bash", { command: "touch abandoned.txt" });
        return "settled";
        "#,
        json!(null),
        "parent-abandon",
    )
    .await
    .expect("the top-level script settles");
    assert_eq!(outcome.value, json!("settled"));
    assert_eq!(
        outcome.calls_started, 0,
        "an unawaited call must not be admitted after the script settles"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !root.join("abandoned.txt").exists(),
        "an abandoned call produced an effect"
    );
    let seen = observed(&take_events(&h));
    assert!(
        seen.dones.is_empty(),
        "a never-dispatched call has no terminal record: {seen:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_permission_wait_aborts_when_the_script_settles_early() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    let ask = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"fixture asks"}});
    let command = format!("printf '%s' '{}'", ask.to_string().replace('\'', "'\"'\"'"));
    std::fs::write(
        root.join(".claude/settings.json"),
        json!({"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","shell":"sh","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
    let hooks = crate::services::tool_hooks::HookRunner::load(&root.join("data"), root).unwrap();
    let mut h = harness_with_hooks(root, PermissionMode::AskPermissions, Some(hooks));

    // The Bash call is not awaited. It reaches the permission wait while the
    // script is still running, then the script settles and its host future is
    // dropped. The executor must observe that abandonment instead of waiting
    // for a permission answer that will never come.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        h.executor.run_toolscript(
            r#"
            const read = tools.call("Read", { file_path: "a.txt" });
            tools.call("Bash", { command: "touch nope.txt" });
            await read;
            let spin = 0;
            for (let i = 0; i < 5000000; i++) spin += i;
            return "settled";
            "#,
            &json!(null),
            "parent-permission-wait",
            &Cancellation::new(),
        ),
    )
    .await
    .expect("the bridge must not hang on an abandoned permission wait")
    .expect("the top-level script settles");
    assert_eq!(result.value, json!("settled"));
    assert!(!root.join("nope.txt").exists());

    let events = take_events(&h);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolPermission { .. })),
        "the fixture never reached the permission wait: {events:?}"
    );
    let seen = observed(&events);
    let bash = seen
        .dones
        .iter()
        .find(|done| done.name == "Bash")
        .expect("the abandoned attempt still gets one terminal record");
    assert!(bash.is_error);
}

#[cfg(unix)]
#[tokio::test]
async fn completed_sibling_results_survive_cancellation() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    let fifo = root.join("b.fifo");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let mut h = harness(root, PermissionMode::BypassPermissions);
    assert!(
        h.executor
            .is_concurrency_safe("Bash", &json!({"command": "cat a.txt"})),
        "fixture reads must classify as concurrency-safe"
    );

    let cancellation = Cancellation::new();
    let token = cancellation.clone();
    let events = h.events.clone();
    let canceller = tokio::spawn(async move {
        // Wait for the first body to actually complete, then cancel while its
        // sibling is still blocked on the FIFO.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let finished = events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    EngineEvent::ToolUseDone {
                        is_error: false,
                        ..
                    }
                )
            });
            if finished || std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        token.cancel();
    });

    let error = h
        .executor
        .run_toolscript(
            r#"
            return await Promise.all([
                tools.call("Bash", { command: "cat a.txt" }),
                tools.call("Bash", { command: "cat b.fifo" }),
            ]);
            "#,
            &json!(null),
            "parent-sibling",
            &cancellation,
        )
        .await
        .expect_err("cancelled script must fail");
    canceller.await.unwrap();
    assert!(format!("{error}").contains("cancelled"), "{error}");

    let seen = observed(&take_events(&h));
    let completed = seen
        .dones
        .iter()
        .find(|done| !done.is_error)
        .expect("the completed sibling must keep its success record");
    assert!(completed.output.contains("alpha"), "{completed:?}");
    let cancelled = seen
        .dones
        .iter()
        .find(|done| done.is_error)
        .expect("the blocked sibling must keep a cancellation record");
    assert!(
        cancelled.output.to_lowercase().contains("cancel"),
        "{cancelled:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_cancelled_first_batch_records_every_later_admitted_call() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("kick.txt"), "kick\n").unwrap();
    let mut h = harness(root, PermissionMode::BypassPermissions);
    let cancellation = Cancellation::new();
    let token = cancellation.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        token.cancel();
    });

    // All four calls are issued before the await, so one admitted group holds
    // a blocked mutating Bash, a queued Write and a queued Read. Cancellation
    // must still produce one terminal record per started occurrence.
    let error = h
        .executor
        .run_toolscript(
            r#"
            tools.call("Bash", { command: "sleep 5" });
            tools.call("Write", { file_path: "late.txt", content: "must not exist" });
            tools.call("Read", { file_path: "kick.txt" });
            const kick = tools.call("Read", { file_path: "kick.txt" });
            await kick;
            return "unreachable";
            "#,
            &json!(null),
            "parent-drain",
            &cancellation,
        )
        .await
        .expect_err("cancelled script must fail");
    canceller.await.unwrap();
    assert!(format!("{error}").contains("cancelled"), "{error}");
    assert!(!root.join("late.txt").exists(), "queued Write escaped");

    let events = take_events(&h);
    let seen = observed(&events);
    let starts: std::collections::HashSet<_> =
        seen.starts.iter().map(|(id, _)| id.clone()).collect();
    let dones: std::collections::HashSet<_> =
        seen.dones.iter().map(|done| done.id.clone()).collect();
    assert_eq!(starts.len(), 4, "{seen:?}");
    assert_eq!(
        starts, dones,
        "every started occurrence needs exactly one terminal record"
    );
    assert!(seen.dones.iter().all(|done| done.is_error), "{seen:?}");

    // Persist through the shared writer and reopen: ids, origins and the
    // provider replay exclusion all hold for cancelled work too.
    let (session, _assistant) = persist(root, "parent-drain", &events);
    let reopened = crate::services::session_messages::context(&root.join("data")).unwrap();
    let store = crate::runtime::SessionStore::new(reopened.db.clone());
    let history =
        serde_json::to_string(&store.model_history(&session, &ApiFormat::OpenAI).unwrap()).unwrap();
    for id in &starts {
        assert!(
            !history.contains(id.as_str()),
            "cancelled nested id leaked into provider history: {id}"
        );
    }
    let _ = reopened;
}

#[cfg(unix)]
#[tokio::test]
async fn a_hook_rewritten_background_bash_is_refused_before_the_body() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    let rewrite = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{
        "command":"touch sentinel.txt",
        "run_in_background":true
    }}});
    let command = format!(
        "printf '%s' '{}'",
        rewrite.to_string().replace('\'', "'\"'\"'")
    );
    std::fs::write(
        root.join(".claude/settings.json"),
        json!({"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","shell":"sh","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
    let hooks = crate::services::tool_hooks::HookRunner::load(&root.join("data"), root).unwrap();
    let mut h = harness_with_hooks(root, PermissionMode::BypassPermissions, Some(hooks));

    let outcome = run(
        &mut h,
        r#"
        try {
            await tools.call("Bash", { command: "true" });
            return "ran";
        } catch (error) {
            return error.code;
        }
        "#,
        json!(null),
        "parent-rewrite",
    )
    .await
    .expect("the script catches the refusal");

    assert_eq!(outcome.value, json!("unsupported_tool"));
    assert!(
        !root.join("sentinel.txt").exists(),
        "the hook-rewritten command ran"
    );
    assert!(
        !root.join("shell-tasks").exists(),
        "a managed shell task directory was created"
    );
    let seen = observed(&take_events(&h));
    let bash = seen.dones.iter().find(|done| done.name == "Bash").unwrap();
    assert!(bash.is_error, "{bash:?}");
    assert!(
        bash.output.contains("run_in_background is not supported"),
        "{bash:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn promise_all_reads_overlap_while_mutations_stay_serial() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // FIFO handshake: both readers only see data when both are open at once.
    let a = root.join("a.fifo");
    let b = root.join("b.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&a)
        .arg(&b)
        .status()
        .unwrap();
    assert!(status.success());
    let mut h = harness(root, PermissionMode::BypassPermissions);
    assert!(
        h.executor
            .is_concurrency_safe("Bash", &json!({"command": "cat a.fifo"})),
        "fixture reads must classify as concurrency-safe"
    );

    let writer_root = root.to_path_buf();
    let overlap = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let overlap_flag = overlap.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut first = std::fs::OpenOptions::new()
            .write(true)
            .open(writer_root.join("a.fifo"))
            .unwrap();
        let second = writer_root.join("b.fifo");
        // If the second reader is already waiting (concurrent bodies), the open
        // returns at once. If it is serialized, this blocks until `a` is
        // written and the first command exits.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let handle = {
            let second = second.clone();
            std::thread::spawn(move || std::fs::OpenOptions::new().write(true).open(second))
        };
        while std::time::Instant::now() < deadline && !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(10));
        }
        overlap_flag.store(handle.is_finished(), std::sync::atomic::Ordering::SeqCst);
        let _ = first.write_all(b"alpha\n");
        let _ = first.flush();
        drop(first);
        let mut second = handle.join().unwrap().unwrap();
        let _ = second.write_all(b"beta\n");
        let _ = second.flush();
    });

    let outcome = run(
        &mut h,
        r#"
        const reads = await Promise.all([
            tools.call("Bash", { command: "cat a.fifo" }),
            tools.call("Bash", { command: "cat b.fifo" }),
        ]);
        return reads;
        "#,
        json!(null),
        "parent-concurrency",
    )
    .await
    .expect("both reads finish");
    writer.join().unwrap();
    let reads = outcome.value.as_array().unwrap();
    assert!(reads[0].as_str().unwrap().contains("alpha"), "{reads:?}");
    assert!(reads[1].as_str().unwrap().contains("beta"), "{reads:?}");
    assert!(
        overlap.load(std::sync::atomic::Ordering::SeqCst),
        "safe reads did not overlap"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_hook_rewritten_mutation_does_not_overlap_a_safe_neighbor() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for name in ["first.fifo", "second.fifo"] {
        assert!(std::process::Command::new("mkfifo")
            .arg(root.join(name))
            .status()
            .unwrap()
            .success());
    }
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    // The hook rewrites only the first call's read-only `cat` into a long
    // mutation that waits for the second call's marker. If the two bodies
    // overlap, `a.done` appears; if the rewrite is reclassified as mutating,
    // the first body times out while it runs alone.
    let script = root.join("hook.sh");
    std::fs::write(
        &script,
        r#"input=$(cat)
case "$input" in
  *first.fifo*)
    printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"touch a.started; for i in $(seq 60); do [ -f b.started ] && touch a.done && exit 0; sleep 0.05; done; exit 3"}}}'
    ;;
  *)
    printf '%s' '{}'
    ;;
esac
"#,
    )
    .unwrap();
    std::fs::write(
        root.join(".claude/settings.json"),
        json!({"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","shell":"sh","command":format!("sh {}", script.display())}]}]}})
            .to_string(),
    )
    .unwrap();
    let hooks = crate::services::tool_hooks::HookRunner::load(&root.join("data"), root).unwrap();
    let mut h = harness_with_hooks(root, PermissionMode::BypassPermissions, Some(hooks));
    assert!(
        !h.executor.is_concurrency_safe(
            "Bash",
            &json!({"command": "touch a.started; for i in $(seq 60); do [ -f b.started ] && touch a.done; sleep 0.05; done"})
        ),
        "the rewritten command must classify as mutating"
    );

    // Opening the FIFO for writing blocks until the safe neighbour reads it;
    // only then is its marker created.
    let second = root.join("second.fifo");
    let marker_root = root.to_path_buf();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .open(second)
            .unwrap();
        std::fs::write(marker_root.join("b.started"), b"1").unwrap();
        out.write_all(b"ok\n").unwrap();
        out.flush().unwrap();
    });

    let outcome = run(
        &mut h,
        r#"
        return await Promise.all([
            tools.call("Bash", { command: "cat first.fifo" }).then(() => "ok").catch(() => "error"),
            tools.call("Bash", { command: "cat second.fifo" }).then(() => "ok").catch(() => "error"),
        ]);
        "#,
        json!(null),
        "parent-rebatch",
    )
    .await
    .expect("both calls finish");
    writer.join().unwrap();

    assert_eq!(
        outcome.value,
        json!(["error", "ok"]),
        "the rewritten mutation overlapped its safe neighbour"
    );
    assert!(
        !root.join("a.done").exists(),
        "the rewritten mutation saw its neighbour's marker"
    );
    let seen = observed(&take_events(&h));
    let errors: Vec<_> = seen.dones.iter().filter(|done| done.is_error).collect();
    assert_eq!(errors.len(), 1, "{seen:?}");
}

#[cfg(target_os = "linux")]
fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// PID plus kernel start time (field 22 of `/proc/<pid>/stat`) so PID reuse
/// cannot be mistaken for the original process.
#[cfg(target_os = "linux")]
fn proc_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

#[cfg(target_os = "linux")]
async fn wait_process_gone(pid: u32, start: u64, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if proc_start(pid) != Some(start) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Test-only cleanup for a failed assertion: signals only the exact PIDs the
/// fixture started, never a pattern or a group name.
#[cfg(target_os = "linux")]
struct KillExact(Vec<(u32, u64)>);

#[cfg(target_os = "linux")]
impl Drop for KillExact {
    fn drop(&mut self) {
        for (pid, start) in &self.0 {
            // Never signal a PID whose identity changed since observation.
            if proc_start(*pid) == Some(*start) {
                let _ = std::process::Command::new("kill")
                    .args(["-9", "--", &pid.to_string()])
                    .status();
            }
        }
    }
}

#[tokio::test]
async fn router_can_load_toolscript_but_nested_edits_hit_the_direct_call_gate() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let data_dir = temp.path().join("data");
    let db = crate::runtime::RuntimePaths::new(data_dir.clone())
        .unwrap()
        .open_database()
        .unwrap();
    let router = crate::runtime::SessionStore::new(db.clone())
        .create(&project, "fixture", "fixture")
        .unwrap();
    let runtime = crate::super_agent::crp::ToolRuntime {
        db,
        data_dir,
        session_id: router.clone(),
    };
    crate::super_agent::role::set(&runtime.db, &router, crate::super_agent::Role::Router).unwrap();
    std::fs::write(project.join("read.txt"), "safe").unwrap();
    let mut h = harness(&project, PermissionMode::BypassPermissions);
    h.executor = h
        .executor
        .with_conv_id(router)
        .with_durable_super_agent(runtime)
        .unwrap();
    assert!(!h
        .executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|t| t.name == "ToolScript"));
    h.executor.loaded_tools.insert("ToolScript".into());
    h.executor.loaded_tools.insert("ApplyPatch".into());
    let names: Vec<_> = h
        .executor
        .tool_definitions()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        names.contains(&"ToolScript".to_owned()),
        "safe scripts remain available on demand"
    );
    for name in ["Write", "Edit", "ApplyPatch"] {
        assert!(!names.iter().any(|t| t == name));
        let reason = h.executor.permission_hard_denial(name, &json!({})).unwrap();
        assert!(reason.contains("cannot edit business code"));
    }
    let outcome = run(
        &mut h,
        r#"
        const read = await tools.call("Read", {file_path:"read.txt"});
        const denied = [];
        for (const name of ["Write", "Edit", "ApplyPatch"]) {
            try { await tools.call(name, {file_path:"blocked.txt",content:"bad"}); }
            catch (error) { denied.push({name, code:error.code, message:error.message}); }
        }
        return {read, denied};
    "#,
        json!(null),
        "router-script",
    )
    .await
    .unwrap();
    assert_eq!(outcome.calls_started, 4);
    assert_eq!(outcome.calls_succeeded, 1);
    assert!(outcome.value["read"].as_str().unwrap().contains("safe"));
    let denied = outcome.value["denied"].as_array().unwrap();
    assert_eq!(denied.len(), 3);
    for error in denied {
        assert_eq!(error["code"], "permission_denied");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("cannot edit business code"));
    }
    assert!(!project.join("blocked.txt").exists());
}
