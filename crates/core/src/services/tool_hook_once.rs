//! Durable, session-scoped success records for command hooks.
use super::*;
use std::io::{Read, Seek, SeekFrom, Write};

impl HookRunner {
    async fn execute_configured(
        &self,
        hook: &ActiveHook,
        session: &str,
        input: &Value,
        transcript: Option<&std::sync::OnceLock<PathBuf>>,
    ) -> Result<(i32, String, String)> {
        let prepared = transcript.map(|cache| self.transcript_input(session, input.clone(), cache));
        let input = prepared.as_ref().unwrap_or(input);
        if let Some(config) = &hook.prompt {
            let options = self
                .recording_db
                .as_ref()
                .map(|db| {
                    crate::native_engine::request_options::RequestOptions::from_db(
                        db,
                        Some(session),
                        "hook",
                    )
                })
                .unwrap_or_else(|| {
                    crate::native_engine::request_options::RequestOptions::from_root(
                        self.once_directory.parent().unwrap_or(&self.workspace),
                        Some(session),
                        "hook",
                    )
                });
            if config.is_agent() {
                return agent::execute_agent(
                    hook,
                    config,
                    self.prompt_provider.as_ref(),
                    input,
                    &self.workspace,
                    options,
                )
                .await;
            }
            return prompt::execute_prompt(
                hook,
                config,
                self.prompt_provider.as_ref(),
                input,
                options,
            )
            .await;
        }
        let env_file = if hook.http.is_none()
            && !matches!(hook.shell.as_deref(), Some("powershell" | "pwsh"))
        {
            let index = self
                .hooks
                .iter()
                .take_while(|entry| !std::ptr::eq(*entry, hook))
                .filter(|entry| entry.event == hook.event)
                .count();
            super::super::session_environment::hook_file(
                self.once_directory.parent().unwrap_or(&self.workspace),
                session,
                hook.event,
                index,
            )?
        } else {
            None
        };
        if hook.asynchronous
            && !self.force_sync
            && !matches!(hook.event, HookEvent::CwdChanged | HookEvent::FileChanged)
        {
            let task = prepare_command(hook, &self.workspace, input, env_file.as_deref())?;
            super::super::async_hooks::register(
                self.once_directory
                    .parent()
                    .context("Async hook state directory is missing")?,
                session,
                task,
                hook.async_rewake.then(|| {
                    let key = match hook.event {
                        HookEvent::SessionStart => "source",
                        HookEvent::SessionEnd => "reason",
                        HookEvent::PreCompact | HookEvent::PostCompact => "trigger",
                        HookEvent::SubagentStart | HookEvent::SubagentStop => "agent_type",
                        _ => "tool_name",
                    };
                    let query = input.get(key).and_then(Value::as_str).unwrap_or("");
                    if query.is_empty() {
                        hook.event.name().to_owned()
                    } else {
                        format!("{}:{query}", hook.event.name())
                    }
                }),
            )?;
            Ok((0, String::new(), String::new()))
        } else {
            if hook.http.is_some() {
                execute(hook, &self.workspace, input).await
            } else {
                prepare_command(hook, &self.workspace, input, env_file.as_deref())?.await
            }
        }
    }
    pub(super) async fn execute_once(
        &self,
        hook: &ActiveHook,
        session: &str,
        input: &Value,
        transcript: Option<&std::sync::OnceLock<PathBuf>>,
    ) -> Result<Option<(i32, String, String)>> {
        if !hook.once {
            return self
                .execute_configured(hook, session, input, transcript)
                .await
                .map(Some);
        }
        ensure!(
            !self.once_directory.as_os_str().is_empty(),
            "Once hook state directory is missing"
        );
        // Identical registrations remain independent; unrelated config additions
        // do not invalidate an already completed registration.
        let definition = serde_json::to_value(hook)?;
        let occurrence = self
            .hooks
            .iter()
            .take_while(|entry| !std::ptr::eq(*entry, hook))
            .filter(|entry| serde_json::to_value(entry).ok().as_ref() == Some(&definition))
            .count();
        let identity = serde_json::to_vec(&(session, &self.workspace, definition, occurrence))?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &identity);
        let name: String = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        std::fs::create_dir_all(&self.once_directory)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.once_directory.join(name))
            .context("Opening once hook state")?;
        // OS ownership is released on cancellation or process exit. Never unlink
        // this file: another process may already hold its handle.
        tokio::time::timeout(Duration::from_secs(hook.timeout), async {
            loop {
                match crate::fs::try_lock_file(&file) {
                    Ok(()) => return Ok::<_, anyhow::Error>(()),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                }
            }
        })
        .await
        .context("Timed out waiting for once hook")??;
        let mut completed = String::new();
        file.read_to_string(&mut completed)?;
        if completed == "success\n" {
            return Ok(None);
        }
        ensure!(
            completed.is_empty(),
            "Invalid once hook state; refusing to replay"
        );
        let output = self
            .execute_configured(hook, session, input, transcript)
            .await?;
        if output.0 == 0 {
            if hook.asynchronous
                && !self.force_sync
                && !matches!(hook.event, HookEvent::CwdChanged | HookEvent::FileChanged)
            {
                // Background registration is success, regardless of eventual exit.
            } else if hook.event == HookEvent::WorktreeCreate {
                ensure!(
                    !output.1.trim().is_empty(),
                    "WorktreeCreate returned no path"
                );
                if output.1.trim().starts_with('{') {
                    let value: Value = serde_json::from_str(output.1.trim())?;
                    ensure!(
                        value["hookSpecificOutput"]["worktreePath"].is_string(),
                        "WorktreeCreate JSON must provide hookSpecificOutput.worktreePath"
                    );
                }
            } else if hook.event != HookEvent::WorktreeRemove {
                parse_hook_output(hook, output.0, &output.1, &output.2)?;
            }
            file.seek(SeekFrom::Start(0))?;
            file.write_all(b"success\n")?;
            file.sync_all()?;
        }
        Ok(Some(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path, event: &str, command: &str, extra: Value) -> HookRunner {
        let mut hook = json!({"type":"command", "once":true, "command":command,
            "shell":if cfg!(windows) {"powershell"} else {"sh"}, "timeout":10});
        hook.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        std::fs::write(
            root.join("settings.json"),
            json!({"hooks":{event:[{"matcher":"Read","hooks":[hook]}]}}).to_string(),
        )
        .unwrap();
        HookRunner::load(root, root).unwrap()
    }

    fn append() -> &'static str {
        if cfg!(windows) {
            "[IO.File]::AppendAllText('runs','x')"
        } else {
            "printf x >> runs"
        }
    }

    async fn run(runner: &HookRunner, session: &str) -> Result<HookOutcome> {
        runner
            .run(
                HookEvent::PreToolUse,
                session,
                json!({"tool_name":"Read","tool_input":{}}),
            )
            .await
    }

    #[tokio::test]
    async fn once_survives_reload_and_isolates_sessions_and_concurrent_runners() {
        let root = tempfile::tempdir().unwrap();
        let first = fixture(root.path(), "PreToolUse", append(), json!({"if":"Read"}));
        first
            .run(HookEvent::PreToolUse, "a", json!({"tool_name":"Write"}))
            .await
            .unwrap();
        assert!(!root.path().join("runs").exists());
        let second = HookRunner::load(root.path(), root.path()).unwrap();
        let (a, b) = tokio::join!(run(&first, "a"), run(&second, "a"));
        a.unwrap();
        b.unwrap();
        run(&HookRunner::load(root.path(), root.path()).unwrap(), "a")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("runs")).unwrap(),
            "x"
        );
        run(&second, "b").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("runs")).unwrap(),
            "xx"
        );
    }

    #[tokio::test]
    async fn once_failures_and_invalid_output_are_not_success() {
        for suffix in ["; exit 2", "; echo '{invalid'"] {
            let root = tempfile::tempdir().unwrap();
            let runner = fixture(
                root.path(),
                "PreToolUse",
                &format!("{}{suffix}", append()),
                json!({}),
            );
            for _ in 0..2 {
                let result = run(&runner, "a").await;
                if suffix.contains("exit") {
                    assert!(result.unwrap().blocked);
                } else {
                    assert!(result.is_err());
                }
            }
            assert_eq!(
                std::fs::read_to_string(root.path().join("runs")).unwrap(),
                "xx"
            );
        }
    }

    #[tokio::test]
    async fn once_timeout_and_cancellation_release_ownership() {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let command = if cfg!(windows) {
                "[IO.File]::AppendAllText('runs','x'); if (!(Test-Path 'release')) { Start-Sleep -Seconds 30 }"
            } else {
                "printf x >> runs; if [ ! -f release ]; then sleep 30; fi"
            };
            let runner = std::sync::Arc::new(fixture(
                root.path(),
                "PreToolUse",
                command,
                json!({"timeout":2}),
            ));
            let worker = runner.clone();
            let task = tokio::spawn(async move { run(&worker, "a").await });
            tokio::time::timeout(Duration::from_secs(10), async {
                while !root.path().join("runs").exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert!(task
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("timed out"));
            }
            std::fs::write(root.path().join("release"), "").unwrap();
            run(&runner, "a").await.unwrap();
            run(&runner, "a").await.unwrap();
            assert_eq!(
                std::fs::read_to_string(root.path().join("runs")).unwrap(),
                "xx"
            );
        }
    }

    #[tokio::test]
    async fn once_permission_request_and_worktree_remove_use_same_guard() {
        for event in [HookEvent::PermissionRequest, HookEvent::WorktreeRemove] {
            let root = tempfile::tempdir().unwrap();
            let runner = fixture(root.path(), event.name(), append(), json!({}));
            for _ in 0..2 {
                if event == HookEvent::WorktreeRemove {
                    runner.run_worktree(event, "a", json!({})).await.unwrap();
                } else {
                    runner
                        .run(event, "a", json!({"tool_name":"Read"}))
                        .await
                        .unwrap();
                }
            }
            assert_eq!(
                std::fs::read_to_string(root.path().join("runs")).unwrap(),
                "x"
            );
        }
    }

    #[test]
    fn once_rejects_non_boolean_config() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("settings.json"), r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo ok","once":"true"}]}]}}"#).unwrap();
        assert!(HookRunner::load(root.path(), root.path()).is_err());
    }

    #[tokio::test]
    async fn once_payload_and_duplicate_registrations_remain_independent() {
        let root = tempfile::tempdir().unwrap();
        let command = if cfg!(windows) {
            "[IO.File]::WriteAllText('payload.json',[Console]::In.ReadToEnd()); [IO.File]::AppendAllText('runs','x')"
        } else {
            "cat > payload.json; printf x >> runs"
        };
        fixture(root.path(), "PreToolUse", command, json!({}));
        let path = root.path().join("settings.json");
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let hooks = settings["hooks"]["PreToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap();
        hooks.push(hooks[0].clone());
        std::fs::write(&path, settings.to_string()).unwrap();
        let runner = HookRunner::load(root.path(), root.path()).unwrap();
        run(&runner, "payload-session").await.unwrap();
        run(&runner, "payload-session").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("runs")).unwrap(),
            "xx"
        );
        let payload: Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("payload.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            payload,
            json!({"session_id":"payload-session", "cwd":root.path(), "transcript_path":crate::services::lifecycle_events::transcript(root.path(),"payload-session").unwrap(), "hook_event_name":"PreToolUse", "tool_name":"Read", "tool_input":{}})
        );
    }

    #[tokio::test]
    #[ignore = "subprocess helper invoked by once_cross_process"]
    async fn once_process_helper() {
        let path = PathBuf::from(std::env::var_os("CCL_ONCE_TEST_ROOT").unwrap());
        run(&HookRunner::load(&path, &path).unwrap(), "process-session")
            .await
            .unwrap();
    }

    #[test]
    fn once_cross_process() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "PreToolUse", append(), json!({}));
        let spawn = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "services::tool_hooks::once::tests::once_process_helper",
                ])
                .env("CCL_ONCE_TEST_ROOT", root.path())
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert!(spawn().wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(root.path().join("runs")).unwrap(),
            "x"
        );
    }
}
