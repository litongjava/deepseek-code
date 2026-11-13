//! One bounded Router-host supervision pass. No scheduler or UI lives here.
//! Recovery never replays a stored tool command. It resumes the same member
//! through Launcher/child_session after durable ownership and Git checks.
use super::{
    model::*, orchestrator::Orchestrator, role::MemberRole, state::StatePolicy, store::TaskStore,
};
use crate::{
    native_engine::child_session,
    services::{
        background_sessions::{self as jobs, Job, JobSnapshot, SessionClaim},
        session_messages, session_worker,
    },
    worktree::session::git,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::Path};

#[derive(Debug, Default)]
pub struct WatchReport {
    pub resumed: Vec<String>,
    pub blocked: Vec<String>,
    pub waiting: Vec<String>,
    /// The ordinary orchestrator must not advance these tasks this pass.
    pub deferred_tasks: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RecoveryLedger {
    attempts: u32,
    first_interruption_ms: u64,
    active: Option<Recovery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recovery {
    operation_id: String,
    session_id: String,
    old_job: String,
    role: MemberRole,
    phase: Phase,
    spec: u32,
    round: u32,
    next_attempt_ms: u64,
    stop_requested: bool,
    launch_errors: u32,
    prompt: Option<String>,
    resumed_job: Option<String>,
    checkpoint_id: Option<String>,
}

pub(crate) struct WatchLock(fs::File);
impl Drop for WatchLock {
    fn drop(&mut self) {
        let _ = crate::fs::unlock_file(&self.0);
    }
}

/// Hosts call this once per tick/notification. Different processes contend on
/// the same OS lock; the durable ledger survives a host crash at every step.
pub async fn watch_once(host: &Orchestrator, router: &str) -> Result<WatchReport> {
    let Some(_lock) = lock_router(host, router)? else {
        return Ok(WatchReport {
            deferred_tasks: TaskStore::from_context(&host.context)
                .list_tasks(router, None)?
                .into_iter()
                .map(|t| t.task_id)
                .collect(),
            ..Default::default()
        });
    };
    watch_locked(host, router, false).await
}

pub(crate) fn lock_router(host: &Orchestrator, router: &str) -> Result<Option<WatchLock>> {
    ensure!(
        super::role::load(&host.context.db, router)?.is_router(),
        "Watcher requires a Router session"
    );
    host.config.validate()?;
    let directory = host
        .context
        .paths
        .data_dir
        .join("super-agent")
        .join("watchers");
    fs::create_dir_all(&directory)?;
    ensure!(
        uuid::Uuid::parse_str(router).is_ok(),
        "Invalid Router identity"
    );
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(format!("{router}.lock")))?;
    match crate::fs::try_lock_file(&file) {
        Ok(()) => Ok(Some(WatchLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Caller holds the Router lock across reconciliation, event processing and
/// advancement. The first pass only adopts jobs already started before a crash.
pub(crate) async fn watch_locked(
    host: &Orchestrator,
    router: &str,
    reconcile_only: bool,
) -> Result<WatchReport> {
    let store = TaskStore::from_context(&host.context);
    let tasks = store.list_tasks(router, None)?;
    let mut report = WatchReport::default();
    for old in tasks {
        let task = store
            .get_task(&old.task_id)?
            .context("Watcher task disappeared")?;
        if task.status == Status::Blocked {
            notify_blocked(host, &task)?;
        }
        if !matches!(task.status, Status::Running | Status::Recovering) {
            if matches!(task.status, Status::Paused | Status::Blocked) {
                report.deferred_tasks.insert(task.task_id);
            }
            continue;
        }
        if task.depends_on.iter().any(|id| {
            store
                .get_task(id)
                .map(|t| t.is_none_or(|t| t.status != Status::Completed))
                .unwrap_or(true)
        }) {
            report.deferred_tasks.insert(task.task_id.clone());
            report
                .waiting
                .push(format!("{} waits for dependencies", task.task_id));
            continue;
        }
        for role in [MemberRole::Coder, MemberRole::Reviewer] {
            let task = store
                .get_task(&task.task_id)?
                .context("Watcher task disappeared")?;
            if task.status == Status::Blocked {
                break;
            }
            if let Err(error) = watch_member(host, &task, role, &mut report, reconcile_only).await {
                block(
                    host,
                    &task,
                    role,
                    &format!("Watcher cannot prove safe recovery: {error:#}"),
                    &mut report,
                )?;
                break;
            }
        }
    }
    Ok(report)
}

fn event(
    host: &Orchestrator,
    task: &TaskRecord,
    role: MemberRole,
    kind: EventType,
    operation: &str,
    payload: Value,
) -> Result<bool> {
    let mut payload = payload;
    payload["role"] = json!(role.as_str());
    Ok(TaskStore::from_context(&host.context)
        .apply_event(
            &task.task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: kind,
                causation_id: None,
                operation_id: operation.into(),
                payload,
            },
            &StatePolicy {
                max_rounds: host.config.review.max_rounds,
            },
        )?
        .inserted)
}

fn block(
    host: &Orchestrator,
    task: &TaskRecord,
    role: MemberRole,
    reason: &str,
    report: &mut WatchReport,
) -> Result<()> {
    let id = format!(
        "watcher.block.{}",
        digest(&json!([
            task.task_id,
            task.spec_version,
            task.review_round,
            role,
            reason
        ]))
    );
    // This durable event is also the Router notification; the host returns it
    // immediately and the Router can inspect it after reopening the session.
    if event(
        host,
        task,
        role,
        EventType::TaskBlocked,
        &id,
        json!({"reason":reason,"notify_router":task.router_session_id,"source":"watcher"}),
    )? {
        report.blocked.push(format!("{}: {reason}", task.task_id));
    }
    report.deferred_tasks.insert(task.task_id.clone());
    notify_blocked(host, task)?;
    Ok(())
}

fn notify_blocked(host: &Orchestrator, task: &TaskRecord) -> Result<()> {
    for e in TaskStore::from_context(&host.context).events(&task.task_id)? {
        if e.event_type != EventType::TaskBlocked || e.payload["source"] != "watcher" {
            continue;
        }
        let sender = if e.payload["role"] == "reviewer" {
            task.reviewer.session_id.as_deref()
        } else {
            task.coder.session_id.as_deref()
        };
        if let Some(sender) = sender {
            session_messages::send_once(
                &host.context,
                &format!("{}.notice", e.operation_id),
                sender,
                &task.router_session_id,
                &format!(
                    "Super Agent Watcher blocked task {}: {}",
                    task.task_id,
                    e.payload["reason"]
                        .as_str()
                        .unwrap_or("recovery cannot proceed")
                ),
            )?;
        }
    }
    Ok(())
}

fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

fn relevant_phase(task: &TaskRecord, role: MemberRole) -> bool {
    matches!(
        (role, task.phase),
        (
            MemberRole::Coder,
            Phase::Planning | Phase::Implementing | Phase::Integrating | Phase::DeliveryPending
        ) | (
            MemberRole::Reviewer,
            Phase::Reviewing | Phase::FinalReview | Phase::Finalizing
        )
    )
}

fn stage_reported(store: &TaskStore, task: &TaskRecord, role: MemberRole) -> Result<bool> {
    let reports = store.reports(&task.task_id)?;
    let has = |kind| {
        reports.iter().any(|r| {
            r.kind == kind
                && r.spec_version == task.spec_version
                && r.round == task.review_round
                && !r.invalidated
                && (!matches!(kind, ReportKind::Review | ReportKind::Harness)
                    || r.body
                        .get("stage")
                        .and_then(Value::as_str)
                        .unwrap_or("IMPLEMENTATION")
                        == if matches!(task.phase, Phase::FinalReview | Phase::Finalizing) {
                            "FINAL_DELIVERY"
                        } else {
                            "IMPLEMENTATION"
                        })
        })
    };
    Ok(match (role, task.phase) {
        (MemberRole::Coder, Phase::Planning) => has(ReportKind::Plan),
        (MemberRole::Coder, Phase::Implementing) => has(ReportKind::Implementation),
        (MemberRole::Coder, Phase::Integrating) => has(ReportKind::Integration),
        (MemberRole::Reviewer, _) => has(ReportKind::Review) && has(ReportKind::Harness),
        _ => false,
    })
}

async fn watch_member(
    host: &Orchestrator,
    task: &TaskRecord,
    role: MemberRole,
    report: &mut WatchReport,
    reconcile_only: bool,
) -> Result<()> {
    let member = match role {
        MemberRole::Coder => &task.coder,
        MemberRole::Reviewer => &task.reviewer,
    };
    let Some(session) = member.session_id.as_deref() else {
        return Ok(());
    };
    let store = TaskStore::from_context(&host.context);
    let key = format!(
        "super-agent.watcher.{}.{}.{}",
        task.task_id,
        task.spec_version,
        role.as_str()
    );
    let mut ledger: RecoveryLedger = host.context.setting(&key)?.unwrap_or_default();
    if let Some(recovery) = ledger
        .active
        .clone()
        .filter(|r| r.resumed_job.is_none() && r.prompt.is_some())
    {
        let launcher = host.launcher();
        let op_key = launcher.operation_key_for(
            task,
            role,
            super::launcher::LaunchKind::Resume,
            &recovery.operation_id,
        );
        if let Some(op) = launcher.load_operation(&op_key)? {
            if matches!(jobs::inspect(&host.context.paths.data_dir, &op.job_id)?, JobSnapshot::Present(ref j) if j.launched)
            {
                report.deferred_tasks.insert(task.task_id.clone());
                return finish_resume(host, task, role, &key, ledger, recovery, report).await;
            }
        }
    }
    if reconcile_only || !relevant_phase(task, role) {
        return Ok(());
    }
    let owner = child_session::owner_record(&host.context, session)?
        .context("Member has no child ownership record")?;
    ensure!(
        owner.parent_session_id == task.router_session_id,
        "Member belongs to another Router"
    );
    let job_id = owner
        .job_id
        .as_deref()
        .context("Member has no associated job")?;
    let job = match jobs::inspect(&host.context.paths.data_dir, job_id)? {
        JobSnapshot::Present(job) => job,
        other => anyhow::bail!("Job {job_id} identity/state is unknown: {other:?}"),
    };
    ensure!(
        job.id == job_id
            && job.session_id == session
            && job.parent_session_id == task.router_session_id,
        "Job identity conflicts with task/member"
    );
    if let Some(reason) = waiting_reason(host, task, member, &job)? {
        report
            .waiting
            .push(format!("{} {}: {reason}", task.task_id, role.as_str()));
        report.deferred_tasks.insert(task.task_id.clone());
        return Ok(());
    }
    let owned_cancel = ledger
        .active
        .as_ref()
        .is_some_and(|r| r.old_job == job.id && r.stop_requested);
    if job.status == "cancelled" && !owned_cancel {
        report.deferred_tasks.insert(task.task_id.clone());
        return Ok(());
    }
    let phase = job_phase(host, task, role, &job, &ledger)?;
    if phase != Some(task.phase) || stage_reported(&store, task, role)? {
        if !processes_gone(&job) {
            report.deferred_tasks.insert(task.task_id.clone());
        }
        return Ok(());
    }
    if task.phase == Phase::DeliveryPending && matches!(job.status.as_str(), "completed" | "failed")
    {
        for plan in store
            .delivery_plans(&task.task_id)?
            .into_iter()
            .filter(|p| p.spec_version == task.spec_version && p.review_round == task.review_round)
        {
            let verified = super::delivery::verify_delivery(task, &plan)?;
            if verified.complete {
                return Ok(());
            }
        }
    }
    let now = jobs::now();
    let pending = pending_tools(host, session)?;
    if pending
        .iter()
        .any(|tool| matches!(tool.name.as_str(), "AskUserQuestion" | "ExitPlanMode"))
    {
        report.deferred_tasks.insert(task.task_id.clone());
        return Ok(());
    }
    let terminal = matches!(job.status.as_str(), "completed" | "failed" | "cancelled");
    let stale = now.saturating_sub(job.heartbeat_ms)
        > host
            .config
            .watcher
            .default_progress_timeout_seconds
            .saturating_mul(1000);
    // A live tool protects its owner until its declared deadline. Missing or
    // unreadable deadlines cannot be converted into proof it is safe to stop.
    let tool_running = pending.iter().any(|tool| tool.protects_live_instance(now));
    let unfinished = ledger
        .active
        .as_ref()
        .is_some_and(|r| r.resumed_job.is_none());
    // Recheck protection after backoff too: the old instance may have resumed
    // heartbeating or entered a tool since we first observed the interruption.
    if !terminal && (tool_running || (!stale && !owned_cancel)) {
        if unfinished
            || ledger
                .active
                .as_ref()
                .is_some_and(|r| r.resumed_job.as_deref() == Some(job.id.as_str()))
        {
            report.deferred_tasks.insert(task.task_id.clone());
        }
        return Ok(());
    }

    if !unfinished && !(terminal || (stale && !tool_running)) {
        if ledger
            .active
            .as_ref()
            .is_some_and(|r| r.resumed_job.as_deref() == Some(job.id.as_str()))
        {
            report.deferred_tasks.insert(task.task_id.clone());
        }
        return Ok(());
    }
    report.deferred_tasks.insert(task.task_id.clone());
    if !unfinished {
        if ledger.attempts >= host.config.watcher.max_recovery_attempts {
            return block(
                host,
                task,
                role,
                "Watcher max_recovery_attempts exhausted",
                report,
            );
        }
        if ledger.first_interruption_ms == 0 {
            ledger.first_interruption_ms = now;
        }
        ledger.attempts += 1;
        let id = format!(
            "watcher.resume.{}",
            digest(&json!([
                task.task_id,
                task.spec_version,
                task.review_round,
                role,
                session,
                job.id
            ]))
        );
        ledger.active = Some(Recovery {
            operation_id: id,
            session_id: session.into(),
            old_job: job.id.clone(),
            role,
            phase: task.phase,
            spec: task.spec_version,
            round: task.review_round,
            next_attempt_ms: now.saturating_add(backoff_ms(host, ledger.attempts)),
            stop_requested: false,
            launch_errors: 0,
            prompt: None,
            resumed_job: None,
            checkpoint_id: None,
        });
        host.context.save_setting(&key, &ledger)?;
    }
    let mut recovery = ledger.active.clone().context("Missing recovery journal")?;
    ensure!(
        recovery.session_id == session
            && recovery.spec == task.spec_version
            && recovery.round == task.review_round
            && recovery.phase == task.phase,
        "Recovery identity/stage changed"
    );
    // An after-spawn crash can leave the ownership record pointing to the new
    // job already. Launcher adopts its reserved operation instead of spawning.
    if job.id != recovery.old_job {
        let launcher = host.launcher();
        let op_key = launcher.operation_key_for(
            task,
            role,
            super::launcher::LaunchKind::Resume,
            &recovery.operation_id,
        );
        let op = launcher
            .load_operation(&op_key)?
            .context("Recovery job changed without reservation")?;
        ensure!(
            op.job_id == job.id && op.session_id == session,
            "Recovery job identity changed"
        );
        return finish_resume(host, task, role, &key, ledger, recovery, report).await;
    }
    // Do not put the task into RECOVERING while the old writer can still
    // submit a normal stage report. Stop intent is durable independently.
    if processes_gone(&job) {
        event(
            host,
            task,
            role,
            EventType::SessionInterrupted,
            &format!("{}.interrupted", recovery.operation_id),
            json!({"job_id":job.id,"reason":if terminal {"job exited before stage completion"}else{"heartbeat expired without a live tool"}}),
        )?;
    }
    if now.saturating_sub(ledger.first_interruption_ms)
        >= host
            .config
            .watcher
            .recovery_budget_seconds
            .saturating_mul(1000)
    {
        return block(
            host,
            task,
            role,
            "Watcher recovery time budget exhausted",
            report,
        );
    }
    if now < recovery.next_attempt_ms {
        return Ok(());
    }
    if !processes_gone(&job)
        || session_messages::execution_active(&host.context.paths.data_dir, session)?
    {
        if !terminal {
            if !recovery.stop_requested {
                recovery.stop_requested = true;
                ledger.active = Some(recovery.clone());
                host.context.save_setting(&key, &ledger)?;
            }
            // Reissue the idempotent signal after a crash between intent and write.
            request_stop(&host.context.paths.data_dir, &job)?;
        }
        return Ok(());
    }
    // Repair only ownership of workers whose lifetime locks/PIDs prove exit.
    session_worker::recover_session(&host.context.paths.data_dir, session)?;
    // The Router OS lock proves the previous watcher is gone. Recover only
    // our own durable operation's abandoned probe, never another owner's claim.
    SessionClaim::release(
        &host.context.paths.data_dir,
        session,
        &recovery.operation_id,
    )?;
    let claim = match SessionClaim::acquire(
        &host.context.paths.data_dir,
        session,
        &recovery.operation_id,
    ) {
        Ok(claim) => claim,
        Err(_) => return Ok(()),
    };
    let lease = session_messages::ExecutionLease::acquire(&host.context.paths.data_dir, session)?;
    if recovery.prompt.is_none() {
        let observation = reconcile_git(task, &pending)?;
        let checkpoints =
            crate::services::checkpoints::list(&host.context.paths.data_dir, session)?;
        recovery.checkpoint_id = checkpoints.last().map(|c| c.id.clone());
        recovery.prompt = Some(format!(
            "[SUPER_AGENT_RECOVERY] Continue the same task {} at {:?}, spec {} round {}. The previous instance is confirmed exited. Do not replay the previous prompt or any completed tool/commit/merge/update-ref. First read the persisted transcript and these host Git observations. Continue only remaining stage work and submit its required report. Unknown external effects must be reported as BLOCKED, never guessed. Checkpoint reference (do not restore/revert): {:?}. Host observation: {}. Unfinished tool calls (outcome unknown, not commands to retry): {}",
            task.task_id, task.phase, task.spec_version, task.review_round, recovery.checkpoint_id, observation, serde_json::to_string(&pending)?
        ));
        ledger.active = Some(recovery.clone());
        host.context.save_setting(&key, &ledger)?;
    }
    // The shared resume path rechecks execution ownership. Keep the Router
    // lock across admission; the engine takes its own lifetime execution lease.
    drop(lease);
    drop(claim);
    finish_resume(host, task, role, &key, ledger, recovery, report).await
}

fn job_phase(
    host: &Orchestrator,
    task: &TaskRecord,
    role: MemberRole,
    job: &Job,
    ledger: &RecoveryLedger,
) -> Result<Option<Phase>> {
    if let Some(r) = ledger
        .active
        .as_ref()
        .filter(|r| r.old_job == job.id || r.resumed_job.as_deref() == Some(job.id.as_str()))
    {
        return Ok(Some(r.phase));
    }
    let operations = host.context.db.with_conn(|conn| -> Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT value FROM peripheral_settings WHERE key LIKE ?1")?;
        let rows = stmt
            .query_map(
                [format!(
                    "super-agent.launch-op.{}.{}.%",
                    task.task_id,
                    role.as_str()
                )],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })??;
    for raw in operations {
        let op: super::launcher::LaunchOperation = serde_json::from_str(&raw)?;
        if op.job_id != job.id {
            continue;
        }
        ensure!(
            op.session_id == job.session_id && op.task_id == task.task_id,
            "Launch operation member mismatch"
        );
        if op.spec_version != task.spec_version || op.review_round != task.review_round {
            return Ok(None);
        }
        return Ok(match (role, op.purpose.as_str()) {
            (MemberRole::Coder, "") => Some(Phase::Planning),
            (MemberRole::Coder, "implementation" | "changes") => Some(Phase::Implementing),
            (MemberRole::Coder, "integration") => Some(Phase::Integrating),
            (MemberRole::Reviewer, "" | "implementation") => Some(Phase::Reviewing),
            (MemberRole::Reviewer, "final-delivery") => Some(Phase::FinalReview),
            (MemberRole::Reviewer, "final-completion") => Some(Phase::Finalizing),
            _ if op.delivery.is_some() => Some(Phase::DeliveryPending),
            _ => anyhow::bail!("Unknown member launch purpose {}", op.purpose),
        });
    }
    anyhow::bail!("No durable launch operation for member job {}", job.id)
}

async fn finish_resume(
    host: &Orchestrator,
    task: &TaskRecord,
    role: MemberRole,
    key: &str,
    mut ledger: RecoveryLedger,
    mut recovery: Recovery,
    report: &mut WatchReport,
) -> Result<()> {
    let prompt = recovery
        .prompt
        .as_deref()
        .context("Recovery prompt not durably prepared")?;
    match host
        .launcher()
        .resume_interrupted(task, role, prompt, &recovery.operation_id)
        .await
    {
        Ok(outcome) if !outcome.in_progress => {
            if event(
                host,
                task,
                role,
                EventType::SessionResumed,
                &format!("{}.resumed", recovery.operation_id),
                json!({"job_id":outcome.job_id,"old_job_id":recovery.old_job,"session_id":outcome.session_id,"checkpoint_id":recovery.checkpoint_id}),
            )? {
                report.resumed.push(format!(
                    "{} {} {}",
                    task.task_id,
                    role.as_str(),
                    outcome.session_id
                ));
            }
            recovery.resumed_job = Some(outcome.job_id);
            ledger.active = Some(recovery);
            host.context.save_setting(key, &ledger)?;
        }
        Ok(_) => {}
        Err(error) => {
            report.waiting.push(format!(
                "{} {} recovery launch: {error:#}",
                task.task_id,
                role.as_str()
            ));
            recovery.launch_errors += 1;
            recovery.next_attempt_ms =
                jobs::now().saturating_add(backoff_ms(host, recovery.launch_errors));
            ledger.active = Some(recovery.clone());
            host.context.save_setting(key, &ledger)?;
            if recovery.launch_errors >= host.config.watcher.max_recovery_attempts {
                block(
                    host,
                    task,
                    role,
                    &format!("Watcher launch recovery attempts exhausted: {error:#}"),
                    report,
                )?;
            }
        }
    }
    Ok(())
}

fn backoff_ms(host: &Orchestrator, attempt: u32) -> u64 {
    retry_delay_ms(&host.config.watcher, attempt)
}
fn retry_delay_ms(config: &super::config::WatcherConfig, attempt: u32) -> u64 {
    config
        .retry_backoff_seconds
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(20))
        .min(config.max_backoff_seconds)
        .saturating_mul(1000)
}

fn processes_gone(job: &Job) -> bool {
    job.supervisor_pid
        .is_some_and(|pid| pid > 0 && session_worker::process_exited(pid))
        && job
            .child_pid
            .is_some_and(|pid| pid > 0 && session_worker::process_exited(pid))
}

fn request_stop(root: &Path, expected: &Job) -> Result<()> {
    let current = jobs::get(root, &expected.id)?;
    ensure!(
        current.session_id == expected.session_id
            && current.parent_session_id == expected.parent_session_id,
        "TaskStop job identity changed"
    );
    // Same cooperative signal as TaskStop, including an expired heartbeat.
    // Never kill a PID from an untrusted or stale record.
    if matches!(current.status.as_str(), "starting" | "running") {
        jobs::cancel(root, &current.id)?;
    } else if current.status == "unresponsive" {
        fs::write(
            jobs::directory(root, &current.id)?.join("cancel"),
            b"cancel",
        )?;
    }
    Ok(())
}

fn waiting_reason(
    host: &Orchestrator,
    task: &TaskRecord,
    member: &SessionRef,
    job: &Job,
) -> Result<Option<String>> {
    if member.agent_status == AgentStatus::WaitingApproval {
        return Ok(Some("agent waits for approval".into()));
    }
    for dependency in &task.depends_on {
        if TaskStore::from_context(&host.context)
            .get_task(dependency)?
            .is_none_or(|t| t.status != Status::Completed)
        {
            return Ok(Some(format!("waiting for dependency {dependency}")));
        }
    }
    if jobs::directory(&host.context.paths.data_dir, &job.id)?
        .join("pending-question.json")
        .try_exists()?
    {
        return Ok(Some("waiting for user/permission/plan answer".into()));
    }
    let pending: bool = host.context.db.with_conn(|conn| conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM permission_approvals WHERE conversation_id=?1 AND status='pending') OR EXISTS(SELECT 1 FROM sa_approvals WHERE task_id=?2 AND status='PENDING')",
        [&job.session_id, &task.task_id], |r| r.get(0)))??;
    Ok(pending.then(|| "waiting for approval".into()))
}

#[derive(Debug, Clone, Serialize)]
struct PendingTool {
    id: String,
    name: String,
    input: Value,
    started_ms: Option<u64>,
}
impl PendingTool {
    fn protects_live_instance(&self, now: u64) -> bool {
        if matches!(self.name.as_str(), "AskUserQuestion" | "ExitPlanMode") {
            return true;
        }
        let timeout = self.input.get("timeout").and_then(Value::as_u64);
        match (self.started_ms, timeout) {
            (Some(start), Some(timeout)) if timeout > 0 => now < start.saturating_add(timeout),
            _ => true,
        }
    }
}
fn pending_tools(host: &Orchestrator, session: &str) -> Result<Vec<PendingTool>> {
    let rows = host.context.db.with_conn(|conn| -> Result<Vec<(String,String,Option<String>)>> {
        let mut stmt = conn.prepare("SELECT t.id,t.name,t.input FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 AND t.output IS NULL ORDER BY m.sort_order,t.sort_order")?;
        let rows = stmt.query_map([session], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })??;
    rows.into_iter()
        .map(|(id, name, input)| {
            // tool_calls has no exact start timestamp. A persisted first-observed
            // deadline is conservative: never cancel a live tool early by using
            // the assistant message's older creation time as its start.
            let key = format!("super-agent.watcher-tool.{}", digest(&json!([session, id])));
            let started = match host.context.setting::<u64>(&key)? {
                Some(t) => t,
                None => {
                    let t = jobs::now();
                    host.context.save_setting(&key, &t)?;
                    t
                }
            };
            Ok(PendingTool {
                id,
                name,
                input: serde_json::from_str(input.as_deref().unwrap_or("{}"))?,
                started_ms: Some(started),
            })
        })
        .collect()
}

fn reconcile_git(task: &TaskRecord, pending: &[PendingTool]) -> Result<Value> {
    ensure!(
        task.non_git_targets.is_empty(),
        "Non-Git recovery needs a verified post-edit hash baseline; refusing automatic replay"
    );
    let mut observed = Vec::new();
    for repo in &task.repositories {
        ensure!(
            super::delivery::canonical_common_dir(&repo.worktree_path)? == repo.git_common_dir,
            "Recovery worktree repository changed"
        );
        ensure!(
            git(&repo.worktree_path, &["symbolic-ref", "HEAD"])?
                == format!("refs/heads/{}", repo.task_branch),
            "Recovery worktree branch changed"
        );
        let head = git(&repo.worktree_path, &["rev-parse", "HEAD"])?;
        let target = git(
            &repo.repo_root,
            &["rev-parse", &format!("refs/heads/{}", repo.target_branch)],
        )?;
        let status = git(
            &repo.worktree_path,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ],
        )?;
        for marker in [
            "MERGE_HEAD",
            "rebase-merge",
            "rebase-apply",
            "CHERRY_PICK_HEAD",
        ] {
            let path = git(&repo.worktree_path, &["rev-parse", "--git-path", marker])?;
            ensure!(
                !repo.worktree_path.join(path).try_exists()?,
                "Unfinished Git operation {marker}; manual reconciliation required"
            );
        }
        observed.push(json!({"repository":repo.repository_id,"worktree":repo.worktree_path,"head":head,"target":target,"base":repo.base_commit,"delivered":repo.integrated_commit,"status":status}));
    }
    // Never replay an indeterminate shell/API operation. A stopped process is
    // not evidence that a compound command's remaining effects did not happen.
    ensure!(
        pending
            .iter()
            .all(|tool| matches!(tool.name.as_str(), "Read" | "Glob" | "Grep" | "ToolSearch")),
        "Unfinished side-effect tool has unknown outcome; Git observations: {}",
        json!(observed)
    );
    ensure!(
        task.phase != Phase::DeliveryPending,
        "Delivery recovery requires typed delivery reconciliation; observed Git state: {}",
        json!(observed)
    );
    Ok(json!(observed))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unfinished_tools_are_protected_until_their_observed_deadline() {
        let mut tool = PendingTool {
            id: "call".into(),
            name: "Bash".into(),
            input: json!({"timeout":5000}),
            started_ms: Some(1000),
        };
        assert!(tool.protects_live_instance(5999));
        assert!(!tool.protects_live_instance(6000));
        tool.input = json!({});
        assert!(tool.protects_live_instance(u64::MAX));
        tool.input = json!({"timeout":5000});
        tool.started_ms = None;
        assert!(tool.protects_live_instance(u64::MAX));
        tool.started_ms = Some(0);
        tool.name = "AskUserQuestion".into();
        assert!(tool.protects_live_instance(u64::MAX));
    }
    #[test]
    fn retry_backoff_is_exponential_capped_and_overflow_safe() {
        let config = super::super::config::WatcherConfig::default();
        assert_eq!(retry_delay_ms(&config, 1), 10_000);
        assert_eq!(retry_delay_ms(&config, 2), 20_000);
        assert_eq!(retry_delay_ms(&config, 3), 40_000);
        assert_eq!(retry_delay_ms(&config, 6), 300_000);
        assert_eq!(retry_delay_ms(&config, u32::MAX), 300_000);
    }
    #[test]
    fn watcher_configuration_rejects_unbounded_or_empty_budgets() {
        let mut config = super::super::config::SuperAgentConfig::default();
        assert!(config.validate().is_ok());
        config.watcher.recovery_budget_seconds = 0;
        assert!(config.validate().is_err());
        config.watcher.recovery_budget_seconds = 1;
        config.watcher.max_recovery_attempts = 21;
        assert!(config.validate().is_err());
        config.watcher.max_recovery_attempts = 0;
        assert!(config.validate().is_ok());
        config.watcher.retry_backoff_seconds = config.watcher.max_backoff_seconds + 1;
        assert!(config.validate().is_err());
    }
}
