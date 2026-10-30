use super::*;

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}

fn executor(root: &std::path::Path) -> ToolLoopExecutor {
    let (tx, _rx) = mpsc::channel(64);
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
    ToolLoopExecutor::new(
        provider(),
        vec![],
        None,
        128,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("parallel-fixture".into())
    .with_permission_manager(manager)
}

fn call(id: &str, name: &str, input: Value, concurrent: bool) -> PendingToolCall {
    PendingToolCall {
        id: id.into(),
        name: name.into(),
        input,
        concurrent,
    }
}

#[test]
fn batches_group_only_consecutive_concurrency_safe_calls() {
    let calls: Vec<_> = [true, true, false, true, false, true, true]
        .into_iter()
        .enumerate()
        .map(|(index, concurrent)| call(&index.to_string(), "Read", json!({}), concurrent))
        .collect();
    let sizes: Vec<_> = tool_batches(&calls)
        .iter()
        .map(|batch| batch.len())
        .collect();
    assert_eq!(sizes, [2, 1, 1, 1, 2]);
    assert!(tool_batches(&[]).is_empty());
}

#[test]
fn only_reading_calls_are_concurrency_safe() {
    let temp = tempfile::tempdir().unwrap();
    let executor = executor(temp.path());
    for (name, input, safe) in [
        ("Read", json!({"file_path":"a"}), true),
        ("Grep", json!({"pattern":"a"}), true),
        ("WebFetch", json!({"url":"https://example.com"}), true),
        ("Bash", json!({"command":"git status && ls -la"}), true),
        ("Bash", json!({"command":"rm -rf build"}), false),
        ("Bash", json!({"command":"cat a > b"}), false),
        (
            "Bash",
            json!({"command":"ls","run_in_background":true}),
            false,
        ),
        ("Write", json!({"file_path":"a","content":""}), false),
        ("Edit", json!({"file_path":"a"}), false),
        ("Agent", json!({"prompt":"x"}), false),
        ("AskUserQuestion", json!({}), false),
        ("mcp__server__read", json!({}), false),
    ] {
        assert_eq!(
            executor.is_concurrency_safe(name, &input),
            safe,
            "{name} {input}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_batch_runs_together_with_hooks_and_ordered_results() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".claude")).unwrap();
    let hook = json!({"type":"command","shell":"sh","command":"cat >> hooks.jsonl; echo >> hooks.jsonl","timeout":10});
    std::fs::write(
        temp.path().join(".claude/settings.json"),
        json!({"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[hook]}],"PostToolUse":[{"matcher":"^Bash$","hooks":[hook]}]}}).to_string(),
    )
    .unwrap();
    let hooks =
        crate::services::tool_hooks::HookRunner::load(&temp.path().join("data"), temp.path())
            .unwrap();
    let mut executor = executor(temp.path()).with_hooks(hooks);
    // Each command waits for the other's marker, so they only both finish when they overlap.
    let rendezvous = |own: &str, other: &str| json!({"command": format!("touch {own}.ready; for i in $(seq 200); do [ -f {other}.ready ] && echo {own}-done && exit 0; sleep 0.05; done; exit 1")});
    let calls = [
        call("first", "Bash", rendezvous("a", "b"), true),
        call("second", "Bash", rendezvous("b", "a"), true),
    ];
    let results = executor.execute_tool_batch(&calls).await;
    assert_eq!(results.len(), 2);
    assert!(
        !results[0].2 && results[0].1.contains("a-done"),
        "{}",
        results[0].1
    );
    assert!(
        !results[1].2 && results[1].1.contains("b-done"),
        "{}",
        results[1].1
    );
    let events: Vec<Value> = std::fs::read_to_string(temp.path().join("hooks.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let seen: Vec<_> = events
        .iter()
        .map(|event| {
            format!(
                "{}:{}",
                event["hook_event_name"].as_str().unwrap(),
                event["tool_use_id"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            "PreToolUse:first",
            "PreToolUse:second",
            "PostToolUse:first",
            "PostToolUse:second"
        ]
    );
}

#[tokio::test]
async fn files_read_in_a_batch_can_be_edited_afterwards() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha").unwrap();
    std::fs::write(temp.path().join("b.txt"), "beta").unwrap();
    let mut executor =
        executor(temp.path()).with_search_config_path(temp.path().join("settings.json"));
    let reads = [
        call("read-a", "Read", json!({"file_path":"a.txt"}), true),
        call("read-b", "Read", json!({"file_path":"b.txt"}), true),
    ];
    let results = executor.execute_tool_batch(&reads).await;
    assert!(results[0].1.contains("alpha") && results[1].1.contains("beta"));
    assert!(results.iter().all(|result| !result.2));
    let (_, output, failed) = executor
        .execute_tool_call(
            "Edit",
            &json!({"file_path":"b.txt","old_string":"beta","new_string":"gamma"}),
            "edit-b",
        )
        .await;
    assert!(!failed, "{output}");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "gamma"
    );
}
