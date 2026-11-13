use super::*;
use crate::{permissions::PermissionMode, services::plans};
use anyhow::Context;
#[cfg(test)]
#[path = "plan_approval_tests.rs"]
mod approval_tests;
#[cfg(test)]
#[path = "structured_plan_approval_tests.rs"]
mod structured_approval_tests;

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
    fn delegated_parent(&self) -> Option<String> {
        if let Some((parent, _)) = &self.delegation_parent {
            return Some(parent.clone());
        }
        let root = self.search_config_path.as_deref()?.parent()?;
        let session = self.conv_id.as_deref()?;
        // Only a live worker control channel can supply a cross-process parent.
        // Historical relationships alone must not re-enable bypass on resume.
        let directory =
            crate::services::background_sessions::worker_directory(root, session).ok()??;
        let job = crate::services::background_sessions::read_job(&directory).ok()?;
        (job.parent_session_id != session).then_some(job.parent_session_id)
    }

    fn plan_document_owner(&self, tool: &str, input: &Value) -> Option<String> {
        let root = self.search_config_path.as_deref()?.parent()?;
        let session = self.conv_id.as_deref()?;
        if plans::is_document_tool(root, session, tool, input) {
            return Some(session.to_owned());
        }
        let parent = self.delegated_parent()?;
        plans::is_document_tool(root, &parent, tool, input).then_some(parent)
    }

    pub(super) fn is_plan_document_tool(&self, tool: &str, input: &Value) -> bool {
        self.plan_document_owner(tool, input).is_some()
    }

    /// Follow parent mode changes without undoing a child's explicit Plan entry.
    pub(super) fn sync_parent_permission_mode(&self) {
        let Some(parent) = self.delegated_parent() else {
            return;
        };
        let Some(manager) = &self.permission_manager else {
            return;
        };
        let context = self
            .search_config_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .and_then(|root| crate::services::session_messages::context(root).ok());
        let mode = self
            .delegation_parent
            .as_ref()
            .map(|(_, parent)| parent.get_mode())
            .or_else(|| {
                context.as_ref().and_then(|context| {
                    crate::services::session_permission_mode::load(&context.db, &parent)
                        .ok()
                        .flatten()
                })
            });
        let Some(mode) = mode else {
            return;
        };
        let mut observed = self
            .observed_parent_mode
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *observed == Some(mode) {
            return;
        }
        if manager.get_mode() != mode {
            if let (Some(context), Some(session)) = (&context, self.conv_id.as_deref()) {
                if let Err(error) = crate::services::session_permission_mode::set(
                    &context.db,
                    session,
                    manager,
                    mode,
                ) {
                    tracing::warn!("Unable to inherit parent permission mode: {error:#}");
                    return;
                }
            } else {
                manager.set_mode(mode);
            }
        }
        *observed = Some(mode);
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
            let owner = self.plan_document_owner(tool, input).context("Plan session unavailable")?;
            let session = owner.as_str();
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
        let entering = tool_name == "EnterPlanMode";
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
            crate::services::session_permission_mode::set(
                &crate::services::session_messages::context(&root)?.db,
                &session,
                &manager,
                PermissionMode::PlanMode,
            )?;
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
        }
        let plan = plans::read(&root, &session)?
            .filter(|plan| !plan.trim().is_empty())
            .ok_or_else(|| anyhow!("No plan written yet; write the session plan file, then call ExitPlanMode (or supply plan text)"))?;
        // A Super Agent Coder's plan approval belongs to the host: it is
        // recorded by the orchestrator after CrpSubmit. Once that current
        // approval exists, ExitPlanMode resumes the inherited Router policy
        // instead of returning the plan to a delegation parent that never
        // approves it.
        if !entering {
            if let (Some(runtime), Some(session_id)) =
                (self.super_agent.as_ref(), self.conv_id.as_deref())
            {
                if let Ok(Some((task_id, crate::super_agent::MemberRole::Coder))) =
                    crate::super_agent::role::membership(&runtime.db, session_id)
                {
                    let store = runtime.store();
                    if let Ok(Some(task)) = store.get_task(&task_id) {
                        let approved = store.approvals(&task_id).is_ok_and(|approvals| {
                            approvals.iter().any(|approval| {
                                approval.request_type
                                    == crate::super_agent::ApprovalType::Plan
                                    && approval.status
                                        == crate::super_agent::ApprovalStatus::Approved
                                    && approval.task_spec_version == task.spec_version
                                    && approval.permission_policy_version
                                        == task.permission_policy_version
                            })
                        });
                        anyhow::ensure!(
                            approved,
                            "The host has not approved the current task plan; submit it with CrpSubmit(kind=\"plan\") and wait"
                        );
                        let context = crate::services::session_messages::context(&root)?;
                        let inherited = crate::services::session_permission_mode::load(
                            &context.db,
                            &task.router_session_id,
                        )?
                        .unwrap_or(PermissionMode::AcceptEdits);
                        let expected_mode = manager.mode_snapshot();
                        anyhow::ensure!(
                            expected_mode.mode == PermissionMode::PlanMode,
                            "Planning mode changed before approval"
                        );
                        crate::services::plan_approval::commit_approved_mode(
                            &context,
                            &session,
                            &manager,
                            expected_mode,
                            inherited,
                            &plan,
                        )?;
                        return Ok(json!({
                            "approved":true,
                            "mode":crate::services::session_permission_mode::name(inherited),
                            "approvedBy":"super-agent-orchestrator",
                            "planFilePath":path,
                            "plan":plan,
                            "allowedPrompts":allowed_prompts,
                            "message":"Plan approved by the Super Agent host. Implement the approved plan."
                        }));
                    }
                }
            }
        }
        if self.delegated_parent().is_some()
            && crate::services::teams::team_for_session(&root, &session)?.is_none()
        {
            return Ok(
                json!({"approved":false,"mode":"plan","submitted":true,"plan":plan,"planFilePath":path,"message":"Plan review complete. Return the plan and findings to the parent agent; only the parent requests implementation approval."}),
            );
        }
        let expected_mode = manager.mode_snapshot();
        anyhow::ensure!(
            expected_mode.mode == PermissionMode::PlanMode,
            "Planning mode changed before approval"
        );
        if let Some(team) = crate::services::teams::team_for_session(&root, &session)? {
            if team.lead_session_id != session {
                use crate::services::teams::{self, PlanDecision};
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
        let context = crate::services::session_messages::context(&root)?;
        let bypass = crate::services::session_permission_mode::plan_returns_to_bypass(
            &context.db,
            &session,
            &manager,
        )?;
        let options = crate::services::plan_approval::options(bypass);
        let request_id = uuid::Uuid::new_v4().to_string();
        let approval_request = crate::services::plan_approval::ApprovalRequest::new(
            &session,
            &request_id,
            id,
            &plan,
            &path,
            manager.bypass_available(),
        );
        let (tx, rx) = oneshot::channel();
        self.answer_waiters
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let _waiter = PermissionWaiterGuard {
            waiters: self.answer_waiters.clone(),
            request_id: request_id.clone(),
        };
        crate::native_engine::permission_response::register_owner(
            &request_id,
            &session,
            id,
            "plan",
            serde_json::to_value(&approval_request)?,
            Some(approval_request.clone()),
            Some(manager.clone()),
            Some(root.clone()),
        );
        let question = format!("Review plan\n{}\n\n{plan}\n\nChoose 1–4, or feedback: <changes>. Clear context preserves this session and its archived transcript.", path.display());
        let sent = self
            .event_tx
            .send(EngineEvent::AskUser {
                request_id: request_id.clone(),
                tool_use_id: id.into(),
                tool_input: json!({"plan":plan,"planFilePath":path,"planApproval":true,"planApprovalRequest":approval_request,"returnToBypass":bypass,"question":question,"options":options.iter().map(|label| json!({"label":label})).collect::<Vec<_>>(),"multiSelect":false}),
                question,
                options: options.iter().map(|s|s.to_string()).collect(),
            })
            .await;
        if sent.is_err() {
            self.answer_waiters.lock().await.remove(&request_id);
            anyhow::bail!("Approval host disconnected; planning mode retained");
        }
        let result = async {
        let answer = self.wait_for_user_answer(rx).await;
        self.answer_waiters.lock().await.remove(&request_id);
        let answer = answer.context("Plan approval cancelled; planning mode retained")??;
        use crate::services::plan_approval::{self, Choice};
        let choice = plan_approval::parse_response(&answer, &approval_request, bypass, manager.bypass_available())?;
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
        let returning_bypass = matches!(choice, Choice::ClearBypass | Choice::KeepBypass);
        anyhow::ensure!(
            !returning_bypass
                || manager.bypass_available(),
            "Bypass launch authority changed during review"
        );
        let mode = if returning_bypass {
            PermissionMode::BypassPermissions
        } else if matches!(choice, Choice::Manual | Choice::ClearManual) {
            PermissionMode::AskPermissions
        } else if matches!(choice, Choice::KeepAuto | Choice::ClearAuto) {
            PermissionMode::Auto
        } else {
            PermissionMode::AcceptEdits
        };
        let clear = matches!(choice, Choice::ClearEdits | Choice::ClearBypass | Choice::ClearManual | Choice::ClearAuto);
        let context = crate::services::session_messages::context(&root)?;
        if clear {
            let message = format!("Implement the following approved plan:\n\n{plan}\n\nPrior conversation context has been cleared. Its transcript remains archived locally in session {session}. Execute under the approved {mode_name} permission mode.", mode_name = crate::services::session_permission_mode::name(mode));
            let boundary = plan_approval::commit_approved_boundary_mode(
                &context,
                &session,
                &message,
                &manager,
                expected_mode,
                mode,
            )?;
            self.pending_plan_reset = Some((boundary, message));
        }
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
        if returning_bypass {
            crate::permissions::set_full_access(manager.full_access_available());
        }
        Ok(
            json!({"approved":true,"mode":if returning_bypass {"bypass"} else if mode == PermissionMode::AskPermissions {"ask"} else if mode == PermissionMode::Auto {"auto"} else {"accept_edits"},"clearContext":clear,"planFilePath":path,"plan":plan,"allowedPrompts":allowed_prompts,
            "message":if returning_bypass {"Plan approved. bypass mode restored."} else {"Plan approved under the selected permission mode. Existing deny and confirmation rules remain in force."}}),
        )
        }.await;
        crate::native_engine::permission_response::finish_answer(
            &request_id,
            result
                .as_ref()
                .cloned()
                .map_err(|error: &anyhow::Error| format!("{error:#}")),
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enter_plan_confirmation_and_child_authority_are_isolated() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        for name in ["EnterPlanMode"] {
            for answer in [Some("yes"), Some("no"), None] {
                let root = tempfile::tempdir().unwrap();
                let parent = crate::runtime::default_permissions();
                parent.set_mode(PermissionMode::AskPermissions);
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
                let input = json!({});
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
                assert_eq!(child.get_mode(), PermissionMode::AskPermissions);
                let sender = waiters.lock().await.remove(&request_id).unwrap();
                if let Some(answer) = answer {
                    sender.send(answer.into()).unwrap();
                } else {
                    drop(sender);
                }
                let (_, output, failed) = task.await.unwrap();
                assert_eq!(failed, answer != Some("yes"), "{output}");
                assert_eq!(parent.get_mode(), PermissionMode::AskPermissions);
                assert_eq!(
                    child.get_mode(),
                    if failed {
                        PermissionMode::AskPermissions
                    } else {
                        PermissionMode::PlanMode
                    }
                );
                child.set_mode(PermissionMode::AskPermissions);
                let context = crate::permissions::PermissionContext {
                    mcp_read_only: false,
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
            let waiters = executor.get_answer_waiters();
            let task = tokio::spawn(async move {
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
                    .send(if answer == "changed" { "yes" } else { answer }.into())
                    .unwrap();
            } else {
                drop(sender);
            }
            let (_, output, failed) = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failed, !matches!(answer, Some("yes" | "no")), "{output}");
            assert_eq!(
                manager.get_mode(),
                if matches!(answer, Some("yes")) {
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
    async fn delegated_plan_review_and_parent_document_need_no_approval() {
        let root = tempfile::tempdir().unwrap();
        let (mut executor, _events, manager) = document_executor(root.path());
        let parent = crate::runtime::default_permissions();
        parent.set_mode(PermissionMode::PlanMode);
        executor.delegation_parent = Some(("parent-session".into(), parent.clone()));
        let target = plans::path(root.path(), "parent-session").unwrap();
        let input = json!({"file_path":target,"content":"# Parent plan"});
        assert!(executor.is_plan_document_tool("Write", &input));
        let (_, output, failed) = executor
            .execute_plan_document("Write", &input, "write")
            .await;
        assert!(!failed, "{output}");
        assert_eq!(
            plans::read(root.path(), "parent-session")
                .unwrap()
                .as_deref(),
            Some("# Parent plan")
        );
        plans::save(root.path(), "file-session", "# Review findings").unwrap();
        let result = executor
            .plan_transition("ExitPlanMode", &json!({}), "review")
            .await
            .unwrap();
        assert_eq!(result["submitted"], true);
        assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
        let context = crate::services::session_messages::context(root.path()).unwrap();
        parent.set_mode(PermissionMode::BypassPermissions);
        executor.sync_parent_permission_mode();
        assert_eq!(manager.get_mode(), PermissionMode::BypassPermissions);
        assert_eq!(
            crate::services::session_permission_mode::load(&context.db, "file-session").unwrap(),
            Some(PermissionMode::BypassPermissions)
        );
        manager.set_mode(PermissionMode::PlanMode);
        executor.sync_parent_permission_mode();
        assert_eq!(
            manager.get_mode(),
            PermissionMode::PlanMode,
            "An unchanged parent must not undo child Plan entry"
        );
        parent.set_mode(PermissionMode::AskPermissions);
        executor.sync_parent_permission_mode();
        assert_eq!(manager.get_mode(), PermissionMode::AskPermissions);
    }

    #[tokio::test]
    async fn pending_child_approval_follows_bypass_and_preserves_deny() {
        for denied in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (mut executor, mut events, manager) = document_executor(root.path());
            let parent = crate::runtime::default_permissions();
            parent.set_mode(PermissionMode::AskPermissions);
            manager.set_mode(PermissionMode::AskPermissions);
            executor.delegation_parent = Some(("parent-session".into(), parent.clone()));
            let destination = root.path().join("project").join("approved.txt");
            let input = json!({"file_path":destination,"content":"approved"});
            let task = tokio::spawn(async move {
                executor.execute_tool_call("Write", &input, "waiting").await
            });
            assert!(matches!(
                timeout(Duration::from_secs(3), events.recv())
                    .await
                    .unwrap(),
                Some(EngineEvent::ToolPermission { .. })
            ));
            if denied {
                manager.set_session_rules(
                    "file-session",
                    std::collections::HashMap::from([(
                        "Write".into(),
                        crate::permissions::rules::PermissionAction::Deny,
                    )]),
                );
            }
            parent.set_mode(PermissionMode::BypassPermissions);
            let (_, output, failed) = timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failed, denied, "{output}");
            assert_eq!(destination.exists(), !denied);
        }
    }

    #[test]
    fn historical_parent_record_does_not_restore_bypass() {
        let root = tempfile::tempdir().unwrap();
        let (executor, _events, manager) = document_executor(root.path());
        let context = crate::services::session_messages::context(root.path()).unwrap();
        context
            .save_setting(
                "delegation.file-session",
                &json!({"parent_session_id":"old-parent"}),
            )
            .unwrap();
        context.db.with_conn(|conn| conn.execute("INSERT INTO peripheral_settings(key,value) VALUES('cli.permission-mode.old-parent','bypass')", [])).unwrap().unwrap();
        executor.sync_parent_permission_mode();
        assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
        assert!(executor.delegated_parent().is_none());
    }

    #[tokio::test]
    async fn background_worker_permissions_follow_live_parent() {
        use crate::services::{background_sessions, session_permission_mode};
        for denied in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let context = crate::services::session_messages::context(root.path()).unwrap();
            let parent = crate::runtime::default_permissions();
            session_permission_mode::set(
                &context.db,
                "parent-session",
                &parent,
                PermissionMode::PlanMode,
            )
            .unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let directory = background_sessions::directory(root.path(), &id).unwrap();
            std::fs::create_dir_all(&directory).unwrap();
            let job = background_sessions::Job {
                id,
                foreground: false,
                parent_session_id: "parent-session".into(),
                session_id: "file-session".into(),
                title: "Permission fixture".into(),
                status: "running".into(),
                supervisor_pid: None,
                child_pid: None,
                started_ms: 0,
                heartbeat_ms: 0,
                finished_ms: None,
                exit_code: None,
                error: None,
                launched: true,
            };
            background_sessions::save(&directory.join("state.json"), &job).unwrap();
            let log = root.path().join("worker.log");
            let mut worker = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["native_engine::tool_loop::plan_tool::tests::background_worker_permission_fixture", "--exact", "--ignored", "--nocapture"])
                .env(background_sessions::CONTROL_ENV, &directory)
                .env("DCL_PERMISSION_TEST_ROOT", root.path())
                .env("DCL_PERMISSION_TEST_DENIED", denied.to_string())
                .stdout(std::fs::File::create(&log).unwrap())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn().unwrap();
            for (signal, mode) in [
                ("plan-reviewed", PermissionMode::AskPermissions),
                ("permission-pending", PermissionMode::BypassPermissions),
                ("permission-finished", PermissionMode::AskPermissions),
            ] {
                timeout(Duration::from_secs(15), async {
                    while !root.path().join(signal).exists() {
                        assert!(
                            worker.try_wait().unwrap().is_none(),
                            "{}",
                            std::fs::read_to_string(&log).unwrap()
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "Waiting for {signal}: {}",
                        std::fs::read_to_string(&log).unwrap()
                    )
                });
                session_permission_mode::set(&context.db, "parent-session", &parent, mode).unwrap();
            }
            let status = timeout(Duration::from_secs(15), worker.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(
                status.success(),
                "{}",
                std::fs::read_to_string(&log).unwrap()
            );
            assert_eq!(root.path().join("project/approved.txt").exists(), !denied);
            assert_eq!(
                plans::read(root.path(), "parent-session")
                    .unwrap()
                    .as_deref(),
                Some("# Revised parent plan")
            );
            assert_eq!(
                session_permission_mode::load(&context.db, "file-session").unwrap(),
                Some(PermissionMode::AskPermissions)
            );
        }
    }

    #[tokio::test]
    #[ignore = "isolated subprocess fixture for background worker permissions"]
    async fn background_worker_permission_fixture() {
        let root = std::path::PathBuf::from(std::env::var_os("DCL_PERMISSION_TEST_ROOT").unwrap());
        let (mut executor, mut events, manager) = document_executor(&root);
        let plan = plans::path(&root, "parent-session").unwrap();
        for (tool, input) in [
            ("Write", json!({"file_path":plan,"content":"# Parent plan"})),
            ("Read", json!({"file_path":plan})),
            (
                "Edit",
                json!({"file_path":plan,"old_string":"# Parent plan","new_string":"# Revised parent plan"}),
            ),
            ("ExitPlanMode", json!({"plan":"# Review complete"})),
        ] {
            let (_, output, failed) = executor.execute_tool_call(tool, &input, tool).await;
            assert!(!failed, "{output}");
            if tool == "ExitPlanMode" {
                assert_eq!(
                    serde_json::from_str::<Value>(&output).unwrap()["submitted"],
                    true
                );
            }
        }
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event,
                EngineEvent::ToolPermission { .. } | EngineEvent::AskUser { .. }
            ));
        }
        std::fs::write(root.join("plan-reviewed"), "ready").unwrap();
        timeout(Duration::from_secs(10), async {
            while manager.get_mode() != PermissionMode::AskPermissions {
                executor.sync_parent_permission_mode();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let input = json!({"file_path":root.join("project/approved.txt"),"content":"approved"});
        let task = tokio::spawn(async move {
            let result = executor
                .execute_tool_call("Write", &input, "pending-write")
                .await;
            (executor, result)
        });
        assert!(matches!(
            timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap(),
            Some(EngineEvent::ToolPermission { .. })
        ));
        let denied = std::env::var("DCL_PERMISSION_TEST_DENIED").unwrap() == "true";
        if denied {
            manager.set_session_rules(
                "file-session",
                std::collections::HashMap::from([(
                    "Write".into(),
                    crate::permissions::rules::PermissionAction::Deny,
                )]),
            );
        }
        std::fs::write(root.join("permission-pending"), "ready").unwrap();
        let (executor, (_, output, failed)) = timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed, denied, "{output}");
        assert_eq!(manager.get_mode(), PermissionMode::BypassPermissions);
        std::fs::write(root.join("permission-finished"), "ready").unwrap();
        timeout(Duration::from_secs(10), async {
            while manager.get_mode() != PermissionMode::AskPermissions {
                executor.sync_parent_permission_mode();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn missing_stream_arguments_do_not_execute_default_directory_tools() {
        let root = tempfile::tempdir().unwrap();
        let (mut executor, _events, _) = document_executor(root.path());
        assert_eq!(
            executor.finalize_streaming_tool_args("missing"),
            Value::Null
        );
        executor.handle_streaming_tool_arg_delta("explicit", "ListDir", "{}");
        assert_eq!(executor.finalize_streaming_tool_args("explicit"), json!({}));
        executor.handle_streaming_tool_arg_delta("blank", "ListDir", "{}");
        executor.handle_streaming_tool_arg_delta("blank", "ListDir", "");
        assert_eq!(executor.finalize_streaming_tool_args("blank"), Value::Null);
        executor.handle_streaming_tool_arg_delta("read", "Read", "{}");
        executor.handle_streaming_tool_arg_delta("read", "Read", r#"{"file_path":"a"}"#);
        assert_eq!(
            executor.finalize_streaming_tool_args("read")["file_path"],
            "a"
        );
        let prepared = executor
            .prepare_tool_call("ListDir", &Value::Null, "missing")
            .await;
        match prepared.step {
            CallStep::Finished((_, message, failed)) => {
                assert!(failed);
                assert!(message.contains("Tool was not executed"));
            }
            _ => panic!("Invalid arguments were executed"),
        }
    }

    #[tokio::test]
    async fn dedicated_plan_path_survives_mode_changes_and_bypass_still_obeys_denies() {
        let root = tempfile::tempdir().unwrap();
        let (mut executor, _events, manager) = document_executor(root.path());
        let target = plans::path(root.path(), "file-session").unwrap();
        let spelling = target
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .replace('\\', "/");
        let write = json!({"file_path":spelling,"content":"# Plan survives a mode change"});
        for mode in [PermissionMode::PlanMode, PermissionMode::BypassPermissions] {
            manager.set_mode(mode);
            executor
                .validate_approved_input("Write", &write)
                .await
                .unwrap();
            let (_, output, failed) = executor
                .execute_tool_call("Write", &write, "plan-write")
                .await;
            assert!(!failed, "{output}");
        }
        manager.set_mode(PermissionMode::Auto);
        assert!(executor.is_plan_document_tool("Write", &write));
        executor
            .validate_approved_input("Write", &write)
            .await
            .unwrap();
        manager.set_mode(PermissionMode::BypassPermissions);
        let outside = tempfile::tempdir().unwrap();
        let input = json!({"file_path":outside.path().join("approved.txt"),"content":"allowed"});
        executor
            .validate_approved_input("Write", &input)
            .await
            .unwrap();
        manager.set_session_rules(
            "file-session",
            std::collections::HashMap::from([(
                "Write".into(),
                crate::permissions::rules::PermissionAction::Deny,
            )]),
        );
        assert!(executor
            .validate_approved_input("Write", &input)
            .await
            .is_err());
        assert!(executor
            .validate_approved_input("Write", &write)
            .await
            .is_err());
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
    async fn empty_and_initial_tool_arguments_are_handled_on_both_protocols() {
        use axum::{routing::post, Json, Router};
        for format in ["anthropic", "openai"] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("project").join("source.txt");
            let (mut executor, mut events, _) = document_executor(root.path());
            std::fs::write(&source, "SOURCE_SENTINEL").unwrap();
            let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
            let captured = requests.clone();
            let app = Router::new().route(if format == "anthropic" { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
                let captured = captured.clone();
                let source = source.clone();
                async move {
                    let mut requests = captured.lock().await;
                    let first = requests.is_empty();
                    requests.push(body);
                    let calls = [("empty-dir", "ListDir", "".to_owned()), ("empty-read", "Read", "".to_owned()), ("valid-read", "Read", json!({"file_path":source}).to_string())];
                    let mut events = Vec::new();
                    if format == "anthropic" {
                        events.push(json!({"type":"message_start","message":{"id":"fixture","model":"fixture","usage":{"input_tokens":1}}}));
                        if first {
                            for (index, (id, name, args)) in calls.iter().enumerate() {
                                let initial = if args.is_empty() { json!({}) } else { serde_json::from_str::<Value>(args).unwrap() };
                                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":initial}}));
                                if args.is_empty() { events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":""}})); }
                                events.push(json!({"type":"content_block_stop","index":index}));
                            }
                        } else {
                            events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"RECOVERED"}}));
                            events.push(json!({"type":"content_block_stop","index":0}));
                        }
                        events.push(json!({"type":"message_delta","delta":{"stop_reason":if first {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}));
                        events.push(json!({"type":"message_stop"}));
                    } else {
                        let delta = if first { json!({"tool_calls":calls.iter().enumerate().map(|(index,(id,name,args))|json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args}})).collect::<Vec<_>>()}) } else { json!({"content":"RECOVERED"}) };
                        events.push(json!({"choices":[{"index":0,"delta":delta,"finish_reason":if first {"tool_calls"} else {"stop"}}]}));
                    }
                    let mut stream = events.iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                    if format == "openai" { stream.push_str("data: [DONE]\n\n"); }
                    ([("content-type", "text/event-stream")], stream)
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            executor.provider.provider.base_url =
                format!("http://{}", listener.local_addr().unwrap());
            executor.provider.provider.api_format = if format == "anthropic" {
                crate::native_engine::provider_manager::ApiFormat::Anthropic
            } else {
                crate::native_engine::provider_manager::ApiFormat::OpenAI
            };
            executor.messages = vec![json!({"role":"user","content":"Inspect the file"})];
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let reader = tokio::spawn(async move {
                let mut results = Vec::new();
                while let Some(event) = events.recv().await {
                    if let EngineEvent::ToolUseDone {
                        output, is_error, ..
                    } = event
                    {
                        results.push((output, is_error));
                    }
                }
                results
            });
            let result = timeout(Duration::from_secs(10), executor.execute())
                .await
                .unwrap()
                .unwrap();
            drop(executor);
            let results = reader.await.unwrap();
            assert_eq!(result.0, "RECOVERED");
            assert_eq!(results.len(), 3);
            assert!(
                results[..2]
                    .iter()
                    .all(|(output, failed)| *failed && output.contains("Tool was not executed")),
                "{format}: {results:?}"
            );
            assert!(
                !results[2].1 && results[2].0.contains("SOURCE_SENTINEL"),
                "{format}: {results:?}"
            );
            let requests = requests.lock().await;
            assert_eq!(requests.len(), 2);
            if format == "anthropic" {
                for message in requests[1]["messages"].as_array().unwrap() {
                    for block in message["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_use" {
                            assert!(
                                block["input"].is_object(),
                                "Rejected calls still need valid API history: {block}"
                            );
                        }
                    }
                }
            }
            server.abort();
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
