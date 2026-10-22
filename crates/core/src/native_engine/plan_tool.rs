use super::*;
use crate::{permissions::PermissionMode, services::plans};
use anyhow::Context;
#[cfg(test)]
#[path = "plan_approval_tests.rs"]
mod approval_tests;

/// Semantic Bash intents are recorded for review; they never become blanket rules.
fn plan_allowed_prompts(input: &Value) -> Result<Vec<Value>> {
    let Some(items) = input.get("allowedPrompts") else {
        return Ok(Vec::new());
    };
    let items = items
        .as_array()
        .context("allowedPrompts must be an array")?;
    anyhow::ensure!(items.len() <= 32, "Too many allowedPrompts");
    let mut prompts = Vec::with_capacity(items.len());
    for item in items {
        anyhow::ensure!(
            item["tool"] == "Bash",
            "allowedPrompts only support the Bash tool"
        );
        let prompt = item["prompt"]
            .as_str()
            .filter(|prompt| !prompt.trim().is_empty())
            .context("allowedPrompts.prompt is required")?;
        anyhow::ensure!(prompt.len() <= 500, "allowedPrompts.prompt is too long");
        prompts.push(json!({"tool":"Bash","prompt":prompt}));
    }
    Ok(prompts)
}

impl ToolLoopExecutor {
    pub(super) fn is_plan_document_tool(&self, tool: &str, input: &Value) -> bool {
        let Some(root) = self
            .search_config_path
            .as_deref()
            .and_then(std::path::Path::parent)
        else {
            return false;
        };
        let Some(session) = self.conv_id.as_deref() else {
            return false;
        };
        self.permission_manager
            .as_ref()
            .is_some_and(|pm| pm.get_mode() == PermissionMode::PlanMode)
            && plans::is_document_tool(root, session, tool, input)
    }

    pub(super) async fn execute_plan_document(
        &mut self,
        tool: &str,
        input: &Value,
        id: &str,
    ) -> (Value, String, bool) {
        let mut actual = input.clone();
        let result = async {
            match self.check_permission(tool, input, id).await {
                PermissionResult::Denied(reason) => anyhow::bail!("{reason}"),
                PermissionResult::RequiresConfirmation(_) => {
                    actual = self.request_tool_approval(tool, input, id).await?;
                }
                PermissionResult::Granted => {}
            }
            let input = &actual;
            if let PermissionResult::Denied(reason) = self.check_permission(tool, input, id).await { anyhow::bail!("{reason}"); }
            anyhow::ensure!(self.is_plan_document_tool(tool, input), "Plan permission or file path changed; retry the operation");
            let root = self.search_config_path.as_deref().and_then(std::path::Path::parent).context("Plan storage unavailable")?;
            let session = self.conv_id.as_deref().context("Plan session unavailable")?;
            if tool == "Read" {
                let content = plans::read(root, session)?.context("Plan file does not exist")?;
                let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
                let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
                let lines: Vec<_> = content.lines().collect();
                let selected: Vec<_> = lines.iter().skip(offset.saturating_sub(1)).take(limit).enumerate().map(|(i,line)|format!("{:>6}\t{}", i+offset, line)).collect();
                return Ok(json!({"content":selected.join("\n"),"total_lines":lines.len(),"file_path":plans::path(root, session)?}));
            }
            plans::edit_document(root, session, tool, input)
        }.await;
        (
            actual,
            match &result {
                Ok(value) => value.to_string(),
                Err(error) => format!("Plan file: {error:#}"),
            },
            result.is_err(),
        )
    }

    pub(super) async fn execute_plan_transition(
        &mut self,
        tool_name: &str,
        input: &Value,
        id: &str,
    ) -> (Value, String, bool) {
        let result = self.plan_transition(tool_name, input, id).await;
        let failed = result.is_err();
        (
            input.clone(),
            match result {
                Ok(value) => value.to_string(),
                Err(error) => format!("Plan mode: {error:#}"),
            },
            failed,
        )
    }

    async fn plan_transition(&mut self, tool_name: &str, input: &Value, id: &str) -> Result<Value> {
        let entering = tool_name == "EnterPlanMode"
            || (tool_name == "plan_mode" && input["operation"] == "enter");
        match self.check_permission(tool_name, input, id).await {
            PermissionResult::Denied(reason) => anyhow::bail!("{reason}"),
            PermissionResult::RequiresConfirmation(_) if entering => {
                self.request_tool_approval(tool_name, input, id).await?;
            }
            _ => {}
        }
        let manager = self
            .permission_manager
            .clone()
            .ok_or_else(|| anyhow!("Permission manager unavailable"))?;
        let root = self
            .search_config_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| anyhow!("Plan storage unavailable"))?
            .to_path_buf();
        let session = self
            .conv_id
            .clone()
            .ok_or_else(|| anyhow!("Plan session unavailable"))?;
        let path = plans::path(&root, &session)?;
        if entering {
            if tool_name == "plan_mode" {
                let mut plan = crate::tools::coordination::execute(
                    tool_name,
                    input.clone(),
                    &self.workspace_cwd,
                )?;
                plan["mode"] = json!("plan");
                plan["planFilePath"] = json!(path);
                manager.set_mode(PermissionMode::PlanMode);
                return Ok(plan);
            }
            manager.set_mode(PermissionMode::PlanMode);
            return Ok(
                json!({"mode":"plan","planFilePath":path,"plan":plans::read(&root,&session)?,
                "message":"Planning enabled. Write/edit only planFilePath with Write or Edit, then call ExitPlanMode to request approval. Do not modify project files."}),
            );
        }
        anyhow::ensure!(
            manager.get_mode() == PermissionMode::PlanMode,
            "Enter plan mode before requesting plan approval"
        );
        let allowed_prompts = plan_allowed_prompts(input)?;
        if let Some(plan) = input.get("plan") {
            plans::save(
                &root,
                &session,
                plan.as_str()
                    .ok_or_else(|| anyhow!("plan must be Markdown text"))?,
            )?;
        } else if tool_name == "plan_mode" {
            let current = crate::tools::coordination::execute(
                "plan_mode",
                json!({"operation":"get_current"}),
                &self.workspace_cwd,
            )?;
            if !current.is_null() {
                plans::save(
                    &root,
                    &session,
                    &format!(
                        "# Implementation plan\n\n```json\n{}\n```\n",
                        serde_json::to_string_pretty(&current)?
                    ),
                )?;
            }
        }
        let plan = plans::read(&root, &session)?
            .filter(|plan| !plan.trim().is_empty())
            .ok_or_else(|| anyhow!("No plan written yet; write the session plan file, then call ExitPlanMode (or supply plan text)"))?;
        let expected_mode = manager.mode_snapshot();
        anyhow::ensure!(
            expected_mode.mode == PermissionMode::PlanMode,
            "Planning mode changed before approval"
        );
        if let Some(team) = crate::services::teams::team_for_session(&root, &session)? {
            if team.lead_session_id != session {
                use crate::services::teams::{self, PlanDecision};
                let legacy_warning = if tool_name == "plan_mode" {
                    crate::tools::coordination::execute(
                        "plan_mode",
                        json!({"operation":"exit"}),
                        &self.workspace_cwd,
                    )
                    .err()
                    .map(|e| format!("Legacy plan state could not be updated: {e:#}"))
                } else {
                    None
                };
                return match teams::plan_decision(&root, &team.name, &session)? {
                    PlanDecision::Approved { feedback } => {
                        let context = crate::services::session_messages::context(&root)?;
                        crate::services::plan_approval::commit_approved_mode(
                            &context,
                            &session,
                            &manager,
                            expected_mode,
                            PermissionMode::AcceptEdits,
                            &plan,
                        )?;
                        Ok(json!({
                            "approved":true,
                            "mode":"accept_edits",
                            "approvedBy":teams::LEAD_NAME,
                            "feedback":feedback,
                            "planFilePath":path,
                            "plan":plan,
                            "allowedPrompts":allowed_prompts,
                            "warning":legacy_warning,
                            "message":"Plan approved by the team leader. Existing deny and confirmation rules remain in force."
                        }))
                    }
                    PlanDecision::Rejected { feedback } => Ok(json!({
                        "approved":false,
                        "mode":"plan",
                        "rejectedBy":teams::LEAD_NAME,
                        "feedback":feedback,
                        "message":"The team leader rejected the plan. Keep planning and apply the feedback before requesting approval again."
                    })),
                    PlanDecision::Pending => {
                        if teams::pending_plan_request(&root, &team.name, &session)?.is_none() {
                            let name = teams::member_for_session(&team, &session)
                                .map(|member| member.name.clone())
                                .unwrap_or_else(|| "teammate".into());
                            let request = teams::create_request(
                                &root,
                                &team.name,
                                "plan_approval",
                                &session,
                                &team.lead_session_id,
                                600_000,
                                Some(&name),
                                Some(&plan),
                            )?;
                            let content = json!({
                                "type":"plan_approval_request",
                                "request_id":request.id,
                                "teammate":name,
                                "plan":plan,
                                "allowedPrompts":allowed_prompts
                            })
                            .to_string();
                            let context = crate::services::session_messages::context(&root)?;
                            crate::services::session_messages::send(
                                &context,
                                &session,
                                &team.lead_session_id,
                                &content,
                            )?;
                        }
                        Ok(json!({
                            "approved":false,
                            "mode":"plan",
                            "submitted":true,
                            "message":"Plan submitted to the team leader for approval. Do not modify project files until the leader responds."
                        }))
                    }
                };
            }
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.answer_waiters
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let _waiter = PermissionWaiterGuard {
            waiters: self.answer_waiters.clone(),
            request_id: request_id.clone(),
        };
        let question = format!("Review plan\n{}\n\n{plan}\n\nChoose 1–4, or feedback: <changes>. Clear context preserves this session and its archived transcript.", path.display());
        let sent = self
            .event_tx
            .send(EngineEvent::AskUser {
                request_id: request_id.clone(),
                tool_use_id: id.into(),
                tool_input: json!({"plan":plan,"planFilePath":path,"planApproval":true,"question":question,"options":crate::services::plan_approval::OPTIONS.iter().map(|label| json!({"label":label})).collect::<Vec<_>>(),"multiSelect":false}),
                question,
                options: crate::services::plan_approval::OPTIONS.iter().map(|s|s.to_string()).collect(),
            })
            .await;
        if sent.is_err() {
            self.answer_waiters.lock().await.remove(&request_id);
            anyhow::bail!("Approval host disconnected; planning mode retained");
        }
        let answer = self.wait_for_user_answer(rx).await;
        self.answer_waiters.lock().await.remove(&request_id);
        let answer = answer.context("Plan approval cancelled; planning mode retained")??;
        use crate::services::plan_approval::{self, Choice};
        let choice = plan_approval::parse(&answer)?;
        anyhow::ensure!(
            plans::read(&root, &session)?.as_deref() == Some(plan.as_str()),
            "Plan changed during review; request approval again"
        );
        anyhow::ensure!(
            manager.get_mode() == PermissionMode::PlanMode,
            "Permission mode changed during review; approval was not applied"
        );
        if let Choice::Revise(feedback) = choice {
            return Ok(
                json!({"approved":false,"mode":"plan","feedback":feedback,"message":"Keep planning. Apply the user's feedback before requesting approval again."}),
            );
        }
        let clear = choice == Choice::ClearEdits;
        let context = crate::services::session_messages::context(&root)?;
        if clear {
            let message = format!("Implement the following approved plan:\n\n{plan}\n\nPrior conversation context has been cleared. Its transcript remains archived locally in session {session}. Existing permission rules remain in force.");
            let boundary = plan_approval::commit_approved_boundary(
                &context,
                &session,
                &message,
                &manager,
                expected_mode,
            )?;
            self.pending_plan_reset = Some((boundary, message));
        }
        let mode = if choice == Choice::Manual {
            PermissionMode::AskPermissions
        } else {
            PermissionMode::AcceptEdits
        };
        if !clear {
            plan_approval::commit_approved_mode(
                &context,
                &session,
                &manager,
                expected_mode,
                mode,
                &plan,
            )?;
        }
        // The old project-wide plan record is a compatibility mirror, never the
        // authority for permission changes. Update only after approval commits.
        let legacy_warning = if tool_name == "plan_mode" {
            crate::tools::coordination::execute(
                "plan_mode",
                json!({"operation":"exit"}),
                &self.workspace_cwd,
            )
            .err()
            .map(|e| format!("Legacy plan state could not be updated: {e:#}"))
        } else {
            None
        };
        Ok(
            json!({"approved":true,"mode":if mode == PermissionMode::AskPermissions {"ask"} else {"accept_edits"},"clearContext":clear,"planFilePath":path,"plan":plan,"allowedPrompts":allowed_prompts,
            "warning":legacy_warning,"message":"Plan approved under the selected permission mode. Existing deny and confirmation rules remain in force."}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enter_plan_confirmation_and_child_authority_are_isolated() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        for name in ["EnterPlanMode", "plan_mode"] {
            for answer in [Some("yes"), Some("no"), None] {
                let root = tempfile::tempdir().unwrap();
                let parent = crate::runtime::default_permissions();
                parent.set_mode(PermissionMode::BypassPermissions);
                parent.set_session_rules(
                    "parent",
                    std::collections::HashMap::from([
                        (
                            name.into(),
                            crate::permissions::rules::PermissionAction::Confirm,
                        ),
                        (
                            "Write".into(),
                            crate::permissions::rules::PermissionAction::Deny,
                        ),
                    ]),
                );
                let child = Arc::new(parent.fork_session("parent", "child"));
                let provider = ResolvedProvider {
                provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
                model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
            };
                let (tx, mut events) = mpsc::channel(16);
                let mut executor = ToolLoopExecutor::new(
                    provider,
                    vec![],
                    None,
                    128,
                    tx,
                    root.path().to_string_lossy().into_owned(),
                    ReasoningMode::Standard,
                )
                .with_conv_id("child".into())
                .with_search_config_path(root.path().join("settings.json"))
                .with_permission_manager(child.clone());
                let input = if name == "plan_mode" {
                    let plan = crate::tools::coordination::execute("plan_mode", json!({"operation":"create","task":"legacy test","steps":[{"description":"Inspect"}]}), &executor.workspace_cwd).unwrap();
                    json!({"operation":"enter","plan_id":plan["plan_id"]})
                } else {
                    json!({})
                };
                let waiters = executor.get_answer_waiters();
                let task = tokio::spawn(async move {
                    executor
                        .execute_tool_call(name, &input, "enter-child")
                        .await
                });
                let EngineEvent::ToolPermission {
                    request_id,
                    tool_name,
                    ..
                } = events.recv().await.unwrap()
                else {
                    panic!("Explicit confirmation was bypassed");
                };
                assert_eq!(tool_name, name);
                assert_eq!(child.get_mode(), PermissionMode::BypassPermissions);
                let sender = waiters.lock().await.remove(&request_id).unwrap();
                if let Some(answer) = answer {
                    sender.send(answer.into()).unwrap();
                } else {
                    drop(sender);
                }
                let (_, output, failed) = task.await.unwrap();
                assert_eq!(failed, answer != Some("yes"), "{output}");
                if name == "plan_mode" {
                    let current = crate::tools::coordination::execute(
                        "plan_mode",
                        json!({"operation":"get_current"}),
                        &root.path().to_string_lossy(),
                    )
                    .unwrap();
                    assert_eq!(current.is_null(), failed);
                }
                assert_eq!(parent.get_mode(), PermissionMode::BypassPermissions);
                assert_eq!(
                    child.get_mode(),
                    if failed {
                        PermissionMode::BypassPermissions
                    } else {
                        PermissionMode::PlanMode
                    }
                );
                child.set_mode(PermissionMode::BypassPermissions);
                let context = crate::permissions::PermissionContext {
                    tool_name: "Write".into(),
                    tool_input: json!({}),
                    conversation_id: "child".into(),
                    user_id: None,
                    workspace_path: None,
                };
                assert!(matches!(
                    child.check_permission(&context),
                    PermissionResult::Denied(_)
                ));
                child.set_session_rules("child", Default::default());
                let context = crate::permissions::PermissionContext {
                    conversation_id: "parent".into(),
                    ..context
                };
                assert!(matches!(
                    parent.check_permission(&context),
                    PermissionResult::Denied(_)
                ));
                assert!(waiters.lock().await.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn plan_approval_requires_explicit_answer_and_preserves_read_only_on_failure() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        for answer in [
            Some("yes"),
            Some("no"),
            None,
            Some("changed"),
            Some("always_allow"),
            Some("legacy"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let context = crate::services::session_messages::context(root.path()).unwrap();
            context.db.with_conn(|conn|conn.execute("INSERT INTO conversations(id,created_at,updated_at) VALUES('plan-session','','')",[])).unwrap().unwrap();
            let manager = crate::runtime::default_permissions();
            manager.set_mode(PermissionMode::BypassPermissions);
            let provider = ResolvedProvider {
                provider: serde_json::from_value(json!({"id":"plan-test","name":"plan-test","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
                model: serde_json::from_value(json!({"id":"plan-test","name":"plan-test","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
            };
            let (tx, mut rx) = mpsc::channel(16);
            let mut executor = ToolLoopExecutor::new(
                provider,
                vec![],
                None,
                128,
                tx,
                root.path().to_string_lossy().into_owned(),
                ReasoningMode::Standard,
            )
            .with_conv_id("plan-session".into())
            .with_search_config_path(root.path().join("settings.json"))
            .with_permission_manager(manager.clone());
            let (_, output, failed) = executor
                .execute_tool_call("EnterPlanMode", &json!({}), "enter")
                .await;
            assert!(!failed, "{output}");
            assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
            let (_, _, failed) = executor
                .execute_tool_call(
                    "Write",
                    &json!({"file_path":"blocked.txt","content":"no"}),
                    "write",
                )
                .await;
            assert!(failed);
            assert!(!root.path().join("blocked.txt").exists());
            let (_, _, failed) = executor
                .execute_tool_call("ExitPlanMode", &json!({}), "empty")
                .await;
            assert!(failed);
            manager.set_session_rules(
                "plan-session",
                std::collections::HashMap::from([(
                    "ExitPlanMode".into(),
                    crate::permissions::rules::PermissionAction::Deny,
                )]),
            );
            let (_, _, denied) = executor
                .execute_tool_call("ExitPlanMode", &json!({"plan":"Denied"}), "denied")
                .await;
            assert!(denied);
            assert!(rx.try_recv().is_err());
            manager.set_session_rules("plan-session", Default::default());
            if answer == Some("legacy") {
                let plan = crate::tools::coordination::execute("plan_mode", json!({"operation":"create","task":"修复","steps":[{"description":"Run tests"}]}), &executor.workspace_cwd).unwrap();
                executor
                    .execute_tool_call(
                        "plan_mode",
                        &json!({"operation":"enter","plan_id":plan["plan_id"]}),
                        "legacy-enter",
                    )
                    .await;
            }
            let waiters = executor.get_answer_waiters();
            let task = tokio::spawn(async move {
                if answer == Some("legacy") {
                    return executor
                        .execute_tool_call("plan_mode", &json!({"operation":"exit"}), "exit")
                        .await;
                }
                executor
                    .execute_tool_call(
                        "ExitPlanMode",
                        &json!({"plan":"# Plan\n1. 修复\n2. Run regression tests"}),
                        "exit",
                    )
                    .await
            });
            let event = timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let EngineEvent::AskUser {
                request_id,
                tool_use_id,
                tool_input: input,
                ..
            } = event
            else {
                panic!("Expected plan approval");
            };
            assert_eq!(tool_use_id, "exit");
            assert!(input["plan"].as_str().unwrap().contains("修复"));
            assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
            if answer == Some("changed") {
                plans::save(root.path(), "plan-session", "# Changed during review").unwrap();
            }
            let sender = waiters.lock().await.remove(&request_id).unwrap();
            if let Some(answer) = answer {
                sender
                    .send(
                        if matches!(answer, "changed" | "legacy") {
                            "yes"
                        } else {
                            answer
                        }
                        .into(),
                    )
                    .unwrap();
            } else {
                drop(sender);
            }
            let (_, output, failed) = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                failed,
                !matches!(answer, Some("yes" | "legacy" | "no")),
                "{output}"
            );
            assert_eq!(
                manager.get_mode(),
                if matches!(answer, Some("yes" | "legacy")) {
                    PermissionMode::AskPermissions
                } else {
                    PermissionMode::PlanMode
                }
            );
            assert!(waiters.lock().await.is_empty());
            assert!(plans::read(root.path(), "plan-session").unwrap().is_some());
        }
    }
    fn document_executor(
        root: &std::path::Path,
    ) -> (
        ToolLoopExecutor,
        mpsc::Receiver<EngineEvent>,
        Arc<crate::permissions::PermissionManager>,
    ) {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let context = crate::services::session_messages::context(root).unwrap();
        context.db.with_conn(|conn|conn.execute("INSERT INTO conversations(id,created_at,updated_at) VALUES('file-session','','')",[])).unwrap().unwrap();
        let manager = crate::runtime::default_permissions();
        manager.set_mode(PermissionMode::PlanMode);
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"plan-test","name":"plan-test","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"plan-test","name":"plan-test","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let workspace = root.join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let (tx, rx) = mpsc::channel(16);
        let executor = ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            workspace.to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_conv_id("file-session".into())
        .with_search_config_path(root.join("settings.json"))
        .with_permission_manager(manager.clone());
        (executor, rx, manager)
    }

    #[tokio::test]
    async fn dedicated_file_write_read_edit_and_exit_require_real_plan_approval() {
        for answer in [Some("yes"), Some("no"), None] {
            let root = tempfile::tempdir().unwrap();
            let (mut executor, mut rx, manager) = document_executor(root.path());
            let target = plans::path(root.path(), "file-session").unwrap();
            let (_, out, failed) = executor
                .execute_tool_call(
                    "Write",
                    &json!({"file_path":target,"content":"# File plan\nRun checks"}),
                    "write",
                )
                .await;
            assert!(!failed, "{out}");
            let (_, out, failed) = executor
                .execute_tool_call(
                    "Read",
                    &json!({"file_path":target,"offset":2,"limit":1}),
                    "read",
                )
                .await;
            assert!(!failed, "{out}");
            assert!(out.contains("Run checks"));
            for denied in [
                plans::path(root.path(), "other-session").unwrap(),
                root.path().join("project/code.rs"),
            ] {
                let (_, _, failed) = executor
                    .execute_tool_call(
                        "Write",
                        &json!({"file_path":denied,"content":"bad"}),
                        "denied",
                    )
                    .await;
                assert!(failed);
                assert!(!denied.exists());
            }
            manager.set_session_rules(
                "file-session",
                std::collections::HashMap::from([(
                    "Edit".into(),
                    crate::permissions::rules::PermissionAction::Deny,
                )]),
            );
            let edit = json!({"file_path":target,"old_string":"Run checks","new_string":"Run regression checks"});
            assert!(
                executor
                    .execute_tool_call("Edit", &edit, "denied-edit")
                    .await
                    .2
            );
            manager.set_session_rules("file-session", Default::default());
            let (_, out, failed) = executor.execute_tool_call("Edit", &edit, "edit").await;
            assert!(!failed, "{out}");
            let waiters = executor.get_answer_waiters();
            let task = tokio::spawn(async move {
                executor
                    .execute_tool_call("ExitPlanMode", &json!({}), "exit")
                    .await
            });
            let EngineEvent::AskUser {
                request_id,
                tool_input: input,
                ..
            } = timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("expected approval");
            };
            assert_eq!(input["planFilePath"], json!(target));
            assert_eq!(input["plan"], "# File plan\nRun regression checks");
            assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
            let sender = waiters.lock().await.remove(&request_id).unwrap();
            if let Some(answer) = answer {
                sender.send(answer.into()).unwrap();
            } else {
                drop(sender);
            }
            let (_, out, failed) = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failed, answer.is_none(), "{out}");
            assert_eq!(
                manager.get_mode(),
                if answer == Some("yes") {
                    PermissionMode::AskPermissions
                } else {
                    PermissionMode::PlanMode
                }
            );
            assert!(waiters.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn plan_file_explicit_ask_rule_denial_cancellation_and_mode_changes_are_respected() {
        for answer in [
            Some("yes"),
            Some("no"),
            None,
            Some("mode-change"),
            Some("deny-change"),
            Some("?"),
            Some("??"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (mut executor, mut rx, manager) = document_executor(root.path());
            let target = plans::path(root.path(), "file-session").unwrap();
            manager.set_session_rules(
                "file-session",
                std::collections::HashMap::from([(
                    "Write".into(),
                    crate::permissions::rules::PermissionAction::Confirm,
                )]),
            );
            let waiters = executor.get_answer_waiters();
            let requested = target.clone();
            let task = tokio::spawn(async move {
                executor
                    .execute_tool_call(
                        "Write",
                        &json!({"file_path":requested,"content":"# Approved write"}),
                        "write",
                    )
                    .await
            });
            let EngineEvent::ToolPermission { request_id, .. } =
                timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("expected approval");
            };
            assert!(!target.exists());
            if answer == Some("mode-change") {
                manager.set_mode(PermissionMode::AcceptEdits);
            }
            if answer == Some("deny-change") {
                manager.set_session_rules(
                    "file-session",
                    std::collections::HashMap::from([(
                        "Write".into(),
                        crate::permissions::rules::PermissionAction::Deny,
                    )]),
                );
            }
            let sender = waiters.lock().await.remove(&request_id).unwrap();
            if let Some(answer) = answer {
                sender
                    .send(
                        if answer.ends_with("change") {
                            "yes"
                        } else {
                            answer
                        }
                        .into(),
                    )
                    .unwrap();
            } else {
                drop(sender);
            }
            let (_, out, failed) = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failed, answer != Some("yes"), "{out}");
            assert_eq!(target.exists(), answer == Some("yes"));
            assert!(waiters.lock().await.is_empty());
        }
    }
    #[tokio::test]
    async fn model_streams_route_plan_file_tools_through_session_executor_on_both_protocols() {
        use axum::{http::header, routing::post, Json, Router};
        for format in ["anthropic", "openai"] {
            for answer in ["yes", "no"] {
                let root = tempfile::tempdir().unwrap();
                let target = plans::path(root.path(), "file-session").unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
                let recorded = requests.clone();
                let requested_path = target.clone();
                let app = Router::new().route(if format == "anthropic" {"/v1/messages"} else {"/v1/chat/completions"}, post(move |Json(body): Json<Value>| {
                    let recorded = recorded.clone();
                    let requested_path = requested_path.clone();
                    async move {
                        let mut recorded = recorded.lock().await;
                        let step = recorded.len();
                        recorded.push(body);
                        let (name, input) = match step {
                            0 => ("Write", json!({"file_path":requested_path,"content":"# Stream plan\nCheck code"})),
                            1 => ("Edit", json!({"file_path":requested_path,"old_string":"Check code","new_string":"Run regression tests"})),
                            2 => ("Read", json!({"file_path":requested_path})),
                            _ => ("ExitPlanMode", json!({})),
                        };
                        let events = if format == "anthropic" {
                            if step < 4 {
                                vec![json!({"type":"message_start","message":{"id":format!("m{step}"),"model":"plan-test","usage":{"input_tokens":10}}}),
                                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("t{step}"),"name":name,"input":{}}}),
                                    json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
                                    json!({"type":"content_block_stop","index":0}),
                                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),json!({"type":"message_stop"})]
                            } else {
                                vec![json!({"type":"message_start","message":{"id":"done","model":"plan-test","usage":{"input_tokens":10}}}),
                                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"finished"}}),
                                    json!({"type":"content_block_stop","index":0}),
                                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),json!({"type":"message_stop"})]
                            }
                        } else if step < 4 {
                            vec![json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":format!("t{step}"),"type":"function","function":{"name":name,"arguments":input.to_string()}}]},"finish_reason":null}]}),
                                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})]
                        } else {
                            vec![json!({"choices":[{"index":0,"delta":{"content":"finished"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})]
                        };
                        let mut stream = events.into_iter().map(|event|format!("data: {event}\n\n")).collect::<String>();
                        if format == "openai" {stream.push_str("data: [DONE]\n\n");}
                        ([(header::CONTENT_TYPE, "text/event-stream")], stream)
                    }
                }));
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let (mut executor, mut events, manager) = document_executor(root.path());
                executor.provider.provider.base_url = base;
                executor.provider.provider.api_format = if format == "anthropic" {
                    crate::native_engine::provider_manager::ApiFormat::Anthropic
                } else {
                    crate::native_engine::provider_manager::ApiFormat::OpenAI
                };
                executor.messages =
                    vec![json!({"role":"user","content":"Write and review a plan"})];
                let waiters = executor.get_answer_waiters();
                let event_reader = tokio::spawn(async move {
                    let mut results = Vec::new();
                    let mut approvals = 0;
                    while let Some(event) = events.recv().await {
                        match event {
                            EngineEvent::AskUser {
                                request_id,
                                tool_input: input,
                                ..
                            } => {
                                assert_eq!(input["plan"], "# Stream plan\nRun regression tests");
                                approvals += 1;
                                waiters
                                    .lock()
                                    .await
                                    .remove(&request_id)
                                    .unwrap()
                                    .send(answer.into())
                                    .unwrap();
                            }
                            EngineEvent::ToolUseDone {
                                tool_name,
                                output,
                                is_error,
                                ..
                            } => results.push((tool_name, output, is_error)),
                            _ => {}
                        }
                    }
                    (results, approvals)
                });
                let result = timeout(Duration::from_secs(15), executor.execute())
                    .await
                    .unwrap()
                    .unwrap();
                drop(executor);
                let (results, approvals) = event_reader.await.unwrap();
                server.abort();
                assert_eq!(result.0, "finished", "{format}/{answer}");
                assert_eq!(approvals, 1);
                assert_eq!(results.len(), 4);
                for (name, output, failed) in &results[..3] {
                    assert!(!failed, "{format} {name}: {output}");
                }
                assert!(!results[3].2);
                assert_eq!(
                    manager.get_mode(),
                    if answer == "yes" {
                        PermissionMode::AskPermissions
                    } else {
                        PermissionMode::PlanMode
                    }
                );
                assert_eq!(
                    plans::read(root.path(), "file-session").unwrap().unwrap(),
                    "# Stream plan\nRun regression tests"
                );
                let requests = requests.lock().await;
                assert_eq!(requests.len(), 5);
                assert!(requests[3]["messages"]
                    .to_string()
                    .contains("Run regression tests"));
            }
        }
    }
}
