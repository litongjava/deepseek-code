use crate::{
    args::{Cli, OutputFormat, PermissionModeArg},
    config::Selection,
};
use anyhow::{bail, Context, Result};
use deepseek_code_core::{
    cache::FileCache,
    db::message_repo,
    mcp::{McpServerManager, McpToolRegistry},
    native_engine::{
        engine_core::ChatRequest, provider_manager::ProviderManager, tool_loop::EngineEvent,
        NativeEngine,
    },
    permissions::PermissionMode,
    runtime::{self, RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    sync::Arc,
};
use tokio::sync::Mutex;
use crate::terminal::input::TerminalInput;

/// One stdin reader for the entire REPL, also used for tool questions. A detached
/// OS thread avoids Tokio's uncancellable stdin read blocking runtime shutdown.
pub fn terminal_input() -> TerminalInput {
    TerminalInput::new()
}

pub struct Session {
    /// Runtime-only options of the latest worker turn, retained for internal wakes.
    pub(crate) hook_wake_cli: std::sync::Mutex<Option<Arc<Cli>>>,
    pub(crate) sdk_permissions: Option<Arc<crate::sdk::Permissions>>,
    pub(crate) sdk_content: Option<Value>,
    pub(crate) initial_hook_message: Option<String>,
    pub(crate) sdk_overrides: crate::sdk_initialization::Overrides,
    pub(crate) sdk_hooks: Option<crate::sdk_hooks::Runtime>,
    pub(crate) sdk_mcp: Arc<deepseek_code_core::mcp::sdk::Runtime>,
    pub(crate) sdk_mcp_transport: Option<Arc<crate::sdk_mcp::Transport>>,
    pub id: String,
    pub store: SessionStore,
    pub engine: NativeEngine,
    pub(crate) paths: RuntimePaths,
    pub(crate) permissions: Arc<deepseek_code_core::permissions::PermissionManager>,
    pub(crate) ide: Arc<deepseek_code_core::services::ide::IdeClient>,
    pub(crate) mcp: Arc<McpServerManager>,
    mcp_ready: Arc<tokio::sync::OnceCell<()>>,
    pub(crate) model: String,
    pub(crate) provider_id: String,
    pub(crate) auto_route: bool,
    /// Bypass is in the Shift+Tab cycle (`--allow-dangerously-skip-permissions`).
    pub(crate) bypass_available: bool,
    /// Launched with `--full-access`: bypass mode is Full Access.
    pub(crate) full_access: bool,
    providers: Arc<Mutex<ProviderManager>>,
    pub(crate) workspace: std::path::PathBuf,
    lifecycle_owned: bool,
    pub(crate) api_format: deepseek_code_core::native_engine::provider_manager::ApiFormat,
    remote_relay: std::sync::Mutex<Option<deepseek_code_core::services::remote_control::Relay>>,
}

impl Session {
    /// Remote Control channel for tool approvals and interrupts during turns.
    pub(crate) fn set_remote_relay(&self, relay: Option<deepseek_code_core::services::remote_control::Relay>) {
        *self.remote_relay.lock().unwrap() = relay;
    }

    pub(crate) fn remote_relay(&self) -> Option<deepseek_code_core::services::remote_control::Relay> {
        self.remote_relay.lock().unwrap().clone()
    }

    pub(crate) fn remote_interrupts(&self) -> deepseek_code_core::services::remote_control::Interrupts {
        self.remote_relay().map(|relay| relay.interrupts()).unwrap_or_default()
    }

    pub(crate) async fn validate_sdk_mcp_servers(&self, names: &[String]) -> Result<()> {
        if names.is_empty() { return Ok(()); }
        // Initialization validates persisted names without starting any process.
        let path = self.paths.data_dir.join("mcp-servers.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                let config: Value = serde_json::from_slice(&bytes)?;
                if let Some(servers) = config["mcpServers"].as_object() {
                    for (id, server) in servers {
                        let name = server["name"].as_str().unwrap_or(id);
                        anyhow::ensure!(!names.iter().any(|entry| entry == id || entry == name || format!("sdk:{entry}") == *id), "SDK MCP server conflicts with configured server: {name}");
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }
        for server in self.mcp.list_servers_full().await {
            anyhow::ensure!(!names.iter().any(|name| name == &server.config.name || name == &server.config.id || format!("sdk:{name}") == server.config.id), "SDK MCP server conflicts with configured server: {}", server.config.name);
        }
        Ok(())
    }

    pub(crate) async fn sdk_models(&self) -> Vec<Value> {
        let providers = self.providers.lock().await;
        let qualify = providers
            .list_providers()
            .iter()
            .filter(|provider| provider.enabled)
            .count()
            > 1;
        let mut models = Vec::new();
        for provider in providers
            .list_providers()
            .iter()
            .filter(|provider| provider.enabled)
        {
            for model in provider.models.iter().filter(|model| model.enabled) {
                let value = if qualify {
                    format!("{}::{}", provider.id, model.id)
                } else {
                    model.id.clone()
                };
                models.push(json!({"value":value,"displayName":model.name,
                    "description":format!("{} / {}",provider.name,model.name)}));
            }
        }
        models
    }

    /// Reference `system/init` message describing this session to SDK clients.
    pub(crate) async fn sdk_init_message(&self) -> Result<Value> {
        let mut tools = deepseek_code_core::tools::get_tool_definitions().into_iter().map(|tool| tool.name).collect::<Vec<_>>();
        tools.extend(self.mcp.get_all_tools().await.into_iter().map(|tool| format!("mcp__{}__{}", tool.server_name, tool.name)));
        // SDK-hosted servers are listed through the host; their names are already qualified.
        tools.extend(self.sdk_mcp.tools().await?.into_iter().map(|tool| tool.name));
        let skills = crate::skill_commands::load_for_workspace(&self.paths, &self.workspace).await?;
        let commands = crate::skill_commands::catalog(&skills).into_iter().map(|(name, _)| name.trim_start_matches('/').to_owned()).collect::<Vec<_>>();
        let context = crate::services::context(&self.paths)?;
        let agents = deepseek_code_core::services::agents::list(&context)?.into_iter().map(|agent| agent.id).collect::<Vec<_>>();
        Ok(json!({"type":"system","subtype":"init","cwd":self.workspace,"session_id":self.id,"tools":tools,
            "mcp_servers":self.sdk_mcp_status().await,"model":self.model,
            "permissionMode":crate::sdk_messages::reference_mode(self.permission_mode()),
            "slash_commands":commands,"agents":agents,"apiKeySource":"none",
            "version":env!("CARGO_PKG_VERSION"),"output_style":crate::display_commands::load(self)?.output_style,
            "uuid":uuid::Uuid::new_v4().to_string()}))
    }

    /// Reference MCP server status entries: configured servers plus SDK-hosted ones.
    pub(crate) async fn sdk_mcp_status(&self) -> Vec<Value> {
        let mut servers = self.mcp.list_servers().await.into_iter().map(|server| {
            let status = if !server.enabled { "disabled" } else if server.running { "connected" } else if server.error.is_some() { "failed" } else { "pending" };
            let mut entry = json!({"name":server.name,"status":status,"config":{"type":server.transport_type,"command":server.command,"args":server.args}});
            if let Some(error) = server.error { entry["error"] = json!(error); }
            entry
        }).collect::<Vec<_>>();
        servers.extend(self.sdk_mcp.names().into_iter().map(|name| json!({"name":name,"status":"connected","config":{"type":"sdk","name":name}})));
        servers
    }

    pub(crate) async fn sdk_resolve_model(
        &self,
        name: &str,
    ) -> Result<deepseek_code_core::native_engine::provider_manager::ResolvedProvider> {
        let providers = self.providers.lock().await;
        let (provider, model) = name.split_once("::").unwrap_or((&self.provider_id, name));
        providers
            .resolve_selected(provider, model)
            .context("SDK agent model is not configured")
    }

    pub(crate) async fn sdk_set_model(&mut self, name: &str) -> Result<()> {
        let providers=self.providers.lock().await;
        let (provider,model)=name.split_once("::").unwrap_or((&self.provider_id,name));
        let resolved=providers.resolve_selected(provider,model).context("SDK model is not configured")?;
        let selection=Selection {providers:providers.list_providers().to_vec(),provider_id:resolved.provider.id,model:model.to_owned()};
        drop(providers);
        let paths=self.paths.clone();
        self.switch(&paths,selection).await
    }
    pub fn runtime_status_source(&self) -> deepseek_code_core::services::runtime_status::RuntimeSource {
        deepseek_code_core::services::runtime_status::RuntimeSource {
            mcp: self.mcp.clone(),
            mcp_ready: self.mcp_ready.clone(),
            workspace: self.workspace.clone(),
            data_dir: self.paths.data_dir.clone(),
        }
    }

    pub async fn selected_provider(&self) -> Result<deepseek_code_core::native_engine::provider_manager::ResolvedProvider> {
        self.providers.lock().await.resolve_selected(&self.provider_id, &self.model).context("Selected provider/model is unavailable")
    }

    pub async fn set_fast(&self, enabled: bool) -> Result<()> {
        self.providers.lock().await.set_model_fast(&self.provider_id, &self.model, enabled)
    }

    pub async fn reload_plugins(&self) -> Result<()> {
        let context = crate::services::context(&self.paths)?;
        if self.mcp_ready.get().is_some() { deepseek_code_core::services::plugins::sync_mcp(&context, &self.mcp).await?; }
        deepseek_code_core::tools::configure_lsp_servers(
            &self.workspace.to_string_lossy(),
            deepseek_code_core::services::plugins::all_language_servers(&context)?,
        )?;
        Ok(())
    }

    pub(crate) async fn ensure_mcp(&self) -> Result<()> {
        self.mcp_ready.get_or_try_init(|| async {
            self.mcp.initialize().await?;
            for server in self.mcp.list_servers().await.into_iter().filter(|server|server.enabled && !server.running) {
                if let Err(error) = self.mcp.start_server(&server.id).await { eprintln!("MCP {}: {error}",server.name); }
            }
            Ok::<(), anyhow::Error>(())
        }).await?;
        Ok(())
    }

    pub async fn context_window(&self) -> Option<u32> {
        self.providers.lock().await.resolve_selected(&self.provider_id, &self.model)
            .and_then(|resolved| resolved.model.context_window)
    }
    pub async fn open(
        cli: &Cli,
        paths: &RuntimePaths,
        mut selection: Selection,
        resumed_id: Option<&str>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !cli.fork_session || resumed_id.is_some(),
            "--fork-session requires --resume or --continue"
        );
        let launch_rules = crate::launch_permissions::rules(cli)?;
        cli.system_prompt_text()?;
        deepseek_code_core::config::OrchestrationConfig::init_global()?;
        let db = paths.open_database()?;
        let store = SessionStore::new(db.clone());
        let id = if let Some(id) = resumed_id {
            store.resume(id).map_err(|error| crate::session_trust::diagnose(&store.db, paths, id, error))?;
            id.to_owned()
        } else {
            store.create_with_id(
                &std::env::current_dir()?,
                &selection.model,
                &selection.provider_id,
                &cli.session_id.clone().unwrap_or_else(||uuid::Uuid::new_v4().to_string()),
            ).context("Creating CLI session")?
        };
        let id = if cli.fork_session {
            store.fork(&id)?
        } else {
            id
        };
        let internal_worker = deepseek_code_core::services::background_sessions::worker_directory(&paths.data_dir, &id)?.is_some();
        let auto_route = if internal_worker { std::env::var("CLAUDE_RUST_WORKER_AUTO_ROUTE").as_deref() == Ok("true") } else { cli.auto_route };
        if internal_worker {
            if let Ok(value) = std::env::var("CLAUDE_RUST_WORKER_MODEL") {
                let model: deepseek_code_core::native_engine::provider_manager::ModelConfig = serde_json::from_str(&value)?;
                let provider = selection.providers.iter_mut().find(|provider|provider.id == selection.provider_id).context("Worker provider unavailable")?;
                let expected=deepseek_code_core::native_engine::provider_manager::resolve_model_selection(&provider.models,&selection.model).context("Worker model unavailable")?;
                anyhow::ensure!(model.id == expected.id, "Worker model snapshot does not match the selected model");
                let existing = provider.models.iter_mut().find(|existing|existing.id == model.id).context("Worker model unavailable")?;
                *existing = model;
            }
        }
        let workspace = store.workspace(&id, &paths.data_dir.join("workspaces")).map_err(|error| crate::session_trust::diagnose(&store.db, paths, &id, error))?;
        store.db.with_conn(|conn| {
            conn.execute(
                "UPDATE conversations SET model = ?1, provider = ?2 WHERE id = ?3",
                [&selection.model, &selection.provider_id, &id],
            )
        })??;
        let permissions = runtime::default_permissions();
        let loaded = deepseek_code_core::services::permission_updates::load_settings(
            &paths.data_dir,
            &workspace,
            true,
        )?;
        let mut merged = launch_rules;
        for (rule, action) in loaded.rules {
            match merged.get(&rule) {
                Some(deepseek_code_core::permissions::rules::PermissionAction::Deny) => {}
                _ => {
                    merged.insert(rule, action);
                }
            }
        }
        permissions.set_launch_rules(&id, merged)?;
        permissions.set_session_directories(&id, loaded.directories);
        if let Some(crate::args::Commands::Agent { agent_type, .. }) = &cli.command {
            let definition = deepseek_code_core::services::agents::resolve(&crate::services::context(paths)?, agent_type)?;
            deepseek_code_core::services::agents::bind(&store.db, &id, &definition)?;
        }
        let selected_mode = match cli.permission_mode {
            PermissionModeArg::Ask => PermissionMode::AskPermissions,
            PermissionModeArg::AcceptEdits => PermissionMode::AcceptEdits,
            PermissionModeArg::Plan => PermissionMode::PlanMode,
            PermissionModeArg::Bypass => PermissionMode::BypassPermissions,
            PermissionModeArg::DontAsk => PermissionMode::DontAsk,
            PermissionModeArg::Auto => PermissionMode::Auto,
        };
        let explicit_mode = cli.permission_mode_explicit || cli.dangerously_skip_permissions || !matches!(cli.permission_mode, PermissionModeArg::Ask);
        let active_job = if internal_worker { None } else { deepseek_code_core::services::background_sessions::list(&paths.data_dir, None)?.into_iter().find(|job|job.session_id == id && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")) };
        if let Some(job) = &active_job {
            anyhow::ensure!(cli.allowed_tools.is_empty() && cli.disallowed_tools.is_empty(), "This session has active background work; attach without launch tool rules, or stop the worker before changing them");
            anyhow::ensure!(!explicit_mode, "This session has active background work; resume without an explicit permission mode and use /tasks attach {} to change its mode", job.id);
        }
        let selected_mode = if explicit_mode {
            selected_mode
        } else {
            deepseek_code_core::services::session_permission_mode::load(&store.db, &id)?.filter(|mode| *mode != PermissionMode::BypassPermissions).unwrap_or(selected_mode)
        };
        if selected_mode == PermissionMode::Auto {
            let provider = selection.providers.iter().find(|p|p.id == selection.provider_id).context("Main provider unavailable")?;
            let model = deepseek_code_core::native_engine::provider_manager::resolve_model_selection(&provider.models, &selection.model).context("Main model unavailable")?;
            deepseek_code_core::native_engine::auto_permissions::configured(&crate::services::context(paths)?, &deepseek_code_core::native_engine::provider_manager::ResolvedProvider { provider:provider.clone(),model })?;
        }
        if active_job.is_some() { permissions.set_mode(selected_mode); }
        else { deepseek_code_core::services::session_permission_mode::set(&store.db, &id, &permissions, selected_mode)?; }
        let api_format = selection
            .providers
            .iter()
            .find(|p| p.id == selection.provider_id)
            .context("Selected provider unavailable")?
            .api_format
            .clone();
        let providers = Arc::new(Mutex::new(ProviderManager::in_memory(
            selection.providers,
            paths.data_dir.join("providers.json"),
        )));
        let mcp = Arc::new(McpServerManager::new(
            paths.data_dir.join("mcp-servers.json"),
        ));
        let ide = Arc::new(deepseek_code_core::services::ide::IdeClient::default());
        let sdk_mcp = Arc::new(deepseek_code_core::mcp::sdk::Runtime::default());
        let engine = runtime::create_engine(
            paths,
            db,
            providers.clone(),
            permissions.clone(),
            Arc::new(FileCache::new()),
        )
        .with_mcp_registry(Arc::new(McpToolRegistry::new(mcp.clone()).with_ide(ide.clone()).with_sdk(sdk_mcp.clone())))
        .with_skills_manager(Arc::new(Mutex::new(
            deepseek_code_core::skills::SkillsManager::with_path(paths.data_dir.join("skills")),
        )));
        Ok(Self {
            hook_wake_cli: std::sync::Mutex::new(None),
            initial_hook_message: None,
            permissions,
            ide,
            paths: paths.clone(),
            id,
            store,
            engine,
            mcp,
            mcp_ready: Arc::new(tokio::sync::OnceCell::new()),
            model: selection.model,
            provider_id: selection.provider_id,
            providers,
            workspace,
            auto_route,
            bypass_available: cli.allow_dangerously_skip_permissions,
            full_access: cli.full_access,
            sdk_permissions: None,
            sdk_content: None,
            sdk_overrides: Default::default(),
            sdk_hooks: None,
            sdk_mcp,
            sdk_mcp_transport: None,
            lifecycle_owned: false,
            api_format,
            remote_relay: Default::default(),
        })
    }

    pub async fn switch(&mut self, paths: &RuntimePaths, selection: Selection) -> Result<()> {
        let api_format = selection
            .providers
            .iter()
            .find(|p| p.id == selection.provider_id)
            .context("Selected provider unavailable")?
            .api_format
            .clone();
        let next =
            ProviderManager::in_memory(selection.providers, paths.data_dir.join("providers.json"));
        anyhow::ensure!(
            next.resolve_selected(&selection.provider_id, &selection.model)
                .is_some(),
            "Selected model unavailable"
        );
        let mut manager = self.providers.lock().await;
        self.store.db.with_conn(|conn| {
            conn.execute(
                "UPDATE conversations SET model = ?1, provider = ?2 WHERE id = ?3",
                [&selection.model, &selection.provider_id, &self.id],
            )
        })??;
        *manager = next;
        self.model = selection.model;
        self.provider_id = selection.provider_id;
        self.api_format = api_format;
        self.auto_route = false;
        Ok(())
    }

    pub async fn start_hooks(&mut self, source: &str) -> Result<()> {
        use deepseek_code_core::services::session_hooks;
        if self.lifecycle_owned {
            return Ok(());
        }
        let context = self.hook_service_context();
        if !session_hooks::owns_lifecycle(&context, &self.id)? {
            return Ok(());
        }
        deepseek_code_core::services::lifecycle_events::check_configuration(
            &context,
            &self.workspace,
            &self.id,
        )
        .await?;
        self.lifecycle_owned = true;
        let outcome = tokio::select! {
            result = crate::hook_progress::during(&self.id, true, session_hooks::start_with_output(&context, &self.workspace, &self.id, source, &self.model)) => result,
            _ = tokio::signal::ctrl_c() => anyhow::bail!("SessionStart hooks interrupted"),
            _ = deepseek_code_core::services::background_sessions::wait_for_cancellation() => anyhow::bail!("SessionStart hooks cancelled"),
        };
        match outcome {
            Ok(outcome) => {
                self.initial_hook_message = outcome.initial_user_message;
                for warning in outcome.warnings {
                    eprintln!("Hook: {warning}");
                }
            }
            Err(error) => eprintln!("SessionStart hook: {error:#}"),
        }
        Ok(())
    }

    pub async fn end_hooks(&mut self, reason: &str) {
        if !std::mem::take(&mut self.lifecycle_owned) {
            return;
        }
        let context = self.hook_service_context();
        for warning in crate::hook_progress::during(&self.id, true, deepseek_code_core::services::session_hooks::end(
            &context,
            &self.workspace,
            &self.id,
            reason,
            deepseek_code_core::services::session_hooks::end_timeout(),
        ))
        .await
        {
            eprintln!("Hook: {warning}");
        }
    }

    pub(crate) fn hook_service_context(&self) -> deepseek_code_core::services::ServiceContext {
        deepseek_code_core::services::ServiceContext {
            paths: self.paths.clone(),
            db: self.store.db.clone(),
            credentials: Arc::new(deepseek_code_core::services::EnvironmentCredentials),
        }
    }

    pub async fn clear(&mut self) -> Result<String> {
        let id = self.store.clear(&self.id)?;
        crate::session_commands::copy_settings(&self.store.db, &self.id, &id)?;
        let lifecycle_owned = self.lifecycle_owned;
        self.end_hooks("clear").await;
        self.id = id.clone();
        if lifecycle_owned {
            self.start_hooks("clear").await?;
        }
        Ok(id)
    }

    /// Validate the project and provider before replacing the current session.
    pub async fn resume_saved(&mut self, paths: &RuntimePaths, id: &str) -> Result<()> {
        let saved = self.store.resume(id).map_err(|error| crate::session_trust::diagnose(&self.store.db, paths, id, error))?;
        let workspace = self.store.workspace(id, &paths.data_dir.join("workspaces")).map_err(|error| crate::session_trust::diagnose(&self.store.db, paths, id, error))?;
        let model = saved.model.context("Saved session has no model")?;
        let provider_id = saved.provider.context("Saved session has no provider")?;
        let resolved = self.providers.lock().await.resolve_selected(&provider_id, &model)
            .context("Saved provider/model is unavailable; current session retained")?;
        let lifecycle_owned = self.lifecycle_owned;
        // As in the reference CLI, the resumed conversation keeps the mode in use.
        let mode = self.permissions.get_mode();
        self.end_hooks("other").await;
        self.workspace = workspace;
        self.api_format = resolved.provider.api_format;
        self.model = model;
        self.provider_id = provider_id;
        self.auto_route = false;
        self.id = id.to_owned();
        deepseek_code_core::services::session_permission_mode::set(&self.store.db, &self.id, &self.permissions, mode)?;
        if lifecycle_owned { self.start_hooks("resume").await?; }
        Ok(())
    }

    /// A separate engine on the same providers, for side requests such as
    /// naming the session's topic while a turn runs.
    pub fn side_engine(&self) -> NativeEngine {
        runtime::create_engine(&self.paths, self.store.db.clone(), self.providers.clone(), self.permissions.clone(), Arc::new(FileCache::new()))
    }

    pub async fn branch_saved(&mut self, paths: &RuntimePaths, title: &str) -> Result<String> {
        anyhow::ensure!(title.chars().count() <= 200, "Session titles must be at most 200 characters");
        let id = self.store.fork(&self.id)?;
        crate::session_commands::copy_settings(&self.store.db, &self.id, &id)?;
        if !title.is_empty() {
            self.store.db.with_conn(|conn| {
                deepseek_code_core::db::conversation_repo::update_conversation_title(conn, &id, title)
            })??;
        }
        self.resume_saved(paths, &id).await?;
        Ok(id)
    }

    pub fn permission_mode(&self) -> &'static str {
        match self.permissions.get_mode() {
            PermissionMode::AskPermissions => "ask",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::PlanMode => "plan",
            PermissionMode::BypassPermissions => "bypass",
            PermissionMode::DontAsk => "dont-ask",
            PermissionMode::Auto => "auto",
        }
    }

    /// As in the reference CLI: ask, accept edits, plan, bypass (when the
    /// launch allows it), auto, then ask again.
    pub fn next_permission_mode(&self) -> &'static str {
        next_mode(self.permission_mode(), self.bypass_available)
    }

    pub async fn cycle_permission_mode(&self) -> Result<&'static str> {
        // Auto mode needs its classifier; without one the cycle goes on to ask.
        if self.set_permission_mode(self.next_permission_mode()).await.is_err() {
            self.set_permission_mode("ask").await?;
        }
        Ok(self.permission_mode())
    }

    pub(crate) async fn check_host_shell_permission(&self, input: &Value, user_prompt: &str, interactive: bool, hooks: &deepseek_code_core::services::tool_hooks::HookRunner, tool_use_id: &str) -> Result<deepseek_code_core::permissions::PermissionResult> {
        let context = crate::services::context(&self.paths)?;
        let provider = self.selected_provider().await?;
        let mut history = self.store.model_history(&self.id, &provider.provider.api_format)?;
        history.push(json!({"role":"user","content":user_prompt}));
        let tool = deepseek_code_core::permissions::PermissionContext { tool_name:"Bash".into(), tool_input:input.clone(), conversation_id:self.id.clone(), user_id:None, workspace_path:Some(self.workspace.to_string_lossy().into_owned()) };
        let authorization = deepseek_code_core::native_engine::auto_permissions::authorize(&context, &self.permissions, &tool, &provider, &history, interactive).await
            .with_denial_hook(Some(hooks), &tool, tool_use_id).await;
        Ok(authorization.result)
    }

    /// Whether an approval can offer "switch to auto mode": only once auto
    /// permissions are configured, never as a way to enable them.
    pub async fn auto_mode_available(&self) -> bool {
        if self.permissions.get_mode() == PermissionMode::Auto {
            return false;
        }
        let Ok(context) = crate::services::context(&self.paths) else {
            return false;
        };
        match self.selected_provider().await {
            Ok(provider) => deepseek_code_core::native_engine::auto_permissions::configured(&context, &provider).is_ok(),
            Err(_) => false,
        }
    }

    pub async fn set_permission_mode(&self, value: &str) -> Result<()> {
        if deepseek_code_core::services::background_sessions::worker_directory(&self.paths.data_dir, &self.id)?.is_none() {
            if let Some(job) = deepseek_code_core::services::background_sessions::list(&self.paths.data_dir, None)?.into_iter().find(|job|job.session_id == self.id && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")) {
                anyhow::bail!("This session has active background work; use /tasks attach {} to change its permission mode", job.id);
            }
        }
        let mode = deepseek_code_core::services::session_permission_mode::parse(value)?;
        if mode == PermissionMode::Auto {
            deepseek_code_core::native_engine::auto_permissions::configured(&crate::services::context(&self.paths)?, &self.selected_provider().await?)?;
        }
        anyhow::ensure!(mode != PermissionMode::BypassPermissions || self.bypass_available, "Bypass permissions mode needs a launch with --dangerously-skip-permissions or --allow-dangerously-skip-permissions");
        deepseek_code_core::services::session_permission_mode::set(&self.store.db, &self.id, &self.permissions, mode)?;
        deepseek_code_core::permissions::set_full_access(self.full_access && mode == PermissionMode::BypassPermissions);
        Ok(())
    }

    /// CLI launch rules win on conflicts, while a persisted deny is never relaxed.
    fn apply_settings_permissions(&self, cli: &Cli) -> Result<()> {
        let loaded = deepseek_code_core::services::permission_updates::load_settings(
            &self.paths.data_dir,
            &self.workspace,
            true,
        )?;
        let mut rules = crate::launch_permissions::rules(cli)?;
        for (rule, action) in loaded.rules {
            match rules.get(&rule) {
                Some(deepseek_code_core::permissions::rules::PermissionAction::Deny) => {}
                _ => {
                    rules.insert(rule, action);
                }
            }
        }
        self.permissions.set_launch_rules(&self.id, rules)?;
        self.permissions
            .set_session_directories(&self.id, loaded.directories);
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        deepseek_code_core::services::async_hooks::cancel_shared(&self.paths.data_dir, &self.id).await;
        self.engine.cancel_turn(&self.id).await;
        self.ide.disconnect().await;
        deepseek_code_core::tools::shutdown_lsp_servers().await;
        self.mcp.shutdown_all().await
    }

    /// Follows EnterWorktree/ExitWorktree from the last turn: the bound
    /// worktree, or the recorded source folder once a worktree is left.
    pub fn sync_workspace(&mut self) -> Result<()> {
        let target = match deepseek_code_core::worktree::hooks::status(&self.store.db, &self.id)? {
            Some(state) => state["path"].as_str().map(std::path::PathBuf::from),
            // A worktree checkout has a .git file; the main checkout a directory.
            None if self.workspace.join(".git").is_file() => self.store.db.with_conn(|conn| {
                conn.query_row("SELECT workspace_path FROM conversations WHERE id=?1", [&self.id], |row| row.get::<_, Option<String>>(0))
            })??.map(std::path::PathBuf::from),
            None => None,
        };
        if let Some(target) = target.filter(|path| path.is_dir() && *path != self.workspace) {
            std::env::set_current_dir(&target)?;
            self.workspace = target;
        }
        Ok(())
    }

    pub async fn turn(
        &self,
        cli: &Cli,
        prompt: &str,
        input: &mut Option<TerminalInput>,
    ) -> Result<()> {
        use std::io::IsTerminal;
        self.apply_settings_permissions(cli)?;
        // Attach @-mentioned files once, in the frontend; a worker receives the
        // expanded prompt and must not attach them again.
        let frontend = std::env::var_os(deepseek_code_core::services::background_sessions::CONTROL_ENV).is_none();
        let mentioned = (frontend && self.sdk_content.is_none())
            .then(|| crate::mentions::expand(&self.workspace, prompt))
            .flatten();
        let prompt = mentioned.as_deref().unwrap_or(prompt);
        if frontend {
            crate::extended_commands::record_branch(self);
        }
        // Frontends record original human input before skill expansion/worker handoff.
        // A worker receives synthesized context and must never re-label it as user intent.
        if std::env::var_os(deepseek_code_core::services::background_sessions::CONTROL_ENV).is_none() {
            deepseek_code_core::services::permission_authority::record_user(&self.store.db, &self.id, self.sdk_content.as_ref().unwrap_or(&json!(prompt)))?;
        }
        // Keep engine/goal futures out of the outer host stack. Debug builds on
        // Windows otherwise exhaust the process stack before a worker can run.
        if self.sdk_content.is_some() {return Box::pin(self.run_turn(cli,prompt,input)).await;}
        let hook_session = self.id.clone();
        let hook_interactive = input.is_some() && cli.output_format == OutputFormat::Text;
        let prepared = tokio::select! {
            prepared = crate::hook_progress::during(&hook_session, hook_interactive, crate::skill_commands::prepare_invocation(cli, self, prompt, input)) => prepared?,
            _ = tokio::signal::ctrl_c() => bail!("Skill invocation cancelled"),
        };
        let expanded;
        let prompt = match prepared {
            crate::skill_commands::PreparedInvocation::NotHandled => prompt,
            crate::skill_commands::PreparedInvocation::Inline(value) => { expanded = value; expanded.as_str() },
            crate::skill_commands::PreparedInvocation::Background(job) => {
                return crate::skill_commands::finish_background(cli, self, job, input).await;
            },
        };
        crate::session_commands::migrate_goal(self)?;
        deepseek_code_core::services::session_goals::begin_input(&self.store.db, &self.id)?;
        if !crate::terminal::screen_reader() && input.is_some() && cli.output_format == OutputFormat::Text && !cli.print && !cli.print_short
            && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb") && !crate::display_commands::load(self)?.focus {
            return crate::worker_frontend::turn(cli, self, prompt, input).await;
        }
        let mut next = prompt.to_owned();
        loop {
            let goal = deepseek_code_core::services::session_goals::active(&self.store.db, &self.id)?;
            if let (Some(goal), Some(limit)) = (&goal, cli.max_turns) {
                if goal.model_requests.saturating_sub(goal.run_start_requests) >= u64::from(limit) {
                    deepseek_code_core::services::session_goals::pause(&self.store.db, &self.id, &goal.id, "Maximum turns reached")?;
                    crate::session_commands::goal_notice("Goal paused: maximum turns reached. Send a message to continue.");
                    break;
                }
            }
            let result = async {
                Box::pin(self.run_turn(cli, &next, input)).await?;
                if (input.is_none() || cli.output_format != OutputFormat::Text) && std::env::var_os(deepseek_code_core::services::background_sessions::CONTROL_ENV).is_none() {
                    return Ok::<Option<String>,anyhow::Error>(None);
                }
                if let Some(kickoff) = deepseek_code_core::services::goal_proposals::activate(&self.store.db, &self.id, &self.paths.data_dir, &self.workspace, &self.permissions)? {
                    crate::session_commands::goal_notice("Approved session goal is now active.");
                    return Ok::<Option<String>,anyhow::Error>(Some(kickoff));
                }
                Box::pin(crate::session_commands::goal_check(self)).await
            }.await;
            match result {
                Ok(Some(followup)) => next = followup,
                Ok(None) => break,
                Err(error) => {
                    deepseek_code_core::services::goal_proposals::discard(&self.store.db, &self.id)?;
                    if let Some(goal) = goal {
                        if crate::session_commands::retryable_goal_error(&error, std::env::var_os(deepseek_code_core::services::background_sessions::CONTROL_ENV).is_some()) {
                            if let Some(retry) = deepseek_code_core::services::session_goals::arm_retry(&self.store.db, &self.id, &goal.id, &error.to_string())? {
                                let seconds = (retry.retry_after_ms.unwrap_or_default() - chrono::Utc::now().timestamp_millis()).max(0) / 1000;
                                crate::session_commands::goal_notice(&format!("Goal still active: transient connection failure. Retrying in {seconds}s ({}/3). Send a message to retry now, or /goal clear to stop.", retry.retry_attempts));
                                match crate::session_commands::wait_goal_retry(self, &goal.id).await {
                                    Ok(true) => {
                                        next = format!("Resume unfinished work toward the explicitly authorized goal: {}. Review existing progress first and do not repeat completed operations. Stay within existing permissions.", goal.condition);
                                        continue;
                                    },
                                    Ok(false) => break,
                                    Err(cancelled) => {
                                        deepseek_code_core::services::session_goals::pause(&self.store.db, &self.id, &goal.id, &cancelled.to_string())?;
                                        return Err(cancelled);
                                    }
                                }
                            }
                        }
                        if deepseek_code_core::services::session_goals::pause(&self.store.db, &self.id, &goal.id, &error.to_string())? {
                            crate::session_commands::goal_notice("Goal paused. Send a message to continue, or /goal clear to remove it.");
                        }
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub async fn wake_hooks(&self, cli: &Cli, input: &mut Option<TerminalInput>) -> Result<()> {
        Box::pin(self.run_turn_origin(cli, "", input, true, false, true)).await
    }

    async fn run_turn(
        &self,
        cli: &Cli,
        prompt: &str,
        input: &mut Option<TerminalInput>,
    ) -> Result<()> {
        match self.run_turn_origin(cli, prompt, input, false, false, true).await {
            Err(error) if crate::session_commands::is_context_overflow(&error) => {
                if !crate::session_commands::reactive_compact(self, prompt).await? {
                    return Err(error);
                }
                self.run_turn_origin(cli, prompt, input, false, false, true).await
            }
            result => result,
        }
    }

    pub(crate) async fn turn_initial_hook(
        &self,
        cli: &Cli,
        prompt: &str,
        input: &mut Option<TerminalInput>,
        publish: bool,
    ) -> Result<()> {
        self.apply_settings_permissions(cli)?;
        self.run_turn_origin(cli, prompt, input, false, true, publish)
            .await
    }

    async fn run_turn_origin(
        &self,
        cli: &Cli,
        prompt: &str,
        input: &mut Option<TerminalInput>,
        hook_wake: bool,
        initial_hook: bool,
        publish: bool,
    ) -> Result<()> {
        deepseek_code_core::services::lifecycle_events::check_configuration(
            &self.hook_service_context(),
            &self.workspace,
            &self.id,
        )
        .await?;
        let started = std::time::Instant::now();
        let turn_goal = deepseek_code_core::services::session_goals::active(&self.store.db, &self.id)?;
        if let Err(error) = crate::ide_commands::restore(self).await {
            eprintln!("IDE connection unavailable: {error:#}");
            self.ide.disconnect().await;
        }
        self.ensure_mcp().await?;
        self.reload_plugins().await?;
        if cli.output_format == OutputFormat::StreamJson {
            let init = self.sdk_init_message().await?;
            crate::sdk_messages::emit_init_once(|| init)?;
        }
        crate::session_commands::prepare_turn(self, self.context_window().await).await?;
        self.permissions
            .set_session_rules(&self.id, crate::permission_commands::load(self)?);
        let mut history = self.store.model_history(&self.id, &self.api_format)?;
        crate::session_commands::microcompact(self, &mut history, self.context_window().await)?;
        if !hook_wake {
            history.push(json!({"role":"user","content":self.sdk_content.as_ref().cloned().unwrap_or(json!(prompt))}));
        }
        let mut system = match &self.sdk_overrides.system_prompt {
            Some(prompt) => prompt.clone(),
            None => cli.system_prompt_text()?,
        };
        let response_schema = match &self.sdk_overrides.response_schema {
            Some(schema) => Some(schema.clone()),
            None => cli.response_schema()?,
        };
        let structured_output = response_schema.is_some();
        let hook_context = deepseek_code_core::services::session_hooks::context(&self.hook_service_context(), &self.id)?;
        if !hook_context.is_empty() { system.push_str("\nSessionStart hook context:\n"); system.push_str(&hook_context); system.push('\n'); }
        system.push_str(&crate::display_commands::style_prompt(self)?);
        system.push_str(&crate::ide_commands::selection_prompt(self).await?);
        if let Some(goal) = crate::session_commands::goal_prompt(self)? {
            system.push_str(&goal);
        }
        system.push_str("\nThe user is working in the standalone DeepSeek Code terminal frontend. No Desktop window or Bridge server is running. Use the available native tools directly.\n");
        let cwd = &self.workspace;
        if self.sdk_content.is_some() {
            anyhow::ensure!(
                self.selected_provider().await?.model.supports_vision,
                "Selected model does not accept Image input"
            );
        }
        let mut checkpoint = if hook_wake {
            None
        } else {
            let user_message_id = if initial_hook {
                self.store.append_hook_user(&self.id, prompt)
            } else {
                match &self.sdk_content {
                    Some(content) => self.store.append_user_content(&self.id, prompt, content),
                    None => self.store.append(&self.id, "user", prompt),
                }
            }
            .context("Persisting CLI user message")?;
            match deepseek_code_core::services::checkpoints::capture(
                &self.paths.data_dir,
                &self.id,
                &user_message_id,
                cwd,
            ) {
                Ok(capture) => Some(capture),
                Err(error) => {
                    // Outside Git there are no checkpoints by design; say nothing.
                    let not_git = format!("{error:#}").contains("requires a Git worktree");
                    if input.is_some() && !not_git {
                        eprintln!("Code checkpoint unavailable: {error:#}");
                    }
                    None
                }
            }
        };
        let interactive_display = input.is_some()
            && cli.output_format == OutputFormat::Text
            && !cli.print
            && !cli.print_short
            && !crate::display_commands::load(self)?.focus;
        self.engine.set_auto_manual_interactive(input.is_some());
        self.engine.set_permission_host_available(input.is_some() || self.sdk_permissions.is_some() || std::env::var_os(deepseek_code_core::services::background_sessions::CONTROL_ENV).is_some());
        // A host flag and a live, session-owned frontend lease are both required.
        // SDK/Desktop engines never enable this local terminal tool.
        self.engine.set_local_goal_frontend(
            deepseek_code_core::services::background_sessions::worker_directory(
                &self.paths.data_dir,
                &self.id,
            )?
            .is_some(),
        );
        let chat_request = ChatRequest {
            conversation_id: self.id.clone(),
            messages: history,
            model: self.model.clone(),
            provider_id: if self.auto_route {
                None
            } else {
                Some(self.provider_id.clone())
            },
            effort: crate::session_commands::effective_effort(self, cli.effort.as_deref())?,
            system_prompt: Some(system),
            max_tokens: cli.max_tokens,
            max_turns: cli.max_turns.map(|limit| {
                turn_goal.as_ref().map_or(limit, |g| {
                    limit
                        .saturating_sub(
                            g.model_requests
                                .saturating_sub(g.run_start_requests)
                                .min(u64::from(u32::MAX)) as u32,
                        )
                        .max(1)
                })
            }),
            response_schema,
            workspace_path: Some(cwd.to_string_lossy().into_owned()),
            temperature: None,
            top_p: None,
            reasoning_mode: None,
        };
        let request = Box::pin(async {
            if hook_wake {
                self.engine.send_hook_wake(chat_request, false).await
            } else {
                self.engine
                    .send_message_with_history(chat_request, false)
                    .await
            }
        });
        let worker_cancel = deepseek_code_core::services::background_sessions::wait_for_cancellation();
        tokio::pin!(worker_cancel);
        let mut events = tokio::select! {
            _ = &mut worker_cancel => bail!("Background task cancelled before model request"),
            result = crate::hook_progress::during(&self.id, interactive_display, request) => result?,
            _ = tokio::signal::ctrl_c() => bail!("Interrupted before model request"),
        };
        let mut progress = crate::terminal::live::TurnDisplay::new(interactive_display, self.permission_mode(), &self.paths.data_dir.join("keybindings.json"), self.store.history(&self.id)?.iter().filter(|row|row["role"]=="user").filter_map(|row|row["content"].as_str().map(str::to_owned)).filter(|text| !crate::background_commands::is_notification(text)).collect())?.with_workspace(&self.workspace);
        if progress.is_live() {
            progress.set_model_label(&crate::session_commands::model_label(self).await);
            progress.set_pinned_prompt(prompt);
            progress.set_transcript(crate::repl::transcript(self).unwrap_or_default());
            progress.set_todos(crate::background_commands::todo_items(&self.paths, &self.id));
        }
        let mut hook_progress = deepseek_code_core::services::hook_progress::subscribe(&self.id);
        progress.hook_snapshot(hook_progress.snapshot());
        let mut side_request: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>> = None;
        let mut text = String::new();
        // Set when a tool runs, so text from the next model round starts on its own paragraph.
        let mut separate_text = false;
        let mut thinking_text = String::new();
        let mut usage = json!({});
        let mut error = None;
        let mut completed = false;
        let mut stop = None::<String>;
        let mut snapshots = Vec::<Value>::new();
        let mut goal_requests = crate::session_commands::GoalRequests::default();
        let mut observed_plan_boundary = deepseek_code_core::services::plan_approval::latest_boundary(&self.store.db, &self.id)?;
        let mut tool_started = std::collections::HashMap::<String, std::time::Instant>::new();
        let mut tool_durations = std::collections::HashMap::<String, u64>::new();
        let mut tools = Vec::<message_repo::ToolCallRow>::new();
        // Notes added to approvals reach the model after the result and are
        // saved apart from it, as the user's words.
        let mut approval_notes = std::collections::HashMap::<String, Vec<String>>::new();
        // Messages typed during this turn that the model has not read yet.
        let mut pending_inputs = Vec::<String>::new();
        // The mode chosen with a plan, applied once the engine has approved it.
        let mut plan_mode_after: Option<(String, &'static str)> = None;
        let mut sdk_messages = (cli.output_format == OutputFormat::StreamJson)
            .then(|| crate::sdk_messages::Translator::new(&self.id, cli.include_partial_messages));
        // num_turns and permission_denials for the json result, as stream-json reports them.
        let (mut model_rounds, mut round_open, mut denials) = (0u32, false, Vec::<Value>::new());
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(1));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut remote_interrupts = self.remote_interrupts();
        let turn_result: Result<()> = async {
            loop {
                let event = tokio::select! {
                    hooks = hook_progress.changed() => { progress.hook_snapshot(hooks); continue; },
                    event = events.recv() => event.context("Engine stream closed before completion")?,
                    _ = &mut worker_cancel => bail!("Background task cancelled"),
                    _ = remote_interrupts.next() => bail!("Interrupted by Remote Control"),
                    _ = tokio::signal::ctrl_c() => bail!("Interrupted"),
                    _ = refresh.tick() => {
                        progress.tick();
                        if interactive_display {
                            for notice in crate::background_commands::collect_completed(self)? { progress.notice(&notice); }
                        }
                        continue;
                    },
                    action = progress.action() => {
                        match action.context("Live terminal input closed")? {
                            crate::terminal::live::Action::CyclePermission => {
                                match self.cycle_permission_mode().await {
                                    Ok(mode) => progress.set_permission_mode(mode),
                                    Err(error) => progress.notice(&format!("Permission mode: {error:#}")),
                                }
                            },
                            crate::terminal::live::Action::Cancel => bail!("Interrupted"),
                            crate::terminal::live::Action::GoalDecision { .. } => progress.notice("Goal proposals require the attached session frontend."),
                            crate::terminal::live::Action::Detach => progress.notice("This turn is not owned by a background worker."),
                            crate::terminal::live::Action::Queue(prompt) => {
                                if let Some(reader) = input.as_mut() { reader.queue_prompt(prompt); }
                            },
                            crate::terminal::live::Action::Submit(prompt) => {
                                if let Some(message) = crate::session_commands::live_goal_command(self, &prompt)? { progress.notice(&message); }
                                else if queues_mid_turn(&prompt) {
                                    // As in the reference CLI the model reads it after its next
                                    // tool results, or as the next prompt.
                                    crate::images::share(&self.id, &prompt);
                                    deepseek_code_core::services::input_queue::push(&self.store.db, &self.id, &prompt)?;
                                    pending_inputs.push(prompt);
                                    progress.queued(pending_inputs.clone());
                                }
                                else if let Some(reader)=input.as_mut() { reader.queue_prompt(prompt); }
                            },
                            crate::terminal::live::Action::BackgroundShell | crate::terminal::live::Action::SendNow => {
                                deepseek_code_core::tools::background_shell::request_background(&self.paths.data_dir, &self.id)?;
                            },
                            crate::terminal::live::Action::EditQueued => {
                                let taken = deepseek_code_core::services::input_queue::take(&self.store.db, &self.id)?;
                                pending_inputs.clear();
                                progress.queued(Vec::new());
                                if !taken.is_empty() { progress.set_draft(&taken.join("\n")); }
                            },
                            crate::terminal::live::Action::Draft(draft) => if let Some(reader)=input.as_ref() { reader.restore_draft(draft); },
                            crate::terminal::live::Action::SideQuestion(question) => {
                                if side_request.is_some() { progress.notice("A side question is already running; wait for its answer before sending another."); }
                                else { side_request=Some(Box::pin(async move { crate::background_commands::side_question(self,&question).await })); }
                            },
                        }
                        continue;
                    },
                    answer = async { match side_request.as_mut() {Some(request)=>request.await,None=>std::future::pending().await} } => {
                        side_request=None;
                        match answer { Ok(answer)=>progress.notice(&format!("Side answer:\n{answer}")),Err(error)=>progress.notice(&format!("Side question failed: {error:#}")) }
                        continue;
                    },
                };
                // Parallel execution reports both the ordinary completion and a
                // pipeline completion. Publish and retain each identical result once.
                if duplicate_tool_result(&event, &tools) {
                    continue;
                }
                if cli.output_format == OutputFormat::StreamJson {
                    if let Some(value) = event_json_with(&event, self.sdk_overrides.agent_progress_summaries) { emit_json(&value)?; }
                }
                if let Some(translator) = sdk_messages.as_mut() { translator.observe(&event)?; }
                match &event {
                    // A provider may announce one round twice; count it once.
                    EngineEvent::MessageStart { .. } if !round_open => { model_rounds += 1; round_open = true; }
                    EngineEvent::MessageStart { .. } => {}
                    EngineEvent::ToolUseDone { tool_use_id, tool_name, tool_input, output, is_error: true } if crate::sdk_messages::is_permission_denial(output) => {
                        round_open = false;
                        denials.push(json!({"tool_name":tool_name,"tool_use_id":tool_use_id,"tool_input":tool_input}));
                    }
                    EngineEvent::Text(_) | EngineEvent::ToolUseStart { .. } | EngineEvent::ToolUseDone { .. } | EngineEvent::MessageStop { .. } => round_open = false,
                    _ => {}
                }
                match event {
                    EngineEvent::Text(delta) => {
                        let delta = if std::mem::take(&mut separate_text) && !text.is_empty() && !text.ends_with('\n') { format!("\n\n{delta}") } else { delta };
                        text.push_str(&delta); if publish && cli.output_format == OutputFormat::Text { progress.before_text(&delta); if !progress.is_live() { print!("{delta}"); io::stdout().flush()?; } } },
                    EngineEvent::Thinking(delta) => {
                        thinking_text.push_str(&delta);
                        progress.record_thinking(&delta);
                        if cli.verbose { progress.before_text(&delta); if !progress.is_live() { eprint!("{delta}"); } }
                        else { progress.thinking(); }
                    },
                    EngineEvent::MessageStart { .. } => progress.set_activity("Composing"),
                    EngineEvent::ContextReset { session_id, boundary_id } if session_id == self.id => {
                        deepseek_code_core::services::plan_approval::archive_segment(&self.store.db, &self.id, &boundary_id, None, &text, &thinking_text, &tools, &snapshots)?;
                        text.clear(); thinking_text.clear(); tools.clear(); snapshots.clear();
                        observed_plan_boundary = Some(boundary_id);
                        progress.notice("Planning context cleared. The approved plan starts a fresh context in this session; prior messages remain archived.");
                    },
                    EngineEvent::AgentProgress { session_id, agent_type, status, tool_name } => progress.agent_progress(&session_id, &agent_type, &status, tool_name.as_deref()),
                    EngineEvent::Usage(value) => { progress.reported_usage(&value); if let Some(fields) = value.as_object() { for (key, value) in fields { usage[key] = value.clone(); } } },
                    EngineEvent::Error(message) => { progress.clear(); error = Some(message); },
                    EngineEvent::MessageStop { full_text, stop_reason } => {
                        stop = stop_reason.clone();
                        if structured_output { text = full_text.clone(); }
                        if text.is_empty() && !full_text.is_empty() { if publish && cli.output_format == OutputFormat::Text { progress.before_text(&full_text); if !progress.is_live() { print!("{full_text}"); } } text = full_text; }
                        if stop_reason.as_deref().is_some_and(|s| ["error", "panic", "stream_error", "interrupted", "failed", "cancelled", "max_iterations", "budget_warning", "max_tokens", "length", "incomplete"].contains(&s)) {
                            error.get_or_insert_with(|| format!("Turn stopped: {}", stop_reason.unwrap()));
                        }
                        completed = true; break;
                    },
                    EngineEvent::ToolPermission { request_id, tool_use_id, tool_name, input: tool_input } => {
                        progress.clear();
                        let auto = tool_name != "ExitPlanMode" && self.auto_mode_available().await;
                        let approval = (tool_name != "ExitPlanMode").then(|| crate::terminal::tool_view::approval(&tool_name, &tool_input, Some(&self.workspace), auto));
                        let question = match &approval {
                            Some(approval) => approval.text(),
                            None => format!("Review plan\n{}\n\n{}\n\nApprove this plan and leave plan mode? Implementation will use ask permissions. [y/N]",
                                tool_input["planFilePath"].as_str().unwrap_or(""), tool_input["plan"].as_str().unwrap_or("")),
                        };
                        let mut completion = self.engine.permission_completion(&request_id).await;
                        if *completion.borrow() { continue; }
                        if let Some(sdk) = &self.sdk_permissions {
                            let decision = tokio::select! {
                                biased;
                                _ = completion.changed() => None,
                                decision = sdk.ask(&request_id, &tool_use_id, &tool_name, &tool_input) => Some(decision?),
                            };
                            if let Some(decision) = decision {
                                let _ = self.engine.resume_with_permission(&request_id, decision).await;
                            } else { sdk.cancel_one(&request_id).await; }
                            progress.set_activity("Working");
                            continue;
                        }
                        // The remote client may answer first; a local answer cancels its prompt.
                        let mut remote = self.remote_relay().map(|relay| relay.request_permission(&request_id, &tool_name, &tool_input, &tool_use_id, &question));
                        let (answer, remote_decided) = {
                            let tool = json!({"tool_name": tool_name, "input": tool_input, "tool_use_id": tool_use_id});
                            let answering = async {
                                if let Some(answer) = deepseek_code_core::services::background_sessions::worker_permission(&self.paths.data_dir, &self.id, &request_id, &question, tool).await? { Ok(answer) } else {
                                    anyhow::ensure!(input.is_some(), "Tool approval required in non-interactive mode; choose --permission-mode explicitly");
                                    match (&approval, progress.live.as_ref()) {
                                        (Some(approval), Some(live)) => {
                                            let reply = live.choice(approval.lines.clone(), approval.question.clone(), approval.options.clone());
                                            wait_answer(self, &mut progress, input, reply).await
                                        }
                                        _ => turn_answer(self, &mut progress, input, question).await,
                                    }
                                }
                            };
                            tokio::select! {
                                biased;
                                _ = completion.changed() => (None, false),
                                allowed = deepseek_code_core::services::remote_control::remote_decision(&mut remote) => (Some(if allowed { "y".to_owned() } else { "n".to_owned() }), true),
                                answer = answering => (Some(answer?), false),
                            }
                        };
                        if let Some(answer) = answer {
                            use deepseek_code_core::native_engine::permission_response::PermissionResponse;
                            use crate::terminal::tool_view::{Decision, Remember};
                            let decision = match &approval {
                                Some(approval) => approval.decide(&answer),
                                None => Decision { allowed: ["y", "yes", "1", "\u{662f}", "\u{5141}\u{8bb8}"].contains(&answer.trim().to_lowercase().as_str()), grant: None, note: None },
                            };
                            let commit = match decision.grant.as_ref() {
                                Some(grant) => match crate::permission_commands::remember_commit(self, grant) {
                                    Ok(commit) => Some((commit, grant.clone())),
                                    Err(error) => { progress.notice(&format!("Approved once; the session grant was not saved: {error:#}")); None },
                                },
                                None => None,
                            };
                            let answer = if decision.allowed { "yes" } else { "no" };
                            if decision.allowed && (commit.is_some() || decision.note.is_some()) {
                                let grant = commit.as_ref().map(|(_, grant)| grant.clone());
                                if let Some(note) = &decision.note {
                                    approval_notes.entry(tool_use_id.clone()).or_default().push(note.clone());
                                }
                                let response = PermissionResponse::Allow { updated_input: None, commit: commit.map(|(commit, _)| commit), note: decision.note.clone() };
                                let _ = self.engine.resume_with_permission(&request_id, response).await;
                                match grant {
                                    Some(Remember::AcceptEdits) => progress.set_permission_mode("accept-edits"),
                                    Some(Remember::Auto) => progress.set_permission_mode("auto"),
                                    Some(Remember::Rule(rule)) => {
                                        if let Err(error) = crate::permission_commands::save_local(self, &rule) {
                                            progress.notice(&format!("Allowed for this session; saving the rule to .claude/settings.local.json failed: {error:#}"));
                                        }
                                    }
                                    None => {}
                                }
                            } else if !decision.allowed && !remote_decided && approval.is_some() {
                                // As in the reference CLI: a bare No stops the turn; No with
                                // a note tells the model how to proceed instead.
                                let (message, interrupt) = decision.rejection();
                                let _ = self.engine.resume_with_permission(&request_id, PermissionResponse::Deny { message, interrupt }).await;
                            } else {
                                let _ = self.engine.resume_with_answer(&request_id, answer.into()).await;
                            }
                            if remote_decided {
                                if let Some(reader) = input.as_mut() { reader.dismiss_answer().await?; }
                                progress.notice(&format!("Remote Control {} {tool_name}.", if answer == "yes" { "allowed" } else { "denied" }));
                            }
                        } else if let Some(reader) = input.as_mut() {
                            reader.dismiss_answer().await?;
                        }
                        progress.set_activity("Working");
                    },
                    EngineEvent::AskUser { request_id, tool_use_id, tool_input, question, options } => {
                        progress.clear();
                        if tool_input["subtype"] == "elicitation" {
                            let answer = if let Some(runtime) = &self.sdk_hooks {
                                tokio::select! {
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(295)) => json!({"action":"cancel"}),
                                    result = runtime.broker.request(tool_input.clone()) => result.unwrap_or_else(|_| json!({"action":"cancel"})),
                                }
                            } else {
                                let answer = if let Some(answer) = deepseek_code_core::services::background_sessions::worker_answer(&self.paths.data_dir, &self.id, &request_id, "question", &question).await? {
                                    answer
                                } else if input.is_some() {
                                    tokio::select! {
                                        _ = tokio::time::sleep(std::time::Duration::from_secs(295)) => "cancel".to_owned(),
                                        answer = turn_answer(self, &mut progress, input, question) => answer.unwrap_or_else(|_| "cancel".into()),
                                    }
                                } else { "cancel".into() };
                                deepseek_code_core::mcp::elicitation::user_response(&answer).unwrap_or_else(|error| {
                                    progress.notice(&format!("Invalid elicitation response; cancelled: {error}"));
                                    json!({"action":"cancel"})
                                })
                            };
                            self.engine.resume_with_answer(&request_id, answer.to_string()).await?;
                            progress.set_activity("Composing");
                            continue;
                        }
                        if let Some(sdk)=&self.sdk_permissions {
                            let answer=sdk.ask_question(&request_id,&tool_use_id,&question,&tool_input).await?;
                            self.engine.resume_with_answer(&request_id,answer).await?;
                            progress.set_activity("Composing");
                            continue;
                        }
                        if tool_input["planApproval"] == true {
                            // As in the reference CLI: a review menu, from this terminal or
                            // an attached frontend; typed answers keep the 1–4 choices.
                            let plan = tool_input["plan"].as_str().unwrap_or_default().to_owned();
                            let path = tool_input["planFilePath"].as_str().unwrap_or_default().to_owned();
                            let auto = self.auto_mode_available().await;
                            let menu = match deepseek_code_core::services::background_sessions::worker_plan(&self.paths.data_dir, &self.id, &request_id, &question, tool_input.clone()).await? {
                                Some(answer) => answer.strip_prefix("plan:").map(str::to_owned).ok_or(answer),
                                None => match progress.live.as_ref() {
                                    Some(live) if input.is_some() => {
                                        let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols.saturating_sub(1) as usize);
                                        let view = crate::terminal::plan_view::review(&plan, &path, auto, width);
                                        let reply = live.plan_choice(view.lines, view.question, view.options, view.footer);
                                        let answer = wait_answer(self, &mut progress, input, reply).await?;
                                        if answer == "edit" {
                                            crate::display_commands::edit_file(std::path::Path::new(&path), "").await?;
                                        }
                                        Ok(answer)
                                    }
                                    _ => Err(String::new()),
                                },
                            };
                            match menu {
                                Ok(answer) => {
                                    let decision = crate::terminal::plan_view::decide(&answer, auto);
                                    if decision.rejected && progress.is_live() {
                                        let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols.saturating_sub(1) as usize);
                                        progress.styled_lines(crate::terminal::plan_view::rejected(&plan, width));
                                    }
                                    if let Some(note) = &decision.note {
                                        deepseek_code_core::services::input_queue::push(&self.store.db, &self.id, note)?;
                                    }
                                    self.engine.resume_with_answer(&request_id, decision.engine).await?;
                                    plan_mode_after = decision.mode.map(|mode| (tool_use_id.clone(), mode));
                                    if decision.stop {
                                        bail!("Tool permission interrupted the turn: the user rejected the plan");
                                    }
                                    progress.set_activity("Working");
                                    continue;
                                }
                                // A frontend without the menu answered as text.
                                Err(answer) if !answer.is_empty() => {
                                    self.engine.resume_with_answer(&request_id, answer).await?;
                                    progress.set_activity("Composing");
                                    continue;
                                }
                                Err(_) => {}
                            }
                        }
                        let prompt = question_prompt(&question, &options, &tool_input);
                        // A worker's frontend gets the options too, to show the menu.
                        let answer = if let Some(answer) = deepseek_code_core::services::background_sessions::worker_question(&self.paths.data_dir, &self.id, &request_id, &prompt, tool_input.clone()).await? {
                            answer
                        } else if let (Some(menu), Some(live), true) = (crate::terminal::question_menu::menu(&question, &tool_input), progress.live.as_ref(), input.is_some()) {
                            let reply = live.question_choice(menu.question.clone(), menu.options.clone(), menu.previews.clone(), menu.multi);
                            menu.answer(&wait_answer(self, &mut progress, input, reply).await?)
                        } else {
                            turn_answer(self, &mut progress, input, prompt).await?
                        };
                        let answer = chosen_options(&answer, &options, tool_input["multiSelect"] == true);
                        self.engine.resume_with_answer(&request_id, answer).await?;
                        progress.set_activity("Composing");
                    },
                    EngineEvent::QueuedInput(text) => {
                        // Saved after the latest result, where the model read it.
                        if let Some(tool) = tools.last() {
                            approval_notes.entry(tool.id.clone()).or_default().push(text.clone());
                        }
                        if let Some(index) = pending_inputs.iter().position(|pending| *pending == text) { pending_inputs.remove(index); }
                        progress.queued_delivered(&text);
                    },
                    EngineEvent::ToolUseStart { tool_use_id, tool_name, tool_input, .. } => {
                        tool_started.entry(tool_use_id.clone()).or_insert_with(std::time::Instant::now);
                        separate_text = true;
                        progress.clear();
                        if cli.verbose && !progress.is_live() { eprintln!("\n[{tool_name}]"); }
                        progress.tool_started(&tool_use_id, &tool_name, &tool_input);
                    },
                    EngineEvent::ToolUseDone { tool_use_id, tool_name, tool_input, output, is_error }
                    | EngineEvent::PipelineToolResult { tool_use_id, tool_name, tool_input, output, is_error, .. } => {
                        // The baseline is captured before the turn. Attribute changed paths to
                        // the actual post-hook input, including tools that partially fail.
                        // Dedicated session plans are runtime data, outside code rewind.
                        let plan_document = deepseek_code_core::services::plans::is_document_tool(
                            &self.paths.data_dir, &self.id, &tool_name, &tool_input,
                        );
                        if let Some(capture) = checkpoint.as_mut().filter(|_| !plan_document) {
                            if let Err(error) = capture.record_tool(&tool_name, &tool_input) {
                                progress.notice(&format!("Cannot checkpoint this tool's file: {error:#}"));
                            }
                        }
                        if matches!(tool_name.as_str(), "EnterPlanMode" | "ExitPlanMode" | "plan_mode") {
                            progress.set_permission_mode(self.permission_mode());
                        }
                        if let Some(start) = tool_started.remove(&tool_use_id) { tool_durations.entry(tool_use_id.clone()).or_insert(start.elapsed().as_millis().min(u64::MAX as u128) as u64); }
                        progress.tool_finished(&tool_use_id, &tool_name, &tool_input, &output, is_error);
                        if let Some((plan_tool, mode)) = plan_mode_after.take() {
                            if plan_tool == tool_use_id && !is_error {
                                match self.set_permission_mode(mode).await {
                                    Ok(()) => progress.set_permission_mode(mode),
                                    Err(error) => progress.notice(&format!("Plan approved; {mode} mode unavailable: {error:#}")),
                                }
                            } else if plan_tool != tool_use_id {
                                plan_mode_after = Some((plan_tool, mode));
                            }
                        }
                        let position = tools.iter().position(|tool| tool.id == tool_use_id);
                        let row = message_repo::ToolCallRow { id: tool_use_id, message_id: String::new(), name: tool_name, input: Some(tool_input.to_string()), output: Some(output), is_error, sort_order: position.unwrap_or(tools.len()) as i64 };
                        if let Some(position) = position { tools[position] = row; } else { tools.push(row); }
                    },
                    EngineEvent::RequestSnapshot(snapshot) => {
                        goal_requests.observe(&snapshot);
                        if let Some(old) = snapshots.iter_mut().find(|s| s["id"] == snapshot["id"]) { *old = snapshot; } else { snapshots.push(snapshot); }
                    },
                    EngineEvent::BudgetWarning { message, .. } => { progress.notice(&message); },
                    _ => {},
                }
            }
            Ok(())
        }.await;
        progress.clear();
        if let Some(translator) = sdk_messages.as_mut() { translator.finish()?; }
        // Join before persisting; only interruption cancels background hooks.
        if completed && turn_result.is_ok() && error.is_none() {
            self.engine.finish_turn(&self.id).await;
        } else {
            self.engine.cancel_turn(&self.id).await;
        }
        // Cancellation may happen after the atomic boundary commit but before its event.
        let latest_boundary =
            deepseek_code_core::services::plan_approval::latest_boundary(&self.store.db, &self.id)?;
        if latest_boundary != observed_plan_boundary {
            if let Some(boundary) = latest_boundary {
                deepseek_code_core::services::plan_approval::archive_segment(&self.store.db, &self.id, &boundary, None, &text, &thinking_text, &tools, &snapshots)?;
                text.clear(); thinking_text.clear(); tools.clear(); snapshots.clear();
            }
        }
        if deepseek_code_core::services::background_sessions::worker_directory(&self.paths.data_dir, &self.id)?.is_none() {
            if let Err(error) = self.set_permission_mode(self.permission_mode()).await {
                progress.notice(&format!("Could not save permission mode: {error:#}"));
            }
        }
        // Only a frontend with an input reader takes unread messages; a
        // background worker leaves them for its frontend.
        if let Some(reader) = input.as_mut() {
            if let Err(error) = requeue_unread(self, reader) {
                progress.notice(&format!("Could not restore queued messages: {error:#}"));
            }
        }
        if let Some(capture) = checkpoint {
            if let Err(error) = deepseek_code_core::services::checkpoints::finish(capture) {
                progress.notice(&format!("Could not save code checkpoint: {error:#}"));
            }
        }
        if let Err(failure) = turn_result {
            error = Some(failure.to_string());
        }
        if !completed && error.is_none() {
            error = Some("Incomplete response".into());
        }
        if !text.is_empty() || !thinking_text.is_empty() || !tools.is_empty() || !snapshots.is_empty() {
            let message_id = self.store.append(&self.id, "assistant", &text)?;
            self.store.db.with_conn(|conn| -> Result<()> {
                if !thinking_text.is_empty() { conn.execute("UPDATE messages SET thinking = ?1 WHERE id = ?2", [&thinking_text, &message_id])?; }
                for tool in &tools {
                    let call_id = format!("{message_id}:{}", tool.id);
                    message_repo::insert_tool_call(
                        conn,
                        &call_id,
                        &message_id,
                        &tool.name,
                        tool.input.as_deref(),
                        tool.output.as_deref(),
                        tool.is_error,
                        tool.sort_order,
                    )?;
                    if let Some(duration) = tool_durations.get(&tool.id) { message_repo::set_tool_duration(conn, &call_id, *duration)?; }
                    if let Some(notes) = approval_notes.get(&tool.id) {
                        message_repo::set_tool_call_note(conn, &call_id, &notes.join("\n\n"))?;
                    }
                }
                message_repo::save_request_snapshots(conn, &message_id, &json!(snapshots))?;
                Ok(())
            })??;
        }
        if let Some(goal) = turn_goal {
            let (tokens, requests, unreported) = goal_requests.usage();
            deepseek_code_core::services::session_goals::record_turn(
                &self.store.db,
                &self.id,
                &goal.id,
                tokens,
                requests,
                unreported,
            )?;
        }
        if publish || error.is_some() {
            match cli.output_format {
                OutputFormat::Text => {
                    if !progress.is_live() {
                        println!();
                        io::stdout().flush()?;
                    }
                }
                _ => {
                    let cost = deepseek_code_core::services::usage::session_cost(&self.store.db, &self.id, &self.paths.read_config()?)
                        .map(|cost| cost["known_subtotal_usd"].as_f64().unwrap_or(0.0)).unwrap_or(0.0);
                    let elapsed = started.elapsed().as_millis() as u64;
                    emit_json(
                        &json!({"type":"result","subtype":crate::sdk_messages::result_subtype(error.as_deref()),"session_id":self.id,"result":text,"is_error":error.is_some(),"error":error,"usage":usage,
                            "duration_ms":elapsed,"duration_api_ms":elapsed,"num_turns":sdk_messages.as_ref().map_or(model_rounds, |t| t.turns),"total_cost_usd":cost,
                            "stop_reason":stop,"permission_denials":sdk_messages.as_ref().map_or_else(|| denials.clone(), |t| t.denials.clone()),"uuid":uuid::Uuid::new_v4().to_string(),
                            "structured_output":if error.is_none() && structured_output {serde_json::from_str::<Value>(&text).ok()} else {None}}),
                    )?
                },
            }
        }
        if let Some(request) = side_request.take() {
            if error.is_none() {
                progress.set_activity("Main task complete \u{b7} finishing side question");
                tokio::pin!(request);
                loop {
                    tokio::select! {
                        answer = &mut request => {
                            match answer { Ok(answer)=>progress.notice(&format!("Side answer:\n{answer}")),Err(error)=>progress.notice(&format!("Side question failed: {error:#}")) }
                            break;
                        },
                        action = progress.action() => match action {
                            Some(crate::terminal::live::Action::CyclePermission) => {
                                match self.cycle_permission_mode().await { Ok(mode)=>progress.set_permission_mode(mode),Err(error)=>progress.notice(&format!("Permission mode: {error:#}")) }
                            },
                            Some(crate::terminal::live::Action::Cancel)|None => { progress.notice("Side question cancelled."); break; },
                            Some(crate::terminal::live::Action::Submit(prompt) | crate::terminal::live::Action::Queue(prompt)) => if let Some(reader)=input.as_mut() { reader.queue_prompt(prompt); },
                            Some(_) => progress.notice("Finishing the current side question; another can be sent from the next prompt."),
                        },
                        _ = tokio::signal::ctrl_c() => break,
                    }
                }
            }
        }
        if progress.is_live() {
            progress.set_running_shells(crate::background_commands::running_shells(&self.paths, &self.id));
            progress.set_running_agents(crate::background_commands::running_agents(&self.paths, &self.store.db, &self.id).len());
        }
        let save_completion = progress.is_live();
        for action in progress.finish(error.as_deref()) {
            if let Some(reader)=input.as_mut() {
                match action {
                    crate::terminal::live::Action::Submit(prompt)|crate::terminal::live::Action::Queue(prompt)=>reader.queue_prompt(prompt),
                    crate::terminal::live::Action::SideQuestion(question)=>reader.queue_prompt(format!("/btw {question}")),
                    crate::terminal::live::Action::Draft(draft)=>reader.restore_draft(draft),
                    crate::terminal::live::Action::CyclePermission => { self.cycle_permission_mode().await?; },
                    // Unread messages were already queued as the next prompts.
                    crate::terminal::live::Action::Cancel|crate::terminal::live::Action::Detach|crate::terminal::live::Action::GoalDecision { .. }|crate::terminal::live::Action::EditQueued|crate::terminal::live::Action::BackgroundShell|crate::terminal::live::Action::SendNow=>{},
                }
            }
        }
        if save_completion {
            if let Err(error) = crate::repl::save_completion(self) {
                eprintln!("Could not save turn completion status: {error}");
            }
        }
        if let Some(error) = error {
            if cli.output_format != OutputFormat::Text {
                return Err(crate::output::ReportedError(format!("{error} (session {})", self.id)).into());
            }
            bail!("{error} (session {})", self.id);
        }
        Ok(())
    }
}

/// An AskUserQuestion question as terminal text: the header, then numbered
/// options with their descriptions and previews shown in full, since the
/// terminal prompt has no per-option focus to reveal them on.
fn question_prompt(question: &str, options: &[String], input: &Value) -> String {
    let mut prompt = match input["header"].as_str().filter(|header| !header.trim().is_empty()) {
        Some(header) => format!("[{header}] {question}"),
        None => question.to_owned(),
    };
    let details = input["options"].as_array();
    for (index, option) in options.iter().enumerate() {
        prompt.push_str(&format!("\n{}. {option}", index + 1));
        let detail = details.and_then(|details| details.get(index));
        if let Some(description) = detail.and_then(|d| d["description"].as_str()).filter(|d| !d.trim().is_empty()) {
            prompt.push_str(&format!(" \u{2014} {description}"));
        }
        if let Some(preview) = detail.and_then(|d| d["preview"].as_str()).filter(|p| !p.trim().is_empty()) {
            for line in preview.lines() { prompt.push_str(&format!("\n     \u{2502} {line}")); }
        }
    }
    if input["multiSelect"] == true {
        prompt.push_str("\n(Choose one or more: numbers separated by commas, or type your own answer.)");
    }
    prompt
}

/// Numbers become option labels; several are allowed for multiSelect.
/// Anything else is the user's own answer.
fn chosen_options(answer: &str, options: &[String], multi: bool) -> String {
    let picks: Option<Vec<&String>> = answer.split(',').map(|part| part.trim().parse::<usize>().ok().and_then(|i| i.checked_sub(1)).and_then(|i| options.get(i))).collect();
    match picks {
        Some(picks) if !picks.is_empty() && (multi || picks.len() == 1) => picks.iter().map(|pick| pick.as_str()).collect::<Vec<_>>().join(", "),
        _ => answer.to_owned(),
    }
}

async fn turn_answer(session: &Session, progress: &mut crate::terminal::live::TurnDisplay, input: &mut Option<TerminalInput>, question: String) -> Result<String> {
    if let Some(live)=progress.live.as_ref() {
        let answer=live.question(question);
        return wait_answer(session, progress, input, answer).await;
    }
    progress.clear();
    let reader=input.as_mut().context("User input required in non-interactive mode; resume this session interactively")?;
    eprintln!("\n{question}");
    read_answer(reader).await
}

/// Waits for a live question's reply while handling mode cycling, queued
/// prompts and cancellation.
async fn wait_answer(session: &Session, progress: &mut crate::terminal::live::TurnDisplay, input: &mut Option<TerminalInput>, mut answer: tokio::sync::oneshot::Receiver<String>) -> Result<String> {
    {
        loop {
            tokio::select! {
                response=&mut answer=>return response.context("Terminal answer cancelled"),
                action=progress.action()=>match action {
                    Some(crate::terminal::live::Action::CyclePermission) => {
                        match session.cycle_permission_mode().await {
                            Ok(mode) => progress.set_permission_mode(mode),
                            Err(error) => progress.notice(&format!("Permission mode: {error:#}")),
                        }
                    },
                    Some(crate::terminal::live::Action::Cancel)|None=>bail!("Interrupted"),
                    Some(crate::terminal::live::Action::Detach)=>progress.notice("Answer the pending question before detaching."),
                    Some(crate::terminal::live::Action::GoalDecision { .. })=>progress.notice("Answer the pending question first."),
                    Some(crate::terminal::live::Action::EditQueued|crate::terminal::live::Action::BackgroundShell|crate::terminal::live::Action::SendNow)=>{},
                    Some(crate::terminal::live::Action::Submit(prompt) | crate::terminal::live::Action::Queue(prompt))=>if let Some(reader)=input.as_mut(){reader.queue_prompt(prompt);},
                    Some(crate::terminal::live::Action::SideQuestion(question))=>if let Some(reader)=input.as_mut(){reader.queue_prompt(format!("/btw {question}"));},
                    Some(crate::terminal::live::Action::Draft(draft))=>if let Some(reader)=input.as_ref(){reader.restore_draft(draft);},
                },
                _=tokio::signal::ctrl_c()=>bail!("Interrupted"),
            }
        }
    }
}

async fn read_answer(reader: &mut TerminalInput) -> Result<String> {
    tokio::select! { answer = reader.read_answer("") => answer?.context("Input closed"), _ = tokio::signal::ctrl_c() => bail!("Interrupted") }
}

pub(crate) fn emit_json(value: &Value) -> Result<()> {
    if deepseek_code_core::services::session_worker::emit_event(value)? { return Ok(()); }
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

/// Plain text typed during a turn goes to the model at its next tool
/// boundary; commands, shell and memory input wait for the turn to end.
pub(crate) fn queues_mid_turn(prompt: &str) -> bool {
    let prompt = prompt.trim_start();
    !prompt.is_empty() && !prompt.starts_with('/') && !prompt.starts_with('!') && !prompt.starts_with("# ")
}

/// Messages the model did not read during the turn become the next prompts.
pub(crate) fn requeue_unread(session: &Session, reader: &mut TerminalInput) -> Result<()> {
    // Task notifications stay queued for the next turn the prompt starts.
    for prompt in deepseek_code_core::services::input_queue::take_where(&session.store.db, &session.id, |text| !crate::background_commands::is_notification(text))? {
        reader.queue_prompt(prompt);
    }
    Ok(())
}

fn duplicate_tool_result(event: &EngineEvent, tools: &[message_repo::ToolCallRow]) -> bool {
    let (id, name, input, output, is_error) = match event {
        EngineEvent::ToolUseDone {
            tool_use_id,
            tool_name,
            tool_input,
            output,
            is_error,
        }
        | EngineEvent::PipelineToolResult {
            tool_use_id,
            tool_name,
            tool_input,
            output,
            is_error,
            ..
        } => (tool_use_id, tool_name, tool_input, output, is_error),
        _ => return false,
    };
    tools
        .iter()
        .find(|tool| tool.id == *id)
        .is_some_and(|tool| {
            tool.name == *name
                && tool.is_error == *is_error
                && tool.output.as_deref() == Some(output.as_str())
                && tool.input.as_deref() == Some(input.to_string().as_str())
        })
}

pub(crate) fn event_json(event: &EngineEvent) -> Option<Value> {
    event_json_with(event, false)
}

/// SDK `agentProgressSummaries` adds a concise human-readable summary.
pub(crate) fn event_json_with(event: &EngineEvent, summaries: bool) -> Option<Value> {
    Some(match event {
        EngineEvent::ContextReset { session_id, boundary_id } => json!({"type":"context_reset","session_id":session_id,"boundary_id":boundary_id}),
        EngineEvent::Text(delta) => json!({"type":"text","delta":delta}),
        EngineEvent::QueuedInput(text) => json!({"type":"queued_input","text":text}),
        EngineEvent::Thinking(delta) => json!({"type":"thinking","delta":delta}),
        EngineEvent::Usage(usage) => json!({"type":"usage","usage":usage}),
        EngineEvent::MessageStart { model } => json!({"type":"message_start","model":model}),
        EngineEvent::AgentProgress { session_id, agent_type, status, tool_name } => {
            let mut value = json!({"type":"agent_progress","session_id":session_id,"agent_type":agent_type,"status":status,"tool_name":tool_name});
            if summaries {
                value["summary"] = json!(match tool_name {
                    Some(tool) => format!("{agent_type}: {status} ({tool})"),
                    None => format!("{agent_type}: {status}"),
                });
            }
            value
        }
        EngineEvent::Error(error) => json!({"type":"error","error":error}),
        EngineEvent::ToolUseStart {
            tool_use_id,
            tool_name,
            tool_input,
            ..
        } => json!({"type":"tool_start","id":tool_use_id,"name":tool_name,"input":tool_input}),
        EngineEvent::ToolUseDone {
            tool_use_id,
            tool_name,
            output,
            is_error,
            ..
        }
        | EngineEvent::PipelineToolResult {
            tool_use_id,
            tool_name,
            output,
            is_error,
            ..
        } => {
            json!({"type":"tool_result","id":tool_use_id,"name":tool_name,"output":output,"is_error":is_error})
        }
        EngineEvent::AskUser {
            request_id,
            question,
            options,
            ..
        } => {
            json!({"type":"ask_user","request_id":request_id,"question":question,"options":options})
        }
        EngineEvent::ToolPermission {
            request_id,
            tool_name,
            input,
            ..
        } => json!({"type":"permission","request_id":request_id,"name":tool_name,"input":input}),
        _ => return None,
    })
}

fn next_mode(mode: &str, bypass_available: bool) -> &'static str {
    match mode {
        "ask" => "accept-edits",
        "accept-edits" => "plan",
        "plan" if bypass_available => "bypass",
        "plan" | "bypass" => "auto",
        _ => "ask",
    }
}

#[cfg(test)]
mod event_tests {
    #[test]
    fn questions_show_headers_descriptions_previews_and_take_several_numbers() {
        let options = vec!["Tabs".to_owned(), "Spaces".to_owned(), "Mixed".to_owned()];
        let input = serde_json::json!({"header":"Indent","multiSelect":true,"options":[{"label":"Tabs","description":"One tab per level","preview":"fn a() {\n\tb();\n}"},{"label":"Spaces","description":"Four spaces"},{"label":"Mixed"}]});
        let prompt = super::question_prompt("Which indentation?", &options, &input);
        assert!(prompt.starts_with("[Indent] Which indentation?\n1. Tabs \u{2014} One tab per level\n     \u{2502} fn a() {\n"));
        assert!(prompt.contains("2. Spaces \u{2014} Four spaces\n3. Mixed\n(Choose one or more"));
        assert_eq!(super::chosen_options("1, 3", &options, true), "Tabs, Mixed");
        assert_eq!(super::chosen_options("1, 3", &options, false), "1, 3");
        assert_eq!(super::chosen_options("2", &options, false), "Spaces");
        assert_eq!(super::chosen_options("my own", &options, true), "my own");
        assert_eq!(super::chosen_options("9", &options, false), "9");
    }

    #[test]
    fn only_plain_text_is_read_by_the_running_turn() {
        assert!(super::queues_mid_turn("Also end with BANANA"));
        for waits in ["/compact", "!ls", "# note", "   "] {
            assert!(!super::queues_mid_turn(waits), "{waits}");
        }
    }

    use super::*;

    #[test]
    fn shift_tab_cycles_like_the_reference_cli() {
        let cycle = |bypass| {
            let mut mode = "ask";
            (0..5).map(|_| { mode = super::next_mode(mode, bypass); mode }).collect::<Vec<_>>()
        };
        assert_eq!(cycle(false), ["accept-edits", "plan", "auto", "ask", "accept-edits"]);
        assert_eq!(cycle(true), ["accept-edits", "plan", "bypass", "auto", "ask"]);
    }

    #[test]
    fn launch_flags_choose_bypass_and_keep_it_in_the_cycle() {
        let parse = |args: &[&str]| { let mut cli = Cli::try_parse_with_mode_source(std::iter::once("dcl").chain(args.iter().copied())).unwrap(); cli.normalize(); cli };
        let full = parse(&["--full-access"]);
        assert!(matches!(full.permission_mode, crate::args::PermissionModeArg::Bypass) && full.allow_dangerously_skip_permissions);
        let skip = parse(&["--dangerously-skip-permissions"]);
        assert!(skip.allow_dangerously_skip_permissions && !skip.full_access);
        // A worker job keeps the mode in use alongside --full-access.
        let worker = parse(&["--permission-mode", "plan", "--full-access", "--allow-dangerously-skip-permissions"]);
        assert!(matches!(worker.permission_mode, crate::args::PermissionModeArg::Plan) && worker.full_access);
        assert!(!parse(&[]).allow_dangerously_skip_permissions);
    }
    #[test]
    fn duplicate_pipeline_completion_does_not_hide_distinct_results() {
        let saved = vec![message_repo::ToolCallRow {
            id: "read-1".into(),
            message_id: String::new(),
            name: "Read".into(),
            input: Some(json!({"file_path":"a.txt"}).to_string()),
            output: Some("contents".into()),
            is_error: false,
            sort_order: 0,
        }];
        let result =
            |id: &str, path: &str, output: &str, is_error| EngineEvent::PipelineToolResult {
                tool_use_id: id.into(),
                tool_name: "Read".into(),
                tool_input: json!({"file_path":path}),
                output: output.into(),
                is_error,
                completed_count: 1,
                total_count: 1,
            };
        assert!(duplicate_tool_result(
            &result("read-1", "a.txt", "contents", false),
            &saved
        ));
        assert!(!duplicate_tool_result(
            &result("read-2", "a.txt", "contents", false),
            &saved
        ));
        assert!(!duplicate_tool_result(
            &result("read-1", "b.txt", "contents", false),
            &saved
        ));
        assert!(!duplicate_tool_result(
            &result("read-1", "a.txt", "changed", false),
            &saved
        ));
        assert!(!duplicate_tool_result(
            &result("read-1", "a.txt", "contents", true),
            &saved
        ));
        assert!(!duplicate_tool_result(
            &EngineEvent::Text("contents".into()),
            &saved
        ));
    }

    #[tokio::test]
    async fn persisted_permission_mode_restores_unless_explicit_cli_overrides() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let selection = || Selection {
            providers: vec![serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","enabled":true,"models":[{"id":"fixture","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false}]})).unwrap()],
            provider_id:"fixture".into(), model:"fixture".into(),
        };
        let cli = Cli::try_parse_with_mode_source(["dcl"]).unwrap();
        let first = Session::open(&cli, &paths, selection(), None).await.unwrap();
        first.set_permission_mode("plan").await.unwrap();
        let saved = first.id.clone();
        first.close().await.unwrap();
        let resumed = Session::open(&cli, &paths, selection(), Some(&saved)).await.unwrap();
        assert_eq!(resumed.permission_mode(), "plan");
        resumed.set_permission_mode("dontAsk").await.unwrap();
        assert!(resumed.set_permission_mode("auto").await.is_err());
        assert_eq!(resumed.permission_mode(), "dont-ask");
        resumed.close().await.unwrap();
        let restored = Session::open(&cli, &paths, selection(), Some(&saved)).await.unwrap();
        assert_eq!(restored.permission_mode(), "dont-ask");
        restored.cycle_permission_mode().await.unwrap();
        assert_eq!(restored.permission_mode(), "ask");
        restored.close().await.unwrap();
        let explicit = Cli::try_parse_with_mode_source(["dcl", "--permission-mode", "ask"]).unwrap();
        let overridden = Session::open(&explicit, &paths, selection(), Some(&saved)).await.unwrap();
        assert_eq!(overridden.permission_mode(), "ask");
        // Bypass needs a launch that allows it.
        assert!(overridden.set_permission_mode("bypass").await.is_err());
        overridden.close().await.unwrap();
        let explicit = Cli::try_parse_with_mode_source(["dcl", "--permission-mode", "ask", "--allow-dangerously-skip-permissions"]).unwrap();
        let overridden = Session::open(&explicit, &paths, selection(), Some(&saved)).await.unwrap();
        overridden.set_permission_mode("bypass").await.unwrap();
        overridden.close().await.unwrap();
        let safe_resume = Session::open(&cli, &paths, selection(), Some(&saved)).await.unwrap();
        assert_eq!(safe_resume.permission_mode(), "ask");
        safe_resume.close().await.unwrap();
    }

    #[tokio::test]
    async fn plan_command_displays_saved_document_without_leaving_plan_mode() {
        use clap::Parser;
        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(directory.path().join("data")).unwrap();
        let provider = serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","enabled":true,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false}]})).unwrap();
        let selection = Selection {
            providers: vec![provider],
            provider_id: "fixture".into(),
            model: "fixture-model".into(),
        };
        let cli = Cli::parse_from(["dcl", "--print", "--permission-mode", "bypass"]);
        let mut session = Session::open(&cli, &paths, selection, None).await.unwrap();
        crate::extended_commands::handle("/plan", "", &paths, &mut session)
            .await
            .unwrap();
        assert_eq!(session.permission_mode(), "plan");
        deepseek_code_core::services::plans::save(&paths.data_dir, &session.id, "# Plan\nUTF-8 检查").unwrap();
        crate::extended_commands::handle("/plan", "", &paths, &mut session)
            .await
            .unwrap();
        assert_eq!(session.permission_mode(), "plan");
        // Tool-driven state changes must be reflected in UI and worker arguments.
        session.permissions.set_mode(PermissionMode::AskPermissions);
        assert_eq!(session.permission_mode(), "ask");
        crate::extended_commands::handle("/plan", "show", &paths, &mut session)
            .await
            .unwrap();
        assert_eq!(session.permission_mode(), "ask");
        let task = crate::extended_commands::plan_task("  add a cache  ", &paths, &mut session).await.unwrap();
        assert_eq!(task.as_deref(), Some("add a cache"));
        assert_eq!(session.permission_mode(), "plan");
        // Already planning: the description is reported, never submitted twice.
        assert!(crate::extended_commands::plan_task("other task", &paths, &mut session).await.unwrap().is_none());
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn context_suffix_survives_session_switch_resume_and_worker_snapshot(){
        use clap::Parser;
        let dir=tempfile::tempdir().unwrap();let paths=RuntimePaths::new(dir.path().join("data")).unwrap();
        let provider=serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1","api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"flash","name":"Flash","enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":128000}]})).unwrap();
        let selection=Selection{providers:vec![provider],provider_id:"fixture".into(),model:"flash[1m]".into()};
        let cli=Cli::parse_from(["dcl","--print"]);
        let mut session=Session::open(&cli,&paths,selection,None).await.unwrap();
        assert_eq!(session.context_window().await,Some(1_000_000));assert_eq!(session.model,"flash[1m]");
        let resolved=session.selected_provider().await.unwrap();
        assert_eq!(resolved.model.id,"flash");
        let env=crate::background_commands::worker_environment(false,&resolved).unwrap();
        let snapshot:Value=serde_json::from_str(&env.iter().find(|(key,_)|key=="CLAUDE_RUST_WORKER_MODEL").unwrap().1).unwrap();
        assert_eq!(snapshot["id"],"flash");assert_eq!(snapshot["context_window"],1_000_000);
        let saved=session.id.clone();session.sdk_set_model("flash").await.unwrap();assert_eq!(session.context_window().await,Some(128000));
        session.sdk_set_model("flash[1m]").await.unwrap();assert_eq!(session.model,"flash[1m]");
        session.resume_saved(&paths,&saved).await.unwrap();assert_eq!(session.context_window().await,Some(1_000_000));
        assert_eq!(session.store.resume(&saved).unwrap().model.as_deref(),Some("flash[1m]"));
    }

    #[tokio::test]
    async fn mcp_starts_on_first_engine_turn_and_is_reused_by_sdk_turns() {
        use clap::Parser;
        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(directory.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let marker = directory.path().join("mcp-starts.txt");
        let script = directory.path().join("mcp.cjs");
        std::fs::write(&script, r#"const fs=require('fs');fs.appendFileSync(process.env.START_MARKER,'start\n');require('readline').createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);if(r.id!==undefined)process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result:{protocolVersion:'2024-11-05',capabilities:{},serverInfo:{name:'fixture',version:'1'},tools:[],resources:[]}})+'\n');});"#).unwrap();
        std::fs::write(paths.data_dir.join("mcp-servers.json"), serde_json::to_vec(&json!({"mcpServers":{"fixture":{"command":"node","args":[script],"enabled":true,"env":{"START_MARKER":marker}}}})).unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|| async {
            ([ ("content-type", "text/event-stream") ], "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
        }));
        let server = tokio::spawn(async move { axum::serve(listener,app).await.unwrap(); });
        let provider = serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":format!("http://{address}/v1"),"api_key":"fixture-only","api_format":"openai","enabled":true,"web_search_strategy":null,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"max_tokens":64,"supports_vision":false,"supports_web_search":false}]})).unwrap();
        let selection = Selection { providers:vec![provider],provider_id:"fixture".into(),model:"fixture-model".into() };
        let cli = Cli::try_parse_from(["dcl", "--print", "--output-format", "stream-json"]).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(directory.path(), "fixture-model", "fixture").unwrap();
        let session = Session::open(&cli, &paths, selection, Some(&id)).await.unwrap();
        assert!(!marker.exists(), "opening the foreground must not start MCP children");
        session.reload_plugins().await.unwrap();
        assert!(!marker.exists(), "metadata refresh must not start unused MCP children");
        for prompt in ["first SDK turn", "second SDK turn"] {
            tokio::time::timeout(std::time::Duration::from_secs(15), session.run_turn(&cli,prompt,&mut None)).await.unwrap().unwrap();
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "start\n");
        }
        session.close().await.unwrap();
        server.abort();
    }

    #[test]
    fn agent_progress_summaries_are_opt_in() {
        let event = EngineEvent::AgentProgress {
            session_id: "child-42".into(),
            agent_type: "reviewer".into(),
            status: "running".into(),
            tool_name: Some("Read".into()),
        };
        assert_eq!(
            event_json_with(&event, true).unwrap()["summary"],
            "reviewer: running (Read)"
        );
        assert!(event_json_with(&event, false).unwrap().get("summary").is_none());
    }

    #[test]
    fn agent_progress_json_preserves_real_session_status() {
        let value = event_json(&EngineEvent::AgentProgress {
            session_id: "child-42".into(),
            agent_type: "reviewer".into(),
            status: "tool_failed".into(),
            tool_name: Some("Read".into()),
        }).unwrap();
        assert_eq!(value, json!({
            "type": "agent_progress", "session_id": "child-42",
            "agent_type": "reviewer", "status": "tool_failed", "tool_name": "Read"
        }));
    }
}
