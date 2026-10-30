//! CLI engine adapter for the persistent, local-IPC session worker.
use crate::{
    args::{Cli, PermissionModeArg},
    session::Session,
};
use anyhow::{ensure, Context, Result};
use claude_core::{
    runtime::RuntimePaths,
    services::{
        background_sessions as jobs,
        session_worker::{self as service, Reply, Request, Worker},
    },
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
type StatusTarget = Arc<std::sync::Mutex<Option<claude_core::services::runtime_status::RuntimeSource>>>;
struct ModeControl {
    job_id: String,
    session_id: String,
    manager: Arc<claude_core::permissions::PermissionManager>,
    db: Arc<claude_core::db::DbManager>,
    provider: claude_core::native_engine::provider_manager::ResolvedProvider,
    owner_manager: Option<Arc<claude_core::permissions::PermissionManager>>,
    /// The job's launch allows bypass, and Full Access when `full_access`.
    bypass_available: bool,
    full_access: bool,
}
type ModeTarget = Arc<std::sync::Mutex<Option<ModeControl>>>;

#[path = "persistent_child.rs"]
mod child;

pub async fn run_from_env() -> Result<bool> {
    let Some(value) = std::env::var_os(service::HOST_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(service::HOST_ENV);
    let dir = PathBuf::from(value);
    let worker = service::read_worker(&dir)?;
    ensure!(
        service::directory(&worker).canonicalize()? == dir.canonicalize()?,
        "Invalid session host directory"
    );
    let mut listener = service::Listener::bind(&worker)?;
    let session = Arc::new(tokio::sync::Mutex::new(None::<Session>));
    let mode_target: ModeTarget = Arc::new(std::sync::Mutex::new(None));
    let status_target: StatusTarget = Arc::new(std::sync::Mutex::new(None));
    let active = Arc::new(AtomicBool::new(false));
    let idle_child_active = Arc::new(AtomicBool::new(false));
    let fatal = Arc::new(AtomicBool::new(false));
    let mut idle = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _=tick.tick()=>{
                if service::read_worker(&dir).map_or(true, |owner| owner.status == "stopped" || chrono::Utc::now().timestamp_millis().max(0) as u64 > owner.heartbeat_ms.saturating_add(10_000)) {
                    fatal.store(true,Ordering::Release);
                    if let Ok(owner)=service::read_worker(&dir) { if let Some(id)=owner.active_job { let _=jobs::cancel(&worker.root,&id); } }
                }
                let turn_active = active.load(Ordering::Acquire);
                let shell_active = claude_core::tools::background_shell::has_active();
                if should_expire(turn_active, shell_active, fatal.load(Ordering::Acquire), idle.elapsed()) {break;}
                if (turn_active && !idle_child_active.load(Ordering::Acquire)) || shell_active {idle=Instant::now();}
                if !turn_active && !fatal.load(Ordering::Acquire) {
                    if let Err(error) = child::start_pending(&worker, &session, &active, &idle_child_active, &fatal, &mode_target).await {
                        eprintln!("Owned child wake unavailable: {error:#}");
                    }
                }
            },
            stream=listener.accept()=>{
                let stream=stream?;
                let parsed=tokio::time::timeout(Duration::from_secs(5),service::read_request(stream)).await;
                let Ok(Ok((stream,request)))=parsed else{continue};
                if matches!(&request,Request::Submit{..}|Request::Shutdown) {
                        // The supervisor publishes completion just before the host
                        // acknowledges it. Briefly drain that handoff, never queue
                        // a request behind a genuinely running turn.
                        if active.load(Ordering::Acquire) {
                            let finished=std::fs::read(dir.join("active.json")).ok()
                                .and_then(|bytes|serde_json::from_slice::<String>(&bytes).ok())
                                .and_then(|id|jobs::get(&worker.root,&id).ok())
                                .is_some_and(|job|job.finished_ms.is_some());
                            if finished {for _ in 0..25 {if !active.load(Ordering::Acquire){break;}tokio::time::sleep(Duration::from_millis(10)).await;}}
                        }
                }
                match request {
                    Request::RuntimeStatus => {
                        let source = status_target.lock().unwrap_or_else(|error|error.into_inner()).clone().unwrap_or_else(||claude_core::services::runtime_status::RuntimeSource {
                            mcp: Arc::new(claude_core::mcp::McpServerManager::new(worker.root.join("mcp-servers.json"))),
                            mcp_ready: Arc::new(tokio::sync::OnceCell::new()), workspace: worker.workspace.clone(), data_dir:worker.root.clone(),
                        });
                        let response = match tokio::time::timeout(Duration::from_secs(1), source.snapshot()).await {
                            Ok(snapshot) => { let mut response=Reply::ok(None); response.runtime_status=Some(snapshot); response },
                            Err(_) => Reply::error("Runtime status inspection timed out"),
                        };
                        let _ = service::reply(stream, &response).await;
                    },
                    Request::Ping=>{let _=service::reply(stream,&Reply::ok(None)).await;},
                    Request::SetPermissionMode { job_id, mode } => {
                        let result = update_live_mode(&worker, &mode_target, &job_id, &mode);
                        let reply = match result { Ok(session_id) => { let mut reply=Reply::ok(Some(job_id)); reply.permission_session_id=Some(session_id); reply }, Err(error) => Reply::error(error) };
                        let _ = service::reply(stream, &reply).await;
                    },
                    Request::GetPermissionMode { job_id } => {
                        let result = current_live_mode(&worker, &mode_target, &job_id);
                        let reply = match result { Ok(mut reply) => { reply.job_id=Some(job_id); reply }, Err(error)=>Reply::error(error) };
                        let _ = service::reply(stream, &reply).await;
                    },
                    Request::Shutdown=>{
                        if active.load(Ordering::Acquire) {let _=service::reply(stream,&Reply::error("Session worker has an active turn; cancel or detach it explicitly")).await;}
                        else if claude_core::tools::background_shell::has_active() {let _=service::reply(stream,&Reply::error("Session worker owns running background shell tasks; stop them explicitly before shutdown")).await;}
                        else {let _=service::reply(stream,&Reply::ok(None)).await;break;}
                    },
                    Request::Submit{id,prompt,arguments,environment}=>{
                        if fatal.load(Ordering::Acquire) {let _=service::reply(stream,&Reply::error("Session worker is shutting down; prompt was not submitted")).await;continue;}
                        if active.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() {
                            let _=service::reply(stream,&Reply::error("Session worker is busy; prompt was not submitted")).await;continue;
                        }
                        if let Err(error)=service::begin_job(&worker,&id,&prompt) {
                            active.store(false,Ordering::Release);let _=service::reply(stream,&Reply::error(error)).await;continue;
                        }
                        let worker=worker.clone();let sessions=session.clone();let active=active.clone();let fatal=fatal.clone();let turn_id=id.clone();let mode_target=mode_target.clone();let status_target=status_target.clone();
                        tokio::spawn(async move {
                            let task_worker=worker.clone();let task_id=turn_id.clone();let task_mode_target=mode_target.clone();
                            let result=tokio::spawn(async move {execute(&task_worker,&task_id,arguments,environment,sessions,task_mode_target,status_target).await}).await;
                            let error=match result {Ok(result)=>result.err().map(|error|format!("{error:#}")),Err(error)=>{fatal.store(true,Ordering::Release);Some(format!("Session turn panicked: {error}"))}};
                            if let Some(error)=&error {let _=service::emit_event(&serde_json::json!({"type":"error","error":error}));}
                            if service::finish_job(&worker,&turn_id,error).await.is_err() {fatal.store(true,Ordering::Release);}
                            *mode_target.lock().unwrap_or_else(|e|e.into_inner()) = None;
                            service::set_event_sink(None);
                            std::env::remove_var(jobs::CONTROL_ENV);
                            active.store(false,Ordering::Release);
                        });
                        // A lost acknowledgement never causes replay: the job ID is durable.
                        let _=service::reply(stream,&Reply::ok(Some(id))).await;
                        idle=Instant::now();
                    },
                }
            }
        }
    }
    if let Some(session) = session.lock().await.take() {
        session.close().await?;
    }
    Ok(true)
}

async fn execute(
    worker: &Worker,
    id: &str,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    sessions: Arc<tokio::sync::Mutex<Option<Session>>>,
    mode_target: ModeTarget,
    status_target: StatusTarget,
) -> Result<()> {
    let dir = worker.root.join("background-tasks").join(id);
    service::set_event_sink(Some(dir.join("output.jsonl")));
    std::env::set_var(jobs::CONTROL_ENV, &dir);
    // These are the only settings accepted from the same-user IPC frontend.
    for name in [
        "CLAUDE_RUST_CHILD_API_KEY",
        "CLAUDE_RUST_WORKER_AUTO_ROUTE",
        "CLAUDE_RUST_WORKER_MODEL",
    ] {
        std::env::remove_var(name);
    }
    for (name, value) in environment {
        ensure!(
            [
                "CLAUDE_RUST_CHILD_API_KEY",
                "CLAUDE_RUST_WORKER_AUTO_ROUTE",
                "CLAUDE_RUST_WORKER_MODEL"
            ]
            .contains(&name.as_str()),
            "Unsupported worker environment setting"
        );
        std::env::set_var(name, value);
    }
    let mut cli = Cli::try_parse_with_mode_source(std::iter::once("dcl".to_owned()).chain(arguments))?;
    cli.normalize();
    ensure!(
        cli.resume.as_deref() == Some(worker.session_id.as_str()) && cli.command.is_none(),
        "Worker request must resume its own session without a subcommand"
    );
    let paths = RuntimePaths::new(worker.root.clone())?;
    let mut selection =
        crate::config::select(&cli, &paths, cli.model.as_deref(), cli.provider.as_deref())?;
    if let Ok(value) = std::env::var("CLAUDE_RUST_WORKER_MODEL") {
        let model: claude_core::native_engine::provider_manager::ModelConfig =
            serde_json::from_str(&value)?;
        let provider = selection
            .providers
            .iter_mut()
            .find(|provider| provider.id == selection.provider_id)
            .context("Worker provider missing")?;
        let expected=claude_core::native_engine::provider_manager::resolve_model_selection(&provider.models,&selection.model).context("Worker selected model missing")?;
        ensure!(model.id==expected.id,"Worker model snapshot mismatch");
        let target = provider
            .models
            .iter_mut()
            .find(|item| item.id == model.id)
            .context("Worker model missing")?;
        *target = model;
    }
    let mut current = sessions.lock().await;
    if let Some(session) = current.as_mut() {
        session.switch(&paths, selection).await?;
        // Each job carries its own launch permissions.
        session.bypass_available = cli.allow_dangerously_skip_permissions;
        session.full_access = cli.full_access;
        session.auto_route =
            std::env::var("CLAUDE_RUST_WORKER_AUTO_ROUTE").as_deref() == Ok("true");
        session
            .set_permission_mode(match cli.permission_mode {
                PermissionModeArg::Ask => "ask",
                PermissionModeArg::AcceptEdits => "accept-edits",
                PermissionModeArg::Plan => "plan",
                PermissionModeArg::Bypass => "bypass",
                PermissionModeArg::DontAsk => "dont-ask",
                PermissionModeArg::Auto => "auto",
            })
            .await?;
    } else {
        *current = Some(Session::open(&cli, &paths, selection, Some(&worker.session_id)).await?);
    }
    let prompt = cli.prompt.as_deref().context("Worker prompt missing")?;
    let session = current.as_ref().unwrap();
    session.engine.set_owned_child_idle_ready(&session.id, false);
    *status_target.lock().unwrap_or_else(|error|error.into_inner()) = Some(session.runtime_status_source());
    let current_provider = session.selected_provider().await?;
    *mode_target.lock().unwrap_or_else(|e|e.into_inner()) = Some(ModeControl { job_id:id.into(), session_id:session.id.clone(), manager:session.permissions.clone(), db:session.store.db.clone(), provider:current_provider, owner_manager:None, bypass_available:session.bypass_available, full_access:session.full_access });
    claude_core::permissions::set_full_access(session.full_access && session.permissions.get_mode() == claude_core::permissions::PermissionMode::BypassPermissions);
    let result = current
        .as_ref()
        .unwrap()
        .turn(&cli, &prompt, &mut None)
        .await;
    *session
        .hook_wake_cli
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(cli));
    session.engine.set_owned_child_idle_ready(&session.id, result.is_ok());
    let _mode_guard = mode_target.lock().unwrap_or_else(|e|e.into_inner());
    claude_core::services::session_permission_mode::set(&session.store.db, &session.id, &session.permissions, session.permissions.get_mode())?;
    result
}

fn update_live_mode(worker: &Worker, target: &ModeTarget, job_id: &str, mode: &str) -> Result<String> {
    let target = target.lock().unwrap_or_else(|e|e.into_inner());
    let control = target.as_ref().context("Session worker has no active initialized turn")?;
    let bypass = claude_core::services::session_permission_mode::parse(mode)? == claude_core::permissions::PermissionMode::BypassPermissions;
    ensure!(!bypass || control.bypass_available, "Live switching cannot enable bypass permissions unless the launch allows it");
    ensure!(control.job_id == job_id, "Permission change targets a different job");
    if control.owner_manager.as_ref().is_some_and(|manager| manager.get_mode() == claude_core::permissions::PermissionMode::PlanMode) {
        ensure!(claude_core::services::session_permission_mode::parse(mode)? == claude_core::permissions::PermissionMode::PlanMode, "Owner remains in plan mode; child mode cannot expand its authority");
    }
    let job = jobs::get(&worker.root, job_id)?;
    ensure!(job.session_id == worker.session_id && job.finished_ms.is_none(), "Permission change targets an inactive session job");
    if claude_core::services::session_permission_mode::parse(mode)? == claude_core::permissions::PermissionMode::Auto {
        claude_core::native_engine::auto_permissions::configured(&crate::services::context(&RuntimePaths::new(worker.root.clone())?)?, &control.provider)?;
    }
    claude_core::services::session_permission_mode::set(&control.db, &control.session_id, &control.manager, claude_core::services::session_permission_mode::parse(mode)?)?;
    claude_core::permissions::set_full_access(control.full_access && bypass);
    let _ = service::emit_event(&serde_json::json!({"type":"permission_mode","mode":mode,"session_id":control.session_id}));
    Ok(control.session_id.clone())
}

fn current_live_mode(worker: &Worker, target: &ModeTarget, job_id: &str) -> Result<Reply> {
    let target = target.lock().unwrap_or_else(|e| e.into_inner());
    let control = target
        .as_ref()
        .context("Session worker has no active initialized turn")?;
    ensure!(
        control.job_id == job_id,
        "Mode query targets a different job"
    );
    let job = jobs::get(&worker.root, job_id)?;
    ensure!(
        job.session_id == worker.session_id && job.finished_ms.is_none(),
        "Mode query targets an inactive session job"
    );
    let mut reply = Reply::ok(Some(job_id.into()));
    reply.permission_mode = Some(
        claude_core::services::session_permission_mode::name(control.manager.get_mode()).into(),
    );
    reply.permission_session_id = Some(control.session_id.clone());
    reply.launch_rules = Some(control.manager.launch_rules(&control.session_id));
    Ok(reply)
}

fn should_expire(turn: bool, shell: bool, fatal: bool, idle: Duration) -> bool {
    !turn && (fatal || (!shell && idle > Duration::from_secs(300)))
}
#[cfg(test)]
#[test]
fn idle_expiry_keeps_managed_servers_alive_but_not_fatal_workers() {
    assert!(!should_expire(false, true, false, Duration::from_secs(301)));
    assert!(should_expire(false, false, false, Duration::from_secs(301)));
    assert!(should_expire(false, true, true, Duration::from_secs(301)));
    assert!(!should_expire(false, false, false, Duration::from_secs(299)));
}
