//! An attachable frontend for a turn owned by an independent supervisor/worker.
//! Only the worker submits the prompt, executes tools, and persists the transcript.
#[cfg(test)]
use crate::fixture_sql::FixtureSql;
use crate::{
    args::Cli,
    session::Session,
    terminal::{
        input::TerminalInput,
        live::{Action, TurnDisplay},
    },
};
use anyhow::Context;
use anyhow::{ensure, Result};
use deepseek_code_core::services::background_sessions::{self as jobs, Launch};
use serde_json::Value;

#[derive(Default)]
struct ProposalRetry {
    since: Option<std::time::Instant>,
    next: Option<std::time::Instant>,
    delay_ms: u64,
    reported: bool,
}

impl ProposalRetry {
    fn due(&self, now: std::time::Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }
    fn failed(&mut self, now: std::time::Instant) -> bool {
        let since = *self.since.get_or_insert(now);
        self.delay_ms = if self.delay_ms == 0 {
            100
        } else {
            (self.delay_ms * 2).min(500)
        };
        self.next = Some(now + std::time::Duration::from_millis(self.delay_ms));
        if !self.reported && now.duration_since(since) >= std::time::Duration::from_secs(5) {
            self.reported = true;
            return true;
        }
        false
    }
    fn recovered(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        let elapsed = self.since.map(|since| now.duration_since(since));
        *self = Self::default();
        elapsed
    }
}

/// Tolerates brief task-state read failures without ending the turn. The
/// supervisor replaces `state.json` about every 100 ms and a filesystem filter
/// can briefly hide it; one lost poll must not look like a dead worker.
#[derive(Default)]
struct StateRetry {
    since: Option<std::time::Instant>,
    failures: u32,
}

impl StateRetry {
    const FAILURES: u32 = 5;
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

    /// Records a failure and reports whether the view must now give up.
    fn failed(&mut self, now: std::time::Instant) -> bool {
        let since = *self.since.get_or_insert(now);
        self.failures += 1;
        self.failures > Self::FAILURES || now.duration_since(since) > Self::WINDOW
    }
    fn recovered(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        let elapsed = self.since.map(|since| now.duration_since(since));
        *self = Self::default();
        elapsed
    }
}

fn log_job_observation(root: &std::path::Path, id: &str, message: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("tasks/background").join(id).join("stderr.log"))
    {
        let _ = writeln!(file, "{} {message}", chrono::Utc::now().to_rfc3339());
    }
}

#[test]
fn proposal_contention_retries_without_blocking_and_reports_only_sustained_failures() {
    let start = std::time::Instant::now();
    let mut retry = ProposalRetry::default();
    let mut now = start;
    for delay in [100, 200, 400, 500, 500] {
        assert!(retry.due(now));
        assert!(!retry.failed(now));
        assert!(!retry.due(now));
        assert_eq!(retry.delay_ms, delay);
        now += std::time::Duration::from_millis(delay);
    }
    now = start + std::time::Duration::from_secs(5);
    assert!(retry.failed(now));
    assert!(!retry.failed(now));
    assert_eq!(retry.recovered(now).unwrap().as_secs(), 5);
    assert!(retry.due(now));
    assert!(!retry.failed(now));
}

#[test]
fn task_state_retry_tolerates_brief_failures_and_gives_up_after_the_window() {
    let start = std::time::Instant::now();
    let mut retry = StateRetry::default();
    for step in 1..=5 {
        assert!(
            !retry.failed(start + std::time::Duration::from_millis(step * 100)),
            "failure {step} must be tolerated"
        );
    }
    assert!(retry.failed(start + std::time::Duration::from_millis(600)));

    let mut retry = StateRetry::default();
    assert!(!retry.failed(start));
    assert!(retry.failed(start + std::time::Duration::from_millis(2_001)));

    let mut retry = StateRetry::default();
    assert!(!retry.failed(start));
    assert_eq!(
        retry.recovered(start + std::time::Duration::from_millis(300)),
        Some(std::time::Duration::from_millis(300))
    );
    assert!(retry.recovered(start).is_none());
}

pub async fn turn(
    cli: &Cli,
    session: &Session,
    prompt: &str,
    input: &mut Option<TerminalInput>,
    mut origin: crate::session::TurnOrigin<'_>,
) -> Result<()> {
    let mut next = prompt.to_owned();
    while !crate::signals::terminating() {
        let Some(kickoff) = Box::pin(single_turn(cli, session, &next, input, origin)).await? else {
            return Ok(());
        };
        next = kickoff;
        origin = crate::session::TurnOrigin::GoalContinuation;
    }
    Ok(())
}

/// A frontend that was killed cannot release its lease, which then expires
/// within seconds. Wait that out; a frontend still heartbeating keeps the lease.
async fn acquire_proposal_frontend(session: &Session) -> Result<deepseek_code_core::services::goal_proposals::Frontend> {
    use deepseek_code_core::services::goal_proposals::Frontend;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        match Frontend::acquire(session.store.db.clone(), &session.id) {
            Err(_) if std::time::Instant::now() < deadline => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            result => return result,
        }
    }
}

/// Clears abandoned worker state and returns the job still occupying the session.
pub(crate) fn settle_session(session: &Session) -> Result<Option<jobs::Job>> {
    let settled = deepseek_code_core::services::session_worker::settle_session(&session.paths.data_dir, &session.id)?;
    if settled.recovered {
        eprintln!("Recovered interrupted worker state; previous work was preserved and will not be replayed.");
    }
    Ok(settled.active)
}

/// How to get past an occupied session, naming the task so it can be acted on.
pub(crate) fn active_worker_message(job: &jobs::Job) -> String {
    let id = &job.id;
    if job.status == "unresponsive" {
        format!("Task {id} stopped responding, but its process is still alive. Use /tasks show {id} to inspect it, or /clear to work in a new session.")
    } else {
        format!("Task {id} is still running in this session. Use /continue to return to it, /tasks cancel {id} to stop it, or /clear to work in a new session.")
    }
}

/// What keeps this session from admitting a new turn. A managed task and a
/// direct print/json owner are different kinds of occupancy: only the former
/// can be attached interactively.
pub(crate) enum Occupancy {
    Job(jobs::Job),
    Lease,
}

/// Reports the owner of the session, checking the managed task first because
/// it has the actionable attach/cancel commands. This only observes; it never
/// takes the execution lease.
pub(crate) fn occupancy(session: &Session) -> Result<Option<Occupancy>> {
    if let Some(job) = settle_session(session)? {
        return Ok(Some(Occupancy::Job(job)));
    }
    if crate::observe::execution_lease_active(session)? {
        return Ok(Some(Occupancy::Lease));
    }
    Ok(None)
}

/// How to get past an occupied session, without the read-only guidance.
pub(crate) fn occupancy_message(session: &Session, occupancy: &Occupancy) -> String {
    match occupancy {
        Occupancy::Job(job) => active_worker_message(job),
        Occupancy::Lease => crate::observe::running_notice(session).unwrap_or_else(|_| {
            "A turn is still running in another process; observe it or wait for it to finish."
                .into()
        }),
    }
}

/// The banner shown once when the REPL enters read-only observation.
pub(crate) fn observation_banner(session: &Session, occupancy: &Occupancy) -> String {
    match occupancy {
        Occupancy::Job(job) => {
            let since =
                chrono::DateTime::<chrono::Utc>::from_timestamp_millis(job.started_ms as i64)
                    .map(|time| {
                        time.with_timezone(&chrono::Local)
                            .format("%-I:%M %p")
                            .to_string()
                    })
                    .unwrap_or_else(|| "an earlier time".into());
            format!(
                "✻ Task {} is executing this session since {since}. Read-only observation: ordinary prompts are not submitted. /continue returns to it, /tasks attach {} opens the full view, Esc exits.",
                job.id, job.id
            )
        }
        Occupancy::Lease => format!(
            "{} Ordinary prompts are not submitted; /continue shows the running turn. Esc exits.",
            occupancy_message(session, occupancy)
        ),
    }
}

async fn single_turn(
    cli: &Cli,
    session: &Session,
    prompt: &str,
    input: &mut Option<TerminalInput>,
    origin: crate::session::TurnOrigin<'_>,
) -> Result<Option<String>> {
    let root = &session.paths.data_dir;
    if let Some(job) = settle_session(session)? {
        anyhow::bail!("{} The current prompt was not submitted.", active_worker_message(&job));
    }
    if crate::observe::execution_lease_active(session)? {
        anyhow::bail!(
            "{} The current prompt was not submitted.",
            crate::observe::running_notice(session)?
        );
    }
    let provider = session.launch_provider().await?;
    let executable = std::env::current_exe()?;
    // Validate interactive configuration before launching a request that must survive us.
    let history = session
        .store
        .history(&session.id)?
        .iter()
        .filter(|row| row["role"] == "user")
        .filter_map(|row| row["content"].as_str().map(str::to_owned))
        // Task notifications are not prompts to recall.
        .filter(|text| !crate::background_commands::is_notification(text))
        .collect();
    let model_label = crate::session_commands::model_label(session).await;
    let mut display = TurnDisplay::new_with_initial(
        true,
        session.permission_mode(),
        &deepseek_code_core::data_layout::DataLayout::from_root(&root)
            .config_file("keybindings.json"),
        history,
        Some(root),
        Some(model_label),
        Some(prompt.to_owned()),
    )?
    .with_workspace(&session.workspace);
    display.restore_numbers(session)?;
    display.set_session_context(root, session.selected_session()?);
    if display.is_live() {
        display.set_transcript(crate::repl::transcript(session).unwrap_or_default());
        display.set_todos(crate::background_commands::todo_items(&session.paths, &session.id));
    }
    let proposal_frontend = if display.live.is_some() {
        match acquire_proposal_frontend(session).await {
            Ok(frontend) => Some(frontend),
            Err(error) => {
                // Goal proposals are auxiliary UI; losing their frontend must
                // never abort the turn itself.
                tracing::warn!(
                    "Goal proposal frontend unavailable for session {}: {error:#}",
                    session.id
                );
                display.notice("Goal proposal controls are unavailable; the turn continues.");
                None
            }
        }
    } else { None };
    let owner = session.binding.selected_session(&session.id)?;
    let _admission = owner.hold_incarnation()?;
    let job = if std::env::var("DEEPSEEK_CODE_SINGLE_TURN_WORKER").as_deref() == Ok("1") {
        let arguments = crate::background_commands::arguments(
            cli,
            &session.paths,
            session,
            &session.id,
            prompt,
            origin,
            &provider,
        )?;
        let mut environment =
            crate::background_commands::worker_environment(session.auto_route, &provider)?;
        environment.push((
            jobs::WORKER_GENERATION_ENV.into(),
            format!("{}:{}", session.id, session.binding_state().generation),
        ));
        jobs::spawn_foreground(
            root,
            &executable,
            Launch {
                executable: executable.clone(),
                arguments,
                cwd: session.workspace.clone(),
            },
            &session.id,
            prompt,
            &environment,
        )?
    } else {
        use deepseek_code_core::services::session_worker::{self, Request};
        let mut worker =
            session_worker::ensure_worker(root, &executable, &session.id, &session.workspace)
                .await?;
        let arguments = crate::background_commands::turn_arguments(
            cli,
            &session.paths,
            session,
            &session.id,
            prompt,
            origin,
            &provider,
        )?;
        let mut environment = vec![(
            "DEEPSEEK_CODE_WORKER_AUTO_ROUTE".to_owned(),
            session.auto_route.to_string(),
        )];
        if session_worker::call(&worker, &Request::Ping)
            .await?
            .generation_supported
            != Some(true)
        {
            session_worker::shutdown_worker(&worker)
                .await
                .context("Provider switch pending; observe the legacy job with /continue")?;
            worker =
                session_worker::ensure_worker(root, &executable, &session.id, &session.workspace)
                    .await?;
        }
        let state = session.binding.read().context("Missing session binding")?;
        environment.push((crate::worker_binding::SNAPSHOT_ENV.into(), serde_json::to_string(state.as_ref())?));
        let id = uuid::Uuid::new_v4().to_string();
        let response = session_worker::call(
            &worker,
            &Request::Submit {
                id: id.clone(),
                prompt: prompt.into(),
                arguments,
                environment,
            },
        )
        .await;
        // Accepted jobs are durable even when the acknowledgement is lost. Never replay.
        match jobs::get(root, &id) {
            Ok(job) => job,
            Err(error) => {
                response?;
                return Err(error);
            }
        }
    };
    drop(_admission);
    present(session, &job, input, &mut display, true, proposal_frontend).await?;
    if jobs::get(root, &job.id)?.status == "completed" { activate_goal_proposal(session, &job.id) } else { Ok(None) }
}

fn activate_goal_proposal(session: &Session, job: &str) -> Result<Option<String>> {
    let kickoff = deepseek_code_core::services::goal_proposals::activate_from_job(&session.store.db, &session.id, &session.paths.data_dir, &session.workspace, &session.permissions, job)?;
    if kickoff.is_some() { crate::session_commands::goal_notice("Approved session goal is now active."); }
    Ok(kickoff)
}

/// Attach to durable events only: never submit or recreate a worker request.
pub async fn attach(cli: &Cli, session: &Session, id: &str, input: &mut Option<TerminalInput>) -> Result<()> {
    let job = jobs::get(&session.paths.data_dir, id)?;
    let _participants =
        jobs::hold_attachment(&session.paths.data_dir, &job, &session.selected_session()?)?;
    let workspace = session.store.workspace(
        &job.session_id,
        &session.paths.data_dir.join("data/workspaces"),
    )?;
    let model_label = crate::session_commands::model_label(session).await;
    let mut display = TurnDisplay::new_with_initial(
        true,
        session.permission_mode(),
        &session.paths.layout().config_file("keybindings.json"),
        Vec::new(),
        Some(&session.paths.data_dir),
        Some(model_label),
        None,
    )?
    .with_workspace(&workspace);
    display.restore_numbers(session)?;
    display.set_session_context(&session.paths.data_dir, jobs::participant_execution(&session.paths.data_dir, &session.selected_session()?, &job)?);
    display.notice(&format!(
        "Attached to task {id}. Esc cancels; Ctrl+B or /background detaches without restarting."
    ));
    if job.session_id != session.id {
        display.notice("Follow-up prompts are queued to your current session after this view closes; they do not steer the attached child. /btw uses your current session context.");
    }
    let main_execution = is_main_execution(&session.paths.data_dir, &job, &session.id)?;
    let proposal_frontend = if display.live.is_some() && main_execution {
        match acquire_proposal_frontend(session).await {
            Ok(frontend) => Some(frontend),
            Err(error) => {
                log_job_observation(
                    &session.paths.data_dir,
                    id,
                    &format!("Goal proposal frontend unavailable: {error:#}"),
                );
                display.notice("Goal proposal controls are unavailable; task output continues.");
                None
            }
        }
    } else { None };
    present(session, &job, input, &mut display, false, proposal_frontend).await?;
    let kickoff = if main_execution && jobs::get(&session.paths.data_dir, &job.id)?.status == "completed" { activate_goal_proposal(session, &job.id)? } else { None };
    if let Some(kickoff) = kickoff {
        Box::pin(turn(
            cli,
            session,
            &kickoff,
            input,
            crate::session::TurnOrigin::GoalContinuation,
        ))
        .await?;
    }
    Ok(())
}

fn is_main_execution(root: &std::path::Path, job: &jobs::Job, session: &str) -> Result<bool> {
    Ok(job.session_id == session && deepseek_code_core::services::session_worker::job_execution_session(root, job)? == session)
}

pub(crate) fn cancel_job(session: &Session, id: &str) -> Result<()> {
    let _participants = jobs::hold_attachment(
        &session.paths.data_dir,
        &jobs::get(&session.paths.data_dir, id)?,
        &session.selected_session()?,
    )?;
    let job = jobs::cancel(&session.paths.data_dir, id)?;
    let execution_session = deepseek_code_core::services::session_worker::job_execution_session(
        &session.paths.data_dir,
        &job,
    )?;
    deepseek_code_core::services::async_hooks::signal_cancel(&session.paths.data_dir, &execution_session);
    if is_main_execution(&session.paths.data_dir, &job, &session.id)? {
        deepseek_code_core::services::goal_proposals::discard(&session.store.db, &session.id)?;
    }
    Ok(())
}

async fn present(
    session: &Session,
    job: &jobs::Job,
    input: &mut Option<TerminalInput>,
    display: &mut TurnDisplay,
    sync_plan_mode: bool,
    proposal_frontend: Option<deepseek_code_core::services::goal_proposals::Frontend>,
) -> Result<()> {
    let _participants =
        jobs::hold_attachment(&session.paths.data_dir, job, &session.selected_session()?)?;
    let root = &session.paths.data_dir;
    let result = observe(session, &job.id, &job.parent_session_id, input, display, sync_plan_mode, proposal_frontend).await;
    let still_running = result.is_err()
        && jobs::get(root, &job.id)
            .is_ok_and(|job| matches!(job.status.as_str(), "starting" | "running"));
    if still_running {
        // Observation is not execution: a failed view must never cancel work.
        let _ = jobs::detach(root, &job.id, &job.parent_session_id);
        display.notice(&format!("Task {} continues in the background after a view error. Use /tasks attach {} to return; /tasks show {} contains the original diagnostic.", job.id, job.id, job.id));
    }
    // Raw engine events and terminal error remain in the durable job logs.
    // The footer owns the single user-facing error; do not rethrow it to REPL.
    let error_text = result.as_ref().err().map(|error| {
        if still_running {
            return format!(
                "Task view failed; worker continues. Details: /tasks show {}",
                job.id
            );
        }
        let state = jobs::get(root, &job.id).ok();
        failure_message(
            &job.id,
            state
                .as_ref()
                .map(|job| job.status.as_str())
                .unwrap_or("failed"),
            &format!("{error:#}"),
        )
    });
    if let Err(error) = &result {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(
            root.join("tasks/background")
                .join(&job.id)
                .join("stderr.log"),
        ) {
            let _ = writeln!(file, "Frontend observed: {error:#}");
        }
    }
    let footer = if matches!(result, Ok(true)) {
        Some("detached - worker continues")
    } else {
        error_text.as_deref()
    };
    if display.is_live() {
        display.set_running_shells(crate::background_commands::running_shells(&session.paths, &session.id));
        display.set_running_agents(crate::background_commands::running_agents(&session.paths, &session.store.db, &session.id).len());
    }
    for action in display.finish(footer) {
        restore(input, action);
    }
    if let Err(error) = crate::repl::save_completion(session) {
        eprintln!("Could not save turn completion status: {error}");
    }
    match result {
        Ok(_) => Ok(()),
        Err(_) => {
            Err(PresentedFailure(error_text.unwrap_or_else(|| "Request failed".into())).into())
        }
    }
}

fn restore(input: &mut Option<TerminalInput>, action: Action) {
    if let Some(input) = input {
        match action {
            Action::CyclePermission => input.queue_permission_cycle(),
            Action::Draft(draft) => input.restore_draft(draft),
            Action::Submit(prompt) | Action::Queue(prompt) => input.queue_prompt(prompt),
            Action::SideQuestion(question) => input.queue_prompt(format!("/btw {question}")),
            _ => {}
        }
    }
}

async fn observe(
    session: &Session,
    id: &str,
    owner: &str,
    input: &mut Option<TerminalInput>,
    display: &mut TurnDisplay,
    sync_plan_mode: bool,
    proposal_frontend: Option<deepseek_code_core::services::goal_proposals::Frontend>,
) -> Result<bool> {
    let root = &session.paths.data_dir;
    let initial_job = jobs::get(root, id)?;
    let execution_session = deepseek_code_core::services::session_worker::job_execution_session(root, &initial_job)?;
    let child_target = (execution_session != initial_job.session_id).then_some(execution_session);
    if let Some(child) = &child_target {
        display.notice(&format!("Task controls apply to owned child session {child}."));
    }
    let sync_mode = child_target.is_none() && may_sync_permission_mode(&session.id, &initial_job.session_id, &initial_job.status, sync_plan_mode);
    let mut cursor = 0;
    let mut streamed = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut answered = std::collections::HashSet::new();
    let mut pending: Option<(String, tokio::sync::oneshot::Receiver<String>)> = None;
    // The plan and its file while a plan review is pending.
    let mut pending_plan: Option<(String, String)> = None;
    // An AskUserQuestion menu waiting for its reply.
    let mut pending_menu: Option<crate::terminal::question_menu::Menu> = None;
    // A background agent's question shown in the live menu: job and request.
    let mut agent_pending: Option<(String, String, tokio::sync::oneshot::Receiver<String>)> = None;
    let mut plain_pending: Option<(String, String)> = None;
    let mut side: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>,
    > = None;
    let mut cancelled = false;
    // Messages typed during this job that the model has not read yet.
    let mut queued_inputs = Vec::<String>::new();
    let mut notified = std::time::Instant::now();
    let mut notification_error = None;
    let mut final_mode_synced = false;
    let mut shown_proposal = None::<String>;
    let mut proposal_retry = ProposalRetry::default();
    let mut state_retry = StateRetry::default();
    let mut current_proposal = None;
    // Goal proposals are auxiliary UI. This frontend may be replaced while a
    // turn runs; that must never end the task view.
    let mut proposal_frontend = proposal_frontend;
    let mut proposal_reacquired = false;
    let relay = session.remote_relay();
    let mut remote_request: Option<deepseek_code_core::services::remote_control::PermissionRequest> = None;
    let mut remote_interrupts = session.remote_interrupts();
    let mut local_hooks = deepseek_code_core::services::hook_progress::subscribe(&session.id);
    let mut local_hook_ids = Vec::new();
    for (id, message) in local_hooks.snapshot() {
        display.hook_progress(&id, Some(&message));
        local_hook_ids.push(id);
    }
    // The count is captured before the first poll, so a Ctrl-C that arrives
    // while a tick or an event is being handled still cancels the worker. The
    // loop keeps watching after cancelling, so the branch rearms.
    let mut interrupt = crate::signals::interrupt_scope();
    let mut termination_handled = false;
    let mut terminal_actions_open = true;
    loop {
        tokio::select! {
            hooks = local_hooks.changed() => {
                for id in local_hook_ids.drain(..) { display.hook_progress(&id, None); }
                for (id, message) in hooks {
                    display.hook_progress(&id, Some(&message));
                    local_hook_ids.push(id);
                }
            },
            _ = tick.tick() => {
                if proposal_retry.due(std::time::Instant::now()) {
                  let polled = proposal_frontend.as_ref().map(|frontend| frontend.poll());
                  match polled {
                    Some(Ok(proposal)) => {
                        if let Some(elapsed) = proposal_retry.recovered(std::time::Instant::now()) {
                            log_job_observation(root, id, &format!("Goal proposal observation recovered after {} ms", elapsed.as_millis()));
                        }
                        current_proposal = proposal;
                    },
                    Some(Err(error)) if deepseek_code_core::db::is_contention(&error) => {
                        if proposal_retry.since.is_none() {
                            log_job_observation(root, id, &format!("Goal proposal observation deferred: {error:#}"));
                        }
                        if proposal_retry.failed(std::time::Instant::now()) {
                            display.notice("Goal proposal status is delayed by database contention. Task output continues.");
                            log_job_observation(root, id, "Goal proposal observation still deferred after 5000 ms");
                        }
                    },
                    Some(Err(error)) => {
                        log_job_observation(root, id, &format!("Goal proposal observation failed: {error:#}"));
                        current_proposal = None;
                        shown_proposal = None;
                        if let Some(live) = &display.live { live.goal_proposal(None); }
                        proposal_frontend = if proposal_reacquired {
                            None
                        } else {
                            proposal_reacquired = true;
                            use deepseek_code_core::services::goal_proposals::Frontend;
                            match Frontend::acquire(session.store.db.clone(), &session.id) {
                                Ok(frontend) => {
                                    log_job_observation(root, id, "Goal proposal frontend reacquired");
                                    Some(frontend)
                                }
                                Err(retry) => {
                                    log_job_observation(root, id, &format!("Goal proposal frontend could not be reacquired: {retry:#}"));
                                    None
                                }
                            }
                        };
                        if proposal_frontend.is_none() {
                            display.notice("Goal proposal status unavailable; task output continues.");
                            log_job_observation(root, id, "Goal proposal observation disabled for this view");
                        }
                    },
                    None => {}
                  }
                }
                let proposal = current_proposal.clone();
                let has_proposal = proposal.is_some();
                if shown_proposal.as_deref() != proposal.as_ref().map(|p| p.id.as_str()) {
                    shown_proposal = proposal.as_ref().map(|p| p.id.clone());
                    if let Some(live) = &display.live { live.goal_proposal(proposal.map(|p| (p.id, p.condition))); }
                }
                let observation = jobs::events_since(root, id, owner, cursor);
                let (job, next, events) = match observation {
                    Ok(observed) => {
                        if let Some(elapsed) = state_retry.recovered(std::time::Instant::now()) {
                            log_job_observation(root, id, &format!("Task state observation recovered after {} ms", elapsed.as_millis()));
                        }
                        observed
                    }
                    // A transient filesystem failure loses one poll, not the turn.
                    Err(error)
                        if jobs::is_transient_state_error(&error)
                            && !state_retry.failed(std::time::Instant::now()) =>
                    {
                        if state_retry.failures == 1 {
                            log_job_observation(root, id, &format!("Task state observation deferred: {error:#}"));
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let drained = cursor == next;
                cursor = next;
                let finished_mode = job.finished_ms.is_some() && matches!(job.status.as_str(), "completed" | "failed" | "cancelled");
                let refresh_mode = sync_mode && job.session_id == session.id
                    && (events.iter().any(permission_event) || (finished_mode && !final_mode_synced));
                if refresh_mode {
                    match refresh_permission_mode(session, &job).await {
                        Ok(mode) => { display.set_permission_mode(mode); final_mode_synced = finished_mode; },
                        Err(error) => display.notice(&format!("Could not refresh permission mode; keeping last confirmed mode: {error:#}")),
                    }
                }
                if let Some(child) = &child_target {
                    if events.iter().any(|event| permission_event(event) || event["type"] == "owned_child_wake") || (finished_mode && !final_mode_synced) {
                        match refresh_child_permission_mode(session, &job, child).await {
                            Ok(mode) => { display.set_permission_mode(mode); final_mode_synced = finished_mode; },
                            Err(error) => display.notice(&format!("Child {child} mode unavailable: {error:#}")),
                        }
                    }
                }
                for event in events {
                    if event["type"] == "text" { streamed = true; }
                    if let Some(text) = event["text"].as_str().filter(|_| event["type"] == "queued_input") {
                        if let Some(index) = queued_inputs.iter().position(|queued| queued == text) { queued_inputs.remove(index); }
                    }
                    if event["type"] == "result" {
                        if let Some(text)=result_text(&event,streamed) {display.before_text(text);}
                        streamed = false;
                    }
                    render(display, &event);
                }
                let question = jobs::pending_question(root, id, owner)?;
                let current = question.as_ref().map(|question| question.request_id.as_str());
                // Dropping a stale remote request cancels it on the remote client.
                if remote_request.as_ref().is_some_and(|request| Some(request.request_id.as_str()) != current || answered.contains(&request.request_id)) {
                    remote_request = None;
                }
                if let (None, Some(relay), Some(question)) = (&remote_request, &relay, &question) {
                    if let Some(tool) = question.tool.as_ref().filter(|_| question.kind == "permission" && !answered.contains(&question.request_id)) {
                        remote_request = Some(relay.request_permission(&question.request_id, tool["tool_name"].as_str().unwrap_or_default(), &tool["input"], tool["tool_use_id"].as_str().unwrap_or_default(), &question.question));
                    }
                }
                if pending.as_ref().is_some_and(|(request, _)| Some(request.as_str()) != current) {
                    // Closing this receiver tells the live renderer to discard
                    // only its answer draft and restore the queued prompt draft.
                    pending = None;
                    pending_plan = None;
                    display.set_activity("Working");
                }
                if plain_pending.as_ref().is_some_and(|(request, _)| Some(request.as_str()) != current) {
                    plain_pending = None;
                    if let Some(reader) = input.as_mut() { reader.dismiss_answer().await?; }
                    display.set_activity("Working");
                }
                if pending.is_none() && plain_pending.is_none() && agent_pending.is_none() && question.is_none() {
                    if let (Some(live), Some((job, description, agent))) = (&display.live, crate::background_commands::agent_question(&session.paths, session)) {
                        let (lines, heading, options) = crate::background_commands::agent_approval(&session.workspace, &description, &agent);
                        agent_pending = Some((job, agent.request_id.clone(), live.choice(lines, heading, options)));
                    }
                }
                if pending.is_none() && plain_pending.is_none() {
                    if let Some(question) = question.filter(|question| !answered.contains(&question.request_id)) {
                        if let Some(live) = &display.live {
                            // Tool approvals get the same menu as a foreground turn; the
                            // worker rebuilds the options from the same call to decide.
                            let tool = question.tool.as_ref().filter(|tool| question.kind == "permission" && tool["tool_name"] != "ExitPlanMode");
                            let auto = tool.is_some() && session.approval_auto_available().await;
                            let menu = tool.map(|tool| {
                                crate::terminal::tool_view::approval(tool["tool_name"].as_str().unwrap_or_default(), &tool["input"], Some(&session.workspace), auto)
                            });
                            let plan = question.tool.as_ref().filter(|_| question.kind == "plan");
                            let reply = match (menu, plan) {
                                (Some(approval), _) => live.choice(approval.lines, approval.question, approval.options),
                                (None, Some(plan)) => {
                                    // The worker reads `plan:` answers as menu choices.
                                    let text = plan["plan"].as_str().unwrap_or_default().to_owned();
                                    let path = plan["planFilePath"].as_str().unwrap_or_default().to_owned();
                                    let width = crate::terminal::size().0.saturating_sub(1) as usize;
                                    let view = crate::terminal::plan_view::review(&text, &path, session.auto_mode_available().await, plan["returnToBypass"] == true, width);
                                    pending_plan = Some((text, path));
                                    live.plan_choice(view.lines, view.question, view.options, view.footer)
                                }
                                (None, None) => match question.tool.as_ref().filter(|_| question.kind == "question").and_then(|input| crate::terminal::question_menu::menu(&question.question, input)) {
                                    // As in the reference CLI: a menu whose focused option shows its preview.
                                    Some(menu) => {
                                        let reply = live.question_choice(menu.question.clone(), menu.options.clone(), menu.previews.clone(), menu.multi);
                                        pending_menu = Some(menu);
                                        reply
                                    }
                                    None => live.question(question.question),
                                },
                            };
                            pending = Some((question.request_id, reply));
                        } else if input.is_some() {
                            plain_pending = Some((question.request_id, question.question));
                        }
                    }
                }
                if drained && !matches!(job.status.as_str(), "starting"|"running") {
                    if job.session_id == owner && matches!(job.status.as_str(),"completed"|"failed"|"cancelled") {jobs::acknowledge_foreground(root,id,owner)?;}
                    ensure!(job.status == "completed", "Worker {}: {}{}", id, job.status, job.error.map(|error|format!(" - {error}")).unwrap_or_default());
                    if side.is_none() && !has_proposal { return Ok(false); }
                    display.set_activity(if has_proposal { "Task complete · goal proposal awaiting your choice" } else { "Main task complete - finishing side question" });
                }
                if notified.elapsed() >= std::time::Duration::from_secs(1) {
                    match crate::background_commands::collect_completed(session) {
                        Ok(notices) => {
                            notification_error = None;
                            for notice in notices { display.notice(&notice); }
                        },
                        Err(error) => {
                            // Other jobs' notification state is not this turn's failure.
                            let diagnostic = format!("Background notifications unavailable: {error:#}");
                            if notification_error.as_ref() != Some(&diagnostic) {
                                display.notice(&diagnostic);
                                notification_error = Some(diagnostic);
                            }
                        }
                    }
                    notified = std::time::Instant::now();
                }
            },
            allowed = deepseek_code_core::services::remote_control::remote_decision(&mut remote_request) => {
                let request = remote_request.take().unwrap().request_id.clone();
                answer_if_current(root, id, owner, &request, if allowed { "y" } else { "n" })?;
                answered.insert(request.clone());
                if pending.as_ref().is_some_and(|(pending, _)| *pending == request) { pending = None; pending_plan = None; pending_menu = None; }
                if plain_pending.as_ref().is_some_and(|(pending, _)| *pending == request) {
                    plain_pending = None;
                    if let Some(reader) = input.as_mut() { reader.dismiss_answer().await?; }
                }
                display.notice(if allowed { "Remote Control allowed the tool request." } else { "Remote Control denied the tool request." });
                display.set_activity("Working");
            },
            _ = remote_interrupts.next() => {
                if !cancelled { cancel_job(session, id)?; cancelled = true; display.notice("Remote Control interrupted the turn."); display.set_activity("Cancelling worker"); }
            },
            answer = async { match agent_pending.as_mut() { Some((_, _, answer)) => answer.await, None => std::future::pending().await } } => {
                let (job, request, _) = agent_pending.take().unwrap();
                if let Ok(answer) = answer {
                    let answer = answer.split('\n').next().unwrap_or("").to_owned();
                    match jobs::answer_question(root, &job, &session.id, &request, &answer) {
                        Ok(()) => crate::background_commands::mark_answered(&request),
                        Err(error) => display.notice(&format!("Agent question: {error:#}")),
                    }
                }
            },
            answer = async { match pending.as_mut() { Some((_, answer)) => answer.await, None => std::future::pending().await } } => {
                let (request, _) = pending.take().unwrap();
                if crate::signals::terminating() { continue; }
                let mut answer = answer?;
                if let Some((plan, path)) = pending_plan.take() {
                    if answer == "edit" {
                        crate::display_commands::edit_file(std::path::Path::new(&path), "").await?;
                    }
                    if crate::terminal::plan_view::decide(&answer, false, false).rejected {
                        let width = crate::terminal::size().0.saturating_sub(1) as usize;
                        display.styled_lines(crate::terminal::plan_view::rejected(&plan, width));
                    }
                    answer = format!("plan:{answer}");
                }
                if let Some(menu) = pending_menu.take() {
                    answer = menu.answer(&answer);
                }
                answer_if_current(root, id, owner, &request, &answer)?;
                answered.insert(request);
                display.set_activity("Working");
            },
            answer = async {
                if let (Some((_, question)), Some(reader)) = (plain_pending.as_ref(), input.as_mut()) {
                    reader.read_answer(question).await
                } else { std::future::pending().await }
            } => {
                let (request, _) = plain_pending.take().unwrap();
                #[cfg(unix)]
                if answer.is_err() { crate::signals::check_terminal_hangup(); }
                if crate::signals::terminating() { continue; }
                if let Some(answer) = answer? { answer_if_current(root, id, owner, &request, &answer)?; }
                answered.insert(request);
                display.set_activity("Working");
            },
            action = display.action(), if terminal_actions_open => {
                // A closed action channel remains ready forever. Cancel once
                // and let the observer sleep between worker status checks.
                terminal_actions_open = action.is_some();
                match action {
                    Some(Action::GoalDecision { id, approved }) => {
                        if let Some(frontend) = proposal_frontend.as_ref() {
                            match frontend.decide(&id, approved, root, &session.workspace, &session.permissions) {
                                Ok(true) => display.notice(if approved { "Goal approved. It will start after the current turn finishes." } else { "Goal proposal declined. Current work continues." }),
                                Ok(false) => display.notice("That goal proposal is no longer current."),
                                Err(error) => display.notice(&format!("Goal proposal not approved: {error:#}")),
                            }
                            shown_proposal = None;
                            current_proposal = None;
                            if let Some(live) = &display.live { live.goal_proposal(None); }
                        } else {
                            display.notice("Goal proposal controls are unavailable; task output continues.");
                        }
                    },
                    Some(Action::CyclePermission) => {
                        let change = async {
                            let job = jobs::get(root, id)?;
                            ensure!(job.session_id == session.id, "Permission cycling applies to your current session; attach that session directly to change its mode");
                            let target = deepseek_code_core::services::session_worker::job_execution_session(root, &job)?;
                            let mode = if target != job.session_id {
                                let current = refresh_child_permission_mode(session, &job, &target).await?;
                                display.notice(&format!("Changing mode for owned child session {target}."));
                                match current { "ask" => "accept-edits", "accept-edits" => "plan", _ => "ask" }
                            } else { session.next_permission_mode() };
                            deepseek_code_core::services::session_worker::set_permission_mode(root, &session.id, id, mode).await?;
                            if target == job.session_id { session.permissions.set_mode(deepseek_code_core::services::session_permission_mode::parse(mode)?); }
                            Ok::<_, anyhow::Error>(mode)
                        }.await;
                        match change {
                            Ok(mode) => display.set_permission_mode(mode),
                            Err(error) => display.notice(&format!("Permission mode unchanged: {error:#}")),
                        }
                    },
                    Some(Action::Detach) => {
                        jobs::detach(root, id, owner)?;
                        display.notice(&format!("Task {id} continues in the background without restarting its request. Use /tasks attach {id} to return."));
                        if jobs::get(root, id)?.session_id == session.id {
                            display.notice("This session is occupied until completion; /clear starts a new session.");
                        }
                        return Ok(true);
                    },
                    Some(Action::Cancel) | None => {
                        if jobs::get(root, id)?.status == "completed" { return Ok(false); }
                        if !cancelled { cancel_job(session, id)?; cancelled = true; display.set_activity("Cancelling worker"); }
                    },
                    Some(Action::SideQuestion(question)) => {
                        if side.is_none() { side=Some(Box::pin(async move { crate::background_commands::side_question(session, &question).await })); }
                        else { display.notice("A side question is already running."); }
                    },
                    Some(Action::Submit(prompt)) => {
                        if crate::session_commands::is_live_inspection(&prompt) {
                            if let Some(message) = crate::session_commands::live_inspection(session, &prompt).await {
                                display.inspection(&prompt, &message);
                                display.set_model_label(&crate::session_commands::model_label(session).await);
                            }
                            continue;
                        }
                        if jobs::get(root, id)?.session_id == session.id {
                            if let Some(message) = crate::session_commands::live_goal_command(session, &prompt)? {
                                display.notice(&message);
                            } else if let Some(message) = crate::session_commands::live_inspection(session, &prompt).await {
                                // Answered here so status never waits for a long tool call.
                                if !matches!(prompt.trim(), "/effort +" | "/effort -") || !message.starts_with("Reasoning effort:") { display.inspection(&prompt, &message); }
                                display.set_model_label(&crate::session_commands::model_label(session).await);
                            } else {
                                if let Some(goal) = deepseek_code_core::services::session_goals::active(&session.store.db, &session.id)?.filter(|g|g.state == "waiting") {
                                    deepseek_code_core::services::session_goals::pause(&session.store.db, &session.id, &goal.id, "New user input interrupted the retry delay")?;
                                    jobs::cancel(root, id)?;
                                    cancelled = true;
                                }
                                if child_target.is_none() && crate::session::queues_mid_turn(&prompt) && display.is_live() {
                                    // As in the reference CLI the running turn reads it after its
                                    // next tool results; unread messages become the next prompt.
                                    crate::images::share(&session.id, &prompt);
                                    session.selected_session()?.enqueue_cli_input(deepseek_code_core::services::input_queue::QueueOrigin::User, &prompt)?;
                                    queued_inputs.push(prompt);
                                    display.queued(queued_inputs.clone());
                                } else {
                                    if let Some(notice) = crate::session_commands::queued_notice(&prompt) {
                                        display.notice(&notice);
                                    }
                                    restore(input, Action::Submit(prompt));
                                }
                                if jobs::get(root, id)?.status == "completed" { return Ok(false); }
                            }
                        } else { restore(input, Action::Submit(prompt)); }
                    },
                    Some(Action::BackgroundShell | Action::SendNow) => {
                        if child_target.is_none() {
                            deepseek_code_core::tools::background_shell::request_background(root, &session.id)?;
                        }
                    },
                    Some(Action::EditQueued) => {
                        let taken = crate::session::recall_queued(session, input, &queued_inputs)?;
                        queued_inputs.clear();
                        display.queued(Vec::new());
                        if !taken.is_empty() { display.set_draft(&taken.join("\n")); }
                    },
                    Some(Action::Queue(prompt)) => {
                        // Tracked with steered input so a later update keeps it on screen.
                        queued_inputs.push(prompt.clone());
                        restore(input, Action::Queue(prompt));
                    },
                    Some(action) => restore(input, action),
                }
            },
            answer = async { match side.as_mut() { Some(side) => side.await, None => std::future::pending().await } } => {
                side=None;
                match answer { Ok(answer)=>display.notice(&format!("Side answer:\n{answer}")), Err(error)=>display.notice(&format!("Side question failed: {error:#}")) }
            },
            _ = interrupt.interrupted(), if !termination_handled => {
                if !cancelled { cancel_job(session, id)?; cancelled = true; display.set_activity("Cancelling worker"); }
                // A termination stays signalled; after cancelling once, keep
                // observing without selecting it again, or the loop would spin
                // until the worker ends.
                termination_handled = crate::signals::terminating();
                interrupt.rearm();
            },
        }
    }
}

fn answer_if_current(root: &std::path::Path, id: &str, owner: &str, request: &str, answer: &str) -> Result<()> {
    if jobs::pending_question(root, id, owner)?.is_some_and(|question| question.request_id == request) {
        if let Err(error) = jobs::answer_question(root, id, owner, request, answer) {
            // The hook can claim between the read and atomic answer reservation.
            if jobs::pending_question(root, id, owner)?.is_some_and(|question| question.request_id == request) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn may_sync_permission_mode(
    current: &str,
    job_session: &str,
    initial_status: &str,
    foreground: bool,
) -> bool {
    current == job_session
        && (foreground || matches!(initial_status, "starting" | "running" | "unresponsive"))
}

#[test]
fn historical_attachment_does_not_refresh_permissions_but_live_attachment_can() {
    assert!(!may_sync_permission_mode(
        "session",
        "session",
        "completed",
        false
    ));
    assert!(!may_sync_permission_mode(
        "session", "session", "failed", false
    ));
    assert!(!may_sync_permission_mode(
        "session", "child", "running", false
    ));
    assert!(may_sync_permission_mode(
        "session", "session", "running", false
    ));
    assert!(may_sync_permission_mode(
        "session",
        "session",
        "completed",
        true
    ));
    let event = serde_json::json!({"type":"tool_result","name":"EnterPlanMode","output":r#"{"mode":"plan"}"#,"is_error":false});
    assert!(permission_event(&event)); // Trigger a fresh worker query, never apply this old payload.
}

fn permission_event(event: &Value) -> bool {
    event["type"] == "permission_mode" || plan_event_mode(event).is_some()
}

async fn refresh_permission_mode(session: &Session, job: &jobs::Job) -> Result<&'static str> {
    ensure!(deepseek_code_core::services::session_worker::job_execution_session(&session.paths.data_dir, job)? == session.id, "Task controls target an owned child, not the owner");
    refresh_permission_mode_with_query(
        session,
        job,
        deepseek_code_core::services::session_worker::permission_mode(
            &session.paths.data_dir,
            &session.id,
            &job.id,
        ),
    )
    .await
}

async fn refresh_child_permission_mode(session: &Session, job: &jobs::Job, child: &str) -> Result<&'static str> {
    use deepseek_code_core::services::{session_permission_mode, session_worker};
    ensure!(session_worker::job_execution_session(&session.paths.data_dir, job)? == child, "Child task target changed");
    let mode = if job.finished_ms.is_some() {
        session_permission_mode::load(&session.store.db, child)?.unwrap_or(session.permissions.get_mode())
    } else {
        let (mode, target) = session_worker::permission_mode_target(&session.paths.data_dir, &job.session_id, &job.id).await?;
        ensure!(target == child, "Permission response targets a different session");
        session_permission_mode::parse(&mode)?
    };
    Ok(session_permission_mode::name(mode))
}

async fn refresh_permission_mode_with_query(
    session: &Session,
    job: &jobs::Job,
    query: impl std::future::Future<Output = Result<String>>,
) -> Result<&'static str> {
    use deepseek_code_core::services::session_permission_mode;
    ensure!(
        job.session_id == session.id,
        "Permission mode belongs to another session"
    );
    let terminal = |job: &jobs::Job| {
        job.finished_ms.is_some()
            && matches!(job.status.as_str(), "completed" | "failed" | "cancelled")
    };
    let persisted = || {
        session_permission_mode::load(&session.store.db, &session.id)?
            .ok_or_else(|| anyhow::anyhow!("Final permission mode unavailable"))
    };
    // Historical events only trigger a query; their old mode is never applied.
    let mode = if terminal(job) {
        persisted()?
    } else {
        match query.await {
            Ok(mode) => session_permission_mode::parse(&mode)?,
            Err(error) => {
                // The worker can finish after the poll reads a running snapshot.
                // Only a fresh terminal record for this session authorizes the
                // persisted fallback; active IPC failures remain visible.
                let finished = jobs::get(&session.paths.data_dir, &job.id)
                    .is_ok_and(|latest| latest.session_id == session.id && terminal(&latest));
                if !finished {
                    return Err(error);
                }
                persisted()?
            }
        }
    };
    session.permissions.set_mode(mode);
    deepseek_code_core::permissions::set_full_access(
        session.full_access
            && mode == deepseek_code_core::permissions::PermissionMode::BypassPermissions,
    );
    Ok(session.permission_mode())
}


fn plan_event_mode(event: &Value) -> Option<deepseek_code_core::permissions::PermissionMode> {
    use deepseek_code_core::permissions::PermissionMode;
    if event["type"] != "tool_result" || event["is_error"] != false {
        return None;
    }
    let output: Value = serde_json::from_str(event["output"].as_str()?).ok()?;
    match event["name"].as_str()? {
        "EnterPlanMode" if output["mode"] == "plan" => Some(PermissionMode::PlanMode),
        "ExitPlanMode" if output["approved"] == true && output["mode"] == "ask" => {
            Some(PermissionMode::AskPermissions)
        }
        "ExitPlanMode" if output["approved"] == true && output["mode"] == "accept_edits" => {
            Some(PermissionMode::AcceptEdits)
        }
        "ExitPlanMode" if output["approved"] == true && output["mode"] == "bypass" => {
            Some(PermissionMode::BypassPermissions)
        }
        _ => None,
    }
}

#[test]
fn plan_worker_events_only_apply_successful_transitions() {
    use deepseek_code_core::permissions::PermissionMode;
    use serde_json::json;
    let event = |name, output, failed| json!({"type":"tool_result","name":name,"output":output,"is_error":failed});
    assert_eq!(
        plan_event_mode(&event("EnterPlanMode", r#"{"mode":"plan"}"#, false)),
        Some(PermissionMode::PlanMode)
    );
    assert_eq!(
        plan_event_mode(&event(
            "ExitPlanMode",
            r#"{"approved":true,"mode":"ask"}"#,
            false
        )),
        Some(PermissionMode::AskPermissions)
    );
    for (name, output, failed) in [
        ("ExitPlanMode", r#"{"approved":true,"mode":"ask"}"#, true),
        ("ExitPlanMode", r#"{"mode":"ask"}"#, false),
        ("Read", r#"{"approved":true,"mode":"ask"}"#, false),
    ] {
        assert_eq!(plan_event_mode(&event(name, output, failed)), None);
    }
}

#[test]
fn resource_alert_renders_a_system_resource_warning_notice() {
    let temp = tempfile::tempdir().unwrap();
    let mut display =
        TurnDisplay::new(false, "ask", &temp.path().join("keys.json"), vec![]).unwrap();
    render(
        &mut display,
        &serde_json::json!({
            "type": "resource_alert",
            "kind": "memory",
            "summary": "Low memory: 1.0 GiB available",
            "details": "Reduce parallel jobs and command output."
        }),
    );
    assert_eq!(
        display.take_notices(),
        vec!["● System resource warning: Low memory: 1.0 GiB available".to_string()]
    );
}

fn render(display: &mut TurnDisplay, event: &Value) {
    let text = |name: &str| event[name].as_str().unwrap_or("");
    match text("type") {
        "hook_progress" => display.hook_progress(text("hook_id"), event["status_message"].as_str()),
        "notice" => display.notice(text("message")),
        "resource_alert" => {
            let summary = text("summary");
            if !summary.is_empty() {
                display.notice(&format!("● System resource warning: {summary}"));
            }
        }
        "request_attempt" => { if let Some(message) = event["message"].as_str() { display.notice(message); } },
        "text" => display.before_text(text("delta")),
        "queued_input" => display.queued_delivered(text("text")),
        "thinking" => {
            display.record_thinking(text("delta"));
            display.thinking();
        }
        "usage" => display.reported_usage(&event["usage"]),
        "message_start" => display.set_activity("Composing"),
        "tool_start" => display.tool_started_named(text("id"), text("name"), &event["input"], event["display_title"].as_str()),
        "tool_result" => display.tool_finished(
            text("id"),
            text("name"),
            &event["input"],
            text("output"),
            event["is_error"].as_bool().unwrap_or(false),
        ),
        "agent_progress" => display.agent_progress(
            serde_json::from_value(event["address"].clone()).ok(),
            text("session_id"),
            text("agent_type"),
            text("status"),
            event["tool_name"].as_str(),
        ),
        "error" => {} // Final job state produces one concise footer; logs retain all diagnostics.
        "result" => {
            display.reported_usage(&event["usage"]);
        }
        _ => {}
    }
}

#[derive(Debug)]
struct PresentedFailure(String);
impl std::fmt::Display for PresentedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PresentedFailure {}
pub fn already_presented(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PresentedFailure>().is_some()
}

fn failure_message(id: &str, status: &str, diagnostic: &str) -> String {
    if status == "cancelled" {
        return "Interrupted".into();
    }
    let lower = diagnostic.to_ascii_lowercase();
    if lower.contains("max tool iterations") {
        return format!(
            "Stopped at the --max-turns limit; send a follow-up to continue. Details: /tasks show {id}"
        );
    }
    // Host-generated structured diagnostics carry the stage, class and the
    // number of HTTP attempts actually made. Never infer these from provider text.
    if let Some(failure) = deepseek_code_core::performance::parse_failure_summary(diagnostic) {
        return structured_failure_message(id, &failure, diagnostic);
    }
    // The task's own durable records could not be read. That is not a provider
    // failure, and the worker may still be running.
    if lower.contains("reading task state")
        || lower.contains("parsing task state")
        || lower.contains("opening task output")
        || lower.contains("reading pending question")
    {
        return format!(
            "Could not read the task state; the task may still be running. Details: /tasks show {id}"
        );
    }
    let reason = if lower.contains("timed out") || lower.contains("timeout") {
        "Request timed out".to_owned()
    } else if lower.contains("error sending request") || lower.contains("connection") {
        "Connection failed".to_owned()
    } else {
        let http = parsed_http_status(&lower);
        if http.is_none()
            && (lower.contains("output token limit")
                || (lower.contains("max_tokens")
                    && (lower.contains("stopped")
                        || lower.contains("stop_reason")
                        || lower.contains("finish_reason"))))
        {
            // Static text only: provider diagnostics can contain credentials.
            // Do not append an endpoint or truncate the task inspection command.
            return format!(
                "The response was cut off at the output token limit even after automatic continuations; send a follow-up to continue or raise the limit with --max-tokens. Details: /tasks show {id}"
            );
        }
        if http == Some(400) && lower.contains("call_id") && lower.contains("string too long") {
            return format!("HTTP 400: tool call ID exceeds the provider length limit. Details: /tasks show {id}");
        }
        http.map(|code| format!("HTTP {code}"))
            .unwrap_or_else(|| "Request failed".into())
    };
    let detail = endpoint_authority(diagnostic)
        .map(|host| format!("{reason} ({host})"))
        .unwrap_or(reason);
    format!("{detail}. Details: /tasks show {id}")
        .chars()
        .take(160)
        .collect()
}

fn parsed_http_status(lower: &str) -> Option<u16> {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| {
            ((pair[0] == "http" || pair[0] == "status" || pair[0] == "error") && pair[1].len() == 3)
                .then(|| pair[1].parse::<u16>().ok())
                .flatten()
                .filter(|code| (400..600).contains(code))
        })
}

/// The user-facing line for a host-generated `RequestFailure` diagnostic.
fn structured_failure_message(
    id: &str,
    failure: &deepseek_code_core::performance::FailureSummary,
    diagnostic: &str,
) -> String {
    let attempts = match (failure.attempts, failure.retries) {
        (Some(attempts), Some(retries)) if attempts > 1 => format!(
            " after {attempts} attempts ({retries} {})",
            if retries == 1 { "retry" } else { "retries" }
        ),
        (Some(1), _) => String::new(),
        (Some(attempts), _) => format!(" after {attempts} attempts"),
        _ => " (attempts unknown)".to_owned(),
    };
    let reason = match failure.stage.as_str() {
        "connect" => format!("Could not connect to the model service{attempts}"),
        "send" => format!("Connection failed while sending the request{attempts}"),
        "body" => format!("The response body transfer failed{attempts}"),
        "stream" => match failure.content_started {
            Some(true) => format!(
                "Response stream interrupted after partial output{attempts}; the partial output was kept and not replayed"
            ),
            Some(false) => format!("Response stream ended before any content{attempts}"),
            None => format!("Response stream interrupted{attempts}"),
        },
        "setup" => format!("The model request could not be prepared{attempts}"),
        _ => match failure.http {
            Some(code @ (401 | 403)) => format!("HTTP {code}: authentication failed"),
            Some(402) => "HTTP 402: billing error".to_owned(),
            Some(429) => format!("HTTP 429: rate limited{attempts}"),
            Some(code) if (500..=599).contains(&code) => {
                format!("HTTP {code}: server error{attempts}")
            }
            Some(code) => format!("HTTP {code}{attempts}"),
            None if failure.class == "context_limit" => "Context window exceeded".to_owned(),
            None => format!("The model service returned no usable response{attempts}"),
        },
    };
    let detail = endpoint_authority(diagnostic)
        .map(|host| format!("{reason} ({host})"))
        .unwrap_or(reason);
    format!("{detail}. Details: /tasks show {id}")
}

/// Only expose a parsed endpoint authority: credentials, path and query are discarded.
fn endpoint_authority(diagnostic: &str) -> Option<String> {
    diagnostic.split_whitespace().find_map(|word| {
        let value = word.trim_matches(|c: char| "()[]\"',".contains(c));
        let scheme = value.find("http://").or_else(|| value.find("https://"))?;
        let value = &value[scheme..];
        let authority = value
            .split_once("://")?
            .1
            .split(['/', '?', '#', ')'])
            .next()?;
        let authority = authority.rsplit('@').next()?;
        (!authority.is_empty()
            && authority.len() <= 80
            && authority
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".:-[]".contains(c)))
        .then(|| authority.to_owned())
    })
}

fn result_text(event: &Value, streamed: bool) -> Option<&str> {
    (!streamed && event["is_error"] != true)
        .then(|| event["result"].as_str())
        .flatten()
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    fn record_fixture_owners(paths: &deepseek_code_core::runtime::RuntimePaths, job: &jobs::Job) {
        let stores = paths.open_database().unwrap();
        let parent = stores.open_session(&job.parent_session_id).unwrap();
        let target = stores.open_session(&job.session_id).unwrap();
        let dir = paths.data_dir.join("tasks/background").join(&job.id);
        std::fs::write(
            dir.join("session-owners.json"),
            serde_json::json!({
                "parent":parent.incarnation(), "execution":target.incarnation()
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn removed_plan_mode_tool_results_do_not_change_mode() {
        for output in [
            r#"{"mode":"plan"}"#,
            r#"{"approved":true,"mode":"ask"}"#,
            r#"{"approved":true,"mode":"accept_edits"}"#,
            r#"{"approved":true,"mode":"bypass"}"#,
        ] {
            let event = serde_json::json!({
                "type": "tool_result",
                "name": "plan_mode",
                "output": output,
                "is_error": false,
            });
            assert_eq!(plan_event_mode(&event), None, "{output}");
        }
    }

    #[test]
    fn supported_plan_tool_results_keep_existing_transitions() {
        use deepseek_code_core::permissions::PermissionMode;

        for (name, output, expected) in [
            (
                "EnterPlanMode",
                r#"{"mode":"plan"}"#,
                PermissionMode::PlanMode,
            ),
            (
                "ExitPlanMode",
                r#"{"approved":true,"mode":"ask"}"#,
                PermissionMode::AskPermissions,
            ),
            (
                "ExitPlanMode",
                r#"{"approved":true,"mode":"accept_edits"}"#,
                PermissionMode::AcceptEdits,
            ),
            (
                "ExitPlanMode",
                r#"{"approved":true,"mode":"bypass"}"#,
                PermissionMode::BypassPermissions,
            ),
        ] {
            let event = serde_json::json!({
                "type": "tool_result",
                "name": name,
                "output": output,
                "is_error": false,
            });
            assert_eq!(plan_event_mode(&event), Some(expected), "{name}: {output}");
        }
    }

    #[tokio::test]
    async fn attachment_cannot_consume_another_tasks_goal_and_main_cancel_revokes_it() {
        use clap::Parser;
        use deepseek_code_core::services::{goal_proposals, session_goals};
        let temp = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let child = store.create(temp.path(), "fixture", "fixture").unwrap();
        let cli = Cli::try_parse_from(["dcl"]).unwrap();
        let mut provider = deepseek_code_core::runtime::provider_from_config(&crate::config::provider_template("fixture"));
        provider.id = "fixture".into();
        let session = Session::open(&cli, &paths, crate::config::Selection {
            providers: vec![provider], model: "fixture".into(), provider_id: "fixture".into(),
        }, Some(&parent)).await.unwrap();
        let frontend = goal_proposals::Frontend::acquire(session.store.db.clone(), &parent).unwrap();
        goal_proposals::propose(&session.store.db, &parent, &paths.data_dir, &session.workspace, &session.permissions,
            &serde_json::json!({"condition":"Verify requested changes"})).unwrap();
        let proposal = frontend.poll().unwrap().unwrap();
        frontend.decide(&proposal.id, true, &paths.data_dir, &session.workspace, &session.permissions).unwrap();
        drop(frontend);
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let mut job = jobs::Job { id: id.clone(), foreground: false, parent_session_id: parent.clone(), session_id: parent.clone(),
            title: "idle child".into(), status: "completed".into(), supervisor_pid: None, child_pid: None,
            started_ms: 1, heartbeat_ms: 2, finished_ms: Some(2), exit_code: Some(0), error: None, launched: true };
        let source_id = uuid::Uuid::new_v4().to_string();
        let source_directory = paths.data_dir.join("tasks/background").join(&source_id);
        std::fs::create_dir_all(&source_directory).unwrap();
        let mut source_job = job.clone();
        source_job.id = source_id.clone(); source_job.status = "running".into(); source_job.finished_ms = None;
        source_job.heartbeat_ms = chrono::Utc::now().timestamp_millis() as u64;
        std::fs::write(source_directory.join("state.json"), serde_json::to_vec(&source_job).unwrap()).unwrap();
        record_fixture_owners(&paths, &source_job);
        session.store.db.shard_sql(&parent, |conn| -> anyhow::Result<()> {
            let key = format!("session.goal-proposals.{parent}");
            let text: String = conn.query_row("SELECT value FROM peripheral_settings WHERE key=?1", [&key], |row| row.get(0))?;
            let mut value: Value = serde_json::from_str(&text)?;
            value["pending"]["source_job"] = serde_json::json!(source_id);
            conn.execute("UPDATE peripheral_settings SET value=?1 WHERE key=?2", [serde_json::to_string(&value)?, key])?;
            Ok(())
        }).unwrap().unwrap();
        std::fs::write(directory.join("state.json"), serde_json::to_vec(&job).unwrap()).unwrap();
        record_fixture_owners(&paths, &job);
        deepseek_code_core::services::session_worker::record_job_child(&paths.data_dir,&id,&store.db.open_session(&child).unwrap()).unwrap();
        std::fs::write(directory.join("output.jsonl"), "").unwrap();
        assert!(!is_main_execution(&paths.data_dir, &job, &parent).unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(3), attach(&cli, &session, &id, &mut None)).await.unwrap().unwrap();
        assert!(session_goals::active(&session.store.db, &parent).unwrap().is_none());
        // A historical main job is also not the currently running proposal source.
        std::fs::remove_file(directory.join("owned-child.json")).unwrap();
        assert!(is_main_execution(&paths.data_dir, &job, &parent).unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(3), attach(&cli, &session, &id, &mut None)).await.unwrap().unwrap();
        assert!(session_goals::active(&session.store.db, &parent).unwrap().is_none());
        deepseek_code_core::services::session_worker::record_job_child(&paths.data_dir,&id,&store.db.open_session(&child).unwrap()).unwrap();
        // Cancelling an idle child also leaves the parent's separate approval intact.
        job.status = "running".into(); job.finished_ms = None; job.heartbeat_ms = chrono::Utc::now().timestamp_millis() as u64;
        std::fs::write(directory.join("state.json"), serde_json::to_vec(&job).unwrap()).unwrap();
        cancel_job(&session, &id).unwrap();
        assert!(goal_proposals::activate(&session.store.db, &parent, &paths.data_dir, &session.workspace, &session.permissions).unwrap().is_some());
        session_goals::clear(&session.store.db, &parent).unwrap();
        let frontend = goal_proposals::Frontend::acquire(session.store.db.clone(), &parent).unwrap();
        goal_proposals::propose(&session.store.db, &parent, &paths.data_dir, &session.workspace, &session.permissions,
            &serde_json::json!({"condition":"Do remaining verification"})).unwrap();
        let proposal = frontend.poll().unwrap().unwrap();
        frontend.decide(&proposal.id, true, &paths.data_dir, &session.workspace, &session.permissions).unwrap();
        std::fs::remove_file(directory.join("owned-child.json")).unwrap();
        assert!(is_main_execution(&paths.data_dir, &job, &parent).unwrap());
        cancel_job(&session, &id).unwrap();
        assert!(goal_proposals::activate(&session.store.db, &parent, &paths.data_dir, &session.workspace, &session.permissions).unwrap().is_none());
        assert!(session_goals::active(&session.store.db, &parent).unwrap().is_none());
    }

    #[tokio::test]
    async fn attachment_checks_owner_and_reads_completed_job_without_resubmission() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let child = store.create(temp.path(), "fixture", "fixture").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let job = jobs::Job {
            id: id.clone(),
            foreground: false,
            parent_session_id: parent.clone(),
            session_id: child.clone(),
            title: "already completed".into(),
            status: "completed".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: 2,
            finished_ms: Some(2),
            exit_code: Some(0),
            error: None,
            launched: true,
        };
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let state = serde_json::to_vec(&job).unwrap();
        std::fs::write(directory.join("state.json"), &state).unwrap();
        assert!(jobs::list(&paths.data_dir, Some(&parent)).unwrap().is_empty(),
            "A session list requires a published ownership record");
        record_fixture_owners(&paths, &job);
        assert_eq!(jobs::list(&paths.data_dir, Some(&parent)).unwrap().len(), 1);
        assert!(jobs::authorize_attachment(&paths.data_dir, &job, &parent).is_ok());
        assert!(jobs::authorize_attachment(&paths.data_dir, &job, &child).is_ok());
        assert!(jobs::authorize_attachment(&paths.data_dir, &job, "unrelated").is_err());
        let events = concat!(
            "{\"type\":\"thinking\",\"delta\":\"thought\"}\n",
            "{\"type\":\"text\",\"delta\":\"existing answer\"}\n",
            "{\"type\":\"result\",\"result\":\"existing answer\"}\n"
        );
        std::fs::write(directory.join("output.jsonl"), events).unwrap();
        let cli = Cli::try_parse_from(["dcl"]).unwrap();
        for session_id in [&parent, &child] {
            let mut provider = deepseek_code_core::runtime::provider_from_config(
                &crate::config::provider_template("fixture"),
            );
            provider.id = "fixture".into();
            provider.base_url = "http://127.0.0.1:1".into();
            let mut session = Session::open(
                &cli,
                &paths,
                crate::config::Selection {
                    providers: vec![provider],
                    model: "fixture".into(),
                    provider_id: "fixture".into(),
                },
                Some(session_id),
            )
            .await
            .unwrap();
            if session_id == &parent {
                let marker = directory.join("session-owners.json");
                let original = std::fs::read(&marker).unwrap();
                let parent_inc = store
                    .db
                    .open_session(&parent)
                    .unwrap()
                    .incarnation()
                    .to_owned();
                let child_inc = store
                    .db
                    .open_session(&child)
                    .unwrap()
                    .incarnation()
                    .to_owned();
                let stale = uuid::Uuid::new_v4().to_string();
                for (owner, target) in [(&stale, &child_inc), (&parent_inc, &stale)] {
                    std::fs::write(
                        &marker,
                        serde_json::to_vec(&serde_json::json!({
                            "parent":owner, "execution":target
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                    let show = format!("show {id}");
                    assert!(crate::background_commands::handle(
                        &cli,
                        "/tasks",
                        &show,
                        crate::session::TurnOrigin::User(&show),
                        &paths,
                        &mut session,
                        &mut None
                    )
                    .await
                    .is_err());
                    assert!(crate::background_commands::live_tasks(
                        &session,
                        &format!("/tasks {show}")
                    )
                    .await
                    .is_err());
                    assert!(!crate::background_commands::live_tasks(&session, "/tasks")
                        .await
                        .unwrap()
                        .contains(&id));
                }
                std::fs::write(marker, original).unwrap();
            }
            let before = session.store.history(session_id).unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                attach(&cli, &session, &id, &mut None),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(session.store.history(session_id).unwrap(), before);
        }
        assert_eq!(std::fs::read(directory.join("state.json")).unwrap(), state);
        assert_eq!(
            std::fs::read_to_string(directory.join("output.jsonl")).unwrap(),
            events
        );
        assert!(!directory.join("cancel").exists());
        assert!(!paths.data_dir.join("state/workers").exists());
        let expected = store.db.open_session(&parent).unwrap();
        let other = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            directory.join("session-owners.json"),
            serde_json::json!({
                "parent":expected.incarnation(),"execution":other
            })
            .to_string(),
        )
        .unwrap();
        assert!(jobs::authorize_attachment(&paths.data_dir, &job, &parent).is_err());
        assert!(jobs::authorize_attachment(&paths.data_dir, &job, &child).is_err());
    }

    #[tokio::test]
    async fn failed_observation_preserves_running_job_and_original_diagnostic() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let mut provider = deepseek_code_core::runtime::provider_from_config(
            &crate::config::provider_template("fixture"),
        );
        provider.id = "fixture".into();
        let cli = Cli::try_parse_from(["dcl"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                model: "fixture".into(),
                provider_id: "fixture".into(),
            },
            Some(&parent),
        )
        .await
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: true,
            parent_session_id: parent.clone(),
            session_id: parent,
            title: "still working".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: chrono::Utc::now().timestamp_millis() as u64,
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        record_fixture_owners(&paths, &job);
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        std::fs::write(directory.join("output.jsonl"), "broken-json\n").unwrap();
        let error = attach(&cli, &session, &id, &mut None).await.unwrap_err();
        assert!(error.to_string().contains("worker continues"));
        assert_eq!(jobs::get(&paths.data_dir, &id).unwrap().status, "running");
        assert!(!directory.join("cancel").exists());
        assert!(directory.join("detached").exists());
        assert!(std::fs::read_to_string(directory.join("stderr.log"))
            .unwrap()
            .contains("Invalid background stream JSON"));
    }

    #[tokio::test]
    async fn losing_the_goal_proposal_frontend_keeps_the_task_view_and_its_output() {
        use clap::Parser;
        use deepseek_code_core::services::goal_proposals;

        let temp = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let mut provider = deepseek_code_core::runtime::provider_from_config(
            &crate::config::provider_template("fixture"),
        );
        provider.id = "fixture".into();
        let cli = Cli::try_parse_from(["dcl"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                model: "fixture".into(),
                provider_id: "fixture".into(),
            },
            Some(&parent),
        )
        .await
        .unwrap();

        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: true,
            parent_session_id: parent.clone(),
            session_id: parent.clone(),
            title: "goal proposal takeover".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: chrono::Utc::now().timestamp_millis() as u64,
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        record_fixture_owners(&paths, &job);
        std::fs::write(directory.join("state.json"), serde_json::to_vec(&job).unwrap()).unwrap();
        std::fs::write(
            directory.join("output.jsonl"),
            concat!(
                "{\"type\":\"text\",\"delta\":\"published before the takeover\\n\"}\n",
                "{\"type\":\"result\",\"result\":\"published before the takeover\"}\n"
            ),
        )
        .unwrap();

        let frontend = goal_proposals::Frontend::acquire(session.store.db.clone(), &parent).unwrap();
        // Another live frontend takes over the proposal presence while this
        // view keeps the same proposal Frontend.
        session.store.db.shard_sql(&parent, |conn| -> anyhow::Result<()> {
            let key = format!("session.goal-proposals.{parent}");
            let text: String = conn.query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [&key],
                |row| row.get(0),
            )?;
            let mut state: Value = serde_json::from_str(&text)?;
            state["frontend"] = serde_json::json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "touched_ms": chrono::Utc::now().timestamp_millis(),
            });
            conn.execute(
                "UPDATE peripheral_settings SET value=?1 WHERE key=?2",
                [serde_json::to_string(&state)?, key],
            )?;
            Ok(())
        }).unwrap().unwrap();
        assert!(frontend.poll().is_err(), "the takeover must be effective");

        // The task finishes after the view has seen the failed proposal poll.
        let completion_directory = directory.clone();
        let completion_job = job.clone();
        let completion = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let mut completed = completion_job;
            completed.status = "completed".into();
            completed.finished_ms = Some(chrono::Utc::now().timestamp_millis() as u64);
            completed.exit_code = Some(0);
            std::fs::write(
                completion_directory.join("state.json"),
                serde_json::to_vec(&completed).unwrap(),
            )
            .unwrap();
        });
        let mut display = TurnDisplay::new_with_initial(
            true,
            session.permission_mode(),
            &session.paths.layout().config_file("keybindings.json"),
            Vec::new(),
            Some(&session.paths.data_dir),
            None,
            None,
        )
        .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            present(&session, &job, &mut None, &mut display, false, Some(frontend)),
        )
        .await
        .unwrap();
        completion.await.unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(jobs::get(&paths.data_dir, &id).unwrap().status, "completed");
        assert!(!directory.join("cancel").exists(), "the job was cancelled");
        assert!(!directory.join("detached").exists(), "the view detached");
        let log = std::fs::read_to_string(directory.join("stderr.log")).unwrap();
        assert!(
            log.contains("Goal proposal observation failed"),
            "{log}"
        );
        assert!(
            log.contains("Goal proposal frontend could not be reacquired"),
            "{log}"
        );
        assert!(
            log.contains("Goal proposal observation disabled for this view"),
            "{log}"
        );
    }

    #[tokio::test]
    async fn permission_refresh_recovers_completion_race_but_preserves_active_errors() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let mut provider = deepseek_code_core::runtime::provider_from_config(
            &crate::config::provider_template("fixture"),
        );
        provider.id = "fixture".into();
        let cli = Cli::try_parse_from(["dcl"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                model: "fixture".into(),
                provider_id: "fixture".into(),
            },
            Some(&parent),
        )
        .await
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: true,
            parent_session_id: parent.clone(),
            session_id: parent,
            title: "still working".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: chrono::Utc::now().timestamp_millis() as u64,
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        use deepseek_code_core::{permissions::PermissionMode, services::session_permission_mode};
        // The poll already captured this running job. Change its durable state
        // inside the query future to deterministically reproduce the IPC race.
        let old = job.clone();
        for status in ["completed", "failed", "cancelled"] {
            session.permissions.set_mode(PermissionMode::PlanMode);
            let refreshed = refresh_permission_mode_with_query(&session, &old, async {
                session_permission_mode::set(
                    &session.store.db,
                    &session.id,
                    &session.permissions,
                    PermissionMode::AskPermissions,
                )?;
                session.permissions.set_mode(PermissionMode::PlanMode);
                let mut completed = old.clone();
                completed.status = status.into();
                completed.finished_ms = Some(3);
                std::fs::write(
                    directory.join("state.json"),
                    serde_json::to_vec(&completed)?,
                )?;
                anyhow::bail!("Session worker has no active initialized turn")
            })
            .await
            .unwrap();
            assert_eq!(refreshed, "ask");
        }
        // A still-running worker must not fall back to even a valid saved mode.
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        session.permissions.set_mode(PermissionMode::PlanMode);
        let error = refresh_permission_mode_with_query(&session, &old, async {
            anyhow::bail!("real active IPC failure")
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "real active IPC failure");
        assert_eq!(session.permission_mode(), "plan");
        // Terminal state belonging to another session is never authoritative.
        let mut foreign = old.clone();
        foreign.session_id = uuid::Uuid::new_v4().to_string();
        foreign.status = "completed".into();
        foreign.finished_ms = Some(3);
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        assert!(refresh_permission_mode_with_query(&session, &old, async {
            anyhow::bail!("wrong session")
        })
        .await
        .is_err());
        assert_eq!(session.permission_mode(), "plan");
    }

    #[test]
    fn failure_message_explains_output_limits_without_leaking_diagnostics() {
        let id = "94727627-adc5-474f-9650-8143c8a6741f";
        for diagnostic in [
            "Incomplete response: provider stopped at the output token limit (max_tokens). Partial output was retained; send a follow-up to continue.",
            "Provider stopped: MAX_TOKENS https://user:secret@example.com/private?key=secret",
            "Incomplete response: stop_reason=max_tokens; private response body",
            "Incomplete response: finish_reason=max_tokens",
            "Incomplete response: provider stopped at the output token limit (length). Partial output was retained; send a follow-up to continue.",
        ] {
            assert_eq!(
                failure_message(id, "failed", diagnostic),
                format!("The response was cut off at the output token limit even after automatic continuations; send a follow-up to continue or raise the limit with --max-tokens. Details: /tasks show {id}")
            );
        }
        for (status, diagnostic, expected) in [
            ("cancelled", "output token limit", "Interrupted"),
            (
                "failed",
                "Max tool iterations reached (session s-1)",
                "Stopped at the --max-turns limit; send a follow-up to continue. Details: /tasks show id",
            ),
            (
                "failed",
                "output token limit request timeout",
                "Request timed out. Details: /tasks show id",
            ),
            (
                "failed",
                "API HTTP 400 invalid max_tokens",
                "HTTP 400. Details: /tasks show id",
            ),
            (
                "failed",
                "HTTP 429 output token limit",
                "HTTP 429. Details: /tasks show id",
            ),
            (
                "failed",
                "unknown private credential max_tokens",
                "Request failed. Details: /tasks show id",
            ),
            (
                "failed",
                r#"Reading task state C:\Users\me\.deepseek-code\background-tasks\id\state.json: The system cannot find the file specified. (os error 2)"#,
                "Could not read the task state; the task may still be running. Details: /tasks show id",
            ),
        ] {
            assert_eq!(failure_message("id", status, diagnostic), expected);
        }
    }

    #[test]
    fn model_api_errors_show_status_and_call_id_limit_without_private_body() {
        let diagnostic = "Model API error 400: Invalid input[1].call_id: string too long secret";
        assert_eq!(failure_message("task", "failed", diagnostic), "HTTP 400: tool call ID exceeds the provider length limit. Details: /tasks show task");
        assert_eq!(failure_message("task", "failed", "Model API error 401: secret"), "HTTP 401. Details: /tasks show task");
    }

    #[test]
    fn structured_request_failures_report_stage_attempts_and_sanitized_endpoint() {
        for (diagnostic, expected) in [
            (
                "Engine execution failed: Model request failed: error sending request[stage=send; class=transient; http=unknown; attempts=4; retries=3; content_started=unknown] (session s)",
                "Connection failed while sending the request after 4 attempts (3 retries). Details: /tasks show task",
            ),
            (
                "Model request failed: request or response body error[stage=stream; class=transient; http=unknown; attempts=1; retries=0; content_started=true]",
                "Response stream interrupted after partial output; the partial output was kept and not replayed. Details: /tasks show task",
            ),
            (
                "Model API error 503: unavailable[stage=response; class=transient; http=503; attempts=4; retries=3; content_started=unknown]",
                "HTTP 503: server error after 4 attempts (3 retries). Details: /tasks show task",
            ),
            (
                "Model API error 401: denied[stage=response; class=authentication; http=401; attempts=1; retries=0; content_started=unknown]",
                "HTTP 401: authentication failed. Details: /tasks show task",
            ),
            (
                "Model request failed: error sending request for url (http://user:pw@api.example.com:8443/v1?key=secret)[stage=connect; class=transient; http=unknown; attempts=2; retries=1; content_started=unknown]",
                "Could not connect to the model service after 2 attempts (1 retry) (api.example.com:8443). Details: /tasks show task",
            ),
        ] {
            let message = failure_message("task", "failed", diagnostic);
            assert_eq!(message, expected, "{diagnostic}");
            for secret in ["user:pw", "key=secret", "/v1"] {
                assert!(!message.contains(secret), "{message}");
            }
        }
        // A provider body cannot inject retry counts into a failure line.
        assert_eq!(
            failure_message(
                "task",
                "failed",
                "Model request failed: echo [stage=stream; class=transient] tail"
            ),
            "Request failed. Details: /tasks show task"
        );
    }

    #[test]
    fn failed_results_do_not_repeat_engine_diagnostics_as_answer_text() {
        let event = serde_json::json!({"type":"result","is_error":true,"result":"Engine execution failed: API request failed: private endpoint","error":"diagnostic"});
        assert_eq!(result_text(&event, false), None);
        assert_eq!(
            failure_message("task-id", "failed", "unknown"),
            "Request failed. Details: /tasks show task-id"
        );
        assert_eq!(
            failure_message("task-id", "cancelled", "cancel"),
            "Interrupted"
        );
        let successful = serde_json::json!({"type":"result","is_error":false,"result":"answer"});
        assert_eq!(result_text(&successful, false), Some("answer"));
        assert_eq!(result_text(&successful, true), None);
        let safe=failure_message("id","failed","Engine execution failed: error sending request for url (https://user:secret@example.com:443/path?key=secret)");
        assert_eq!(
            safe,
            "Connection failed (example.com:443). Details: /tasks show id"
        );
        assert_eq!(
            failure_message("id", "failed", "API HTTP 400 secret response body"),
            "HTTP 400. Details: /tasks show id"
        );
        assert!(already_presented(&anyhow::Error::new(PresentedFailure(
            safe
        ))));
    }

    // --- A real signal delivered to the observer --------------------------
    //
    // Signals are process-wide and this binary runs its tests in one process,
    // so the observer under test runs in a child process: the ignored helper
    // below, started by both signal tests. The parent only watches the `cancel`
    // file, which `observe` writes when its signal branch actually runs.

    #[cfg(target_os = "linux")]
    const OBSERVER_HELPER_DIR: &str = "DCL_OBSERVER_HELPER_DIR";
    #[cfg(target_os = "linux")]
    const OBSERVER_NOTICE_PREFIX: &[u8] = b"OBSERVER-NOTICE-";

    #[cfg(target_os = "linux")]
    extern "C" {
        fn fcntl(fd: std::ffi::c_int, cmd: std::ffi::c_int, ...) -> std::ffi::c_int;
        fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
    }
    #[cfg(target_os = "linux")]
    const F_SETPIPE_SZ: std::ffi::c_int = 1031;
    #[cfg(target_os = "linux")]
    const F_GETPIPE_SZ: std::ffi::c_int = 1032;
    #[cfg(target_os = "linux")]
    const SIGINT: std::ffi::c_int = 2;
    #[cfg(target_os = "linux")]
    const SIGTERM: std::ffi::c_int = 15;

    /// Runs the real observer against a `running` job whose output already
    /// holds one very large notice, so printing it blocks inside the tick
    /// branch instead of returning to the select.
    ///
    /// Only a helper for the two signal tests below; without
    /// `DCL_OBSERVER_HELPER_DIR` it does nothing, so running ignored tests
    /// normally cannot signal this process.
    #[cfg(target_os = "linux")]
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observer_signal_helper() {
        use clap::Parser;
        let Some(dir) = std::env::var_os(OBSERVER_HELPER_DIR) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        // The real listeners, in this process only: the parent signals this pid.
        crate::signals::install().unwrap();
        let paths = deepseek_code_core::runtime::RuntimePaths::new(dir.join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(&dir, "fixture", "fixture").unwrap();
        let mut provider = deepseek_code_core::runtime::provider_from_config(
            &crate::config::provider_template("fixture"),
        );
        provider.id = "fixture".into();
        let cli = Cli::try_parse_from(["dcl"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                model: "fixture".into(),
                provider_id: "fixture".into(),
            },
            Some(&parent),
        )
        .await
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: true,
            parent_session_id: parent.clone(),
            session_id: parent,
            title: "observer signal".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            // Ahead of now: `jobs::get` turns a job whose heartbeat is 10 s old
            // into "unresponsive", which would end observation early.
            heartbeat_ms: chrono::Utc::now().timestamp_millis() as u64 + 600_000,
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        record_fixture_owners(&paths, &job);
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        let notice = serde_json::json!({
            "type": "notice",
            "message": format!("{}{}", "OBSERVER-NOTICE-", "N".repeat(200_000)),
        });
        std::fs::write(directory.join("output.jsonl"), format!("{notice}\n")).unwrap();
        std::fs::write(dir.join("job-id"), &id).unwrap();
        std::fs::write(
            dir.join("data-dir"),
            paths.data_dir.display().to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("ready"), b"").unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            attach(&cli, &session, &id, &mut None),
        )
        .await;
        println!("observer helper finished: {result:?}");
    }

    /// The helper child. Killing and waiting in `Drop` covers every early
    /// return and panic of the two tests below.
    #[cfg(target_os = "linux")]
    struct HelperChild(Option<std::process::Child>);

    #[cfg(target_os = "linux")]
    impl HelperChild {
        fn spawn(dir: &std::path::Path) -> Self {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "worker_frontend::failure_tests::observer_signal_helper",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(OBSERVER_HELPER_DIR, dir)
                .env_remove("DEEPSEEK_CODE_DATA_DIR")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            Self(Some(child))
        }

        fn child(&mut self) -> &mut std::process::Child {
            self.0.as_mut().unwrap()
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for HelperChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn signal_wait_for_path(path: &std::path::Path, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        path.exists()
    }

    #[cfg(target_os = "linux")]
    fn signal_wait_exit(
        child: &mut std::process::Child,
        secs: u64,
    ) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        None
    }

    /// Ends observation: `observe` returns once the job is no longer running.
    #[cfg(target_os = "linux")]
    fn signal_finish_job(directory: &std::path::Path) {
        let path = directory.join("state.json");
        let mut job: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        job["status"] = serde_json::json!("cancelled");
        job["finished_ms"] = serde_json::json!(chrono::Utc::now().timestamp_millis() as u64);
        std::fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
    }

    /// utime + stime of a pid, in clock ticks: fields 14 and 15 of
    /// `/proc/<pid>/stat`, which follow the parenthesized command name.
    #[cfg(target_os = "linux")]
    fn signal_cpu_ticks(pid: u32) -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<&str> = stat
            .rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
    }

    /// Waits until the helper has written `ready` and returns its job.
    #[cfg(target_os = "linux")]
    fn signal_helper_job(dir: &std::path::Path) -> (String, std::path::PathBuf) {
        assert!(
            signal_wait_for_path(&dir.join("ready"), 20),
            "the observer helper never became ready"
        );
        let id = std::fs::read_to_string(dir.join("job-id")).unwrap();
        let data = std::path::PathBuf::from(std::fs::read_to_string(dir.join("data-dir")).unwrap());
        (id, data)
    }

    /// A Ctrl-C delivered while the observer is printing inside a tick (not
    /// polling the select) must still cancel the job. The parent shrinks the
    /// stderr pipe to 4 KiB and stops reading at the notice prefix, so the
    /// helper is blocked in that write when the signal arrives.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sigint_during_a_tick_cancels_the_worker() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let mut helper = HelperChild::spawn(dir.path());
        let pid = helper.child().id();
        let mut stderr = helper.child().stderr.take().unwrap();
        let fd = stderr.as_raw_fd();
        // SAFETY: fcntl with an integer size is the documented ABI for this
        // command, and `fd` is the pipe read end this test still holds.
        let set = unsafe { fcntl(fd, F_SETPIPE_SZ, 4096 as std::ffi::c_int) };
        assert!(
            set > 0,
            "F_SETPIPE_SZ failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: as above, with no extra argument for the getter.
        let capacity = unsafe { fcntl(fd, F_GETPIPE_SZ) };
        assert!(
            capacity > 0 && capacity < 200_000,
            "the stderr pipe holds {capacity} bytes, not a small buffer"
        );
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buffer = [0u8; 512];
            let mut paused = false;
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if paused => {}
                    Ok(n) => {
                        seen.extend_from_slice(&buffer[..n]);
                        if seen
                            .windows(OBSERVER_NOTICE_PREFIX.len())
                            .any(|window| window == OBSERVER_NOTICE_PREFIX)
                        {
                            paused = true;
                            let _ = blocked_tx.send(());
                            let _ = release_rx.recv();
                        }
                    }
                }
            }
        });
        blocked_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the observer never blocked on the notice write");
        let (id, data) = signal_helper_job(dir.path());
        // Other runtime workers publish the signal while the observer is
        // blocked in the synchronous write.
        std::thread::sleep(std::time::Duration::from_millis(300));
        // SAFETY: sending a signal to the child this test started.
        assert_eq!(unsafe { kill(pid as std::ffi::c_int, SIGINT) }, 0);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = release_tx.send(());
        let cancel = data.join("tasks/background").join(&id).join("cancel");
        assert!(
            signal_wait_for_path(&cancel, 10),
            "a Ctrl-C received during a tick was lost: no {}",
            cancel.display()
        );
        signal_finish_job(cancel.parent().unwrap());
        assert!(
            signal_wait_exit(helper.child(), 15).is_some(),
            "the observer helper did not exit after the job ended"
        );
        let _ = reader.join();
    }

    /// SIGTERM stays signalled, so a branch that keeps re-selecting it would
    /// spin until the worker ends. After cancelling once the observer must
    /// idle: the process's CPU time barely moves over a second.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_termination_does_not_make_the_observer_spin() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let mut helper = HelperChild::spawn(dir.path());
        let pid = helper.child().id();
        let mut stderr = helper.child().stderr.take().unwrap();
        let drain = std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = stderr.read_to_end(&mut sink);
            sink
        });
        let (id, data) = signal_helper_job(dir.path());
        std::thread::sleep(std::time::Duration::from_millis(500));
        // SAFETY: sending a signal to the child this test started.
        assert_eq!(unsafe { kill(pid as std::ffi::c_int, SIGTERM) }, 0);
        let cancel = data.join("tasks/background").join(&id).join("cancel");
        assert!(
            signal_wait_for_path(&cancel, 10),
            "SIGTERM did not cancel the worker: no {}",
            cancel.display()
        );
        assert!(
            helper.child().try_wait().unwrap().is_none(),
            "the observer helper exited before the job ended"
        );
        let before = signal_cpu_ticks(pid);
        std::thread::sleep(std::time::Duration::from_secs(1));
        let after = signal_cpu_ticks(pid);
        let used = after.saturating_sub(before);
        assert!(
            used < 30,
            "the observer used {used} CPU ticks in one second ({before} -> {after}); it is spinning after the termination"
        );
        signal_finish_job(cancel.parent().unwrap());
        assert!(
            signal_wait_exit(helper.child(), 15).is_some(),
            "the observer helper did not exit after the job ended"
        );
        let _ = drain.join();
    }
}
