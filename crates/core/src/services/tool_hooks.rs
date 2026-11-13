//! Command lifecycle hooks. Hosts must load workspace configuration only after
//! selecting/trusting that workspace, and retain normal tool permission checks.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const OUTPUT_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    PermissionRequest,
    SubagentStart,
    SubagentStop,
    UserPromptSubmit,
    Stop,
    WorktreeCreate,
    WorktreeRemove,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    Notification,
    StopFailure,
    Setup,
    ConfigChange,
    InstructionsLoaded,
    CwdChanged,
    FileChanged,
    Elicitation,
    ElicitationResult,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
}
impl HookEvent {
    fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionDenied => "PermissionDenied",
            Self::PermissionRequest => "PermissionRequest",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::WorktreeCreate => "WorktreeCreate",
            Self::WorktreeRemove => "WorktreeRemove",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Notification => "Notification",
            Self::StopFailure => "StopFailure",
            Self::Setup => "Setup",
            Self::ConfigChange => "ConfigChange",
            Self::InstructionsLoaded => "InstructionsLoaded",
            Self::CwdChanged => "CwdChanged",
            Self::FileChanged => "FileChanged",
            Self::Elicitation => "Elicitation",
            Self::ElicitationResult => "ElicitationResult",
            Self::TeammateIdle => "TeammateIdle",
            Self::TaskCreated => "TaskCreated",
            Self::TaskCompleted => "TaskCompleted",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveHook {
    pub event: HookEvent,
    pub matcher: String,
    pub command: String,
    #[serde(flatten)]
    pub http: Option<HttpHook>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<prompt::PromptHook>,
    pub timeout: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
    #[serde(rename = "async", skip_serializing_if = "std::ops::Not::not")]
    pub asynchronous: bool,
    #[serde(rename = "asyncRewake", skip_serializing_if = "std::ops::Not::not")]
    pub async_rewake: bool,
    #[serde(rename = "statusMessage", skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    pub source: PathBuf,
    pub shell: Option<String>,
    /// Permission-rule filter evaluated before spawning (upstream `if`).
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
}

impl ActiveHook {
    /// Hooks with `if` only run for tool events whose call matches the rule.
    /// Like upstream, a condition the matcher cannot evaluate never matches,
    /// while ambiguous compound shell commands do match, so a guarding hook is
    /// not skipped for a call it might cover.
    fn condition_matches(
        &self,
        event: HookEvent,
        input: &serde_json::Map<String, Value>,
        workspace: &Path,
    ) -> bool {
        let Some(condition) = &self.if_condition else {
            return true;
        };
        if !matches!(
            event,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionRequest
        ) {
            return false;
        }
        let Ok(rule) = crate::permissions::patterns::Rule::parse(condition) else {
            return false;
        };
        let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
        rule.matches(
            &crate::permissions::PermissionContext {
                mcp_read_only: false,
                tool_name: crate::permissions::patterns::canonical_tool_name(tool).into(),
                tool_input: input.get("tool_input").cloned().unwrap_or(Value::Null),
                conversation_id: String::new(),
                user_id: None,
                workspace_path: Some(workspace.to_string_lossy().into_owned()),
            },
            &crate::permissions::rules::PermissionAction::Deny,
        )
    }
}

#[derive(Debug, Default)]
pub struct HookOutcome {
    pub allow_permission: bool,
    pub permission_response: Option<HookPermissionResponse>,
    pub blocked: bool,
    pub reason: String,
    pub additional_context: Vec<String>,
    pub updated_input: Option<Value>,
    /// Model-facing MCP content, never a replacement transport envelope.
    pub updated_mcp_tool_output: Option<Value>,
    pub elicitation_response: Option<Value>,
    pub initial_user_message: Option<String>,
    pub watch_paths: Vec<String>,
    mcp_output_order: usize,
    pub warnings: Vec<String>,
    /// An explicit hook request for approval must not become an automatic grant.
    pub ask_permission: bool,
    pub prevent_continuation: bool,
    /// PermissionDenied advice, never an execution grant.
    pub retry: bool,
}

impl HookOutcome {
    fn observe_mcp_output(&mut self, order: &std::sync::atomic::AtomicUsize) {
        if self.updated_mcp_tool_output.is_some()
            || self.elicitation_response.is_some()
            || self.initial_user_message.is_some()
        {
            self.mcp_output_order = order.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        }
    }

    fn merge_mcp_output(&mut self, other: &mut Self) {
        self.watch_paths.append(&mut other.watch_paths);
        if other.initial_user_message.is_some() && other.mcp_output_order >= self.mcp_output_order {
            self.initial_user_message = other.initial_user_message.take();
            self.mcp_output_order = other.mcp_output_order;
        }
        if other.elicitation_response.is_some() && other.mcp_output_order >= self.mcp_output_order {
            self.elicitation_response = other.elicitation_response.take();
            self.mcp_output_order = other.mcp_output_order;
        }
        if other.updated_mcp_tool_output.is_some()
            && other.mcp_output_order >= self.mcp_output_order
        {
            self.updated_mcp_tool_output = other.updated_mcp_tool_output.take();
            self.mcp_output_order = other.mcp_output_order;
        }
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[path = "tool_hook_http.rs"]
mod http;
pub use http::HttpHook;
#[path = "tool_hook_prompt.rs"]
mod prompt;
#[path = "tool_hook_agent.rs"]
pub(crate) mod agent;

#[path = "tool_hook_once.rs"]
mod once;

#[cfg(test)]
#[path = "tool_hook_outputs_tests.rs"]
mod output_tests;

#[cfg(test)]
#[path = "lifecycle_events_tests.rs"]
mod lifecycle_tests;

#[derive(Debug)]
pub enum HookPermissionResponse {
    Allow { updated_input: Option<Value>, updated_permissions: Option<Value> },
    Deny { message: String, interrupt: bool },
}

#[derive(Debug, Deserialize)]
struct MatcherConfig {
    #[serde(default)]
    matcher: String,
    hooks: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct CommandConfig {
    command: String,
    #[serde(default)]
    once: bool,
    #[serde(default, rename = "async")]
    asynchronous: bool,
    #[serde(default, rename = "asyncRewake")]
    async_rewake: bool,
    #[serde(default, rename = "statusMessage")]
    status_message: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default, rename = "if")]
    if_condition: Option<String>,
}
fn default_timeout() -> u64 {
    60
}

#[derive(Default, Clone)]
pub struct HookRunner {
    recording_db: Option<std::sync::Arc<crate::db::DbManager>>,
    workspace: PathBuf,
    once_directory: PathBuf,
    hooks: Vec<ActiveHook>,
    callbacks: Option<std::sync::Weak<super::hook_callbacks::Registration>>,
    disabled: bool,
    force_sync: bool,
    prompt_provider: Option<crate::native_engine::provider_manager::ResolvedProvider>,
}

impl HookRunner {
    pub fn load(data_dir: &Path, workspace: &Path) -> Result<Self> {
        Self::load_with_configs(data_dir, workspace, Vec::new())
    }

    pub fn load_with_plugins(context: &super::ServiceContext, workspace: &Path) -> Result<Self> {
        Self::load_with_configs(
            &context.paths.data_dir,
            workspace,
            super::plugins::command_hooks(context)?,
        )
        .map(|mut runner| {
            runner.recording_db = Some(context.db.clone());
            runner
        })
    }

    fn load_with_configs(
        data_dir: &Path,
        workspace: &Path,
        mut configs: Vec<(PathBuf, Value)>,
    ) -> Result<Self> {
        let sources = [
            data_dir.join("settings.json"),
            workspace.join(".claude/settings.json"),
            workspace.join(".claude/settings.local.json"),
        ];
        for source in sources {
            let content = match std::fs::read_to_string(&source) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Reading hooks from {}", source.display()))
                }
            };
            let settings: Value = serde_json::from_str(content.trim_start_matches('\u{feff}'))
                .with_context(|| format!("Invalid hook settings: {}", source.display()))?;
            configs.push((source, settings));
        }
        Self::from_configs(data_dir, workspace, configs)
    }

    pub(crate) fn from_configs(
        data_dir: &Path,
        workspace: &Path,
        configs: Vec<(PathBuf, Value)>,
    ) -> Result<Self> {
        let mut hooks = Vec::new();
        let mut disabled = false;
        let mut allowed_http_urls: Option<Vec<String>> = None;
        let mut http_env_vars: Option<Vec<String>> = None;
        for (source, settings) in configs {
            for (field, target) in [("allowedHttpHookUrls", &mut allowed_http_urls), ("httpHookAllowedEnvVars", &mut http_env_vars)] {
                if let Some(value) = settings.get(field) {
                    let entries: Vec<String> = serde_json::from_value(value.clone()).with_context(|| format!("{field} must be an array of strings"))?;
                    target.get_or_insert_with(Vec::new).extend(entries);
                }
            }
            if let Some(value) = settings.get("disableAllHooks") {
                disabled = value.as_bool().context("disableAllHooks must be boolean")?;
            }
            let Some(config) = settings.get("hooks") else {
                continue;
            };
            let events: BTreeMap<String, Vec<MatcherConfig>> =
                serde_json::from_value(config.clone())
                    .context("hooks must map event names to matcher arrays")?;
            for (name, groups) in events {
                let event = match name.as_str() {
                    "PreToolUse" => HookEvent::PreToolUse,
                    "PostToolUse" => HookEvent::PostToolUse,
                    "PostToolUseFailure" => HookEvent::PostToolUseFailure,
                    "PermissionDenied" => HookEvent::PermissionDenied,
                    "PermissionRequest" => HookEvent::PermissionRequest,
                    "SubagentStart" => HookEvent::SubagentStart,
                    "SubagentStop" => HookEvent::SubagentStop,
                    "UserPromptSubmit" => HookEvent::UserPromptSubmit,
                    "Stop" => HookEvent::Stop,
                    "WorktreeCreate" => HookEvent::WorktreeCreate,
                    "WorktreeRemove" => HookEvent::WorktreeRemove,
                    "SessionStart" => HookEvent::SessionStart,
                    "SessionEnd" => HookEvent::SessionEnd,
                    "PreCompact" => HookEvent::PreCompact,
                    "PostCompact" => HookEvent::PostCompact,
                    "Notification" => HookEvent::Notification,
                    "StopFailure" => HookEvent::StopFailure,
                    "Setup" => HookEvent::Setup,
                    "ConfigChange" => HookEvent::ConfigChange,
                    "InstructionsLoaded" => HookEvent::InstructionsLoaded,
                    "CwdChanged" => HookEvent::CwdChanged,
                    "FileChanged" => HookEvent::FileChanged,
                    "Elicitation" => HookEvent::Elicitation,
                    "ElicitationResult" => HookEvent::ElicitationResult,
                    "TeammateIdle" => HookEvent::TeammateIdle,
                    "TaskCreated" => HookEvent::TaskCreated,
                    "TaskCompleted" => HookEvent::TaskCompleted,
                    _ => {
                        ensure!(
                            !groups.iter().any(|g| g
                                .hooks
                                .iter()
                                .any(|h| h["type"] == "prompt" || h["type"] == "agent")),
                            "Prompt/agent hooks are not supported for {name}"
                        );
                        continue;
                    }
                };
                for group in groups {
                    if event != HookEvent::CwdChanged
                        && !group.matcher.is_empty()
                        && group.matcher != "*"
                    {
                        regex::Regex::new(&group.matcher)
                            .context("Invalid hook matcher regular expression")?;
                    }
                    for mut hook in group.hooks {
                        ensure!(hook["type"] == "command" || hook["type"] == "http" || hook["type"] == "prompt" || hook["type"] == "agent", "Unsupported hook type for {name} in {}; only command, http, prompt and agent hooks are implemented", source.display());
                        for option in ["async", "asyncRewake"] {
                            if let Some(value) = hook.get(option) {
                                ensure!(value.is_boolean(), "Hook {option} must be boolean");
                                ensure!(
                                    value == false || hook["type"] == "command",
                                    "Hook {option} is not implemented for {}",
                                    hook["type"]
                                );
                            }
                        }
                        if matches!(event, HookEvent::Elicitation | HookEvent::ElicitationResult) {
                            ensure!(
                                hook["async"] != true && hook["asyncRewake"] != true,
                                "Elicitation hooks must run synchronously"
                            );
                        }
                        let prompt = prompt::load(&mut hook, event)?;
                        let http = if hook["type"] == "http" {
                            ensure!(
                                event != HookEvent::Setup,
                                "HTTP hooks are not supported for Setup"
                            );
                            ensure!(
                                hook.get("command").is_none() && hook.get("shell").is_none(),
                                "HTTP hooks cannot specify command or shell"
                            );
                            let config: HttpHook = serde_json::from_value(hook.clone())?;
                            config.validate()?;
                            hook["command"] = json!(config.url());
                            if hook.get("timeout").is_none() { hook["timeout"] = json!(600); }
                            Some(config)
                        } else { None };
                        let hook: CommandConfig = serde_json::from_value(hook)?;
                        ensure!(
                            !hook.command.trim().is_empty(),
                            "Hook command cannot be empty"
                        );
                        ensure!(
                            (1..=3600).contains(&hook.timeout),
                            "Hook timeout must be 1..3600 seconds"
                        );
                        if let Some(shell) = &hook.shell {
                            ensure!(
                                ["bash", "sh", "cmd", "powershell", "pwsh"]
                                    .contains(&shell.as_str()),
                                "Unsupported hook shell: {shell}"
                            );
                        }
                        hooks.push(ActiveHook {
                            event,
                            matcher: group.matcher.clone(),
                            command: hook.command,
                            http,
                            prompt,
                            timeout: hook.timeout,
                            once: hook.once,
                            asynchronous: hook.asynchronous || hook.async_rewake,
                            async_rewake: hook.async_rewake,
                            status_message: hook.status_message,
                            source: source.clone(),
                            shell: hook.shell,
                            if_condition: hook.if_condition.map(|condition| {
                                crate::permissions::patterns::canonical_rule(&condition)
                            }),
                        });
                    }
                }
            }
        }
        for hook in &mut hooks {
            if let Some(http) = &mut hook.http {
                http.allowed_urls = allowed_http_urls.clone();
                http.policy_env_vars = http_env_vars.clone();
            }
        }
        if disabled {
            hooks.clear();
        }
        Ok(Self {
            workspace: workspace.to_path_buf(),
            once_directory: data_dir.join("hook-once"),
            hooks,
            disabled,
            force_sync: false,
            callbacks: None,
            prompt_provider: None,
            recording_db: None,
        })
    }

    pub fn force_synchronous(&mut self) {
        self.force_sync = true;
    }

    pub fn list(&self) -> &[ActiveHook] {
        &self.hooks
    }

    pub fn with_session_callbacks(mut self, session: &str) -> Self {
        self.callbacks = super::hook_callbacks::lookup(session);
        self
    }
    pub fn has_hooks(&self) -> bool {
        !self.disabled
            && (!self.hooks.is_empty()
                || self
                    .callbacks
                    .as_ref()
                    .and_then(|callbacks| callbacks.upgrade())
                    .is_some_and(|callbacks| callbacks.has_callbacks()))
    }
    fn callbacks(
        &self,
        session: &str,
    ) -> Option<std::sync::Arc<super::hook_callbacks::Registration>> {
        if self.disabled {
            return None;
        }
        self.callbacks
            .clone()
            .or_else(|| super::hook_callbacks::lookup(session))
            .and_then(|callbacks| callbacks.upgrade())
    }

    /// Worktree hooks return a filesystem path, not tool-permission JSON.
    pub async fn run_worktree(&self, event: HookEvent, session: &str, mut payload: Value) -> Result<Option<String>> {
        ensure!(matches!(event, HookEvent::WorktreeCreate | HookEvent::WorktreeRemove), "Invalid worktree hook event");
        payload["session_id"] = json!(session);
        payload["cwd"] = json!(self.workspace);
        payload["hook_event_name"] = json!(event.name());
        let mut path = None;
        for hook in self
            .hooks
            .iter()
            .filter(|hook| hook.event == event && hook.if_condition.is_none())
        {
            let Some((code, stdout, stderr)) =
                self.execute_once(hook, session, &payload, None).await?
            else {
                continue;
            };
            ensure!(
                code == 0,
                "{} hook failed ({code}): {}",
                event.name(),
                stderr.trim()
            );
            if event == HookEvent::WorktreeCreate && path.is_none() && !stdout.trim().is_empty() {
                path = Some(if stdout.trim().starts_with('{') {
                    let value: Value = serde_json::from_str(stdout.trim())?;
                    value["hookSpecificOutput"]["worktreePath"].as_str().context("WorktreeCreate JSON must provide hookSpecificOutput.worktreePath")?.to_owned()
                } else { stdout.trim().to_owned() });
            }
        }
        if event == HookEvent::WorktreeCreate { ensure!(path.is_some(), "WorktreeCreate returned no path"); }
        Ok(path)
    }

    pub fn run<'a>(
        &'a self,
        event: HookEvent,
        session_id: &'a str,
        payload: Value,
    ) -> futures::future::BoxFuture<'a, Result<HookOutcome>> {
        // Keep command/callback state out of the enclosing engine and CLI futures.
        Box::pin(async move {
            let reset = matches!(event, HookEvent::SessionStart | HookEvent::CwdChanged);
            if reset || event == HookEvent::SessionEnd {
                super::file_changed::stop(session_id).await;
            }
            if reset {
                super::session_environment::clear_cwd(
                    self.once_directory.parent().unwrap_or(&self.workspace),
                    session_id,
                )?;
            }
            let outcome = self.run_inner(event, session_id, payload).await;
            if reset && !self.disabled {
                let paths = outcome
                    .as_ref()
                    .map(|o| o.watch_paths.clone())
                    .unwrap_or_default();
                super::file_changed::start(session_id, self.clone(), paths).await?;
            }
            outcome
        })
    }

    fn transcript_input(
        &self,
        session: &str,
        mut input: Value,
        cache: &std::sync::OnceLock<PathBuf>,
    ) -> Value {
        let path = cache.get_or_init(|| {
            let root = self.once_directory.parent().unwrap_or(&self.workspace);
            let path = super::lifecycle_events::transcript_path(root, session);
            if let Err(error) = super::lifecycle_events::transcript(root, session) {
                tracing::warn!(session_id = session, transcript_path = %path.display(),
                    "Hook transcript snapshot unavailable; dispatch continues: {error:#}");
            }
            path
        });
        input["transcript_path"] = json!(path);
        input
    }

    pub(crate) async fn run_inner(
        &self,
        event: HookEvent,
        session_id: &str,
        payload: Value,
    ) -> Result<HookOutcome> {
        if self.disabled {
            return Ok(HookOutcome::default());
        }
        let mut input = payload
            .as_object()
            .cloned()
            .context("Hook payload must be an object")?;
        // Shared by actual executions, after matcher/if/once filtering. Initializing
        // this cell performs no I/O; even a failed snapshot is attempted only once.
        let transcript = &std::sync::OnceLock::new();
        input.insert("session_id".into(), json!(session_id));
        input.insert("cwd".into(), json!(self.workspace));
        input.insert("hook_event_name".into(), json!(event.name()));
        let lifecycle = matches!(
            event,
            HookEvent::SessionStart
                | HookEvent::SessionEnd
                | HookEvent::PreCompact
                | HookEvent::PostCompact
                | HookEvent::SubagentStart
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionDenied
                | HookEvent::Notification
                | HookEvent::StopFailure
                | HookEvent::Setup
                | HookEvent::InstructionsLoaded
                | HookEvent::FileChanged
        );
        let match_key = match event {
            HookEvent::Elicitation | HookEvent::ElicitationResult => "mcp_server_name",
            HookEvent::SessionStart | HookEvent::ConfigChange => "source",
            HookEvent::Setup => "trigger",
            HookEvent::Notification => "notification_type",
            HookEvent::StopFailure => "error",
            HookEvent::InstructionsLoaded => "load_reason",
            HookEvent::SessionEnd => "reason",
            HookEvent::PreCompact | HookEvent::PostCompact => "trigger",
            HookEvent::SubagentStart | HookEvent::SubagentStop => "agent_type",
            _ => "tool_name",
        };
        let mut name = input
            .get(match_key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if event == HookEvent::FileChanged {
            name = input
                .get("file_path")
                .and_then(Value::as_str)
                .and_then(|p| Path::new(p).file_name())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        if event == HookEvent::PermissionRequest {
            use futures::{stream::FuturesUnordered, StreamExt};
            let mut pending: FuturesUnordered<futures::future::BoxFuture<'_, Result<HookOutcome>>> =
                FuturesUnordered::new();
            for hook in self.hooks.iter().filter(|hook| hook.event == event) {
                if !(matches!(event, HookEvent::TeammateIdle | HookEvent::TaskCreated | HookEvent::TaskCompleted)
                    || matches_tool(&hook.matcher, &name)?)
                    || !hook.condition_matches(event, &input, &self.workspace)
                {
                    continue;
                }
                let input = Value::Object(input.clone());
                pending.push(Box::pin(async move {
                    let Some((code, stdout, stderr)) = self
                        .execute_once(hook, session_id, &input, Some(transcript))
                        .await?
                    else {
                        return Ok(HookOutcome::default());
                    };
                    parse_hook_output(hook, code, &stdout, &stderr)
                }));
            }
            if let Some(callbacks) = self.callbacks(session_id) {
                for callback in callbacks.matching(event, &name) {
                    let callbacks = callbacks.clone();
                    let input = Value::Object(input.clone());
                    pending.push(Box::pin(async move {
                        let input = self.transcript_input(session_id, input, transcript);
                        parse_json_output(event, callbacks.invoke(&callback, input).await?)
                    }));
                }
            }
            // A prompt evaluator is a guard, never an approval source. Do not let
            // a faster callback approval cancel a pending prompt denial.
            let has_prompt_guard = self.hooks.iter().any(|hook| {
                hook.event == event
                    && hook.prompt.is_some()
                    && matches_tool(&hook.matcher, &name).unwrap_or(false)
                    && hook.condition_matches(event, &input, &self.workspace)
            });
            let mut result = HookOutcome::default();
            while let Some(outcome) = pending.next().await {
                match outcome {
                    Ok(outcome) => {
                        result.warnings.extend(outcome.warnings);
                        if outcome.blocked {
                            result.warnings.push(outcome.reason);
                        }
                        if let Some(response) = outcome.permission_response {
                            let denied = matches!(response, HookPermissionResponse::Deny { .. });
                            if denied || result.permission_response.is_none() {
                                result.permission_response = Some(response);
                            }
                            if denied || !has_prompt_guard {
                                // Dropping losing futures cancels their work and status.
                                return Ok(result);
                            }
                        }
                    }
                    Err(error) => result
                        .warnings
                        .push(format!("PermissionRequest hook failed: {error:#}")),
                }
            }
            return Ok(result);
        }
        use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
        let output_order = std::sync::atomic::AtomicUsize::new(0);
        let mut pending: FuturesUnordered<BoxFuture<'_, (bool, Result<HookOutcome>)>> =
            FuturesUnordered::new();
        let command_input = input.clone();
        let command_name = name.clone();
        // Local to this invocation: a slow trailing command must not reorder an
        // earlier command replacement after a newer SDK callback replacement.
        let command_order = &output_order;
        pending.push(Box::pin(async move {
            (
                false,
                async {
                    let mut input = command_input;
                    let name = command_name;
                    let mut result = HookOutcome::default();
                    for hook in self.hooks.iter().filter(|h| h.event == event) {
                        if (lifecycle
                            || matches!(
                                event,
                                HookEvent::Elicitation
                                    | HookEvent::ElicitationResult
                                    | HookEvent::ConfigChange
                                    | HookEvent::PreToolUse
                                    | HookEvent::PermissionRequest
                                    | HookEvent::PostToolUse
                                    | HookEvent::SubagentStop
                            ))
                            && !matches_tool(&hook.matcher, &name)?
                        {
                            continue;
                        }
                        if !hook.condition_matches(event, &input, &self.workspace) {
                            continue;
                        }
                        let outcome = async {
                            let output = self
                                .execute_once(
                                    hook,
                                    session_id,
                                    &Value::Object(input.clone()),
                                    Some(transcript),
                                )
                                .await?;
                            let Some(output) = output else {
                                return Ok(HookOutcome::default());
                            };
                            parse_hook_output(hook, output.0, &output.1, &output.2)
                        }
                        .await;
                        let mut outcome = match outcome {
                            Ok(outcome) => outcome,
                            Err(error) if lifecycle => {
                                result
                                    .warnings
                                    .push(format!("{} hook: {error:#}", event.name()));
                                continue;
                            }
                            Err(error) => return Err(error),
                        };
                        outcome.observe_mcp_output(command_order);
                        result.merge_mcp_output(&mut outcome);
                        result.additional_context.extend(outcome.additional_context);
                        result.warnings.extend(outcome.warnings);
                        result.ask_permission |= outcome.ask_permission;
                        result.allow_permission |= outcome.allow_permission;
                        if outcome.permission_response.is_some() {
                            result.permission_response = outcome.permission_response;
                            return Ok(result);
                        }
                        result.prevent_continuation |= outcome.prevent_continuation;
                        result.retry |= outcome.retry;
                        if let Some(updated) = outcome.updated_input {
                            input.insert("tool_input".into(), updated.clone());
                            result.updated_input = Some(updated);
                        }
                        if outcome.blocked && lifecycle {
                            result.warnings.push(outcome.reason);
                            continue;
                        }
                        if outcome.blocked {
                            result.blocked = true;
                            result.reason = outcome.reason;
                            break;
                        }
                    }
                    Ok(result)
                }
                .await,
            )
        }));
        if let Some(callbacks) = self.callbacks(session_id) {
            for callback in callbacks.matching(event, &name) {
                let callbacks = callbacks.clone();
                let input = Value::Object(input.clone());
                let callback_order = &output_order;
                pending.push(Box::pin(async move {
                    (
                        true,
                        async {
                            let input = self.transcript_input(session_id, input, transcript);
                            let mut outcome = parse_json_output(event, callbacks.invoke(&callback, input).await?)?;
                            outcome.observe_mcp_output(callback_order);
                            Ok(outcome)
                        }
                        .await,
                    )
                }));
            }
        }
        let mut result = HookOutcome::default();
        while let Some((callback, outcome)) = pending.next().await {
            let mut outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) if callback => {
                    result
                        .warnings
                        .push(format!("{} callback failed: {error:#}", event.name()));
                    continue;
                }
                Err(error) => return Err(error),
            };
            result.merge_mcp_output(&mut outcome);
            result.additional_context.extend(outcome.additional_context);
            result.warnings.extend(outcome.warnings);
            result.ask_permission |= outcome.ask_permission;
            result.allow_permission |= outcome.allow_permission;
            result.prevent_continuation |= outcome.prevent_continuation;
            result.retry |= outcome.retry;
            if outcome.updated_input.is_some() {
                result.updated_input = outcome.updated_input;
            }
            if outcome.blocked {
                if lifecycle {
                    result.warnings.push(outcome.reason);
                } else {
                    result.blocked = true;
                    result.reason = outcome.reason;
                }
            }
        }
        if event == HookEvent::ConfigChange && name == "policy_settings" {
            result.blocked = false;
            result.prevent_continuation = false;
        }
        Ok(result)
    }
}

fn matches_tool(matcher: &str, tool: &str) -> Result<bool> {
    Ok(matcher.is_empty() || matcher == "*" || regex::Regex::new(matcher)?.is_match(tool))
}

fn parse_hook_output(
    hook: &ActiveHook,
    code: i32,
    stdout: &str,
    stderr: &str,
) -> Result<HookOutcome> {
    if hook
        .prompt
        .as_ref()
        .is_some_and(prompt::PromptHook::is_agent)
        && code == 3
    {
        return Ok(HookOutcome::default());
    }
    if hook.prompt.is_some() && code == 2 {
        return Ok(HookOutcome {
            blocked: true,
            prevent_continuation: true,
            reason: stderr.into(),
            permission_response: (hook.event == HookEvent::PermissionRequest).then(|| {
                HookPermissionResponse::Deny {
                    message: stderr.into(),
                    interrupt: true,
                }
            }),
            ..Default::default()
        });
    }
    if hook.http.is_some() {
        if code == 0 { parse_json_output(hook.event, serde_json::from_str(stdout)?) }
        else { Ok(HookOutcome { warnings: vec![stderr.into()], ..Default::default() }) }
    } else { parse_output(hook.event, code, stdout, stderr) }
}

fn parse_output(event: HookEvent, exit: i32, stdout: &str, stderr: &str) -> Result<HookOutcome> {
    let mut result = HookOutcome::default();
    if exit == 2 {
        result.blocked = !matches!(
            event,
            HookEvent::Notification
                | HookEvent::StopFailure
                | HookEvent::InstructionsLoaded
                | HookEvent::CwdChanged
                | HookEvent::FileChanged
                | HookEvent::Setup
        );
        result.reason = if stderr.trim().is_empty() {
            "Blocked by command hook (exit 2)".into()
        } else {
            stderr.trim().into()
        };
        if !result.blocked {
            result.warnings.push(result.reason.clone());
        }
        return Ok(result);
    }
    if exit != 0 {
        result
            .warnings
            .push(format!("Hook exited with {exit}: {}", stderr.trim()));
        return Ok(result);
    }
    let output = stdout.trim();
    if output.is_empty() {
        return Ok(result);
    }
    // Compact hooks consume successful stdout as text, never as permission decisions.
    if event == HookEvent::PreCompact {
        result.additional_context.push(output.into());
        return Ok(result);
    }
    if event == HookEvent::PostCompact {
        result.warnings.push(output.into());
        return Ok(result);
    }
    if !output.starts_with('{') {
        if matches!(
            event,
            HookEvent::UserPromptSubmit
                | HookEvent::SessionStart
                | HookEvent::SubagentStart
                | HookEvent::Setup
        ) {
            result.additional_context.push(output.into());
        }
        return Ok(result);
    }
    let output: Value = serde_json::from_str(output).context("Hook returned invalid JSON")?;
    parse_json_output(event, output)
}

fn parse_json_output(event: HookEvent, output: Value) -> Result<HookOutcome> {
    ensure!(output.is_object(), "Hook output must be an object");
    if let Some(value) = output
        .get("hookSpecificOutput")
        .and_then(|v| v.get("initialUserMessage"))
    {
        ensure!(
            event == HookEvent::SessionStart,
            "initialUserMessage requires SessionStart"
        );
        ensure!(value.is_string(), "initialUserMessage must be a string");
    }
    let mut result = HookOutcome::default();
    result.watch_paths = watch_paths(event, &output)?;
    if matches!(event, HookEvent::Elicitation | HookEvent::ElicitationResult) {
        result.blocked = string_field(&output, "decision")? == Some("block");
        result.reason = string_field(&output, "reason")?
            .unwrap_or("Elicitation blocked by hook")
            .into();
        if let Some(specific) = output.get("hookSpecificOutput") {
            ensure!(specific.is_object(), "hookSpecificOutput must be an object");
            ensure!(
                string_field(specific, "hookEventName")? == Some(event.name()),
                "Hook returned an incorrect hookEventName"
            );
            if let Some(action) = string_field(specific, "action")? {
                ensure!(
                    ["accept", "decline", "cancel"].contains(&action),
                    "Invalid elicitation hook action"
                );
                let mut response = json!({"action":action});
                if let Some(content) = specific.get("content") {
                    ensure!(content.is_object(), "Elicitation content must be an object");
                    response["content"] = content.clone();
                }
                result.blocked |= action == "decline";
                result.elicitation_response = Some(response);
            }
        }
        return Ok(result);
    }
    if matches!(
        event,
        HookEvent::Notification
            | HookEvent::StopFailure
            | HookEvent::InstructionsLoaded
            | HookEvent::CwdChanged
            | HookEvent::FileChanged
            | HookEvent::Setup
            | HookEvent::ConfigChange
    ) {
        if let Some(message) = string_field(&output, "systemMessage")? {
            result.warnings.push(message.into());
        }
        if event == HookEvent::ConfigChange && string_field(&output, "decision")? == Some("block") {
            result.blocked = true;
            result.reason = string_field(&output, "reason")?
                .unwrap_or("Configuration change blocked by hook")
                .into();
        }
        if let Some(specific) = output.get("hookSpecificOutput") {
            ensure!(specific.is_object(), "hookSpecificOutput must be an object");
            ensure!(
                string_field(specific, "hookEventName")? == Some(event.name()),
                "Hook returned an incorrect hookEventName"
            );

            if event == HookEvent::Setup {
                if let Some(text) = string_field(specific, "additionalContext")? {
                    result.additional_context.push(text.into());
                }
            }
        }
        // Observer events cannot grant permission, mutate input, stop a turn or inject context.
        return Ok(result);
    }
    if event == HookEvent::PermissionRequest {
        if let Some(message) = string_field(&output, "systemMessage")? { result.warnings.push(message.into()); }
        if let Some(specific) = output.get("hookSpecificOutput") {
            ensure!(specific.is_object(), "hookSpecificOutput must be an object");
            ensure!(string_field(specific, "hookEventName")? == Some(event.name()), "Hook returned an incorrect hookEventName");
            if let Some(decision) = specific.get("decision") {
                ensure!(decision.is_object(), "PermissionRequest decision must be an object");
                result.permission_response = Some(match string_field(decision, "behavior")? {
                    Some("allow") => {
                        let updated_input = decision.get("updatedInput").map(|value| {
                            ensure!(value.is_object(), "Hook updatedInput must be an object"); Ok(value.clone())
                        }).transpose()?;
                        HookPermissionResponse::Allow { updated_input, updated_permissions: decision.get("updatedPermissions").cloned() }
                    }
                    Some("deny") => HookPermissionResponse::Deny {
                        message: string_field(decision, "message")?.unwrap_or("Permission denied by hook").into(),
                        interrupt: decision.get("interrupt").map(|value| value.as_bool().context("Hook interrupt must be boolean")).transpose()?.unwrap_or(false),
                    },
                    _ => bail!("Invalid PermissionRequest behavior"),
                });
            }
        }
        return Ok(result);
    }
    if event == HookEvent::PermissionDenied {
        if let Some(message) = string_field(&output, "systemMessage")? { result.warnings.push(message.into()); }
        if let Some(specific) = output.get("hookSpecificOutput") {
            ensure!(specific.is_object(), "hookSpecificOutput must be an object");
            ensure!(string_field(specific, "hookEventName")? == Some(event.name()), "Hook returned an incorrect hookEventName");
            if let Some(retry) = specific.get("retry") { result.retry = retry.as_bool().context("PermissionDenied retry must be boolean")?; }
        }
        // This notification cannot mutate input, permit execution or stop a turn.
        return Ok(result);
    }
    if let Some(value) = output.get("continue") {
        ensure!(value.is_boolean(), "Hook continue must be boolean");
        if value == false {
            result.prevent_continuation = true;
            result.blocked = true;
            result.reason = string_field(&output, "stopReason")?
                .unwrap_or("Stopped by hook")
                .into();
        }
    }
    if let Some(decision) = string_field(&output, "decision")? {
        ensure!(
            ["approve", "block"].contains(&decision),
            "Invalid hook decision"
        );
        if decision == "block" {
            result.blocked = true;
            result.reason = string_field(&output, "reason")?
                .unwrap_or("Blocked by hook")
                .into();
        }
    }
    if let Some(message) = string_field(&output, "systemMessage")? {
        result.warnings.push(message.into());
    }
    if let Some(specific) = output.get("hookSpecificOutput") {
        ensure!(specific.is_object(), "hookSpecificOutput must be an object");
        ensure!(
            string_field(specific, "hookEventName")? == Some(event.name()),
            "Hook returned an incorrect hookEventName"
        );
        if let Some(message) = string_field(specific, "initialUserMessage")? {
            if !message.is_empty() {
                result.initial_user_message = Some(message.into());
            }
        }
        if let Some(context) = string_field(specific, "additionalContext")? {
            result.additional_context.push(context.into());
        }
        if let Some(updated) = specific.get("updatedMCPToolOutput") {
            ensure!(event == HookEvent::PostToolUse, "updatedMCPToolOutput requires PostToolUse");
            if json_truthy(updated) {
                result.updated_mcp_tool_output = Some(updated.clone());
            }
        }
        if event == HookEvent::PreToolUse {
            if let Some(decision) = string_field(specific, "permissionDecision")? {
                match decision {
                    "deny" => {
                        result.blocked = true;
                        result.reason = string_field(specific, "permissionDecisionReason")?
                            .unwrap_or("Tool denied by hook")
                            .into();
                    }
                    "ask" => result.ask_permission = true,
                    "allow" => result.allow_permission = true,
                    _ => bail!("Invalid hook permissionDecision"),
                }
            }
            if let Some(updated) = specific.get("updatedInput") {
                ensure!(updated.is_object(), "Hook updatedInput must be an object");
                result.updated_input = Some(updated.clone());
            }
        }
    }
    Ok(result)
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<Option<&'a str>> {
    value
        .get(name)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("Hook {name} must be a string"))
        })
        .transpose()
}

async fn execute(hook: &ActiveHook, cwd: &Path, input: &Value) -> Result<(i32, String, String)> {
    if let Some(config) = &hook.http {
        return http::execute_http(hook, config, input).await;
    }
    prepare_command(hook, cwd, input, None)?.await
}

fn prepare_command(
    hook: &ActiveHook,
    cwd: &Path,
    input: &Value,
    env_file: Option<&Path>,
) -> Result<futures::future::BoxFuture<'static, Result<(i32, String, String)>>> {
    let shell = hook
        .shell
        .as_deref()
        .unwrap_or(if cfg!(windows) { "cmd" } else { "sh" });
    let mut command = Command::new(shell);
    command.env_remove("CLAUDE_ENV_FILE");
    // A hook is a separate session, not part of the supervising worker.
    crate::services::background_sessions::strip_internal_env(&mut command);
    if !matches!(shell, "powershell" | "pwsh") {
        if let Some(path) = env_file {
            command.env("CLAUDE_ENV_FILE", path);
        }
    }
    match shell {
        "cmd" => {
            command.args(["/D", "/S", "/C"]);
            #[cfg(windows)]
            command.raw_arg(format!("\"chcp 65001 >nul & {}\"", hook.command));
            #[cfg(not(windows))]
            command.arg(&hook.command);
        }
        "powershell" | "pwsh" => {
            let utf8_command = format!("$OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::InputEncoding = $OutputEncoding; [Console]::OutputEncoding = $OutputEncoding; $PSDefaultParameterValues['*:Encoding'] = 'utf8';\n{}", hook.command);
            command.args(["-NoProfile", "-NonInteractive", "-Command", &utf8_command]);
        }
        _ => {
            command.args(["-c", &hook.command]);
        }
    }
    command
        .current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(metadata) = hook.source.parent().filter(|parent| {
        parent
            .file_name()
            .is_some_and(|name| name == ".claude-plugin")
    }) {
        if let Some(root) = metadata.parent() {
            command.env("CLAUDE_PLUGIN_ROOT", root);
        }
    }
    #[cfg(windows)]
    // Assign the suspended process to its kill-on-close job before it can spawn
    // descendants. Assigning a running process leaves a child-escape race.
    command.creation_flags(0x08000004);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Starting {} hook", hook.event.name()))?;
    let _tree = ProcessTree::new(child.id().context("Hook has no process ID")?)?;
    #[cfg(windows)]
    resume_command(child.id().context("Hook has no process ID")?)?;
    let _progress = super::hook_progress::Running::start(
        input["session_id"].as_str().unwrap_or(""), hook.status_message.as_deref());
    let mut stdin = child.stdin.take().context("Hook stdin unavailable")?;
    let stdout = child.stdout.take().context("Hook stdout unavailable")?;
    let stderr = child.stderr.take().context("Hook stderr unavailable")?;
    let mut data = serde_json::to_vec(input)?;
    data.push(b'\n');
    let timeout = hook.timeout;
    let event = hook.event;
    Ok(Box::pin(async move {
        let _tree = _tree;
        let _progress = _progress;
        let operation = async {
            let write = async {
                stdin.write_all(&data).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            };
            let read_out = async {
                let mut bytes = Vec::new();
                stdout
                    .take(OUTPUT_LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                Ok::<_, std::io::Error>(bytes)
            };
            let read_err = async {
                let mut bytes = Vec::new();
                stderr
                    .take(OUTPUT_LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                Ok::<_, std::io::Error>(bytes)
            };
            let (written, status, stdout, stderr) =
                tokio::join!(write, child.wait(), read_out, read_err);
            // A hook may intentionally exit without reading its input.
            if let Err(error) = written {
                if error.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(error.into());
                }
            }
            let stdout = stdout?;
            let stderr = stderr?;
            ensure!(
                stdout.len() <= OUTPUT_LIMIT as usize && stderr.len() <= OUTPUT_LIMIT as usize,
                "Hook output exceeded 1 MiB"
            );
            Ok((
                status?.code().unwrap_or(-1),
                String::from_utf8(stdout).context("Hook stdout is not UTF-8")?,
                String::from_utf8(stderr).context("Hook stderr is not UTF-8")?,
            ))
        };
        tokio::time::timeout(Duration::from_secs(timeout), operation)
            .await
            .with_context(|| format!("{} hook timed out after {} seconds", event.name(), timeout))?
    }))
}

#[cfg(windows)]
fn resume_command(pid: u32) -> Result<()> {
    use windows::Win32::{Foundation::CloseHandle, System::Threading::*};
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: windows::Win32::Foundation::HANDLE) -> i32;
    }
    unsafe {
        let process = OpenProcess(PROCESS_SUSPEND_RESUME, false, pid)?;
        let status = NtResumeProcess(process);
        let _ = CloseHandle(process);
        ensure!(status >= 0, "Resuming hook process failed: {status:#x}");
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) struct ProcessTree(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        use windows::Win32::{
            Foundation::CloseHandle,
            System::{JobObjects::*, Threading::*},
        };
        unsafe {
            let job = CreateJobObjectW(None, None)?;
            let tree = Self(job);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )?;
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)?;
            let assigned = AssignProcessToJobObject(job, process);
            let _ = CloseHandle(process);
            assigned?;
            Ok(tree)
        }
    }
}
#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(unix)]
pub(crate) struct ProcessTree(u32);
#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        Ok(Self(pid))
    }
}
#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe {
            kill(-(self.0 as i32), 9);
        }
        // Drop only *requests* cleanup. A ToolScript job that reports a
        // completed cancellation must wait for the proof below instead of
        // claiming the OS processes are gone.
        pending_cleanup()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(self.0);
    }
}

#[cfg(unix)]
fn pending_cleanup() -> &'static std::sync::Mutex<Vec<u32>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<Vec<u32>>> = std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

#[cfg(unix)]
fn process_group_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // Signal 0 only checks existence; the group is the one this tree owns.
    unsafe { kill(-(pid as i32), 0) == 0 }
}

/// Proof that every process group a dropped tool body signalled is gone.
pub(crate) struct CleanupReport {
    /// Whether this platform can verify awaited cleanup at all.
    pub(crate) verified: bool,
    /// Groups still alive after the limit; empty means cleanup is complete.
    pub(crate) pending: Vec<u32>,
}

/// Waits until the owned process groups signalled by dropped bodies are gone.
///
/// `ProcessTree::drop` cannot block, so this is the explicit boundary a caller
/// must await before publishing a terminal "cancelled" state.
pub(crate) async fn await_process_cleanup(limit: std::time::Duration) -> CleanupReport {
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + limit;
        loop {
            let mut pending = pending_cleanup()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            pending.retain(|pid| process_group_alive(*pid));
            if pending.is_empty() {
                return CleanupReport {
                    verified: true,
                    pending: Vec::new(),
                };
            }
            if std::time::Instant::now() >= deadline {
                return CleanupReport {
                    verified: true,
                    pending: pending.clone(),
                };
            }
            drop(pending);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = limit;
        CleanupReport {
            verified: false,
            pending: Vec::new(),
        }
    }
}

#[cfg(unix)]
pub(crate) fn clear_pending_cleanup() {
    pending_cleanup()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_request_requires_typed_event_and_valid_decision() {
        for output in [
            json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","decision":{"behavior":"allow"}}}),
            json!({"hookSpecificOutput":{"decision":{"behavior":"allow"}}}),
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":[]}}}),
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","interrupt":"true"}}}),
        ] { assert!(parse_output(HookEvent::PermissionRequest,0,&output.to_string(),"").is_err()); }
        let allow = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{}}}});
        assert!(matches!(parse_output(HookEvent::PermissionRequest,0,&allow.to_string(),"").unwrap().permission_response,Some(HookPermissionResponse::Allow {..})));
        for exit in [1,2] {assert!(parse_output(HookEvent::PermissionRequest,exit,&allow.to_string(),"failure").unwrap().permission_response.is_none());}
    }

    #[tokio::test]
    async fn permission_request_hooks_race_each_other_and_ignore_invalid_peers() {
        let root = tempfile::tempdir().unwrap();
        let allow = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        let commands = if cfg!(windows) {
            vec!["[IO.File]::WriteAllText('slow-started','yes'); Start-Sleep -Seconds 30".into(), "[Console]::Out.Write('{invalid')".into(), format!("while (-not [IO.File]::Exists('slow-started')) {{Start-Sleep -Milliseconds 20}}; [Console]::Out.Write('{allow}')")]
        } else {
            vec!["printf yes > slow-started; sleep 30".into(), "printf '%s' '{invalid'".into(), format!("while [ ! -f slow-started ]; do sleep 0.02; done; printf '%s' '{allow}'")]
        };
        let runner = HookRunner {
            workspace: root.path().into(),
            hooks: commands
                .into_iter()
                .map(|command| ActiveHook {
                    event: HookEvent::PermissionRequest,
                    matcher: "Write".into(),
                    command,
                    http: None,
                    prompt: None,
                    timeout: 35,
                    source: root.path().into(),
                    shell: Some(if cfg!(windows) { "powershell" } else { "sh" }.into()),
                    if_condition: None,
                    once: false,
                    asynchronous: false,
                    async_rewake: false,
                    status_message: None,
                })
                .collect(),
            ..Default::default()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            runner.run(
                HookEvent::PermissionRequest,
                "session",
                json!({"tool_name":"Write","tool_use_id":"exact-id"}),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            result.permission_response,
            Some(HookPermissionResponse::Allow { .. })
        ));
        assert!(root.path().join("slow-started").exists());
    }

    #[test]
    fn matcher_and_output_decisions_are_validated() {
        assert!(matches_tool("Edit|Write", "Edit").unwrap());
        assert!(!matches_tool("^Read$", "ReadExtra").unwrap());
        assert!(matches_tool("*", "Anything").unwrap());
        assert!(
            parse_output(HookEvent::PreToolUse, 2, "", "policy reason")
                .unwrap()
                .blocked
        );
        let outcome = parse_output(HookEvent::PreToolUse, 0, r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"reason","updatedInput":{"path":"safe"}}}"#, "").unwrap();
        assert!(outcome.blocked);
        assert_eq!(outcome.reason, "reason");
        assert_eq!(outcome.updated_input.unwrap()["path"], "safe");
        assert!(parse_output(
            HookEvent::Stop,
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse"}}"#,
            ""
        )
        .is_err());
        assert!(parse_output(HookEvent::Stop, 0, r#"{"continue":"false"}"#, "").is_err());
    }

    #[test]
    fn permission_denied_output_is_only_retry_advice() {
        let outcome = parse_output(HookEvent::PermissionDenied, 0, r#"{"decision":"approve","continue":false,"hookSpecificOutput":{"hookEventName":"PermissionDenied","retry":true,"updatedInput":{"file_path":"unsafe"},"permissionDecision":"allow"}}"#, "").unwrap();
        assert!(outcome.retry); assert!(!outcome.blocked); assert!(!outcome.prevent_continuation); assert!(outcome.updated_input.is_none()); assert!(!outcome.ask_permission);
        for text in [r#"{"hookSpecificOutput":{"hookEventName":"PermissionDenied","retry":"true"}}"#, r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","retry":true}}"#, "{invalid"] { assert!(parse_output(HookEvent::PermissionDenied,0,text,"").is_err()); }
        assert!(!parse_output(HookEvent::PermissionDenied,1,r#"{"hookSpecificOutput":{"hookEventName":"PermissionDenied","retry":true}}"#,"failed").unwrap().retry);
    }

    #[test]
    fn configuration_merges_and_disables_effective_hooks() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        let config = r#"{"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"echo check"}]}]}}"#;
        std::fs::write(data.join("settings.json"), config).unwrap();
        std::fs::write(workspace.join(".claude/settings.json"), config).unwrap();
        let runner = HookRunner::load(&data, &workspace).unwrap();
        assert_eq!(runner.list().len(), 2);
        // Preserve the serialized provenance of existing hook worktrees.
        let serialized = serde_json::to_value(&runner.list()[0]).unwrap();
        assert!(serialized.get("once").is_none());
        assert!(serialized.get("statusMessage").is_none());
        std::fs::write(
            workspace.join(".claude/settings.local.json"),
            r#"{"disableAllHooks":true}"#,
        )
        .unwrap();
        assert!(HookRunner::load(&data, &workspace)
            .unwrap()
            .list()
            .is_empty());
    }

    #[tokio::test]
    async fn command_stdin_and_exit_two_work() {
        let root = tempfile::tempdir().unwrap();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "$v=[Console]::In.ReadToEnd() | ConvertFrom-Json; if ($v.session_id -ne 'test') {exit 7}; [Console]::Error.Write('denied'); exit 2")
        } else {
            (
                "sh",
                "read line; case \"$line\" in *test*) printf denied >&2; exit 2;; *) exit 7;; esac",
            )
        };
        let hook = ActiveHook {
            event: HookEvent::PreToolUse,
            matcher: "*".into(),
            command: command.into(),
            http: None,
            prompt: None,
            timeout: 10,
            source: root.path().into(),
            shell: Some(shell.into()),
            if_condition: None,
            once: false,
            asynchronous: false,
            async_rewake: false,
            status_message: None,
        };
        let runner = HookRunner {
            workspace: root.path().into(),
            hooks: vec![hook],
            ..Default::default()
        };
        let output = runner
            .run(HookEvent::PreToolUse, "test", json!({"tool_name":"Read"}))
            .await
            .unwrap();
        assert!(output.blocked);
        assert_eq!(output.reason, "denied");
    }

    #[tokio::test]
    async fn if_condition_filters_tool_hooks_before_spawning() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let (shell, command, session) = if cfg!(windows) {
            (
                "powershell",
                "[Console]::Error.Write('ran'); exit 2",
                "[IO.File]::WriteAllText('session-ran','x')",
            )
        } else {
            ("sh", "printf ran >&2; exit 2", "printf x > session-ran")
        };
        let hooks = |condition: &str| {
            json!({"hooks":{"PreToolUse":[{"matcher":"*","hooks":[
                {"type":"command","command":command,"shell":shell,"if":condition}
            ]}],"SessionStart":[{"hooks":[
                {"type":"command","command":session,"shell":shell,"if":"Bash"}
            ]}]}})
        };
        let blocked = |condition: &'static str, tool: &'static str, input: Value| {
            let data = data.clone();
            let workspace = root.path().to_path_buf();
            async move {
                std::fs::write(data.join("settings.json"), hooks(condition).to_string()).unwrap();
                let runner = HookRunner::load(&data, &workspace).unwrap();
                runner
                    .run(
                        HookEvent::PreToolUse,
                        "test",
                        json!({"tool_name":tool,"tool_input":input}),
                    )
                    .await
                    .unwrap()
                    .blocked
            }
        };
        assert!(
            blocked(
                "Bash(git push:*)",
                "Bash",
                json!({"command":"git push origin"})
            )
            .await
        );
        assert!(!blocked("Bash(git push:*)", "Bash", json!({"command":"git status"})).await);
        // Compound commands cannot be proven outside the pattern.
        assert!(
            blocked(
                "Bash(git push:*)",
                "Bash",
                json!({"command":"echo ok && git push"})
            )
            .await
        );
        assert!(!blocked("Bash(git push:*)", "Write", json!({"command":"git push"})).await);
        assert!(blocked("Write(src/**)", "Write", json!({"file_path":"src/main.rs"})).await);
        assert!(!blocked("Write(src/**)", "Write", json!({"file_path":"docs/a.md"})).await);
        assert!(blocked("Task(*)", "Agent", json!({})).await);
        assert!(blocked("mcp__fixture", "mcp__fixture__echo", json!({})).await);
        assert!(!blocked("mcp__fixture", "mcp__other__echo", json!({})).await);
        // Content on a tool without a pattern matcher never matches.
        assert!(!blocked("Grep(*.rs)", "Grep", json!({"pattern":"x"})).await);
        let runner = HookRunner::load(&data, root.path()).unwrap();
        assert_eq!(runner.list()[0].if_condition.as_deref(), Some("Grep(*.rs)"));
        runner
            .run(HookEvent::SessionStart, "test", json!({"source":"startup"}))
            .await
            .unwrap();
        assert!(!root.path().join("session-ran").exists());
    }

    #[tokio::test]
    async fn hook_timeout_is_an_error_not_permission_to_continue() {
        let root = tempfile::tempdir().unwrap();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "Start-Sleep -Seconds 30")
        } else {
            ("sh", "sleep 30")
        };
        let hook = ActiveHook {
            event: HookEvent::PreToolUse,
            matcher: "".into(),
            command: command.into(),
            http: None,
            prompt: None,
            timeout: 1,
            source: root.path().into(),
            shell: Some(shell.into()),
            if_condition: None,
            once: false,
            asynchronous: false,
            async_rewake: false,
            status_message: None,
        };
        let error = execute(&hook, root.path(), &json!({})).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn cancelled_hook_cannot_keep_running() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_path_buf();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "[IO.File]::WriteAllText('started','yes'); Start-Sleep -Seconds 2; [IO.File]::WriteAllText('survived','bad')")
        } else {
            ("sh", "printf yes > started; sleep 2; printf bad > survived")
        };
        let hook = ActiveHook {
            event: HookEvent::PreToolUse,
            matcher: "".into(),
            command: command.into(),
            http: None,
            prompt: None,
            timeout: 10,
            source: cwd.clone(),
            shell: Some(shell.into()),
            if_condition: None,
            once: false,
            asynchronous: false,
            async_rewake: false,
            status_message: None,
        };
        let task = tokio::spawn(async move { execute(&hook, &cwd, &json!({})).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!root.path().join("survived").exists());
    }
}

/// Only the reference environment events can register dynamic watch paths.
pub(crate) fn watch_paths(event: HookEvent, output: &Value) -> Result<Vec<String>> {
    let Some(specific) = output.get("hookSpecificOutput") else {
        return Ok(vec![]);
    };
    let Some(paths) = specific.get("watchPaths") else {
        return Ok(vec![]);
    };
    ensure!(
        matches!(
            event,
            HookEvent::SessionStart | HookEvent::CwdChanged | HookEvent::FileChanged
        ),
        "watchPaths requires SessionStart, CwdChanged or FileChanged"
    );
    ensure!(
        specific.get("hookEventName").and_then(Value::as_str) == Some(event.name()),
        "Hook returned an incorrect hookEventName"
    );
    let paths: Vec<String> =
        serde_json::from_value(paths.clone()).context("watchPaths must be an array of strings")?;
    ensure!(
        paths.iter().all(|p| Path::new(p).is_absolute()),
        "watchPaths must contain absolute paths"
    );
    Ok(paths)
}

impl HookRunner {
    pub(crate) fn file_watch_paths(&self, session: &str, dynamic: &[String]) -> Vec<PathBuf> {
        let mut matchers: Vec<String> = self
            .hooks
            .iter()
            .filter(|h| h.event == HookEvent::FileChanged)
            .map(|h| h.matcher.clone())
            .collect();
        if let Some(callbacks) = self.callbacks(session) {
            matchers.extend(callbacks.file_watch_matchers());
        }
        let mut paths: Vec<PathBuf> = matchers
            .iter()
            .flat_map(|m| m.split('|'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|p| self.workspace.join(p))
            .collect();
        paths.extend(dynamic.iter().map(PathBuf::from));
        paths.sort();
        paths.dedup();
        paths
    }
}
