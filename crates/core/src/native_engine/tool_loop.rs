#[path = "tool_approval.rs"]
mod tool_approval;
#[cfg(test)]
#[path = "auto_permission_tests.rs"]
mod auto_permission_tests;
#[cfg(test)]
#[path = "permission_denied_tests.rs"]
mod permission_denied_tests;
#[path = "delegation.rs"]
pub(crate) mod delegation;
#[path = "deep_research.rs"]
mod deep_research;
#[cfg(test)]
#[path = "hook_lifecycle_tests.rs"]
mod hook_lifecycle_tests;
#[path = "skill_tool.rs"]
mod skill_tool;
#[path = "plan_tool.rs"]
mod plan_tool;

use crate::native_engine::anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage, ContentBlock};
use crate::native_engine::engine_core::ReasoningMode;
use crate::native_engine::openai_client::{ImageUrl, OpenAIClient, OpenAIContent, OpenAIContentPart, OpenAIMessage};
use crate::native_engine::provider_manager::{ApiFormat, ResolvedProvider};
use crate::native_engine::token_counter::TokenCounter;
use crate::native_engine::context_compressor::{ContextCompressor, ContextWindow, CompressionLevel};
use crate::native_engine::request_trace::RequestTrace;
use crate::permissions::{PermissionManager, PermissionResult};
use crate::prefetch::PrefetchEngine;
use crate::streaming::sse_parser::{consume_sse_payloads, merge_tool_args, recover_malformed_tool_input};
use crate::tools::get_tool_definitions;
use crate::prompt::{PromptProfile, code::{adapt_tools, is_memory_tool}};
use crate::mcp::McpToolRegistry;
use super::mcp_output::McpOutput;
use crate::memory::{ContextManager, MemExClient, TieredCompressor};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, Mutex, oneshot};
use tokio::time::{timeout, Duration};

#[cfg(test)]
#[path = "search_integration_tests.rs"]
mod search_integration_tests;

#[cfg(test)]
#[path = "code_prompt_tests.rs"]
mod code_prompt_tests;

#[cfg(test)]
#[path = "tool_hooks_tests.rs"]
mod tool_hooks_tests;

#[cfg(test)]
#[path = "builtin_failure_tests.rs"]
mod builtin_failure_tests;

#[cfg(test)]
#[path = "turn_budget_tests.rs"]
mod turn_budget_tests;

#[cfg(test)]
#[path = "durable_input_tests.rs"]
mod durable_input_tests;
#[cfg(test)]
#[path = "goal_proposal_tests.rs"]
mod goal_proposal_tests;

#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A message the user sent while the turn ran, now given to the model
    /// after the last tool results.
    QueuedInput(String),
    /// A durable plan boundary; hosts must finish the old assistant segment before it.
    ContextReset { session_id: String, boundary_id: String },
    /// Delegated task state without exposing the child's private model output.
    AgentProgress {
        session_id: String,
        agent_type: String,
        status: String,
        tool_name: Option<String>,
    },
    RequestSnapshot(Value),
    Text(String),
    Thinking(String),
    ToolUseStart {
        tool_use_id: String,
        tool_name: String,
        tool_input: Value,
        text_before: String,
    },
    ToolArgDelta {
        tool_use_id: String,
        delta: String,
    },
    ToolUseDone {
        tool_use_id: String,
        tool_name: String,
        tool_input: Value,
        output: String,
        is_error: bool,
    },
    MessageStart {
        model: String,
    },
    MessageDelta {
        stop_reason: Option<String>,
    },
    MessageStop {
        full_text: String,
        stop_reason: Option<String>,
    },
    Error(String),
    Usage(Value),
    AskUser {
        request_id: String,
        tool_use_id: String,
        tool_input: Value,
        question: String,
        options: Vec<String>,
    },
    ToolPermission {
        request_id: String,
        tool_use_id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    PipelineToolResult {
        tool_use_id: String,
        tool_name: String,
        tool_input: Value,
        output: String,
        is_error: bool,
        completed_count: usize,
        total_count: usize,
    },
    BudgetWarning {
        message: String,
        usage: u64,
        limit: u64,
    },
}

pub struct ToolLoopExecutor {
    provider: ResolvedProvider,
    auto_manual_interactive: bool,
    permission_host_available: bool,
    local_goal_frontend: bool,
    auto_history: Vec<Value>,
    messages: Vec<Value>,
    system_prompt: Option<String>,
    max_tokens: u32,
    max_tool_iterations: usize,
    response_schema: Option<Value>,
    event_tx: mpsc::Sender<EngineEvent>,
    anthropic_client: AnthropicClient,
    openai_client: OpenAIClient,
    workspace_cwd: String,
    mcp_registry: Option<Arc<McpToolRegistry>>,
    mcp_outputs: HashMap<String, McpOutput>,
    mcp_tools: Vec<crate::tools::ToolDefinition>,
    streaming_tool_args: HashMap<String, StreamingToolCall>,
    conv_id: Option<String>,
    answer_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    permission_mailbox: crate::native_engine::permission_response::Mailbox,
    permission_interrupted: std::sync::Mutex<Option<String>>,
    /// Notes the user attached to approvals, delivered after the tool results.
    approval_notes: std::sync::Mutex<Vec<String>>,
    approved_call: Option<tool_approval::ExactApproval>,
    runtime_agents: super::runtime_agents::Catalog,
    permission_manager: Option<Arc<PermissionManager>>,
    memex_client: Option<MemExClient>,
    context_manager: Option<ContextManager>,
    prompt_profile: PromptProfile,
    reasoning_mode: ReasoningMode,
    prefetch_engine: Option<Arc<PrefetchEngine>>,
    bundled_resources: Arc<crate::skills::bundled_resources::Resources>,
    search_config_path: Option<std::path::PathBuf>,
    allowed_tools: Option<Vec<String>>,
    learning_tools: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    delegation_depth: usize,
    owned_children: delegation::OwnedChildren,
    child_resumes: Arc<AtomicUsize>,
    subagent_hook_context: Option<(String, String)>,
    hooks: Option<Arc<crate::services::tool_hooks::HookRunner>>,
    stop_hook_active: bool,
    stop_hook_blocks: u64,
    goal_db: Option<Arc<crate::db::DbManager>>,
    skill_shell_active: bool,
    pending_user_skill: Option<Value>,
    manual_skill_active: bool,
    durable_user_input: bool,
    message_inbox: Option<crate::services::session_messages::Target>,
    require_pending_peer_message: bool,
    require_hook_wake: bool,
    execution_lease: Option<crate::services::session_messages::ExecutionLease>,
    message_deliveries: usize,
    pending_plan_reset: Option<(String, String)>,
    allowed_directories: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
struct StreamingToolCall {
    name: String,
    accumulated_args: String,
}

struct PendingToolCall {
    id: String,
    name: String,
    input: Value,
    requires_user_interaction: bool,
}

/// A cancelled child must not leave an unreachable permission sender behind.
struct PermissionWaiterGuard {
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    request_id: String,
}
impl Drop for PermissionWaiterGuard {
    fn drop(&mut self) {
        if let Ok(mut waiters) = self.waiters.try_lock() {
            waiters.remove(&self.request_id);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let waiters = self.waiters.clone();
            let id = self.request_id.clone();
            runtime.spawn(async move { waiters.lock().await.remove(&id); });
        }
    }
}

fn model_tool_output(tool_name: &str, output: &str) -> String {
    if matches!(tool_name, "Write" | "Edit" | "MultiEdit") {
        if let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(output) {
            if fields.remove("file_change").is_some() {
                return serde_json::to_string(&fields).unwrap_or_else(|_| output.to_owned());
            }
        }
    }
    output.to_owned()
}

fn compact_tool_output(tool_name: &str, output: &str) -> String {
    let cleaned = model_tool_output(tool_name, output);
    let output = cleaned.as_str();
    match tool_name {
        "Edit" | "Write" | "MultiEdit" | "smart_edit" => {
            if output.len() > 500 {
                let safe_end = output.char_indices()
                    .take(500)
                    .last()
                    .map(|(idx, ch)| idx + ch.len_utf8())
                    .unwrap_or(0);
                format!("{}... (truncated, {} bytes total)", &output[..safe_end], output.len())
            } else {
                output.to_string()
            }
        }
        _ => output.to_string(),
    }
}

fn uses_independent_deepseek_search(provider: &ResolvedProvider) -> bool {
    match provider.provider.web_search_strategy.as_deref().unwrap_or("auto") {
        "deepseek_native" | "deepseek" => true,
        "auto" | "" => provider.model.id.to_ascii_lowercase().contains("deepseek")
            || provider.provider.builtin_id.as_deref() == Some("deepseek")
            || reqwest::Url::parse(&provider.provider.base_url).ok()
                .and_then(|url| url.host_str().map(String::from)).as_deref() == Some("api.deepseek.com"),
        _ => false,
    }
}

async fn independent_search_provider(path: Option<&std::path::Path>) -> Result<ResolvedProvider> {
    // Read at call time so saving settings takes effect without restarting a turn.
    let settings = if let Some(path) = path {
        match tokio::fs::read(path).await {
            Ok(bytes) => serde_json::from_slice::<crate::config::AppConfig>(&bytes)
                .map_err(|_| anyhow!("无法读取联网搜索设置，请在系统设置 → 联网搜索中重新保存 DeepSeek 配置。"))?
                .deepseek_search,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(_) => return Err(anyhow!("无法读取联网搜索设置，请检查应用配置文件权限。")),
        }
    } else { Default::default() };
    settings.resolved_provider()
}

/// Provider credentials are request-scoped, including parallel tool execution.
async fn execute_builtin_tool(
    name: &str,
    input: Value,
    cwd: &str,
    provider: &ResolvedProvider,
    event_tx: &mpsc::Sender<EngineEvent>,
    search_config_path: Option<&std::path::Path>,
    skill_shell_active: bool,
) -> Result<Value> {
    if name == "Bash" && skill_shell_active {
        let input = crate::skills::invocation::shell_input(input["command"].as_str().ok_or_else(||anyhow!("Skill shell command missing"))?, input["shell"].as_str())?;
        let permit = crate::skills::invocation::authorize(input, PermissionResult::Granted, false)?;
        return Ok(json!({"stdout":crate::skills::invocation::execute(permit,std::path::Path::new(cwd)).await?,"success":true,"exit_code":0}));
    }
    if name == "PluginRun" {
        let root=search_config_path.and_then(|p|p.parent()).ok_or_else(||anyhow!("Plugin storage is unavailable"))?;
        let context=crate::services::ServiceContext::open(crate::runtime::RuntimePaths::new(root.to_path_buf())?,std::sync::Arc::new(crate::services::EnvironmentCredentials))?;
        let id=input["plugin"].as_str().ok_or_else(||anyhow!("plugin is required"))?;
        let command=input["command"].as_str().ok_or_else(||anyhow!("command is required"))?;
        return crate::services::plugins::run(&context,id,command,input["input"].clone(),std::path::Path::new(cwd)).await;
    }
    if name != "WebSearch" {
        let file = if matches!(name, "Read" | "Edit" | "Write" | "MultiEdit") { input["file_path"].as_str().map(str::to_owned) } else { None };
        let mut result = crate::tools::execute_tool_async(name, input, cwd).await?;
        if result["is_error"] == true || result["isError"] == true || result["success"] == false || result["exit_code"].as_i64().is_some_and(|code| code != 0) {
            return Err(anyhow!(serde_json::to_string_pretty(&result)?));
        }
        if let Some(file) = file {
            if let Some(feedback) = crate::tools::lsp_passive_feedback(cwd, &file).await {
                if let Some(object) = result.as_object_mut() { object.insert("lsp_diagnostics".into(), feedback); }
                else { result = json!({"result":result,"lsp_diagnostics":feedback}); }
            }
        }
        return Ok(result);
    }
    if crate::tools::provider_search::uses_public_search(provider) {
        return crate::tools::execute_public_web_search(input).await;
    }

    let independent;
    let provider = if uses_independent_deepseek_search(provider) {
        independent = independent_search_provider(search_config_path).await?;
        &independent
    } else { provider };

    let mut trace = RequestTrace::new(json!({
        "id": uuid::Uuid::new_v4().to_string(), "number": 0,
        "startedAt": chrono::Utc::now().timestamp_millis(),
        "model": provider.model.id, "providerId": provider.provider.id,
        "providerName": provider.provider.name, "apiFormat": provider.provider.api_format,
        "source": "web_search", "systemPrompt": null, "systemMessages": [], "tools": [],
        "searchQuery": input["query"]
    }));
    let _ = event_tx.send(EngineEvent::RequestSnapshot(trace.started())).await;
    let result = crate::tools::provider_search::search(input, provider).await;
    let snapshot = match &result {
        Ok(value) => {
            trace.observe_openai(&json!({"usage": value["usage"], "model": value["model"], "id": value["response_id"]}));
            trace.mark_completed();
            let mut snapshot = trace.finish("completed", None);
            snapshot["apiFormat"] = value["api_format"].clone();
            if let Some(tokens) = snapshot["totalTokens"].as_f64() {
                crate::metrics::TOKENS_CONSUMED.inc_by(tokens);
            }
            snapshot
        }
        Err(error) => {
            if let Some(response) = error.downcast_ref::<crate::tools::provider_search::SearchResponseError>() {
                trace.observe_openai(&json!({"usage": response.usage, "model": response.model, "id": response.response_id}));
            }
            let mut snapshot = trace.finish("error", Some(&error.to_string()));
            if let Some(response) = error.downcast_ref::<crate::tools::provider_search::SearchResponseError>() {
                snapshot["apiFormat"] = json!(response.api_format);
            }
            if let Some(tokens) = snapshot["totalTokens"].as_f64() {
                crate::metrics::TOKENS_CONSUMED.inc_by(tokens);
            }
            snapshot
        }
    };
    let _ = event_tx.send(EngineEvent::RequestSnapshot(snapshot)).await;
    result
}

async fn execute_tool_standalone(
    tool_name: String,
    tool_input: Value,
    tool_use_id: String,
    event_tx: mpsc::Sender<EngineEvent>,
    mcp_registry: Option<Arc<McpToolRegistry>>,
    workspace_cwd: String,
    permission_manager: Option<Arc<PermissionManager>>,
    conv_id: Option<String>,
    completed_count: Arc<AtomicUsize>,
    total_count: usize,
    provider: ResolvedProvider,
    search_config_path: Option<std::path::PathBuf>,
    allowed_tools: Option<Vec<String>>,
    output_db: Option<Arc<crate::db::DbManager>>,
) -> (String, String, Value, String, bool, Option<McpOutput>) {
    crate::metrics::TOOL_CALLS_TOTAL.inc();
    let _tool_timer = crate::metrics::TOOL_CALL_DURATION.start_timer();

    let permission_result = if allowed_tools.as_ref().is_some_and(|names| !names.contains(&tool_name)) { PermissionResult::Denied("Tool is not allowed by this agent definition".into()) } else if let Some(ref pm) = permission_manager {
        let cid = conv_id.clone().unwrap_or_default();
        let workspace_path = Some(workspace_cwd.clone());
        let context = crate::permissions::PermissionContext {
            tool_name: tool_name.clone(),
            tool_input: tool_input.clone(),
            conversation_id: cid,
            user_id: None,
            workspace_path,
        };
        pm.check_permission(&context)
    } else {
        PermissionResult::Granted
    };

    match permission_result {
        PermissionResult::Denied(reason) => {
            let output = format!("Permission denied: {}", reason);
            let count = completed_count.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = event_tx.send(EngineEvent::ToolUseDone {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                output: output.clone(),
                is_error: true,
            }).await;
            let _ = event_tx.send(EngineEvent::PipelineToolResult {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                output: output.clone(),
                is_error: true,
                completed_count: count,
                total_count,
            }).await;
            return (tool_use_id, tool_name, tool_input, output, true, None);
        }
        PermissionResult::RequiresConfirmation(_) => {
            let output = "Tool requires user confirmation - must be executed serially".to_string();
            let count = completed_count.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = event_tx.send(EngineEvent::ToolUseDone {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                output: output.clone(),
                is_error: true,
            }).await;
            let _ = event_tx.send(EngineEvent::PipelineToolResult {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                output: output.clone(),
                is_error: true,
                completed_count: count,
                total_count,
            }).await;
            return (tool_use_id, tool_name, tool_input, output, true, None);
        }
        PermissionResult::Granted => {}
    }

    let mut mcp_output = None;
    let (mut output_str, mut is_error) = if let Some(ref registry) = mcp_registry {
        if registry.is_mcp_tool(&tool_name).await {
            let result = registry.execute_with_provenance(&tool_name, tool_input.clone()).await;
            match result {
                Ok(result) => {
                    let failed = result.value["isError"].as_bool().unwrap_or(false);
                    if result.is_tool_call {
                        match McpOutput::from_transport(&result.value) {
                            Ok(content) => { let output = content.display(); mcp_output = Some(content); (output, failed) }
                            Err(error) => (format!("Invalid MCP result: {error:#}"), true),
                        }
                    } else { (serde_json::to_string_pretty(&result.value).unwrap_or_default(), failed) }
                }
                Err(e) => (format!("Error: {}", e), true),
            }
        } else {
            let result = execute_builtin_tool(&tool_name, tool_input.clone(), &workspace_cwd, &provider, &event_tx, search_config_path.as_deref(), false).await;
            match &result {
                Ok(val) => (serde_json::to_string_pretty(val).unwrap_or_default(), false),
                Err(e) => (format!("Error: {}", e), true),
            }
        }
    } else {
        let result = execute_builtin_tool(&tool_name, tool_input.clone(), &workspace_cwd, &provider, &event_tx, search_config_path.as_deref(), false).await;
        match &result {
            Ok(val) => (serde_json::to_string_pretty(val).unwrap_or_default(), false),
            Err(e) => (format!("Error: {}", e), true),
        }
    };

    if let (Some(content), Some(db), Some(session)) = (&mcp_output, &output_db, &conv_id) {
        if let Err(error) = db.with_conn(|conn|content.save(conn, session, &tool_use_id, &output_str, is_error)).and_then(|result|result) {
            output_str = format!("Cannot retain MCP tool content: {error:#}");
            is_error = true;
            mcp_output = None;
        }
    }
    let count = completed_count.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = event_tx.send(EngineEvent::ToolUseDone {
        tool_use_id: tool_use_id.clone(),
        tool_name: tool_name.clone(),
        tool_input: tool_input.clone(),
        output: output_str.clone(),
        is_error,
    }).await;
    let _ = event_tx.send(EngineEvent::PipelineToolResult {
        tool_use_id: tool_use_id.clone(),
        tool_name: tool_name.clone(),
        tool_input: tool_input.clone(),
        output: output_str.clone(),
        is_error,
        completed_count: count,
        total_count,
    }).await;

    (tool_use_id, tool_name, tool_input, output_str, is_error, mcp_output)
}

/// Continuations requested in one turn after responses cut off by the output limit.
const MAX_OUTPUT_RECOVERIES: usize = 3;

fn output_limit_resume(limit: u32) -> String {
    format!("Output token limit hit ({limit} tokens); any tool call in that response was incomplete and did not run. Resume directly \u{2014} no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces, e.g. write a large file in parts: create it with Write, then append with Edit.")
}

fn output_limit_notice(limit: u32, attempt: usize) -> EngineEvent {
    EngineEvent::BudgetWarning {
        message: format!("Output reached the {limit}-token limit; asking the model to continue ({attempt}/{MAX_OUTPUT_RECOVERIES})."),
        usage: limit as u64,
        limit: limit as u64,
    }
}

impl ToolLoopExecutor {
    pub fn with_response_schema(mut self, schema: Option<Value>) -> Result<Self> {
        if let Some(schema) = &schema { crate::structured_output::validate_schema(schema)?; }
        self.openai_client = self.openai_client.with_response_schema(schema.clone());
        self.anthropic_client = self.anthropic_client.with_response_schema(schema.clone());
        self.response_schema = schema;
        Ok(self)
    }

    pub fn with_max_turns(mut self, max_turns: Option<u32>) -> Result<Self> {
        if let Some(max_turns) = max_turns {
            anyhow::ensure!(max_turns > 0, "max_turns must be greater than zero");
            self.max_tool_iterations = max_turns as usize;
        }
        Ok(self)
    }

    pub fn new(
        provider: ResolvedProvider,
        messages: Vec<Value>,
        system_prompt: Option<String>,
        max_tokens: u32,
        event_tx: mpsc::Sender<EngineEvent>,
        workspace_cwd: String,
        reasoning_mode: ReasoningMode,
    ) -> Self {
        Self {
            auto_manual_interactive: false,
            permission_host_available: true,
            local_goal_frontend: false,
            auto_history: messages.clone(),
            provider,
            messages,
            system_prompt,
            max_tokens,
            max_tool_iterations: crate::config::OrchestrationConfig::max_tool_iterations(),
            response_schema: None,
            event_tx,
            anthropic_client: AnthropicClient::new(),
            openai_client: OpenAIClient::new(),
            workspace_cwd,
            mcp_registry: None,
            mcp_outputs: HashMap::new(),
            mcp_tools: Vec::new(),
            streaming_tool_args: HashMap::new(),
            conv_id: None,
            answer_waiters: Arc::new(Mutex::new(HashMap::new())),
            permission_mailbox: Default::default(),
            permission_interrupted: std::sync::Mutex::new(None),
            approval_notes: std::sync::Mutex::new(Vec::new()),
            approved_call: None,
            runtime_agents: Default::default(),
            permission_manager: None,
            memex_client: None,
            context_manager: None,
            prompt_profile: PromptProfile::Desktop,
            reasoning_mode,
            prefetch_engine: None,
            bundled_resources: Arc::default(),
            search_config_path: None,
            allowed_tools: None,
            learning_tools: None,
            delegation_depth: 0,
            owned_children: Default::default(),
            child_resumes: Arc::new(AtomicUsize::new(0)),
            subagent_hook_context: None,
            hooks: None,
            stop_hook_active: false,
            stop_hook_blocks: 0,
            goal_db: None,
            skill_shell_active: false,
            pending_user_skill: None,
            manual_skill_active: false,
            durable_user_input: false,
            message_inbox: None,
            require_pending_peer_message: false,
            require_hook_wake: false,
            execution_lease: None,
            message_deliveries: 0,
            pending_plan_reset: None,
            allowed_directories: Vec::new(),
        }
    }

    pub fn with_learning_tools(mut self, tools: Option<Arc<std::sync::Mutex<Vec<String>>>>) -> Self {
        self.learning_tools = tools;
        self
    }

    pub(super) fn with_allowed_directories(mut self, directories: Vec<std::path::PathBuf>) -> Self {
        self.allowed_directories = directories;
        self
    }

    pub(super) fn with_user_skill(mut self, skill: Option<Value>) -> Self {
        self.pending_user_skill = skill;
        self
    }

    pub fn with_hooks(mut self, hooks: crate::services::tool_hooks::HookRunner) -> Self {
        if hooks.has_hooks() { self.hooks = Some(Arc::new(hooks)); }
        self
    }

    pub fn with_goal_state(mut self, db: Arc<crate::db::DbManager>) -> Self {
        self.goal_db = Some(db);
        self
    }

    async fn stop_hook(&mut self, text: &str) -> Result<Option<String>> {
        use crate::services::tool_hooks::HookEvent;
        let Some(hooks) = &self.hooks else { return Ok(None); };
        let mut payload = json!({"stop_hook_active":self.stop_hook_active,"last_assistant_message":text});
        let (event, session) = if let Some((parent, agent_type)) = &self.subagent_hook_context {
            payload["agent_id"] = json!(self.conv_id);
            payload["agent_type"] = json!(agent_type);
            (HookEvent::SubagentStop, parent.as_str())
        } else { (HookEvent::Stop, self.conv_id.as_deref().unwrap_or("unknown")) };
        let outcome = hooks.run(event, session, payload).await?;
        for warning in outcome.warnings { tracing::warn!("{event:?} hook: {warning}"); }
        if outcome.prevent_continuation {
            if let (Some(db), Some(id)) = (&self.goal_db, &self.conv_id) {
                crate::services::goal_proposals::discard(db, id)?;
                if let Some(goal) = crate::services::session_goals::active(db, id)? {
                    crate::services::session_goals::pause(db, id, &goal.id, "Stop hook prevented continuation")?;
                }
            }
            return Ok(None);
        }
        if outcome.blocked {
            self.stop_hook_blocks = self.stop_hook_blocks.saturating_add(1);
            let cap = crate::services::session_goals::stop_cap();
            let shared = match (&self.goal_db, &self.conv_id) {
                (Some(db), Some(id)) => crate::services::session_goals::block(db, id, None, cap)?,
                _ => None,
            };
            if !shared.unwrap_or_else(|| crate::services::session_goals::within_stop_cap(self.stop_hook_blocks, cap)) {
                tracing::warn!("{event:?} consecutive block cap reached; send a message to continue");
                return Ok(None);
            }
            self.stop_hook_active = true;
            return Ok(Some(format!("The configured {event:?} hook requires follow-up before finishing: {}", outcome.reason)));
        }
        Ok(None)
    }

    async fn execute_tool_call(&mut self, name: &str, input: &Value, id: &str) -> (Value, String, bool) {
        self.mcp_outputs.remove(id);
        if let Err(error) = self.ensure_permission_continuation() { return (input.clone(), error.to_string(), true); }
        if self.pending_plan_reset.is_some() {
            return (input.clone(), "Tool skipped: the user cleared planning context. Request it again from the approved implementation context.".into(), true);
        }
        use crate::services::tool_hooks::HookEvent;
        let Some(hooks) = self.hooks.clone() else { return Box::pin(self.execute_tool_call_inner(name, input, id)).await; };
        let session = self.conv_id.clone().unwrap_or_else(|| "unknown".into());
        let pre = match hooks.run(HookEvent::PreToolUse, &session, json!({"tool_name":name,"tool_input":input,"tool_use_id":id})).await {
            Ok(outcome) => outcome,
            Err(error) => return (input.clone(), format!("PreToolUse hook failed; tool not executed: {error:#}"), true),
        };
        if pre.blocked { return (input.clone(), format!("PreToolUse hook blocked tool: {}", pre.reason), true); }
        let mut input = pre.updated_input.unwrap_or_else(|| input.clone());
        if pre.ask_permission {
            if self.permission_manager.as_ref().is_some_and(|manager| manager.get_mode() == crate::permissions::PermissionMode::DontAsk) {
                return (input, "dontAsk mode: hook requested permission; tool was not executed".into(), true);
            }
            let hook_decision = self.permission_manager.as_ref().map(|pm| pm.check_permission(&crate::permissions::PermissionContext { tool_name:name.into(), tool_input:input.clone(), conversation_id:self.conv_id.clone().unwrap_or_default(), user_id:None, workspace_path:Some(self.workspace_cwd.clone()) })).unwrap_or(PermissionResult::Granted);
            if let PermissionResult::Denied(reason) = hook_decision {
                return (input, format!("Permission denied: {reason}"), true);
            }
            input = match self.request_tool_approval(name, &input, id).await {
                Ok(actual) => actual,
                Err(error) => return (input, format!("Hook requested permission: {error:#}"), true),
            };
            self.approved_call = Some(tool_approval::ExactApproval { tool:name.into(), input:input.clone(), id:id.into(), pre_tool_hook:false });
        }
        if pre.allow_permission && !pre.ask_permission && !matches!(name, "AskUserQuestion" | "AskUser" | "EnterPlanMode" | "ExitPlanMode" | "plan_mode") {
            let can_replace_prompt = self.permission_manager.as_ref().is_none_or(|manager| {
                let decision = manager.evaluate_permission(&crate::permissions::PermissionContext {
                    tool_name: name.into(), tool_input: input.clone(), conversation_id: self.conv_id.clone().unwrap_or_default(),
                    user_id: None, workspace_path: Some(self.workspace_cwd.clone()),
                });
                decision.origin == crate::permissions::DecisionOrigin::DefaultPrompt
                    || matches!(decision.result, PermissionResult::Granted)
            });
            if can_replace_prompt {
                if let Err(error) = self.validate_approved_input_for(name, &input, true).await {
                    return (input, format!("PreToolUse allow rejected: {error:#}"), true);
                }
                self.approved_call = Some(tool_approval::ExactApproval { tool:name.into(), input:input.clone(), id:id.into(), pre_tool_hook:true });
            }
        }
        // Updated arguments pass through the complete normal permission path.
        let (actual, mut output, mut failed) = Box::pin(self.execute_tool_call_inner(name, &input, id)).await;
        self.approved_call = None;
        let mut mcp_output = self.mcp_outputs.remove(id);
        let mut contexts: Vec<String> = pre.additional_context.into_iter().map(|context|format!("PreToolUse context: {context}")).collect();
        if mcp_output.is_none() {
            for context in contexts.drain(..) { output.push('\n'); output.push_str(&context); }
        }
        for warning in pre.warnings { tracing::warn!("PreToolUse hook: {warning}"); }
        let event = if failed { HookEvent::PostToolUseFailure } else { HookEvent::PostToolUse };
        let mut payload = json!({"tool_name":name,"tool_input":actual,"tool_use_id":id});
        if failed { payload["error"] = json!(output); payload["is_interrupt"] = json!(false); }
        else { payload["tool_response"] = mcp_output.as_ref().map(|value|value.content.clone()).unwrap_or_else(||json!(output)); }
        match hooks.run(event, &session, payload).await {
            Ok(post) => {
                if !failed {
                    if let (Some(content), Some(replacement)) = (&mut mcp_output, &post.updated_mcp_tool_output) { content.replace(replacement); }
                }
                for context in post.additional_context { contexts.push(format!("{event:?} context: {context}")); }
                for warning in post.warnings { tracing::warn!("{event:?} hook: {warning}"); }
                if post.blocked { contexts.push(format!("{event:?} hook requires attention: {}", post.reason)); failed = true; }
            }
            Err(error) => { contexts.push(format!("{event:?} hook failed: {error:#}")); failed = true; }
        }
        if let Some(mut content) = mcp_output {
            for context in contexts { content.append_context(&context); }
            output = content.display();
            self.mcp_outputs.insert(id.to_owned(), content);
        } else {
            for context in contexts { output.push('\n'); output.push_str(&context); }
        }
        (actual, output, failed)
    }

    fn retained_tool_output(&self, tool_name: &str, output: &str) -> String {
        if let Some(tools) = &self.learning_tools {
            if let Ok(mut tools) = tools.lock() { if tools.len() < 256 { tools.push(tool_name.to_string()); } }
        }
        let cleaned = model_tool_output(tool_name, output);
        let output = cleaned.as_str();
        // Read already selects the requested line range. This is the fresh
        // result sent to the model, not a summary of old conversation history.
        // Spilling it would also truncate any subsequent Read of the spill file.
        if tool_name == "Read" {
            return output.to_string();
        }
        let preview = compact_tool_output(tool_name, output);
        if let Some(root) = self.search_config_path.as_deref().and_then(|p| p.parent()) {
            return crate::performance::retain_tool_output(root, output, &preview);
        }
        output.to_string()
    }

    fn finish_mcp_output(&self, id: &str, name: &str, display: &str, failed: bool, content: Option<McpOutput>) -> Result<Option<McpOutput>> {
        let Some(mut content) = content else { return Ok(None); };
        if let (Some(db), Some(session)) = (&self.goal_db, &self.conv_id) {
            db.with_conn(|conn|content.save(conn, session, id, display, failed))??;
        }
        // Images and structured blocks must remain real provider content. Only
        // plain text can use the existing spill/preview representation.
        if !content.has_images() {
            if let Some(text) = content.content.as_str() {
                content.content = json!(self.retained_tool_output(name, text));
            }
        }
        Ok(Some(content))
    }

    pub fn with_bundled_resources(mut self, resources: Arc<crate::skills::bundled_resources::Resources>) -> Self {
        self.bundled_resources = resources;
        self
    }

    pub fn with_search_config_path(mut self, path: std::path::PathBuf) -> Self {
        self.search_config_path = Some(path);
        self
    }

    pub fn with_conv_id(mut self, conv_id: String) -> Self {
        self.conv_id = Some(conv_id);
        self
    }

    pub fn with_answer_waiters(mut self, waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>) -> Self {
        self.answer_waiters = waiters;
        self
    }

    pub fn get_answer_waiters(&self) -> Arc<Mutex<HashMap<String, oneshot::Sender<String>>>> {
        self.answer_waiters.clone()
    }

    pub fn with_mcp_registry(mut self, registry: Arc<McpToolRegistry>) -> Self {
        self.mcp_registry = Some(registry);
        self
    }

    pub fn with_permission_manager(mut self, manager: Arc<PermissionManager>) -> Self {
        if manager.get_mode() == crate::permissions::PermissionMode::PlanMode {
            self.system_prompt.get_or_insert_with(String::new).push_str("\nThis turn starts in read-only plan mode. Investigate without modifying project files until ExitPlanMode succeeds. Read-only shell commands (for example ls, cat, grep, git status/log/diff, Get-ChildItem, Get-Content) run directly; any other command asks the user first. Write a concrete Markdown implementation and verification plan to the dedicated session plan file using Write/Edit, then call ExitPlanMode (the legacy plan text parameter is also accepted). It requests explicit user approval before returning to normal ask permissions. Do not claim approval without a successful ExitPlanMode result.\n");
        }
        self.permission_manager = Some(manager);
        self
    }

    pub fn with_memex(mut self, client: MemExClient) -> Self {
        let client_arc = Arc::new(client.clone());
        self.context_manager = Some(ContextManager::new(client_arc));
        self.memex_client = Some(client);
        self
    }

    pub fn with_prompt_profile(mut self, profile: PromptProfile) -> Self {
        self.prompt_profile = profile;
        self
    }

    pub fn with_allowed_tools(mut self, tools: Option<Vec<String>>) -> Self { self.allowed_tools = tools; self }

    pub(crate) fn with_runtime_agents(mut self, agents: super::runtime_agents::Catalog) -> Self {
        self.runtime_agents = agents;
        self
    }

    fn tool_definitions(&self) -> Vec<crate::tools::ToolDefinition> {
        let mut tools = get_tool_definitions();
        if !self.goal_proposals_available() { tools.retain(|tool| tool.name != "ProposeGoal"); }
        tools.extend(self.mcp_tools.clone());
        if self.search_config_path.is_some() && self.delegation_depth < 2 {
            let mut definitions = delegation::definitions();
            if !self.runtime_agents.is_empty() {
                let catalog = self
                    .runtime_agents
                    .iter()
                    .map(|agent| format!("{}: {}", agent.definition.id, agent.definition.description))
                    .collect::<Vec<_>>()
                    .join("; ");
                for definition in &mut definitions {
                    definition.description.push_str(&format!(" Run-local agents: {catalog}"));
                }
            }
            tools.extend(definitions);
        }
        if self.search_config_path.is_some() && self.delegation_depth == 0 { tools.push(deep_research::definition()); }
        adapt_tools(tools, self.prompt_profile).into_iter().filter(|tool| self.allowed_tools.as_ref().is_none_or(|names| names.contains(&tool.name))).collect()
    }

    pub fn with_prefetch_engine(mut self, engine: Arc<PrefetchEngine>) -> Self {
        self.prefetch_engine = Some(engine);
        self
    }

    pub fn with_permission_host(mut self, available: bool) -> Self {
        self.permission_host_available = available;
        self
    }

    fn goal_proposals_available(&self) -> bool {
        if !self.local_goal_frontend || self.delegation_depth != 0 || self.subagent_hook_context.is_some() { return false; }
        match (&self.goal_db, &self.conv_id, &self.permission_manager, self.search_config_path.as_deref().and_then(std::path::Path::parent)) {
            (Some(db), Some(session), Some(permissions), Some(root)) => crate::services::goal_proposals::available(db, session, root, std::path::Path::new(&self.workspace_cwd), permissions),
            _ => false,
        }
    }
    pub fn with_local_goal_frontend(mut self, enabled: bool) -> Self {
        self.local_goal_frontend = enabled;
        self
    }

    pub fn with_auto_manual_interactive(mut self, interactive: bool) -> Self {
        self.auto_manual_interactive = interactive;
        self
    }

    fn ensure_auto_not_exhausted(&self) -> Result<()> {
        if !self.auto_manual_interactive {
            if let Some(manager) = &self.permission_manager {
                anyhow::ensure!(manager.get_mode() != crate::permissions::PermissionMode::Auto || !manager.auto_stats(self.conv_id.as_deref().unwrap_or("")).denial_limit(), "Auto permission denial limit reached; stopping the headless turn");
            }
        }
        Ok(())
    }

    async fn check_permission(&self, tool_name: &str, tool_input: &Value, tool_use_id: &str) -> PermissionResult {
        if let Some(approval) = self.approved_call.as_ref().filter(|approval| approval.tool == tool_name && approval.input == *tool_input && approval.id == tool_use_id) {
            return self.permission_hard_denial_for(tool_name, tool_input, approval.pre_tool_hook).map(PermissionResult::Denied).unwrap_or(PermissionResult::Granted);
        }
        if let Some(ref pm) = self.permission_manager {
            let conv_id = self.conv_id.clone().unwrap_or_default();
            let workspace_path = Some(self.workspace_cwd.clone());

            let context = crate::permissions::PermissionContext {
                tool_name: tool_name.to_string(),
                tool_input: tool_input.clone(),
                conversation_id: conv_id,
                user_id: None,
                workspace_path,
            };
            if self.is_plan_document_tool(tool_name, tool_input) {
                pm.check_plan_document_permission(&context)
            } else if pm.get_mode() == crate::permissions::PermissionMode::Auto {
                let Some(root) = self.search_config_path.as_deref().and_then(std::path::Path::parent) else { return PermissionResult::Denied("Auto classifier requires a configured data directory".into()); };
                let service = match crate::services::session_messages::context(root) { Ok(service) => service, Err(_) => return PermissionResult::Denied("Auto classifier settings unavailable".into()) };
                let authorization = super::auto_permissions::authorize(&service, pm, &context, &self.provider, &self.auto_history, self.auto_manual_interactive).await
                    .with_denial_hook(self.hooks.as_deref(), &context, tool_use_id).await;
                if let Some(telemetry) = authorization.telemetry { let _ = self.event_tx.send(EngineEvent::Usage(telemetry)).await; }
                authorization.result
            } else {
                pm.check_permission(&context)
            }
        } else {
            PermissionResult::Granted
        }
    }

    fn check_permission_requires_confirmation(&self, tool_name: &str, tool_input: &Value) -> bool {
        if let Some(ref pm) = self.permission_manager {
            if pm.get_mode() == crate::permissions::PermissionMode::Auto { return true; }
            let conv_id = self.conv_id.clone().unwrap_or_default();
            let workspace_path = Some(self.workspace_cwd.clone());
            let context = crate::permissions::PermissionContext {
                tool_name: tool_name.to_string(),
                tool_input: tool_input.clone(),
                conversation_id: conv_id,
                user_id: None,
                workspace_path,
            };
            matches!(pm.check_permission(&context), PermissionResult::RequiresConfirmation(_))
        } else {
            false
        }
    }

    /// Messages queued during the turn; only read after tool results, so a
    /// final answer is never interrupted.
    async fn deliver_queued_inputs(&self, iteration: usize) -> Vec<String> {
        let (Some(db), Some(id)) = (&self.goal_db, &self.conv_id) else {
            return Vec::new();
        };
        if iteration == 0 {
            return Vec::new();
        }
        let inputs = crate::services::input_queue::take(db, id).unwrap_or_else(|error| {
            tracing::warn!("Queued input unavailable: {error:#}");
            Vec::new()
        });
        for text in &inputs {
            let _ = self.event_tx.send(EngineEvent::QueuedInput(text.clone())).await;
        }
        inputs
    }

    /// The blocks for a queued message whose `[Image #N]` names pasted
    /// images, when the model accepts images.
    fn queued_images(&self, text: &str) -> Option<Vec<Value>> {
        if !self.provider.model.supports_vision || !text.contains("[Image #") {
            return None;
        }
        let id = self.conv_id.as_deref()?;
        let data_dir = crate::runtime::resolve_default_data_dir().ok()?;
        crate::services::input_queue::content_with_images(&data_dir, id, text)
    }

    fn drain_approval_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.approval_notes.lock().unwrap_or_else(|error| error.into_inner()))
    }

    fn drain_peer_messages(&mut self) -> Result<Vec<String>> {
        let messages = match self.message_inbox.as_ref() {
            Some(inbox) => inbox.drain(32usize.saturating_sub(self.message_deliveries))?,
            None => Vec::new(),
        };
        self.message_deliveries += messages.len();
        Ok(messages)
    }

    pub(crate) fn with_owned_children(mut self, children: delegation::OwnedChildren) -> Self {
        self.owned_children = children;
        self
    }

    pub(crate) fn with_hook_wake(mut self, required: bool) -> Self {
        self.require_hook_wake = required;
        self
    }

    pub async fn execute(&mut self) -> Result<(String, Option<String>)> {
        let _execution_owner = if self.execution_lease.is_some() {
            self.execution_lease.take()
        } else {
            match (
                self.search_config_path
                    .as_deref()
                    .and_then(std::path::Path::parent),
                self.conv_id.as_deref(),
            ) {
                (Some(root), Some(session)) => {
                    crate::services::session_messages::ExecutionLease::acquire(root, session)?
                }
                _ => None,
            }
        };
        let mut async_hook_guard = crate::services::async_hooks::TurnGuard(self.conv_id.clone());
        if self.require_hook_wake {
            let root = self
                .search_config_path
                .as_deref()
                .and_then(std::path::Path::parent)
                .ok_or_else(|| anyhow!("Missing hook wake root"))?;
            let session = self.conv_id.as_deref().unwrap_or_default();
            anyhow::ensure!(
                crate::services::async_hooks::has_pending_wake(root, session),
                "No pending hook wake remains"
            );
            let incoming = crate::services::async_hooks::drain_wake(root, session);
            anyhow::ensure!(!incoming.is_empty(), "Hook wake already consumed");
            self.messages.extend(incoming.into_iter().map(|content| json!({"role":"user","content":format!("<system-reminder>\n{content}\n</system-reminder>")})));
            self.require_hook_wake = false;
        }
        if self
            .permission_manager
            .as_ref()
            .is_some_and(|pm| pm.get_mode() == crate::permissions::PermissionMode::Auto)
        {
            let root = self
                .search_config_path
                .as_deref()
                .and_then(std::path::Path::parent)
                .ok_or_else(|| anyhow!("Auto permissions require a configured data directory"))?;
            let context = crate::services::session_messages::context(root)?;
            let (config, _) = super::auto_permissions::configured(&context, &self.provider)?;
            let authority = crate::services::permission_authority::history(&context.db, self.conv_id.as_deref().unwrap_or(""), config.max_input_tokens as usize)?;
            anyhow::ensure!(!authority.is_empty(), "No verified user authorization is recorded for this session; provide a new explicit user request before using Auto");
        }
        // A previous invocation may have been dropped while executing a manual
        // skill. Explicit invocation authority never carries into the next turn.
        self.manual_skill_active = false;
        *self.permission_interrupted.lock().unwrap_or_else(|error| error.into_inner()) = None;
        self.approved_call = None;
        if let Some((boundary_id, content)) = self.pending_plan_reset.take() {
            self.messages = vec![json!({"role":"user","content":content})];
            self.stop_hook_active = false;
            let _ = self.event_tx.send(EngineEvent::ContextReset { session_id: self.conv_id.clone().unwrap_or_default(), boundary_id }).await;
        }
        // The OS lock is scoped to this future: cancellation/crash releases it.
        let inbox_owner = match (self.search_config_path.as_deref().and_then(std::path::Path::parent), self.conv_id.as_deref()) {
            (Some(root), Some(session)) => crate::services::session_messages::Consumer::attach(root, session)?,
            _ => None,
        };
        self.message_inbox = inbox_owner.as_ref().map(|owner| owner.target.clone());
        self.message_deliveries = 0;
        self.child_resumes.store(0, Ordering::Relaxed);
        if self.require_pending_peer_message {
            let incoming = self.drain_peer_messages()?;
            anyhow::ensure!(!incoming.is_empty(), "No trusted pending peer message remains; needs_resume");
            self.messages.extend(incoming.into_iter().map(|content| json!({"role":"user","content":content})));
            self.require_pending_peer_message = false;
        }

        if self.permission_manager.as_ref().is_some_and(|pm| pm.get_mode() == crate::permissions::PermissionMode::PlanMode) {
            if let (Some(root), Some(session)) = (self.search_config_path.as_deref().and_then(std::path::Path::parent), self.conv_id.as_deref()) {
                let path = crate::services::plans::path(root, session)?;
                self.system_prompt.get_or_insert_with(String::new).push_str(&format!("\nWhile planning mode is active, the only writable plan file for this session is: {}. Use its exact absolute path with Write/Edit; all other file changes remain prohibited until the user approves leaving plan mode.\n", path.display()));
            }
        }

        if !self.durable_user_input {
            if let (Some(root), Some(session)) = (self.search_config_path.as_deref().and_then(|p| p.parent()), self.conv_id.as_deref()) {
                self.durable_user_input = crate::services::background_sessions::worker_directory(root, session)?.is_some();
            }
        }
        // MCP discovery is a live external exchange, so execution ownership and
        // authority checks must succeed before any SDK/stdio server is contacted.
        if let (Some(db), Some(session)) = (&self.goal_db, &self.conv_id) {
            db.with_conn(|conn|super::mcp_output::restore_history(&mut self.messages, conn, session))??;
        }
        Box::pin(self.refresh_mcp_tools()).await?;
        let mut directories = self.allowed_directories.clone();
        if let (Some(manager), Some(conv)) = (&self.permission_manager, self.conv_id.as_deref()) {
            directories.extend(manager.session_directories(conv));
        }
        let context = self
            .search_config_path
            .as_deref()
            .and_then(|p| p.parent())
            .map(|root| (root.to_path_buf(), self.conv_id.clone().unwrap_or_default()));
        let result = crate::tools::background_shell::scope(
            context,
            crate::tools::path_scope::scope(directories, self.execute_scoped()),
        )
        .await;
        if result.is_ok() {
            async_hook_guard.0 = None;
        }
        result
    }

    fn drain_async_hook_messages(&self) -> Vec<String> {
        let session = self.conv_id.as_deref().unwrap_or_default();
        let mut messages = crate::services::async_hooks::drain(session);
        if let Some(root) = self
            .search_config_path
            .as_deref()
            .and_then(std::path::Path::parent)
        {
            messages.extend(crate::services::async_hooks::drain_shared(root, session));
        }
        messages
    }

    async fn refresh_mcp_tools(&mut self) -> Result<()> {
        self.mcp_tools.clear();
        if let Some(registry) = &self.mcp_registry {
            let mut builtin = get_tool_definitions();
            builtin.extend(delegation::definitions());
            builtin.push(deep_research::definition());
            let builtin = builtin.into_iter().map(|tool| tool.name).collect::<std::collections::HashSet<_>>();
            self.mcp_tools = registry.get_all_tools().await?.into_iter().map(|tool| {
                anyhow::ensure!(!builtin.contains(&tool.name), "MCP tool conflicts with native tool: {}", tool.name);
                Ok(crate::tools::ToolDefinition { name:tool.name, description:tool.description, input_schema:tool.input_schema })
            }).collect::<Result<Vec<_>>>()?;
        }
        Ok(())
    }

    async fn execute_scoped(&mut self) -> Result<(String, Option<String>)> {
        let conv_id = self.conv_id.clone().unwrap_or_default();
        tracing::info!(module = "ToolLoop", "[E1] execute ENTRY: conv_id={}, messages_count={}, provider={}", conv_id, self.messages.len(), self.provider.provider.id);

        // Token 计数与自动压缩 - First pass
        tracing::info!(module = "ToolLoop", "[E2] Checking token threshold...");
        let token_threshold = crate::config::OrchestrationConfig::token_threshold();
        if TokenCounter::exceeds_threshold(&self.messages, token_threshold) {
            tracing::info!(module = "ToolLoop", "[E2.1] Tokens exceed threshold, compressing with TieredCompressor...");
            let compressor = TieredCompressor::new();
            let compressed = compressor.compress(&self.messages);
            let original_len = self.messages.len();
            self.messages = compressed;
            tracing::info!(module = "ToolLoop", "Context compressed: {} -> {} messages", original_len, self.messages.len());
        }
        tracing::info!(module = "ToolLoop", "[E2] Token check COMPLETED");

        // 提取最后一条用户消息用于记忆检索
        tracing::info!(module = "ToolLoop", "[E3] Extracting last user message...");
        let last_user_msg = self.messages.iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .and_then(|m| m.get("content"))
            .map(|content| {
                if let Some(text) = content.as_str() { return text.to_string(); }
                content.as_array().map(|blocks| blocks.iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>().join("\n")).unwrap_or_default()
            })
            .unwrap_or_default();
        tracing::info!(module = "ToolLoop", "[E3] Last user msg len={}", last_user_msg.len());

        if let Some(ref prefetch) = self.prefetch_engine {
            let msg = last_user_msg.clone();
            let prefetch_clone = prefetch.clone();
            tokio::spawn(async move {
                prefetch_clone.prefetch_for_message(&msg).await;
            });
        }

        if let Some(ctx_mgr) = &self.context_manager {
            tracing::info!(module = "ToolLoop", "[E4] Injecting memory context...");
            if let Some(memory_context) = ctx_mgr.before_api_call(&conv_id, &last_user_msg).await {
                self.messages.insert(0, json!({
                    "role": "user",
                    "content": memory_context
                }));
            }
        }
        tracing::info!(module = "ToolLoop", "[E4] Memory injection COMPLETED");

        // 模型特定的检查和二次压缩
        tracing::info!(module = "ToolLoop", "[E4.5] Model-specific token check...");
        let model_id = &self.provider.model.id;
        let safety_margin = 0.85; // Keep 15% buffer for response
        let safe_limit = self.provider.model.context_window.map(|n|(n as f64*safety_margin) as usize).unwrap_or_else(||TokenCounter::get_safe_limit(model_id, safety_margin));
        let estimated_tokens = TokenCounter::estimate(&self.messages);

        tracing::info!(module = "ToolLoop", "Model: {}, Estimated tokens: {}, Safe limit: {}", model_id, estimated_tokens, safe_limit);

        if estimated_tokens > safe_limit {
            tracing::warn!(module = "ToolLoop", "Context still exceeds model limit! Applying progressive compression...");
            let compressor = TieredCompressor::new();
            let original_estimate = TokenCounter::estimate(&self.messages);

            self.messages = compressor.compress_progressive(
                &self.messages,
                safe_limit,
                |msgs| TokenCounter::estimate(msgs),
            );

            let final_estimate = TokenCounter::estimate(&self.messages);
            tracing::info!(module = "ToolLoop", "Progressive compression complete: {} -> {} tokens", original_estimate, final_estimate);
        }
        tracing::info!(module = "ToolLoop", "[E4.5] Model check COMPLETED");

        tracing::info!(module = "ToolLoop", "[E5] Sending MessageStart event...");
        let _ = self.event_tx.send(EngineEvent::MessageStart {
            model: self.provider.model.id.clone(),
        }).await;
        tracing::info!(module = "ToolLoop", "[E5] MessageStart sent");

        tracing::info!(module = "ToolLoop", "[E6] API format: {:?}", self.provider.provider.api_format);

        let user_skill = self.prepare_user_skill().await;
        let api_request = matches!(user_skill, Ok(None));
        let loop_result = match user_skill {
            Err(error) => Err(error),
            Ok(Some(text)) => Ok((text, Some("end_turn".into()))),
            Ok(None) => match self.provider.provider.api_format {
            ApiFormat::Anthropic => {
                tracing::info!(module = "ToolLoop", "[E6.1] Starting Anthropic loop...");
                self.execute_anthropic_loop().await
            }
            ApiFormat::OpenAI => {
                tracing::info!(module = "ToolLoop", "[E6.2] Starting OpenAI loop...");
                self.execute_openai_loop().await
            }
            },
        };

        if api_request {
            let failure = match &loop_result {
                Err(error) => Some(error.to_string()),
                Ok((text, reason))
                    if matches!(reason.as_deref(), Some("stream_error" | "error" | "failed")) =>
                {
                    Some(text.clone())
                }
                _ => None,
            };
            if let (Some(error), Some(hooks)) = (failure, &self.hooks) {
                if !self.event_tx.is_closed()
                    && self
                        .permission_interrupted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_none()
                {
                    let payload = json!({"error":crate::services::lifecycle_events::api_error_kind(&error),"error_details":error});
                    if let Err(error) = hooks
                        .run(
                            crate::services::tool_hooks::HookEvent::StopFailure,
                            &conv_id,
                            payload,
                        )
                        .await
                    {
                        tracing::warn!("StopFailure hook: {error:#}");
                    }
                }
            }
        }
        let loop_result = loop_result.and_then(|(mut text, stop_reason)| {
            if let Some(schema) = &self.response_schema {
                if !matches!(stop_reason.as_deref(),Some("stream_error"|"error"|"interrupted"|"failed"|"cancelled"|"max_iterations"|"max_tokens"|"length"|"incomplete")) {
                    crate::structured_output::validate_result(schema,&text)?;
                    // Hand back the bare JSON even when the model fenced it.
                    text = crate::structured_output::strip_code_fence(&text).to_string();
                }
            }
            Ok((text,stop_reason))
        });
        match &loop_result {
            Ok((full_text, stop_reason)) => {
                tracing::info!(module = "ToolLoop", "[E6] API loop COMPLETED, full_text_len={}", full_text.len());
                // Persist before announcing completion so the next session can recall this turn.
                if !matches!(stop_reason.as_deref(), Some("stream_error" | "error" | "interrupted" | "failed" | "cancelled" | "max_iterations" | "max_tokens" | "length" | "incomplete")) {
                    if let Some(ctx_mgr) = &self.context_manager {
                        ctx_mgr.after_response(&conv_id, &last_user_msg, full_text).await;
                    }
                }
                let _ = self.event_tx.send(EngineEvent::MessageStop {
                    full_text: full_text.clone(),
                    stop_reason: stop_reason.clone(),
                }).await;
                tracing::info!(module = "ToolLoop", "[E7] MessageStop sent (success)");
            }
            Err(e) => {
                let err_msg = format!("Engine execution failed: {}", e);
                tracing::error!(module = "ToolLoop", "[E6] API loop FAILED: {}", err_msg);
                let _ = self.event_tx.send(EngineEvent::Error(err_msg)).await;
                let _ = self.event_tx.send(EngineEvent::MessageStop {
                    full_text: String::new(),
                    stop_reason: Some("error".to_string()),
                }).await;
                tracing::info!(module = "ToolLoop", "[E7] Error + MessageStop sent (failure)");
            }
        }

        loop_result
    }

    async fn execute_tool_call_inner(
        &mut self,
        tool_name: &str,
        tool_input: &Value,
        _tool_use_id: &str,
    ) -> (Value, String, bool) {
        if self.allowed_tools.as_ref().is_some_and(|names| !names.iter().any(|name| name == tool_name)) { return (tool_input.clone(), "Tool is not allowed by this agent definition".into(), true); }
        if self.permission_manager.as_ref().is_some_and(|pm| pm.get_mode() == crate::permissions::PermissionMode::PlanMode)
            && (matches!(tool_name, "Write" | "Edit") || (tool_name == "Read" && self.is_plan_document_tool(tool_name, tool_input))) {
            return self.execute_plan_document(tool_name, tool_input, _tool_use_id).await;
        }
        if matches!(tool_name, "EnterPlanMode" | "ExitPlanMode") || (tool_name == "plan_mode" && matches!(tool_input["operation"].as_str(), Some("enter" | "exit"))) {
            return self.execute_plan_transition(tool_name, tool_input, _tool_use_id).await;
        }
        if matches!(tool_name, "Agent" | "Delegate" | "Worker") || (tool_name == "team" && tool_input["operation"] == "run") {
            match self.check_permission(tool_name, tool_input, _tool_use_id).await {
                PermissionResult::Denied(reason) => return (tool_input.clone(), format!("Permission denied: {reason}"), true),
                PermissionResult::RequiresConfirmation(message) => return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await,
                PermissionResult::Granted => {},
            }
        }
        if tool_name == "team" && tool_input["operation"] == "run" {
            let result = Box::pin(self.execute_team(tool_input)).await;
            let failed = result.is_err();
            let output = match result { Ok(value) => value.to_string(), Err(error) => error.to_string() };
            return (tool_input.clone(), output, failed);
        }
        if matches!(tool_name, "TeamCreate" | "TeamDelete" | "TeamSpawn") {
            match self.check_permission(tool_name, tool_input, _tool_use_id).await {
                PermissionResult::Denied(reason) => return (tool_input.clone(), format!("Permission denied: {reason}"), true),
                PermissionResult::RequiresConfirmation(message) => return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await,
                PermissionResult::Granted => {},
            }
            let result = if tool_name == "TeamSpawn" {
                Box::pin(self.execute_team_spawn(tool_input)).await
            } else {
                match (self.search_config_path.as_deref().and_then(std::path::Path::parent), self.conv_id.as_deref()) {
                    (Some(root), Some(session)) => crate::tools::teams::execute(tool_name, tool_input, root, session, std::path::Path::new(&self.workspace_cwd), Some(&self.provider.model.id)),
                    _ => Err(anyhow::anyhow!("Team tools require an active session and runtime directory")),
                }
            };
            let failed = result.is_err();
            let output = match result { Ok(value) => value.to_string(), Err(error) => error.to_string() };
            return (tool_input.clone(), output, failed);
        }
        if matches!(tool_name, "TeamCreate" | "TeamDelete" | "TeamSpawn") {
            match self.check_permission(tool_name, tool_input, _tool_use_id).await {
                PermissionResult::Denied(reason) => return (tool_input.clone(), format!("Permission denied: {reason}"), true),
                PermissionResult::RequiresConfirmation(message) => return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await,
                PermissionResult::Granted => {},
            }
            let result = if tool_name == "TeamSpawn" {
                Box::pin(self.execute_team_spawn(tool_input)).await
            } else {
                match (self.search_config_path.as_deref().and_then(std::path::Path::parent), self.conv_id.as_deref()) {
                    (Some(root), Some(session)) => crate::tools::teams::execute(
                        tool_name,
                        tool_input,
                        root,
                        session,
                        std::path::Path::new(&self.workspace_cwd),
                        Some(&self.provider.model.id),
                    ),
                    _ => Err(anyhow::anyhow!("Team tools require an active session and runtime directory")),
                }
            };
            let failed = result.is_err();
            let output = match result { Ok(value) => value.to_string(), Err(error) => error.to_string() };
            return (tool_input.clone(), output, failed);
        }
        if tool_name == "plan_mode" {
            match self.check_permission(tool_name, tool_input, _tool_use_id).await {
                PermissionResult::Denied(reason) => return (tool_input.clone(), format!("Permission denied: {reason}"), true),
                PermissionResult::RequiresConfirmation(message) => return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await,
                PermissionResult::Granted => {}
            }
            let result = crate::tools::execute_tool_async(tool_name, tool_input.clone(), &self.workspace_cwd).await;
            let failed = result.is_err();
            let output = match result { Ok(value) => value.to_string(), Err(error) => error.to_string() };
            return (tool_input.clone(), output, failed);
        }
        if tool_name == "SendMessage" {
            match self.check_permission(tool_name, tool_input, _tool_use_id).await {
                PermissionResult::Denied(reason) => return (tool_input.clone(), format!("Team message access denied: {reason}"), true),
                PermissionResult::RequiresConfirmation(message) => return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await,
                PermissionResult::Granted => {}
            }
            let result = match (self.search_config_path.as_deref().and_then(std::path::Path::parent), self.conv_id.as_deref()) {
                (Some(root), Some(session)) => crate::services::session_messages::context(root).and_then(|context| crate::tools::agent_messages::execute_live(&context, session, tool_input.clone())),
                _ => Err(anyhow::anyhow!("Session-local message routing is unavailable")),
            };
            let failed = result.is_err();
            let mut result = result;
            if matches!(tool_input["operation"].as_str(), Some("send" | "shutdown_request" | "shutdown_response" | "plan_approval_response")) {
                if let Ok(value) = &mut result {
                    let targets: Vec<String> = value["delivered"].as_array().map(|items| items.iter().filter_map(|item| item["session_id"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
                    let mut resumes = Vec::new();
                    for recipient in targets {
                        let owned={let registry=self.owned_children.lock().unwrap_or_else(|e|e.into_inner());registry.get(&recipient).is_some_and(|entry|Some(entry.parent.as_str())==self.conv_id.as_deref())};
                        if !owned { continue; }
                        match Box::pin(self.resume_owned_child(&recipient,None,true)).await {
                            Ok(resumed)=>{resumes.push(json!({"session_id":recipient,"status":"resumed_in_owner_turn","resume":resumed}));},
                            Err(error)=>{resumes.push(json!({"session_id":recipient,"status":"needs_resume","reason":error.to_string()}));}
                        }
                    }
                    if !resumes.is_empty() { value["resumes"] = json!(resumes); value["delivery"] = json!("resumed_in_owner_turn"); }
                }
            }
            let output = match result { Ok(value) => value.to_string(), Err(error) => error.to_string() };
            return (tool_input.clone(), output, failed);
        }
        if matches!(tool_name, "Agent" | "Delegate" | "Worker") {
            let result = Box::pin(self.execute_delegated_authorized(tool_input, None, None, true)).await;
            let failed = result.is_err();
            let output = match result { Ok(value) => value.to_string(), Err(error) => format!("Delegation failed: {error}") };
            return (tool_input.clone(), output, failed);
        }
        if tool_name == "AskUserQuestion" {
            return self.execute_ask_user_question(tool_input, _tool_use_id).await;
        }
        if tool_name == "ProposeGoal" {
            let result = (|| -> Result<Value> {
                anyhow::ensure!(self.goal_proposals_available(), "ProposeGoal is only available in an attached local main session outside plan mode");
                crate::services::goal_proposals::propose(self.goal_db.as_ref().unwrap(), self.conv_id.as_deref().unwrap(), self.search_config_path.as_deref().unwrap().parent().unwrap(), std::path::Path::new(&self.workspace_cwd), self.permission_manager.as_ref().unwrap(), tool_input)
            })();
            let failed = result.is_err();
            return (tool_input.clone(), match result { Ok(value) => value.to_string(), Err(error) => error.to_string() }, failed);
        }

        if matches!(tool_name, "memory_search" | "memory_ingest") {
            match self.check_permission(tool_name, tool_input, _tool_use_id).await {
                PermissionResult::Denied(reason) => return (tool_input.clone(), format!("Permission denied: {reason}"), true),
                PermissionResult::RequiresConfirmation(message) => return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await,
                PermissionResult::Granted => {},
            }
        }
        // Route memory_search and memory_ingest directly to MemEx backend
        if tool_name == "memory_search" {
            return self.execute_memory_search(tool_input).await;
        }
        if tool_name == "memory_ingest" {
            return self.execute_memory_ingest(tool_input).await;
        }

        let permission_result = self.check_permission(tool_name, tool_input, _tool_use_id).await;
        match permission_result {
            PermissionResult::Denied(reason) => {
                return (tool_input.clone(), format!("Permission denied: {}", reason), true);
            }
            PermissionResult::RequiresConfirmation(message) => {
                return self.execute_ask_user_confirmation(tool_name, tool_input, _tool_use_id, &message).await;
            }
            PermissionResult::Granted => {}
        }

        let output_str;
        let mut is_error;

        if tool_name == "Workflow" {
            let result = Box::pin(self.execute_workflow(tool_input)).await;
            return (tool_input.clone(), match &result { Ok(value)=>value.to_string(),Err(error)=>format!("Workflow failed: {error:#}") }, result.is_err());
        }
        if crate::tools::tasks::is_task(tool_name) {
            let result = async {
                let root = self.search_config_path.as_deref().and_then(std::path::Path::parent).ok_or_else(|| anyhow!("Task runtime directory unavailable"))?;
                let session = self.conv_id.as_deref().ok_or_else(|| anyhow!("Task session unavailable"))?;
                let identity = crate::services::teams::identity(root, session)?;
                let payload = |task: &Value| {
                    let mut payload = json!({"task_id":task["id"],"task_subject":task["subject"],"task_description":task["description"]});
                    if let Some((team, teammate)) = &identity { payload["team_name"] = json!(team); payload["teammate_name"] = json!(teammate); }
                    payload
                };
                use crate::services::tool_hooks::HookEvent;
                if tool_name == "TaskCreate" {
                    let task = crate::tools::tasks::execute_for_session_async(tool_name, tool_input.clone(), root, session).await?;
                    if let Some(hooks) = &self.hooks {
                        let outcome = hooks.run(HookEvent::TaskCreated, session, payload(&task)).await?;
                        if outcome.blocked {
                            let id = task["id"].as_str().unwrap_or_default().to_owned();
                            let _ = crate::tools::tasks::execute_for_session_async("TaskUpdate", json!({"id":id,"status":"deleted"}), root, session).await;
                            anyhow::bail!("TaskCreated hook: {}", outcome.reason);
                        }
                    }
                    return Ok(task);
                }
                if tool_name == "TaskUpdate" && tool_input["status"] == "completed" {
                    let id = tool_input["id"].as_str().or_else(|| tool_input["taskId"].as_str()).ok_or_else(|| anyhow!("task id is required"))?.to_owned();
                    let existing = crate::tools::tasks::execute_for_session_async("TaskGet", json!({"id":id}), root, session).await?;
                    if existing["status"] != "completed" {
                        if let Some(hooks) = &self.hooks {
                            let outcome = hooks.run(HookEvent::TaskCompleted, session, payload(&existing)).await?;
                            if outcome.blocked {
                                return Ok(json!({"success":false,"taskId":id,"updatedFields":[],"error":format!("TaskCompleted hook: {}", outcome.reason)}));
                            }
                        }
                    }
                }
                crate::tools::tasks::execute_for_session_async(tool_name, tool_input.clone(), root, session).await
            }.await;
            return (tool_input.clone(), match &result { Ok(value) => value.to_string(), Err(error) => format!("Task failed: {error:#}") }, result.is_err());
        }

        if tool_name == "Skill" {
            let result = Box::pin(self.execute_skill_tool(tool_input)).await;
            return (tool_input.clone(), match &result { Ok(value)=>value.to_string(),Err(error)=>format!("Skill failed: {error:#}") }, result.is_err());
        }

        if let Some(ref registry) = self.mcp_registry {
            if registry.is_mcp_tool(tool_name).await {
                let event_tx = self.event_tx.clone();
                let waiters = self.answer_waiters.clone();
                let tool_use_id = _tool_use_id.to_owned();
                let durable_user_input = self.durable_user_input;
                let context = crate::mcp::elicitation::RequestContext {
                    hooks: self.hooks.clone(),
                    session: self.conv_id.clone().unwrap_or_default(),
                    ask: Arc::new(move |input| {
                        let event_tx = event_tx.clone();
                        let waiters = waiters.clone();
                        let tool_use_id = tool_use_id.clone();
                        Box::pin(async move {
                            let request_id = uuid::Uuid::new_v4().to_string();
                            let (tx, rx) = oneshot::channel();
                            waiters.lock().await.insert(request_id.clone(), tx);
                            let _guard = PermissionWaiterGuard {
                                waiters,
                                request_id: request_id.clone(),
                            };
                            let mut input = input;
                            input["subtype"] = serde_json::json!("elicitation");
                            event_tx.send(EngineEvent::AskUser {
                                request_id, tool_use_id, tool_input: input.clone(),
                                question: format!("MCP {}: {}\nSchema: {}\nEnter a JSON object to accept, or decline / cancel:", input["mcp_server_name"], input["message"], input["requested_schema"]),
                                options: vec![],
                            }).await.map_err(|_| anyhow!("Elicitation host closed"))?;
                            let answer = if durable_user_input {
                                rx.await
                            } else {
                                tokio::time::timeout(Duration::from_secs(300), rx)
                                    .await
                                    .map_err(|_| anyhow!("Elicitation timed out"))?
                            }
                            .map_err(|_| anyhow!("Elicitation cancelled"))?;
                            Ok(serde_json::from_str(&answer)?)
                        })
                    }),
                };
                let result = crate::mcp::elicitation::CONTEXT
                    .scope(
                        context,
                        registry.execute_with_provenance(tool_name, tool_input.clone()),
                    )
                    .await;
                match result {
                    Ok(result) => {
                        is_error = result.value["isError"].as_bool().unwrap_or(false);
                        if result.is_tool_call {
                            match McpOutput::from_transport(&result.value) {
                                Ok(content) => {
                                    output_str = content.display();
                                    self.mcp_outputs.insert(_tool_use_id.to_owned(), content);
                                }
                                Err(error) => { output_str = format!("Invalid MCP result: {error:#}"); is_error = true; }
                            }
                        } else { output_str = serde_json::to_string_pretty(&result.value).unwrap_or_default(); }
                    }
                    Err(e) => {
                        output_str = format!("Error: {}", e);
                        is_error = true;
                    }
                };
            } else {
                let cwd = self.get_workspace_cwd().to_string();
                let result = execute_builtin_tool(tool_name, tool_input.clone(), &cwd, &self.provider, &self.event_tx, self.search_config_path.as_deref(), self.skill_shell_active).await;
                output_str = match &result {
                    Ok(val) => serde_json::to_string_pretty(val).unwrap_or_default(),
                    Err(e) => format!("Error: {}", e),
                };
                is_error = result.is_err();
            }
        } else {
            let cwd = self.get_workspace_cwd().to_string();
            let result = execute_builtin_tool(tool_name, tool_input.clone(), &cwd, &self.provider, &self.event_tx, self.search_config_path.as_deref(), self.skill_shell_active).await;
            output_str = match &result {
                Ok(val) => serde_json::to_string_pretty(val).unwrap_or_default(),
                Err(e) => format!("Error: {}", e),
            };
            is_error = result.is_err();
        }

        (tool_input.clone(), output_str, is_error)
    }

    async fn execute_ask_user_question(&mut self, tool_input: &Value, tool_use_id: &str) -> (Value, String, bool) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let question = tool_input["question"].as_str().unwrap_or("").to_string();
        let options: Vec<String> = tool_input["options"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|opt| opt["label"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let (tx, rx) = oneshot::channel::<String>();
        {
            let mut waiters = self.answer_waiters.lock().await;
            waiters.insert(request_id.clone(), tx);
        }

        let _waiter_guard = PermissionWaiterGuard { waiters: self.answer_waiters.clone(), request_id: request_id.clone() };
        let _ = self.event_tx.send(EngineEvent::AskUser {
            request_id: request_id.clone(),
            tool_use_id: tool_use_id.to_owned(),
            tool_input: tool_input.clone(),
            question: question.clone(),
            options: options.clone(),
        }).await;

        match self.wait_for_user_answer(rx).await {
            Ok(Ok(answer)) => {
                let result = serde_json::json!({
                    "type": "ask_user_question",
                    "question": question,
                    "answer": answer,
                    "content": answer,
                    "requires_user_input": false
                });
                (tool_input.clone(), serde_json::to_string_pretty(&result).unwrap_or_default(), false)
            }
            Ok(Err(_)) => {
                let mut waiters = self.answer_waiters.lock().await;
                waiters.remove(&request_id);
                let result = serde_json::json!({
                    "type": "ask_user_question",
                    "question": question,
                    "content": "User did not respond",
                    "is_error": true
                });
                (tool_input.clone(), serde_json::to_string_pretty(&result).unwrap_or_default(), true)
            }
            Err(_) => {
                let mut waiters = self.answer_waiters.lock().await;
                waiters.remove(&request_id);
                let result = serde_json::json!({
                    "type": "ask_user_question",
                    "question": question,
                    "content": "User response timed out (300s)",
                    "is_error": true
                });
                (tool_input.clone(), serde_json::to_string_pretty(&result).unwrap_or_default(), true)
            }
        }
    }

    async fn execute_memory_search(&self, tool_input: &Value) -> (Value, String, bool) {
        let query = tool_input["query"].as_str().unwrap_or("");
        let top_k = tool_input["top_k"].as_u64().map(|v| v as usize);

        if let Some(ref client) = self.memex_client {
            match client.search(query, top_k).await {
                Ok(results) => {
                    let output = serde_json::json!({
                        "status": "search_completed",
                        "query": query,
                        "results": results,
                        "count": results.len()
                    });
                    (tool_input.clone(), serde_json::to_string_pretty(&output).unwrap_or_default(), false)
                }
                Err(e) => {
                    let output = serde_json::json!({
                        "status": "search_failed",
                        "query": query,
                        "error": format!("{}", e)
                    });
                    (tool_input.clone(), serde_json::to_string_pretty(&output).unwrap_or_default(), true)
                }
            }
        } else {
            let output = serde_json::json!({
                "status": "search_unavailable",
                "query": query,
                "message": "MemEx backend is not configured. Memory search is unavailable."
            });
            (tool_input.clone(), serde_json::to_string_pretty(&output).unwrap_or_default(), false)
        }
    }

    async fn execute_memory_ingest(&self, tool_input: &Value) -> (Value, String, bool) {
        let content = tool_input["content"].as_str().unwrap_or("");
        let importance = tool_input["importance"].as_f64();

        if let Some(ref client) = self.memex_client {
            match client.ingest(content, importance, None).await {
                Ok(()) => {
                    let output = serde_json::json!({
                        "status": "ingested",
                        "content_length": content.len(),
                        "importance": importance
                    });
                    (tool_input.clone(), serde_json::to_string_pretty(&output).unwrap_or_default(), false)
                }
                Err(e) => {
                    let output = serde_json::json!({
                        "status": "ingest_failed",
                        "error": format!("{}", e)
                    });
                    (tool_input.clone(), serde_json::to_string_pretty(&output).unwrap_or_default(), true)
                }
            }
        } else {
            let output = serde_json::json!({
                "status": "ingest_unavailable",
                "message": "MemEx backend is not configured. Memory ingest is unavailable."
            });
            (tool_input.clone(), serde_json::to_string_pretty(&output).unwrap_or_default(), false)
        }
    }

    async fn wait_for_user_answer(&self, receiver: oneshot::Receiver<String>) -> std::result::Result<std::result::Result<String, oneshot::error::RecvError>, tokio::time::error::Elapsed> {
        // A supervised worker owns a durable pending mailbox. Expiring only the
        // engine waiter leaves that mailbox presenting an approval that no longer
        // exists. Cancellation/shutdown aborts the owning turn and drops receiver;
        // channel closure still resolves immediately without executing the tool.
        if self.durable_user_input { Ok(receiver.await) }
        else { timeout(Duration::from_secs(300), receiver).await }
    }

    async fn execute_anthropic_loop(&mut self) -> Result<(String, Option<String>)> {
        tracing::info!(module = "AnthropicLoop", "ENTRY: building messages, msg_count={}", self.messages.len());
        let mut conversation_messages: Vec<AnthropicMessage> = self.build_anthropic_messages();
        tracing::info!(module = "AnthropicLoop", "Messages built, count={}", conversation_messages.len());

        let tools = self.tool_definitions();
        tracing::info!(module = "AnthropicLoop", "Tool definitions loaded, count={}, mode={:?}", tools.len(), self.reasoning_mode);

        if matches!(self.reasoning_mode, ReasoningMode::Deep) {
            if let Some(ref sp) = self.system_prompt {
                self.system_prompt = Some(format!("{}\n\nYou are in deep reasoning mode. For any destructive operations, you MUST ask for user confirmation before proceeding.", sp));
            } else {
                self.system_prompt = Some("You are in deep reasoning mode. For any destructive operations, you MUST ask for user confirmation before proceeding.".to_string());
            }
        }

        let mut full_text = String::new();
        let mut stop_reason = None;
        let base_url = self.provider.provider.base_url.clone();
        let model_id = self.provider.model.id.clone();
        let api_key_len = self.provider.provider.api_key.len();

        let enable_thinking = matches!(self.reasoning_mode, ReasoningMode::Deep);
        let mut output_recoveries = 0;

        for iteration in 0..self.max_tool_iterations {
            for context in self.drain_async_hook_messages() {
                conversation_messages.push(AnthropicMessage {
                    role: "user".into(),
                    content: AnthropicContent::Text(format!(
                        "<system-reminder>\nAsync hook context:\n{context}\n</system-reminder>"
                    )),
                });
            }
            self.ensure_auto_not_exhausted()?;
            self.ensure_permission_continuation()?;
            self.resume_pending_owned_children().await;
            let queued = self.deliver_queued_inputs(iteration).await;
            for content in self.drain_approval_notes().into_iter().chain(queued).chain(self.drain_peer_messages()?) {
                // A queued message that names pasted images carries them.
                let content = match self.queued_images(&content) {
                    Some(blocks) => AnthropicContent::Blocks(blocks.into_iter().filter_map(|block| serde_json::from_value(block).ok()).collect()),
                    None => AnthropicContent::Text(content),
                };
                conversation_messages.push(AnthropicMessage { role: "user".into(), content });
            }

            self.auto_history = conversation_messages.iter().filter_map(|message| serde_json::to_value(message).ok()).collect();
            if self.response_schema.is_some() { full_text.clear(); }
            let round_text_start = full_text.len();
            tracing::info!(module = "AnthropicLoop", "Iteration {}/{} starting, base_url={}, model={}", iteration+1, self.max_tool_iterations, base_url, model_id);
            tracing::info!(module = "AnthropicLoop", "API key length={}", api_key_len);

            // Context compression check before each API call
            if iteration > 0 {
                let model_context_limit = self.provider.model.context_window.map(|n| (n as f64 * 0.90) as usize).unwrap_or_else(|| TokenCounter::get_safe_limit(&model_id, 0.90));
                let conv_values: Vec<Value> = conversation_messages.iter().filter_map(|m| {
                    match &m.content {
                        AnthropicContent::Text(t) => Some(json!({"role": m.role, "content": t})),
                        AnthropicContent::Blocks(blocks) => {
                            let content_blocks: Vec<Value> = blocks.iter().filter_map(|b| {
                                match b {
                                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                                    ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                                        Some(json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}))
                                    }
                                    ContentBlock::ToolUse { id, name, input } => {
                                        Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
                                    }
                                    _ => None,
                                }
                            }).collect();
                            Some(json!({"role": m.role, "content": content_blocks}))
                        }
                    }
                }).collect();
                let estimated = TokenCounter::estimate(&conv_values);
                if estimated > (model_context_limit * 70 / 100) {
                    tracing::info!(module = "AnthropicLoop", "Context compression needed: {} tokens (limit: {})", estimated, model_context_limit);
                    let compressor = ContextCompressor::new(None);
                    let context_window = ContextWindow {
                        model_id: model_id.clone(),
                        total_tokens: estimated as u64,
                        reserved_for_completion: (self.max_tokens as u64) * 2,
                        context_window: model_context_limit as u64,
                    };
                    let level = compressor.determine_compression_level(&context_window);
                    if level != CompressionLevel::None {
                        let conv_values: Vec<Value> = conversation_messages.iter().filter_map(|m| {
                            match &m.content {
                                AnthropicContent::Text(t) => Some(json!({"role": m.role, "content": t})),
                                AnthropicContent::Blocks(blocks) => {
                                    let content_blocks: Vec<Value> = blocks.iter().filter_map(|b| {
                                        match b {
                                            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                                            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                                                Some(json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}))
                                            }
                                            ContentBlock::ToolUse { id, name, input } => {
                                                Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
                                            }
                                            _ => None,
                                        }
                                    }).collect();
                                    Some(json!({"role": m.role, "content": content_blocks}))
                                }
                            }
                        }).collect();
                        let (compressed, metadata) = compressor.compress(
                            conv_values,
                            self.system_prompt.clone(),
                            level,
                            Some(&self.provider),
                        ).await;
                        if metadata.was_compressed {
                            conversation_messages = compressed.iter().filter_map(|v| {
                                let role = v.get("role")?.as_str()?.to_string();
                                let content = v.get("content")?;
                                let anthropic_content = if content.is_string() {
                                    AnthropicContent::Text(content.as_str()?.to_string())
                                } else if content.is_array() {
                                    let blocks: Vec<ContentBlock> = content.as_array()?.iter().filter_map(|b| {
                                        let bt = b.get("type")?.as_str()?;
                                        match bt {
                                            "text" => Some(ContentBlock::Text { text: b.get("text")?.as_str()?.to_string() }),
                                            "tool_result" => Some(ContentBlock::ToolResult {
                                                tool_use_id: b.get("tool_use_id")?.as_str()?.to_string(),
                                                content: b.get("content")?.clone(),
                                                is_error: b.get("is_error").and_then(|v| v.as_bool()),
                                            }),
                                            "tool_use" => Some(ContentBlock::ToolUse {
                                                id: b.get("id")?.as_str()?.to_string(),
                                                name: b.get("name")?.as_str()?.to_string(),
                                                input: b.get("input")?.clone(),
                                            }),
                                            _ => None,
                                        }
                                    }).collect();
                                    AnthropicContent::Blocks(blocks)
                                } else {
                                    return None;
                                };
                                Some(AnthropicMessage { role, content: anthropic_content })
                            }).collect();
                            tracing::info!(module = "AnthropicLoop", "Context compressed: {} -> {} messages, saved {} tokens",
                                metadata.original_count, metadata.compressed_count, metadata.token_reduction);
                        }
                    }
                }
            }

            self.streaming_tool_args.clear();
            let mut request_trace = RequestTrace::new(json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "number": iteration + 1,
                "startedAt": chrono::Utc::now().timestamp_millis(),
                "model": self.provider.model.id,
                "providerId": self.provider.provider.id,
                "providerName": self.provider.provider.name,
                "apiFormat": self.provider.provider.api_format,
                "maxOutputTokens": self.max_tokens,
                "contextWindow": self.provider.model.context_window.map(|n| n as usize).unwrap_or_else(|| TokenCounter::get_model_context_limit(&self.provider.model.id)),
                "reservedForCompletion": (self.max_tokens as u64) * 2,
                "effort": self.provider.model.effort,
                "systemPrompt": self.system_prompt,
                "systemMessages": conversation_messages.iter().filter(|m| m.role == "system").collect::<Vec<_>>(),
                "tools": tools,
                "source": "native_engine"
            }));
            let _ = self.event_tx.send(EngineEvent::RequestSnapshot(request_trace.started())).await;
            let context_capture = request_trace.capture_context(self.event_tx.clone());
            let stream = self.anthropic_client
                .send_message_stream_with_thinking_observed(
                    &self.provider,
                    conversation_messages.clone(),
                    self.system_prompt.as_deref(),
                    tools.clone(),
                    self.max_tokens,
                    enable_thinking,
                    Some(&context_capture),
                )
                .await;

            let mut stream = match stream {
                Ok(s) => {
                    tracing::info!(module = "AnthropicLoop", "Stream created successfully");
                    s
                }
                Err(e) => {
                    let err_msg = format!("API request failed: {}", e);
                    tracing::error!(module = "AnthropicLoop", "Stream creation FAILED: {}", err_msg);
                    let _ = self.event_tx.send(EngineEvent::RequestSnapshot(request_trace.finish("error", Some(&err_msg)))).await;
                    let _ = self.event_tx.send(EngineEvent::Error(err_msg.clone())).await;
                    return Err(anyhow!(err_msg));
                }
            };

            let mut sse_buffer = String::new();
            let mut has_tool_use = false;
            let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
            let mut current_text = String::new();
            let mut current_thinking = String::new();
            let mut current_tool_use_id: Option<String> = None;
            let mut current_tool_name: Option<String> = None;
            let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();

            loop {
                let chunk_result = match stream.next().await {
                    Some(chunk) => chunk,
                    // Some providers omit the delimiter on their final SSE event.
                    None if !sse_buffer.is_empty() => Ok("\n\n".to_string()),
                    None => break,
                };
                if self.event_tx.is_closed() {
                    drop(stream);
                    break;
                }
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        let err_msg = format!("Stream error: {}", e);
                        tracing::error!(module = "AnthropicLoop", "Stream chunk error: {}", err_msg);
                        let _ = self.event_tx.send(EngineEvent::RequestSnapshot(request_trace.finish("error", Some(&err_msg)))).await;
                        let _ = self.event_tx.send(EngineEvent::Error(err_msg)).await;
                        let _ = self.event_tx.send(EngineEvent::MessageStop {
                            full_text: full_text.clone(),
                            stop_reason: Some("stream_error".to_string()),
                        }).await;
                        drop(stream);
                        return Ok((full_text, Some("stream_error".to_string())));
                    }
                };

                sse_buffer.push_str(&chunk);
                let consumed = consume_sse_payloads(&sse_buffer);
                sse_buffer = consumed.remainder;

                for payload in &consumed.payloads {
                    let event: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    request_trace.observe_anthropic(&event);
                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match event_type {
                        "message_start" => {
                            if let Some(message) = event.get("message") {
                                if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
                                    let _ = self.event_tx.send(EngineEvent::MessageStart {
                                        model: model.to_string(),
                                    }).await;
                                }
                                if let Some(usage) = message.get("usage") {
                                    if let Some(input_tokens) = usage.get("input_tokens").and_then(|v| v.as_f64()) {
                                        crate::metrics::TOKENS_CONSUMED.inc_by(input_tokens);
                                    }
                                    if let Some(output_tokens) = usage.get("output_tokens").and_then(|v| v.as_f64()) {
                                        crate::metrics::TOKENS_CONSUMED.inc_by(output_tokens);
                                    }
                                    let _ = self.event_tx.send(EngineEvent::Usage(usage.clone())).await;
                                }
                            }
                        }
                        "content_block_start" => {
                            let block = event.get("content_block");
                            let block_type = block.and_then(|b| b.get("type")).and_then(|t| t.as_str()).unwrap_or("");

                            match block_type {
                                "text" => {
                                    current_text.clear();
                                }
                                "thinking" => {
                                    current_thinking.clear();
                                }
                                "tool_use" => {
                                    has_tool_use = true;
                                    let id = block.and_then(|b| b.get("id")).and_then(|i| i.as_str()).unwrap_or("").to_string();
                                    let name = block.and_then(|b| b.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
                                    current_tool_use_id = Some(id.clone());
                                    current_tool_name = Some(name.clone());

                                    // Announce only after the complete arguments arrive. Consumers
                                    // use this event to capture pre-edit file contents/checkpoints.
                                }
                                _ => {}
                            }
                        }
                        "content_block_delta" => {
                            let delta = event.get("delta");
                            let delta_type = delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()).unwrap_or("");

                            match delta_type {
                                "text_delta" => {
                                    let text = delta.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("");
                                    if !text.is_empty() {
                                        current_text.push_str(text);
                                        full_text.push_str(text);
                                        let _ = self.event_tx.send(EngineEvent::Text(text.to_string())).await;
                                    }
                                }
                                "thinking_delta" => {
                                    let thinking = delta.and_then(|d| d.get("thinking")).and_then(|t| t.as_str()).unwrap_or("");
                                    if !thinking.is_empty() {
                                        current_thinking.push_str(thinking);
                                        let _ = self.event_tx.send(EngineEvent::Thinking(thinking.to_string())).await;
                                    }
                                }
                                "input_json_delta" => {
                                    let partial = delta.and_then(|d| d.get("partial_json")).and_then(|p| p.as_str()).unwrap_or("");
                                    if !partial.is_empty() {
                                        if let (Some(ref id), Some(ref name)) = (&current_tool_use_id, &current_tool_name) {
                                            self.handle_streaming_tool_arg_delta(id, name, partial);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            let _index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                            if !current_text.is_empty() {
                                assistant_blocks.push(ContentBlock::Text { text: current_text.clone() });
                                current_text.clear();
                            } else if !current_thinking.is_empty() {
                                assistant_blocks.push(ContentBlock::Thinking {
                                    thinking: current_thinking.clone(),
                                    signature: None,
                                });
                                current_thinking.clear();
                            } else if current_tool_use_id.is_some() {
                                let id = current_tool_use_id.clone().unwrap_or_default();
                                let name = current_tool_name.clone().unwrap_or_default();
                                let input = self.finalize_streaming_tool_args(&id);

                                let _ = self.event_tx.send(EngineEvent::ToolUseStart {
                                    tool_use_id: id.clone(),
                                    tool_name: name.clone(),
                                    tool_input: input.clone(),
                                    text_before: full_text.clone(),
                                }).await;

                                // Memory calls need this executor's configured client, not a generic stub.
                                let requires_user_interaction = matches!(
                                    name.as_str(),
                                    "Workflow"
                                        | "Skill"
                                        | "Agent"
                                        | "Delegate"
                                        | "Worker"
                                        | "SendMessage"
                                        | "TeamCreate"
                                        | "TeamDelete"
                                        | "TeamSpawn"
                                        | "team"
                                        | "plan_mode"
                                        | "EnterPlanMode"
                                        | "ExitPlanMode"
                                ) || matches!(
                                    name.as_str(),
                                    "AskUserQuestion" | "ProposeGoal"
                                ) || name.starts_with("mcp__")
                                    || matches!(
                                        name.as_str(),
                                        "ListMcpResources" | "ReadMcpResource"
                                    )
                                    || crate::tools::tasks::is_task(&name)
                                    || is_memory_tool(&name)
                                    || self.hooks.is_some()
                                    || self.is_plan_document_tool(&name, &input)
                                    || self.check_permission_requires_confirmation(&name, &input);

                                assistant_blocks.push(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                });

                                pending_tool_calls.push(PendingToolCall {
                                    id,
                                    name,
                                    input,
                                    requires_user_interaction,
                                });

                                current_tool_use_id = None;
                                current_tool_name = None;
                            }
                        }
                        "message_delta" => {
                            let delta = event.get("delta");
                            let sr = delta.and_then(|d| d.get("stop_reason")).and_then(|s| s.as_str()).map(String::from);
                            if sr.is_some() {
                                stop_reason = sr.clone();
                                let _ = self.event_tx.send(EngineEvent::MessageDelta {
                                    stop_reason: sr,
                                }).await;
                            }
                            if let Some(usage) = event.get("usage") {
                                if let Some(output_tokens) = usage.get("output_tokens").and_then(|v| v.as_f64()) {
                                    crate::metrics::TOKENS_CONSUMED.inc_by(output_tokens);
                                }
                                let _ = self.event_tx.send(EngineEvent::Usage(usage.clone())).await;
                            }
                        }
                        "message_stop" => {}
                        "ping" => {}
                        _ => {}
                    }
                }
            }

            let interrupted = self.event_tx.is_closed();
            let snapshot = request_trace.finish(if interrupted { "interrupted" } else { "completed" }, None);
            let response_status = snapshot["status"].as_str().unwrap_or("interrupted").to_string();
            let response_error = snapshot["error"].as_str().unwrap_or("Model response ended before completion").to_string();
            let _ = self.event_tx.send(EngineEvent::RequestSnapshot(snapshot)).await;
            // A response cut off by the output limit is "incomplete" in the
            // trace; continue it below instead of ending the turn.
            let continue_cut_off = response_status == "incomplete"
                && stop_reason.as_deref() == Some("max_tokens")
                && output_recoveries < MAX_OUTPUT_RECOVERIES
                && iteration + 1 < self.max_tool_iterations;
            if response_status != "completed" && !continue_cut_off {
                let _ = self.event_tx.send(EngineEvent::Error(response_error)).await;
                return Ok((full_text, Some(response_status)));
            }
            if stop_reason.as_deref() == Some("max_tokens")
                && output_recoveries < MAX_OUTPUT_RECOVERIES
                && iteration + 1 < self.max_tool_iterations
            {
                output_recoveries += 1;
                // Never run a call whose arguments were cut off.
                for call in &pending_tool_calls {
                    let _ = self.event_tx.send(EngineEvent::ToolUseDone {
                        tool_use_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        tool_input: call.input.clone(),
                        output: "Not run: the response reached the output token limit before this call was complete.".into(),
                        is_error: true,
                    }).await;
                }
                assistant_blocks.retain(|block| matches!(block, ContentBlock::Text { .. }));
                if !assistant_blocks.is_empty() {
                    conversation_messages.push(AnthropicMessage { role: "assistant".into(), content: AnthropicContent::Blocks(assistant_blocks) });
                }
                conversation_messages.push(AnthropicMessage { role: "user".into(), content: AnthropicContent::Text(output_limit_resume(self.max_tokens)) });
                let _ = self.event_tx.send(output_limit_notice(self.max_tokens, output_recoveries)).await;
                stop_reason = None;
                continue;
            }
            if has_tool_use && !pending_tool_calls.is_empty() {
                let mut tool_results: Vec<ContentBlock> = Vec::new();
                if pending_tool_calls.iter().any(|t| matches!(t.name.as_str(), "ExitPlanMode" | "plan_mode")) {
                    for call in &mut pending_tool_calls { call.requires_user_interaction = true; }
                }

                let (serial_calls, parallel_calls): (Vec<_>, Vec<_>) = pending_tool_calls
                    .into_iter()
                    .partition::<Vec<_>, _>(|tc| tc.requires_user_interaction);

                for tc in &serial_calls {
                    let (actual_input, output_str, is_error) = self.execute_tool_call(&tc.name, &tc.input, &tc.id).await;
                    for block in &mut assistant_blocks {
                        if let ContentBlock::ToolUse { id, input, .. } = block { if id == &tc.id { *input = actual_input.clone(); } }
                    }
                    let mcp = self.mcp_outputs.remove(&tc.id);
                    let mcp = self.finish_mcp_output(&tc.id, &tc.name, &output_str, is_error, mcp)?;
                    if !is_error { if let Some(manager) = &self.permission_manager { manager.record_tool_success(self.conv_id.as_deref().unwrap_or("")); } }
                    let _ = self.event_tx.send(EngineEvent::ToolUseDone {
                        tool_use_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        tool_input: actual_input,
                        output: output_str.clone(),
                        is_error,
                    }).await;
                    let content = mcp.map(|value|value.content).unwrap_or_else(||json!(self.retained_tool_output(&tc.name, &output_str)));
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content,
                        is_error: Some(is_error),
                    });
                }

                self.ensure_permission_continuation()?;
                if !parallel_calls.is_empty() {
                    let total = parallel_calls.len();
                    let completed_count = Arc::new(AtomicUsize::new(serial_calls.len()));
                    let event_tx_clone = self.event_tx.clone();
                    let mcp_registry_clone = self.mcp_registry.clone();
                    let workspace_cwd_clone = self.workspace_cwd.clone();
                    let permission_manager_clone = self.permission_manager.clone();
                    let conv_id_clone = self.conv_id.clone();

                    let mut futures = Vec::new();
                    for tc in parallel_calls {
                        let fut = execute_tool_standalone(
                            tc.name,
                            tc.input.clone(),
                            tc.id.clone(),
                            event_tx_clone.clone(),
                            mcp_registry_clone.clone(),
                            workspace_cwd_clone.clone(),
                            permission_manager_clone.clone(),
                            conv_id_clone.clone(),
                            completed_count.clone(),
                            total + serial_calls.len(),
                            self.provider.clone(),
                            self.search_config_path.clone(),
                            self.allowed_tools.clone(),
                            self.goal_db.clone(),
                        );
                        futures.push(fut);
                    }

                    let results = futures::future::join_all(futures).await;

                    for (tool_use_id, tool_name, _tool_input, output_str, is_error, mcp) in results {
                        let mcp = self.finish_mcp_output(&tool_use_id, &tool_name, &output_str, is_error, mcp)?;
                        let content = mcp.map(|value|value.content).unwrap_or_else(||json!(self.retained_tool_output(&tool_name, &output_str)));
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content,
                            is_error: Some(is_error),
                        });
                    }
                }

                conversation_messages.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicContent::Blocks(assistant_blocks),
                });

                // All results for one assistant turn belong in a single user message.
                conversation_messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(tool_results),
                });
                if let Some((boundary_id, content)) = self.pending_plan_reset.take() {
                    self.messages = vec![json!({"role":"user","content":content})];
                    conversation_messages = vec![AnthropicMessage { role: "user".into(), content: AnthropicContent::Text(content) }];
                    full_text.clear();
                    self.messages.clear();
                    self.stop_hook_active = false;
                    let _ = self.event_tx.send(EngineEvent::ContextReset { session_id: self.conv_id.clone().unwrap_or_default(), boundary_id }).await;
                }
            } else {
                let incoming = if iteration + 1 < self.max_tool_iterations { self.drain_peer_messages()? } else { Vec::new() };
                if !incoming.is_empty() {
                    conversation_messages.push(AnthropicMessage { role: "assistant".into(), content: AnthropicContent::Blocks(assistant_blocks) });
                    for content in incoming { conversation_messages.push(AnthropicMessage { role: "user".into(), content: AnthropicContent::Text(content) }); }
                    continue;
                }
                if let Some(follow_up) = self.stop_hook(&full_text[round_text_start..]).await? {
                    anyhow::ensure!(iteration + 1 < self.max_tool_iterations, "Stop hook requested continuation after the tool iteration limit");
                    conversation_messages.push(AnthropicMessage { role: "assistant".into(), content: AnthropicContent::Blocks(assistant_blocks) });
                    conversation_messages.push(AnthropicMessage { role: "user".into(), content: AnthropicContent::Text(follow_up) });
                    continue;
                }
                break;
            }

            if iteration == self.max_tool_iterations - 1 {
                stop_reason = Some("max_iterations".to_string());
                let _ = self.event_tx.send(EngineEvent::Error("Max tool iterations reached".to_string())).await;
                let _ = self.event_tx.send(EngineEvent::MessageStop {
                    full_text: full_text.clone(),
                    stop_reason: Some("max_iterations".to_string()),
                }).await;
                break;
            }

        }

        Ok((full_text, stop_reason))
    }

    async fn execute_openai_loop(&mut self) -> Result<(String, Option<String>)> {
        tracing::info!(module = "OpenAILoop", "ENTRY: building messages, msg_count={}", self.messages.len());
        let mut conversation_messages: Vec<OpenAIMessage> = self.build_openai_messages();
        tracing::info!(module = "OpenAILoop", "Messages built, count={}", conversation_messages.len());

        let tools = self.tool_definitions();
        tracing::info!(module = "OpenAILoop", "Tool definitions loaded, count={}, mode={:?}", tools.len(), self.reasoning_mode);

        if matches!(self.reasoning_mode, ReasoningMode::Deep) {
            if let Some(ref sp) = self.system_prompt {
                self.system_prompt = Some(format!("{}\n\nYou are in deep reasoning mode. For any destructive operations, you MUST ask for user confirmation before proceeding.", sp));
            } else {
                self.system_prompt = Some("You are in deep reasoning mode. For any destructive operations, you MUST ask for user confirmation before proceeding.".to_string());
            }
        }

        let mut full_text = String::new();
        let mut stop_reason = None;
        let base_url = self.provider.provider.base_url.clone();
        let model_id = self.provider.model.id.clone();
        let api_key_len = self.provider.provider.api_key.len();
        let mut output_recoveries = 0;

        for iteration in 0..self.max_tool_iterations {
            for context in self.drain_async_hook_messages() {
                conversation_messages.push(OpenAIMessage {
                    role: "user".into(),
                    content: OpenAIContent::Text(format!(
                        "<system-reminder>\nAsync hook context:\n{context}\n</system-reminder>"
                    )),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    provider_content: None,
                });
            }
            self.ensure_auto_not_exhausted()?;
            self.ensure_permission_continuation()?;
            self.resume_pending_owned_children().await;
            let queued = self.deliver_queued_inputs(iteration).await;
            for content in self.drain_approval_notes().into_iter().chain(queued).chain(self.drain_peer_messages()?) {
                let content = match self.queued_images(&content) {
                    Some(blocks) => OpenAIContent::Multi(blocks.iter().map(|block| match block["type"].as_str() {
                        Some("image") => OpenAIContentPart::Image { image_url: ImageUrl { url: format!("data:{};base64,{}", block["source"]["media_type"].as_str().unwrap_or("image/png"), block["source"]["data"].as_str().unwrap_or_default()) } },
                        _ => OpenAIContentPart::Text { text: block["text"].as_str().unwrap_or_default().to_owned() },
                    }).collect()),
                    None => OpenAIContent::Text(content),
                };
                conversation_messages.push(OpenAIMessage { role: "user".into(), content, tool_calls: None, tool_call_id: None, reasoning_content: None, provider_content: None });
            }

            self.auto_history = conversation_messages.iter().filter_map(|message| serde_json::to_value(message).ok()).collect();
            if self.response_schema.is_some() { full_text.clear(); }
            let round_text_start = full_text.len();
            tracing::info!(module = "OpenAILoop", "Iteration {}/{} starting, base_url={}, model={}", iteration+1, self.max_tool_iterations, base_url, model_id);
            tracing::info!(module = "OpenAILoop", "API key length={}", api_key_len);

            // Context compression check before each API call
            if iteration > 0 {
                let model_context_limit = self.provider.model.context_window.map(|n| (n as f64 * 0.90) as usize).unwrap_or_else(|| TokenCounter::get_safe_limit(&model_id, 0.90));
                let conv_values: Vec<Value> = conversation_messages.iter().filter_map(|m| {
                    let content_value = match &m.content {
                        OpenAIContent::Text(t) => json!(t),
                        OpenAIContent::Multi(parts) => {
                            let arr: Vec<Value> = parts.iter().filter_map(|p| {
                                match p {
                                    crate::native_engine::openai_client::OpenAIContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                                    crate::native_engine::openai_client::OpenAIContentPart::Image { image_url } => Some(json!({"type": "image_url", "image_url": {"url": &image_url.url}})),
                                }
                            }).collect();
                            json!(arr)
                        }
                    };
                    let mut obj = json!({"role": m.role, "content": content_value});
                    if let Some(ref tc) = m.tool_calls {
                        if let Ok(tc_val) = serde_json::to_value(tc) {
                            obj["tool_calls"] = tc_val;
                        }
                    }
                    if let Some(ref tid) = m.tool_call_id {
                        obj["tool_call_id"] = json!(tid);
                    }
                    Some(obj)
                }).collect();
                let estimated = TokenCounter::estimate(&conv_values);
                if estimated > (model_context_limit * 70 / 100) {
                    tracing::info!(module = "OpenAILoop", "Context compression needed: {} tokens (limit: {})", estimated, model_context_limit);
                    let compressor = ContextCompressor::new(None);
                    let context_window = ContextWindow {
                        model_id: model_id.clone(),
                        total_tokens: estimated as u64,
                        reserved_for_completion: (self.max_tokens as u64) * 2,
                        context_window: model_context_limit as u64,
                    };
                    let level = compressor.determine_compression_level(&context_window);
                    if level != CompressionLevel::None {
                        let conv_values: Vec<Value> = conversation_messages.iter().filter_map(|m| {
                            let content_value = match &m.content {
                                OpenAIContent::Text(t) => json!(t),
                                OpenAIContent::Multi(parts) => {
                                    let arr: Vec<Value> = parts.iter().filter_map(|p| {
                                        match p {
                                            crate::native_engine::openai_client::OpenAIContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                                            crate::native_engine::openai_client::OpenAIContentPart::Image { image_url } => Some(json!({"type": "image_url", "image_url": {"url": &image_url.url}})),
                                        }
                                    }).collect();
                                    json!(arr)
                                }
                            };
                            let mut obj = json!({"role": m.role, "content": content_value});
                            if let Some(ref tc) = m.tool_calls {
                                if let Ok(tc_val) = serde_json::to_value(tc) {
                                    obj["tool_calls"] = tc_val;
                                }
                            }
                            if let Some(ref tid) = m.tool_call_id {
                                obj["tool_call_id"] = json!(tid);
                            }
                            Some(obj)
                        }).collect();
                        let (compressed, metadata) = compressor.compress(
                            conv_values,
                            self.system_prompt.clone(),
                            level,
                            Some(&self.provider),
                        ).await;
                        if metadata.was_compressed {
                            conversation_messages = compressed.iter().filter_map(|v| {
                                let role = v.get("role")?.as_str()?.to_string();
                                let content = v.get("content")?;
                                let openai_content = if content.is_string() {
                                    OpenAIContent::Text(content.as_str()?.to_string())
                                } else if content.is_array() {
                                    let parts: Vec<crate::native_engine::openai_client::OpenAIContentPart> = content.as_array()?.iter().filter_map(|p| {
                                        let pt = p.get("type")?.as_str()?;
                                        match pt {
                                            "text" => Some(crate::native_engine::openai_client::OpenAIContentPart::Text { text: p.get("text")?.as_str()?.to_string() }),
                                            "image_url" => Some(crate::native_engine::openai_client::OpenAIContentPart::Image {
                                                image_url: crate::native_engine::openai_client::ImageUrl { url: p.get("image_url")?.get("url")?.as_str()?.to_string() },
                                            }),
                                            _ => None,
                                        }
                                    }).collect();
                                    OpenAIContent::Multi(parts)
                                } else {
                                    return None;
                                };
                                let tool_calls = v.get("tool_calls").and_then(|tc| serde_json::from_value(tc.clone()).ok());
                                let tool_call_id = v.get("tool_call_id").and_then(|id| id.as_str()).map(String::from);
                                Some(OpenAIMessage { role, content: openai_content, tool_calls, tool_call_id, reasoning_content: None, provider_content: None })
                            }).collect();
                            tracing::info!(module = "OpenAILoop", "Context compressed: {} -> {} messages, saved {} tokens",
                                metadata.original_count, metadata.compressed_count, metadata.token_reduction);
                        }
                    }
                }
            }

            self.streaming_tool_args.clear();
            let mut request_trace = RequestTrace::new(json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "number": iteration + 1,
                "startedAt": chrono::Utc::now().timestamp_millis(),
                "model": self.provider.model.id,
                "providerId": self.provider.provider.id,
                "providerName": self.provider.provider.name,
                "apiFormat": self.provider.provider.api_format,
                "maxOutputTokens": self.max_tokens,
                "contextWindow": self.provider.model.context_window.map(|n| n as usize).unwrap_or_else(|| TokenCounter::get_model_context_limit(&self.provider.model.id)),
                "reservedForCompletion": (self.max_tokens as u64) * 2,
                "effort": self.provider.model.effort,
                "systemPrompt": self.system_prompt,
                "systemMessages": conversation_messages.iter().filter(|m| m.role == "system").collect::<Vec<_>>(),
                "tools": tools,
                "source": "native_engine"
            }));
            let _ = self.event_tx.send(EngineEvent::RequestSnapshot(request_trace.started())).await;
            let context_capture = request_trace.capture_context(self.event_tx.clone());
            let stream = self.openai_client
                .send_message_stream_observed(
                    &self.provider,
                    conversation_messages.clone(),
                    self.system_prompt.as_deref(),
                    tools.clone(),
                    self.max_tokens,
                    Some(&context_capture),
                )
                .await;

            let mut stream = match stream {
                Ok(s) => {
                    tracing::info!(module = "OpenAILoop", "Stream created successfully");
                    s
                }
                Err(e) => {
                    let err_msg = format!("API request failed: {}", e);
                    tracing::error!(module = "OpenAILoop", "Stream creation FAILED: {}", err_msg);
                    let _ = self.event_tx.send(EngineEvent::RequestSnapshot(request_trace.finish("error", Some(&err_msg)))).await;
                    let _ = self.event_tx.send(EngineEvent::Error(err_msg.clone())).await;
                    return Err(anyhow!(err_msg));
                }
            };

            let mut sse_buffer = String::new();
            let mut has_tool_calls = false;
            let mut assistant_content: Option<OpenAIContent> = None;
            let mut assistant_reasoning: Option<String> = None;
            let mut assistant_provider_content: Option<Value> = None;
            let mut assistant_tool_calls: Vec<crate::native_engine::openai_client::OpenAIToolCall> = Vec::new();
            let mut openai_tool_args: HashMap<usize, (String, String, String)> = HashMap::new();

            loop {
                let chunk_result = match stream.next().await {
                    Some(chunk) => chunk,
                    None if !sse_buffer.is_empty() => Ok("\n\n".to_string()),
                    None => break,
                };
                if self.event_tx.is_closed() {
                    drop(stream);
                    break;
                }
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        let err_msg = format!("Stream error: {}", e);
                        tracing::error!(module = "OpenAILoop", "Stream chunk error: {}", err_msg);
                        let _ = self.event_tx.send(EngineEvent::RequestSnapshot(request_trace.finish("error", Some(&err_msg)))).await;
                        let _ = self.event_tx.send(EngineEvent::Error(err_msg)).await;
                        let _ = self.event_tx.send(EngineEvent::MessageStop {
                            full_text: full_text.clone(),
                            stop_reason: Some("stream_error".to_string()),
                        }).await;
                        drop(stream);
                        return Ok((full_text, Some("stream_error".to_string())));
                    }
                };

                sse_buffer.push_str(&chunk);
                let consumed = consume_sse_payloads(&sse_buffer);
                sse_buffer = consumed.remainder;

                for payload in &consumed.payloads {
                    if payload == "[DONE]" {
                        request_trace.mark_completed();
                        continue;
                    }

                    let event: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    request_trace.observe_openai(&event);
                    if let Some(content) = event.get("provider_content") { assistant_provider_content = Some(content.clone()); }
                    // Usage-only final chunks have no choices and must still be recorded.
                    if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
                        if let Some(tokens) = usage.get("prompt_tokens").and_then(|v| v.as_f64()) { crate::metrics::TOKENS_CONSUMED.inc_by(tokens); }
                        if let Some(tokens) = usage.get("completion_tokens").and_then(|v| v.as_f64()) { crate::metrics::TOKENS_CONSUMED.inc_by(tokens); }
                        let _ = self.event_tx.send(EngineEvent::Usage(usage.clone())).await;
                    }
                    let choices = match event.get("choices").and_then(|c| c.as_array()) {
                        Some(c) => c,
                        None => continue,
                    };

                    for choice in choices {
                        let delta = match choice.get("delta") {
                            Some(d) => d,
                            None => continue,
                        };

                        if let Some(role) = delta.get("role").and_then(|r| r.as_str()) {
                            if role == "assistant" {
                                let _ = self.event_tx.send(EngineEvent::MessageStart {
                                    model: self.provider.model.id.clone(),
                                }).await;
                            }
                        }

                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                            if !content.is_empty() {
                                full_text.push_str(content);
                                let _ = self.event_tx.send(EngineEvent::Text(content.to_string())).await;
                                match &assistant_content {
                                    None => {
                                        assistant_content = Some(OpenAIContent::Text(content.to_string()));
                                    }
                                    Some(OpenAIContent::Text(existing)) => {
                                        assistant_content = Some(OpenAIContent::Text(format!("{}{}", existing, content)));
                                    }
                                    Some(OpenAIContent::Multi(_)) => {}
                                }
                            }
                        }

                        if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
                            if !reasoning.is_empty() {
                                match &mut assistant_reasoning {
                                    None => assistant_reasoning = Some(reasoning.to_string()),
                                    Some(r) => r.push_str(reasoning),
                                }
                                let _ = self.event_tx.send(EngineEvent::Thinking(reasoning.to_string())).await;
                            }
                        }

                        if let Some(tool_calls_arr) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                            has_tool_calls = true;
                            for tc_delta in tool_calls_arr {
                                let index = tc_delta.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                                let entry = openai_tool_args.entry(index).or_insert_with(|| (String::new(), String::new(), String::new()));

                                if let Some(id) = tc_delta.get("id").and_then(|i| i.as_str()) {
                                    entry.0 = id.to_string();
                                }
                                if let Some(call_type) = tc_delta.get("type").and_then(|t| t.as_str()) {
                                    entry.1 = call_type.to_string();
                                }
                                if let Some(func) = tc_delta.get("function") {
                                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                        entry.1 = name.to_string();
                                    }
                                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                        entry.2.push_str(args);
                                    }
                                }
                            }
                        }

                        if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                            if finish != "tool_calls" {
                                stop_reason = Some(finish.to_string());
                                let _ = self.event_tx.send(EngineEvent::MessageDelta {
                                    stop_reason: Some(finish.to_string()),
                                }).await;
                            }
                        }
                    }

                }
            }

            let interrupted = self.event_tx.is_closed();
            let snapshot = request_trace.finish(if interrupted { "interrupted" } else { "completed" }, None);
            let response_status = snapshot["status"].as_str().unwrap_or("interrupted").to_string();
            let response_error = snapshot["error"].as_str().unwrap_or("Model response ended before completion").to_string();
            let _ = self.event_tx.send(EngineEvent::RequestSnapshot(snapshot)).await;
            // A response cut off by the output limit is "incomplete" in the
            // trace; continue it below instead of ending the turn.
            let continue_cut_off = response_status == "incomplete"
                && stop_reason.as_deref() == Some("length")
                && output_recoveries < MAX_OUTPUT_RECOVERIES
                && iteration + 1 < self.max_tool_iterations;
            if response_status != "completed" && !continue_cut_off {
                let _ = self.event_tx.send(EngineEvent::Error(response_error)).await;
                return Ok((full_text, Some(response_status)));
            }
            if stop_reason.as_deref() == Some("length")
                && output_recoveries < MAX_OUTPUT_RECOVERIES
                && iteration + 1 < self.max_tool_iterations
            {
                output_recoveries += 1;
                // Cut-off tool arguments are dropped unrun; keep the visible text.
                if assistant_content.as_ref().is_some_and(|content| !matches!(content, OpenAIContent::Text(text) if text.is_empty())) {
                    conversation_messages.push(OpenAIMessage { role: "assistant".into(), content: assistant_content.unwrap_or(OpenAIContent::Text(String::new())), tool_calls: None, tool_call_id: None, reasoning_content: assistant_reasoning, provider_content: assistant_provider_content });
                }
                conversation_messages.push(OpenAIMessage { role: "user".into(), content: OpenAIContent::Text(output_limit_resume(self.max_tokens)), tool_calls: None, tool_call_id: None, reasoning_content: None, provider_content: None });
                let _ = self.event_tx.send(output_limit_notice(self.max_tokens, output_recoveries)).await;
                stop_reason = None;
                continue;
            }
            if has_tool_calls {
                let mut indices: Vec<usize> = openai_tool_args.keys().copied().collect();
                indices.sort();

                let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();

                for idx in &indices {
                    let (id, name, args_str) = &openai_tool_args[idx];
                    let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| {
                        recover_malformed_tool_input(name, args_str).unwrap_or(json!({}))
                    });

                    let _ = self.event_tx.send(EngineEvent::ToolUseStart {
                        tool_use_id: id.clone(),
                        tool_name: name.clone(),
                        tool_input: input.clone(),
                        text_before: full_text.clone(),
                    }).await;

                    // Memory calls need this executor's configured client, not a generic stub.
                    let requires_user_interaction =
                        matches!(
                            name.as_str(),
                            "Workflow"
                                | "Skill"
                                | "Agent"
                                | "Delegate"
                                | "Worker"
                                | "SendMessage"
                                | "TeamCreate"
                                | "TeamDelete"
                                | "TeamSpawn"
                                | "team"
                                | "plan_mode"
                                | "EnterPlanMode"
                                | "ExitPlanMode"
                        ) || matches!(name.as_str(), "AskUserQuestion" | "ProposeGoal")
                            || name.starts_with("mcp__")
                            || matches!(name.as_str(), "ListMcpResources" | "ReadMcpResource")
                            || crate::tools::tasks::is_task(&name)
                            || is_memory_tool(&name)
                            || self.hooks.is_some()
                            || self.is_plan_document_tool(name, &input)
                            || self.check_permission_requires_confirmation(name, &input);

                    assistant_tool_calls.push(crate::native_engine::openai_client::OpenAIToolCall {
                        id: id.clone(),
                        call_type: "function".to_string(),
                        function: crate::native_engine::openai_client::FunctionCall {
                            name: name.clone(),
                            arguments: args_str.clone(),
                        },
                    });

                    pending_tool_calls.push(PendingToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input,
                        requires_user_interaction,
                    });
                }

                let mut tool_results: Vec<OpenAIMessage> = Vec::new();
                let mut tool_images = Vec::new();
                if pending_tool_calls.iter().any(|t| matches!(t.name.as_str(), "ExitPlanMode" | "plan_mode")) {
                    for call in &mut pending_tool_calls { call.requires_user_interaction = true; }
                }

                let (serial_calls, parallel_calls): (Vec<_>, Vec<_>) = pending_tool_calls
                    .into_iter()
                    .partition::<Vec<_>, _>(|tc| tc.requires_user_interaction);

                for tc in &serial_calls {
                    let (actual_input, output_str, is_error) = self.execute_tool_call(&tc.name, &tc.input, &tc.id).await;
                    for call in &mut assistant_tool_calls {
                        if call.id == tc.id { call.function.arguments = actual_input.to_string(); }
                    }
                    let mcp = self.mcp_outputs.remove(&tc.id);
                    let mcp = self.finish_mcp_output(&tc.id, &tc.name, &output_str, is_error, mcp)?;
                    if !is_error { if let Some(manager) = &self.permission_manager { manager.record_tool_success(self.conv_id.as_deref().unwrap_or("")); } }
                    let _ = self.event_tx.send(EngineEvent::ToolUseDone {
                        tool_use_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        tool_input: actual_input,
                        output: output_str.clone(),
                        is_error,
                    }).await;
                    if let Some(content) = &mcp { tool_images.extend(content.openai_images(&tc.id)); }
                    let compacted = mcp.map(|value|value.openai_text()).unwrap_or_else(||self.retained_tool_output(&tc.name, &output_str));
                    tool_results.push(OpenAIMessage {
                        role: "tool".to_string(),
                        content: OpenAIContent::Text(compacted),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        reasoning_content: None, provider_content: None,
                    });
                }

                self.ensure_permission_continuation()?;
                if !parallel_calls.is_empty() {
                    let total = parallel_calls.len();
                    let completed_count = Arc::new(AtomicUsize::new(serial_calls.len()));
                    let event_tx_clone = self.event_tx.clone();
                    let mcp_registry_clone = self.mcp_registry.clone();
                    let workspace_cwd_clone = self.workspace_cwd.clone();
                    let permission_manager_clone = self.permission_manager.clone();
                    let conv_id_clone = self.conv_id.clone();

                    let mut futures = Vec::new();
                    for tc in parallel_calls {
                        let fut = execute_tool_standalone(
                            tc.name,
                            tc.input.clone(),
                            tc.id.clone(),
                            event_tx_clone.clone(),
                            mcp_registry_clone.clone(),
                            workspace_cwd_clone.clone(),
                            permission_manager_clone.clone(),
                            conv_id_clone.clone(),
                            completed_count.clone(),
                            total + serial_calls.len(),
                            self.provider.clone(),
                            self.search_config_path.clone(),
                            self.allowed_tools.clone(),
                            self.goal_db.clone(),
                        );
                        futures.push(fut);
                    }

                    let results = futures::future::join_all(futures).await;

                    for (tool_use_id, tool_name, _tool_input, output_str, is_error, mcp) in results {
                        let mcp = self.finish_mcp_output(&tool_use_id, &tool_name, &output_str, is_error, mcp)?;
                        if let Some(content) = &mcp { tool_images.extend(content.openai_images(&tool_use_id)); }
                        let compacted = mcp.map(|value|value.openai_text()).unwrap_or_else(||self.retained_tool_output(&tool_name, &output_str));
                        tool_results.push(OpenAIMessage {
                            role: "tool".to_string(),
                            content: OpenAIContent::Text(compacted),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id),
                            reasoning_content: None, provider_content: None,
                        });
                    }
                }

                conversation_messages.push(OpenAIMessage {
                    role: "assistant".to_string(),
                    content: assistant_content.unwrap_or(OpenAIContent::Text(String::new())),
                    tool_calls: Some(assistant_tool_calls),
                    tool_call_id: None,
                    reasoning_content: assistant_reasoning, provider_content: assistant_provider_content,
                });

                for tool_result_msg in tool_results {
                    conversation_messages.push(tool_result_msg);
                }
                if !tool_images.is_empty() {
                    conversation_messages.push(OpenAIMessage {
                        role: "user".into(), content: serde_json::from_value(json!(tool_images))?,
                        tool_calls: None, tool_call_id: None, reasoning_content: None, provider_content: None,
                    });
                }
                if let Some((boundary_id, content)) = self.pending_plan_reset.take() {
                    self.messages = vec![json!({"role":"user","content":content})];
                    conversation_messages = vec![OpenAIMessage { role: "user".into(), content: OpenAIContent::Text(content), tool_calls: None, tool_call_id: None, reasoning_content: None, provider_content: None }];
                    full_text.clear();
                    self.messages.clear();
                    self.stop_hook_active = false;
                    let _ = self.event_tx.send(EngineEvent::ContextReset { session_id: self.conv_id.clone().unwrap_or_default(), boundary_id }).await;
                }
            } else {
                let incoming = if iteration + 1 < self.max_tool_iterations { self.drain_peer_messages()? } else { Vec::new() };
                if !incoming.is_empty() {
                    conversation_messages.push(OpenAIMessage { role: "assistant".into(), content: assistant_content.unwrap_or(OpenAIContent::Text(String::new())), tool_calls: None, tool_call_id: None, reasoning_content: assistant_reasoning, provider_content: assistant_provider_content });
                    for content in incoming { conversation_messages.push(OpenAIMessage { role: "user".into(), content: OpenAIContent::Text(content), tool_calls: None, tool_call_id: None, reasoning_content: None, provider_content: None }); }
                    continue;
                }
                if let Some(follow_up) = self.stop_hook(&full_text[round_text_start..]).await? {
                    anyhow::ensure!(iteration + 1 < self.max_tool_iterations, "Stop hook requested continuation after the tool iteration limit");
                    conversation_messages.push(OpenAIMessage { role: "assistant".into(), content: assistant_content.unwrap_or(OpenAIContent::Text(String::new())), tool_calls: None, tool_call_id: None, reasoning_content: assistant_reasoning, provider_content: assistant_provider_content });
                    conversation_messages.push(OpenAIMessage { role: "user".into(), content: OpenAIContent::Text(follow_up), tool_calls: None, tool_call_id: None, reasoning_content: None, provider_content: None });
                    continue;
                }
                break;
            }

            if iteration == self.max_tool_iterations - 1 {
                stop_reason = Some("max_iterations".to_string());
                let _ = self.event_tx.send(EngineEvent::Error("Max tool iterations reached".to_string())).await;
                let _ = self.event_tx.send(EngineEvent::MessageStop {
                    full_text: full_text.clone(),
                    stop_reason: Some("max_iterations".to_string()),
                }).await;
                break;
            }

        }

        Ok((full_text, stop_reason))
    }

    fn build_anthropic_messages(&self) -> Vec<AnthropicMessage> {
        self.messages.iter().filter_map(|msg| {
            let role = msg.get("role")?.as_str()?;
            let content = msg.get("content")?;

            let anthropic_content = if content.is_string() {
                AnthropicContent::Text(content.as_str()?.to_string())
            } else if content.is_array() {
                let blocks: Vec<ContentBlock> = content.as_array()?.iter().filter_map(|block| {
                    let block_type = block.get("type")?.as_str()?;
                    match block_type {
                        "text" => {
                            let text = block.get("text")?.as_str()?.to_string();
                            Some(ContentBlock::Text { text })
                        }
                        "image_url" => {
                            let url = block.get("image_url")?.get("url")?.as_str()?;
                            let (media_type, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
                            Some(ContentBlock::Image {
                                source: crate::native_engine::anthropic_client::ImageSource {
                                    source_type: "base64".into(), media_type: media_type.into(), data: data.into(),
                                },
                            })
                        }
                        "image" => {
                            let source = block.get("source")?;
                            Some(ContentBlock::Image {
                                source: crate::native_engine::anthropic_client::ImageSource {
                                    source_type: source.get("type")?.as_str()?.to_string(),
                                    media_type: source.get("media_type")?.as_str()?.to_string(),
                                    data: source.get("data")?.as_str()?.to_string(),
                                },
                            })
                        }
                        "tool_result" => {
                            Some(ContentBlock::ToolResult {
                                tool_use_id: block.get("tool_use_id")?.as_str()?.to_string(),
                                content: block.get("content")?.clone(),
                                is_error: block.get("is_error").and_then(|v| v.as_bool()),
                            })
                        }
                        "tool_use" => Some(ContentBlock::ToolUse {
                            id: block.get("id")?.as_str()?.to_string(),
                            name: block.get("name")?.as_str()?.to_string(),
                            input: block.get("input")?.clone(),
                        }),
                        _ => None,
                    }
                }).collect();
                AnthropicContent::Blocks(blocks)
            } else {
                return None;
            };

            Some(AnthropicMessage {
                role: role.to_string(),
                content: anthropic_content,
            })
        }).collect()
    }

    fn build_openai_messages(&self) -> Vec<OpenAIMessage> {
        self.messages.iter().filter_map(|msg| {
            let role = msg.get("role")?.as_str()?;
            let content = msg.get("content")?;

            let openai_content = if content.is_string() {
                OpenAIContent::Text(content.as_str()?.to_string())
            } else if content.is_array() {
                let parts: Vec<crate::native_engine::openai_client::OpenAIContentPart> = content.as_array()?.iter().filter_map(|part| {
                    let part_type = part.get("type")?.as_str()?;
                    match part_type {
                        "text" => {
                            Some(crate::native_engine::openai_client::OpenAIContentPart::Text {
                                text: part.get("text")?.as_str()?.to_string(),
                            })
                        }
                        "image_url" => {
                            let url_obj = part.get("image_url")?;
                            Some(crate::native_engine::openai_client::OpenAIContentPart::Image {
                                image_url: crate::native_engine::openai_client::ImageUrl {
                                    url: url_obj.get("url")?.as_str()?.to_string(),
                                },
                            })
                        }
                        _ => None,
                    }
                }).collect();
                OpenAIContent::Multi(parts)
            } else {
                return None;
            };

            let tool_calls = msg.get("tool_calls").and_then(|tc| {
                serde_json::from_value(tc.clone()).ok()
            });

            let tool_call_id = msg.get("tool_call_id").and_then(|id| id.as_str()).map(String::from);

            let reasoning_content = msg.get("reasoning_content").and_then(|r| r.as_str()).map(String::from);

            Some(OpenAIMessage {
                role: role.to_string(),
                content: openai_content,
                tool_calls,
                tool_call_id,
                reasoning_content, provider_content: None,
            })
        }).collect()
    }

    fn get_workspace_cwd(&self) -> &str {
        &self.workspace_cwd
    }

    fn handle_streaming_tool_arg_delta(&mut self, tool_use_id: &str, tool_name: &str, delta: &str) {
        let prev_args = self.streaming_tool_args
            .get(tool_use_id)
            .map(|s| s.accumulated_args.clone())
            .unwrap_or_default();

        let merged = merge_tool_args(&prev_args, delta);

        let delta_to_emit = if merged.starts_with(&prev_args) && !prev_args.is_empty() {
            merged[prev_args.len()..].to_string()
        } else {
            delta.to_string()
        };

        self.streaming_tool_args.insert(
            tool_use_id.to_string(),
            StreamingToolCall {
                name: tool_name.to_string(),
                accumulated_args: merged,
            },
        );

        if !delta_to_emit.is_empty() {
            let _ = self.event_tx.try_send(EngineEvent::ToolArgDelta {
                tool_use_id: tool_use_id.to_string(),
                delta: delta_to_emit,
            });
        }
    }

    fn finalize_streaming_tool_args(&mut self, tool_use_id: &str) -> Value {
        if let Some(stc) = self.streaming_tool_args.remove(tool_use_id) {
            let parsed: Option<Value> = serde_json::from_str(&stc.accumulated_args).ok();
            parsed.or_else(|| recover_malformed_tool_input(&stc.name, &stc.accumulated_args))
                .unwrap_or(json!({}))
        } else {
            json!({})
        }
    }
}
