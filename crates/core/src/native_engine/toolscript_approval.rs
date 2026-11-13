//! Cross-process transport only: the owning executor uses ordinary approval,
//! including its timeout, input validation and policy commits. Files are scoped
//! to one job and UUID request; the host lease and request guard bound liveness.
use super::*;
use crate::native_engine::{permission_response::ToolScriptOrigin, toolscript_job};
use crate::toolscript::Cancellation;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(super) struct WorkerApproval {
    directory: PathBuf,
    token: Cancellation,
    policy_error: Arc<std::sync::Mutex<Option<String>>>,
}

pub(crate) struct HostLease(std::fs::File);
impl HostLease {
    pub(crate) fn acquire(directory: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("approval-host.lock"))?;
        crate::fs::try_lock_file(&file)?;
        Ok(Self(file))
    }
}
impl Drop for HostLease {
    fn drop(&mut self) {
        let _ = crate::fs::unlock_file(&self.0);
    }
}
fn host_live(directory: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join("approval-host.lock"))
    else {
        return false;
    };
    match crate::fs::try_lock_file(&file) {
        Ok(()) => {
            let _ = crate::fs::unlock_file(&file);
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(_) => false,
    }
}

pub(super) fn has_pending_request(directory: &Path) -> bool {
    std::fs::read_dir(directory.join("approvals")).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".request.json"))
        })
    })
}

pub(crate) struct HostTask(tokio::task::JoinHandle<()>);
impl Drop for HostTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    session: String,
    parent_tool_use_id: String,
    tool_use_id: String,
    tool: String,
    input: Value,
    allow_host_rule: bool,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    input: Option<Value>,
    error: Option<String>,
    edits_session: bool,
    bash_session: bool,
}
struct RequestFiles {
    request: PathBuf,
    response: PathBuf,
}
impl Drop for RequestFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.request);
        let _ = std::fs::remove_file(&self.response);
    }
}

impl ToolLoopExecutor {
    pub(crate) fn with_toolscript_approval(
        mut self,
        directory: &Path,
        interactive: bool,
        token: Cancellation,
        policy_error: Arc<std::sync::Mutex<Option<String>>>,
    ) -> Self {
        self.permission_host_available = interactive;
        if interactive {
            self.toolscript_approval = Some(WorkerApproval {
                directory: directory.to_path_buf(),
                token,
                policy_error,
            });
        }
        self
    }

    pub(super) fn start_toolscript_approval_host(
        &mut self,
        directory: &Path,
        parent: &str,
        lease: HostLease,
    ) {
        // This executor is only an approval context; it never dispatches a
        // tool body. Shared mailboxes deliver the usual host UI.
        let mut host = ToolLoopExecutor::new(
            self.provider.clone(),
            Vec::new(),
            None,
            self.max_tokens,
            self.event_tx.clone(),
            self.workspace_cwd.clone(),
            self.reasoning_mode.clone(),
        );
        host.conv_id = self.conv_id.clone();
        host.permission_manager = self.permission_manager.clone();
        host.approval_notes = self.approval_notes.clone();
        host.permission_interrupted = self.permission_interrupted.clone();
        host.permission_mailbox = self.permission_mailbox.clone();
        host.answer_waiters = self.answer_waiters.clone();
        host.hooks = self.hooks.clone();
        host.search_config_path = self.search_config_path.clone();
        host.allowed_directories = self.allowed_directories.clone();
        host.allowed_tools = self.allowed_tools.clone();
        host.mcp_registry = self.mcp_registry.clone();
        host.mcp_read_only_tools = self.mcp_read_only_tools.clone();
        host.super_agent_role = self.super_agent_role;
        host.super_agent_member = self.super_agent_member;
        host.super_agent = self.super_agent.clone();
        host.durable_user_input = self.durable_user_input;
        host.approval_origin = Some(ToolScriptOrigin {
            parent_tool_use_id: parent.into(),
            task_id: directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        });
        let directory = directory.to_path_buf();
        self.toolscript_hosts.retain(|task| !task.0.is_finished());
        self.toolscript_hosts.push(HostTask(tokio::spawn(async move {
            let _lease = lease;
            loop {
                if toolscript_job::state(&directory).map_or(true, |state| state.is_terminal()) { break; }
                if let Ok(entries) = std::fs::read_dir(directory.join("approvals")) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let Some(stem) = path.file_name().and_then(|s| s.to_str()).and_then(|s| s.strip_suffix(".request.json")) else { continue; };
                        if uuid::Uuid::parse_str(stem).is_err() { continue; }
                        let response = path.with_file_name(format!("{stem}.response.json"));
                        if response.exists() { continue; }
                        let Ok(request) = std::fs::read(&path).map_err(anyhow::Error::from)
                            .and_then(|bytes| Ok(serde_json::from_slice::<Request>(&bytes)?)) else { continue; };
                        if Some(&request.session) != host.conv_id.as_ref()
                            || request.parent_tool_use_id != host.approval_origin.as_ref().unwrap().parent_tool_use_id { continue; }
                        let approval = async {
                            if let Some(reason) = host.permission_hard_denial(&request.tool, &request.input) { anyhow::bail!("{reason}"); }
                            // SDK session patches live in the owning process.
                            // They can resolve a default prompt, but cannot
                            // replace an explicit ask or a PreToolUse ask.
                            if request.allow_host_rule && matches!(host.check_permission_with(&request.tool, &request.input, &request.tool_use_id).await, PermissionResult::Granted) {
                                host.validate_approved_input(&request.tool, &request.input).await?;
                                Ok(request.input.clone())
                            } else {
                                host.request_tool_approval_with(&request.tool, &request.input, &request.tool_use_id).await
                            }
                        };
                        let gone = async {
                            loop {
                                if !path.exists() || directory.join("cancel").exists()
                                    || toolscript_job::state(&directory).map_or(true, |s| s.is_terminal()) { break; }
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                        };
                        let result = tokio::select! { biased; _ = gone => continue, result = approval => result };
                        if host.permission_interrupted.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                            let _ = std::fs::write(directory.join("cancel"), b"approval interrupted the turn");
                            continue;
                        }
                        if !path.exists() || directory.join("cancel").exists() { continue; }
                        let (input, error) = match result { Ok(input) => (Some(input), None), Err(error) => (None, Some(format!("{error:#}"))) };
                        let session = request.session.as_str();
                        let reply = Reply {
                            input, error,
                            edits_session: host.permission_manager.as_ref().is_some_and(|m| m.edit_session_allowed(session)),
                            bash_session: host.permission_manager.as_ref().is_some_and(|m| m.bash_session_allowed(session)),
                        };
                        if toolscript_job::save_atomic(&response, &reply).is_err() {
                            // A transport failure must release the lease; the
                            // worker cancels rather than waiting for a lost host.
                            return;
                        }
                        if !path.exists() { let _ = std::fs::remove_file(&response); }
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })));
    }

    pub(super) async fn request_toolscript_approval(
        &self,
        bridge: &WorkerApproval,
        tool: &str,
        input: &Value,
        id: &str,
        hook_asks: bool,
    ) -> Result<Value> {
        let allow_host_rule = !hook_asks
            && self.permission_manager.as_ref().is_some_and(|manager| {
                let decision =
                    manager.evaluate_permission(&crate::permissions::PermissionContext {
                        mcp_read_only: self.mcp_read_only_tools.contains(tool),
                        tool_name: tool.into(),
                        tool_input: input.clone(),
                        conversation_id: self.conv_id.clone().unwrap_or_default(),
                        user_id: None,
                        workspace_path: Some(self.workspace_cwd.clone()),
                    });
                decision.origin == crate::permissions::DecisionOrigin::DefaultPrompt
                    && matches!(decision.result, PermissionResult::RequiresConfirmation(_))
            });
        let directory = bridge.directory.as_path();
        let token = &bridge.token;
        let state = toolscript_job::state(directory)?;
        let approvals = directory.join("approvals");
        std::fs::create_dir_all(&approvals)?;
        let key = uuid::Uuid::new_v4();
        let files = RequestFiles {
            request: approvals.join(format!("{key}.request.json")),
            response: approvals.join(format!("{key}.response.json")),
        };
        toolscript_job::save_atomic(
            &files.request,
            &Request {
                session: state.session,
                parent_tool_use_id: state.parent_tool_use_id,
                tool_use_id: id.into(),
                tool: tool.into(),
                input: input.clone(),
                allow_host_rule,
            },
        )?;
        loop {
            // Check cancellation/liveness before accepting even a ready reply.
            if token.is_cancelled() || directory.join("cancel").exists() || !host_live(directory) {
                token.cancel();
                anyhow::bail!(
                    "ToolScript approval cancelled: owning session closed or script stopped"
                );
            }
            if files.response.exists() {
                let reply: Reply = serde_json::from_slice(&std::fs::read(&files.response)?)?;
                if let Some(error) = reply.error {
                    anyhow::bail!("{error}");
                }
                let actual = reply
                    .input
                    .ok_or_else(|| anyhow!("Missing ToolScript approval input"))?;
                let manager = self
                    .permission_manager
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing ToolScript permission manager"))?;
                // Re-read durable policy after a host commit. It may tighten
                // while the request waits, and must beat any one-off approval.
                if let Err(error) = toolscript_job::refresh_approval_policy(directory, manager) {
                    toolscript_job::record_policy_failure(
                        &bridge.policy_error,
                        token,
                        &directory.join("output.log"),
                        &error,
                    );
                    return Err(error);
                }
                let session = self.conv_id.as_deref().unwrap_or_default();
                manager.set_edit_session_allowed(session, reply.edits_session);
                manager.set_bash_session_allowed(session, reply.bash_session);
                self.validate_approved_input(tool, &actual).await?;
                return Ok(actual);
            }
            tokio::select! {
                _ = token.cancelled() => anyhow::bail!("ToolScript approval cancelled"),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolscript_host_lease_tracks_liveness_and_releases_explicitly() {
        let root = tempfile::tempdir().unwrap();
        assert!(!host_live(root.path()));
        let lease = HostLease::acquire(root.path()).unwrap();
        assert!(host_live(root.path()));
        assert!(HostLease::acquire(root.path()).is_err());
        // A fork/retained descriptor must not keep our approval authority alive.
        let retained = lease.0.try_clone().unwrap();
        drop(lease);
        assert!(!host_live(root.path()));
        drop(retained);
    }

    #[test]
    fn toolscript_request_cleanup_invalidates_only_its_own_files() {
        let root = tempfile::tempdir().unwrap();
        let files = RequestFiles {
            request: root.path().join("one.request.json"),
            response: root.path().join("one.response.json"),
        };
        let sibling = root.path().join("two.request.json");
        for path in [&files.request, &files.response, &sibling] {
            std::fs::write(path, "{}").unwrap();
        }
        let request = files.request.clone();
        let response = files.response.clone();
        drop(files);
        assert!(!request.exists());
        assert!(!response.exists());
        assert!(sibling.exists());
    }
}
