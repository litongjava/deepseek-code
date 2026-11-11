//! CLI engine adapter for the persistent, local-IPC session worker.
use crate::{
    args::{Cli, PermissionModeArg},
    session::Session,
};
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{
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
type StatusTarget = Arc<std::sync::Mutex<Option<deepseek_code_core::services::runtime_status::RuntimeSource>>>;
struct ModeControl {
    job_id: String,
    session_id: String,
    manager: Arc<deepseek_code_core::permissions::PermissionManager>,
    db: Arc<deepseek_code_core::db::DbManager>,
    provider: deepseek_code_core::native_engine::provider_manager::ResolvedProvider,
    owner_manager: Option<Arc<deepseek_code_core::permissions::PermissionManager>>,
    /// The job's launch allows bypass, and Full Access when `full_access`.
    bypass_available: bool,
    full_access: bool,
}
type ModeTarget = Arc<std::sync::Mutex<Option<ModeControl>>>;
type BashTarget = Arc<std::sync::Mutex<Option<Arc<deepseek_code_core::permissions::PermissionManager>>>>;

/// The turn this host admitted, if any. Kept next to the admission flag so a
/// submission can tell "the turn that just finished is still releasing
/// admission" from "a turn is still running". The durable job record alone
/// cannot tell those apart: the supervisor publishes a turn's completion before
/// this host has removed `active.json` and cleared the in-memory flag.
type CurrentTurn = Arc<std::sync::Mutex<Option<String>>>;

/// How long a submission waits for a turn that is already durably finished to
/// release admission. Only bookkeeping remains by then in the normal case, but
/// expiry does **not** prove the host is stuck: scheduling and disk I/O can
/// delay it. A timed-out submission is unsubmitted and is answered with the
/// existing busy error; it is never queued, and no second turn is admitted while
/// admission is still held. Published completion therefore does not by itself
/// guarantee that the next prompt is accepted immediately.
const ADMISSION_GRACE: Duration = Duration::from_secs(2);

/// The admission decision for one submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Ownership is free: the submission may take it.
    Admit,
    /// An admitted turn is still running, or admission was taken and its job is
    /// not recorded yet. Refuse now; never queue behind it.
    Busy,
    /// The admitted turn has finished durably but this host has not released
    /// admission. Wait for the release event, then re-check.
    Finalizing,
    /// A finalizing host did not release within the grace. The prompt stays
    /// unsubmitted and is refused; this is not proof that the host is stuck.
    Expired,
}

/// Decides what a submission should do about the turn this host admitted.
///
/// Only a turn that this host admitted *and* whose durable record proves it
/// finished may be waited for. A running turn - or the window in which
/// admission was taken but its job is not recorded yet - is refused at once, so
/// two turns can never overlap. `active.json` is deliberately not consulted: the
/// supervisor removes it at a different moment than this host releases
/// admission, which is what made the previous fixed-window drain unreliable.
fn admission_state(
    active: bool,
    current: Option<&str>,
    finished: impl FnOnce(&str) -> bool,
) -> Admission {
    if !active {
        Admission::Admit
    } else if current.is_some_and(finished) {
        Admission::Finalizing
    } else {
        Admission::Busy
    }
}

/// Waits for a *provably finished* turn's admission release, bounded by `grace`.
///
/// The host notifies on the actual release, so this waits for the event rather
/// than polling a fixed window. Returning [`Admission::Expired`] means admission
/// is still held: the caller must refuse the prompt, which then stays
/// unsubmitted and is never replayed. Only a free flag yields
/// [`Admission::Admit`].
async fn await_admission(
    active: &AtomicBool,
    current: Option<&str>,
    release: &tokio::sync::Notify,
    finished: impl FnOnce(&str) -> bool,
    grace: Duration,
) -> Admission {
    match admission_state(active.load(Ordering::Acquire), current, finished) {
        Admission::Finalizing => {
            let _ = tokio::time::timeout(grace, async {
                while active.load(Ordering::Acquire) {
                    release.notified().await;
                }
            })
            .await;
            if active.load(Ordering::Acquire) {
                Admission::Expired
            } else {
                Admission::Admit
            }
        }
        settled => settled,
    }
}

#[path = "persistent_child.rs"]
mod child;

pub async fn run_from_env() -> Result<bool> {
    let Some(value) = std::env::var_os(service::HOST_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(service::HOST_ENV);
    let dir = PathBuf::from(value);
    // The supervisor names itself out of band. The worker record is written by
    // the supervisor only after this process starts, so at startup it can still
    // hold no supervisor pid, and later damage to it can hide that pid for good.
    // Without a pid the orphan check below can never fire and this process would
    // keep the session locked after its owner is gone.
    let supervisor_pid = std::env::var(service::SUPERVISOR_PID_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    std::env::remove_var(service::SUPERVISOR_PID_ENV);
    let _lifetime = service::host_lifetime_lock(&dir)?;
    let worker = service::read_worker(&dir)?;
    let _resource_sampler = crate::resource_sampling::start(&worker.root);
    crate::resource_sampling::session(&worker.session_id);
    ensure!(
        service::directory(&worker).canonicalize()? == dir.canonicalize()?,
        "Invalid session host directory"
    );
    let mut listener = service::Listener::bind(&worker)?;
    let session = Arc::new(tokio::sync::Mutex::new(None::<Session>));
    let mode_target: ModeTarget = Arc::new(std::sync::Mutex::new(None));
    let status_target: StatusTarget = Arc::new(std::sync::Mutex::new(None));
    let bash_target: BashTarget = Arc::new(std::sync::Mutex::new(None));
    let active = Arc::new(AtomicBool::new(false));
    let current_turn: CurrentTurn = Arc::new(std::sync::Mutex::new(None));
    let admission_released = Arc::new(tokio::sync::Notify::new());
    let idle_child_active = Arc::new(AtomicBool::new(false));
    let fatal = Arc::new(AtomicBool::new(false));
    let mut idle = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut owner_pid = supervisor_pid.or(worker.supervisor_pid);
    let mut orphaned_since = None::<Instant>;
    loop {
        tokio::select! {
            _=tick.tick()=>{
                if let Some(pid) = service::read_worker(&dir).ok().and_then(|owner| owner.supervisor_pid) { owner_pid = Some(pid); }
                // Without its supervisor no one can report this host's work, and its
                // lifetime lock blocks recovery. Allow time to cancel, then release it.
                if owner_pid.is_some_and(service::process_exited) {
                    fatal.store(true,Ordering::Release);
                    if orphaned_since.get_or_insert_with(Instant::now).elapsed() > ORPHAN_GRACE {
                        std::process::exit(1);
                    }
                }
                if service::read_worker(&dir).map_or(true, |owner| owner.status == "stopped" || chrono::Utc::now().timestamp_millis().max(0) as u64 > owner.heartbeat_ms.saturating_add(10_000)) {
                    fatal.store(true,Ordering::Release);
                    if let Ok(owner)=service::read_worker(&dir) { if let Some(id)=owner.active_job { let _=jobs::cancel(&worker.root,&id); } }
                }
                let turn_active = active.load(Ordering::Acquire);
                let shell_active = deepseek_code_core::tools::background_shell::has_active();
                if should_expire(turn_active, shell_active, fatal.load(Ordering::Acquire), idle.elapsed()) {break;}
                if (turn_active && !idle_child_active.load(Ordering::Acquire)) || shell_active {idle=Instant::now();}
                if !turn_active && !fatal.load(Ordering::Acquire) {
                    if let Err(error) = child::start_pending(&worker, &session, &active, &admission_released, &current_turn, &idle_child_active, &fatal, &mode_target).await {
                        eprintln!("Owned child wake unavailable: {error:#}");
                    }
                }
            },
            stream=listener.accept()=>{
                let stream=stream?;
                let parsed=tokio::time::timeout(Duration::from_secs(5),service::read_request(stream)).await;
                let Ok(Ok((stream,request)))=parsed else{continue};
                if matches!(&request,Request::Submit{..}|Request::Shutdown) {
                        // The supervisor publishes a turn's completion before this
                        // host releases admission, so a caller reacting to that
                        // publication may be early rather than wrong. Wait for the
                        // actual release of a turn proven finished; a running turn
                        // is refused at once.
                        let current=current_turn.lock().unwrap_or_else(|e|e.into_inner()).clone();
                        let admission=await_admission(&active,current.as_deref(),&admission_released,|id|jobs::get(&worker.root,id).is_ok_and(|job|job.finished_ms.is_some()),ADMISSION_GRACE).await;
                        // A submission that outlives the grace is not queued and not
                        // replayed: it is answered as unsubmitted, exactly as any
                        // other refusal, and the turn keeps its admission.
                        if admission==Admission::Expired && matches!(&request,Request::Submit{..}) {
                            let _=service::reply(stream,&Reply::error("Session worker is busy; prompt was not submitted")).await;continue;
                        }
                }
                match request {
                    Request::BashSession { revoke } => {
                        let manager = bash_target.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if revoke { if let Some(manager) = &manager { manager.set_bash_session_allowed(&worker.session_id, false); } }
                        let mut response = Reply::ok(None);
                        response.permission_session_id = Some(worker.session_id.clone());
                        response.bash_session = Some(manager.is_some_and(|manager| manager.bash_session_allowed(&worker.session_id)));
                        let _ = service::reply(stream, &response).await;
                    },
                    Request::EditSession { revoke } => {
                        let manager = bash_target.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if revoke { if let Some(manager) = &manager { manager.set_edit_session_allowed(&worker.session_id, false); } }
                        let mut response = Reply::ok(None);
                        response.permission_session_id = Some(worker.session_id.clone());
                        response.edit_session = Some(manager.is_some_and(|manager| manager.edit_session_allowed(&worker.session_id)));
                        let _ = service::reply(stream, &response).await;
                    },
                    Request::RuntimeStatus => {
                        let source = status_target.lock().unwrap_or_else(|error|error.into_inner()).clone().unwrap_or_else(||deepseek_code_core::services::runtime_status::RuntimeSource {
                            mcp: Arc::new(deepseek_code_core::mcp::McpServerManager::new(worker.root.join("mcp-servers.json"))),
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
                        else if deepseek_code_core::tools::background_shell::has_active() {let _=service::reply(stream,&Reply::error("Session worker owns running background shell tasks; stop them explicitly before shutdown")).await;}
                        else {let _=service::reply(stream,&Reply::ok(None)).await;break;}
                    },
                    Request::Submit{id,prompt,arguments,environment}=>{
                        if fatal.load(Ordering::Acquire) {let _=service::reply(stream,&Reply::error("Session worker is shutting down; prompt was not submitted")).await;continue;}
                        if active.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() {
                            let _=service::reply(stream,&Reply::error("Session worker is busy; prompt was not submitted")).await;continue;
                        }
                        *current_turn.lock().unwrap_or_else(|e|e.into_inner())=Some(id.clone());
                        if let Err(error)=service::begin_job(&worker,&id,&prompt) {
                            *current_turn.lock().unwrap_or_else(|e|e.into_inner())=None;
                            active.store(false,Ordering::Release);let _=service::reply(stream,&Reply::error(error)).await;continue;
                        }
                        let worker=worker.clone();let sessions=session.clone();let active=active.clone();let fatal=fatal.clone();let turn_id=id.clone();let mode_target=mode_target.clone();let status_target=status_target.clone();let bash_target=bash_target.clone();let current_turn=current_turn.clone();let admission_released=admission_released.clone();
                        tokio::spawn(async move {
                            let task_worker=worker.clone();let task_id=turn_id.clone();let task_mode_target=mode_target.clone();
                            let result=tokio::spawn(async move {execute(&task_worker,&task_id,arguments,environment,sessions,task_mode_target,status_target,bash_target).await}).await;
                            let error=match result {Ok(result)=>result.err().map(|error|format!("{error:#}")),Err(error)=>{fatal.store(true,Ordering::Release);Some(format!("Session turn panicked: {error}"))}};
                            if let Some(error)=&error {let _=service::emit_event(&serde_json::json!({"type":"error","error":error}));}
                            if service::finish_job(&worker,&turn_id,error).await.is_err() {fatal.store(true,Ordering::Release);}
                            *mode_target.lock().unwrap_or_else(|e|e.into_inner()) = None;
                            service::set_event_sink(None);
                            std::env::remove_var(jobs::CONTROL_ENV);
                            active.store(false,Ordering::Release);
                            admission_released.notify_one();
                            // Clear this turn's identity only while it still names
                            // this turn. Admission is already open here, so a newer
                            // submission can have replaced the identity in between;
                            // that record must survive this cleanup.
                            {
                                let mut current=current_turn.lock().unwrap_or_else(|e|e.into_inner());
                                if current.as_deref()==Some(&turn_id) { *current=None; }
                            }
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
    bash_target: BashTarget,
) -> Result<()> {
    let dir = worker.root.join("background-tasks").join(id);
    service::set_event_sink(Some(dir.join("output.jsonl")));
    std::env::set_var(jobs::CONTROL_ENV, &dir);
    // These are the only settings accepted from the same-user IPC frontend.
    for name in [
        "DEEPSEEK_CODE_CHILD_API_KEY",
        "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
        "DEEPSEEK_CODE_WORKER_MODEL",
    ] {
        std::env::remove_var(name);
    }
    for (name, value) in environment {
        ensure!(
            [
                "DEEPSEEK_CODE_CHILD_API_KEY",
                "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
                "DEEPSEEK_CODE_WORKER_MODEL"
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
    if let Ok(value) = std::env::var("DEEPSEEK_CODE_WORKER_MODEL") {
        let model: deepseek_code_core::native_engine::provider_manager::ModelConfig =
            serde_json::from_str(&value)?;
        let provider = selection
            .providers
            .iter_mut()
            .find(|provider| provider.id == selection.provider_id)
            .context("Worker provider missing")?;
        let expected=deepseek_code_core::native_engine::provider_manager::resolve_model_selection(&provider.models,&selection.model).context("Worker selected model missing")?;
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
        session.permissions.set_plan_launch_permissions(cli.allow_dangerously_skip_permissions, cli.full_access);
        session.auto_route =
            std::env::var("DEEPSEEK_CODE_WORKER_AUTO_ROUTE").as_deref() == Ok("true");
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
    *bash_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.permissions.clone());
    session.engine.set_owned_child_idle_ready(&session.id, false);
    *status_target.lock().unwrap_or_else(|error|error.into_inner()) = Some(session.runtime_status_source());
    let current_provider = session.selected_provider().await?;
    *mode_target.lock().unwrap_or_else(|e|e.into_inner()) = Some(ModeControl { job_id:id.into(), session_id:session.id.clone(), manager:session.permissions.clone(), db:session.store.db.clone(), provider:current_provider, owner_manager:None, bypass_available:session.bypass_available, full_access:session.full_access });
    deepseek_code_core::permissions::set_full_access(session.full_access && session.permissions.get_mode() == deepseek_code_core::permissions::PermissionMode::BypassPermissions);
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
    deepseek_code_core::services::session_permission_mode::set(&session.store.db, &session.id, &session.permissions, session.permissions.get_mode())?;
    result
}

fn update_live_mode(worker: &Worker, target: &ModeTarget, job_id: &str, mode: &str) -> Result<String> {
    let target = target.lock().unwrap_or_else(|e|e.into_inner());
    let control = target.as_ref().context("Session worker has no active initialized turn")?;
    let bypass = deepseek_code_core::services::session_permission_mode::parse(mode)? == deepseek_code_core::permissions::PermissionMode::BypassPermissions;
    ensure!(!bypass || control.bypass_available, "Live switching cannot enable bypass permissions unless the launch allows it");
    ensure!(control.job_id == job_id, "Permission change targets a different job");
    if control.owner_manager.as_ref().is_some_and(|manager| manager.get_mode() == deepseek_code_core::permissions::PermissionMode::PlanMode) {
        ensure!(deepseek_code_core::services::session_permission_mode::parse(mode)? == deepseek_code_core::permissions::PermissionMode::PlanMode, "Owner remains in plan mode; child mode cannot expand its authority");
    }
    let job = jobs::get(&worker.root, job_id)?;
    ensure!(job.session_id == worker.session_id && job.finished_ms.is_none(), "Permission change targets an inactive session job");
    if deepseek_code_core::services::session_permission_mode::parse(mode)? == deepseek_code_core::permissions::PermissionMode::Auto {
        deepseek_code_core::native_engine::auto_permissions::configured(&crate::services::context(&RuntimePaths::new(worker.root.clone())?)?, &control.provider)?;
    }
    deepseek_code_core::services::session_permission_mode::set(&control.db, &control.session_id, &control.manager, deepseek_code_core::services::session_permission_mode::parse(mode)?)?;
    deepseek_code_core::permissions::set_full_access(control.full_access && bypass);
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
        deepseek_code_core::services::session_permission_mode::name(control.manager.get_mode()).into(),
    );
    reply.permission_session_id = Some(control.session_id.clone());
    reply.launch_rules = Some(control.manager.launch_rules(&control.session_id));
    Ok(reply)
}

const ORPHAN_GRACE: Duration = Duration::from_secs(5);

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
#[cfg(test)]
mod admission_tests {
    use super::*;

    /// One virtual interval of the previous implementation's drain loop.
    const TICK: Duration = Duration::from_millis(10);
    /// How many intervals that loop probed: 25 x 10 ms = 250 ms.
    const OLD_POLL_STEPS: usize = 25;

    fn finished(_: &str) -> bool {
        true
    }

    /// Advances `ticks` virtual 10 ms intervals, yielding after each one so a
    /// loop that creates a fresh `sleep` per iteration is actually polled
    /// through every interval. Advancing in one jump would let such a loop
    /// create all its sleeps "after" the deadline instead of over it.
    async fn advance_ticks(ticks: usize) {
        for _ in 0..ticks {
            tokio::time::advance(TICK).await;
            tokio::task::yield_now().await;
        }
    }

    /// Reads the decision the waiter sent, advancing one virtual tick per
    /// attempt. The bound is virtual time, so this cannot spin forever and uses
    /// no wall clock.
    async fn settled<T>(rx: &mut tokio::sync::mpsc::Receiver<T>, ticks: usize) -> Option<T> {
        for _ in 0..ticks {
            if let Ok(value) = rx.try_recv() {
                return Some(value);
            }
            tokio::time::advance(TICK).await;
            tokio::task::yield_now().await;
        }
        rx.try_recv().ok()
    }

    /// A running turn is refused at once, and so is the window in which
    /// admission was taken but its job is not recorded yet. Waiting would let a
    /// second turn overlap the first.
    #[test]
    fn only_a_provably_finished_admitted_turn_is_waited_for() {
        assert_eq!(
            admission_state(true, Some("running"), |_| false),
            Admission::Busy
        );
        assert_eq!(admission_state(true, None, finished), Admission::Busy);
        assert_eq!(
            admission_state(true, Some("finished"), finished),
            Admission::Finalizing
        );
        assert_eq!(
            admission_state(false, Some("finished"), finished),
            Admission::Admit
        );
        assert_eq!(admission_state(false, None, finished), Admission::Admit);
    }

    /// The defect being fixed: the old fixed window gave up after 25 x 10 ms and
    /// the submission was refused as busy even though the turn had finished
    /// durably. Virtual time is advanced one interval at a time so the old loop
    /// would really be polled through its 25 sleeps; the wait must still be
    /// running afterwards. Only the real release may end it.
    #[tokio::test(start_paused = true)]
    async fn a_finalizing_turn_is_still_waited_for_past_the_old_poll_window() {
        let active = Arc::new(AtomicBool::new(true));
        let release = Arc::new(tokio::sync::Notify::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<Admission>(1);
        let flag = active.clone();
        let signal = release.clone();
        let waiter = tokio::spawn(async move {
            let _ = started_tx.send(());
            let decision = await_admission(
                &flag,
                Some("finished-job"),
                &signal,
                finished,
                ADMISSION_GRACE,
            )
            .await;
            let _ = done_tx.send(decision).await;
        });
        started_rx.await.unwrap();
        // Let the waiter register its grace wait before virtual time moves.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // 44 ticks = 440 ms virtual: past the old 250 ms window, still well
        // inside the 2 s grace.
        advance_ticks(OLD_POLL_STEPS + 19).await;
        assert!(
            done_rx.try_recv().is_err(),
            "the submission stopped waiting while the turn still held admission"
        );
        assert!(
            active.load(Ordering::Acquire),
            "waiting must not release another turn's admission"
        );

        // The host's real release is the only thing that ends the wait.
        active.store(false, Ordering::Release);
        release.notify_one();
        assert_eq!(settled(&mut done_rx, 10).await, Some(Admission::Admit));
        waiter.await.unwrap();
    }

    /// A finalization that outlives the grace leaves the prompt unsubmitted: no
    /// second turn is admitted and nothing else is touched. Expiry is reached in
    /// virtual time with no external I/O. A later real release then admits the
    /// next submission, which is the only path that can start a turn - a
    /// timed-out prompt is never queued and never replayed.
    #[tokio::test(start_paused = true)]
    async fn an_expired_finalization_is_unsubmitted_and_a_later_release_admits() {
        let active = Arc::new(AtomicBool::new(true));
        let release = Arc::new(tokio::sync::Notify::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<Admission>(1);
        let flag = active.clone();
        let signal = release.clone();
        let grace = Duration::from_millis(30);
        let waiter = tokio::spawn(async move {
            let _ = started_tx.send(());
            let decision =
                await_admission(&flag, Some("finished-job"), &signal, finished, grace).await;
            let _ = done_tx.send(decision).await;
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(settled(&mut done_rx, 10).await, Some(Admission::Expired));
        assert!(
            active.load(Ordering::Acquire),
            "expiry must not release the turn's admission"
        );
        // Taking admission is the only side effect that follows `Admit`; it fails
        // here, so the prompt was not submitted and no turn started.
        assert!(
            active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err(),
            "a timed-out finalization admitted a second turn"
        );
        waiter.await.unwrap();

        // The host's real release admits the next submission.
        active.store(false, Ordering::Release);
        release.notify_one();
        assert_eq!(
            await_admission(
                &active,
                Some("finished-job"),
                &release,
                finished,
                ADMISSION_GRACE
            )
            .await,
            Admission::Admit
        );
        assert!(
            active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a released turn must admit the next submission"
        );
    }

    /// A host with nothing running admits immediately, without waiting out any
    /// window and without consulting `active.json`.
    #[tokio::test]
    async fn an_already_released_host_admits_immediately() {
        let active = AtomicBool::new(false);
        let release = tokio::sync::Notify::new();
        assert_eq!(
            await_admission(
                &active,
                None,
                &release,
                |_| panic!("must not consult the job"),
                ADMISSION_GRACE
            )
            .await,
            Admission::Admit
        );
    }
}
