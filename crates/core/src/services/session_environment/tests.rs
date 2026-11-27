use super::*;

#[test]
fn platform_contract() {
    let root = tempfile::tempdir().unwrap();
    let path = hook_file(root.path(), "a", HookEvent::SessionStart, 0).unwrap();
    assert_eq!(path.is_none(), cfg!(windows));
    assert!(script(root.path(), "a").unwrap().is_none());
    assert!(hook_file(root.path(), "a", HookEvent::PreToolUse, 0)
        .unwrap()
        .is_none());
}

#[cfg(unix)]
async fn bash(root: &Path, session: &str, command: &str) -> serde_json::Value {
    // Bash reads the process-wide orchestration defaults for its limits.
    crate::config::OrchestrationConfig::init_global().unwrap();
    crate::tools::background_shell::scope(
        Some((root.into(), session.into())),
        crate::tools::execute_tool_async(
            "Bash",
            serde_json::json!({"command":command}),
            root.to_str().unwrap(),
        ),
    )
    .await
    .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn hooks_feed_bash_isolate_sessions_reset_cwd_and_close() {
    use super::super::{session_hooks, tool_hooks::HookRunner};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let context = crate::services::session_messages::context(root.path()).unwrap();
    let config = json!({"hooks":{
        "Setup":[{"hooks":[{"type":"command","command":"printf 'export CCL_ENV_BASE=setup\\n' > \"$CLAUDE_ENV_FILE\""}]}],
        "SessionStart":[{"hooks":[{"type":"command","command":"printf 'export CCL_ENV_BASE=start\\n' > \"$CLAUDE_ENV_FILE\""}]}],
        "CwdChanged":[{"hooks":[{"type":"command","command":"printf 'export CCL_ENV_CWD=cwd\\n' > \"$CLAUDE_ENV_FILE\""}]}],
        "FileChanged":[{"matcher":"probe","hooks":[{"type":"command","command":"printf 'export CCL_ENV_FILE_VALUE=file\\n' > \"$CLAUDE_ENV_FILE\""}]}]
    }});
    crate::data_layout::write_config_file(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
        config.to_string(),
    )
    .unwrap();
    let runner = HookRunner::load(root.path(), root.path()).unwrap();
    for (event, payload) in [
        (HookEvent::Setup, json!({"trigger":"init"})),
        (HookEvent::SessionStart, json!({"source":"startup"})),
        (HookEvent::CwdChanged, json!({"old_cwd":"a","new_cwd":"b"})),
        (
            HookEvent::FileChanged,
            json!({"file_path":root.path().join("probe"),"event":"change"}),
        ),
    ] {
        let outcome = runner.run(event, "a", payload).await.unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }
    let command =
        "printf '%s:%s:%s' \"${CCL_ENV_BASE-}\" \"${CCL_ENV_CWD-}\" \"${CCL_ENV_FILE_VALUE-}\"";
    assert_eq!(
        bash(root.path(), "a", command).await["stdout"],
        "start:cwd:file"
    );
    assert_eq!(bash(root.path(), "b", command).await["stdout"], "::");
    for (_, _, path) in files(root.path(), "a").unwrap() {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        std::fs::metadata(directory(root.path(), "a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    runner
        .run(
            HookEvent::CwdChanged,
            "a",
            json!({"old_cwd":"b","new_cwd":"c"}),
        )
        .await
        .unwrap();
    assert_eq!(
        bash(root.path(), "a", command).await["stdout"],
        "start:cwd:"
    );
    assert!(session_hooks::end(
        &context,
        root.path(),
        "a",
        "exit",
        std::time::Duration::from_secs(2)
    )
    .await
    .is_empty());
    assert!(!directory(root.path(), "a").exists());
    assert_eq!(bash(root.path(), "a", command).await["stdout"], "::");
}

#[cfg(unix)]
#[tokio::test]
async fn numeric_order_fresh_reads_and_symlink_rejection() {
    let root = tempfile::tempdir().unwrap();
    for index in [10, 2] {
        let path = hook_file(root.path(), "a", HookEvent::SessionStart, index)
            .unwrap()
            .unwrap();
        std::fs::write(path, format!("export CCL_ENV_ORDER={index}\n")).unwrap();
    }
    assert_eq!(
        bash(root.path(), "a", "printf %s $CCL_ENV_ORDER").await["stdout"],
        "10"
    );
    let path = hook_file(root.path(), "a", HookEvent::SessionStart, 10)
        .unwrap()
        .unwrap();
    std::fs::write(&path, "export CCL_ENV_ORDER=updated\n").unwrap();
    assert_eq!(
        bash(root.path(), "a", "printf %s $CCL_ENV_ORDER").await["stdout"],
        "updated"
    );
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(root.path().join("outside"), &path).unwrap();
    assert!(hook_file(root.path(), "a", HookEvent::SessionStart, 10).is_err());
    assert!(script(root.path(), "a").is_err());
    assert!(!root.path().join("outside").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn async_writer_and_failed_script_are_observed_by_next_bash() {
    use super::super::{session_hooks, tool_hooks::HookRunner};
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let context = crate::services::session_messages::context(root.path()).unwrap();
    crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"), json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","async":true,"command":"sleep 0.1; printf 'export CCL_ASYNC_ENV=ready\\n' > \"$CLAUDE_ENV_FILE\""}]}]}}).to_string()).unwrap();
    let runner = HookRunner::load(root.path(), root.path()).unwrap();
    runner
        .run(
            HookEvent::SessionStart,
            "async",
            json!({"source":"startup"}),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if bash(root.path(), "async", "printf %s \"${CCL_ASYNC_ENV-}\"").await["stdout"]
                == "ready"
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let path = hook_file(root.path(), "async", HookEvent::SessionStart, 0)
        .unwrap()
        .unwrap();
    std::fs::write(path, "false\n").unwrap();
    assert_eq!(
        bash(root.path(), "async", "printf SHOULD_NOT_RUN").await["stdout"],
        ""
    );
    assert!(session_hooks::end(
        &context,
        root.path(),
        "async",
        "exit",
        std::time::Duration::from_secs(2)
    )
    .await
    .is_empty());
    assert!(!directory(root.path(), "async").exists());
}
