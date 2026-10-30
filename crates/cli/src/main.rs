mod hook_progress;
mod workflows_view;
mod lsp_commands;
mod prompt_suggestion;
mod shell_mode;
mod mentions;
mod memory_shortcut;
mod images;
mod launch_permissions;
mod context_commands;
mod persistent_worker;
mod worker_frontend;
mod mcp_menu;
mod hooks_menu;
mod settings_dialog;
mod stats_view;
mod permissions_menu;
mod startup_prompts;
mod repl_loop;
mod command_catalog;
mod mcp_prompts;
mod workflow_runner;
mod workflow_commands;
mod interactive_config;
mod args;
mod migration;
mod config;
#[allow(dead_code)]
mod legacy_api_config;
mod session;
mod services;
mod memory;
mod magic_docs;
mod plugins;
mod help;
mod voice;
#[allow(dead_code)]
mod terminal;
mod repl;
mod output;
mod compaction;
mod skill_commands;
mod extended_commands;
mod session_commands;
mod session_trust;
mod status_commands;
mod display_commands;
mod statusline_commands;
mod doctor_commands;
mod background_commands;
mod batch_commands;
mod rewind_commands;
mod keybindings;
mod permission_commands;
mod sdk_input;
mod sdk;
mod sdk_initialization;
mod sdk_hooks;
mod sdk_control;
mod sdk_mcp;
mod sdk_messages;
mod sdk_queries;
mod sdk_operations;
mod account_commands;
mod design_commands;
mod ide_commands;
mod startup_options;
mod debug_logging;
mod resource_commands;
mod powerup;
mod feedback_commands;
mod remote_commands;

use anyhow::{bail, ensure, Context, Result};
use args::{Cli, Commands, McpCommands, OutputFormat, SessionCommands, SkillsCommands};
use deepseek_code_core::{
    mcp::{McpServerConfig, McpServerManager},
    runtime::{RuntimePaths, SessionStore},
    skills::SkillsManager,
};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Inside a sandbox with a domain allowlist: carry the proxy, then run the command.
    if let Some(code) = deepseek_code_core::tools::sandbox::run_bridge_from_args() {
        return std::process::ExitCode::from(code.clamp(0, 255) as u8);
    }
    match deepseek_code_core::services::batch::run_worker_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {},
        Err(error) => { eprintln!("Batch worker failed: {error:#}"); return std::process::ExitCode::FAILURE; },
    }
    match deepseek_code_core::services::background_sessions::run_worker_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {},
        Err(error) => { eprintln!("Background worker failed: {error:#}"); return std::process::ExitCode::FAILURE; },
    }
    match deepseek_code_core::services::session_worker::run_supervisor_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {},
        Err(error) => { eprintln!("Session supervisor failed: {error:#}"); return std::process::ExitCode::FAILURE; },
    }
    match Box::pin(persistent_worker::run_from_env()).await {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {},
        Err(error) => { eprintln!("Session host failed: {error:#}"); return std::process::ExitCode::FAILURE; },
    }
    let mut cli = Cli::parse_with_mode_source();
    if cli.agent_teams {
        std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
    }
    cli.normalize();
    terminal::configure_accessibility(cli.ax_screen_reader);
    debug_logging::install(cli.verbose);
    if let Err(error) = Box::pin(run(&cli)).await {
        if !worker_frontend::already_presented(&error) {
            if let Err(output_error) = output::report_startup_error(&cli, &error) {
                eprintln!("Cannot write error result: {output_error}");
            }
            eprintln!("Error: {error:#}");
        }
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

/// A typed prompt's turn; image paths in it are sent as images.
async fn prompt_turn(
    cli: &Cli,
    session: &mut session::Session,
    prompt: &str,
    input: &mut Option<terminal::input::TerminalInput>,
) -> Result<()> {
    terminal::title::name_topic(session, prompt);
    let Some(content) = images::content(&session.workspace, prompt)? else {
        return session.turn(cli, prompt, input).await;
    };
    session.sdk_content = Some(content);
    let result = session.turn(cli, prompt, input).await;
    session.sdk_content = None;
    result
}

async fn run(cli: &Cli) -> Result<()> {
    if let Some(Commands::Help {topic,command})=&cli.command {return help::show(topic.as_deref(),command); }
    cli.response_schema()?;
    let data_dir = match &cli.data_dir {
        Some(path) => path.clone(),
        None => deepseek_code_core::runtime::resolve_default_data_dir()?,
    };
    let paths = RuntimePaths::new(absolute(data_dir)?)?;
    // Engine code that has no RuntimePaths (e.g. the task tools) and child
    // processes resolve the data directory from the environment.
    std::env::set_var(deepseek_code_core::runtime::DATA_DIR_ENV, &paths.data_dir);
    if let Some(path) = &cli.path {
        std::env::set_current_dir(path).context("Cannot open project directory")?;
    }
    if cli.debug.is_some() || cli.debug_file.is_some() {
        let log = cli.debug_file.clone().unwrap_or_else(||paths.data_dir.join(format!("debug-{}.log",uuid::Uuid::new_v4())));
        debug_logging::configure_filtered(log, cli.debug.as_deref().unwrap_or(""))?;
    }
    if cli.info {
        println!("dcl {}\nOS: {}\nProject: {}\nData: {}\nEngine: deepseek-code-core (direct; no Bridge)", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::current_dir()?.display(), paths.data_dir.display());
        return Ok(());
    }
    let mut skill_prompt = None;
    match &cli.command {
        Some(Commands::Auth { action }) => return account_commands::run(&paths, action).await,
        Some(Commands::Plugin { action }) => return plugins::run(&paths,action).await,
        Some(Commands::Update { url }) => {
            let update = deepseek_code_core::services::updates::check(url,env!("CARGO_PKG_VERSION")).await?;
            return services::print(&serde_json::json!({"up_to_date":update.is_none(),"update":update}));
        }
        Some(Commands::StressTest { concurrency, iterations, endpoint, allow_remote }) => {
            return tokio::select! {result=deepseek_code_core::services::diagnostics::stress(*concurrency,*iterations,endpoint.clone(),*allow_remote)=>services::print(&result?), _=tokio::signal::ctrl_c()=>anyhow::bail!("Diagnostics cancelled")};
        }
        Some(Commands::Learning { action }) => return migration::learning(&paths, action).await,
        Some(Commands::Performance { action }) => return migration::performance(&paths, action).await,
        Some(Commands::Project { action }) => return migration::project(action).await,
        Some(Commands::Cron { action }) => return migration::cron(&paths, &std::env::current_dir()?, action).await,
        Some(Commands::WorkflowRun { script, run_dir, args_json, resume_dir, agent_args, concurrency }) => {
            let source = std::fs::read_to_string(script).with_context(|| format!("Reading workflow script {}", script.display()))?;
            let result = workflow_runner::run(workflow_runner::Run {
                script: &source,
                args: args_json.as_deref().map(serde_json::from_str).transpose()?.unwrap_or(serde_json::Value::Null),
                run_dir,
                resume_dir: resume_dir.as_deref(),
                cwd: &std::env::current_dir()?,
                agent_args: serde_json::from_str(agent_args).context("--agent-args must be a JSON array")?,
                concurrency: *concurrency,
            }).await;
            // status.json tells /workflows how the run ended.
            let finished = chrono::Utc::now().timestamp_millis();
            return match result {
                Ok(value) => {
                    std::fs::write(run_dir.join("result.json"), serde_json::to_vec_pretty(&value)?)?;
                    std::fs::write(run_dir.join("status.json"), serde_json::json!({"status":"completed","finished_ms":finished}).to_string())?;
                    println!("Workflow result:\n{}", serde_json::to_string_pretty(&value)?);
                    Ok(())
                }
                Err(error) => {
                    let _ = std::fs::write(run_dir.join("status.json"), serde_json::json!({"status":"failed","error":format!("{error:#}"),"finished_ms":finished}).to_string());
                    Err(error)
                }
            };
        }
        Some(Commands::Ssh { config, operation, yes }) => return tokio::select! { result=migration::ssh(config,operation,*yes)=>result, _=tokio::signal::ctrl_c()=>anyhow::bail!("SSH operation cancelled") },
        Some(Commands::Lsp { action, language, yes }) => {
            let workspace = cli.path.clone().map_or_else(std::env::current_dir, Ok)?;
            return lsp_commands::command(&paths, &workspace, action.as_deref(), language.as_deref(), *yes).await;
        }
        Some(Commands::Remote { request, yes }) => return tokio::select! { result=migration::remote(request,*yes)=>result, _=tokio::signal::ctrl_c()=>anyhow::bail!("Remote operation cancelled") },
        Some(Commands::Agents { action }) => {
            let context = services::context(&paths)?;
            match action {
                args::AgentCommands::ImportLegacy { source, id, preview } => return services::print(&deepseek_code_core::services::agent_import::import(&context, source, id, *preview)?),
                args::AgentCommands::List => return services::print(&deepseek_code_core::services::agents::list(&context)?),
                args::AgentCommands::Import { source } => {
                    let definition = serde_json::from_slice(&std::fs::read(source)?)?;
                    deepseek_code_core::services::agents::install(&context, definition)?;
                    return services::print(&serde_json::json!({"imported":true}));
                }
            }
        }
        Some(Commands::Agent { agent_type, prompt }) => {
            deepseek_code_core::services::agents::resolve(&services::context(&paths)?, agent_type)?;
            skill_prompt = Some(prompt.clone());
        }
        Some(Commands::MagicDocs { action }) => return magic_docs::run(cli, &paths, action).await,
        Some(Commands::Memory { action }) => return memory::run(cli, &paths, action).await,
        Some(Commands::VoiceConfig { endpoint, model, credential, language }) => {
            let context = services::context(&paths)?;
            let config = deepseek_code_core::services::voice::VoiceConfig {
                endpoint: endpoint.clone(), model: model.clone(),
                credential: credential.clone(), language: language.clone(),
            };
            config.validate()?;
            context.save_setting("voice", &config)?;
            return services::print(&config);
        }
        Some(Commands::Voice { file, push_to_talk, continuous, send }) => {
            let context = services::context(&paths)?;
            return voice::run(cli, &paths, &context, file, *push_to_talk, *continuous, *send).await;
        }
        Some(Commands::TeamSync { action }) => {
            return services::team(&services::context(&paths)?, action).await;
        }
        Some(Commands::Services { action }) => {
            return services::services(&services::context(&paths)?, action).await;
        }
        Some(Commands::Config { action }) => return config::run(&paths, action),
        Some(Commands::Mcp { action }) => return mcp(&paths, action).await,
        Some(Commands::Sessions { action }) => {
            if let SessionCommands::ShutdownWorker { id } = action {
                return session_trust::shutdown_worker(&paths, id).await;
            }
            if let SessionCommands::StopTask { id, task_id } = action {
                return session_trust::stop_task(&paths, id, task_id).await;
            }
            if let SessionCommands::Compact { id, instructions } = action {
                return compaction::run(cli, &paths, id, instructions).await;
            }
            let db = paths.open_database()?;
            let value = match action {
                SessionCommands::ShutdownWorker { .. } | SessionCommands::StopTask { .. } => unreachable!(),
                SessionCommands::Trust { id, folders } => serde_json::to_value(deepseek_code_core::services::session_trust::repair(&db, &paths.data_dir, id, folders).map_err(|error| session_trust::repair_error(&paths, id, error))?)?,
                SessionCommands::Compact { .. } => unreachable!(),
                SessionCommands::ImportLegacy {source,preview} => serde_json::to_value(deepseek_code_core::services::session_import::import(&services::context(&paths)?,source,&std::env::current_dir()?,*preview)?)?,
                SessionCommands::List => serde_json::to_value(
                    db.with_conn(deepseek_code_core::db::conversation_repo::list_conversations)??,
                )?,
                SessionCommands::Show { id } => {
                    let store = SessionStore::new(db);
                    serde_json::json!({"session":store.resume(id)?,"messages":store.history(id)?})
                }
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        Some(Commands::Init { name }) => {
            let cwd = std::env::current_dir()?;
            terminal::init::ensure_directories(&cwd)?;
            let file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(cwd.join("CLAUDE.md"));
            match file {
                Ok(mut file) => writeln!(
                    file,
                    "# {}\n\nAdd project instructions and verification commands here.",
                    name.as_deref().unwrap_or("Project")
                )?,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            eprintln!("Initialized {}", cwd.display());
            return Ok(());
        }
        Some(Commands::Skills { action }) => {
            if let SkillsCommands::ImportLegacy {source,preview} = action {
                return services::print(&deepseek_code_core::skills::import::import_legacy(source,&paths.data_dir.join("skills"),*preview)?);
            }
            let manager = SkillsManager::with_path(paths.data_dir.join("skills"));
            let skills = manager.load_skills().await?;
            match action {
                SkillsCommands::ImportLegacy {..} => unreachable!(),
                SkillsCommands::List => {
                    println!("{}", serde_json::to_string_pretty(&skills)?);
                    return Ok(());
                }
                SkillsCommands::Search { query } => {
                    let query = query.to_lowercase();
                    let selected: Vec<_> = skills
                        .iter()
                        .filter(|s| {
                            format!("{} {}", s.name, s.description)
                                .to_lowercase()
                                .contains(&query)
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&selected)?);
                    return Ok(());
                }
                SkillsCommands::Help { skill } | SkillsCommands::Execute { skill, .. } => {
                    let found = skills
                        .iter()
                        .find(|s| &s.id == skill || &s.name == skill)
                        .context("Skill not found")?;
                    let content = manager.get_skill_content(&found.id, "SKILL.md")?;
                    if let SkillsCommands::Execute { args, .. } = action {
                        ensure!(found.enabled, "Skill is disabled");
                        skill_prompt = Some(format!(
                            "Use this skill for the following task:\n\n{content}\n\nTask: {}",
                            args.join(" ")
                        ));
                    } else {
                        println!("{content}");
                        return Ok(());
                    }
                }
            }
        }
        _ => {}
    }
    let explicit_repl = matches!(cli.command, Some(Commands::Repl { .. }));
    let sdk_stream = cli.input_format == args::InputFormat::StreamJson;
    ensure!(
        !cli.replay_user_messages || sdk_stream,
        "--replay-user-messages requires --input-format stream-json"
    );
    ensure!(
        !cli.include_partial_messages || cli.output_format == OutputFormat::StreamJson,
        "--include-partial-messages requires --output-format stream-json"
    );
    ensure!(
        cli.permission_prompt_tool.is_none() || sdk_stream,
        "--permission-prompt-tool stdio requires --input-format stream-json"
    );
    ensure!(
        !sdk_stream || cli.output_format == OutputFormat::StreamJson,
        "--input-format stream-json requires --output-format stream-json"
    );
    let single = cli.init_only
        || sdk_stream
        || cli.print
        || cli.print_short
        || cli.no_interactive
        || cli.no_interactive_short
        || matches!(cli.command, Some(Commands::Query { .. }))
        || skill_prompt.is_some()
        || !io::stdin().is_terminal();
    ensure!(
        !explicit_repl || !single,
        "repl requires interactive stdin; use query or --print for scripts"
    );
    ensure!(
        single || cli.output_format == OutputFormat::Text,
        "JSON output requires --print or query"
    );
    let mut prompt = skill_prompt
        .or_else(|| match &cli.command {
            Some(Commands::Repl { prompt }) | Some(Commands::Query { prompt }) => prompt.clone(),
            _ => cli.prompt.clone(),
        })
        .unwrap_or_default();
    if !sdk_stream && !io::stdin().is_terminal() {
        let piped = read_piped_stdin(io::stdin(), STDIN_FIRST_BYTE_TIMEOUT)?;
        if !piped.trim().is_empty() {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(&piped);
        }
    }

    let resumed = if let Some(id) = &cli.resume {
        let store = SessionStore::new(paths.open_database()?);
        Some(store.resume(id).map_err(|error| session_trust::diagnose(&store.db, &paths, id, error))?)
    } else if cli.continue_session {
        Some(SessionStore::new(paths.open_database()?).latest(&std::env::current_dir()?)?
            .context("No resumable Code session in this directory")?)
    } else {
        None
    };
    if let Some(resumed) = &resumed {
        let db = paths.open_database()?;
        let (workspace, _) = db.with_conn(|conn| {
            deepseek_code_core::native_engine::workspace::resolve_workspace(
                conn,
                &paths.data_dir.join("workspaces"),
                &resumed.id,
                None,
            )
        })??;
        if cli.path.is_some() {
            ensure!(
                std::fs::canonicalize(&workspace)?
                    == std::fs::canonicalize(std::env::current_dir()?)?,
                "--path differs from the resumed session's project"
            );
        }
        std::env::set_current_dir(workspace)?;
    }
    if cli.init_only {
        ensure!(
            !sdk_stream && cli.worktree.is_none() && !cli.fork_session,
            "--init-only does not support stream-json, worktree or fork-session"
        );
        let context = services::context(&paths)?;
        let workspace = std::env::current_dir()?;
        let store = SessionStore::new(context.db.clone());
        let id = if let Some(resumed) = &resumed {
            resumed.id.clone()
        } else {
            store.create_with_id(
                &workspace,
                "",
                "",
                &cli.session_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            )?
        };
        let work = async {
            deepseek_code_core::services::lifecycle_events::check_configuration(&context, &workspace, &id)
                .await?;
            for warning in deepseek_code_core::services::lifecycle_events::setup(
                &context, &workspace, &id, "init", true,
            )
            .await?
            {
                eprintln!("Setup hook: {warning}");
            }
            let mut hooks = deepseek_code_core::services::tool_hooks::HookRunner::load_with_plugins(
                &context, &workspace,
            )?;
            hooks.force_synchronous();
            let result = hooks
                .run(
                    deepseek_code_core::services::tool_hooks::HookEvent::SessionStart,
                    &id,
                    serde_json::json!({"source":"startup"}),
                )
                .await?;
            for warning in result.warnings {
                eprintln!("SessionStart hook: {warning}");
            }
            Ok::<(), anyhow::Error>(())
        };
        return tokio::select! { result = hook_progress::during(&id, true, work) => result, _ = tokio::signal::ctrl_c() => anyhow::bail!("Setup hooks interrupted") };
    }
    let mut input = if single { None } else { Some(session::terminal_input()) };
    let selection = config::select(
        cli,
        &paths,
        resumed.as_ref().and_then(|s| s.model.as_deref()),
        resumed.as_ref().and_then(|s| s.provider.as_deref()),
    );
    let selection = match selection {
        Ok(selection) => selection,
        Err(error) if !single => {
            // A first start has nothing configured: the onboarding explains
            // itself. Otherwise show what is wrong with the saved setup.
            if !paths.providers()?.is_empty() {
                eprintln!("Provider setup required: {error:#}");
            }
            interactive_config::setup(&paths, input.as_mut().unwrap()).await?
        }
        Err(error) if single && !sdk_stream && prompt.trim().is_empty() => {
            return Err(error.context("No prompt was supplied; configure a provider before SessionStart can supply initialUserMessage"));
        }
        Err(error) => return Err(error),
    };
    startup_options::validate(cli, &std::env::current_dir()?)?;
    if !single && !sdk_stream {
        // As in the reference CLI, before project settings and hooks load.
        let folder = cli.path.clone().unwrap_or(std::env::current_dir()?);
        startup_prompts::confirm(&paths.data_dir, &folder, matches!(cli.permission_mode, args::PermissionModeArg::Bypass), cli.full_access)?;
    }
    deepseek_code_core::permissions::set_full_access(cli.full_access && matches!(cli.permission_mode, args::PermissionModeArg::Bypass));
    let mut session = session::Session::open(cli, &paths, selection, resumed.as_ref().map(|s| s.id.as_str())).await?;
    startup_options::apply(cli, &mut session).await?;
    let mut screen_mode = terminal::screen_mode::ScreenMode::default();
    let result = async {
        if !sdk_stream { session.start_hooks(if resumed.is_some() { "resume" } else { "startup" }).await?; }
        if cli.init || cli.init_only || cli.maintenance {
            anyhow::ensure!(!sdk_stream, "Setup flags with stream-json input are not supported");
            let trigger = if cli.maintenance { "maintenance" } else { "init" };
            let context = session.hook_service_context();
            let outcome = tokio::select! {
                result = hook_progress::during(&session.id, true, deepseek_code_core::services::lifecycle_events::setup(&context, &session.workspace, &session.id, trigger, false)) => result,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Setup hooks interrupted"),
            }?;
            for warning in outcome { eprintln!("Setup hook: {warning}"); }
        }
        if !single {
            eprintln!("{}", terminal::banner(&session.model, &session.workspace));
            // --resume and --continue show the conversation so far, as in the reference CLI.
            if resumed.is_some() { repl::show_resumed(&session, false)?; }
        }
        if !sdk_stream {
            let initial = session.initial_hook_message.take();
            if single {
                ensure!(initial.is_some() || !prompt.trim().is_empty(), "A prompt or piped stdin is required");
                if let Some(message) = initial {
                    Box::pin(session.turn_initial_hook(cli, &message, &mut input, prompt.trim().is_empty() || cli.output_format == OutputFormat::StreamJson)).await?;
                }
            }
        }
        if !sdk_stream && !prompt.trim().is_empty() {
            if !single {
                let display = display_commands::load(&session)?;
                terminal::style::configure(&display.theme, &display.color);
                terminal::prompt_display::write_submitted(
                    &mut io::stderr(), &format!("{} ", terminal::prompt_glyph()), &prompt,
                    crossterm::terminal::size().map(|(cols, _)| cols.saturating_sub(1) as usize).unwrap_or(79),
                    io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !terminal::screen_reader(), false,
                )?;
            }
            // As in the reference CLI, /compact also works without a terminal.
            let compact = prompt.trim().strip_prefix("/compact").filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
            match compact {
                Some(instructions) if single => {
                    let summarized = repl::compact(&paths, &mut session, instructions).await?;
                    let message = format!("Compacted: {summarized} messages summarized; the original transcript remains in the session history.");
                    if cli.output_format == OutputFormat::Text {
                        println!("{message}");
                    } else {
                        session::emit_json(&serde_json::json!({"type":"result","subtype":"success","is_error":false,"session_id":session.id,"result":message,"num_turns":0,"permission_denials":[],"uuid":uuid::Uuid::new_v4().to_string()}))?;
                    }
                }
                _ => prompt_turn(cli, &mut session, &prompt, &mut input).await?,
            }
        }
        if sdk_stream {
            Box::pin(sdk::run(cli,&mut session,(!prompt.trim().is_empty()).then_some(prompt.as_str()),if resumed.is_some() { "resume" } else { "startup" })).await?;
        }
        if single { return Ok(()); }
        let mut skills = skill_commands::load_for_workspace(&paths, &session.workspace).await?;
        input.as_mut().unwrap().set_commands(skill_commands::catalog(&skills));
        let mut composer = terminal::prompt::Composer::default();
        let mut background_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
        // /loop prompts: interval, prompt and when each runs next.
        let mut loops: Vec<(std::time::Duration, String, std::time::Instant)> = Vec::new();
        background_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut remote = remote_commands::Remote::default();
        // As in the reference CLI, remoteControlAtStartup connects every session.
        if remote_commands::at_startup(&paths.data_dir) {
            if let Err(error) = remote.command("", &paths, &session, &mut input).await { eprintln!("Remote Control: {error:#}"); }
        }
        let mut last_config_error = String::new();
        terminal::tips::start_session(&paths.data_dir);
        // Lets other sessions see this one is open while it is idle.
        let mut presence: Option<(String, deepseek_code_core::services::session_messages::PresenceLease)> = None;
        // History length the last suggestion was generated for.
        let mut suggested_for = session.store.history(&session.id)?.len();
        terminal::STARTED.get_or_init(std::time::Instant::now);
        terminal::title::start();
        if let Some(speed) = std::fs::read_to_string(paths.data_dir.join("scroll-speed")).ok().and_then(|text| text.trim().parse::<usize>().ok()).filter(|speed| (1..=20).contains(speed)) {
            terminal::SCROLL_SPEED.store(speed, std::sync::atomic::Ordering::Relaxed);
        }
        'repl: loop {
            terminal::title::apply(&session.id);
            let idle_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut idle_notified = false;
            for notice in background_commands::collect_completed(&session)? { eprintln!("{notice}"); }
            // A background command that finished meanwhile starts a turn, as in
            // the reference CLI.
            if !composer.is_pending() {
                match background_commands::wake_for_notifications(cli, &mut session, &mut input).await {
                    Ok(true) => continue 'repl,
                    Ok(false) => {},
                    Err(error) => if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); },
                }
            }
            if let Some(prompt) = remote.take_deferred() {
                if let Err(error) = session.turn(cli, &prompt, &mut input).await { if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); } }
            }
            if let Err(error) = remote.sync(&session) { eprintln!("Remote Control: {error:#}"); }
            if presence.as_ref().is_none_or(|(id, _)| *id != session.id) {
                drop(presence.take());
                presence = deepseek_code_core::services::session_messages::PresenceLease::acquire(&paths.data_dir, &session.id)
                    .ok()
                    .map(|lease| (session.id.clone(), lease));
            }
            if prompt_suggestion::enabled(&paths.data_dir) {
                let history = session.store.history(&session.id)?;
                if history.len() != suggested_for {
                    suggested_for = history.len();
                    if let (Some(text), Ok(provider)) = (prompt_suggestion::context(&history), session.selected_provider().await) {
                        prompt_suggestion::spawn(provider, text, input.as_ref().unwrap().suggestion_sender());
                    }
                }
            }
            skills = skill_commands::load_for_workspace(&paths, &session.workspace).await?;
            if let Err(error) = input.as_mut().unwrap().set_keybindings(&paths.data_dir.join("keybindings.json")) {
                eprintln!("Keybindings: {error:#}");
            }
            let display = display_commands::load(&session)?;
            if screen_mode.set(&display.tui)? {
                for row in session.store.history(&session.id)? {
                    let content: String = row["content"].as_str().unwrap_or("").chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).collect();
                    eprintln!("\n{}\n{content}", row["role"].as_str().unwrap_or("message"));
                }
            }
            terminal::style::configure(&display.theme, &display.color);
            let mut catalog = skill_commands::catalog(&skills);
            if let Some((_, description)) = catalog.iter_mut().find(|(name, _)| name == "/model") {
                *description = format!("Set the AI model (currently {} · {})", session.model, session.provider_id);
            }
            input.as_mut().unwrap().set_commands(catalog);
            input.as_ref().unwrap().set_workspace(&session.workspace);
            input.as_ref().unwrap().set_context_note(session_commands::context_status(&session).await);
            input.as_ref().unwrap().set_shells(background_commands::running_shells(&paths, &session.id));
            let (counter_paths, counter_session) = (paths.clone(), session.id.clone());
            input.as_ref().unwrap().set_shell_counter(Box::new(move || background_commands::running_shells(&counter_paths, &counter_session)));
            input.as_ref().unwrap().set_effort(Some(session_commands::model_label(&session).await));
            let (agent_paths, agent_db, agent_session) = (paths.clone(), session.store.db.clone(), session.id.clone());
            input.as_ref().unwrap().set_agent_rows(Box::new(move |width| background_commands::agent_rows(&background_commands::running_agents(&agent_paths, &agent_db, &agent_session), width)));
            let (todo_paths, todo_session) = (paths.clone(), session.id.clone());
            input.as_ref().unwrap().set_todos(Box::new(move || background_commands::todo_items(&todo_paths, &todo_session)));
            input.as_ref().unwrap().set_vim(terminal::vim::enabled(&paths.data_dir));
            if display.diff_panel && !display.focus {
                match deepseek_code_core::services::worktree_diff::snapshot(&session.workspace, display.diff_staged).await {
                    Ok(diff) => input.as_mut().unwrap().set_panel(vec![(if display.diff_staged { "Staged diff" } else { "Uncommitted changes (untracked files listed by name)" }.into(), diff)], true),
                    Err(error) => { eprintln!("Diff: {error:#}"); input.as_mut().unwrap().set_details(vec![]); },
                }
            } else {
                input.as_mut().unwrap().set_details(vec![]);
            }
            input.as_mut().unwrap().set_transcript(if display.focus || display.diff_panel { vec![] } else { repl::transcript(&session)? });
            match display_commands::statusline(&session, &display).await {
                Ok(Some(line)) => eprintln!("{line}"),
                Ok(None) => {},
                Err(error) => eprintln!("Status line: {error:#}"),
            }
            input.as_ref().expect("interactive reader").set_permission_mode(session.permission_mode());
            let idle_prompt = format!("\n{} ", terminal::prompt_glyph());
            let prompt = if composer.is_pending() { "... " } else { idle_prompt.as_str() };
            let line = 'read: loop { tokio::select! {
                line = input.as_mut().expect("interactive reader").read_prompt(prompt) => break 'read match line {
                    Ok(Some(terminal::input::PromptEvent::Line(line))) => line,
                    Ok(Some(terminal::input::PromptEvent::CyclePermission)) => {
                        if let Err(error) = session.cycle_permission_mode().await { eprintln!("Permission mode: {error:#}"); }
                        continue 'repl;
                    },
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        if composer.is_pending() {
                            composer.cancel();
                            eprintln!("Prompt discarded.");
                            continue 'repl;
                        }
                        break 'repl;
                    },
                    Err(error) => return Err(error.into()),
                    Ok(None) => {
                        if composer.is_pending() { eprintln!("Unfinished prompt discarded."); }
                        break 'repl;
                    }
                },
                _ = background_refresh.tick() => {
                    match deepseek_code_core::services::lifecycle_events::check_configuration(&session.hook_service_context(), &session.workspace, &session.id).await {
                        Ok(()) => last_config_error.clear(),
                        Err(error) => { let text = format!("{error:#}"); if last_config_error != text { input.as_ref().expect("interactive reader").notice(text.clone()); last_config_error = text; } }
                    }
                    if last_config_error.is_empty() && !idle_notified && tokio::time::Instant::now() >= idle_deadline {
                        idle_notified = true;
                        if let Err(error) = deepseek_code_core::services::lifecycle_events::run(&session.hook_service_context(), &session.workspace, &session.id,
                            deepseek_code_core::services::tool_hooks::HookEvent::Notification,
                            serde_json::json!({"message":"Claude is waiting for your input","notification_type":"idle_prompt"})).await {
                            input.as_ref().expect("interactive reader").notice(format!("Notification hook: {error:#}"));
                        }
                    }
                    if !composer.is_pending()
                        && deepseek_code_core::services::async_hooks::has_pending_wake(&paths.data_dir, &session.id)
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        if let Err(error) = session.wake_hooks(cli, &mut input).await { eprintln!("Hook wake: {error:#}"); }
                        continue 'repl;
                    }
                    if let Err(error) = session.sync_workspace() { eprintln!("Worktree: {error:#}"); }
                    // A due CronCreate job runs as if typed, once the input is empty.
                    if !composer.is_pending() {
                        let scope = deepseek_code_core::tools::cron_jobs::Scope { root: &paths.data_dir, session: &session.id, project: &session.workspace };
                        match deepseek_code_core::tools::cron_jobs::take_due(&scope, chrono::Local::now()) {
                            Ok(due) if !due.is_empty() => {
                                if input.as_mut().expect("interactive reader").yield_prompt().await? {
                                    for prompt in due {
                                        eprintln!("\u{23f0} Scheduled: {}", prompt.lines().next().unwrap_or_default());
                                        input.as_mut().expect("interactive reader").queue_prompt(prompt);
                                    }
                                    continue 'repl;
                                }
                                for prompt in due { deepseek_code_core::services::input_queue::push(&session.store.db, &session.id, &prompt)?; }
                            }
                            Ok(_) => {}
                            Err(error) => eprintln!("Scheduled jobs: {error:#}"),
                        }
                    }
                    // A self-paced /loop wakes as if its input were typed again.
                    if !composer.is_pending() && deepseek_code_core::tools::wakeup::is_due(&paths.data_dir, &session.id, chrono::Utc::now().timestamp())
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        if let Some((prompt, reason)) = deepseek_code_core::tools::wakeup::take_due(&paths.data_dir, &session.id, chrono::Utc::now().timestamp()).unwrap_or(None) {
                            eprintln!("\u{21bb} Loop wakeup: {reason}");
                            input.as_mut().expect("interactive reader").queue_prompt(prompt);
                            continue 'repl;
                        }
                    }
                    // A due /loop prompt runs as if typed, once the input is empty.
                    if let Some(index) = loops.iter().position(|(_, _, next)| *next <= std::time::Instant::now()) {
                        if !composer.is_pending() && input.as_mut().expect("interactive reader").yield_prompt().await? {
                            let (every, prompt, next) = &mut loops[index];
                            *next = std::time::Instant::now() + *every;
                            eprintln!("\u{21bb} Loop: {prompt}");
                            input.as_mut().expect("interactive reader").queue_prompt(prompt.clone());
                            continue 'repl;
                        }
                    }
                    // A background agent waiting for approval asks here, as in the
                    // reference CLI, once the input is empty.
                    if !composer.is_pending()
                        && background_commands::agent_question(&paths, &session.id).is_some()
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        if let Err(error) = background_commands::answer_agent_question(&paths, &session) { eprintln!("Agent question: {error:#}"); }
                        continue 'repl;
                    }
                    let notices = background_commands::collect_completed(&session)?;
                    // A finished background command starts a turn; its line is
                    // printed first, above that turn.
                    if !composer.is_pending()
                        && deepseek_code_core::services::input_queue::any(&session.store.db, &session.id, background_commands::is_notification).unwrap_or(false)
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        for notice in notices { eprintln!("{notice}"); }
                        continue 'repl;
                    }
                    for notice in notices {
                        input.as_ref().expect("interactive reader").notice(notice);
                    }
                },
                update = remote.update() => {
                    let reader = input.as_ref().expect("interactive reader");
                    match remote.handle(update, &mut session, |line| reader.notice(line)).await {
                        Ok(remote_commands::Action::Prompt(prompt)) => {
                            if !input.as_mut().expect("interactive reader").yield_prompt().await? {
                                eprintln!("Remote prompt queued until the current input is submitted.");
                                remote.defer(prompt);
                            } else if composer.is_pending() {
                                eprintln!("Remote prompt queued until the multiline prompt is finished.");
                                remote.defer(prompt);
                            } else {
                                eprintln!("Remote prompt: {}", prompt.lines().next().unwrap_or_default());
                                remote.defer(prompt);
                                continue 'repl;
                            }
                        },
                        Ok(remote_commands::Action::Refresh) => {
                            if input.as_mut().expect("interactive reader").yield_prompt().await? { continue 'repl; }
                        },
                        Ok(remote_commands::Action::None) => {},
                        Err(error) => eprintln!("Remote Control: {error:#}"),
                    }
                    input.as_ref().expect("interactive reader").set_permission_mode(session.permission_mode());
                },
                _ = tokio::signal::ctrl_c() => {
                    if composer.is_pending() {
                        composer.cancel();
                        eprintln!("Prompt discarded.");
                        continue 'repl;
                    }
                    break 'repl;
                },
            } };
            let line = match composer.push(line) {
                terminal::prompt::Input::Pending => continue,
                terminal::prompt::Input::Cancelled => { eprintln!("Prompt discarded."); continue; },
                terminal::prompt::Input::Prompt(prompt) => {
                    if !prompt.trim().is_empty() {
                        if let Err(error) = prompt_turn(cli, &mut session, &prompt, &mut input).await { if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); } }
                    }
                    continue;
                },
                terminal::prompt::Input::Command(line) => line,
            };
            if line.trim() == "/vim" {
                let on = !terminal::vim::enabled(&paths.data_dir);
                match terminal::vim::set_enabled(&paths.data_dir, on) {
                    Ok(()) => eprintln!("{}", if on { "Vim mode on: Esc for normal mode, i/a to insert." } else { "Vim mode off." }),
                    Err(error) => eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if let Some(note) = line.trim_start().strip_prefix("# ") {
                match memory_shortcut::save(&session.workspace, note) {
                    Ok(path) => eprintln!("Saved to {}: - {}", path.display(), note.trim()),
                    Err(error) => eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if let Some(command) = line.trim_start().strip_prefix('!') {
                if let Err(error) = shell_mode::run(&session, command).await { eprintln!("Error: {error:#}"); }
                continue;
            }
            if line.trim() == "/reload-skills" {
                match skill_commands::load_for_workspace(&paths, &session.workspace).await {
                    Ok(updated) => {
                        skills = updated;
                        input.as_mut().unwrap().set_commands(skill_commands::catalog(&skills));
                        eprintln!("Loaded {} skill commands.", skills.len());
                    },
                    Err(error) => eprintln!("Cannot reload skills: {error:#}"),
                }
                continue;
            }
            let (command, argument) = line.trim().split_once(char::is_whitespace).unwrap_or((line.trim(), ""));
            let command = command_catalog::canonical(command);
            if command == "/powerup" && powerup::interactive_topic(argument).is_some() && powerup::available() {
                if let Err(error) = input.as_mut().unwrap().powerup(paths.clone(), argument.to_owned()).await {
                    eprintln!("Power-ups: {error:#}");
                }
                continue;
            }
            match design_commands::handle(cli, command, argument, &paths, &mut session).await {
                Ok(true) => continue,
                Err(error) => { eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match batch_commands::handle(cli, command, argument, &paths, &mut session).await {
                Ok(true) => continue,
                Err(error) => { eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match background_commands::handle(cli, command, argument, &paths, &mut session, &mut input).await {
                Ok(background_commands::Outcome::Handled) => continue,
                Ok(background_commands::Outcome::Exit) => break,
                Err(error) => { if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); } continue; },
                Ok(background_commands::Outcome::NotHandled) => {},
            }
            if command == "/scroll-speed" {
                // As in the reference CLI: lines per mouse wheel notch, kept.
                let file = paths.data_dir.join("scroll-speed");
                match argument.trim() {
                    "" => eprintln!("Scroll speed: {} lines per wheel notch. /scroll-speed 1-20 changes it.", terminal::SCROLL_SPEED.load(std::sync::atomic::Ordering::Relaxed)),
                    value => match value.parse::<usize>().ok().filter(|speed| (1..=20).contains(speed)) {
                        Some(speed) => {
                            terminal::SCROLL_SPEED.store(speed, std::sync::atomic::Ordering::Relaxed);
                            if let Err(error) = std::fs::write(&file, speed.to_string()) { eprintln!("Scroll speed set for this session; not saved: {error}"); }
                            else { eprintln!("Scroll speed: {speed} lines per wheel notch."); }
                        }
                        None => eprintln!("Usage: /scroll-speed 1-20"),
                    },
                }
                continue;
            }
            if command.starts_with("/mcp__") {
                match mcp_prompts::expand(&session, command, argument).await {
                    Ok(Some(prompt)) => if let Err(error) = session.turn(cli, &prompt, &mut input).await {
                        if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); }
                    },
                    Ok(None) => {}
                    Err(error) => eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if command == "/loop" {
                match repl_loop::run(argument, &mut loops) {
                    Ok(repl_loop::Outcome::Message(message)) => eprintln!("{message}"),
                    Ok(repl_loop::Outcome::Run(prompt)) => if let Err(error) = session.turn(cli, &prompt, &mut input).await {
                        if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); }
                    },
                    Err(error) => eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if matches!(command, "/remote-control" | "/rc") {
                if let Err(error) = remote.command(argument, &paths, &session, &mut input).await { eprintln!("Error: {error:#}"); }
                continue;
            }
            if command == "/plan" {
                match extended_commands::plan_task(argument, &paths, &mut session).await {
                    Ok(Some(task)) => if let Err(error) = session.turn(cli, &task, &mut input).await {
                        if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); }
                    },
                    Err(error) => eprintln!("Error: {error:#}"),
                    Ok(None) => {},
                }
                continue;
            }
            if command == "/goal" {
                match session_commands::handle(command, argument, &paths, &mut session).await {
                    Ok(_) if !argument.trim().is_empty() && !session_commands::is_goal_clear(argument) => {
                        if let Err(error) = session.turn(cli, argument.trim(), &mut input).await {
                            if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); }
                        }
                    },
                    Err(error) => eprintln!("Error: {error:#}"),
                    _ => {},
                }
                continue;
            }
            match lsp_commands::handle(line.trim(), &session, input.as_mut().unwrap()).await {
                Ok(true) => continue,
                Err(error) => { eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match interactive_config::handle(line.trim(), &paths, &mut session, input.as_mut().unwrap()).await {
                Ok(true) => continue,
                Err(error) => { eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match repl::handle(&line, &paths, &mut session).await {
                Ok(true) => {
                    if let Some(draft) = terminal::NEXT_DRAFT.lock().unwrap_or_else(|error| error.into_inner()).take() {
                        if let Some(input) = &input { input.restore_draft(draft); }
                    }
                    continue;
                },
                Err(error) => { eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match line.trim() {
                "" => continue,
                "/exit" | "/quit" | "exit" | "quit" | ".exit" | ":q" => break,
                "/" | "/help" | "help" | ".help" | ":h" => {
                    eprintln!("{}", command_catalog::help());
                    for (name, description) in &skills { eprintln!("  {name:<24} {description}"); }
                },
                "status" | ".status" => { if let Err(error) = status_commands::run(&session, "").await { eprintln!("Status: {error:#}"); } },
                "/history" | "history" | ".history" => println!("{}", serde_json::to_string_pretty(&session.store.history(&session.id)?)?),
                "/tools" | "tools" | ".tools" => for tool in deepseek_code_core::tools::get_tool_definitions() { eprintln!("{}: {}", tool.name, tool.description); },
                command if skills.contains_key(command.split_whitespace().next().unwrap_or(command)) => {
                    if let Err(error) = session.turn(cli, &line, &mut input).await { if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); } }
                },
                command if command.starts_with('/') => eprintln!("Unknown command: {}. Use /help; use /multiline to send text starting with /.", command.split_whitespace().next().unwrap_or(command)),
                _ => if let Err(error) = prompt_turn(cli, &mut session, &line, &mut input).await { if !worker_frontend::already_presented(&error) { eprintln!("Error: {error:#}"); } },
            }
        }
        if remote.is_connected() {
            if let Err(error) = remote.disconnect().await { eprintln!("Remote Control: {error:#}"); }
        }
        // As in the reference CLI, a conversation worth returning to ends with how to.
        if session.store.history(&session.id).is_ok_and(|rows| !rows.is_empty()) {
            eprintln!("\nResume this session with:\ndcl --resume {}", session.id);
        }
        Ok(())
    }.await;
    if !single { terminal::title::clear(); }
    session.end_hooks(if result.is_ok() && !single { "prompt_input_exit" } else { "other" }).await;
    let shutdown = session.close().await;
    // A completed single turn already wrote its terminal result. Shutdown may
    // still fail, but stdout must not acquire a second result record.
    if single && cli.output_format != OutputFormat::Text && result.is_ok() {
        if let Err(error) = shutdown {
            return Err(output::ReportedError(format!("Session shutdown failed: {error:#}")).into());
        }
        return result;
    }
    result.and(shutdown)
}

async fn mcp(paths: &RuntimePaths, action: &McpCommands) -> Result<()> {
    std::fs::create_dir_all(&paths.data_dir)?;
    let manager = McpServerManager::new(paths.data_dir.join("mcp-servers.json"));
    if let McpCommands::ImportLegacy { source, preview } = action {
        return services::print(&manager.import_legacy(source, *preview).await?);
    }
    manager.initialize().await?;
    match action {
        McpCommands::ImportLegacy { .. } => unreachable!(),
        McpCommands::Hosts => services::print(&deepseek_code_core::mcp::control::hosts(&services::context(paths)?)?)?,
        McpCommands::Restart {name,host} => {
            let owner=deepseek_code_core::mcp::control::restart(&services::context(paths)?,name,host.as_deref()).await?;
            services::print(&serde_json::json!({"restarted":name,"host":owner}))?;
        }
        McpCommands::List => {
            let mut value = serde_json::to_value(manager.list_servers().await)?;
            config::redact(&mut value);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        McpCommands::Add {
            transport,
            headers,
            name,
            command,
            args,
        } => {
            let transport = transport.as_deref().map(|t| match t { "streamable-http" => "http", "websocket" => "ws", other => other });
            ensure!(matches!(transport, None | Some("stdio" | "http" | "sse" | "ws")), "--transport must be stdio, http, sse or ws");
            let remote = matches!(transport, Some("http" | "sse" | "ws"));
            ensure!(!remote || args.is_empty(), "Remote MCP servers take a URL, not command arguments");
            ensure!(remote || headers.is_empty(), "--header applies to http, sse and ws servers");
            let headers = headers.iter().map(|header| header.split_once(':').map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned())).filter(|(key, _)| !key.is_empty()).with_context(|| format!("Header must look like \"Name: value\", not {header}"))).collect::<Result<HashMap<_, _>>>()?;
            ensure!(
                manager.get_server(name).await.is_none(),
                "MCP server already exists"
            );
            manager
                .add_server(McpServerConfig {
                    id: name.clone(),
                    name: name.clone(),
                    command: if remote { String::new() } else { command.clone() },
                    args: args.clone(),
                    env: HashMap::new(),
                    cwd: None,
                    enabled: true,
                    transport: transport.filter(|_| remote).map(str::to_owned),
                    url: remote.then(|| command.clone()),
                    headers,
                })
                .await?;
            eprintln!("Added {name}");
        }
        McpCommands::Remove { name } => {
            if manager.get_server(name).await.is_none() {
                bail!("MCP server not found");
            }
            manager.remove_server(name).await?;
            eprintln!("Removed {name}");
        }
    }
    manager.shutdown_all().await
}

/// How long a non-terminal stdin may stay silent before it is treated as empty.
const STDIN_FIRST_BYTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Reads piped stdin without hanging when the parent keeps the pipe open but never writes.
/// Once the first byte arrives, the rest is read to EOF without a deadline.
fn read_piped_stdin<R: Read + Send + 'static>(
    mut reader: R,
    first_byte_timeout: std::time::Duration,
) -> Result<String> {
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<io::Result<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let mut chunk = [0u8; 8192];
        let result = loop {
            match reader.read(&mut chunk) {
                Ok(0) => break Ok(data),
                Ok(n) => {
                    if data.is_empty() {
                        let _ = started_tx.send(());
                    }
                    data.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        let _ = done_tx.send(result);
    });
    match started_rx.recv_timeout(first_byte_timeout) {
        // Data arrived, or the reader finished (EOF/error) before writing anything.
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!(
                "Warning: no stdin data received in {}s, proceeding without it; redirect stdin from /dev/null to skip this wait",
                first_byte_timeout.as_secs()
            );
            return Ok(String::new());
        }
    }
    let bytes = done_rx.recv().context("stdin reader exited")??;
    String::from_utf8(bytes).context("stdin is not valid UTF-8")
}

#[cfg(test)]
mod piped_stdin_tests {
    use super::*;
    use std::time::Duration;

    struct Silent;
    impl Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    struct Delayed(Option<Vec<u8>>, Duration);
    impl Read for Delayed {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.take() {
                Some(data) => {
                    buf[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                None => {
                    // EOF arrives well after the first-byte deadline.
                    std::thread::sleep(self.1);
                    Ok(0)
                }
            }
        }
    }

    #[test]
    fn silent_open_pipe_times_out_as_empty() {
        let started = std::time::Instant::now();
        assert_eq!(read_piped_stdin(Silent, Duration::from_millis(50)).unwrap(), "");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn data_and_eof_are_read_fully() {
        let input = io::Cursor::new(b"hello\nworld".to_vec());
        assert_eq!(read_piped_stdin(input, Duration::from_secs(1)).unwrap(), "hello\nworld");
        assert_eq!(read_piped_stdin(io::empty(), Duration::from_secs(1)).unwrap(), "");
    }

    #[test]
    fn slow_body_after_first_byte_has_no_deadline() {
        let input = Delayed(Some(b"late".to_vec()), Duration::from_millis(300));
        assert_eq!(read_piped_stdin(input, Duration::from_millis(50)).unwrap(), "late");
    }
}
