#[cfg(test)]
#[path = "../../core/tests/support/session_sql.rs"]
mod fixture_sql;
#[macro_use]
mod terminal_output;
mod request_attempts;
mod signals;
mod session_export;
// Local command output shares the terminal's display-only history.
macro_rules! command_eprintln {
    ($($arg:tt)*) => {{
        use std::io::IsTerminal as _;
        if std::io::stderr().is_terminal() {
            let _ = crate::terminal::print_recorded(&format!("{}\n", format_args!($($arg)*)));
        } else {
            eprintln!($($arg)*);
        }
    }};
}
macro_rules! command_println {
    ($($arg:tt)*) => {{
        use std::io::IsTerminal as _;
        if std::io::stdout().is_terminal() && std::io::stderr().is_terminal() {
            let _ = crate::terminal::print_recorded(&format!("{}\n", format_args!($($arg)*)));
        } else { println!($($arg)*); }
    }};
}
mod account_commands;
mod agent_commands;
mod args;
mod background_commands;
mod batch_commands;
mod command_catalog;
mod compaction;
mod config;
mod context_commands;
mod debug_logging;
mod design_commands;
mod display_commands;
mod doctor_commands;
mod env_init;
mod extended_commands;
mod feedback_commands;
mod help;
mod hook_progress;
mod hooks_menu;
mod ide_commands;
mod images;
mod interactive_config;
mod keybindings;
mod launch_permissions;
#[allow(dead_code)]
mod legacy_api_config;
mod local_coordination;
mod lsp_commands;
mod magic_docs;
mod mcp_menu;
mod mcp_prompts;
mod mcp_states;
mod memory;
mod memory_shortcut;
mod mentions;
mod migration;
mod observe;
mod output;
mod permission_commands;
mod permissions_menu;
mod persistent_worker;
mod plugins;
mod plugins_panel;
mod powerup;
mod prompt_suggestion;
mod provider_panel;
mod remote_client;
mod remote_commands;
mod remote_host;
mod repl;
mod repl_loop;
mod resource_commands;
mod resource_sampling;
mod restart;
mod rewind_commands;
mod sdk;
mod sdk_control;
mod sdk_hooks;
mod sdk_initialization;
mod sdk_input;
mod sdk_mcp;
mod sdk_messages;
mod sdk_operations;
mod sdk_queries;
mod services;
mod session;
mod session_commands;
mod session_trust;
mod settings_dialog;
mod shell_mode;
mod skill_commands;
mod startup_options;
mod startup_prompts;
mod stats_view;
mod status_commands;
mod statusline_commands;
mod super_agent_tick;
mod task_commands;
#[allow(dead_code)]
mod terminal;
mod tool_settings;
mod voice;
mod worker_binding;
use deepseek_code_core::db::session_store::GlobalSetting;
use deepseek_code_core::native_engine::session_binding::store as binding_store;
mod worker_frontend;
mod workflow_commands;
mod workflow_runner;
mod workflows_view;

use anyhow::{bail, ensure, Context, Result};
use args::{Cli, Commands, McpCommands, OutputFormat, SessionCommands, SkillsCommands};
use deepseek_code_core::{
    mcp::{McpServerConfig, McpServerManager},
    runtime::{RuntimePaths, SessionStore},
    skills::SkillsManager,
};
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
};

/// Rebuild the interactive slash list from disk; `/reload-skills` and a
/// successful `/skills state` change both need exactly this.
async fn reload_skill_commands(
    paths: &RuntimePaths,
    workspace: &std::path::Path,
    skills: &mut BTreeMap<String, String>,
    input: &mut Option<terminal::input::TerminalInput>,
) -> Result<usize> {
    let updated = skill_commands::load_for_workspace(paths, workspace).await?;
    *skills = updated;
    input
        .as_mut()
        .context("interactive reader")?
        .set_commands(skill_commands::catalog(skills));
    Ok(skills.len())
}

/// Prints a turn or command error once, after removing an input box a
/// finished turn left on screen for the prompt.
fn report_error(error: anyhow::Error) -> Result<()> {
    if error.is::<terminal_output::StdoutWriteFailed>() {
        return Err(error);
    }
    if !worker_frontend::already_presented(&error) {
        terminal::release_handoff();
        command_eprintln!("Error: {error:#}");
    }
    Ok(())
}

/// Tool calls nest deep futures. Windows gives the main thread a small stack and
/// debug builds overflow it mid-turn, aborting the process (0xc00000fd) before
/// the turn can report anything. Run the runtime on a thread with room.
const MAIN_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> std::process::ExitCode {
    cli_main(Cli::parse_with_mode_source)
}

fn cli_main(parse_cli: fn() -> Cli) -> std::process::ExitCode {
    terminal_output::capture_terminals();
    // Internal managed-PTY roles are separate execs of this executable and must
    // run before the Tokio runtime, argument parsing or any database handle
    // exists. Nothing is printed for a normal start.
    match deepseek_code_core::tools::run_internal_pty_role() {
        Ok(Some(code)) => return std::process::ExitCode::from(code.clamp(0, 255) as u8),
        Ok(None) => {}
        Err(error) => {
            eprintln!("Managed PTY role failed: {error:#}");
            return std::process::ExitCode::FAILURE;
        }
    }
    // The ToolScript owner is a private process mode. It must be dispatched
    // before this binary starts its own runtime: the worker builds one itself,
    // and nesting a runtime inside a runtime would panic.
    #[cfg(not(target_os = "android"))]
    match deepseek_code_core::native_engine::toolscript_job::run_worker_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("ToolScript worker failed: {error:#}");
            return std::process::ExitCode::FAILURE;
        }
    }
    // Only the stack size changes: the runtime keeps its multi-threaded
    // scheduler, and every worker thread gets the same room so a nested tool
    // call cannot overflow any of them.
    let build = || {
        tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(MAIN_STACK_SIZE)
            .thread_name("dcl-worker")
            .enable_all()
            .build()
    };
    match std::thread::Builder::new()
        .name("dcl-main".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(move || match build() {
            Ok(runtime) => runtime.block_on(async_main(parse_cli)),
            Err(error) => {
                eprintln!("Starting the runtime: {error}");
                std::process::ExitCode::FAILURE
            }
        }) {
        Ok(handle) => handle.join().unwrap_or(std::process::ExitCode::FAILURE),
        Err(error) => {
            eprintln!("Starting the main thread: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn async_main(parse_cli: fn() -> Cli) -> std::process::ExitCode {
    #[cfg(windows)]
    let _console_modes = terminal::screen_mode::ConsoleModes::capture();
    // Inside a sandbox with a domain allowlist: carry the proxy, then run the command.
    if let Some(code) = deepseek_code_core::tools::sandbox::run_bridge_from_args() {
        return std::process::ExitCode::from(code.clamp(0, 255) as u8);
    }
    match deepseek_code_core::services::batch::run_worker_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Batch worker failed: {error:#}");
            return std::process::ExitCode::FAILURE;
        }
    }
    match deepseek_code_core::services::background_sessions::run_worker_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Background worker failed: {error:#}");
            return std::process::ExitCode::FAILURE;
        }
    }
    match deepseek_code_core::services::session_worker::run_supervisor_from_env() {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Session supervisor failed: {error:#}");
            return std::process::ExitCode::FAILURE;
        }
    }
    match worker_binding::retire_from_env().await {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => (),
        Err(error) => { eprintln!("Worker retirement failed: {error}"); return std::process::ExitCode::FAILURE; }
    }
    match Box::pin(persistent_worker::run_from_env()).await {
        Ok(true) => return std::process::ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Session host failed: {error:#}");
            return std::process::ExitCode::FAILURE;
        }
    }
    // Register Ctrl-C, SIGTERM and SIGHUP before any turn can start, so a
    // termination signal always reaches the turn's own cancellation instead of
    // killing the process before its owned work is cleaned up. A failed
    // registration stops startup: running with a partial handler set could
    // silently ignore a termination signal.
    if let Err(error) = signals::install() {
        eprintln!("Error: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let mut cli = parse_cli();
    if cli.agent_teams {
        std::env::set_var("DEEPSEEK_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
    }
    cli.normalize();
    terminal::configure_accessibility(cli.ax_screen_reader);
    debug_logging::install(cli.verbose);
    let result = Box::pin(run(&mut cli)).await;
    terminal::release_handoff();
    // A termination request ends the process nonzero even when the interrupted
    // command or prompt swallowed the turn's cancellation error.
    if signals::terminating() {
        if result.is_ok() {
            eprintln!("Interrupted.");
        }
        return std::process::ExitCode::FAILURE;
    }
    if let Err(error) = result {
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

use session::TurnOrigin;

/// A submitted prompt's turn; image paths in it are sent as images.
async fn prompt_turn(
    cli: &Cli,
    session: &mut session::Session,
    prompt: &str,
    input: &mut Option<terminal::input::TerminalInput>,
    origin: TurnOrigin<'_>,
) -> Result<()> {
    terminal::title::name_topic(session, prompt);
    let Some(content) = images::content(&session.workspace, prompt)? else {
        return session.turn(cli, prompt, input, origin).await;
    };
    session.sdk_content = Some(content);
    let result = session.turn(cli, prompt, input, origin).await;
    session.sdk_content = None;
    result
}

/// Commands that submit a model turn or a side model request must wait while
/// another process owns the session.
fn starts_turn_command(command: &str, argument: &str) -> bool {
    match command {
        "/plan" | "/btw" | "/compact" => true,
        "/loop" => {
            let argument = argument.trim();
            !argument.is_empty()
                && argument != "list"
                && argument.split_whitespace().next() != Some("stop")
        }
        "/goal" => !argument.trim().is_empty() && !session_commands::is_goal_clear(argument),
        _ => command.starts_with("/mcp__"),
    }
}

/// Whether this REPL must stay read-only: another owner exists now, or the
/// observer watched a turn that is still finalizing.
fn observing_session(
    session: &session::Session,
    announced: bool,
    state: observe::State,
) -> Result<bool> {
    Ok((announced && state == observe::State::Running)
        || worker_frontend::occupancy(session)?.is_some())
}

async fn run(cli: &mut Cli) -> Result<()> {
    if let Some(Commands::Help { topic, command }) = &cli.command {
        return help::show(topic.as_deref(), command);
    }
    cli.response_schema()?;
    let data_dir = match &cli.data_dir {
        Some(path) => path.clone(),
        None => deepseek_code_core::runtime::resolve_default_data_dir()?,
    };
    let paths = RuntimePaths::new(absolute(data_dir)?)?;
    // Read-only commands must not initialize a data directory the user only
    // pointed at. Export migrates an existing v1 root before reading it.
    if cli.info {
        println!(
            "dcl {}\nOS: {}\nProject: {}\nData: {}\nEngine: deepseek-code-core (direct; no Bridge)",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::current_dir()?.display(),
            paths.data_dir.display()
        );
        return Ok(());
    }
    if let Some(Commands::Sessions {
        action: SessionCommands::Export { id, output },
    }) = &cli.command
    {
        if paths.data_dir.join("global.db").is_file() || paths.data_dir.join("sessions").is_dir() {
            let outcome = paths.layout().migrate_if_needed()?;
            if !outcome.warnings.is_empty() {
                eprintln!(
                    "Data layout warning: unexpected entries remain in {}: {}",
                    paths.data_dir.display(),
                    outcome.warnings.join(", ")
                );
            }
        }
        return session_export::run(&paths.data_dir, id, output.as_deref());
    }
    // The layout must be settled before logs, resource samples or databases are
    // opened: every one of them writes into a category directory.
    let outcome = paths.layout().migrate_if_needed()?;
    if !outcome.warnings.is_empty() {
        eprintln!(
            "Data layout warning: unexpected entries remain in {}: {}",
            paths.data_dir.display(),
            outcome.warnings.join(", ")
        );
    }
    // The coordination root resolves against the launch directory, before `-C`,
    // and the same absolute root is exported for every host child later.
    let launch_cwd = std::env::current_dir().ok();
    // Read-only coordination inspection runs before the resource sampler, logs,
    // environment mutation, provider setup or conversation database.
    if let Some(Commands::Sessions {
        action: SessionCommands::Live { json },
    }) = &cli.command
    {
        let launch_cwd =
            launch_cwd.as_deref().ok_or_else(|| local_coordination::unavailable(None))?;
        let coordination = local_coordination::resolve_paths(cli, launch_cwd)?;
        return local_coordination::show_live(&coordination, *json);
    }
    // Explicit task maintenance runs before any other startup work so it never
    // creates logs, environment state or task storage on its own, and never
    // imports anything automatically.
    if let Some(Commands::Tasks { action }) = &cli.command {
        return task_commands::run(&paths, action);
    }
    if let Some(Commands::Requests { session_id }) = &cli.command {
        print!(
            "{}",
            deepseek_code_core::native_engine::request_recording::list(
                &paths.data_dir,
                session_id
            )?
        );
        return Ok(());
    }
    let _resource_sampler = resource_sampling::start(&paths.data_dir);
    // Engine code that has no RuntimePaths (e.g. the task tools) and child
    // processes resolve the data directory from the environment.
    std::env::set_var(deepseek_code_core::runtime::DATA_DIR_ENV, &paths.data_dir);
    debug_logging::configure_terminal(
        paths
            .layout()
            .logs()
            .join(format!("cli-{}.log", std::process::id())),
    )?;
    if let Some(path) = &cli.path {
        std::env::set_current_dir(path).context("Cannot open project directory")?;
    }
    if cli.debug.is_some() || cli.debug_file.is_some() {
        let log = cli.debug_file.clone().unwrap_or_else(|| {
            paths
                .layout()
                .debug_logs()
                .join(format!("debug-{}.log", uuid::Uuid::new_v4()))
        });
        debug_logging::configure_filtered(log, cli.debug.as_deref().unwrap_or(""))?;
    }
    let mut skill_prompt = None;
    match &cli.command {
        Some(Commands::Requests { .. }) => unreachable!("handled before startup"),
        Some(Commands::Auth { action }) => return account_commands::run(&paths, action).await,
        Some(Commands::Plugin { action }) => return plugins::run(&paths, action).await,
        Some(Commands::Update { url }) => {
            let update =
                deepseek_code_core::services::updates::check(url, env!("CARGO_PKG_VERSION"))
                    .await?;
            return services::print(
                &serde_json::json!({"up_to_date":update.is_none(),"update":update}),
            );
        }
        Some(Commands::StressTest {
            concurrency,
            iterations,
            endpoint,
            allow_remote,
        }) => {
            return tokio::select! {result=deepseek_code_core::services::diagnostics::stress(*concurrency,*iterations,endpoint.clone(),*allow_remote)=>services::print(&result?), _=crate::signals::interrupted()=>anyhow::bail!("Diagnostics cancelled")};
        }
        Some(Commands::Learning { action }) => return migration::learning(&paths, action).await,
        Some(Commands::Performance { action }) => {
            return migration::performance(&paths, action).await
        }
        Some(Commands::Project { action }) => return migration::project(action).await,
        Some(Commands::Cron { action }) => {
            return migration::cron(&paths, &std::env::current_dir()?, action).await
        }
        Some(Commands::WorkflowRun {
            script,
            run_dir,
            args_json,
            resume_dir,
            agent_args,
            concurrency,
        }) => {
            let source = std::fs::read_to_string(script)
                .with_context(|| format!("Reading workflow script {}", script.display()))?;
            let result = workflow_runner::run(workflow_runner::Run {
                script: &source,
                args: args_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or(serde_json::Value::Null),
                run_dir,
                resume_dir: resume_dir.as_deref(),
                cwd: &std::env::current_dir()?,
                agent_args: serde_json::from_str(agent_args)
                    .context("--agent-args must be a JSON array")?,
                concurrency: *concurrency,
            })
            .await;
            // status.json tells /workflows how the run ended.
            let finished = chrono::Utc::now().timestamp_millis();
            return match result {
                Ok(value) => {
                    std::fs::write(
                        run_dir.join("result.json"),
                        serde_json::to_vec_pretty(&value)?,
                    )?;
                    std::fs::write(
                        run_dir.join("status.json"),
                        serde_json::json!({"status":"completed","finished_ms":finished})
                            .to_string(),
                    )?;
                    println!(
                        "Workflow result:\n{}",
                        serde_json::to_string_pretty(&value)?
                    );
                    Ok(())
                }
                Err(error) => {
                    let _ = std::fs::write(run_dir.join("status.json"), serde_json::json!({"status":"failed","error":format!("{error:#}"),"finished_ms":finished}).to_string());
                    Err(error)
                }
            };
        }
        Some(Commands::Ssh {
            config,
            operation,
            yes,
        }) => {
            return tokio::select! { result=migration::ssh(config,operation,*yes)=>result, _=crate::signals::interrupted()=>anyhow::bail!("SSH operation cancelled") }
        }
        Some(Commands::Lsp {
            action,
            language,
            yes,
        }) => {
            let workspace = cli.path.clone().map_or_else(std::env::current_dir, Ok)?;
            return lsp_commands::command(
                &paths,
                &workspace,
                action.as_deref(),
                language.as_deref(),
                *yes,
            )
            .await;
        }
        Some(Commands::InitEnv { target, dry_run }) => {
            let workspace = cli.path.clone().map_or_else(std::env::current_dir, Ok)?;
            return env_init::run(&paths, &workspace, target, *dry_run).await;
        }
        Some(Commands::RemoteControl {
            action: args::RemoteControlCommands::Connect { address, token },
        }) => return remote_client::run(address, token.as_deref()).await,
        Some(Commands::RemoteControl {
            action: args::RemoteControlCommands::Listen { host, port },
        }) => {
            let listening = remote_host::Listening::bind(&remote_host::Options {
                host: *host,
                port: *port,
            })
            .await?;
            // This launcher has no Session row, but its child must still inherit
            // the root prepared from the pre-`-C` launch directory.
            let prepared = local_coordination::prepare_startup(cli, &paths, launch_cwd.as_deref()).await;
            local_coordination::export_child_root(&prepared);
            let workspace = std::env::current_dir()?;
            let launch = restart::fresh(cli, &paths.data_dir, &workspace);
            let code = remote_host::run(listening, launch, &workspace).await?;
            std::process::exit(code);
        }
        Some(Commands::Remote { request, yes }) => {
            return tokio::select! { result=migration::remote(request,*yes)=>result, _=crate::signals::interrupted()=>anyhow::bail!("Remote operation cancelled") }
        }
        Some(Commands::Agents { action }) => {
            let context = services::context(&paths)?;
            match action {
                args::AgentCommands::ImportLegacy {
                    source,
                    id,
                    preview,
                } => {
                    return services::print(&deepseek_code_core::services::agent_import::import(
                        &context, source, id, *preview,
                    )?)
                }
                args::AgentCommands::List => {
                    return services::print(&deepseek_code_core::services::agents::list(&context)?)
                }
                args::AgentCommands::Import { source } => {
                    let definition = serde_json::from_slice(&std::fs::read(source)?)?;
                    deepseek_code_core::services::agents::install(&context, definition)?;
                    return services::print(&serde_json::json!({"imported":true}));
                }
                args::AgentCommands::Model { agent, model } => {
                    println!(
                        "{}",
                        agent_commands::model_command(&paths, agent.as_deref(), model.as_deref())?
                    );
                    return Ok(());
                }
            }
        }
        Some(Commands::Agent { agent_type, prompt }) => {
            deepseek_code_core::services::agents::resolve(&services::context(&paths)?, agent_type)?;
            skill_prompt = Some(prompt.clone());
        }
        Some(Commands::MagicDocs { action }) => return magic_docs::run(cli, &paths, action).await,
        Some(Commands::Memory { action }) => return memory::run(cli, &paths, action).await,
        Some(Commands::VoiceConfig {
            endpoint,
            model,
            credential,
            language,
        }) => {
            let context = services::context(&paths)?;
            let config = deepseek_code_core::services::voice::VoiceConfig {
                endpoint: endpoint.clone(),
                model: model.clone(),
                credential: credential.clone(),
                language: language.clone(),
            };
            config.validate()?;
            context.save_global_setting(GlobalSetting::MachinePreference("voice"), &config)?;
            return services::print(&config);
        }
        Some(Commands::Voice {
            file,
            push_to_talk,
            continuous,
            send,
        }) => {
            let context = services::context(&paths)?;
            // Sending constructs a Session, so the launch decision is prepared
            // and exported before voice can change the current directory.
            let coordination = if *send {
                let prepared = local_coordination::prepare_startup(cli, &paths, launch_cwd.as_deref()).await;
                local_coordination::export_child_root(&prepared);
                Some(prepared)
            } else {
                None
            };
            return voice::run(
                cli,
                &paths,
                &context,
                file,
                *push_to_talk,
                *continuous,
                *send,
                coordination,
            )
            .await;
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
            if let SessionCommands::List = action {
                let rows = paths.list_published_sessions()?;
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            let db = paths.open_database()?;
            let value = match action {
                SessionCommands::List
                | SessionCommands::Export { .. }
                | SessionCommands::Live { .. }
                | SessionCommands::ShutdownWorker { .. }
                | SessionCommands::StopTask { .. } => unreachable!(),
                SessionCommands::Trust { id, folders } => serde_json::to_value(
                    deepseek_code_core::services::session_trust::repair(
                        &db,
                        &paths.data_dir,
                        id,
                        folders,
                    )
                    .map_err(|error| session_trust::repair_error(&paths, id, error))?,
                )?,
                SessionCommands::Compact { .. } => unreachable!(),
                SessionCommands::ImportLegacy { source, preview } => {
                    serde_json::to_value(deepseek_code_core::services::session_import::import(
                        &services::context(&paths)?,
                        source,
                        &std::env::current_dir()?,
                        *preview,
                    )?)?
                }
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
            if let SkillsCommands::ImportLegacy { source, preview } = action {
                return services::print(&deepseek_code_core::skills::import::import_legacy(
                    source,
                    &paths.data_dir.join("skills"),
                    *preview,
                )?);
            }
            let manager = SkillsManager::with_path(paths.data_dir.join("skills"));
            let skills = manager.load_skills().await?;
            match action {
                SkillsCommands::ImportLegacy { .. } => unreachable!(),
                SkillsCommands::List => {
                    let states =
                        deepseek_code_core::skills::states::load(&paths.data_dir);
                    let listed = skills
                        .iter()
                        .map(|skill| {
                            let bundled = skill.source
                                == deepseek_code_core::skills::SkillSource::Bundled;
                            let state = deepseek_code_core::skills::states::effective_in(
                                &states,
                                &skill.name,
                                bundled,
                            );
                            let mut value = serde_json::to_value(skill)?;
                            value["state"] = serde_json::Value::String(state.label().into());
                            // `enabled` follows the effective state, so a built-in
                            // that is off by default never reads as enabled.
                            value["enabled"] = serde_json::Value::Bool(
                                state != deepseek_code_core::skills::states::SkillState::Off,
                            );
                            Ok(value)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    println!("{}", serde_json::to_string_pretty(&listed)?);
                    return Ok(());
                }
                SkillsCommands::State { skill, state } => {
                    let workspace = std::env::current_dir()?;
                    println!(
                        "{}",
                        skill_commands::state_command(
                            &paths,
                            &workspace,
                            skill.as_deref(),
                            state.as_deref()
                        )
                        .await?
                    );
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
                        cli.original_user_input = Some(args.join(" "));
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
    #[cfg(unix)]
    if !single {
        signals::watch_terminal();
    }
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
            if let Some(raw) = &mut cli.original_user_input {
                if !raw.is_empty() {
                    raw.push_str("\n\n");
                }
                raw.push_str(&piped);
            }
        }
    }

    let resumed = if let Some(id) = &cli.resume {
        let store = SessionStore::new(paths.open_database()?);
        Some(
            store
                .resume(id)
                .map_err(|error| session_trust::diagnose(&store.db, &paths, id, error))?,
        )
    } else if cli.continue_session {
        Some(
            SessionStore::new(paths.open_database()?)
                .latest(&std::env::current_dir()?)?
                .context("No resumable Code session in this directory")?,
        )
    } else {
        None
    };
    if let Some(resumed) = &resumed {
        let db = paths.open_database()?;
        let workspace = db.open_session(&resumed.id)?.workspace()?;
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
            deepseek_code_core::services::lifecycle_events::check_configuration(
                &context, &workspace, &id,
            )
            .await?;
            for warning in deepseek_code_core::services::lifecycle_events::setup(
                &context, &workspace, &id, "init", true,
            )
            .await?
            {
                eprintln!("Setup hook: {warning}");
            }
            let mut hooks =
                deepseek_code_core::services::tool_hooks::HookRunner::load_with_plugins(
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
        return tokio::select! { result = hook_progress::during(&id, true, work) => result, _ = crate::signals::interrupted() => anyhow::bail!("Setup hooks interrupted") };
    }
    let _exit_cleanup = terminal::screen_mode::ExitCleanup(
        !single
            && !sdk_stream
            && io::stdin().is_terminal()
            && io::stderr().is_terminal()
            && !terminal::screen_reader(),
    );
    if _exit_cleanup.0 && std::env::var_os(remote_host::HANDOFF_ENV).is_none() {
        terminal::screen_mode::prepare_screen()?;
    }
    let mut input = if single {
        None
    } else {
        Some(session::terminal_input())
    };
    // Observe the selector and its version together, before resolving any configuration.
    let observed = if let Some(saved) = resumed.as_ref() {
        Some(paths.open_database()?.observed_binding(&saved.id)?)
    } else {
        None
    };
    cli.observed_generation = observed.as_ref().map(|row| row.generation);
    if let Some(carried) = cli.binding_generation {
        if Some(carried) != cli.observed_generation { eprintln!("Provider binding changed before relaunch; using the current binding."); }
        std::env::remove_var("API_BASE_URL");
    }
    let normal_frontend = match &resumed {
        Some(saved) => deepseek_code_core::services::background_sessions::worker_directory(&paths.data_dir, &saved.id)?.is_none(),
        None => true,
    };
    if cli.binding_generation.is_some() || (normal_frontend && cli.observes_binding(resumed.is_some())) {
        cli.discard_binding_options();
    }
    let selection = config::select(
        cli,
        &paths,
        observed
            .as_ref()
            .map(|s| s.model.as_str())
            .filter(|s| !s.is_empty()),
        observed
            .as_ref()
            .map(|s| s.provider.as_str())
            .filter(|s| !s.is_empty()),
    );
    let selection = if selection.is_err() && !cli.changes_binding() {
        if let (Some(saved), Some(row)) = (&resumed, &observed) {
            match worker_binding::resident(&paths, &saved.id, row).await? {
                Some(state) => Ok(config::Selection {
                    providers: state.providers,
                    provider_id: state.provider_id,
                    model: state.model,
                }),
                None => selection,
            }
        } else {
            selection
        }
    } else {
        selection
    };

    // Only a successful startup selection consumed the original provider flags.
    let startup_selected = selection.is_ok();
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
        let folder = std::env::current_dir()?;
        startup_prompts::confirm(
            &paths.data_dir,
            &folder,
            matches!(cli.permission_mode, args::PermissionModeArg::Bypass),
            cli.full_access,
        )?;
    }
    deepseek_code_core::permissions::set_full_access(
        cli.full_access && matches!(cli.permission_mode, args::PermissionModeArg::Bypass),
    );
    // One prepared decision covers export, activation and later incarnations.
    // It resolves against the launch directory captured before `-C`, and every
    // known host child inherits that absolute native root.
    let prepared = local_coordination::prepare_startup(cli, &paths, launch_cwd.as_deref()).await;
    local_coordination::export_child_root(&prepared);
    let mut session = session::Session::open(
        cli,
        &paths,
        selection,
        resumed.as_ref().map(|s| s.id.as_str()),
    )
    .await?;
    if normal_frontend && resumed.is_some() && session.startup_provider.is_none() {
        // After observation or a rejected selection, raw startup options have no authority.
        cli.discard_binding_options();
    }
    if !startup_selected { session.startup_provider=None; }
    resource_sampling::session(&session.id);
    let mut screen_mode = terminal::screen_mode::ScreenMode::default();
    // Set by /restart; the replacement starts after this session is closed.
    let mut restart_launch = None;
    let result = async {
        session.admit_launch_prompt(&mut prompt, single)?;
        // Applied inside the cleanup scope so a startup failure reaches teardown
        // and publishes no incarnation.
        startup_options::apply(cli, &mut session).await?;
        if !sdk_stream { session.start_hooks(if resumed.is_some() { "resume" } else { "startup" }).await?; }
        if cli.init || cli.init_only || cli.maintenance {
            anyhow::ensure!(!sdk_stream, "Setup flags with stream-json input are not supported");
            let trigger = if cli.maintenance { "maintenance" } else { "init" };
            let context = session.hook_service_context();
            let outcome = tokio::select! {
                result = hook_progress::during(&session.id, true, deepseek_code_core::services::lifecycle_events::setup(&context, &session.workspace, &session.id, trigger, false)) => result,
                _ = crate::signals::interrupted() => anyhow::bail!("Setup hooks interrupted"),
            }?;
            for warning in outcome { eprintln!("Setup hook: {warning}"); }
        }
        // Startup succeeded and the final workspace is known: publish now, so a
        // failed startup never leaves a registry row or owner lease behind.
        // A one-shot run with nothing to do is rejected before it can publish.
        if !sdk_stream && single {
            anyhow::ensure!(
                session.initial_hook_message.is_some() || !prompt.trim().is_empty(),
                "A prompt or piped stdin is required"
            );
        }
        session.activate_coordination(prepared).await;
        if !single {
            if std::env::var_os(remote_host::HANDOFF_ENV).is_none() {
                let banner = terminal::banner(&session.binding_state().model, &session.workspace);
                eprintln!("{banner}");
                if resumed.is_none() {
                    let mut history = terminal::scrollback::Scrollback::take();
                    history.set_welcome(format!("{banner}\n"));
                    history.save();
                }
            }
            // --resume and --continue show the conversation so far, as in the reference CLI.
            if resumed.is_some() && std::env::var_os(remote_host::HANDOFF_ENV).is_none() { repl::show_resumed(&session, false)?; }
        }
        if !sdk_stream {
            let initial = session.initial_hook_message.take();
            if single {
                if let Some(message) = initial {
                    Box::pin(session.turn_initial_hook(cli, &message, &mut input, prompt.trim().is_empty() || cli.output_format == OutputFormat::StreamJson)).await?;
                }
            }
        }
        if !sdk_stream && !prompt.trim().is_empty() {
            if !single {
                let display = display_commands::load(&session)?;
                terminal::style::configure(&display.theme, &display.color);
                if io::stderr().is_terminal() && !terminal::screen_reader() {
                    let mut history = terminal::scrollback::Scrollback::take();
                    terminal::prompt_display::write_recorded(
                        &mut crate::terminal_output::stderr(), &mut history, &prompt,
                        crate::terminal::size().0.saturating_sub(1) as usize, false,
                    )?;
                    history.save();
                } else {
                    terminal::prompt_display::write_submitted(
                    &mut crate::terminal_output::stderr(), &format!("{} ", terminal::prompt_glyph()), &prompt,
                    crate::terminal::size().0.saturating_sub(1) as usize,
                    io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !terminal::screen_reader(), false,
                )?;
                }
            }
            // As in the reference CLI, /compact also works without a terminal.
            let compact = prompt.trim().strip_prefix("/compact").filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
            match compact {
                Some(instructions) if single => {
                    let compacted = repl::compact(&paths, &mut session, instructions, None).await?;
                    let message = crate::compaction::completion_report(&compacted.report);
                    if cli.output_format == OutputFormat::Text {
                        println!("{message}");
                    } else {
                        session::emit_json(&serde_json::json!({"type":"result","subtype":"success","is_error":false,"session_id":session.id,"result":message,"num_turns":0,"permission_denials":[],"uuid":uuid::Uuid::new_v4().to_string()}))?;
                    }
                }
                _ => prompt_turn(cli, &mut session, &prompt, &mut input, TurnOrigin::command_line(cli, &prompt)).await?,
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
        remote.attach_host().await;
        // As in the reference CLI, remoteControlAtStartup connects every session.
        if remote_commands::at_startup(&paths.data_dir) {
            if let Err(error) = remote.command("", &paths, &session, &mut input).await { command_eprintln!("Remote Control: {error:#}"); }
        }
        let mut last_config_error = String::new();
        let mut reported_binding_failures = std::collections::HashMap::new();
        // Read-only view of a turn a direct print/json owner still executes.
        let mut execution_observer = observe::Observer::default();
        // Cross-session state is reset when /resume, /branch or /clear changes
        // session.id; observation state follows the same boundary.
        let mut active_session = session.id.clone();
        let mut observe_announced = false;
        let mut observe_state = observe::State::Idle;
        let mut last_occupancy_error = String::new();
        terminal::tips::start_session(&paths.data_dir);
        // Lets other sessions see this one is open while it is idle.
        let mut presence: Option<(String, deepseek_code_core::services::session_messages::PresenceLease)> = None;
        terminal::STARTED.get_or_init(std::time::Instant::now);
        terminal::title::start();
        if let Some(speed) = std::fs::read_to_string(paths.layout().config_file("scroll-speed")).ok().and_then(|text| text.trim().parse::<usize>().ok()).filter(|speed| (1..=20).contains(speed)) {
            terminal::SCROLL_SPEED.store(speed, std::sync::atomic::Ordering::Relaxed);
        }
        'repl: loop {
            if crate::signals::terminating() {
                break 'repl;
            }
            if active_session != session.id {
                active_session = session.id.clone();
                // Remote state, loops and drafts belong to the session that was
                // active when they were created. Never carry them across a
                // /resume, /branch or /clear.
                let was_connected = remote.reset_for_session_change().await;
                session.set_remote_relay(None);
                if was_connected {
                    command_eprintln!("Remote Control disconnected because the session changed; reconnect with /remote-control.");
                }
                if !loops.is_empty() {
                    loops.clear();
                    command_eprintln!("/loop prompts from the previous session were cleared.");
                }
                let had_draft = composer.is_pending();
                composer.cancel();
                let had_queued = input.as_mut().is_some_and(|input| input.discard_queued());
                let restored = terminal::NEXT_DRAFT
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                    .is_some();
                if had_draft || had_queued || restored {
                    command_eprintln!("Draft or queued prompts from the previous session were discarded.");
                }
                execution_observer = observe::Observer::default();
                observe_announced = false;
                observe_state = observe::State::Idle;
            }
            let occupancy = match worker_frontend::occupancy(&session) {
                Ok(occupancy) => {
                    last_occupancy_error.clear();
                    occupancy
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    if text != last_occupancy_error {
                        command_eprintln!("Session occupancy: {text}");
                        last_occupancy_error = text;
                    }
                    None
                }
            };
            let occupied = occupancy.is_some();
            let observing =
                occupied || (observe_announced && observe_state == observe::State::Running);
            if observing {
                // Keep automatic notifications and remote prompts from opening
                // a turn while another process owns the session.
                session.hold_automatic_wake();
            }
            if observing && !observe_announced {
                observe_announced = true;
                if let Some(occupancy) = occupancy.as_ref() {
                    command_eprintln!(
                        "{}",
                        worker_frontend::observation_banner(&session, occupancy)
                    );
                    if let Err(error) = execution_observer.preannounce(&session) {
                        command_eprintln!("Observation: {error:#}");
                    }
                }
            }
            if !observing && observe_announced {
                observe_announced = false;
                command_eprintln!("✻ The other turn finished; input is enabled. Nothing queued while observing was submitted.");
            }
            // A committed transition may have resolved a root this launch deferred,
            // so children of this iteration inherit the current native value.
            local_coordination::export_session_child_root(session.coordination_child_root());
            resource_sampling::session(&session.id);
            // Router sessions advance their tasks between turns. Normal Agent
            // sessions pay only one setting read.
            if let Err(error) = super_agent_tick::tick(&session).await {
                command_eprintln!("Super Agent: {error:#}");
            }
            terminal::title::apply(&session.id);
            let idle_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut idle_notified = false;
            for notice in background_commands::collect_completed(&session)? { command_eprintln!("{notice}"); }
            // A background command that finished meanwhile starts a turn, as in
            // the reference CLI.
            if last_occupancy_error.is_empty() && !composer.is_pending() && !observing {
                match background_commands::wake_for_notifications(cli, &mut session, &mut input).await {
                    Ok(true) => continue 'repl,
                    Ok(false) => {},
                    Err(error) => report_error(error)?,
                }
            }
            if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !observing {
                if let Some(prompt) = remote.take_deferred() {
                    if let Err(error) = session.turn(cli, &prompt, &mut input, TurnOrigin::User(&prompt)).await { report_error(error)?; }
                }
            }
            if let Err(error) = remote.sync(&session) { command_eprintln!("Remote Control: {error:#}"); }
            if presence.as_ref().is_none_or(|(id, _)| *id != session.id) {
                drop(presence.take());
                presence = session.store.db.acquire_presence(&session.id)
                    .ok()
                    .map(|lease| (session.id.clone(), lease));
            }
            let suggestions_enabled = prompt_suggestion::enabled(&paths.data_dir);
            let reader = input.as_ref().expect("interactive reader");
            let ghost = suggestions_enabled.then(|| prompt_suggestion::Ghost::load(&session)).flatten();
            let mut shown_suggestion = ghost.as_ref().map(|ghost| ghost.saved.clone());
            reader.set_suggestion(ghost);
            skills = skill_commands::load_for_workspace(&paths, &session.workspace).await?;
            if let Err(error) = input.as_mut().unwrap().set_keybindings(&paths.layout().config_file("keybindings.json")) {
                command_eprintln!("Keybindings: {error:#}");
            }
            let display = display_commands::load(&session)?;
            if screen_mode.set(&display.tui)? {
                for row in session.store.history(&session.id)? {
                    let content: String = row["content"].as_str().unwrap_or("").chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).collect();
                    command_eprintln!("\n{}\n{content}", row["role"].as_str().unwrap_or("message"));
                }
            }
            terminal::style::configure(&display.theme, &display.color);
            let mut catalog = skill_commands::catalog(&skills);
            if let Some((_, description)) = catalog.iter_mut().find(|(name, _)| name == "/model") {
                *description = format!("Set the AI model (currently {} · {})", session.binding_state().model, session.binding_state().provider_id);
            }
            input.as_mut().unwrap().set_commands(catalog);
            input
                .as_ref()
                .unwrap()
                .set_provider_ids(interactive_config::provider_ids(&paths));
            input.as_ref().unwrap().set_workspace(&session.workspace);
            input.as_ref().unwrap().set_context_note(session_commands::context_status(&session).await);
            input.as_ref().unwrap().set_background(
                background_commands::running_shells(&paths, &session.id),
                background_commands::running_monitors(&paths, &session.id),
            );
            let (counter_paths, counter_session) = (paths.clone(), session.id.clone());
            input.as_ref().unwrap().set_background_counter(Box::new(move || (
                background_commands::running_shells(&counter_paths, &counter_session),
                background_commands::running_monitors(&counter_paths, &counter_session),
            )));
            input.as_ref().unwrap().set_effort(Some(session_commands::model_label(&session).await));
            let (agent_paths, agent_db, agent_session) = (paths.clone(), session.store.db.clone(), session.id.clone());
            input.as_ref().unwrap().set_agent_rows(Box::new(move || background_commands::running_agents(&agent_paths, &agent_db, &agent_session)));
            let agent_context = session.hook_service_context();
            input.as_ref().unwrap().set_subagents(agent_context.clone(), session.id.clone());
            input.as_ref().unwrap().set_agent_details(Box::new(move |id| background_commands::agent_execution(&agent_context, id)));
            let (todo_paths, todo_session) = (paths.clone(), session.id.clone());
            input.as_ref().unwrap().set_todos(Box::new(move || background_commands::todo_items(&todo_paths, &todo_session)));
            input.as_ref().unwrap().set_vim(terminal::vim::enabled(&paths.data_dir));
            if display.diff_panel && !display.focus {
                match deepseek_code_core::services::worktree_diff::snapshot(&session.workspace, display.diff_staged).await {
                    Ok(diff) => input.as_mut().unwrap().set_panel(vec![(if display.diff_staged { "Staged diff" } else { "Uncommitted changes (untracked files listed by name)" }.into(), diff)], true),
                    Err(error) => { command_eprintln!("Diff: {error:#}"); input.as_mut().unwrap().set_details(vec![]); },
                }
            } else {
                input.as_mut().unwrap().set_details(vec![]);
            }
            input.as_mut().unwrap().set_transcript(if display.focus || display.diff_panel { vec![] } else { repl::transcript(&session)? });
            match display_commands::statusline(&session, &display).await {
                Ok(Some(line)) => command_eprintln!("{line}"),
                Ok(None) => {},
                Err(error) => command_eprintln!("Status line: {error:#}"),
            }
            input.as_ref().expect("interactive reader").set_permission_mode(session.permission_mode());
            let idle_prompt = format!("\n{} ", terminal::prompt_glyph());
            let prompt = if composer.is_pending() {
                "... "
            } else if observing {
                "\n… "
            } else {
                idle_prompt.as_str()
            };
            let (line, scheduled) = 'read: loop { tokio::select! {
                line = input.as_mut().expect("interactive reader").read_prompt(prompt) => break 'read match line {
                    Ok(Some(terminal::input::PromptEvent::Line(line))) => (line, false),
                    Ok(Some(terminal::input::PromptEvent::Scheduled(line))) => (line, true),
                    Ok(Some(terminal::input::PromptEvent::StepEffort(up))) => {
                        if let Err(error) = session_commands::step_effort(&session, up).await {
                            input.as_ref().unwrap().notice(format!("Reasoning effort: {error:#}"));
                        }
                        input.as_ref().unwrap().set_effort(Some(session_commands::model_label(&session).await));
                        continue 'read;
                    },
                    Ok(Some(terminal::input::PromptEvent::CyclePermission)) => {
                        if let Err(error) = session.cycle_permission_mode().await { command_eprintln!("Permission mode: {error:#}"); }
                        continue 'repl;
                    },
                    Err(_) if signals::terminating() => break 'repl,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        if composer.is_pending() {
                            composer.cancel();
                            command_eprintln!("Prompt discarded.");
                            continue 'repl;
                        }
                        break 'repl;
                    },
                    Err(error) => return Err(error.into()),
                    Ok(None) => {
                        #[cfg(unix)]
                        signals::check_terminal_hangup();
                        if composer.is_pending() { command_eprintln!("Unfinished prompt discarded."); }
                        break 'repl;
                    }
                },
                _ = background_refresh.tick() => {
                    let failure = session.store.db.binding_notice(&session.id,&session.binding_requester)?;
                    if let Some((generation, requested, restored, needs_ack)) = failure {
                        let seen = reported_binding_failures.entry(session.id.clone()).or_insert(0);
                        if generation > *seen {
                            let reader = input.as_ref().expect("interactive reader");
                            reader.notice(format!("Provider switch to {requested} did not apply: activation failed. Restored {restored}."));
                            match session.refresh_binding().await {
                                Ok(()) => reader.set_effort(Some(session_commands::model_label(&session).await)),
                                Err(error) => reader.notice(format!("Could not refresh the restored binding: {error:#}")),
                            }
                            *seen = generation;
                        }
                        if needs_ack {
                            session.store.db.acknowledge_binding(&session.id,&session.binding_requester,generation)?;
                        }
                    }
                    if suggestions_enabled {
                        let ghost = prompt_suggestion::Ghost::load(&session);
                        if ghost.as_ref().map(|ghost| &ghost.saved) != shown_suggestion.as_ref() {
                            shown_suggestion = ghost.as_ref().map(|ghost| ghost.saved.clone());
                            input.as_ref().expect("interactive reader").set_suggestion(ghost);
                        }
                    }
                    // Another process can take over while this prompt waits:
                    // stop every automatic source here, then enter the same
                    // read-only view the top of the REPL would show.
                    let occupancy = match worker_frontend::occupancy(&session) {
                        Ok(occupancy) => {
                            last_occupancy_error.clear();
                            occupancy
                        }
                        Err(error) => {
                            let text = format!("{error:#}");
                            if text != last_occupancy_error {
                                input.as_ref().expect("interactive reader").notice(format!("Session occupancy: {text}"));
                                last_occupancy_error = text;
                            }
                            None
                        }
                    };
                    let observing = occupancy.is_some()
                        || (observe_announced && observe_state == observe::State::Running);
                    if observing {
                        // No hook wake, cron, loop, wakeup, agent question or
                        // desktop input may start a turn while another owner
                        // holds the session.
                        session.hold_automatic_wake();
                    }
                    if observing && !observe_announced {
                        observe_announced = true;
                        if let Some(occupancy) = occupancy.as_ref() {
                            let reader = input.as_ref().expect("interactive reader");
                            reader.notice(worker_frontend::observation_banner(&session, occupancy));
                            if let Err(error) = execution_observer.preannounce(&session) {
                                reader.notice(format!("Observation: {error:#}"));
                            }
                        }
                        if input
                            .as_mut()
                            .expect("interactive reader")
                            .yield_prompt()
                            .await?
                        {
                            continue 'repl;
                        }
                    }
                    // While a resumed session is owned elsewhere, show the public
                    // text/tool progress that owner publishes. The observer never
                    // takes the lease, submits input or runs a tool.
                    if session.waiting_for_continue() {
                        match execution_observer.poll(&session) {
                            // Observing never opens the automatic-wake path: a
                            // queued notification waits for explicit input.
                            Ok((state, lines)) => {
                                observe_state = state;
                                if let Some(reader) = input.as_ref() {
                                    for line in lines { reader.notice(line); }
                                }
                                if observe_announced && state != observe::State::Running {
                                    let occupied = worker_frontend::occupancy(&session).ok().flatten().is_some();
                                    if state == observe::State::Finished || !occupied {
                                        observe_announced = false;
                                        observe_state = observe::State::Idle;
                                        if let Some(reader) = input.as_ref() {
                                            reader.notice("✻ The other turn finished; input is enabled. Nothing queued while observing was submitted.".into());
                                        }
                                        // The current prompt still shows the
                                        // read-only marker; redraw it as idle.
                                        if input
                                            .as_mut()
                                            .expect("interactive reader")
                                            .yield_prompt()
                                            .await?
                                        {
                                            continue 'repl;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                if let Some(reader) = input.as_ref() { reader.notice(format!("Observation: {error:#}")); }
                            }
                        }
                    }
                    match deepseek_code_core::services::lifecycle_events::check_configuration(&session.hook_service_context(), &session.workspace, &session.id).await {
                        Ok(()) => last_config_error.clear(),
                        Err(error) => { let text = format!("{error:#}"); if last_config_error != text { input.as_ref().expect("interactive reader").notice(text.clone()); last_config_error = text; } }
                    }
                    if last_config_error.is_empty() && !idle_notified && tokio::time::Instant::now() >= idle_deadline {
                        idle_notified = true;
                        if let Err(error) = deepseek_code_core::services::lifecycle_events::run(&session.hook_service_context(), &session.workspace, &session.id,
                            deepseek_code_core::services::tool_hooks::HookEvent::Notification,
                            serde_json::json!({"message":"DeepSeek Code is waiting for your input","notification_type":"idle_prompt"})).await {
                            input.as_ref().expect("interactive reader").notice(format!("Notification hook: {error:#}"));
                        }
                    }
                    if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !composer.is_pending()
                        && deepseek_code_core::services::async_hooks::has_pending_wake(&paths.data_dir, &session.id)
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        if let Err(error) = session.wake_hooks(cli, &mut input).await {
                            if error.is::<terminal_output::StdoutWriteFailed>() { return Err(error); }
                            command_eprintln!("Hook wake: {error:#}");
                        }
                        continue 'repl;
                    }
                    if let Err(error) = session.sync_workspace() { command_eprintln!("Worktree: {error:#}"); }
                    // Process due jobs once the composer is idle.
                    if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !composer.is_pending() {
                        let scope = deepseek_code_core::tools::cron_jobs::Scope { root: &paths.data_dir, session: &session.id, project: &session.workspace };
                        match deepseek_code_core::tools::cron_jobs::take_due(&scope, chrono::Local::now()) {
                            Ok(due) if !due.is_empty() => {
                                if input.as_mut().expect("interactive reader").yield_prompt().await? {
                                    for prompt in due {
                                        command_eprintln!("\u{23f0} Scheduled: {}", prompt.lines().next().unwrap_or_default());
                                        input.as_mut().expect("interactive reader").queue_scheduled(prompt);
                                    }
                                    continue 'repl;
                                }
                                for prompt in due { session.selected_session()?.enqueue_cli_input(deepseek_code_core::services::input_queue::QueueOrigin::Scheduled, &prompt)?; }
                            }
                            Ok(_) => {}
                            Err(error) => command_eprintln!("Scheduled jobs: {error:#}"),
                        }
                    }
                    // A self-paced loop resumes its scheduled prompt.
                    if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !composer.is_pending() && deepseek_code_core::tools::wakeup::is_due(&paths.data_dir, &session.id, chrono::Utc::now().timestamp())
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        if let Some((prompt, reason)) = deepseek_code_core::tools::wakeup::take_due(&paths.data_dir, &session.id, chrono::Utc::now().timestamp()).unwrap_or(None) {
                            command_eprintln!("\u{21bb} Loop wakeup: {reason}");
                            input.as_mut().expect("interactive reader").queue_scheduled(prompt);
                            continue 'repl;
                        }
                    }
                    // Dispatch an interval loop once the composer is idle.
                    if let Some(index) = loops.iter().position(|(_, _, next)| *next <= std::time::Instant::now()) {
                        if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !composer.is_pending() && input.as_mut().expect("interactive reader").yield_prompt().await? {
                            let (every, prompt, next) = &mut loops[index];
                            *next = std::time::Instant::now() + *every;
                            command_eprintln!("\u{21bb} Loop: {prompt}");
                            input.as_mut().expect("interactive reader").queue_scheduled(prompt.clone());
                            continue 'repl;
                        }
                    }
                    // A background agent waiting for approval asks here, as in the
                    // reference CLI, once the input is empty.
                    if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !composer.is_pending()
                        && background_commands::agent_question(&paths, &session).is_some()
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        if let Err(error) = background_commands::answer_agent_question(&paths, &session) { command_eprintln!("Agent question: {error:#}"); }
                        continue 'repl;
                    }
                    let notices = background_commands::collect_completed(&session)?;
                    // A finished background command starts a turn; its line is
                    // printed first, above that turn.
                    if last_occupancy_error.is_empty() && !session.waiting_for_continue() && !composer.is_pending()
                        && session.store.db.next_desktop_input(&session.id).ok().flatten().is_some()
                        && input.as_mut().expect("interactive reader").yield_prompt().await? {
                        for notice in notices { command_eprintln!("{notice}"); }
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
                            match observing_session(&session, observe_announced, observe_state) {
                                Ok(true) => {
                                    command_eprintln!("Remote prompt not submitted: this session is executing in another process. It was not queued.");
                                }
                                Ok(false) => {
                                    session.allow_automatic_wake();
                                    if !input.as_mut().expect("interactive reader").yield_prompt().await? {
                                        command_eprintln!("Remote prompt queued until the current input is submitted.");
                                        remote.defer(prompt);
                                    } else if composer.is_pending() {
                                        command_eprintln!("Remote prompt queued until the multiline prompt is finished.");
                                        remote.defer(prompt);
                                    } else {
                                        command_eprintln!("Remote prompt: {}", prompt.lines().next().unwrap_or_default());
                                        remote.defer(prompt);
                                        continue 'repl;
                                    }
                                }
                                Err(error) => command_eprintln!("Remote prompt not submitted: {error:#}"),
                            }
                        },
                        Ok(remote_commands::Action::Refresh) => {
                            if input.as_mut().expect("interactive reader").yield_prompt().await? { continue 'repl; }
                        },
                        Ok(remote_commands::Action::None) => {},
                        Err(error) => command_eprintln!("Remote Control: {error:#}"),
                    }
                    input.as_ref().expect("interactive reader").set_permission_mode(session.permission_mode());
                },
                // Idle Ctrl-C discards a draft and otherwise ends the prompt;
                // Termination, including terminal loss, also stops the prompt.
                _ = tokio::signal::ctrl_c() => {
                    if composer.is_pending() {
                        composer.cancel();
                        command_eprintln!("Prompt discarded.");
                        continue 'repl;
                    }
                    break 'repl;
                },
                _ = crate::signals::termination() => break 'repl,
            } };
            fn input_origin(scheduled: bool, text: &str) -> TurnOrigin<'_> {
                if scheduled { TurnOrigin::Scheduled } else { TurnOrigin::User(text) }
            }
            let submission = if scheduled { terminal::prompt::Input::Command(line) } else { composer.push(line) };
            let line = match submission {
                terminal::prompt::Input::Pending => continue,
                terminal::prompt::Input::Cancelled => { command_eprintln!("Prompt discarded."); continue; },
                terminal::prompt::Input::Prompt(prompt) => {
                    if !prompt.trim().is_empty() {
                        match observing_session(&session, observe_announced, observe_state) {
                            Ok(true) => {
                                command_eprintln!("The session is executing in another process; the prompt was not submitted. Use /continue to return to it, /tasks to inspect it, or wait for it to finish.");
                                if !scheduled {
                                    if let Some(input) = input.as_ref() { input.restore_draft(prompt); }
                                }
                            }
                            Ok(false) => {
                                if let Err(error) = prompt_turn(cli, &mut session, &prompt, &mut input, input_origin(scheduled, &prompt)).await { report_error(error)?; }
                            }
                            Err(error) => command_eprintln!("Session occupancy: {error:#}"),
                        }
                    }
                    continue;
                },
                terminal::prompt::Input::Command(line) => line,
            };
            if line.trim() == "/vim" {
                let on = !terminal::vim::enabled(&paths.data_dir);
                match terminal::vim::set_enabled(&paths.data_dir, on) {
                    Ok(()) => command_eprintln!("{}", if on { "Vim mode on: Esc for normal mode, i/a to insert." } else { "Vim mode off." }),
                    Err(error) => command_eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if let Some(note) = line.trim_start().strip_prefix("# ").filter(|_| !line.contains(['\n', '\r'])) {
                match memory_shortcut::save(&session.workspace, note) {
                    Ok(path) => command_eprintln!("Saved to {}: - {}", path.display(), note.trim()),
                    Err(error) => command_eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if let Some(command) = line.trim_start().strip_prefix('!') {
                if let Err(error) = shell_mode::run(&session, command).await { command_eprintln!("Error: {error:#}"); }
                continue;
            }
            let words: Vec<&str> = line.trim().split_whitespace().collect();
            if words.first() == Some(&"/skills") && words.get(1) == Some(&"state") {
                if words.len() > 4 {
                    command_eprintln!("Usage: /skills state [NAME [STATE]]");
                    continue;
                }
                let skill = words.get(2).copied();
                let state = words.get(3).copied();
                match skill_commands::state_command(&paths, &session.workspace, skill, state).await {
                    Ok(text) => {
                        if let Some(state) = state {
                            match reload_skill_commands(&paths, &session.workspace, &mut skills, &mut input).await {
                                Ok(count) => command_eprintln!("Reloaded: {count} skill commands · {} is now {state}.", skill.unwrap_or_default()),
                                Err(error) => command_eprintln!("Error: {error:#}"),
                            }
                        } else {
                            command_eprintln!("{text}");
                        }
                    }
                    Err(error) => command_eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if let Some(name) = doctor_commands::subcommand(&line) {
                match doctor_commands::run(name, &paths, &session).await {
                    Ok(text) => command_eprintln!("{text}"),
                    Err(error) => command_eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if line.trim() == "/plugins" || line.trim() == "/skill-doctor" {
                let (scope, start) = if line.trim() == "/skill-doctor" {
                    (plugins_panel::Scope::Skills, "skill-doctor")
                } else {
                    (plugins_panel::Scope::Plugins, "installed")
                };
                if terminal::picker::available() {
                    match plugins_panel::run(scope, start, &paths, &mut session, input.as_mut().unwrap()).await {
                        Ok(true) => match reload_skill_commands(&paths, &session.workspace, &mut skills, &mut input).await {
                            Ok(count) => command_eprintln!("Reloaded: {count} skill commands."),
                            Err(error) => command_eprintln!("Error: {error:#}"),
                        },
                        Ok(false) => {},
                        Err(error) => command_eprintln!("Error: {error:#}"),
                    }
                } else {
                    // Scripts and tests get the same Stats text without a terminal.
                    match plugins_panel::load(&paths, &session, scope).await {
                        Ok(model) => command_eprintln!("{}", plugins_panel::stats_rows(scope, &model, chrono::Utc::now(), true).join("\n")),
                        Err(error) => command_eprintln!("Error: {error:#}"),
                    }
                }
                continue;
            }
            if line.trim() == "/skills" {
                if terminal::picker::available() {
                    match plugins_panel::run(plugins_panel::Scope::Skills, "installed", &paths, &mut session, input.as_mut().unwrap()).await {
                        Ok(true) => match reload_skill_commands(&paths, &session.workspace, &mut skills, &mut input).await {
                            Ok(count) => command_eprintln!("Reloaded: {count} skill commands."),
                            Err(error) => command_eprintln!("Error: {error:#}"),
                        },
                        Ok(false) => {},
                        Err(error) => command_eprintln!("Error: {error:#}"),
                    }
                } else {
                    // Scripts keep the full JSON listing.
                    let manager = SkillsManager::with_path(paths.data_dir.join("skills"));
                    match manager.load_skills().await {
                        Ok(listed) => match serde_json::to_string_pretty(&listed) {
                            Ok(text) => command_println!("{text}"),
                            Err(error) => command_eprintln!("Error: {error:#}"),
                        },
                        Err(error) => command_eprintln!("Error: {error:#}"),
                    }
                }
                continue;
            }
            if line.trim() == "/reload-skills" {
                match reload_skill_commands(&paths, &session.workspace, &mut skills, &mut input).await {
                    Ok(count) => command_eprintln!("Loaded {count} skill commands."),
                    Err(error) => command_eprintln!("Cannot reload skills: {error:#}"),
                }
                continue;
            }
            let (command, argument) = line.trim().split_once(char::is_whitespace).unwrap_or((line.trim(), ""));
            let command = command_catalog::canonical(command);
            if starts_turn_command(command, argument) {
                match observing_session(&session, observe_announced, observe_state) {
                    Ok(true) => {
                        command_eprintln!("The session is executing in another process; `{command}` was not submitted. Use /continue to return to it, /tasks to inspect it, or wait for it to finish.");
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        command_eprintln!("Session occupancy: {error:#}");
                        continue;
                    }
                }
            }
            if command == "/powerup" && powerup::interactive_topic(argument).is_some() && powerup::available() {
                if let Err(error) = input.as_mut().unwrap().powerup(paths.clone(), argument.to_owned()).await {
                    command_eprintln!("Power-ups: {error:#}");
                }
                continue;
            }
            match design_commands::handle(cli, command, argument, &paths, &mut session).await {
                Ok(true) => continue,
                Err(error) => { command_eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match batch_commands::handle(cli, command, argument, &paths, &mut session).await {
                Ok(true) => continue,
                Err(error) => { command_eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match background_commands::handle(cli, command, argument, input_origin(scheduled, argument.trim()), &paths, &mut session, &mut input).await {
                Ok(background_commands::Outcome::Handled) => continue,
                Ok(background_commands::Outcome::Exit) => break,
                Err(error) => { report_error(error)?; continue; },
                Ok(background_commands::Outcome::NotHandled) => {},
            }
            if command == "/scroll-speed" {
                // As in the reference CLI: lines per mouse wheel notch, kept.
                let file = paths.layout().config_file("scroll-speed");
                match argument.trim() {
                    "" => command_eprintln!("Scroll speed: {} lines per wheel notch. /scroll-speed 1-20 changes it.", terminal::SCROLL_SPEED.load(std::sync::atomic::Ordering::Relaxed)),
                    value => match value.parse::<usize>().ok().filter(|speed| (1..=20).contains(speed)) {
                        Some(speed) => {
                            terminal::SCROLL_SPEED.store(speed, std::sync::atomic::Ordering::Relaxed);
                            if let Err(error) = deepseek_code_core::data_layout::write_config_file(&file, speed.to_string()) { command_eprintln!("Scroll speed set for this session; not saved: {error}"); }
                            else { command_eprintln!("Scroll speed: {speed} lines per wheel notch."); }
                        }
                        None => command_eprintln!("Usage: /scroll-speed 1-20"),
                    },
                }
                continue;
            }
            if command.starts_with("/mcp__") {
                match mcp_prompts::expand(&session, command, argument).await {
                    Ok(Some(prompt)) => if let Err(error) = session.turn(cli, &prompt, &mut input, TurnOrigin::McpPrompt).await {
                        report_error(error)?;
                    },
                    Ok(None) => {}
                    Err(error) => command_eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if command == "/loop" {
                match repl_loop::run(argument, &mut loops) {
                    Ok(repl_loop::Outcome::Message(message)) => command_eprintln!("{message}"),
                    Ok(repl_loop::Outcome::Run(prompt)) => if let Err(error) = session.turn(cli, &prompt, &mut input, input_origin(scheduled, argument)).await {
                        report_error(error)?;
                    },
                    Err(error) => command_eprintln!("Error: {error:#}"),
                }
                continue;
            }
            if matches!(command, "/remote-control" | "/rc") {
                if let Err(error) = remote.command(argument, &paths, &session, &mut input).await { command_eprintln!("Error: {error:#}"); }
                if let Some(listening) = remote.take_handoff() {
                    if let Err(error) = worker_binding::retain_for_transfer(&session).await {
                        command_eprintln!("Handoff did not apply: {error:#}");
                        continue;
                    }
                    let launch = restart::resume(cli, &session)?;
                    // The session continues in a child dcl that local and remote terminals share.
                    drop(presence.take());
                    // The old OS input reader must be gone before the host owns stdin.
                    drop(input.take());
                    // The relay child inherits this session's current native root.
                    local_coordination::export_session_child_root(session.coordination_child_root());
                    // The child resumes the session; this owner is deliberately
                    // handing it off, so its row closes before the transfer.
                    session.close_coordination().await;
                    let code = remote_host::run(listening, launch, &session.workspace).await?;
                    std::process::exit(code);
                }
                continue;
            }
            if command == "/plan" {
                match extended_commands::plan_task(argument, &paths, &mut session).await {
                    Ok(Some(task)) => if let Err(error) = session.turn(cli, &task, &mut input, input_origin(scheduled, &task)).await {
                        report_error(error)?;
                    },
                    Err(error) => command_eprintln!("Error: {error:#}"),
                    Ok(None) => {},
                }
                continue;
            }
            if command == "/goal" {
                match session_commands::handle(command, argument, &paths, &mut session).await {
                    Ok(_) if !argument.trim().is_empty() && !session_commands::is_goal_clear(argument) => {
                        if let Err(error) = session.turn(cli, argument.trim(), &mut input, input_origin(scheduled, argument.trim())).await {
                            report_error(error)?;
                        }
                    },
                    Err(error) => command_eprintln!("Error: {error:#}"),
                    _ => {},
                }
                continue;
            }
            match lsp_commands::handle(line.trim(), &session, input.as_mut().unwrap()).await {
                Ok(true) => continue,
                Err(error) => { command_eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match interactive_config::handle(line.trim(), &paths, &mut session, input.as_mut().unwrap()).await {
                Ok(true) => {
                    if let Some(input) = &input {
                        input.set_provider_ids(interactive_config::provider_ids(&paths));
                    }
                    continue;
                }
                Err(error) => { command_eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            match repl::handle(&line, &paths, &mut session, input.as_mut()).await {
                Ok(true) => {
                    // A command that switched sessions must let the next 'repl
                    // iteration discard the previous session's draft first.
                    if active_session == session.id {
                        if let Some(draft) = terminal::NEXT_DRAFT.lock().unwrap_or_else(|error| error.into_inner()).take() {
                            if let Some(input) = &input { input.restore_draft(draft); }
                        }
                    }
                    continue;
                },
                Err(error) => { command_eprintln!("Error: {error:#}"); continue; },
                Ok(false) => {},
            }
            // Match the exact command so a skill such as /restart-notes still runs.
            let restart_line = line.trim();
            if restart_line == "/restart" || restart_line.starts_with("/restart ") {
                if restart_line != "/restart" {
                    command_eprintln!("Usage: /restart");
                    continue;
                }
                if let Err(error) = worker_binding::retain_for_transfer(&session).await { command_eprintln!("Relaunch did not apply: {error:#}"); continue; }
                let launch = restart::resume(cli, &session)?;
                restart_launch = Some(launch);
                break;
            }
            match line.trim() {
                "" => continue,
                "/exit" | "/quit" | "exit" | "quit" | ".exit" | ":q" => {
                    session_trust::quit_worker(&session.paths, &session.id).await?;
                    break;
                },
                "/" | "/help" | "help" | ".help" | ":h" => {
                    let mut text = command_catalog::help();
                    for (name, description) in &skills { text.push_str(&format!("\n  {name:<24} {description}")); }
                    if terminal::picker::available() {
                        let lines: Vec<_> = text.lines().map(terminal::change_view::body).collect();
                        terminal::transcript_pager::view_help(&lines)?;
                    } else {
                        command_eprintln!("{text}");
                    }
                },
                "status" | ".status" => { if let Err(error) = status_commands::run(&session, "").await { command_eprintln!("Status: {error:#}"); } },
                "/history" | "history" | ".history" => command_println!("{}", serde_json::to_string_pretty(&session.store.history(&session.id)?)?),
                "/tools" | "tools" | ".tools" => {
                    let result = if terminal::picker::available() {
                        plugins_panel::run(
                            plugins_panel::Scope::Tools,
                            "built-in",
                            &paths,
                            &mut session,
                            input.as_mut().unwrap(),
                        )
                        .await
                        .map(|_| ())
                    } else {
                        tool_settings::print(&paths)
                    };
                    if let Err(error) = result { command_eprintln!("Tools: {error:#}"); }
                },
                command if skills.contains_key(command.split_whitespace().next().unwrap_or(command)) => {
                    match observing_session(&session, observe_announced, observe_state) {
                        Ok(true) => command_eprintln!("The session is executing in another process; the skill prompt was not submitted. Use /continue to return to it, /tasks to inspect it, or wait for it to finish."),
                        Ok(false) => if let Err(error) = session.turn(cli, &line, &mut input, input_origin(scheduled, &line)).await { report_error(error)?; },
                        Err(error) => command_eprintln!("Session occupancy: {error:#}"),
                    }
                },
                command if command.starts_with('/') => command_eprintln!("Unknown command: {}. Use /help; use /multiline to send text starting with /.", command.split_whitespace().next().unwrap_or(command)),
                _ => match observing_session(&session, observe_announced, observe_state) {
                    Ok(true) => command_eprintln!("The session is executing in another process; the prompt was not submitted. Use /continue to return to it, /tasks to inspect it, or wait for it to finish."),
                    Ok(false) => if let Err(error) = prompt_turn(cli, &mut session, &line, &mut input, input_origin(scheduled, &line)).await { report_error(error)?; },
                    Err(error) => command_eprintln!("Session occupancy: {error:#}"),
                },
            }
        }
        if remote.is_connected() {
            if let Err(error) = remote.disconnect().await { command_eprintln!("Remote Control: {error:#}"); }
        }
        Ok(())
    }.await;
    if !single {
        terminal::title::clear();
    }
    session
        .end_hooks(if result.is_ok() && !single && !signals::terminating() {
            "prompt_input_exit"
        } else {
            "other"
        })
        .await;
    let resume_hint = restart_launch.is_none()
        && !single
        && !sdk_stream
        && session
            .store
            .history(&session.id)
            .is_ok_and(|rows| !rows.is_empty());
    let shutdown = session.close_owned().await;
    // Restore the shell before printing the one message that must survive cleanup.
    drop(input);
    drop(screen_mode);
    drop(_exit_cleanup);
    if resume_hint {
        eprintln!(
            "\nResume this session with:\ndcl --resume {}",
            session.id
        );
    }
    if let Some(launch) = restart_launch.take() {
        // The replacement inherits the session's current native root.
        local_coordination::export_session_child_root(session.coordination_child_root());
        // Release the session and its database before the replacement opens it.
        shutdown?;
        drop(session);
        eprintln!("Restarting dcl in this conversation\u{2026}");
        return restart::relaunch(launch);
    }
    // A completed single turn already wrote its terminal result. Shutdown may
    // still fail, but stdout must not acquire a second result record.
    if single && cli.output_format != OutputFormat::Text && result.is_ok() {
        if let Err(error) = shutdown {
            return Err(
                output::ReportedError(format!("Session shutdown failed: {error:#}")).into(),
            );
        }
        let pending = session.pending_suggestion.lock().unwrap().take();
        if let Some(pending) = pending {
            if deepseek_code_core::services::background_sessions::finish_before_suggestion(
                &session.paths.data_dir,
                &session.id,
            )
            .is_ok()
            {
                let _ = pending.generate().await;
            }
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
        McpCommands::Hosts => services::print(&deepseek_code_core::mcp::control::hosts(
            &services::context(paths)?,
        )?)?,
        McpCommands::Restart { name, host } => {
            let owner = deepseek_code_core::mcp::control::restart(
                &services::context(paths)?,
                name,
                host.as_deref(),
            )
            .await?;
            services::print(&serde_json::json!({"restarted":name,"host":owner}))?;
        }
        McpCommands::List => {
            let mut value = serde_json::to_value(manager.list_servers().await)?;
            config::redact(&mut value);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        McpCommands::State { server, state } => {
            let text =
                mcp_states::state_command(paths, &manager, server.as_deref(), state.as_deref())
                    .await?;
            println!("{text}");
        }
        McpCommands::Add {
            transport,
            headers,
            name,
            command,
            args,
        } => {
            let transport = transport.as_deref().map(|t| match t {
                "streamable-http" => "http",
                "websocket" => "ws",
                other => other,
            });
            ensure!(
                matches!(transport, None | Some("stdio" | "http" | "sse" | "ws")),
                "--transport must be stdio, http, sse or ws"
            );
            let remote = matches!(transport, Some("http" | "sse" | "ws"));
            ensure!(
                !remote || args.is_empty(),
                "Remote MCP servers take a URL, not command arguments"
            );
            ensure!(
                remote || headers.is_empty(),
                "--header applies to http, sse and ws servers"
            );
            let headers = headers
                .iter()
                .map(|header| {
                    header
                        .split_once(':')
                        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
                        .filter(|(key, _)| !key.is_empty())
                        .with_context(|| {
                            format!("Header must look like \"Name: value\", not {header}")
                        })
                })
                .collect::<Result<HashMap<_, _>>>()?;
            ensure!(
                manager.get_server(name).await.is_none(),
                "MCP server already exists"
            );
            manager
                .add_server(McpServerConfig {
                    id: name.clone(),
                    name: name.clone(),
                    command: if remote {
                        String::new()
                    } else {
                        command.clone()
                    },
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
mod turn_command_tests {
    use super::starts_turn_command;

    #[test]
    fn loop_stop_is_recognized_only_as_its_own_first_word() {
        for argument in ["stop", "stop 2", "  stop   now  "] {
            assert!(
                !starts_turn_command("/loop", argument),
                "/loop {argument} does not start a turn"
            );
        }
        for argument in ["", "list"] {
            assert!(
                !starts_turn_command("/loop", argument),
                "/loop {argument} does not start a turn"
            );
        }
        for argument in [
            "stop-watching-build",
            "stopwatch",
            "5m check",
            "watch the build",
            "list extra",
        ] {
            assert!(
                starts_turn_command("/loop", argument),
                "/loop {argument} starts a turn"
            );
        }
    }

    #[test]
    fn only_turn_commands_are_gated_while_occupied() {
        for (command, argument) in [
            ("/plan", ""),
            ("/btw", "question"),
            ("/compact", ""),
            ("/goal", "ship it"),
            ("/mcp__fixture__search", ""),
        ] {
            assert!(starts_turn_command(command, argument), "{command}");
        }
        for (command, argument) in [
            ("/status", ""),
            ("/goal", "clear"),
            ("/loop", "list"),
            ("/clear", ""),
        ] {
            assert!(!starts_turn_command(command, argument), "{command}");
        }
    }
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
        assert_eq!(
            read_piped_stdin(Silent, Duration::from_millis(50)).unwrap(),
            ""
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn data_and_eof_are_read_fully() {
        let input = io::Cursor::new(b"hello\nworld".to_vec());
        assert_eq!(
            read_piped_stdin(input, Duration::from_secs(1)).unwrap(),
            "hello\nworld"
        );
        assert_eq!(
            read_piped_stdin(io::empty(), Duration::from_secs(1)).unwrap(),
            ""
        );
    }

    #[test]
    fn slow_body_after_first_byte_has_no_deadline() {
        let input = Delayed(Some(b"late".to_vec()), Duration::from_millis(300));
        assert_eq!(
            read_piped_stdin(input, Duration::from_millis(50)).unwrap(),
            "late"
        );
    }
}
