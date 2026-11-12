use anyhow::Context;
use crate::cost_tracker::{BudgetCheckResult, CostTracker};
use crate::db::DbManager;
use crate::native_engine::provider_manager::ProviderManager;
use crate::native_engine::tool_loop::{EngineEvent, ToolLoopExecutor};
use crate::mcp::McpToolRegistry;
use crate::permissions::{PermissionContext, PermissionManager, PermissionResult, audit::{AuditAction, AuditEntry}};
use crate::prefetch::PrefetchEngine;
use crate::skills::SkillsManager;
use crate::cache::FileCache;
use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, oneshot};
use futures::FutureExt;
use uuid::Uuid;

#[path = "idle_children.rs"]
mod idle_children;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningMode {
    #[serde(alias = "quick")]
    Quick,
    #[serde(alias = "standard")]
    Standard,
    #[serde(alias = "deep")]
    Deep,
}

pub fn analyze_complexity(message: &str) -> ReasoningMode {
    let msg_lower = message.to_lowercase();
    let has_code = msg_lower.contains("function") || msg_lower.contains("class ") || msg_lower.contains("def ") || msg_lower.contains("import ") || msg_lower.contains("const ") || msg_lower.contains("let ") || msg_lower.contains("fn ");
    let has_file_op = msg_lower.contains("file") || msg_lower.contains("编辑") || msg_lower.contains("修改") || msg_lower.contains("删除") || msg_lower.contains("write") || msg_lower.contains("read") || msg_lower.contains("目录");
    let has_danger = msg_lower.contains("delete") || msg_lower.contains("rm ") || msg_lower.contains("drop ") || msg_lower.contains("删除") || msg_lower.contains("格式化") || msg_lower.contains("format");
    let is_long = message.len() > 200;

    if has_danger {
        ReasoningMode::Deep
    } else if has_code || has_file_op || is_long {
        ReasoningMode::Standard
    } else {
        ReasoningMode::Quick
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub conversation_id: String,
    pub messages: Vec<Value>,
    /// The compaction boundary the supplied messages were built from, resolved
    /// by the same read that produced them. `None` means the caller cannot
    /// prove a boundary, so status must treat the request as unattributed.
    #[serde(default)]
    pub context_boundary: Option<String>,
    pub model: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u32>,
    /// Per-request model/tool-loop budget. None keeps the configured default.
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub response_schema: Option<Value>,
    pub workspace_path: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub reasoning_mode: Option<ReasoningMode>,
}

#[derive(Debug)]
pub struct ActiveTurn {
    pub event_tx: mpsc::Sender<EngineEvent>,
    pub executor_handle: Option<tokio::task::JoinHandle<()>>,
    pub cancelled: bool,
}

/// Observed state of the executor attempt that ran a session's last turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorWait {
    /// The executor task - and everything it owns, including the execution
    /// lease - has ended.
    Completed,
    /// The deadline passed while the attempt was still running. The attempt
    /// still owns the session and no completion is established.
    Pending,
}

/// A single executor attempt. The task holds the sender half; waiters clone the
/// receiver half. The latest attempt for a session is kept in the map only so a
/// host can find it; the task removes its own entry when it ends.
struct ExecutorAttempt {
    id: u64,
    finished: tokio::sync::watch::Receiver<bool>,
}

/// Per-session completion bookkeeping, indexed by attempt identity so a stale
/// attempt can never remove or wait for a newer one under the same session.
#[derive(Default)]
struct ExecutorAttempts {
    entries: std::sync::Mutex<HashMap<String, ExecutorAttempt>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl ExecutorAttempts {
    fn begin(
        self: &Arc<Self>,
        session: &str,
        lease: Option<Arc<crate::services::session_messages::ExecutionLease>>,
    ) -> Arc<AttemptOwnership> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let (finished, receiver) = tokio::sync::watch::channel(false);
        let replaced = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                session.to_owned(),
                ExecutorAttempt {
                    id,
                    finished: receiver,
                },
            );
        if let Some(replaced) = replaced {
            tracing::warn!(
                module = "EngineCore",
                "Replacing unfinished executor attempt {} for {}",
                replaced.id,
                session
            );
        }
        Arc::new(AttemptOwnership {
            lease,
            attempts: self.clone(),
            session: session.to_owned(),
            id,
            finished: Some(finished),
        })
    }

    fn waiter(&self, session: &str) -> Option<ExecutorAttemptWaiter> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session)
            .map(|attempt| ExecutorAttemptWaiter {
                finished: attempt.finished.clone(),
            })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

/// Lives in the executor task itself, and is cloned into every mandatory
/// mutating tail job that can outlive that task. A started `spawn_blocking` job
/// cannot be aborted by dropping its handle, so such a job must keep the same
/// attempt ownership: only the last holder releases the lease and establishes
/// completion. Every exit path - normal return, cancellation/abort, panic or a
/// disconnected host - therefore stays consistent.
struct AttemptOwnership {
    /// The same lease the executor holds. Keeping a reference here means a tail
    /// job that outlives an aborted outer task still owns the session.
    lease: Option<Arc<crate::services::session_messages::ExecutionLease>>,
    attempts: Arc<ExecutorAttempts>,
    session: String,
    id: u64,
    finished: Option<tokio::sync::watch::Sender<bool>>,
}

impl Drop for AttemptOwnership {
    fn drop(&mut self) {
        // Release the session before completion is established, so observing
        // `Completed` always implies that a new attempt is admissible.
        drop(self.lease.take());
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(true);
        }
        // Only remove the entry this attempt registered. A newer attempt that
        // replaced it must survive an older attempt's cleanup.
        let mut entries = self
            .attempts
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries
            .get(&self.session)
            .is_some_and(|attempt| attempt.id == self.id)
        {
            entries.remove(&self.session);
        }
    }
}

struct ExecutorAttemptWaiter {
    finished: tokio::sync::watch::Receiver<bool>,
}

impl ExecutorAttemptWaiter {
    async fn wait(&mut self, timeout: std::time::Duration) -> ExecutorWait {
        if *self.finished.borrow() {
            return ExecutorWait::Completed;
        }
        match tokio::time::timeout(timeout, self.finished.changed()).await {
            // A change or a lost sender both mean the attempt is over: the
            // guard has been dropped and the lease released.
            Ok(_) => ExecutorWait::Completed,
            Err(_) if *self.finished.borrow() => ExecutorWait::Completed,
            Err(_) => ExecutorWait::Pending,
        }
    }
}

/// Test-only gate at the actual outer executor tail: the public `MessageStop`
/// has been emitted, while the executor, its lease and its cleanup task are
/// still alive. Tests use it to make the post-stop race deterministic.
#[cfg(test)]
pub(crate) struct ExecutorTailGate {
    pub(crate) entered: oneshot::Sender<()>,
    pub(crate) release: oneshot::Receiver<()>,
}

/// Test-only gate inside the mandatory assistant-persistence blocking job,
/// after it has started and before it mutates the database. A started
/// `spawn_blocking` job cannot be aborted by dropping its handle, so this is the
/// window in which a cancelled outer task can otherwise release the session
/// while the write is still in flight. The release half is a blocking channel
/// because the job runs on a blocking thread.
#[cfg(test)]
pub(crate) struct PersistenceGate {
    pub(crate) entered: oneshot::Sender<()>,
    pub(crate) release: std::sync::mpsc::Receiver<()>,
}

#[derive(Debug, Clone)]
pub struct ConversationState {
    pub conversation_id: String,
    pub model: String,
    pub messages: Vec<Value>,
    pub system_prompt: Option<String>,
    pub last_activity: String,
    pub turn_count: usize,
    pub status: ConversationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationStatus {
    Idle,
    Active,
    WaitingForUser,
    Completed,
    Error,
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub tool_use_id: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub output: String,
    pub is_error: bool,
    pub timestamp: String,
}

pub struct QueryEngine {
    owned_children: super::tool_loop::delegation::OwnedChildren,
    runtime_agents: std::sync::RwLock<HashMap<String, super::runtime_agents::Catalog>>,
    auto_manual_interactive: std::sync::atomic::AtomicBool,
    permission_host_available: std::sync::atomic::AtomicBool,
    local_goal_frontend: std::sync::atomic::AtomicBool,
    /// The active history's estimated tokens and the model window, snapshotted
    /// by an interactive host. `None` keeps CompactContext unavailable.
    context_usage: std::sync::Mutex<Option<(u64, u64)>>,
    bundled_resources: Arc<crate::skills::bundled_resources::Resources>,
    provider_manager: Arc<Mutex<ProviderManager>>,
    db_manager: Arc<DbManager>,
    active_turns: Arc<Mutex<HashMap<String, ActiveTurn>>>,
    /// Completion bookkeeping for the executor attempt that owns a session.
    /// Entries are created before the executor is spawned and removed by that
    /// task itself, so callers that never wait do not accumulate entries.
    executor_attempts: Arc<ExecutorAttempts>,
    #[cfg(test)]
    executor_tail_gate: Arc<std::sync::Mutex<Option<ExecutorTailGate>>>,
    #[cfg(test)]
    persistence_gate: Arc<std::sync::Mutex<Option<PersistenceGate>>>,
    conversation_states: Arc<Mutex<HashMap<String, ConversationState>>>,
    workspaces_dir: PathBuf,
    mcp_registry: Option<Arc<McpToolRegistry>>,
    answer_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    permission_mailbox: super::permission_response::Mailbox,
    tool_call_history: Arc<Mutex<HashMap<String, Vec<ToolCallRecord>>>>,
    permission_manager: Arc<PermissionManager>,
    skills_manager: Option<Arc<Mutex<SkillsManager>>>,
    file_cache: Arc<FileCache>,
    cost_tracker: Option<Arc<CostTracker>>,
}

impl QueryEngine {
    /// No tools, automatic memory ingestion, detached tasks, or conversation mutations.
    /// Dropping this future cancels the underlying HTTP/runtime request.
    pub async fn generate_text(
        &self,
        provider_id: &str,
        model: &str,
        system: &str,
        input: &str,
    ) -> Result<String> {
        self.generate_text_for_session(None, provider_id, model, system, input)
            .await
    }

    pub async fn generate_text_for_session(
        &self,
        session: Option<&str>,
        provider_id: &str,
        model: &str,
        system: &str,
        input: &str,
    ) -> Result<String> {
        let provider = self
            .provider_manager
            .lock()
            .await
            .resolve_selected(provider_id, model)
            .ok_or_else(|| anyhow!("Provider/model is unavailable: {provider_id}::{model}"))?;
        super::generation::generate_for_session(provider, system, input, session).await
    }

    /// Tool-free summarization that also accepts histories larger than the model window.
    pub async fn generate_summary(&self, session: Option<&str>, provider_id: &str, model: &str, system: &str, input: &str) -> Result<String> {
        let provider = self.provider_manager.lock().await.resolve_selected(provider_id, model)
            .ok_or_else(|| anyhow!("Provider/model is unavailable: {provider_id}::{model}"))?;
        let mut checkpoint = session.map(|id| crate::services::compaction::checkpoint::Checkpoint::open(&self.db_manager, id, &provider, system)).transpose()?;
        super::generation::summarize(provider, system, input, checkpoint.as_mut()).await
    }
    pub async fn generate_compaction(&self, session: &str, provider_id: &str, model: &str, system: &str, input: &str, instructions: &str) -> Result<crate::services::compaction::GeneratedSummary> {
        let provider = self.provider_manager.lock().await.resolve_selected(provider_id, model)
            .ok_or_else(|| anyhow!("Provider/model is unavailable: {provider_id}::{model}"))?;
        let history = crate::runtime::SessionStore::new(self.db_manager.clone()).model_history(session, &provider.provider.api_format)?;
        crate::services::compaction::native::validate_history(&history, &provider)?;
        if crate::services::compaction::native::supports(&provider) {
            return crate::services::compaction::native::generate(&self.db_manager, session, &provider, instructions).await;
        }
        self.generate_summary(Some(session), provider_id, model, system, input).await.map(Into::into)
    }
    pub fn new(
        provider_manager: Arc<Mutex<ProviderManager>>,
        db_manager: Arc<DbManager>,
        workspaces_dir: PathBuf,
        permission_manager: Arc<PermissionManager>,
        file_cache: Arc<FileCache>,
    ) -> Self {
        Self {
            auto_manual_interactive: std::sync::atomic::AtomicBool::new(false),
            permission_host_available: std::sync::atomic::AtomicBool::new(true),
            local_goal_frontend: std::sync::atomic::AtomicBool::new(false),
            context_usage: std::sync::Mutex::new(None),
            bundled_resources: Arc::default(),
            owned_children: Default::default(),
            runtime_agents: Default::default(),
            provider_manager,
            db_manager,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            executor_attempts: Arc::new(ExecutorAttempts::default()),
            #[cfg(test)]
            executor_tail_gate: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            persistence_gate: Arc::new(std::sync::Mutex::new(None)),
            conversation_states: Arc::new(Mutex::new(HashMap::new())),
            workspaces_dir,
            mcp_registry: None,
            answer_waiters: Arc::new(Mutex::new(HashMap::new())),
            permission_mailbox: Default::default(),
            tool_call_history: Arc::new(Mutex::new(HashMap::new())),
            permission_manager,
            skills_manager: None,
            file_cache,
            cost_tracker: None,
        }
    }

    pub fn with_mcp_registry(mut self, registry: Arc<McpToolRegistry>) -> Self {
        self.mcp_registry = Some(registry);
        self
    }

    pub fn with_skills_manager(mut self, manager: Arc<Mutex<SkillsManager>>) -> Self {
        self.skills_manager = Some(manager);
        self
    }

    pub fn with_cost_tracker(mut self, tracker: Arc<CostTracker>) -> Self {
        self.cost_tracker = Some(tracker);
        self
    }

    pub fn set_permission_host_available(&self, available: bool) {
        self.permission_host_available.store(available, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_auto_manual_interactive(&self, interactive: bool) {
        self.auto_manual_interactive.store(interactive, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn set_local_goal_frontend(&self, enabled: bool) {
        self.local_goal_frontend.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }
    /// Only an interactive host that can show the next prompt sets this.
    pub fn set_context_usage(&self, usage: Option<(u64, u64)>) {
        *self
            .context_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = usage;
    }

    pub async fn check_tool_permission(
        &self,
        tool_name: &str,
        tool_input: Value,
        conversation_id: &str,
        workspace_path: Option<String>,
    ) -> PermissionResult {
        let context = PermissionContext {
            mcp_read_only: false,
            tool_name: tool_name.to_string(),
            tool_input,
            conversation_id: conversation_id.to_string(),
            user_id: None,
            workspace_path,
        };
        self.permission_manager.check_permission(&context)
    }

    /// Apply an enabled skill to the normal conversation executor and stream real results.
    pub async fn execute_skill(&self, skill_id: &str, request: ChatRequest) -> Result<mpsc::Receiver<EngineEvent>> {
        let manager = self.skills_manager.as_ref().context("Skills manager not configured")?;
        let content = {
            let manager = manager.lock().await;
            let skill = manager.get_skill_by_id(skill_id).await?.filter(|s| s.enabled).context("Skill not found or disabled")?;
            skill.content.context("Skill has no content")?
        };
        // Kept after the user's message for the rest of the session.
        let root = self.provider_manager.lock().await.config_path().parent().context("Missing runtime directory")?.to_path_buf();
        let (session, messages) = (request.conversation_id.clone(), request.messages.clone());
        tokio::task::spawn_blocking(move || crate::services::turn_context::attach_to_last(&root, &session, &messages, vec![content])).await??;
        self.send_message(request).await
    }

    pub async fn confirm_tool_permission(&self, conversation_id: &str, tool_name: &str) {
        self.permission_manager.confirm_permission(conversation_id, tool_name);
    }

    pub async fn set_permission_mode(&self, mode: crate::permissions::PermissionMode) {
        self.permission_manager.set_mode(mode);
    }

    /// Legacy warm clients may initialize an idle engine, never alter a live executor.
    pub async fn warm_permission_mode(&self, mode: crate::permissions::PermissionMode) {
        let active = self.active_turns.lock().await;
        if active.is_empty() {
            self.permission_manager.set_mode(mode);
        }
    }

    pub async fn sync_providers(&self, providers: Vec<crate::native_engine::provider_manager::Provider>) {
        let mut pm = self.provider_manager.lock().await;
        let config_path = pm.config_path().clone();
        *pm = ProviderManager::from_providers(providers, config_path);
    }

    pub async fn resolve_provider(&self, model_id: &str) -> Option<crate::native_engine::provider_manager::ResolvedProvider> {
        let pm = self.provider_manager.lock().await;
        pm.resolve_provider(model_id).await
    }

    pub async fn get_conversation_state(&self, conv_id: &str) -> Option<ConversationState> {
        let states = self.conversation_states.lock().await;
        states.get(conv_id).cloned()
    }

    pub async fn update_conversation_state(&self, conv_id: &str, state: ConversationState) {
        let mut states = self.conversation_states.lock().await;
        states.insert(conv_id.to_string(), state);
    }

    pub async fn load_conversation_state(&self, conv_id: &str) -> Result<Option<ConversationState>> {
        let db = self.db_manager.clone();
        let conv_id_clone = conv_id.to_string();

        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ConversationState>> {
            db.with_conn(|conn| {
                let messages = crate::db::message_repo::get_messages_by_conversation(conn, &conv_id_clone)?;
                let conv = crate::db::conversation_repo::get_conversation(conn, &conv_id_clone)?;

                if messages.is_empty() && conv.is_none() {
                    return Ok(None);
                }

                let model = conv.as_ref().and_then(|c| c.model.as_deref()).unwrap_or("");

                let start = messages.iter().rposition(|msg| msg.is_compact_boundary).unwrap_or(0);
                let messages_json: Vec<Value> = messages.into_iter().skip(start).map(|msg| {
                    serde_json::json!({
                        "role": msg.role,
                        "content": msg.content,
                    })
                }).collect();

                Ok(Some(ConversationState {
                    conversation_id: conv_id_clone,
                    model: model.to_string(),
                    messages: messages_json,
                    system_prompt: None,
                    last_activity: Utc::now().to_rfc3339(),
                    turn_count: 0,
                    status: ConversationStatus::Idle,
                }))
            })?
        }).await??;

        if let Some(state) = &result {
            self.update_conversation_state(conv_id, state.clone()).await;
        }

        Ok(result)
    }

    pub async fn save_conversation_state(&self, conv_id: &str) -> Result<()> {
        let states = self.conversation_states.lock().await;
        if let Some(state) = states.get(conv_id) {
            let db = self.db_manager.clone();
            let state_clone = state.clone();

            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let result = db.with_conn(|conn| {
                    let tx = crate::db::write_transaction(&conn)?;

                    let existing = crate::db::conversation_repo::get_conversation(&tx, &state_clone.conversation_id)?;
                    if existing.is_none() {
                        crate::db::conversation_repo::insert_conversation(
                            &tx,
                            &state_clone.conversation_id,
                            None,
                            Some(&state_clone.model),
                            None,
                            None,
                            None,
                            false,
                            false,
                            false,
                            &state_clone.last_activity,
                            &state_clone.last_activity,
                            state_clone.turn_count as i64,
                        )?;
                    } else {
                        crate::db::conversation_repo::update_conversation(
                            &tx,
                            &state_clone.conversation_id,
                            None,
                            Some(&state_clone.model),
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some(&state_clone.last_activity),
                            Some(state_clone.turn_count as i64),
                        )?;
                    }

                    tx.commit()?;
                    Ok(())
                });
                result?
            }).await??;
        }
        Ok(())
    }

    pub async fn send_message(&self, request: ChatRequest) -> Result<mpsc::Receiver<EngineEvent>> {
        self.send_message_with_history(request, true).await
    }

    /// The desktop SSE route owns its assistant row so snapshots and results share one ID.
    pub async fn send_message_with_history(
        &self,
        request: ChatRequest,
        save_response: bool,
    ) -> Result<mpsc::Receiver<EngineEvent>> {
        self.send_with_origin(request, save_response, false, None).await
    }

    /// Internal hook feedback: no user prompt hooks, skill invocation or authority.
    pub async fn send_hook_wake(
        &self,
        request: ChatRequest,
        save_response: bool,
    ) -> Result<mpsc::Receiver<EngineEvent>> {
        self.send_with_origin(request, save_response, true, None).await
    }

    /// Share ownership acquired by a host for the whole turn. The engine holds
    /// a clone only while it executes, so the host keeps the lease across
    /// compaction, hooks and final persistence.
    pub async fn send_message_with_lease(
        &self,
        request: ChatRequest,
        save_response: bool,
        lease: std::sync::Arc<crate::services::session_messages::ExecutionLease>,
    ) -> Result<mpsc::Receiver<EngineEvent>> {
        self.send_with_origin(request, save_response, false, Some(lease)).await
    }

    /// Hook wakes share ownership the same way.
    pub async fn send_hook_wake_with_lease(
        &self,
        request: ChatRequest,
        save_response: bool,
        lease: std::sync::Arc<crate::services::session_messages::ExecutionLease>,
    ) -> Result<mpsc::Receiver<EngineEvent>> {
        self.send_with_origin(request, save_response, true, Some(lease)).await
    }

    async fn send_with_origin(
        &self,
        mut request: ChatRequest,
        save_response: bool,
        hook_wake: bool,
        execution_lease: Option<std::sync::Arc<crate::services::session_messages::ExecutionLease>>,
    ) -> Result<mpsc::Receiver<EngineEvent>> {
        // Every submission owns the session for its whole attempt. A host that
        // already acquired (CLI, SDK, Desktop SSE) shares that same `Arc`; an
        // independent caller is refused here, before any turn work.
        let execution_lease = match execution_lease {
            Some(lease) => Some(lease),
            None => {
                let root = self
                    .provider_manager
                    .lock()
                    .await
                    .config_path()
                    .parent()
                    .context("Missing runtime directory")?
                    .to_path_buf();
                crate::services::session_messages::ExecutionLease::acquire(
                    &root,
                    &request.conversation_id,
                )?
                .map(std::sync::Arc::new)
            }
        };
        crate::services::compaction::apply_boundary(
            &self.db_manager,
            &request.conversation_id,
            &mut request.messages,
        )?;
        let skill_invocation = request
            .messages
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .and_then(|m| m["content"].as_str())
            .and_then(|text| text.trim_start().strip_prefix('/'))
            .map(|text| {
                let mut parts = text.splitn(2, char::is_whitespace);
                (
                    parts.next().unwrap_or("").to_owned(),
                    parts.next().unwrap_or("").trim().to_owned(),
                )
            });
        let conv_id = request.conversation_id.clone();
        let plugin_root=self.provider_manager.lock().await.config_path().parent().context("Missing runtime directory")?.to_path_buf();
        let plugin_context=crate::services::ServiceContext{paths:crate::runtime::RuntimePaths::new(plugin_root)?,db:self.db_manager.clone(),credentials:Arc::new(crate::services::EnvironmentCredentials)};
        if let Some(router) = plugin_context.setting::<crate::performance::RouterConfig>("performance.router")? {
            // Route only a fresh plain-text turn; never switch models mid tool conversation.
            if router.enabled && request.provider_id.is_none() && request.messages.len() == 1 && request.messages[0]["role"] == "user"
                && request.messages[0]["content"].as_str().is_some_and(crate::performance::simple_turn) {
                let manager = self.provider_manager.lock().await;
                if manager.resolve_selected(&router.provider_id, &router.model_id).is_some() {
                    request.provider_id = Some(router.provider_id);
                    request.model = router.model_id;
                } else { anyhow::bail!("Configured simple-turn routing target is unavailable"); }
            }
        }
        let plugins=crate::services::plugins::list(&plugin_context)?;
        if plugins.iter().any(|p|p.enabled) {
            request.system_prompt.get_or_insert_with(String::new).push_str(&format!("\nAvailable PluginRun commands: {}\n{}",serde_json::to_string(&plugins.iter().filter(|p|p.enabled).collect::<Vec<_>>())?,crate::services::plugins::prompt_hooks(&plugin_context)?));
        }
        let agent = crate::services::agents::for_session(&self.db_manager, &conv_id)?;
        if let Some(definition) = &agent {
            request.system_prompt.get_or_insert_with(String::new).push_str(&format!("\nAgent role ({}):\n{}", definition.id, definition.system_prompt));
        }
        let db = self.db_manager.clone();
        let project_conv_id = conv_id.clone();
        let workspaces_root = self.workspaces_dir.clone();
        let requested_workspace = request.workspace_path.clone();
        let (workspace, project, prompt_profile) = tokio::task::spawn_blocking(move || db.with_conn(|conn| {
            let (workspace, project) = super::workspace::resolve_workspace(conn, &workspaces_root, &project_conv_id, requested_workspace.as_deref())?;
            let profile = super::workspace::conversation_prompt_profile(conn, &project_conv_id)?;
            Ok::<_, anyhow::Error>((workspace, project, profile))
        })).await???;
        crate::services::lifecycle_events::check_configuration(
            &plugin_context,
            &workspace,
            &conv_id,
        )
        .await?;
        let customization = crate::services::customization::load(&plugin_context.paths.data_dir)?;
        request.system_prompt = Some(crate::prompt::compose_customized(
            request.system_prompt.as_deref(), &request.model, prompt_profile, Some(&workspace), &customization.system_prompt,
        ));
        let snapshot_root = plugin_context.paths.data_dir.clone();
        let snapshot_session = conv_id.clone();
        let snapshot_workspace = workspace.clone();
        let git_snapshot = tokio::task::spawn_blocking(move || crate::services::git_snapshot::for_session(&snapshot_root, &snapshot_session, &snapshot_workspace)).await?;
        let workspace_path = workspace.to_string_lossy().into_owned();
        let mut discovered_skills = crate::services::plugins::skill_commands(&plugin_context)?;
        discovered_skills.extend(crate::skills::discovery::discover(
            &plugin_context.paths.data_dir,
            &workspace,
        )?);
        let mut pending_user_skill = None;
        // Blocks kept after this turn's user message for the rest of the session;
        // a skill's block also records the skill for after a compaction.
        let mut turn_blocks: Vec<(String, Option<crate::services::turn_context::InvokedSkill>)> = Vec::new();
        if let Some((command, arguments)) = skill_invocation.filter(|_| !hook_wake) {
            let content = if let Some(skill) = discovered_skills.get(&command) {
                let embedded =
                    skill.path.parent() == Some(std::path::Path::new("__embedded_cli_skills__"));
                let requires_execution = if embedded {
                    true
                } else {
                    let template = skill.render_template(&arguments, &conv_id)?;
                    skill.context.as_deref() == Some("fork") || crate::skills::invocation::parse(&template)?.iter().any(|part| matches!(part, crate::skills::invocation::Segment::Shell(_)))
                };
                if requires_execution {
                    pending_user_skill = Some(serde_json::json!({"skill":command,"args":arguments}));
                    None
                } else {
                    let content = skill.render(&arguments, &conv_id)?;
                    Some((content.clone(), crate::services::turn_context::InvokedSkill { name: command.clone(), path: skill.path.display().to_string(), content }))
                }
            } else if let Some(manager) = &self.skills_manager {
                let skills = manager.lock().await.list_enabled_skills().await?;
                let matches: Vec<_> = skills.into_iter().filter(|skill| skill.id == command || skill.name.to_lowercase().split_whitespace().collect::<Vec<_>>().join("-") == command).collect();
                anyhow::ensure!(matches.len() <= 1, "Ambiguous skill command; use its full skill ID");
                matches.into_iter().next().and_then(|skill| {
                    let content = format!("{}\n\nUser arguments: {arguments}", skill.content?);
                    Some((content.clone(), crate::services::turn_context::InvokedSkill { name: command.clone(), path: format!("skill:{}", skill.id), content }))
                })
            } else { None };
            // Hosts reject unknown slash commands; ordinary Desktop or multiline
            // messages may legitimately begin with a literal filesystem path.
            if let Some((content, skill)) = content { turn_blocks.push((content, Some(skill))); }
        }
        let available_skills = discovered_skills.values().filter(|skill| skill.model_invocable()).map(|skill| if skill.listing_name_only {
            serde_json::json!({"name":skill.name,"context":skill.context})
        } else {
            serde_json::json!({"name":skill.name,"description":skill.description,"context":skill.context})
        }).collect::<Vec<_>>();
        if !available_skills.is_empty() {
            request.system_prompt.get_or_insert_with(String::new).push_str(&format!("\nAvailable skills: invoke the Skill tool with skill=name and optional args. Do not bypass Skill by reading its template: fork isolation and approved shell context must run through the tool. {}\n", serde_json::to_string(&available_skills)?));
        }
        crate::tools::configure_lsp_servers(
            &workspace_path,
            crate::services::plugins::all_language_servers(&plugin_context)?,
        )?;
        let hook_provider = {
            let pm = self.provider_manager.lock().await;
            if let Some(id) = request.provider_id.as_deref() {
                pm.resolve_selected(id, &request.model)
            } else {
                pm.resolve_provider(&request.model).await
            }
        }
        .ok_or_else(|| anyhow!("No provider found for model: {}", request.model))?;
        crate::services::compaction::native::validate_history(&request.messages, &hook_provider)?;
        let hooks = crate::services::tool_hooks::HookRunner::load_with_plugins(
            &plugin_context,
            &workspace,
        )?
        .with_prompt_provider(hook_provider)?
        .with_session_callbacks(&conv_id);
        let prompt = request
            .messages
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| m["content"].clone())
            .unwrap_or(Value::Null);
        if !hook_wake {
            let hook_result = hooks
                .run(
                    crate::services::tool_hooks::HookEvent::UserPromptSubmit,
                    &conv_id,
                    serde_json::json!({"prompt":prompt}),
                )
                .await?;
            anyhow::ensure!(
                !hook_result.blocked,
                "UserPromptSubmit hook blocked request: {}",
                hook_result.reason
            );
            if !hook_result.additional_context.is_empty() {
                turn_blocks.push((format!(
                    "<system-reminder>\nUserPromptSubmit hook additional context: {}\n</system-reminder>",
                    hook_result.additional_context.join("\n")
                ), None));
            }
            for warning in hook_result.warnings {
                tracing::warn!("UserPromptSubmit hook: {warning}");
            }
        }
        request.workspace_path = Some(workspace_path.clone());
        let instructions_workspace = workspace.clone();
        let instructions_user_dir = plugin_context.paths.data_dir.clone();
        let instructions = tokio::task::spawn_blocking(move || {
            crate::project_instructions::load_all(Some(&instructions_user_dir), &instructions_workspace)
        })
        .await??;
        let instruction_files: Vec<_> = instructions.iter().map(|item| (item.path.clone(), item.sha256.clone())).collect();
        crate::services::lifecycle_events::instructions(
            &plugin_context,
            &hooks,
            &conv_id,
            &instruction_files,
        )
        .await?;
        // Like the reference CLI, instructions and the Git snapshot open the first
        // user message instead of the system prompt, which stays the same across
        // projects and so shares one cache entry.
        let turn_root = plugin_context.paths.data_dir.clone();
        let turn_session = conv_id.clone();
        let turn_messages = request.messages.clone();
        let turn_context = tokio::task::spawn_blocking(move || {
            for (block, skill) in turn_blocks {
                if let Some(entry) = crate::services::turn_context::for_last(&turn_messages, vec![block]) {
                    crate::services::turn_context::save(&turn_root, &turn_session, &crate::services::turn_context::Entry { skill, ..entry })?;
                }
            }
            crate::services::turn_context::load(&turn_root, &turn_session)
        })
        .await??;
        // Skills whose message a compaction removed follow, as in the reference CLI.
        let context_reminders: Vec<String> = crate::project_instructions::reminder(&instructions)
            .into_iter()
            .chain(git_snapshot.as_deref().map(crate::services::git_snapshot::reminder))
            .chain(crate::services::turn_context::invoked_skills_reminder(&turn_context, &request.messages))
            .collect();
        let allowed_directories = project
            .as_ref()
            .map(|project| {
                project
                    .folders
                    .iter()
                    .map(|folder| std::path::PathBuf::from(&folder.path))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(project) = project {
            let folders = project.folders.iter().map(|f| f.path.as_str()).collect::<Vec<_>>();
            let mut context = format!("\nProject: {}\nProject source folders (the first is the working directory):\n{}", project.name, serde_json::to_string(&folders)?);
            if let Some(instructions) = project.instructions.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
                context.push_str(&format!("\nProject instructions:\n{instructions}"));
            }
            request.system_prompt.get_or_insert_with(String::new).push_str(&context);
        }
        let team_context=crate::services::team::model_context(&plugin_context,&workspace)?;
        request.system_prompt.get_or_insert_with(String::new).push_str(&team_context);
        let model = request.model.clone();

        tracing::info!(module = "EngineCore", "send_message: conv_id={}, model={}, workspace={}", conv_id, model, workspace_path);

        if let Some(ref tracker) = self.cost_tracker {
            let estimated_tokens = request.messages.iter()
                .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
                .map(|c| (c.len() as u64) / 4)
                .sum::<u64>();
            let estimated_total = estimated_tokens + 2048;

            match tracker.check_budget(estimated_total) {
                BudgetCheckResult::Exceeded(msg) => {
                    tracing::warn!(module = "EngineCore", "Budget exceeded: {}", msg);
                    anyhow::bail!("Budget exceeded: {}", msg);
                }
                BudgetCheckResult::Warning(msg, usage, limit) => {
                    tracing::warn!(module = "EngineCore", "Budget warning: {}", msg);
                    let (warn_tx, warn_rx) = mpsc::channel::<EngineEvent>(10);
                    let _ = warn_tx.send(EngineEvent::BudgetWarning {
                        message: msg,
                        usage,
                        limit,
                    }).await;
                    let _ = warn_tx.send(EngineEvent::MessageStop {
                        full_text: String::new(),
                        stop_reason: Some("budget_warning".to_string()),
                    }).await;
                    return Ok(warn_rx);
                }
                BudgetCheckResult::WithinBudget => {}
            }
        }

        let reasoning_mode = request.reasoning_mode.clone().unwrap_or_else(|| {
            let first_user_msg = request.messages.iter()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            analyze_complexity(first_user_msg)
        });

        tracing::info!(module = "EngineCore", "Reasoning mode: {:?}", reasoning_mode);

        let mut provider = {
            let pm = self.provider_manager.lock().await;
            let resolved = if let Some(id) = request.provider_id.as_deref() { pm.resolve_selected(id, &model) } else { pm.resolve_provider(&model).await };
            tracing::info!(module = "EngineCore", "resolve_provider({}): {:?}", model, resolved.as_ref().map(|p| &p.provider.id));
            resolved.ok_or_else(|| anyhow!("No provider found for model: {}", model))?
        };

        let (event_tx, event_rx) = mpsc::channel::<EngineEvent>(500);
        let event_tx_for_turn = event_tx.clone();

        let state = ConversationState {
            conversation_id: conv_id.clone(),
            model: request.provider_id.as_ref().map(|id| format!("{}::{}", id, model)).unwrap_or_else(|| model.clone()),
            messages: request.messages.clone(),
            system_prompt: request.system_prompt.clone(),
            last_activity: Utc::now().to_rfc3339(),
            turn_count: 0,
            status: ConversationStatus::Active,
        };
        self.update_conversation_state(&conv_id, state).await;
        tracing::info!(module = "EngineCore", "Conversation state updated");

        if let Some(effort) = request.effort.as_deref() {
            if !["off", "low", "medium", "high", "xhigh", "max"].contains(&effort) { anyhow::bail!("Invalid effort"); }
        }
        provider.model.effort = request.effort.clone();
        validate_model_input(&provider.model, &request.messages)?;
        let configured_max = provider.model.max_tokens.unwrap_or(8192);
        let mut max_tokens = request.max_tokens.unwrap_or(configured_max).min(configured_max);
        if max_tokens == 0 { anyhow::bail!("max_tokens must be positive"); }
        if let Some(context) = provider.model.context_window {
            let estimated = crate::native_engine::token_counter::TokenCounter::estimate(&request.messages) as u64;
            if estimated + max_tokens as u64 > context as u64 {
                // Shrink the output budget to what the window still holds; only
                // fail when too little is left for a useful answer.
                let remaining = (context as u64).saturating_sub(estimated);
                if remaining < 4096 { anyhow::bail!("Input plus output budget exceeds the configured context window; compact the conversation or lower max output tokens"); }
                max_tokens = remaining as u32;
            }
        }

        let search_config_path = self.provider_manager.lock().await.config_path()
            .parent().unwrap_or(std::path::Path::new(".")).join("config.json");
        let learning = if plugin_context.setting::<bool>("learning.enabled")?.unwrap_or(false) {
            Some(crate::learning::TurnObservation::new(plugin_context.clone(), conv_id.clone()))
        } else { None };
        let mut executor = ToolLoopExecutor::new(
            provider,
            request.messages,
            request.system_prompt,
            max_tokens,
            event_tx.clone(),
            workspace_path.clone(),
            reasoning_mode.clone(),
        )
        .with_owned_children(self.owned_children.clone())
        .with_runtime_agents(
            self.runtime_agents
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&conv_id)
                .cloned()
                .unwrap_or_default(),
        )
        .with_allowed_directories(allowed_directories)
        .with_hook_wake(hook_wake)
        .with_user_skill(pending_user_skill)
        .with_max_turns(request.max_turns)?
        .with_response_schema(request.response_schema)?
        .with_learning_tools(learning.as_ref().map(|l| l.tools.clone()))
        .with_hooks(hooks)
        .with_goal_state(self.db_manager.clone())
        .with_context_boundary(request.context_boundary)
        .with_allowed_tools(agent.as_ref().and_then(|d|d.tools.clone()))
        .with_read_only_shell(agent.as_ref().is_some_and(|d|d.read_only_shell))
        .with_prompt_profile(prompt_profile)
        .with_context_reminders(context_reminders)
        .with_turn_context(turn_context)
        .with_memex(crate::memory::MemExClient::with_path(
            self.workspaces_dir.parent().unwrap_or(&self.workspaces_dir).join("memory.sqlite3"), None,
        ))
        .with_bundled_resources(self.bundled_resources.clone())
        .with_search_config_path(search_config_path)
        .with_conv_id(conv_id.clone())
        .with_answer_waiters(self.answer_waiters.clone())
        .with_permission_mailbox(self.permission_mailbox.clone())
        .with_permission_manager(self.permission_manager.clone())
        .with_auto_manual_interactive(self.auto_manual_interactive.load(std::sync::atomic::Ordering::Relaxed))
        .with_permission_host(self.permission_host_available.load(std::sync::atomic::Ordering::Relaxed))
        .with_local_goal_frontend(self.local_goal_frontend.load(std::sync::atomic::Ordering::Relaxed))
        .with_context_usage(
            *self
                .context_usage
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
        .with_prefetch_engine(Arc::new(PrefetchEngine::new(self.file_cache.clone(), workspace_path.clone())));

        if let Some(ref registry) = self.mcp_registry {
            executor = executor.with_mcp_registry(registry.clone());
        }
        // Ownership stays with the executor for its entire task lifetime,
        // including the cleanup after the public MessageStop. The host shares
        // this same lease; the last remaining owner performs the explicit
        // unlock. The engine must not drop its reference early.
        executor.execution_lease = execution_lease;
        executor.share_owned_child_budget();

        let conv_id_clone = conv_id.clone();
        let active_turns_clone = self.active_turns.clone();
        let conversation_states_clone = self.conversation_states.clone();
        let _tool_call_history_clone = self.tool_call_history.clone();
        let db = self.db_manager.clone();
        let provider_manager_title = self.provider_manager.clone();
        let db_title = self.db_manager.clone();

        tracing::info!(module = "EngineCore", "Spawning executor task for conv_id={}", conv_id_clone);

        // Registered before the task starts, so a host that waits immediately
        // after this call always finds this attempt. The task removes its own
        // entry, so callers that never wait leave nothing behind.
        let completion = self
            .executor_attempts
            .begin(&conv_id_clone, executor.execution_lease.clone());
        #[cfg(test)]
        let executor_tail_gate = self.executor_tail_gate.clone();
        #[cfg(test)]
        let persistence_gate = self.persistence_gate.clone();

        let executor_handle = tokio::spawn(async move {
            // Declared before the executor so the executor - and its lease -
            // is dropped before completion is signalled. A mandatory tail job
            // that outlives an abort keeps its own clone and defers both until
            // it really ends.
            let attempt_ownership = completion;
            let mut executor = executor;
            let mut learning = learning;
            tracing::info!(module = "EngineCore", "Executor task STARTED for {}", conv_id_clone);

            // 使用 catch_unwind 捕获 panic
            let exec_result = tokio::select! {
                biased;
                // A disconnected host must win over callback failures becoming ready
                // during SDK cleanup; those failures normally permit continuation.
                _ = event_tx.closed() => Ok(Err(anyhow!("Response receiver disconnected; turn cancelled"))),
                result = std::panic::AssertUnwindSafe(Box::pin(executor.execute())).catch_unwind() => result,
            };

            let exec_result = match exec_result {
                Ok(result) => result,
                Err(panic_info) => {
                    let panic_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = panic_info.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "Unknown panic".to_string()
                    };
                    tracing::error!(module = "EngineCore", "Executor PANIC for {}: {}", conv_id_clone, panic_msg);
                    let _ = event_tx.send(EngineEvent::Error(format!("Engine panic: {}", panic_msg))).await;
                    let _ = event_tx.send(EngineEvent::MessageStop {
                        full_text: String::new(),
                        stop_reason: Some("panic".to_string()),
                    }).await;
                    return;
                }
            };

            // Test-only barrier at the real outer tail: the public
            // `MessageStop` has already been emitted and the executor still
            // owns the session until this task ends.
            #[cfg(test)]
            {
                let gate = executor_tail_gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(gate) = gate {
                    let ExecutorTailGate { entered, release } = gate;
                    let _ = entered.send(());
                    let _ = release.await;
                }
            }

            let successful_turn = matches!(&exec_result, Ok((_, reason)) if !matches!(reason.as_deref(), Some("stream_error" | "error" | "interrupted" | "failed" | "cancelled" | "max_iterations" | "max_tokens" | "length" | "incomplete")));
            if let Some(observation) = learning.as_mut() {
                observation.complete(successful_turn);
            }
            drop(learning);
            match exec_result {
                Ok((full_text, _stop_reason)) => {
                    tracing::info!(module = "EngineCore", "Executor completed for {}, full_text_len={}", conv_id_clone, full_text.len());
                    if save_response && !full_text.is_empty() {
                        let conv_id = conv_id_clone.clone();
                        // This blocking write cannot be aborted once started, so
                        // it keeps the attempt's ownership until it exits: the
                        // session stays admitted and completion stays pending
                        // even if the outer task is cancelled meanwhile.
                        let ownership = attempt_ownership.clone();
                        #[cfg(test)]
                        let persistence_gate = persistence_gate.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ownership = ownership;
                            #[cfg(test)]
                            if let Some(gate) = persistence_gate
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .take()
                            {
                                let PersistenceGate { entered, release } = gate;
                                let _ = entered.send(());
                                // Blocking thread: wait for the harness without
                                // holding anything the async side needs.
                                let _ = release.recv();
                            }
                            db.with_conn(|conn| {
                                let msg_id = Uuid::new_v4().to_string();
                                let now = Utc::now().to_rfc3339();
                                let sort_order = crate::db::message_repo::get_messages_by_conversation(conn, &conv_id)
                                    .unwrap_or_default()
                                    .len() as i64;
                                crate::db::message_repo::insert_message(
                                    conn, &msg_id, &conv_id, "assistant", &full_text, None, &now, false, sort_order,
                                )?;
                                crate::db::conversation_repo::increment_message_count(conn, &conv_id)?;
                                Ok::<(), anyhow::Error>(())
                            })
                        }).await;
                    }
                }
                Err(e) => {
                    tracing::error!(module = "QueryEngine", "Error in turn for {}: {}", conv_id_clone, crate::performance::request_diagnostic(&e));
                    // Note: execute() already sends EngineEvent::Error + MessageStop to event_tx,
                    // so the frontend will receive a proper error message instead of channel closed.
                    let mut states = conversation_states_clone.lock().await;
                    if let Some(state) = states.get_mut(&conv_id_clone) {
                        state.status = ConversationStatus::Error;
                        state.last_activity = Utc::now().to_rfc3339();
                    }
                }
            }

            let mut turns = active_turns_clone.lock().await;
            if turns.get(&conv_id_clone).is_some_and(|turn| !turn.event_tx.same_channel(&event_tx)) { return; }
            if let Some(turn) = turns.get_mut(&conv_id_clone) {
                turn.cancelled = true;
            }
            turns.remove(&conv_id_clone);

            let mut states = conversation_states_clone.lock().await;
            if let Some(state) = states.get_mut(&conv_id_clone) {
                state.status = if successful_turn { ConversationStatus::Completed } else { ConversationStatus::Error };
                state.last_activity = Utc::now().to_rfc3339();
                let current_turn_count = state.turn_count;
                state.turn_count += 1;

                if successful_turn && current_turn_count == 0 {
                    let title_conv_id = conv_id_clone.clone();
                    let title_model = state.model.clone();
                    let pm = provider_manager_title.clone();
                    let db_t = db_title.clone();
                    let title_identity = super::request_identity::TurnIdentity::new(Some(title_conv_id.clone()), None);
                    tokio::spawn(title_identity.scope(async move {
                        tracing::info!(module = "EngineCore", "Auto-generating title for conv_id={}", title_conv_id);
                        let title_conv_id_inner = title_conv_id.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            db_t.with_conn(|conn| {
                                let conv = crate::db::conversation_repo::get_conversation(conn, &title_conv_id_inner)?;
                                if conv.as_ref().and_then(|c| c.title.as_ref()).map_or(false, |t| !t.is_empty()) {
                                    return Ok::<bool, anyhow::Error>(false);
                                }
                                let messages = crate::db::message_repo::get_messages_by_conversation(conn, &title_conv_id_inner)?;
                                let _first_user_msg = messages.iter().find(|m| m.role == "user").map(|m| m.content.clone());
                                let _first_assistant_msg = messages.iter().find(|m| m.role == "assistant").map(|m| m.content.clone());
                                Ok::<bool, anyhow::Error>(true)
                            })
                        }).await;

                        let should_generate = match result {
                            Ok(Ok(Ok(true))) => true,
                            _ => false,
                        };

                        if should_generate {
                            let resolved = {
                                let pm_guard = pm.lock().await;
                                pm_guard.resolve_provider(&title_model).await
                            };

                            if let Some(resolved) = resolved {
                                let db_msgs = db_title.clone();
                                let conv_id_for_msgs = title_conv_id.clone();
                                let msgs_result = tokio::task::spawn_blocking(move || {
                                    db_msgs.with_conn(|conn| {
                                        let messages = crate::db::message_repo::get_messages_by_conversation(conn, &conv_id_for_msgs)?;
                                        let first_user = messages.iter().find(|m| m.role == "user").map(|m| m.content.clone());
                                        let first_assistant = messages.iter().find(|m| m.role == "assistant").map(|m| m.content.clone());
                                        Ok::<_, anyhow::Error>((first_user, first_assistant))
                                    })
                                }).await;

                                if let Ok(Ok(Ok((Some(user_msg), assistant_msg)))) = msgs_result {
                                    let assistant_text: String = assistant_msg.unwrap_or_default();
                                    let truncated_assistant = if assistant_text.chars().count() > 200 {
                                        let mut end = 0;
                                        for (i, _) in assistant_text.char_indices() {
                                            if i >= 200 { break; }
                                            end = i;
                                        }
                                        assistant_text[..end].to_string()
                                    } else {
                                        assistant_text
                                    };

                                    let prompt = format!(
                                        "Based on the following conversation, generate a concise title (max 50 characters, in the same language as the user's message). Only output the title, nothing else.\n\nUser: {}\nAssistant: {}",
                                        user_msg, truncated_assistant
                                    );

                                    let user_msg_ref: &str = &user_msg;
                                    let fallback: String = if user_msg_ref.chars().count() > 30 {
                                        let mut end = 0;
                                        for (i, _) in user_msg_ref.char_indices() {
                                            if i >= 30 { break; }
                                            end = i;
                                        }
                                        user_msg_ref[..end].to_string()
                                    } else {
                                        user_msg.clone()
                                    };

                                    let title_result: Result<String, anyhow::Error> = match resolved.provider.api_format {
                                        crate::native_engine::provider_manager::ApiFormat::OpenAI => {
                                            let client = crate::native_engine::openai_client::OpenAIClient::new()
                                                .with_request_options(super::request_options::RequestOptions::for_session(Some(&title_conv_id)));
                                            let msgs = vec![
                                                crate::native_engine::openai_client::OpenAIMessage {
                                                    role: "user".to_string(),
                                                    content: crate::native_engine::openai_client::OpenAIContent::Text(prompt),
                                                    tool_calls: None,
                                                    tool_call_id: None,
                                                    reasoning_content: None, provider_content: None,
                                                }
                                            ];
                                            match client.send_message(&resolved, msgs, None, vec![], 100).await {
                                                Ok(resp) => {
                                                    let t = resp.choices.first()
                                                        .map(|c| match &c.message.content {
                                                            crate::native_engine::openai_client::OpenAIContent::Text(t) => t.trim().to_string(),
                                                            crate::native_engine::openai_client::OpenAIContent::Multi(parts) => parts.iter()
                                                                .filter_map(|p| if let crate::native_engine::openai_client::OpenAIContentPart::Text { text } = p { Some(text.as_str()) } else { None })
                                                                .collect::<Vec<_>>()
                                                                .join(""),
                                                        })
                                                        .unwrap_or_default();
                                                    Ok(t)
                                                }
                                                Err(e) => Err(e),
                                            }
                                        }
                                        crate::native_engine::provider_manager::ApiFormat::Anthropic => {
                                            let client = crate::native_engine::anthropic_client::AnthropicClient::new()
                                                .with_request_options(super::request_options::RequestOptions::for_session(Some(&title_conv_id)));
                                            let msgs = vec![
                                                crate::native_engine::anthropic_client::AnthropicMessage {
                                                    role: "user".to_string(),
                                                    content: crate::native_engine::anthropic_client::AnthropicContent::Text(prompt),
                                                }
                                            ];
                                            match client.send_message(&resolved, msgs, None, vec![], 100).await {
                                                Ok(resp) => {
                                                    let t = resp.content.iter()
                                                        .filter_map(|block| if let crate::native_engine::anthropic_client::ContentBlock::Text { text } = block { Some(text.as_str()) } else { None })
                                                        .collect::<Vec<_>>()
                                                        .join("")
                                                        .trim()
                                                        .to_string();
                                                    Ok(t)
                                                }
                                                Err(e) => Err(e),
                                            }
                                        }
                                    };

                                    let final_title = match title_result {
                                        Ok(t) if !t.is_empty() => t,
                                        _ => fallback,
                                    };

                                    let db_save = db_title.clone();
                                    let conv_id_save = title_conv_id.clone();
                                    let title_save = final_title.clone();
                                    let _ = tokio::task::spawn_blocking(move || {
                                        db_save.with_conn(|conn| {
                                            crate::db::conversation_repo::update_title_if_empty(conn, &conv_id_save, &title_save)
                                        })
                                    }).await;
                                    tracing::info!(module = "EngineCore", "Title generated for {}: {}", title_conv_id, final_title);
                                }
                            }
                        }
                    }));
                }
            }
        });

        {
            let mut turns = self.active_turns.lock().await;
            turns.insert(conv_id.clone(), ActiveTurn {
                event_tx: event_tx_for_turn,
                executor_handle: Some(executor_handle),
                cancelled: false,
            });
        }

        Ok(event_rx)
    }

    /// Join a completed turn without cancelling background hooks it launched.
    pub async fn finish_turn(&self, conv_id: &str) {
        let turn = self.active_turns.lock().await.remove(conv_id);
        if let Some(mut turn) = turn {
            if let Some(handle) = turn.executor_handle.take() {
                let _ = handle.await;
            }
        }
    }

    /// Observe whether the executor attempt that produced the last event has
    /// fully stopped. Public completion (MessageStop) is emitted from inside
    /// the task, so a host must not treat the turn as fully released before
    /// this reports [`ExecutorWait::Completed`].
    ///
    /// A deadline never establishes completion: a still-running attempt is
    /// reported as [`ExecutorWait::Pending`], keeps its lease and therefore
    /// keeps refusing a new same-session attempt until it really ends.
    pub async fn wait_for_executor(
        &self,
        conv_id: &str,
        timeout: std::time::Duration,
    ) -> ExecutorWait {
        match self.executor_attempts.waiter(conv_id) {
            Some(mut waiter) => waiter.wait(timeout).await,
            // No registered attempt means nothing of this session is running.
            None => ExecutorWait::Completed,
        }
    }

    #[cfg(test)]
    pub(crate) fn install_executor_tail_gate(&self, gate: ExecutorTailGate) {
        *self
            .executor_tail_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(gate);
    }

    #[cfg(test)]
    pub(crate) fn install_persistence_gate(&self, gate: PersistenceGate) {
        *self
            .persistence_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(gate);
    }

    #[cfg(test)]
    pub(crate) fn executor_attempt_count(&self) -> usize {
        self.executor_attempts.len()
    }

    /// Cancels the turn for `conv_id` and waits for the executor task to stop.
    ///
    /// Aborting the task does not stop a mandatory tail job that already runs on
    /// the blocking pool (for example the `save_response` assistant write): that
    /// job keeps the attempt's lease and completion until it exits. Admission is
    /// therefore still refused and [`Self::wait_for_executor`] reports
    /// [`ExecutorWait::Pending`] until the job really ends. Running blocking
    /// code is not promised to be interruptible.
    pub async fn cancel_turn(&self, conv_id: &str) {
        let turn = self.active_turns.lock().await.remove(conv_id);
        if let Some(mut turn) = turn {
            if let Some(handle) = turn.executor_handle.take() {
                handle.abort();
                let _ = handle.await;
            }
        }
        crate::services::async_hooks::cancel(conv_id).await;
        self.answer_waiters.lock().await.retain(|_, sender| !sender.is_closed());
        self.permission_mailbox.lock().await.retain(|_, sender| !sender.is_closed());

        let mut states = self.conversation_states.lock().await;
        if let Some(state) = states.get_mut(conv_id) {
            state.status = ConversationStatus::Idle;
            state.last_activity = Utc::now().to_rfc3339();
        }
    }

    pub fn set_runtime_agents(&self, session: &str, agents: super::runtime_agents::Catalog) {
        self.runtime_agents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.to_owned(), agents);
    }

    pub fn clear_runtime_agents(&self, session: &str) {
        self.runtime_agents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session);
        // Retained executors must not keep a run-local catalog usable after SDK exit.
        self.owned_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, child| child.parent != session);
    }

    pub async fn resume_with_permission(&self, request_id: &str, response: super::permission_response::PermissionResponse) -> Result<()> {
        super::permission_response::respond(&self.permission_mailbox, request_id, response).await
    }

    pub async fn permission_completion(&self, request_id: &str) -> tokio::sync::watch::Receiver<bool> {
        super::permission_response::completion(&self.permission_mailbox, request_id).await
    }

    pub async fn pending_questions(&self, session: &str) -> Vec<Value> {
        let waiters = self.answer_waiters.lock().await;
        super::permission_response::pending_owners(session).into_iter()
            .filter(|(id, _)| waiters.get(id).is_some_and(|sender| !sender.is_closed()))
            .map(|(id, owner)| serde_json::json!({"request_id":id,"session_id":owner.session_id,
                "tool_use_id":owner.tool_use_id,"kind":owner.kind,"tool_input":owner.input,
                "planApprovalRequest":owner.plan})).collect()
    }

    /// Scope Desktop responses to the emitting session, including delegated children.
    /// Validate first; malformed or stale submissions must not consume a live waiter.
    pub async fn resume_scoped_answer(&self, session: &str, request_id: &str, tool_use_id: &str, answer: String, permission: bool) -> Result<Value> {
        let owner = super::permission_response::request_owner(request_id)
            .ok_or_else(|| anyhow!("Request expired or already completed"))?;
        anyhow::ensure!(owner.session_id == session && owner.tool_use_id == tool_use_id,
            "Request session or tool ownership mismatch");
        anyhow::ensure!((owner.kind == "permission") == permission, "Incorrect response endpoint for request");
        if let Some(request) = &owner.plan {
            let manager = owner.manager.as_ref().ok_or_else(|| anyhow!("Plan authority unavailable"))?;
            let root = owner.root.as_ref().ok_or_else(|| anyhow!("Plan storage unavailable"))?;
            let context = crate::services::session_messages::context(root)?;
            let bypass = crate::services::session_permission_mode::plan_returns_to_bypass(&context.db, session, manager)?;
            crate::services::plan_approval::parse_response(&answer, request, bypass, manager.bypass_available())?;
            anyhow::ensure!(manager.get_mode() == crate::permissions::PermissionMode::PlanMode, "Plan mode changed during review");
            anyhow::ensure!(crate::services::plans::read(root, session)?.as_deref() == Some(request.plan.as_str()), "Plan changed during review; request approval again");
        }
        let mut outcome = owner.outcome.subscribe();
        if permission {
            let response = if answer == "allow_session" {
                anyhow::ensure!(super::permission_response::can_allow_bash_session(owner.tool_name.as_deref().unwrap_or_default(), &owner.input), "Session Bash approval is unavailable for this request");
                super::permission_response::PermissionResponse::AllowBashForSession { updated_input: None, note: None }
            } else { super::permission_response::PermissionResponse::Text(answer) };
            super::permission_response::respond(&self.permission_mailbox, request_id, response).await?;
            return Ok(serde_json::json!({"ok":true,"status":"submitted"}));
        }
        {
            let mut waiters = self.answer_waiters.lock().await;
            anyhow::ensure!(waiters.get(request_id).is_some_and(|sender| !sender.is_closed()), "Request expired or already completed");
            waiters.remove(request_id).unwrap().send(answer).map_err(|_| anyhow!("Request cancelled before delivery"))?;
        }
        if owner.plan.is_some() {
            loop {
                if let Some(result) = outcome.borrow().clone() {
                    let result = result.map_err(|error| anyhow!(error))?;
                    return Ok(serde_json::json!({"ok":true,"status":"applied","result":result}));
                }
                outcome.changed().await.map_err(|_| anyhow!("Plan approval cancelled before commit"))?;
            }
        }
        Ok(serde_json::json!({"ok":true,"status":"submitted"}))
    }

    pub async fn resume_with_answer(&self, conv_id: &str, answer: String) -> Result<()> {
        if self.permission_mailbox.lock().await.contains_key(conv_id) {
            return super::permission_response::respond(&self.permission_mailbox, conv_id,
                super::permission_response::PermissionResponse::Text(answer)).await;
        }
        let mut waiters = self.answer_waiters.lock().await;
        if let Some(tx) = waiters.remove(conv_id) {
            tx.send(answer).map_err(|_| anyhow!("Failed to send answer: receiver already dropped"))?;
            Ok(())
        } else {
            anyhow::bail!("No pending AskUserQuestion for conversation {}", conv_id)
        }
    }

    pub async fn record_tool_call(
        &self,
        conv_id: &str,
        tool_name: &str,
        tool_input: &Value,
        output: &str,
        is_error: bool,
        message_id: Option<&str>,
    ) {
        let start = Instant::now();
        let elapsed = start.elapsed();

        let record = ToolCallRecord {
            tool_use_id: message_id.unwrap_or("").to_string(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            output: output.to_string(),
            is_error,
            timestamp: Utc::now().to_rfc3339(),
        };

        let mut history = self.tool_call_history.lock().await;
        history.entry(conv_id.to_string())
            .or_insert_with(Vec::new)
            .push(record);

        let _audit_entry = AuditEntry::new(
            if is_error { AuditAction::ToolCancelled } else { AuditAction::ToolExecuted },
            tool_name,
            conv_id,
        )
        .with_result(if is_error { "Error" } else { "Success" })
        .with_details(serde_json::json!({
            "input": tool_input,
            "output": output,
            "is_error": is_error,
            "duration_ms": elapsed.as_millis(),
            "message_id": message_id,
        }));

        tracing::info!(
            module = "Audit",
            "Tool: {} | Conv: {} | Error: {} | Duration: {}ms",
            tool_name, conv_id, is_error, elapsed.as_millis()
        );
    }

    pub async fn get_tool_call_history(&self, conv_id: &str) -> Vec<ToolCallRecord> {
        let history = self.tool_call_history.lock().await;
        history.get(conv_id).cloned().unwrap_or_default()
    }

    pub fn get_workspaces_dir(&self) -> &PathBuf {
        &self.workspaces_dir
    }

    pub async fn list_active_conversations(&self) -> Vec<String> {
        let states = self.conversation_states.lock().await;
        states.values()
            .filter(|s| s.status == ConversationStatus::Active)
            .map(|s| s.conversation_id.clone())
            .collect()
    }

    pub async fn cleanup_inactive_conversations(&self, max_idle_minutes: u64) -> usize {
        let now = Utc::now();
        let mut removed_count = 0;

        let mut states = self.conversation_states.lock().await;
        let inactive_ids: Vec<String> = states.iter()
            .filter(|(_, s)| {
                if let Ok(last_activity) = chrono::DateTime::parse_from_rfc3339(&s.last_activity) {
                    let duration = now.signed_duration_since(last_activity);
                    duration.num_minutes() > max_idle_minutes as i64
                } else {
                    true
                }
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in inactive_ids {
            states.remove(&id);
            removed_count += 1;
        }

        removed_count
    }
}

#[cfg(test)]
#[path = "scoped_answer_tests.rs"]
mod scoped_answer_tests;

#[cfg(test)]
#[path = "executor_lifetime_tests.rs"]
mod executor_lifetime_tests;

pub type NativeEngine = QueryEngine;

fn validate_model_input(model: &crate::native_engine::provider_manager::ModelConfig, messages: &[Value]) -> Result<()> {
    for message in messages.iter().filter(|m| m["role"] == "user") {
        let content = &message["content"];
        if content.as_str().is_some_and(|s| !s.is_empty()) && !model.supports_text { anyhow::bail!("Selected model does not accept Text input"); }
        if let Some(parts) = content.as_array() {
            for part in parts {
                match part["type"].as_str().unwrap_or("") {
                    "text" | "input_text" if !model.supports_text => anyhow::bail!("Selected model does not accept Text input"),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod provider_input_tests {
    use super::*;
    #[test]
    fn provider_input_capabilities_are_enforced() {
        let mut model: crate::native_engine::provider_manager::ModelConfig = serde_json::from_value(serde_json::json!({"id":"m","name":"M","enabled":true,"max_tokens":32000,"supports_vision":false,"supports_web_search":false})).unwrap();
        assert!(validate_model_input(&model,&[serde_json::json!({"role":"user","content":"hello"})]).is_ok());
        // Stale capability metadata must not prevent the endpoint from deciding.
        for kind in ["image", "image_url", "input_image"] {
            assert!(validate_model_input(&model,&[serde_json::json!({"role":"user","content":[{"type":kind,"image_url":{"url":"data:image/png;base64,AA=="}}]})]).is_ok());
        }
        model.supports_text = false;
        assert!(validate_model_input(&model,&[serde_json::json!({"role":"user","content":"hello"})]).is_err());
    }
}
