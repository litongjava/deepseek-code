//! Child tasks reuse ToolLoopExecutor inline: dropping a parent drops every child future.
#[path = "child_transcript.rs"]
mod child_transcript;
use super::{EngineEvent, ToolLoopExecutor};
use crate::{
    db::session_store::ExecutorHistory,
    runtime::{RuntimePaths, SessionStore},
    services::{agents, EnvironmentCredentials, ServiceContext},
    tools::{background_shell::SessionStop, ToolDefinition},
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

pub(crate) type OwnedChildren = Arc<Mutex<HashMap<String, OwnedChild>>>;
pub(crate) struct OwnedChild {
    executor: Box<ToolLoopExecutor>,
    owner_provider: crate::native_engine::provider_manager::ResolvedProvider,
    pub(in crate::native_engine) parent: String,
    pub(in crate::native_engine) owner_budget: Arc<std::sync::atomic::AtomicUsize>,
    pub(in crate::native_engine) idle_ready: bool,
    policy: Value,
    binding: Value,
    retained: Instant,
}

impl OwnedChild {
    pub(in crate::native_engine) fn retained_session(&self) -> Option<crate::db::session_store::SessionDb> {
        match self.executor.executor_history.as_ref()? {
            crate::db::session_store::ExecutorHistory::Session(shard) => Some(shard.clone()),
            _ => None,
        }
    }

    pub(in crate::native_engine) fn binding_is_current(
        &self,
        manager: &crate::native_engine::provider_manager::ProviderManager,
    ) -> bool {
        // Session bindings are per owner. A retained child owns its launch
        // snapshot. A shared provider catalog can still revoke the child itself.
        let held = &self.executor.provider;
        manager.binding.read().is_some() || manager
            .resolve_selected(&held.provider.id, &held.model.id)
            .is_some_and(|live| held.same_binding(&live) && held.model.api == live.model.api)
    }

    pub(in crate::native_engine) fn permission_target(
        &self,
    ) -> Result<(
        Arc<crate::permissions::PermissionManager>,
        crate::native_engine::provider_manager::ResolvedProvider,
    )> {
        Ok((
            self.executor
                .permission_manager
                .clone()
                .context("Owned child permissions unavailable")?,
            self.executor.provider.clone(),
        ))
    }
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        summary: Some("Start a child agent on one task and return its result".into()),
        name: "Agent".into(),
        description: "Start a child agent on one task. Pick a subagent_type from the runtime list; leaving it out starts a general-purpose agent, whose prompt and tools come from its definition.\n\nA fork branches this session instead: it keeps the full history, runs in the background on this session's model and takes prompt as its directive, and it cannot start another fork.\n\nMain-session children run in the background by default and send a completion notification, so keep working rather than polling; pass run_in_background: false to get the result before you continue. isolation: \"worktree\" puts the child in its own git worktree. The user does not see a child's report, so relay what matters; SendMessage continues the same child with its context, while a new Agent call starts fresh.".into(),
        input_schema: json!({"type":"object","properties":{
            "description":{"description":"Short label (about 3-5 words) for the task.","type":"string"},
            "prompt":{"description":"What the child agent should do.","type":"string"},
            "subagent_type":{"description":"Agent definition to use; choose from the runtime list, or \"fork\" to branch this session.","type":"string"},
            "model":{"description":"Optional model override for this agent: a model ID configured for the current provider. If omitted, the agent uses the parent's model.","type":"string"},
            "effort":{"description":"Overrides the child's reasoning effort. Pass it only when the user, AGENTS.md or a skill explicitly names a level for this work; omit it to use the agent type's normal setting.","type":"string","enum":["low","medium","high","xhigh","max"]},
            "isolation":{"description":"Optional isolation. Omit or use null for the current workspace. Use worktree only when the task requests an isolated copy.","type":["string","null"],"enum":["worktree",null]},
            "run_in_background":{"description":"Defaults to true. Set to false to wait for the agent's result in this turn.","type":"boolean"}
        },"required":["description","prompt"],"additionalProperties":false})
    }]
}

/// Maps the reference Agent input onto the delegation fields: subagent_type
/// names the definition and, from the main session, agents run in the background.
pub(super) fn normalize_agent_input(input: &Value, main_session: bool) -> Value {
    let mut input = input.clone();
    if input["agent_type"].is_null() {
        if let Some(kind) = input["subagent_type"]
            .as_str()
            .filter(|kind| !kind.is_empty())
        {
            input["agent_type"] = json!(if kind == "general-purpose" {
                "general"
            } else {
                kind
            });
        }
    }
    if input["run_in_background"].is_null() && main_session {
        input["run_in_background"] = json!(true);
    }
    input
}

/// A fork is a background branch of this session, not an agent definition.
fn is_fork(input: &Value) -> bool {
    input["agent_type"] == "fork"
}

/// Rejects fork options that do not apply to a copy of this session.
fn validate_fork(input: &Value, typed: bool) -> Result<()> {
    if input["run_in_background"] == false {
        anyhow::bail!("A fork runs in the background; omit run_in_background or set it to true");
    }
    if input["model"]
        .as_str()
        .is_some_and(|model| !model.trim().is_empty())
    {
        anyhow::bail!("A fork uses this session's model; omit model");
    }
    if typed {
        anyhow::bail!("A fork cannot take an output schema or a tool list");
    }
    Ok(())
}

/// Fills `input["model"]` from the stored default of its agent type when the
/// call has no model of its own. Returns the type whose default was used, so a
/// provider that rejects it can name the type in the error.
fn fill_default_model(input: &mut Value, data_dir: &std::path::Path) -> Option<String> {
    if is_fork(input) {
        return None;
    }
    if input["model"]
        .as_str()
        .is_some_and(|model| !model.trim().is_empty())
    {
        return None;
    }
    let raw = input["agent_type"].as_str().unwrap_or("general");
    let normalized = raw.to_lowercase();
    let agent = agents::canonical(&normalized);
    let model = agents::default_model(data_dir, agent)?;
    input["model"] = json!(model);
    Some(agent.to_owned())
}

struct ChildState {
    history: crate::db::session_store::ExecutorHistory,
    value: Value,
    event_tx: tokio::sync::mpsc::Sender<EngineEvent>,
    request_id: String,
    request_base: u64,
    streamed_bytes: u64,
}

struct TeamTaskGuard {
    cwd: String,
    team_id: String,
    task_id: String,
    finished: bool,
}
impl Drop for TeamTaskGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = crate::tools::coordination::finish_team_task(
                &self.cwd,
                &self.team_id,
                &self.task_id,
                None,
                Some("Parent task was cancelled or interrupted"),
            );
        }
    }
}
impl ChildState {
    fn snapshot(&mut self, snapshot: &Value, assistant: &str) -> Result<()> {
        let total=self.history.child_request(assistant,snapshot)?;
        let id = snapshot["id"].as_str().unwrap_or_default();
        if self.request_id != id {
            self.request_id = id.to_owned();
            self.request_base = total;
            self.streamed_bytes = 0;
        }
        if snapshot["outputTokens"].is_number() {
            self.value["output_tokens"] = json!(total);
            self.value["output_tokens_estimated"] = json!(false);
        }
        self.history.save_child_state(&self.value)
    }

    fn usage(&mut self, usage: &Value) {
        if let Some(tokens) = usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .filter(|tokens| *tokens > 0)
        {
            self.value["output_tokens"] = json!(self.request_base.saturating_add(tokens));
            self.value["output_tokens_estimated"] = json!(false);
        }
    }

    fn streamed(&mut self, text: &str) {
        self.streamed_bytes = self.streamed_bytes.saturating_add(text.len() as u64);
        if self.streamed_bytes > 0 {
            self.value["output_tokens"] = json!(self
                .request_base
                .saturating_add(self.streamed_bytes.div_ceil(3)));
            self.value["output_tokens_estimated"] = json!(true);
        }
    }

    fn progress(&self, status: &str, tool_name: Option<String>) -> EngineEvent {
        EngineEvent::AgentProgress {
            address: self.history.address(),
            session_id: self.value["session_id"].as_str().unwrap_or_default().into(),
            agent_type: self.value["agent_type"].as_str().unwrap_or_default().into(),
            status: status.into(),
            tool_name,
        }
    }
}
impl Drop for ChildState {
    fn drop(&mut self) {
        if self.value["status"] == "running" {
            self.value["status"] = json!("cancelled");
            let _ = self.event_tx.try_send(self.progress("cancelled", None));
        }
        self.value["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
        if self.history.save_child_state(&self.value).is_err() {
            tracing::warn!("Could not persist child task state");
        }
    }
}

fn owned_binding(
    context: &ServiceContext,
    parent: &str,
    child: &str,
    expected: &str,
) -> Result<Value> {
    context.db.owned_workspace_binding(parent,child,std::path::Path::new(expected))
}

/// Loads agent memory Markdown from the upstream scope directories.
fn agent_memory(
    root: &std::path::Path,
    workspace: &std::path::Path,
    agent: &str,
    scope: &str,
) -> Result<String> {
    ensure!(
        !agent.is_empty()
            && agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "Invalid agent type for memory scope"
    );
    let directory = match scope {
        "user" => root.join("data/agent-memory").join(agent),
        "project" => workspace.join(".claude/agent-memory").join(agent),
        "local" => workspace.join(".claude/agent-memory-local").join(agent),
        _ => anyhow::bail!("Invalid agent memory scope"),
    };
    let mut files: Vec<_> = match std::fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.into()),
    };
    files.sort();
    let mut memory = String::new();
    for path in files {
        let content = std::fs::read_to_string(&path)?;
        if content.trim().is_empty() {
            continue;
        }
        ensure!(
            memory.len() + content.len() <= 64 * 1024,
            "Agent memory exceeds 64 KiB"
        );
        memory.push_str(&format!(
            "## {}\n{}\n\n",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("memory.md"),
            content
        ));
    }
    Ok(memory)
}

struct PreparedChild {
    context: ServiceContext,
    id: String,
    agent_type: String,
    child: ToolLoopExecutor,
    receiver: tokio::sync::mpsc::Receiver<EngineEvent>,
    policy: Value,
    model: String,
}

/// Marks a teammate idle when its child finishes, or stopped when the leader
/// future is cancelled or dropped.
struct MemberGuard {
    root: std::path::PathBuf,
    team: String,
    agent: String,
    session: String,
    finished: bool,
}

impl Drop for MemberGuard {
    fn drop(&mut self) {
        crate::services::teams::unregister_live(&self.session);
        if !self.finished {
            let _ = crate::services::teams::finish_member(
                &self.root,
                &self.team,
                &self.agent,
                "cancelled",
                None,
            );
        }
    }
}

impl ToolLoopExecutor {
    /// The worker may coordinate only children whose original runtime it still owns.
    /// This executor never executes a parent model turn or resolves new credentials.
    pub(in crate::native_engine) fn idle_owner_for_child(
        registry: OwnedChildren,
        owner: &str,
        child: &str,
        events: tokio::sync::mpsc::Sender<EngineEvent>,
    ) -> Result<Self> {
        let entries = registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries
            .get(child)
            .context("Original child runtime is unavailable; needs_resume")?;
        ensure!(entry.parent == owner, "Child belongs to another owner");
        let mut host = Self::new(
            entry.owner_provider.clone(),
            Vec::new(),
            None,
            entry.executor.max_tokens,
            events,
            entry.executor.workspace_cwd.clone(),
            entry.executor.reasoning_mode.clone(),
        );
        host.provider_manager = entry.executor.provider_manager.clone();
        host.conv_id = Some(owner.into());
        host.search_config_path = entry.executor.search_config_path.clone();
        host.child_resumes = entry.owner_budget.clone();
        drop(entries);
        host.owned_children = registry;
        Ok(host)
    }

    pub(in crate::native_engine) fn share_owned_child_budget(&self) {
        let Some(owner) = self.conv_id.as_deref() else {
            return;
        };
        for child in self
            .owned_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values_mut()
        {
            if child.parent == owner {
                child.owner_budget = self.child_resumes.clone();
            }
        }
    }

    fn child_owner_policy(&self) -> Value {
        json!({"permissions":self.permission_manager.as_ref().map(|manager|manager.delegation_policy_snapshot(self.conv_id.as_deref().unwrap_or_default())),"tools":self.allowed_tools,"directories":self.allowed_directories})
    }

    pub(super) async fn resume_pending_owned_children(&self) {
        if self
            .child_resumes
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 8
        {
            return;
        }
        let ids: Vec<String> = {
            let registry = self
                .owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry
                .iter()
                .filter(|(_, child)| Some(child.parent.as_str()) == self.conv_id.as_deref())
                .map(|(id, _)| id.clone())
                .collect()
        };
        let Some(root) = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
        else {
            return;
        };
        let Ok(context) = crate::services::session_messages::context(root) else {
            return;
        };
        for id in ids {
            if self
                .child_resumes
                .load(std::sync::atomic::Ordering::Relaxed)
                >= 8
            {
                break;
            }
            if crate::services::session_messages::pending_count(&context, &id).unwrap_or(0) > 0 {
                // The queue remains durable when policy/configuration no longer permits a wake.
                if let Ok(result) = Box::pin(self.resume_owned_child(&id, None, true)).await {
                    if let Some(parent) = self.conv_id.as_deref() {
                        let mut summary = format!(
                            "Owned child {id} resumed at an owner turn boundary and completed: {}",
                            result["result"]
                        );
                        if summary.len() > 60 * 1024 {
                            let mut cut = 60 * 1024;
                            while !summary.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            summary.truncate(cut);
                            summary.push_str(" (truncated; inspect child transcript)");
                        }
                        let _ = crate::services::session_messages::send(
                            &context, &id, parent, &summary,
                        );
                    }
                }
            }
        }
    }

    pub(in crate::native_engine) async fn resume_owned_child(
        &self,
        id: &str,
        prompt: Option<&str>,
        automatic: bool,
    ) -> Result<Value> {
        ensure!(
            self.child_resumes
                .load(std::sync::atomic::Ordering::Relaxed)
                < 8,
            "Owned child resume budget exhausted; needs_resume"
        );
        if let Some(prompt) = prompt {
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= 64000,
                "Resume prompt must contain 1..64000 bytes"
            );
        }
        let parent = self
            .conv_id
            .as_deref()
            .context("Child owner unavailable")?
            .to_owned();
        let root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
            .context("Runtime unavailable")?;
        let context = crate::services::session_messages::context(root)?;
        let saved: Value = context
            .child_progress(&id)?
            .context("Child ownership record unavailable; needs_resume")?;
        ensure!(
            saved["parent_session_id"] == parent,
            "Child belongs to another owner"
        );
        ensure!(
            saved["status"] == "completed",
            "Child is stopped, cancelled, failed or running; automatic reactivation is prohibited"
        );
        ensure!(
            !crate::services::session_messages::receiver_active(root, id)?,
            "Child already has an active receiver"
        );
        // The owner must still be admitted to dispatch a continuation, even
        // though the retained child uses its own launch-time transport.
        self.provider.current()?;
        let policy = self.child_owner_policy();
        let mut entry = {
            let manager = match &self.provider_manager {
                Some(manager) => Some(manager.lock().await),
                None => None,
            };
            let mut registry = self
                .owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if registry.get(id).is_some_and(|entry| entry.parent == parent &&
                manager.as_ref().is_some_and(|catalog| !entry.binding_is_current(catalog))
            ) {
                registry.remove(id);
            }
            let retained = registry
                .get(id)
                .context("Original child configuration is no longer resident; needs_resume")?;
            ensure!(
                retained.parent == parent && retained.policy == policy,
                "Owner policy changed; needs_resume"
            );
            ensure!(
                owned_binding(&context, &parent, id, &retained.executor.workspace_cwd)?
                    == retained.binding,
                "Child workspace/provider binding changed; needs_resume"
            );
            registry.remove(id).unwrap()
        };
        self.child_resumes
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count < 8).then_some(count + 1),
            )
            .map_err(|_| anyhow::anyhow!("Owned child resume budget exhausted"))?;
        // Original provider/model, tool bounds, permissions and hooks stay with this executor.
        // Only event routing and durable transcript are refreshed for the current owner turn.
        let child_history=entry.executor.executor_history.clone().context("Missing retained child history")?;
        entry.executor.execution_lease =
            child_history.execution_lease()?
            .map(std::sync::Arc::new);
        let current: Value = context
            .child_progress(&id)?
            .context("Child ownership disappeared")?;
        ensure!(
            current["parent_session_id"] == parent && current["status"] == "completed",
            "Child was stopped or ownership changed before resume"
        );
        // Refresh verified owner constraints on every resume, without promoting
        // the model-generated resume prompt or peer inbox data to user authority.
        let parent_history=match self.executor_history.clone() {
            Some(history)=>history,
            None=>context.db.executor_history(&parent)?,
        };
        child_history.inherit_child_authority(&parent_history)?;
        let sessions = SessionStore::new(context.db.clone());
        if let Some(prompt) = prompt {
            sessions.append_prompt(id, prompt, "delegation_resume")?;
        }
        entry
            .executor
            .load_foreground_child_context(&context, id)
            .await?;
        let (events, receiver) = tokio::sync::mpsc::channel(500);
        entry.executor.event_tx = events;
        entry.executor.answer_waiters = self.answer_waiters.clone();
        entry.executor.permission_mailbox = self.permission_mailbox.clone();
        entry.executor.permission_host_available = self.permission_host_available;
        entry.executor.require_pending_peer_message = automatic;
        entry.executor.stop_hook_active = false;
        if let Some(manager) = &entry.executor.permission_manager {
            manager.clear_all_confirmations();
        }
        let result = Box::pin(self.run_owned_child(
            context,
            id.into(),
            saved["agent_type"].as_str().unwrap_or("general").into(),
            *entry.executor,
            receiver,
            entry.policy,
        ))
        .await?;
        Ok(
            json!({"session_id":id,"status":"completed","resumed_in_owner_turn":true,"trigger":if automatic {"inbox"} else {"explicit"},"result":result["result"]}),
        )
    }

    pub(super) async fn execute_team(&mut self, input: &Value) -> Result<Value> {
        let team_id = input["team_id"].as_str().context("team_id is required")?;
        let mut results = Vec::new();
        for _ in 0..100 {
            let Some(task) =
                crate::tools::coordination::claim_team_task(&self.workspace_cwd, team_id)?
            else {
                break;
            };
            let task_id = task["task_id"]
                .as_str()
                .context("Claimed task has no ID")?
                .to_string();
            let mut guard = TeamTaskGuard {
                cwd: self.workspace_cwd.clone(),
                team_id: team_id.into(),
                task_id: task_id.clone(),
                finished: false,
            };
            let result = Box::pin(
                self.execute_delegated(&json!({"prompt":task["prompt"],"agent_type":"general"})),
            )
            .await;
            match result {
                Ok(value) => {
                    crate::tools::coordination::finish_team_task(
                        &self.workspace_cwd,
                        team_id,
                        &task_id,
                        Some(&value.to_string()),
                        None,
                    )?;
                    guard.finished = true;
                    results.push(json!({"task_id":task_id,"result":value}));
                }
                Err(error) => {
                    crate::tools::coordination::finish_team_task(
                        &self.workspace_cwd,
                        team_id,
                        &task_id,
                        None,
                        Some(&error.to_string()),
                    )?;
                    guard.finished = true;
                    return Err(error);
                }
            }
        }
        Ok(json!({"team_id":team_id,"completed_tasks":results}))
    }

    pub(super) async fn execute_delegated(&self, input: &Value) -> Result<Value> {
        self.execute_delegated_typed(input, None, None).await
    }

    pub(super) async fn execute_delegated_invocation(
        &self,
        input: &Value,
        tool_use_id: &str,
    ) -> Result<Value> {
        Box::pin(self.execute_delegated_with(input, None, None, true, false, Some(tool_use_id)))
            .await
    }

    fn record_child_invocation(
        &self,
        context: &ServiceContext,
        child: &str,
        input: &Value,
        tool_use_id: Option<&str>,
        mode: &str,
    ) -> Result<Option<crate::native_engine::child_session::InvocationGuard>> {
        let parent = self.conv_id.as_deref().context("Child owner unavailable")?;
        crate::native_engine::child_session::record_child_invocation(
            context,
            parent,
            child,
            input["description"].as_str().unwrap_or("Agent"),
            tool_use_id,
            self.parent_message_id.as_deref(),
            mode,
        )
    }

    /// Workflow children inherit authority and may only narrow their tool set.
    pub(super) async fn execute_delegated_typed(
        &self,
        input: &Value,
        schema: Option<Value>,
        tools: Option<Vec<String>>,
    ) -> Result<Value> {
        self.execute_delegated_authorized(input, schema, tools, false)
            .await
    }

    /// A deep-research child: a typed delegation that may fetch any page
    /// without asking, since the user explicitly started web research with
    /// /deep-research. Deny and ask rules for WebFetch still apply.
    pub(super) async fn execute_research_child(
        &self,
        input: &Value,
        schema: Value,
        tools: Vec<String>,
    ) -> Result<Value> {
        Box::pin(self.execute_delegated_with(input, Some(schema), Some(tools), false, true, None))
            .await
    }

    pub(super) async fn execute_delegated_authorized(
        &self,
        input: &Value,
        schema: Option<Value>,
        tools: Option<Vec<String>>,
        preapproved: bool,
    ) -> Result<Value> {
        // Boxed: the delegation future is large and nests inside tool calls.
        Box::pin(self.execute_delegated_with(input, schema, tools, preapproved, false, None)).await
    }

    async fn execute_delegated_with(
        &self,
        input: &Value,
        schema: Option<Value>,
        tools: Option<Vec<String>>,
        preapproved: bool,
        web_research: bool,
        tool_use_id: Option<&str>,
    ) -> Result<Value> {
        let mut input = if preapproved {
            input.clone()
        } else {
            use crate::permissions::PermissionResult;
            let invocation_id = uuid::Uuid::new_v4().to_string();
            match self.check_permission("Agent", input, &invocation_id).await {
                PermissionResult::Denied(reason) => {
                    anyhow::bail!("Delegation permission denied: {reason}")
                }
                PermissionResult::RequiresConfirmation(_) => {
                    self.request_tool_approval("Agent", input, &invocation_id)
                        .await?
                }
                PermissionResult::Granted => input.clone(),
            }
        };
        if let Some(id) = input["resume"].as_str() {
            let root = self
                .search_config_path
                .as_deref()
                .and_then(crate::data_layout::data_root_of)
                .context("Runtime unavailable")?;
            let context = crate::services::session_messages::context(root)?;
            let saved: Value = context
                .child_progress(&id)?
                .context("Child ownership record unavailable")?;
            ensure!(
                saved["parent_session_id"].as_str() == self.conv_id.as_deref(),
                "Child belongs to another owner"
            );
            let mut invocation =
                self.record_child_invocation(&context, id, &input, tool_use_id, "foreground")?;
            let result =
                Box::pin(self.resume_owned_child(id, input["prompt"].as_str(), false)).await;
            if let Some(invocation) = &mut invocation {
                invocation.finish(&result)?;
            }
            return result;
        }
        if is_fork(&input) {
            validate_fork(&input, schema.is_some() || tools.is_some())?;
            return Box::pin(self.spawn_background_agent(&input, tool_use_id)).await;
        }
        // A per-type default model stands in for an omitted model; an explicit
        // per-call model always wins. Remember the type for the validation error.
        let default_for = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
            .and_then(|data_dir| fill_default_model(&mut input, data_dir));
        if input["run_in_background"] == true && schema.is_none() && tools.is_none() {
            return Box::pin(self.spawn_background_agent(&input, tool_use_id)).await;
        }
        let mut prepared =
            Box::pin(self.prepare_delegated_child(&input, schema, tools, default_for.as_deref()))
                .await?;
        let mut invocation = self.record_child_invocation(
            &prepared.context,
            &prepared.id,
            &input,
            tool_use_id,
            "foreground",
        )?;
        if web_research {
            prepared.child.raw_web_fetch = true;
            if let Some(manager) = &prepared.child.permission_manager {
                manager.allow_in_session(&prepared.id, "WebFetch");
            }
        }
        let result = Box::pin(self.run_owned_child(
            prepared.context,
            prepared.id,
            prepared.agent_type,
            prepared.child,
            prepared.receiver,
            prepared.policy,
        ))
        .await;
        if let Some(invocation) = &mut invocation {
            invocation.finish(&result)?;
        }
        result
    }

    /// Arguments and environment that start a `dcl` child with this session's
    /// provider, model and permission mode.
    pub(super) fn child_cli_launch(
        &self,
        root: &std::path::Path,
    ) -> Result<(Vec<String>, Vec<(String, String)>)> {
        let mode = self
            .permission_manager
            .as_ref()
            .map(|manager| crate::services::session_permission_mode::name(manager.get_mode()))
            .unwrap_or("ask");
        Ok(crate::native_engine::child_session::child_cli_arguments(
            &self.provider,
            &self.model_selector,
            mode,
            root,
        ))
    }

    /// As the reference CLI's background agents: the child runs in its own
    /// session as a background task with the same provider, model and
    /// permission mode, and this call returns at once. The host announces it
    /// and wakes this session when it finishes.
    async fn spawn_background_agent(
        &self,
        input: &Value,
        tool_use_id: Option<&str>,
    ) -> Result<Value> {
        // The Agent tool is a model entry point. The reusable child-session
        // launcher deliberately has no depth rule of its own.
        ensure!(
            self.delegation_depth == 0,
            "Background agents start from the main session only"
        );
        let prompt = input["prompt"].as_str().context("prompt is required")?;
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= 64_000,
            "Child prompt must contain 1–64000 bytes"
        );
        let parent = self.conv_id.clone().context("Child owner unavailable")?;
        let root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
            .context("Runtime directory is unavailable")?
            .to_path_buf();
        let context = ServiceContext::open(
            RuntimePaths::new(root.clone())?,
            Arc::new(EnvironmentCredentials),
        )?;
        let fork = is_fork(input);
        if fork {
            ensure!(
                !context.db.is_agent_fork(&parent)?,
                "A fork cannot start another fork"
            );
        }
        let description = input["description"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                prompt
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(60)
                    .collect()
            });
        let workspace = match input["isolation"].as_str() {
            None => crate::native_engine::child_session::ChildWorkspace::Directory(
                std::path::PathBuf::from(&self.workspace_cwd),
            ),
            Some("worktree") => crate::native_engine::child_session::ChildWorkspace::Worktree,
            Some(other) => anyhow::bail!("Unsupported isolation mode: {other}"),
        };
        let mode = self
            .permission_manager
            .as_ref()
            .map(|manager| crate::services::session_permission_mode::name(manager.get_mode()))
            .unwrap_or("ask")
            .to_owned();
        let handle = crate::native_engine::child_session::spawn_child_session(
            crate::native_engine::child_session::ChildLaunchConfig {
                provider: self.provider.clone(),
                permission_mode: mode,
            },
            crate::native_engine::child_session::ChildSpawn {
                data_dir: root.clone(),
                parent_session_id: parent.clone(),
                parent_workspace: std::path::PathBuf::from(&self.workspace_cwd),
                workspace,
                prompt: prompt.into(),
                description: description.clone(),
                agent_type: input["agent_type"].as_str().unwrap_or("general").into(),
                model: input["model"]
                    .as_str()
                    .filter(|model| !model.trim().is_empty())
                    .map(str::to_owned),
                effort: input["effort"].as_str().map(str::to_owned),
                system_prompt_append: None,
                title: if fork {
                    format!("Fork: {description}")
                } else {
                    format!("Agent: {description}")
                },
                resume_session: None,
                reserved_session_id: None,
                reserved_job_id: None,
                extra_env: Vec::new(),
                executable_override: None,
                invocation: tool_use_id.map(|tool_use_id| {
                    crate::native_engine::child_session::ChildInvocation {
                        parent_message_id:self.parent_message_id.clone(),
                        tool_use_id: tool_use_id.into(),
                        description: description.clone(),
                        mode: "background".into(),
                    }
                }),
                member_binding: None,
                allow_workspace_move: false,
                // A fork declares its branch here; the shared launch path
                // copies the parent history and keeps its agent binding.
                fork,
            },
        )
        .await?;
        context.db.register_background_agent(
            &handle.job_id,
            &json!({"agent_type":handle.agent_type,"description":handle.description,"parent":parent,"session_id":handle.session_id,"started_at":chrono::Utc::now().to_rfc3339()}),
        )?;
        Ok(json!({
            "agent_id": handle.job_id,
            "session_id": handle.session_id,
            "to": handle.session_id,
            "status": "running",
            "backgrounded": true,
            "description": handle.description,
            "message": "The agent is running in the background. You will be notified when it finishes; do not wait or poll for it."
        }))
    }

    async fn load_foreground_child_context(
        &mut self,
        context: &ServiceContext,
        id: &str,
    ) -> Result<()> {
        let root = &context.paths.data_dir;
        let Some(ExecutorHistory::Session(shard)) = &self.executor_history else {
            anyhow::bail!("Foreground child requires its session history");
        };
        ensure!(
            shard.id() == id,
            "Foreground child history belongs to another session"
        );
        let (workspace, project, profile) = shard.scoped_stores().engine_workspace(
            &root.join("data/workspaces"),
            id,
            Some(&self.workspace_cwd),
        )?;
        self.prompt_profile = profile;
        self.reload_history(&shard.clone())?;
        let resident_indexes = self.session_reminders.resident_indexes.clone();
        let instructions = crate::project_instructions::load_all(Some(root), &workspace)?;
        let (user_instructions, project_instructions) =
            crate::project_instructions::reminder_parts(&instructions);
        let (environment, date) =
            crate::prompt::session_environment(root, id, &workspace, project.as_ref());

        let customization = self.customization()?;
        let (builtin, mcp) = if customization.loading("ToolLoad").enabled
            && self
                .allowed_tools
                .as_ref()
                .is_none_or(|tools| tools.iter().any(|name| name == "ToolLoad"))
        {
            let catalog = customization.described(self.builtin_catalog());
            let mcp_tools = match &self.mcp_registry {
                Some(registry) => super::discover_mcp_tools(registry).await?.0,
                None => Vec::new(),
            };
            super::loadable_tool_lists(
                &customization,
                &catalog,
                &mcp_tools,
                &crate::mcp::states::load(root),
                self.allowed_tools.as_deref(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let mut skills = crate::services::plugins::skill_commands(context)?;
        skills.extend(crate::skills::discovery::discover(root, &workspace)?);
        let lines = skills
            .values()
            .filter(|skill| skill.model_invocable() && !skill.always_inline)
            .map(|skill| {
                crate::native_engine::engine_core::skill_listing_line(
                    &skill.name,
                    &skill.description,
                    skill.context.as_deref(),
                    skill.listing_name_only,
                )
            })
            .collect::<Vec<_>>();
        let experience_enabled = crate::experience::enabled(context);
        let experience_workspace =
            (!crate::experience::is_internal_workspace(&workspace, &root.join("data/workspaces")))
                .then(|| workspace.clone());
        let scanned = experience_workspace
            .as_deref()
            .map(crate::experience::scan)
            .unwrap_or_default();
        self.set_session_reminders(crate::prompt::SessionReminders {
            user_instructions,
            project_instructions,
            resident_indexes,
            experience: crate::prompt::experience_for_session(
                root,
                id,
                experience_workspace.as_deref(),
                experience_enabled,
                &scanned.entries,
            ),
            environment: Some(environment),
            date: Some(date),
            git: crate::services::git_snapshot::for_session(root, id, &workspace)
                .as_deref()
                .map(crate::services::git_snapshot::reminder),
            loadable_tools: crate::prompt::loadable_tools_for_session(root, id, &builtin, &mcp),
            available_skills: crate::prompt::skills_for_session(root, id, &lines),
            inline_skills: crate::prompt::inline_skills_for_session(
                root,
                id,
                &crate::prompt::inline_skill_blocks(&skills),
            ),
            standalone_frontend: self.standalone_frontend,
            skills: crate::services::turn_context::invoked_skills_reminder(
                &self.turn_context,
                &self.messages,
            ),
        });
        Ok(())
    }

    /// Builds an owned child session without running it so a leader can start
    /// several teammates before awaiting any of them.
    async fn prepare_delegated_child(
        &self,
        input: &Value,
        schema: Option<Value>,
        tools: Option<Vec<String>>,
        default_for: Option<&str>,
    ) -> Result<PreparedChild> {
        ensure!(self.delegation_depth < 2, "Delegation depth limit reached");
        let prompt = input["prompt"].as_str().context("prompt is required")?;
        let max_prompt = if schema.is_some() {
            1024 * 1024
        } else {
            64_000
        };
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= max_prompt,
            "Child prompt must contain 1–{max_prompt} bytes"
        );
        let root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
            .context("Runtime directory is unavailable")?;
        let context = ServiceContext::open(
            RuntimePaths::new(root.to_path_buf())?,
            Arc::new(EnvironmentCredentials),
        )?;
        let agent_type = input["agent_type"].as_str().unwrap_or("general");
        let runtime = self
            .runtime_agents
            .iter()
            .find(|agent| agent.definition.id == agent_type)
            .cloned();
        let mut definition = match &runtime {
            Some(agent) => agent.definition.clone(),
            None => agents::resolve(&context, agent_type)?,
        };
        let mut provider = match runtime.as_ref().and_then(|agent| agent.provider.as_ref()) {
            Some(selector) => {
                let manager = self.provider_manager.as_ref().context("Agent provider manager unavailable")?.lock().await;
                selector.resolve(&manager)?
            }
            None => self.provider.clone(),
        };
        if let Some(effort) = runtime.as_ref().and_then(|agent| agent.effort.clone()) {
            provider.model.effort = Some(effort);
        }
        if let Some(model) = input["model"]
            .as_str()
            .filter(|model| !model.trim().is_empty())
        {
            if model != provider.model.id {
                let selected = crate::native_engine::provider_manager::resolve_model_selection(
                    &provider.provider.models,
                    model,
                );
                provider.model = match selected {
                    Some(selected) => selected,
                    None => {
                        let reason = format!("Unknown model for agent provider: {model}");
                        match default_for {
                            Some(agent) => anyhow::bail!("The default model for agent type {agent} ({model}) is not available: {reason}. Change it with dcl agents model {agent} <model> or clear it with dcl agents model {agent} default."),
                            None => anyhow::bail!("{reason}"),
                        }
                    }
                };
            }
        }
        if let Some(effort) = input["effort"].as_str() {
            ensure!(
                ["low", "medium", "high", "xhigh", "max"].contains(&effort),
                "Invalid effort"
            );
            provider.model.effort = Some(effort.into());
        }
        ensure!(
            input["isolation"].is_null() || input["isolation"] == "worktree",
            "Unsupported isolation mode"
        );
        if let Some(agent) = &runtime {
            if !agent.disallowed_tools.is_empty() {
                let available = match definition.tools.take() {
                    Some(tools) => tools,
                    None => {
                        let mut tools = self
                            .builtin_catalog()
                            .into_iter()
                            .map(|tool| tool.name)
                            .collect::<Vec<_>>();
                        if let Some(registry) = &self.mcp_registry {
                            tools.extend(
                                registry
                                    .get_all_tools()
                                    .await?
                                    .into_iter()
                                    .map(|tool| tool.name),
                            );
                        }
                        tools
                    }
                };
                definition.tools = Some(
                    available
                        .into_iter()
                        .filter(|tool| !agent.disallowed_tools.contains(tool))
                        .collect(),
                );
            }
        }
        if let Some(agent) = &runtime {
            if !agent.mcp_servers.is_empty() {
                let mut available: Vec<String> = self
                    .builtin_catalog()
                    .into_iter()
                    .map(|tool| tool.name)
                    .filter(|name| !name.starts_with("mcp__"))
                    .collect();
                if let Some(registry) = &self.mcp_registry {
                    available.extend(
                        registry
                            .get_all_tools()
                            .await?
                            .into_iter()
                            .map(|tool| tool.name)
                            .filter(|name| {
                                agent
                                    .mcp_servers
                                    .iter()
                                    .any(|server| name.starts_with(&format!("mcp__{server}__")))
                            }),
                    );
                }
                definition.tools = Some(match definition.tools.take() {
                    Some(tools) => available
                        .into_iter()
                        .filter(|tool| tools.contains(tool))
                        .collect(),
                    None => available,
                });
            }
        }
        if let Some(parent) = &self.allowed_tools {
            definition.tools = Some(match definition.tools {
                Some(child) => child
                    .into_iter()
                    .filter(|name| parent.contains(name))
                    .collect(),
                None => parent.clone(),
            });
        }
        if let Some(tools) = tools {
            definition.tools = Some(match definition.tools {
                Some(allowed) => tools
                    .into_iter()
                    .filter(|name| allowed.contains(name))
                    .collect(),
                None => tools,
            });
        }
        let sessions = SessionStore::new(context.db.clone());
        let id = if let Some(parent) = self.conv_id.as_deref() {
            // Preserve the parent's trusted source identity and managed worktree.
            // Creating from actual cwd would incorrectly trust a worktree as a new project.
            let parent_row = context
                .db
                .conversation_metadata(parent)?
                .context("Parent session not found")?;
            let workspaces_root = if parent_row.project_id.as_deref().is_none_or(str::is_empty) {
                // The engine may use a private root outside data_dir/workspaces.
                std::path::Path::new(&self.workspace_cwd)
                    .parent()
                    .context("Private workspace has no root")?
                    .to_path_buf()
            } else {
                context.paths.data_dir.join("data/workspaces")
            };
            let child = sessions.derive_owned_child(
                parent,
                &workspaces_root,
                std::path::Path::new(&self.workspace_cwd),
            )?;
            use crate::db::session_store::SessionAccess;
            context.db.select_session(&child)?.initialize_child_model(&provider.model.id,&provider.provider.id)?;
            child
        } else {
            sessions.create(
                std::path::Path::new(&self.workspace_cwd),
                &provider.model.id,
                &provider.provider.id,
            )?
        };
        let child_workspace = if input["isolation"] == "worktree" {
            let worktree_input = json!({"name":format!("agent-{}", &id[..8.min(id.len())])});
            let worktree: std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<(Value, std::path::PathBuf)>>
                        + Send
                        + '_,
                >,
            > = Box::pin(crate::tools::session_worktree::execute(
                "EnterWorktree",
                &worktree_input,
                root,
                &id,
                std::path::Path::new(&self.workspace_cwd),
            ));
            worktree.await?.1.to_string_lossy().into_owned()
        } else {
            self.workspace_cwd.clone()
        };
        if runtime.is_some() {
            // Preserve the effective tool boundary on a later standalone child resume,
            // without making the SDK prompt/definition available after this run.
            agents::bind(
                &context.db,
                &id,
                &agents::Definition {
                    id: "sdk-runtime-boundary".into(),
                    description: "Inherited tool restrictions from an SDK child".into(),
                    system_prompt:
                        "This session retains the tool restrictions of its original SDK child task."
                            .into(),
                    tools: definition.tools.clone(),
                    read_only_shell: definition.read_only_shell,
                    legacy: None,
                },
            )?;
        } else {
            agents::bind(&context.db, &id, &definition)?;
        }
        context.set_child_description(&id, input["description"].as_str().unwrap_or(prompt))?;
        context.db.open_session(&id)?.write(|w| {
            w.set_setting(
                crate::db::session_store::settings::MEMORY_AGENT,
                &definition.id,
            )
        })?;
        sessions.append_prompt(&id, prompt, "delegation")?;
        let (events, receiver) = tokio::sync::mpsc::channel(500);
        let mut system = format!(
            "{}\nChild task role:\n{}\nChild session ID: {}\nParent session ID: {}",
            self.system_prompt.as_deref().unwrap_or(""),
            definition.system_prompt,
            id,
            self.conv_id.as_deref().unwrap_or("")
        );
        if let Some(agent) = &runtime {
            if let Some(context) = &agent.skill_context {
                system.push_str("\n\n");
                system.push_str(context);
            }
            if let Some(scope) = &agent.memory {
                let memory = agent_memory(
                    root,
                    std::path::Path::new(&self.workspace_cwd),
                    &definition.id,
                    scope,
                )?;
                if !memory.is_empty() {
                    system.push_str("\n\nAgent memory:\n");
                    system.push_str(&memory);
                }
            }
            if let Some(reminder) = &agent.reminder {
                system.push_str("\n\nCritical system reminder:\n");
                system.push_str(reminder);
            }
        }
        let child_max_tokens = self
            .max_tokens
            .min(provider.model.max_tokens.unwrap_or(self.max_tokens));
        let model = provider.model.id.clone();
        let read_only_shell = definition.read_only_shell;
        let mut child = ToolLoopExecutor::new(
            provider,
            Vec::new(),
            Some(system),
            child_max_tokens,
            events,
            child_workspace,
            self.reasoning_mode.clone(),
        )
        .with_provider_manager(self.provider_manager.clone())
        .with_runtime_agents(self.runtime_agents.clone())
        .with_allowed_tools(definition.tools)
        .with_read_only_shell(read_only_shell)
        .with_prompt_profile(self.prompt_profile)
        .with_conv_id(id.clone())
        .with_executor_history(context.db.executor_history(&id)?)?
        .with_answer_waiters(self.answer_waiters.clone())
        .with_permission_mailbox(self.permission_mailbox.clone())
        .with_permission_host(self.permission_host_available)
        .with_bundled_resources(self.bundled_resources.clone())
        .with_search_config_path(self.search_config_path.clone().unwrap());
        child = child.with_response_schema(schema)?;
        child.allowed_directories = self.allowed_directories.clone();
        child.delegation_depth = self.delegation_depth + 1;
        child.durable_user_input = self.durable_user_input;
        child.auto_manual_interactive = self.auto_manual_interactive;
        child.delegation_parent = self
            .conv_id
            .as_ref()
            .zip(self.permission_manager.as_ref())
            .map(|(parent, manager)| (parent.clone(), manager.clone()));
        *child
            .observed_parent_mode
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = self
            .permission_manager
            .as_ref()
            .map(|manager| manager.get_mode());
        child.permission_manager = self.permission_manager.as_ref().map(|manager| {
            Arc::new(manager.fork_session(self.conv_id.as_deref().unwrap_or_default(), &id))
        });
        if let Some(mode) = runtime
            .as_ref()
            .and_then(|agent| agent.permission_mode.as_ref())
        {
            let mode = crate::permissions::PermissionMode::parse(mode)?;
            let parent_mode = self
                .permission_manager
                .as_ref()
                .map(|manager| manager.get_mode())
                .unwrap_or_default();
            ensure!(
                mode != crate::permissions::PermissionMode::BypassPermissions
                    || parent_mode == crate::permissions::PermissionMode::BypassPermissions,
                "SDK agent {agent_type} cannot enable bypassPermissions"
            );
            if let Some(manager) = &child.permission_manager {
                manager.set_mode(mode);
            }
        }
        child.hooks = self.hooks.clone();
        let hook_session = self
            .subagent_hook_context
            .as_ref()
            .map(|(parent, _)| parent.clone())
            .or_else(|| self.conv_id.clone())
            .unwrap_or_else(|| "unknown".into());
        child.subagent_hook_context = Some((hook_session.clone(), definition.id.clone()));
        if let Some(hooks) = &child.hooks {
            use crate::services::tool_hooks::HookEvent;
            let start = hooks
                .run(
                    HookEvent::SubagentStart,
                    &hook_session,
                    json!({"agent_id":id,"agent_type":definition.id}),
                )
                .await?;
            for context in start.additional_context {
                child
                    .system_prompt
                    .get_or_insert_with(String::new)
                    .push_str(&format!("\nSubagentStart context:\n{context}"));
            }
            for warning in start.warnings {
                tracing::warn!("SubagentStart hook: {warning}");
            }
        }
        child.max_tool_iterations = runtime.as_ref().and_then(|agent| agent.max_turns).map_or(
            self.max_tool_iterations,
            |limit| {
                Some(
                    self.max_tool_iterations
                        .map_or(limit as usize, |parent| parent.min(limit as usize)),
                )
            },
        );
        child.mcp_registry = self.mcp_registry.clone();
        child.standalone_frontend = self.standalone_frontend;
        child.load_foreground_child_context(&context, &id).await?;
        if let Some(memory) = &self.file_memory {
            child = child.with_file_memory(memory.child(&id)?);
        }
        let policy = self.child_owner_policy();
        Ok(PreparedChild {
            model,
            context,
            id,
            agent_type: definition.id.clone(),
            child,
            receiver,
            policy,
        })
    }

    /// Runs named teammates concurrently inside the leader's turn. Each child
    /// owns its session, transcript and forked permission state; dropping the
    /// leader future cancels all of them and marks the members stopped.
    pub(super) async fn execute_team_spawn(&self, input: &Value) -> Result<Value> {
        use crate::services::teams;
        teams::ensure_enabled()?;
        let root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
            .context("Runtime unavailable")?;
        let session = self
            .conv_id
            .as_deref()
            .context("Team tools require an active session")?;
        let team_name = input["team_name"]
            .as_str()
            .context("team_name is required")?;
        let team = teams::read_recovered(root, team_name)?
            .with_context(|| format!("Team not found: {team_name}"))?;
        ensure!(
            team.lead_session_id == session,
            "Only the team leader can spawn teammates"
        );
        let members = input["members"].as_array().context("members is required")?;
        ensure!(
            (1..=8).contains(&members.len()),
            "TeamSpawn needs 1-8 members"
        );
        let mut names = std::collections::HashSet::new();
        for member in members {
            let name = member["name"].as_str().context("member name is required")?;
            let prompt = member["prompt"]
                .as_str()
                .context("member prompt is required")?;
            ensure!(!name.trim().is_empty(), "Teammate name is required");
            ensure!(
                name != teams::LEAD_NAME,
                "team-lead is reserved for the leader"
            );
            ensure!(
                names.insert(name.to_ascii_lowercase()),
                "Duplicate teammate name: {name}"
            );
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= 64_000,
                "Teammate prompt must contain 1-64000 bytes"
            );
        }
        let cwd = std::path::Path::new(&self.workspace_cwd);
        let team_name = team.name.clone();
        let mut prepared = Vec::new();
        for member in members {
            let name = member["name"].as_str().unwrap_or_default();
            let prompt = member["prompt"].as_str().unwrap_or_default();
            let agent_type = member["agent_type"].as_str().unwrap_or("general");
            let child = Box::pin(self.prepare_delegated_child(
                &json!({"prompt":prompt,"agent_type":agent_type}),
                None,
                None,
                None,
            ))
            .await?;
            let registered = teams::register_member(
                root,
                &team.name,
                session,
                name,
                Some(agent_type),
                Some(&child.model),
                prompt,
                cwd,
            )?;
            teams::bind_member_session(root, &team.name, &registered.agent_id, &child.id)?;
            teams::register_live(&child.id);
            prepared.push((name.to_owned(), registered.agent_id, child));
        }
        let results = futures::future::join_all(prepared.into_iter().map(
            |(name, agent, child)| {
                let team_name = team_name.clone();
                async move {
                let mut guard = MemberGuard {
                    root: root.to_path_buf(),
                    team: team_name.clone(),
                    agent: agent.clone(),
                    session: child.id.clone(),
                    finished: false,
                };
                let session_id = child.id.clone();
                let result = self
                    .run_owned_child(
                        child.context,
                        child.id,
                        child.agent_type,
                        child.child,
                        child.receiver,
                        child.policy,
                    )
                    .await;
                match result {
                    Ok(mut value) => {
                        // A blocking TeammateIdle hook keeps the teammate working
                        // with its feedback instead of reporting the member idle.
                        for _ in 0..3 {
                            let Some(hooks) = &self.hooks else { break };
                            match hooks
                                .run(
                                    crate::services::tool_hooks::HookEvent::TeammateIdle,
                                    &session_id,
                                    json!({"teammate_name":name,"team_name":team_name}),
                                )
                                .await
                            {
                                Ok(outcome) if outcome.blocked => {
                                    let feedback =
                                        format!("TeammateIdle hook feedback:
{}", outcome.reason);
                                    match self
                                        .resume_owned_child(&session_id, Some(&feedback), false)
                                        .await
                                    {
                                        Ok(resumed) => value = resumed,
                                        Err(error) => {
                                            tracing::warn!(
                                                "TeammateIdle feedback could not resume {name}: {error:#}"
                                            );
                                            break;
                                        }
                                    }
                                }
                                Ok(_) => break,
                                Err(error) => {
                                    tracing::warn!("TeammateIdle hook failed for {name}: {error:#}");
                                    break;
                                }
                            }
                        }
                        let summary = value["result"].to_string();
                        let _ = teams::finish_member(root, &team_name, &agent, "idle", Some(&summary));
                        guard.finished = true;
                        json!({"name":name,"agent_id":agent,"session_id":session_id,"status":"completed","result":value["result"]})
                    }
                    Err(error) => {
                        let _ = teams::finish_member(root, &team_name, &agent, "failed", Some(&error.to_string()));
                        guard.finished = true;
                        json!({"name":name,"agent_id":agent,"session_id":session_id,"status":"failed","error":error.to_string()})
                    }
                }
                }
            },
        ))
        .await;
        Ok(json!({"team_name":team_name,"members":results}))
    }

    fn run_owned_child(
        &self,
        context: ServiceContext,
        id: String,
        agent_type: String,
        mut child: ToolLoopExecutor,
        receiver: tokio::sync::mpsc::Receiver<EngineEvent>,
        policy: Value,
    ) -> futures::future::BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            // The initial turn follows live rebindings until it completes. Keep
            // the exact launch selection and account for later, independent resumes.
            let launch_provider = child.provider.clone();
            let launch_model = child.model_selector.clone();
            let launch_state = launch_provider.binding_snapshot();
            launch_provider.ensure_current()?;
            let binding = owned_binding(
                &context,
                self.conv_id.as_deref().context("Child owner unavailable")?,
                &id,
                &child.workspace_cwd,
            )?;
            if child.execution_lease.is_none() {
                child.execution_lease = context.db.executor_history(&id)?.execution_lease()?
                .map(std::sync::Arc::new);
            }
            let state = ChildState {
                history: context.db.executor_history(&id)?,
                event_tx: self.event_tx.clone(),
                value: json!({"session_id":id,"parent_session_id":self.conv_id,"agent_type":agent_type,"status":"running","started_at":chrono::Utc::now().to_rfc3339()}),
                request_id: String::new(),
                request_base: 0,
                streamed_bytes: 0,
            };
            state.history.save_child_state(&state.value)?;
            let _ = self.event_tx.send(state.progress("started", None)).await;
            let collect_web_evidence = child.response_schema.is_some();
            let mut transcript =
                child_transcript::ChildTranscript::new(state, receiver, collect_web_evidence)?;
            child.parent_message_id=Some(transcript.assistant_id.clone());
            // The tool loop scopes the child's shells and monitors to the
            // data root, the parent directory of the search config path.
            let cleanup_root = self
                .search_config_path
                .as_deref()
                .and_then(crate::data_layout::data_root_of)
                .map(std::path::Path::to_path_buf);
            let child_id = id.clone();
            // Boxing alone still polls the child on the parent's deep tool-call stack.
            // JoinSet schedules it independently and aborts it if this owner is dropped.
            let mut execution = tokio::task::JoinSet::new();
            execution.spawn(async move {
                // The cleanup guard lives inside the child's own task. An abort
                // therefore drops it only after this future stopped publishing
                // tasks, so work started late cannot slip past the sweep.
                let mut cleanup = ChildSessionTasks::new(cleanup_root, child_id);
                let result = child.execute().await;
                let stop = cleanup.stop().await;
                (child, result, stop)
            });
            let joined = loop {
                tokio::select! {
                    biased;
                    Some(event) = transcript.receiver.recv() => {
                        // The child is still running and is dropped with this
                        // future, so its guard only requests the stops: say so
                        // instead of claiming they already happened.
                        transcript
                            .process(event)
                            .await
                            .map_err(|error| annotate(error, BACKGROUND_TASKS_REQUESTED))?;
                    }
                    result = execution.join_next() => {
                        break result.context("Child execution task missing")?;
                    }
                }
            };
            let (mut child, result, stop) = match joined {
                Ok(value) => value,
                Err(error) => {
                    // The guard in the dropped child task requested the stops,
                    // but its outcome cannot be read here.
                    anyhow::bail!(
                        "Child execution task failed: {error}\n{}",
                        BACKGROUND_TASKS_REQUESTED
                    )
                }
            };
            let summary = stop_summary(&stop);
            // Completion (including errors) can queue events in the final execute poll.
            if let Err(error) = transcript.drain().await {
                return Err(annotate(error, &summary));
            }
            match result {
                Ok((mut text, reason)) => {
                    text = with_summary(text, &summary);
                    // The cleanup already ran, so its summary must survive a
                    // failure to persist the final text as well.
                    transcript
                        .finish_text(&text)
                        .map_err(|error| annotate(error, &summary))?;
                    transcript.state.value["preview"] = json!(text
                        .chars()
                        .rev()
                        .take(8000)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>());
                    let failed = matches!(
                        reason.as_deref(),
                        Some(
                            "stream_error"
                                | "error"
                                | "interrupted"
                                | "failed"
                                | "cancelled"
                                | "max_iterations"
                                | "max_tokens"
                                | "length"
                                | "incomplete"
                        )
                    );
                    let status = if failed { "failed" } else { "completed" };
                    transcript.state.value["status"] = json!(status);
                    let _ = self
                        .event_tx
                        .send(transcript.state.progress(status, None))
                        .await;
                    // A failed or cancelled child returns an error, so the
                    // cleanup summary must travel with the message as well.
                    if failed {
                        anyhow::bail!(
                            "{}",
                            with_summary(
                                format!(
                                    "Child session {id} did not complete: {}",
                                    reason.unwrap_or_default()
                                ),
                                &summary
                            )
                        );
                    }
                    if !collect_web_evidence {
                        if let Some(state) = launch_state {
                            let mut state = (*state).clone();
                            state.provider_id = launch_provider.provider.id.clone();
                            state.model = launch_provider.model.id.clone();
                            let manager =
                                crate::native_engine::provider_manager::ProviderManager::in_memory(
                                    Vec::new(),
                                    context.paths.layout().config_file("providers.json"),
                                );
                            manager.binding.install(state)?;
                            let mut retained = launch_provider;
                            retained.provider.lease = manager.binding.resolve(None)
                                .context("Missing retained child provider")?.provider.lease;
                            child.provider = retained;
                            child.model_selector = launch_model;
                            child.provider_manager = Some(Arc::new(tokio::sync::Mutex::new(manager)));
                        }
                        // Retain configuration/capabilities, not the last turn's transcript or handles.
                        child.messages.clear();
                        child.messages.shrink_to_fit();
                        child.auto_history.clear();
                        child.auto_history.shrink_to_fit();
                        child.streaming_tool_args.clear();
                        child.streaming_tool_args.shrink_to_fit();
                        child.execution_lease = None;
                        child.answer_waiters = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
                        child.permission_mailbox = Default::default();
                        let (events, _receiver) = tokio::sync::mpsc::channel(1);
                        child.event_tx = events;
                        let mut registry = self
                            .owned_children
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if registry.len() >= 32 {
                            if let Some(oldest) = registry
                                .iter()
                                .min_by_key(|(_, entry)| entry.retained)
                                .map(|(id, _)| id.clone())
                            {
                                registry.remove(&oldest);
                            }
                        }
                        registry.insert(
                            id.clone(),
                            OwnedChild {
                                executor: Box::new(child),
                                owner_provider: self.provider.clone(),
                                parent: self.conv_id.clone().unwrap_or_default(),
                                owner_budget: self.child_resumes.clone(),
                                idle_ready: false,
                                policy,
                                binding,
                                retained: Instant::now(),
                            },
                        );
                    }
                    Ok(
                        json!({"session_id":id,"status":"completed","result":text,"web_evidence":&transcript.web_evidence,"resume_available":!collect_web_evidence}),
                    )
                }
                Err(error) => {
                    transcript.state.value["status"] = json!("failed");
                    let _ = self
                        .event_tx
                        .send(transcript.state.progress("failed", None))
                        .await;
                    Err(annotate(error, &summary))
                }
            }
        })
    }
}

/// Reported when the cleanup ran but its outcome could not be read (a panic).
const BACKGROUND_TASKS_REQUESTED: &str = "Background tasks of the subagent were asked to stop.";

/// Appends the cleanup summary to a message, on its own line.
fn with_summary(message: impl std::fmt::Display, summary: &str) -> String {
    if summary.is_empty() {
        message.to_string()
    } else {
        format!("{message}\n{summary}")
    }
}

/// Appends the cleanup summary to an error, keeping the typed error when there
/// is nothing to add.
fn annotate(error: anyhow::Error, summary: &str) -> anyhow::Error {
    if summary.is_empty() {
        error
    } else {
        anyhow::anyhow!("{error:#}\n{summary}")
    }
}

/// One line per non-empty list of the cleanup outcome, for the parent.
fn stop_summary(stop: &SessionStop) -> String {
    let mut lines = Vec::new();
    if !stop.stopped.is_empty() {
        lines.push(format!(
            "Stopped {} background task(s) the subagent left running: {}.",
            stop.stopped.len(),
            stop.stopped.join(", ")
        ));
    }
    if !stop.requested.is_empty() {
        lines.push(format!(
            "Requested a stop of {} background task(s); the stop was not confirmed within 5 seconds: {}.",
            stop.requested.len(),
            stop.requested.join(", ")
        ));
    }
    if !stop.failed.is_empty() {
        let failures = stop
            .failed
            .iter()
            .map(|(subject, error)| format!("{subject} ({error})"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "Could not stop {} background task(s): {failures}.",
            stop.failed.len()
        ));
    }
    if let Some(error) = &stop.scan_error {
        lines.push(format!(
            "Could not check the subagent's background tasks: {error}."
        ));
    }
    lines.join("\n")
}

/// Stops the managed shells and monitors a delegated child left running.
///
/// It lives inside the child's own execution task: a normal end waits for the
/// final status of every requested stop, while an abort or panic only requests
/// them, because acting on a request needs the shell owner's runtime thread.
struct ChildSessionTasks {
    root: Option<PathBuf>,
    session: String,
    outcome: Option<SessionStop>,
}
impl ChildSessionTasks {
    fn new(root: Option<PathBuf>, session: String) -> Self {
        Self {
            root,
            session,
            outcome: None,
        }
    }
    /// The normal path: request the stops and wait (bounded) for confirmation.
    async fn stop(&mut self) -> SessionStop {
        let outcome = match self.root.as_deref() {
            Some(root) => {
                crate::tools::background_shell::stop_running_session_tasks_async(
                    root,
                    &self.session,
                )
                .await
                .unwrap_or_else(|error| self.scan_failure(&error))
            }
            // Without a data root the tool loop gives `background_shell` no
            // session either, so the child could not start an owned task.
            None => SessionStop::default(),
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
    /// The abort/panic fallback: request only, never wait for the owner.
    fn request(&mut self) -> &SessionStop {
        if self.outcome.is_none() {
            let outcome = match self.root.as_deref() {
                Some(root) => crate::tools::background_shell::stop_running_session_tasks(
                    root,
                    &self.session,
                    false,
                )
                .unwrap_or_else(|error| self.scan_failure(&error)),
                None => SessionStop::default(),
            };
            self.outcome = Some(outcome);
        }
        self.outcome
            .as_ref()
            .expect("the cleanup outcome was stored above")
    }
    /// A failed scan is reported, not turned into "nothing to stop".
    fn scan_failure(&self, error: &anyhow::Error) -> SessionStop {
        tracing::warn!(
            "Could not stop the background tasks of subagent {}: {error:#}",
            self.session
        );
        SessionStop {
            scan_error: Some(format!("{error:#}")),
            ..SessionStop::default()
        }
    }
}
impl Drop for ChildSessionTasks {
    fn drop(&mut self) {
        // The child may have been cancelled or panicked; nothing remains to
        // await this stop, so it is requested synchronously.
        let _ = self.request();
    }
}

#[cfg(test)]
#[path = "delegation_task_cleanup_tests.rs"]
mod task_cleanup_tests;

#[cfg(test)]
#[path = "delegation_workspace_tests.rs"]
mod workspace_tests;

#[cfg(test)]
#[path = "delegation_resume_tests.rs"]
mod resume_tests;

#[cfg(test)]
#[path = "delegation_launch_tests.rs"]
mod launch_tests;

#[cfg(test)]
#[path = "team_spawn_tests.rs"]
mod team_spawn_tests;

#[cfg(test)]
#[path = "sdk_agent_fields_tests.rs"]
mod sdk_agent_fields_tests;

#[cfg(test)]
#[path = "delegation_fork_tests.rs"]
mod fork_tests;

#[cfg(test)]
#[test]
fn agent_input_maps_subagent_type_and_defaults_to_background_in_the_main_session() {
    let input = normalize_agent_input(
        &json!({"prompt":"p","description":"d","subagent_type":"general-purpose"}),
        true,
    );
    assert_eq!(input["agent_type"], "general");
    assert_eq!(input["run_in_background"], true);
    let child = normalize_agent_input(
        &json!({"prompt":"p","subagent_type":"reviewer","run_in_background":false}),
        true,
    );
    assert_eq!(
        (
            child["agent_type"].as_str(),
            child["run_in_background"].as_bool()
        ),
        (Some("reviewer"), Some(false))
    );
    assert!(normalize_agent_input(&json!({"prompt":"p"}), false)["run_in_background"].is_null());
}

#[cfg(test)]
#[test]
fn child_usage_accumulates_requests_without_counting_repeated_snapshots_or_parent_usage() {
    let temp = tempfile::tempdir().unwrap();
    let context = crate::services::session_messages::context(temp.path()).unwrap();
    let sessions = SessionStore::new(context.db.clone());
    let id = sessions.create(temp.path(), "fixture", "fixture").unwrap();
    let (event_tx, _) = tokio::sync::mpsc::channel(32);
    let mut state = ChildState {
        history: context.db.executor_history(&id).unwrap(),
        value: json!({"session_id":id,"status":"completed"}),
        event_tx,
        request_id: String::new(),
        request_base: 0,
        streamed_bytes: 0,
    };
    let request = |id: &str, tokens: Value| json!({"id":id,"number":1,"status":"running","outputTokens":tokens});
    state
        .snapshot(&request("r1", Value::Null), "assistant")
        .unwrap();
    state.usage(&json!({"output_tokens":0}));
    state.streamed("streaming answer");
    assert!(state.value["output_tokens"].as_u64().unwrap() > 0);
    assert_eq!(state.value["output_tokens_estimated"], true);
    state
        .snapshot(&request("r1", json!(100)), "assistant")
        .unwrap();
    state
        .snapshot(&request("r1", json!(100)), "assistant")
        .unwrap();
    state
        .snapshot(&request("r2", Value::Null), "assistant")
        .unwrap();
    state.usage(&json!({"completion_tokens":30}));
    assert_eq!(state.value["output_tokens"], 130);
    state
        .snapshot(&request("r2", json!(30)), "assistant")
        .unwrap();
    assert_eq!(state.value["output_tokens"], 130);
    assert_eq!(state.value["output_tokens_estimated"], false);
    let count = context
        .db
        .fixture_shard(&id, |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM model_requests WHERE conversation_id=?1",
                [&id],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(count, 2);
}

#[cfg(test)]
#[test]
fn a_stored_default_model_fills_an_agent_call_without_model() {
    let temp = tempfile::tempdir().unwrap();
    agents::set_model(temp.path(), "explore", Some("cheap-model")).unwrap();

    let mut input = json!({"prompt":"p","agent_type":"explore"});
    assert_eq!(
        fill_default_model(&mut input, temp.path()).as_deref(),
        Some("explore")
    );
    assert_eq!(input["model"], "cheap-model");

    // A blank model counts as omitted.
    let mut blank = json!({"prompt":"p","agent_type":"explore","model":"  "});
    assert_eq!(
        fill_default_model(&mut blank, temp.path()).as_deref(),
        Some("explore")
    );
    assert_eq!(blank["model"], "cheap-model");

    // A type without a stored default keeps the parent's model.
    let mut general = json!({"prompt":"p","agent_type":"general"});
    assert!(fill_default_model(&mut general, temp.path()).is_none());
    assert!(general["model"].is_null());

    // A fork always uses this session's model.
    let mut fork = json!({"prompt":"p","agent_type":"fork"});
    assert!(fill_default_model(&mut fork, temp.path()).is_none());
    assert!(fork["model"].is_null());
}

#[cfg(test)]
#[test]
fn an_explicit_model_wins_over_the_stored_default() {
    let temp = tempfile::tempdir().unwrap();
    agents::set_model(temp.path(), "explore", Some("cheap-model")).unwrap();
    let mut input = json!({"prompt":"p","agent_type":"explore","model":"chosen-model"});
    assert!(fill_default_model(&mut input, temp.path()).is_none());
    assert_eq!(input["model"], "chosen-model");
}
