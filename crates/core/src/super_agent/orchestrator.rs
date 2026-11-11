//! Host-owned Super Agent orchestration.
//!
//! The Router model and the CRP tools only signal intent: they append events
//! and reports. This deterministic service is what prepares a task once,
//! launches the Coder in plan mode, requests or applies the configured plan
//! approval, and resumes the same Coder afterwards. Every step is keyed by a
//! durable operation or marker, so repeated ticks cannot duplicate sessions,
//! jobs or approvals.
use crate::native_engine::provider_manager::ResolvedProvider;
use crate::services::ServiceContext;
use crate::super_agent::config::{PlanApprovalPolicy, SuperAgentConfig};
use crate::super_agent::discovery::DiscoveryOptions;
use crate::super_agent::isolation::{prepare_environment, TargetPlan};
use crate::super_agent::launcher::{
    coder_delivery_prompt, coder_implementation_prompt, coder_integration_prompt, LaunchKind,
    Launcher,
};
use crate::super_agent::model::{
    ApprovalStatus, ApprovalType, EventType, NewEvent, Phase, Report, ReportKind, ReviewStage,
    ReviewVerdict, Status, TaskRecord,
};
use crate::super_agent::state::StatePolicy;
use crate::super_agent::store::{ApprovalDraft, StoreError, TaskStore};
use anyhow::{ensure, Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct TickReport {
    /// Events the deterministic processor applied.
    pub processed: usize,
    /// `task role session` for every member process started or resumed.
    pub launched: Vec<String>,
    /// Approvals created or answered by policy during this tick.
    pub approvals: Vec<String>,
    /// Credential-free warnings, such as a model fallback.
    pub warnings: Vec<String>,
    /// Tasks the host had to block, with a short reason.
    pub blocked: Vec<String>,
}

impl TickReport {
    pub fn is_empty(&self) -> bool {
        self.processed == 0
            && self.launched.is_empty()
            && self.approvals.is_empty()
            && self.warnings.is_empty()
            && self.blocked.is_empty()
    }
}

/// Result of validating a recorded delivery job before its status is read as
/// evidence. `Blocked` carries the refreshed task so the caller never continues.
enum DeliveryJobCheck {
    Valid(crate::services::background_sessions::JobSnapshot),
    Blocked(TaskRecord),
}

pub struct Orchestrator {
    pub context: ServiceContext,
    pub config: SuperAgentConfig,
    pub provider: ResolvedProvider,
    pub permission_mode: String,
    pub executable_override: Option<PathBuf>,
    /// Test-only crash boundary forwarded to the Launcher. Production hosts
    /// pass `None` through `new`.
    pub gate: Option<crate::super_agent::launcher::LaunchGate>,
}

impl Orchestrator {
    pub fn new(
        context: ServiceContext,
        config: SuperAgentConfig,
        provider: ResolvedProvider,
        permission_mode: String,
        executable_override: Option<PathBuf>,
    ) -> Self {
        Self {
            context,
            config,
            provider,
            permission_mode,
            executable_override,
            gate: None,
        }
    }

    /// Test-only crash boundary. Hosts use `new`, which leaves it unset.
    pub fn with_gate(mut self, gate: crate::super_agent::launcher::LaunchGate) -> Self {
        self.gate = Some(gate);
        self
    }

    fn policy(&self) -> StatePolicy {
        StatePolicy {
            max_rounds: self.config.review.max_rounds,
        }
    }

    fn launcher(&self) -> Launcher {
        Launcher {
            context: self.context.clone(),
            config: self.config.clone(),
            provider: self.provider.clone(),
            permission_mode: self.permission_mode.clone(),
            executable_override: self.executable_override.clone(),
            gate: self.gate.clone(),
        }
    }

    /// The Router's real workspace plus the additional directories the user
    /// granted through the supported settings path. A task specification can
    /// only narrow this scope, never widen it.
    fn authorized_roots(&self, router_session: &str) -> Result<Vec<PathBuf>> {
        let workspace: Option<String> =
            self.context
                .db
                .with_conn(|conn| -> Result<Option<String>> {
                    Ok(
                        crate::db::conversation_repo::get_conversation(conn, router_session)?
                            .and_then(|row| row.workspace_path),
                    )
                })??;
        let workspace = PathBuf::from(
            workspace.context("Router session has no persisted workspace to authorize")?,
        );
        ensure!(
            workspace.is_dir(),
            "Router workspace is unavailable: {}",
            workspace.display()
        );
        let loaded = crate::services::permission_updates::load_settings(
            &self.context.paths.data_dir,
            &workspace,
            true,
        )?;
        let mut roots = vec![workspace];
        roots.extend(loaded.directories);
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    /// One deterministic pass. Safe to call repeatedly; it only advances work
    /// that a durable event or a missing step allows.
    pub async fn tick(&self, router_session: &str) -> Result<TickReport> {
        let store = TaskStore::from_context(&self.context);
        let policy = self.policy();
        let mut report = TickReport::default();
        report.processed += store
            .process_pending(router_session, &policy)
            .map_err(store_error)?
            .len();

        let tasks = store
            .list_tasks(router_session, None)
            .map_err(store_error)?;
        for task in tasks {
            if task.status.is_terminal() {
                continue;
            }
            match self.advance(&store, &task, &mut report).await {
                Ok(()) => {}
                Err(error) => {
                    let reason = format!("{error:#}");
                    report.blocked.push(format!("{}: {reason}", task.task_id));
                    let _ = self.block_task(&store, &task.task_id, &reason, "advance");
                }
            }
            report.processed += store
                .process_pending(router_session, &policy)
                .map_err(store_error)?
                .len();
        }
        Ok(report)
    }

    async fn advance(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        // 1. Prepare the isolated environment exactly once.
        if task.repositories.is_empty() && task.non_git_targets.is_empty() {
            self.prepare(store, task, report)?;
        }
        let mut task = store
            .get_task(&task.task_id)
            .map_err(store_error)?
            .context("Task disappeared")?;

        // 2. Start or reconcile the Coder's initial launch. The operation is
        // keyed by task/member/spec/round, so calling it on every tick is a
        // no-op once complete and recovers a crash at any earlier stage.
        if matches!(task.phase, Phase::Discovering | Phase::Planning) && task.status.is_active() {
            let outcome = self.launcher().launch_coder(&task).await?;
            if outcome.fresh {
                report
                    .launched
                    .push(format!("{} coder {}", task.task_id, outcome.session_id));
                if let Some(warning) = &outcome.warning {
                    report.warnings.push(format!("{}: {warning}", task.task_id));
                }
            }
            task = store
                .get_task(&task.task_id)
                .map_err(store_error)?
                .context("Task disappeared")?;
        }

        // 3. Turn a submitted plan into a plan approval and apply the policy.
        if task.phase == Phase::Planning && task.coder.session_id.is_some() {
            self.request_plan_approval(store, &task, report)?;
        }
        task = store
            .get_task(&task.task_id)
            .map_err(store_error)?
            .context("Task disappeared")?;

        // 4. Resume the same Coder only after its current plan is approved.
        if task.phase == Phase::Implementing && task.status.is_active() {
            self.resume_after_approval(store, &task, report).await?;
        }

        // 5. Pin the actual submitted commit and record non-Git changes before
        // the Reviewer starts.
        if task.phase == Phase::Reviewing {
            task = self.pin_submission(store, &task)?;
            self.ensure_reviewer_for_round(store, &task, report).await?;
            self.decide_review(store, &task, report)?;
            task = store
                .get_task(&task.task_id)
                .map_err(store_error)?
                .context("Task disappeared")?;
        }

        // 6. Start or reconcile the Reviewer once the submission is pinned. The
        // launch operation is idempotent, so a bound-but-not-started Reviewer is
        // recovered instead of being skipped forever.
        if task.phase == Phase::Reviewing
            && (!task.repositories.is_empty() || !task.non_git_targets.is_empty())
            && task
                .repositories
                .iter()
                .all(|repo| repo.submitted_commit.is_some())
        {
            let outcome = self.launcher().launch_reviewer(&task).await?;
            if outcome.fresh {
                report
                    .launched
                    .push(format!("{} reviewer {}", task.task_id, outcome.session_id));
                if let Some(warning) = &outcome.warning {
                    report.warnings.push(format!("{}: {warning}", task.task_id));
                }
            }
        }

        // 7. Corrections go back to the same Coder session with the exact
        // issues from the ChangesRequested review.
        if task.phase == Phase::Implementing && task.status.is_active() {
            self.resume_after_changes(store, &task, report).await?;
            task = store
                .get_task(&task.task_id)
                .map_err(store_error)?
                .context("Task disappeared")?;
        }

        // 8. Integration preparation: the Coder merges the target reference in
        // its own isolated task worktree. The host verifies the claim before
        // recording any preparation fact; nothing is delivered here.
        if task.phase == Phase::Integrating && task.status.is_active() {
            task = self.prepare_integration(store, &task, report).await?;
        }

        // 9. Integrated content that differs from the reviewed content needs a
        // new final-delivery review of the exact integrated commit.
        if task.phase == Phase::FinalReview {
            self.ensure_final_reviewer(store, &task, report).await?;
            self.decide_review(store, &task, report)?;
        }

        // 10. Stage5B2a: a SINGLE Git repository with approved, host-verified
        // integrated content is delivered through the SAME Coder session. The
        // host only verifies real Git state afterwards; the Coder writes.
        if task.phase == Phase::DeliveryPending
            && task.status.is_active()
            && self.config.delivery.enabled
        {
            self.deliver(store, &task, report).await?;
        }
        Ok(())
    }

    /// Delivers one prepared repository to its target branch through the
    /// original Coder session and records only host-verified facts.
    ///
    /// Delivery is deliberately limited to exactly one Git repository and no
    /// non-Git target. Multi-repository work stays explicitly pending rather
    /// than partially running before the recovery/completion increment.
    async fn deliver(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<TaskRecord> {
        if task.repositories.len() != 1 || !task.non_git_targets.is_empty() {
            report.warnings.push(format!(
                "{}: delivery currently supports exactly one Git repository and no non-Git target; the task stays in {}",
                task.task_id,
                task.phase.as_str()
            ));
            return Ok(task.clone());
        }
        // An already recorded immutable plan is authoritative. Re-running the
        // preflight after the Coder moved the target would compare the moved
        // ref with a stale baseline and block an already-delivered task, so the
        // plan is loaded instead of recomputed once it exists.
        let existing_plan = store
            .delivery_plans(&task.task_id)
            .map_err(store_error)?
            .into_iter()
            .filter(|plan| {
                plan.spec_version == task.spec_version
                    && plan.review_round == task.review_round
                    && plan.repositories.len() == 1
            })
            .max_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then(left.operation_id.cmp(&right.operation_id))
            });
        let plan = match existing_plan {
            Some(plan) => {
                let current = task.repositories[0].integrated_commit.as_deref();
                if current != Some(plan.repositories[0].new_oid.as_str()) {
                    let reason = format!(
                        "{}: the recorded delivery plan targets {} but the task integrated commit is {current:?}; a new plan is required",
                        task.task_id, plan.repositories[0].new_oid
                    );
                    self.block_task(store, &task.task_id, &reason, "delivery-plan-mismatch")?;
                    report.blocked.push(format!("{}: {reason}", task.task_id));
                    return store
                        .get_task(&task.task_id)
                        .map_err(store_error)?
                        .context("Task disappeared");
                }
                plan
            }
            None => match crate::super_agent::delivery::prepare_delivery(store, task)? {
                crate::super_agent::delivery::DeliveryPreflight::Planned(plan) => plan,
                crate::super_agent::delivery::DeliveryPreflight::NotApplicable => {
                    return Ok(task.clone())
                }
                crate::super_agent::delivery::DeliveryPreflight::ReintegrationNeeded { reason } => {
                    self.block_task(store, &task.task_id, &reason, "delivery-reintegration")?;
                    report.blocked.push(format!("{}: {reason}", task.task_id));
                    return store
                        .get_task(&task.task_id)
                        .map_err(store_error)?
                        .context("Task disappeared");
                }
                crate::super_agent::delivery::DeliveryPreflight::Blocked { reason } => {
                    self.block_task(store, &task.task_id, &reason, "delivery-blocked")?;
                    report.blocked.push(format!("{}: {reason}", task.task_id));
                    return store
                        .get_task(&task.task_id)
                        .map_err(store_error)?
                        .context("Task disappeared");
                }
            },
        };

        // The stored plan is only executable while it still binds the current
        // host evidence for this task version.
        if let Some(reason) = crate::super_agent::delivery::verify_plan_binding(store, task, &plan)?
        {
            return self.block_delivery(store, &task.task_id, &reason, "delivery-binding", report);
        }

        // A dispatch already exists: reconcile only against it. Never prepare a
        // second plan or start a second child for the same immutable evidence.
        if let Some(dispatch) = store
            .repository_dispatch(
                &task.task_id,
                &plan.operation_id,
                &plan.repositories[0].repository_id,
            )
            .map_err(store_error)?
        {
            return self
                .reconcile_delivery(store, task, &plan, &dispatch, report)
                .await;
        }

        // No child has been started for this plan yet. A target that already
        // matches the approved content is recorded as an observation instead of
        // being rewritten.
        let observed = crate::super_agent::delivery::verify_delivery(task, &plan)?;
        if observed.complete {
            return self.finish_delivery(store, task, &plan, None, true, report);
        }
        // Carrying the plan is not ownership: an explicitly released or stolen
        // branch must block before any child is started.
        if let Some(reason) =
            crate::super_agent::delivery::delivery_ownership(store, &plan)?.reason()
        {
            return self.block_delivery(
                store,
                &task.task_id,
                &reason,
                "delivery-ownership",
                report,
            );
        }
        match crate::super_agent::delivery::revalidate_for_dispatch(task, &plan)? {
            crate::super_agent::delivery::DispatchRevalidation::Ready => {}
            crate::super_agent::delivery::DispatchRevalidation::ReintegrationNeeded(reason) => {
                return self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-reintegration",
                    report,
                );
            }
            crate::super_agent::delivery::DispatchRevalidation::Blocked(reason) => {
                return self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-blocked",
                    report,
                );
            }
        }

        // The dispatch receipt is durable before the child is started. It is
        // keyed by the real repository and binds the approved old/new OIDs and
        // the explicit attempt, not only the plan operation ID.
        let target = &plan.repositories[0];
        store
            .begin_repository_dispatch(&crate::super_agent::delivery::DeliveryDispatch {
                task_id: task.task_id.clone(),
                spec_version: task.spec_version,
                review_round: task.review_round,
                plan_operation_id: plan.operation_id.clone(),
                coder_session_id: task
                    .coder
                    .session_id
                    .clone()
                    .context("Task has no Coder session to deliver the integration")?,
                purpose: "delivery".into(),
                repository_id: Some(target.repository_id.clone()),
                old_oid: Some(target.old_oid.clone()),
                new_oid: Some(target.new_oid.clone()),
                attempt: target.attempt,
                job_id: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .map_err(store_error)?;
        let instruction = coder_delivery_prompt(task, &plan);
        let outcome = self
            .launcher()
            .resume_coder(task, &instruction, "delivery")
            .await?;
        store
            .set_repository_dispatch_job(
                &task.task_id,
                &plan.operation_id,
                &target.repository_id,
                &outcome.job_id,
            )
            .map_err(store_error)?;
        self.observe_delivery_job(
            store,
            task,
            &plan,
            &outcome.job_id,
            outcome.in_progress,
            report,
        )
        .await
    }

    /// Reconciles an already dispatched delivery. Only the recorded plan and
    /// the real target decide the result; the preflight is never re-run after
    /// the side effect.
    async fn reconcile_delivery(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        plan: &crate::super_agent::delivery::DeliveryPlan,
        dispatch: &crate::super_agent::delivery::DeliveryDispatch,
        report: &mut TickReport,
    ) -> Result<TaskRecord> {
        // The plan, receipt and task version are immutable host facts. Validate
        // them together before any branch can adopt a child or observe a status.
        if let Some(reason) = delivery_context_mismatch(task, plan, dispatch) {
            return self.block_delivery(store, &task.task_id, &reason, "delivery-identity", report);
        }
        // A recorded job is observed read-only: never resume the Coder just
        // because the host reopened the database, which could start a second
        // child from a stale plan.
        if let Some(job_id) = &dispatch.job_id {
            let job_id = job_id.clone();
            // The recorded ID is only a pointer. Read the actual job and prove
            // its child and Router identities before its status can decide
            // delivery.
            match self.check_delivery_job(
                store,
                task,
                &job_id,
                &dispatch.coder_session_id,
                report,
            )? {
                DeliveryJobCheck::Blocked(blocked) => return Ok(blocked),
                DeliveryJobCheck::Valid(_) => {}
            }
            return self
                .observe_delivery_job(store, task, plan, &job_id, false, report)
                .await;
        }
        // The receipt exists but no job was bound: the previous host stopped
        // between reserving the launch operation and recording its job. The
        // launcher's operation record is authoritative, so an already reserved
        // or running child is adopted instead of launched again.
        let key = self.launcher().operation_key_for(
            task,
            crate::super_agent::role::MemberRole::Coder,
            LaunchKind::Resume,
            "delivery",
        );
        let recorded = match self.launcher().load_operation(&key) {
            Ok(value) => value,
            Err(error) => {
                let reason = format!(
                    "The delivery launch record {key} cannot be read ({error:#}); the original record was preserved"
                );
                return self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-operation-unreadable",
                    report,
                );
            }
        };
        if let Some(operation) = recorded.as_ref() {
            // A matching storage key is not proof that the contents belong to
            // this delivery. Validate every immutable identity before any
            // adoption, receipt mutation or observation.
            if let Some(reason) = delivery_operation_mismatch(task, plan, dispatch, operation) {
                return self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-operation-mismatch",
                    report,
                );
            }
            // A reserved operation whose job already exists is authoritative
            // even when the previous host crashed before persisting its handle.
            // Adopt it instead of re-validating the old baseline that its own
            // child may already have moved.
            if operation.handle.is_none() {
                match self.check_delivery_job(
                    store,
                    task,
                    &operation.job_id,
                    &operation.session_id,
                    report,
                )? {
                    DeliveryJobCheck::Blocked(blocked) => return Ok(blocked),
                    DeliveryJobCheck::Valid(
                        crate::services::background_sessions::JobSnapshot::Present(job),
                    ) => {
                        let progressed =
                            job.launched || job.supervisor_pid.is_some() || job.child_pid.is_some();
                        if progressed {
                            store
                                .set_repository_dispatch_job(
                                    &task.task_id,
                                    &plan.operation_id,
                                    &plan.repositories[0].repository_id,
                                    &job.id,
                                )
                                .map_err(store_error)?;
                            return self
                                .observe_delivery_job(store, task, plan, &job.id, false, report)
                                .await;
                        }
                        // The reservation reached disk but no child was ever
                        // authorized; the launcher's own recovery continues it.
                    }
                    // A missing job continues the launcher's reservation; an
                    // unreadable one was already blocked by the check.
                    DeliveryJobCheck::Valid(_) => {}
                }
            }
            if let Some(handle) = operation.handle.as_ref() {
                if let Some(reason) = delivery_handle_mismatch(operation, dispatch, handle) {
                    return self.block_delivery(
                        store,
                        &task.task_id,
                        &reason,
                        "delivery-child-mismatch",
                        report,
                    );
                }
                // Validate the job the handle points at, not only the handle.
                match self.check_delivery_job(
                    store,
                    task,
                    &handle.job_id,
                    &handle.session_id,
                    report,
                )? {
                    DeliveryJobCheck::Blocked(blocked) => return Ok(blocked),
                    DeliveryJobCheck::Valid(_) => {}
                }
                let job_id = handle.job_id.clone();
                store
                    .set_repository_dispatch_job(
                        &task.task_id,
                        &plan.operation_id,
                        &plan.repositories[0].repository_id,
                        &job_id,
                    )
                    .map_err(store_error)?;
                return self
                    .observe_delivery_job(store, task, plan, &job_id, false, report)
                    .await;
            }
        }
        // No launch was ever recorded for this operation. If the real target
        // already proves the delivery, record that observation instead of
        // running the command again.
        if recorded.is_none() {
            let observed = crate::super_agent::delivery::verify_delivery(task, plan)?;
            if observed.complete {
                return self.finish_delivery(store, task, plan, None, true, report);
            }
        }
        // The receipt exists but no job was recorded: the previous host stopped
        // before the launcher returned one. This is still a first launch, so it
        // needs current ownership.
        if let Some(reason) =
            crate::super_agent::delivery::delivery_ownership(store, plan)?.reason()
        {
            return self.block_delivery(
                store,
                &task.task_id,
                &reason,
                "delivery-ownership",
                report,
            );
        }
        // A genuinely fresh launch repeats the live baseline/checkout
        // preflight. An adopted operation was handled above and is never
        // re-validated against the old OID that its own write already moved.
        match crate::super_agent::delivery::revalidate_for_dispatch(task, plan)? {
            crate::super_agent::delivery::DispatchRevalidation::Ready => {}
            crate::super_agent::delivery::DispatchRevalidation::ReintegrationNeeded(reason) => {
                return self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-reintegration",
                    report,
                );
            }
            crate::super_agent::delivery::DispatchRevalidation::Blocked(reason) => {
                return self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-blocked",
                    report,
                );
            }
        }
        let instruction = coder_delivery_prompt(task, plan);
        let outcome = self
            .launcher()
            .resume_coder(task, &instruction, "delivery")
            .await?;
        store
            .set_repository_dispatch_job(
                &task.task_id,
                &plan.operation_id,
                &plan.repositories[0].repository_id,
                &outcome.job_id,
            )
            .map_err(store_error)?;
        self.observe_delivery_job(
            store,
            task,
            plan,
            &outcome.job_id,
            outcome.in_progress,
            report,
        )
        .await
    }

    /// Reads and validates a recorded delivery job without treating its status
    /// as evidence. A mismatched or unreadable record blocks; the files stay.
    /// Reads a recorded delivery job and validates its identity before its
    /// status can be used. A malformed ID, an inspection error, unreadable
    /// state, or a mismatched child/Router blocks with the files preserved.
    fn check_delivery_job(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        job_id: &str,
        session_id: &str,
        report: &mut TickReport,
    ) -> Result<DeliveryJobCheck> {
        let snapshot = match crate::services::background_sessions::inspect(
            &self.context.paths.data_dir,
            job_id,
        ) {
            Ok(value) => value,
            Err(error) => {
                let reason = format!(
                    "The delivery background task {job_id} cannot be read ({error:#}); the original files were preserved"
                );
                let blocked = self.block_delivery(
                    store,
                    &task.task_id,
                    &reason,
                    "delivery-job-unreadable",
                    report,
                )?;
                return Ok(DeliveryJobCheck::Blocked(blocked));
            }
        };
        match &snapshot {
            crate::services::background_sessions::JobSnapshot::Present(job) => {
                if let Some(reason) = delivery_job_identity_mismatch(task, job_id, session_id, job)
                {
                    let blocked = self.block_delivery(
                        store,
                        &task.task_id,
                        &reason,
                        "delivery-job-mismatch",
                        report,
                    )?;
                    return Ok(DeliveryJobCheck::Blocked(blocked));
                }
            }
            crate::services::background_sessions::JobSnapshot::Unreadable(reason) => {
                let error = format!(
                    "The delivery background task {job_id} has unreadable state ({reason}); the original files were preserved"
                );
                let blocked = self.block_delivery(
                    store,
                    &task.task_id,
                    &error,
                    "delivery-job-unreadable",
                    report,
                )?;
                return Ok(DeliveryJobCheck::Blocked(blocked));
            }
            // `observe_delivery_job` reports a missing job explicitly.
            crate::services::background_sessions::JobSnapshot::Missing => {}
        }
        Ok(DeliveryJobCheck::Valid(snapshot))
    }

    /// Blocks a delivery with a user-readable reason and returns the refreshed
    /// task. No Git ref or worktree is touched.
    fn block_delivery(
        &self,
        store: &TaskStore,
        task_id: &str,
        reason: &str,
        key: &str,
        report: &mut TickReport,
    ) -> Result<TaskRecord> {
        self.block_task(store, task_id, reason, key)?;
        report.blocked.push(format!("{task_id}: {reason}"));
        store
            .get_task(task_id)
            .map_err(store_error)?
            .context("Task disappeared")
    }

    /// Reads the dispatched job without ever inferring success from a member
    /// report. A running child is never declared finished, even if the target
    /// temporarily matches; only a stopped child is verified against the
    /// pinned plan and the current checkout.
    async fn observe_delivery_job(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        plan: &crate::super_agent::delivery::DeliveryPlan,
        job_id: &str,
        in_progress: bool,
        report: &mut TickReport,
    ) -> Result<TaskRecord> {
        if in_progress {
            return Ok(task.clone());
        }
        match crate::services::background_sessions::inspect(&self.context.paths.data_dir, job_id)? {
            crate::services::background_sessions::JobSnapshot::Unreadable(reason) => {
                let error = format!(
                    "Delivery background task {job_id} has unreadable state ({reason}); the original files were preserved"
                );
                self.block_task(store, &task.task_id, &error, "delivery-job-unreadable")?;
                report.blocked.push(format!("{}: {error}", task.task_id));
                store
                    .get_task(&task.task_id)
                    .map_err(store_error)?
                    .context("Task disappeared")
            }
            crate::services::background_sessions::JobSnapshot::Missing => {
                let error = format!(
                    "Delivery background task {job_id} produced no state; the target was not verified"
                );
                self.block_task(store, &task.task_id, &error, "delivery-job-missing")?;
                report.blocked.push(format!("{}: {error}", task.task_id));
                store
                    .get_task(&task.task_id)
                    .map_err(store_error)?
                    .context("Task disappeared")
            }
            crate::services::background_sessions::JobSnapshot::Present(job) => {
                if matches!(job.status.as_str(), "starting" | "running" | "unresponsive") {
                    return Ok(task.clone());
                }
                let verification = crate::super_agent::delivery::verify_delivery(task, plan)?;
                if verification.complete {
                    return self.finish_delivery(store, task, plan, Some(job_id), false, report);
                }
                let detail = if verification.failures.is_empty() {
                    "the target does not contain the approved content".to_owned()
                } else {
                    verification.failures.join("; ")
                };
                let reason = if job.status == "completed" {
                    format!(
                        "The Coder delivery job {job_id} finished but the target branch was not delivered: {detail}"
                    )
                } else {
                    format!(
                        "The Coder delivery job {job_id} ended {}: {detail}",
                        job.status
                    )
                };
                self.block_task(store, &task.task_id, &reason, "delivery-not-applied")?;
                report.blocked.push(format!("{}: {reason}", task.task_id));
                store
                    .get_task(&task.task_id)
                    .map_err(store_error)?
                    .context("Task disappeared")
            }
        }
    }

    /// Persists the host-verified delivery observation. Each planned repository
    /// gets its own durable observation first; only then may the whole task be
    /// marked delivered. It never emits task completion or cleanup.
    fn finish_delivery(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        plan: &crate::super_agent::delivery::DeliveryPlan,
        job_id: Option<&str>,
        observed_without_dispatch: bool,
        report: &mut TickReport,
    ) -> Result<TaskRecord> {
        let coder_session_id = task
            .coder
            .session_id
            .clone()
            .context("Task has no Coder session to record the delivery")?;
        for target in &plan.repositories {
            store
                .record_repository_delivery_observed(
                    &crate::super_agent::delivery::RepositoryDeliveryObservation {
                        task_id: plan.task_id.clone(),
                        spec_version: plan.spec_version,
                        review_round: plan.review_round,
                        plan_operation_id: plan.operation_id.clone(),
                        repository_id: target.repository_id.clone(),
                        old_oid: target.old_oid.clone(),
                        new_oid: target.new_oid.clone(),
                        target_ref: target.target_ref.clone(),
                        coder_session_id: coder_session_id.clone(),
                        attempt: target.attempt,
                        verified: true,
                        observed_at: chrono::Utc::now().to_rfc3339(),
                    },
                )
                .map_err(store_error)?;
        }
        let repositories: Vec<serde_json::Value> = plan
            .repositories
            .iter()
            .map(|target| {
                serde_json::json!({
                    "repository_id": target.repository_id,
                    "target_ref": target.target_ref,
                    "old_oid": target.old_oid,
                    "new_oid": target.new_oid,
                    "checkout": match &target.destination {
                        crate::super_agent::delivery::DeliveryDestination::CheckedOut { path } => serde_json::json!(path),
                        crate::super_agent::delivery::DeliveryDestination::RefOnly => serde_json::Value::Null,
                    },
                })
            })
            .collect();
        let operation = format!(
            "orchestrator:{}:delivery-completed:{}:{}:{}",
            task.task_id, task.spec_version, task.review_round, plan.operation_id
        );
        store
            .record_delivery_completed(
                plan,
                &operation,
                serde_json::json!({
                    "complete": true,
                    "plan_operation_id": plan.operation_id,
                    "dispatch_job_id": job_id,
                    "observed_without_dispatch": observed_without_dispatch,
                    "repositories": repositories,
                }),
            )
            .map_err(store_error)?;
        report.processed += store
            .process_pending(&task.router_session_id, &self.policy())
            .map_err(store_error)?
            .len();
        store
            .get_task(&task.task_id)
            .map_err(store_error)?
            .context("Task disappeared")
    }

    fn prepare(&self, store: &TaskStore, task: &TaskRecord, report: &mut TickReport) -> Result<()> {
        // Authorization comes from the Router session's real persisted
        // workspace and the directories the user granted in settings, never
        // from the model-supplied specification itself.
        let roots = self.authorized_roots(&task.router_session_id)?;
        let workspace =
            crate::super_agent::discovery::resolve_authorized_path(&task.spec.workspace, &roots)
                .context("Task workspace is outside the Router's authorized scope")?;
        ensure!(
            workspace.is_dir(),
            "Task workspace is unavailable: {}",
            workspace.display()
        );
        let mut plans = Vec::new();
        for target in &task.spec.targets {
            let path = PathBuf::from(target);
            if !path.is_absolute() {
                anyhow::bail!("Task target must be absolute: {target}");
            }
            crate::super_agent::discovery::resolve_authorized_path(&path, &roots)
                .with_context(|| format!("Task target {target} is outside the authorized scope"))?;
            let discovery = crate::super_agent::discovery::discover_target(
                &path,
                &DiscoveryOptions {
                    authorized_roots: roots.clone(),
                    ..Default::default()
                },
            );
            let target_branch = task
                .spec
                .repository_target_branches
                .get(target)
                .cloned()
                .or_else(|| task.spec.target_branch.clone());
            plans.push(TargetPlan {
                discovery,
                target_branch,
            });
        }
        let environment = prepare_environment(
            &task.task_id,
            &self.context.paths.data_dir,
            &plans,
            Some(store),
            &format!("orchestrator:{}", task.task_id),
        )?;
        if environment.repositories.is_empty() && environment.non_git_targets.is_empty() {
            anyhow::bail!("No usable task targets");
        }
        self.ingest(
            store,
            &task.task_id,
            EventType::DiscoveryCompleted,
            format!("orchestrator:{}:discovery", task.task_id),
            serde_json::json!({"repositories":environment.repositories.len(),"non_git":environment.non_git_targets.len()}),
        )?;
        report.processed += 1;
        Ok(())
    }

    fn request_plan_approval(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let plan = reports.iter().any(|candidate| {
            candidate.kind == ReportKind::Plan
                && !candidate.invalidated
                && candidate.spec_version == task.spec_version
                && candidate.round == task.review_round
        });
        if !plan {
            return Ok(());
        }
        let approvals = store.approvals(&task.task_id).map_err(store_error)?;
        let current = approvals.iter().find(|approval| {
            approval.request_type == ApprovalType::Plan
                && approval.task_spec_version == task.spec_version
                && approval
                    .request
                    .get("round")
                    .and_then(serde_json::Value::as_u64)
                    == Some(task.review_round as u64)
        });
        if let Some(approval) = current {
            if approval.status == ApprovalStatus::Approved {
                return Ok(());
            }
            if approval.status != ApprovalStatus::Pending {
                return Ok(());
            }
            if self.config.approvals.plan == PlanApprovalPolicy::Auto {
                self.auto_approve(store, task, approval.approval_id.clone(), report)?;
            }
            return Ok(());
        }
        let operation = format!(
            "orchestrator:{}:plan-approval:{}:{}",
            task.task_id, task.spec_version, task.review_round
        );
        let approval = store
            .request_approval(
                &operation,
                &task.task_id,
                ApprovalDraft {
                    source_session_id: task
                        .coder
                        .session_id
                        .clone()
                        .context("Plan approval without a Coder session")?,
                    parent_session_id: task.router_session_id.clone(),
                    request_type: ApprovalType::Plan,
                    request: serde_json::json!({
                        "task_id": task.task_id,
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "report_id": reports.iter().find(|report| report.kind == ReportKind::Plan && !report.invalidated).map(|report| report.report_id.clone())
                    }),
                    task_spec_version: task.spec_version,
                    permission_policy_version: task.permission_policy_version,
                },
            )
            .map_err(store_error)?;
        self.ingest(
            store,
            &task.task_id,
            EventType::ApprovalRequested,
            format!("{operation}:requested"),
            serde_json::json!({"request_type":"PLAN","approval_id":approval.approval_id}),
        )?;
        report.approvals.push(approval.approval_id.clone());
        if self.config.approvals.plan == PlanApprovalPolicy::Auto {
            self.auto_approve(store, task, approval.approval_id, report)?;
        }
        Ok(())
    }

    fn auto_approve(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        approval_id: String,
        report: &mut TickReport,
    ) -> Result<()> {
        let operation = format!("orchestrator:{}:auto-approve:{approval_id}", task.task_id);
        store
            .resolve_approval(
                &operation,
                &approval_id,
                ApprovalStatus::Approved,
                Some("Auto policy: the plan satisfied the configured approval policy"),
            )
            .map_err(store_error)?;
        self.ingest(
            store,
            &task.task_id,
            EventType::ApprovalResolved,
            format!("{operation}:resolved"),
            serde_json::json!({
                "approval_id": approval_id,
                "request_type": "PLAN",
                "decision": "APPROVED",
                "policy": "auto"
            }),
        )?;
        // Processing the answer is what moves the task into implementation.
        let policy = self.policy();
        report.processed += store
            .process_pending(&task.router_session_id, &policy)
            .map_err(store_error)?
            .len();
        Ok(())
    }

    async fn resume_after_approval(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        let approvals = store.approvals(&task.task_id).map_err(store_error)?;
        let approved = approvals.iter().any(|approval| {
            approval.request_type == ApprovalType::Plan
                && approval.status == ApprovalStatus::Approved
                && approval.task_spec_version == task.spec_version
                && approval.permission_policy_version == task.permission_policy_version
                && approval
                    .request
                    .get("round")
                    .and_then(serde_json::Value::as_u64)
                    == Some(task.review_round as u64)
        });
        if !approved {
            return Ok(());
        }
        let key = format!(
            "super-agent.resume.{}.{}.{}",
            task.task_id, task.spec_version, task.review_round
        );
        if self.context.setting::<serde_json::Value>(&key)?.is_some() {
            return Ok(());
        }
        let outcome = self
            .launcher()
            .resume_coder(task, &coder_implementation_prompt(task), "implementation")
            .await?;
        self.context.save_setting(
            &key,
            &serde_json::json!({
                "session_id": outcome.session_id,
                "job_id": outcome.job_id,
                "at": chrono::Utc::now().to_rfc3339()
            }),
        )?;
        if outcome.fresh {
            report.launched.push(format!(
                "{} coder {} (resumed)",
                task.task_id, outcome.session_id
            ));
            if let Some(warning) = outcome.warning {
                report.warnings.push(format!("{}: {warning}", task.task_id));
            }
        }
        Ok(())
    }

    fn pin_submission(&self, store: &TaskStore, task: &TaskRecord) -> Result<TaskRecord> {
        for repo in &task.repositories {
            let status =
                crate::worktree::session::command(&repo.working_path, &["status", "--porcelain"])?;
            ensure!(
                status.status.success(),
                "Cannot read the status of {}: {}",
                repo.working_path.display(),
                String::from_utf8_lossy(&status.stderr).trim()
            );
            ensure!(
                status.stdout.is_empty(),
                "The task worktree {} has uncommitted changes; commit the delivery before submitting",
                repo.working_path.display()
            );
            let head =
                crate::worktree::session::command(&repo.working_path, &["rev-parse", "HEAD"])?;
            ensure!(
                head.status.success(),
                "Cannot read HEAD of {}: {}",
                repo.working_path.display(),
                String::from_utf8_lossy(&head.stderr).trim()
            );
            let commit = String::from_utf8_lossy(&head.stdout).trim().to_owned();
            ensure!(!commit.is_empty(), "Task worktree HEAD is empty");
            if repo.submitted_commit.as_deref() != Some(commit.as_str()) {
                let mut updated = repo.clone();
                updated.submitted_commit = Some(commit);
                store.save_repo(&updated).map_err(store_error)?;
            }
        }
        for target in &task.non_git_targets {
            crate::super_agent::review::record_non_git_changes(store, task, target)?;
        }
        store
            .get_task(&task.task_id)
            .map_err(store_error)?
            .context("Task disappeared")
    }

    async fn ensure_reviewer_for_round(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        if task.review_round == 0 {
            // The initial Reviewer launch is handled by `advance`; only a later
            // round needs an explicit resume with a new review request.
            return Ok(());
        }
        let Some(reviewer) = task.reviewer.session_id.as_deref() else {
            return Ok(());
        };
        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let current = |kind| {
            reports.iter().any(|report| {
                report.kind == kind
                    && !report.invalidated
                    && report.spec_version == task.spec_version
                    && report.round == task.review_round
            })
        };
        if !current(ReportKind::Implementation)
            || (current(ReportKind::Review) && current(ReportKind::Harness))
        {
            return Ok(());
        }
        let key = format!(
            "super-agent.reviewer-resume.{}.{}.{}",
            task.task_id, task.spec_version, task.review_round
        );
        if self.context.setting::<serde_json::Value>(&key)?.is_some() {
            return Ok(());
        }
        let outcome = self.launcher().resume_reviewer(task).await?;
        self.context.save_setting(
            &key,
            &serde_json::json!({
                "session_id": outcome.session_id,
                "job_id": outcome.job_id,
                "at": chrono::Utc::now().to_rfc3339()
            }),
        )?;
        if outcome.fresh {
            report.launched.push(format!(
                "{} reviewer {} (round {})",
                task.task_id, reviewer, task.review_round
            ));
        }
        Ok(())
    }

    fn decide_review(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        // The phase decides which review evidence is current. An
        // implementation review and its harness can never decide the final
        // delivery review, even at the same spec version and review round.
        let expected = match task.phase {
            Phase::Reviewing => ReviewStage::Implementation,
            Phase::FinalReview => ReviewStage::FinalDelivery,
            _ => return Ok(()),
        };
        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let current: Vec<&Report> = reports
            .iter()
            .filter(|report| {
                !report.invalidated
                    && report.spec_version == task.spec_version
                    && report.round == task.review_round
                    && report_stage(report) == Some(expected)
            })
            .collect();
        let review_report = current
            .iter()
            .rev()
            .find(|report| report.kind == ReportKind::Review);
        let harness_report = current
            .iter()
            .rev()
            .find(|report| report.kind == ReportKind::Harness);
        let (Some(review_report), Some(harness_report)) = (review_report, harness_report) else {
            return Ok(());
        };
        let events = store.events(&task.task_id).map_err(store_error)?;
        let decided = events.iter().any(|event| {
            event.event_type == EventType::ReviewCompleted
                && event.spec_version == task.spec_version
                && event.review_round == task.review_round
                && event.payload["stage"].as_str() == Some(expected.as_str())
        });
        if decided {
            return Ok(());
        }
        let review: crate::super_agent::TaskReviewReport =
            crate::super_agent::model::decode_body(review_report)?;
        let harness: crate::super_agent::HarnessAnalysisReport =
            crate::super_agent::model::decode_body(harness_report)?;
        // Only the configured number of create_task findings become proposals.
        for (index, finding) in harness
            .findings
            .iter()
            .filter(|finding| finding.create_task)
            .take(self.config.review.max_task_proposals)
            .enumerate()
        {
            self.ingest(
                store,
                &task.task_id,
                EventType::NewTaskProposed,
                format!(
                    "orchestrator:{}:harness-proposal:{}:{index}",
                    task.task_id, harness_report.report_id
                ),
                serde_json::json!({
                    "from": "reviewer",
                    "report_id": harness_report.report_id,
                    "finding": finding
                }),
            )?;
        }
        let harness_blocks = harness.findings.iter().any(|finding| {
            finding.category == crate::super_agent::model::HarnessCategory::Harness
                && finding.severity >= self.config.review.blocking_severity
        });
        let verdict = if harness_blocks {
            ReviewVerdict::Blocked
        } else {
            review.verdict
        };
        // The final delivery review must approve the exact integrated version.
        // A mismatched approval is rejected as evidence, not trusted.
        if expected == ReviewStage::FinalDelivery && verdict == ReviewVerdict::Approved {
            for repo in &task.repositories {
                let approved = review
                    .reviewed_versions
                    .iter()
                    .find(|version| version.repository_id == repo.repository_id)
                    .and_then(|version| version.commit.as_deref())
                    .map(str::trim)
                    .filter(|commit| !commit.is_empty());
                if approved != repo.integrated_commit.as_deref() {
                    self.block_task(
                        store,
                        &task.task_id,
                        &format!(
                            "The final delivery review approved {:?} for {}, but the verified integrated commit is {:?}",
                            approved, repo.repository_id, repo.integrated_commit
                        ),
                        "final-review-version",
                    )?;
                    report.blocked.push(format!(
                        "{}: final review approved a different integrated version",
                        task.task_id
                    ));
                    return Ok(());
                }
            }
        }
        if expected == ReviewStage::Implementation && verdict == ReviewVerdict::Approved {
            // The approved submission becomes the reviewed version that
            // integration must carry forward.
            for repo in &task.repositories {
                let Some(submitted) = repo.submitted_commit.as_deref() else {
                    continue;
                };
                if repo.reviewed_commit.as_deref() != Some(submitted) {
                    let mut updated = repo.clone();
                    updated.reviewed_commit = Some(submitted.to_owned());
                    store.save_repo(&updated).map_err(store_error)?;
                }
            }
        }
        self.ingest(
            store,
            &task.task_id,
            EventType::ReviewCompleted,
            format!(
                "orchestrator:{}:review-completed:{}:{}:{}",
                task.task_id,
                task.spec_version,
                task.review_round,
                expected.as_str()
            ),
            serde_json::json!({
                "verdict": verdict.as_str(),
                "stage": expected.as_str(),
                "review_report_id": review_report.report_id,
                "harness_report_id": harness_report.report_id,
                "harness_blocked": harness_blocks
            }),
        )?;
        if verdict == ReviewVerdict::Blocked {
            self.request_delivery_blocker(store, task, report, review_report, harness_blocks)?;
        }
        report.processed += store
            .process_pending(&task.router_session_id, &self.policy())
            .map_err(store_error)?
            .len();
        Ok(())
    }

    fn request_delivery_blocker(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
        review_report: &Report,
        harness_blocked: bool,
    ) -> Result<()> {
        let approvals = store.approvals(&task.task_id).map_err(store_error)?;
        let exists = approvals.iter().any(|approval| {
            approval.request_type == ApprovalType::DeliveryBlocker
                && approval.status == ApprovalStatus::Pending
                && approval.task_spec_version == task.spec_version
        });
        if exists {
            return Ok(());
        }
        let operation = format!(
            "orchestrator:{}:delivery-blocker:{}:{}",
            task.task_id, task.spec_version, task.review_round
        );
        let approval = store
            .request_approval(
                &operation,
                &task.task_id,
                ApprovalDraft {
                    source_session_id: task
                        .reviewer
                        .session_id
                        .clone()
                        .context("Blocked review without a Reviewer session")?,
                    parent_session_id: task.router_session_id.clone(),
                    request_type: ApprovalType::DeliveryBlocker,
                    request: serde_json::json!({
                        "task_id": task.task_id,
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "review_report_id": review_report.report_id,
                        "harness_blocked": harness_blocked,
                        "reason": "Review or harness analysis blocked delivery"
                    }),
                    task_spec_version: task.spec_version,
                    permission_policy_version: task.permission_policy_version,
                },
            )
            .map_err(store_error)?;
        self.ingest(
            store,
            &task.task_id,
            EventType::ApprovalRequested,
            format!("{operation}:requested"),
            serde_json::json!({
                "request_type": "DELIVERY_BLOCKER",
                "approval_id": approval.approval_id
            }),
        )?;
        report.approvals.push(approval.approval_id);
        report.processed += store
            .process_pending(&task.router_session_id, &self.policy())
            .map_err(store_error)?
            .len();
        Ok(())
    }

    async fn resume_after_changes(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        let Some(previous_round) = task.review_round.checked_sub(1) else {
            return Ok(());
        };
        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let changes = reports.iter().rev().find(|report| {
            report.kind == ReportKind::Review
                && !report.invalidated
                && report.spec_version == task.spec_version
                && report.round == previous_round
                && report.body["verdict"].as_str() == Some("CHANGES_REQUESTED")
        });
        let Some(changes) = changes else {
            return Ok(());
        };
        let key = format!(
            "super-agent.resume-changes.{}.{}.{}",
            task.task_id, task.spec_version, task.review_round
        );
        if self.context.setting::<serde_json::Value>(&key)?.is_some() {
            return Ok(());
        }
        let issues: Vec<String> = changes.body["issues"]
            .as_array()
            .map(|issues| {
                issues
                    .iter()
                    .map(|issue| {
                        format!(
                            "- [{}] {} (expected: {})",
                            issue["severity"].as_str().unwrap_or("UNKNOWN"),
                            issue["description"].as_str().unwrap_or(""),
                            issue["expected"].as_str().unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let instruction = format!(
            "The Reviewer requested changes for task {} (spec version {}, review round {}). Fix exactly these issues in the same task workspace, rerun verification, commit, and submit CrpSubmit(kind=\"implementation\") again with a report whose spec_version is {} and round is {}:\n{}",
            task.task_id,
            task.spec_version,
            task.review_round,
            task.spec_version,
            task.review_round,
            issues.join("\n")
        );
        let outcome = self
            .launcher()
            .resume_coder(task, &instruction, "changes")
            .await?;
        self.context.save_setting(
            &key,
            &serde_json::json!({
                "session_id": outcome.session_id,
                "job_id": outcome.job_id,
                "at": chrono::Utc::now().to_rfc3339()
            }),
        )?;
        if outcome.fresh {
            report.launched.push(format!(
                "{} coder {} (changes)",
                task.task_id, outcome.session_id
            ));
        }
        Ok(())
    }

    /// Stage5A: prepares integration in the Coder's isolated worktree and
    /// verifies the claim against real Git state. The user's target branch is
    /// never changed, so this cannot publish a delivery fact.
    async fn prepare_integration(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<TaskRecord> {
        // The baseline is captured once, before any instruction is sent, so a
        // later move of the target is detectable.
        let baseline_key = format!(
            "super-agent.integration-baseline.{}.{}.{}",
            task.task_id, task.spec_version, task.review_round
        );
        let baselines: Vec<crate::super_agent::integration::IntegrationBaseline> =
            match self.context.setting::<serde_json::Value>(&baseline_key)? {
                Some(value) => serde_json::from_value(value)
                    .context("The stored integration baseline is invalid")?,
                None => {
                    let captured = crate::super_agent::integration::capture_baseline(task)?;
                    self.context
                        .save_setting(&baseline_key, &serde_json::to_value(&captured)?)?;
                    captured
                }
            };

        let resume_key = format!(
            "super-agent.resume-integration.{}.{}.{}",
            task.task_id, task.spec_version, task.review_round
        );
        if self
            .context
            .setting::<serde_json::Value>(&resume_key)?
            .is_none()
        {
            let instruction = coder_integration_prompt(task, &baselines);
            let outcome = self
                .launcher()
                .resume_coder(task, &instruction, "integration")
                .await?;
            self.context.save_setting(
                &resume_key,
                &serde_json::json!({
                    "session_id": outcome.session_id,
                    "job_id": outcome.job_id,
                    "at": chrono::Utc::now().to_rfc3339()
                }),
            )?;
            if outcome.fresh {
                report.launched.push(format!(
                    "{} coder {} (integration)",
                    task.task_id, outcome.session_id
                ));
            }
        }

        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let claim = reports.iter().rev().find(|report| {
            report.kind == ReportKind::Integration
                && !report.invalidated
                && report.spec_version == task.spec_version
                && report.round == task.review_round
        });
        let Some(claim) = claim else {
            return store
                .get_task(&task.task_id)
                .map_err(store_error)?
                .context("Task disappeared");
        };
        let events = store.events(&task.task_id).map_err(store_error)?;
        if events.iter().any(|event| {
            event.event_type == EventType::IntegrationPrepared
                && event.spec_version == task.spec_version
                && event.review_round == task.review_round
        }) {
            return store
                .get_task(&task.task_id)
                .map_err(store_error)?
                .context("Task disappeared");
        }

        let body: crate::super_agent::model::IntegrationReport =
            crate::super_agent::model::decode_body(claim)?;
        let outcome = crate::super_agent::integration::verify_preparation(task, &body, &baselines)?;

        // Successful repositories keep their verified facts even when another
        // repository fails; the task never advances on a partial result.
        for verified in &outcome.verified {
            let Some(mut repo) = task
                .repositories
                .iter()
                .find(|repo| repo.repository_id == verified.repository_id)
                .cloned()
            else {
                continue;
            };
            let changed = repo.reviewed_commit.as_deref()
                != Some(verified.reviewed_commit.as_str())
                || repo.integrated_commit.as_deref() != Some(verified.integrated_commit.as_str())
                || repo.integration_status != crate::super_agent::IntegrationStatus::Prepared
                || repo.verification_status != crate::super_agent::VerificationStatus::Passed;
            if changed {
                repo.reviewed_commit = Some(verified.reviewed_commit.clone());
                repo.integrated_commit = Some(verified.integrated_commit.clone());
                repo.integration_status = crate::super_agent::IntegrationStatus::Prepared;
                repo.verification_status = crate::super_agent::VerificationStatus::Passed;
                store.save_repo(&repo).map_err(store_error)?;
            }
        }

        if !outcome.complete() {
            let reason = if outcome.stale_baseline {
                format!(
                    "Integration preparation was rejected because the target moved after the baseline was captured; verify the target and start a new integration. Details: {}",
                    outcome.failures.join("; ")
                )
            } else {
                format!(
                    "Integration preparation failed verification: {}",
                    outcome.failures.join("; ")
                )
            };
            self.block_task(store, &task.task_id, &reason, "integration-prepared")?;
            report.blocked.push(format!("{}: {reason}", task.task_id));
            report.processed += store
                .process_pending(&task.router_session_id, &self.policy())
                .map_err(store_error)?
                .len();
            return store
                .get_task(&task.task_id)
                .map_err(store_error)?
                .context("Task disappeared");
        }

        // Publish the verified preparation fact. A deterministic operation ID
        // makes a replayed verification a no-op.
        let payload = serde_json::json!({
            "repositories": outcome.verified,
            "tree_changed": outcome.tree_changed(),
        });
        self.ingest(
            store,
            &task.task_id,
            EventType::IntegrationPrepared,
            format!(
                "orchestrator:{}:integration-prepared:{}:{}",
                task.task_id, task.spec_version, task.review_round
            ),
            payload,
        )?;
        report.processed += store
            .process_pending(&task.router_session_id, &self.policy())
            .map_err(store_error)?
            .len();
        store
            .get_task(&task.task_id)
            .map_err(store_error)?
            .context("Task disappeared")
    }

    /// Resumes the SAME Reviewer exactly once for the final delivery review of
    /// the verified integrated content. Its durable marker is stage-qualified,
    /// so the implementation review of the same round is never reused.
    async fn ensure_final_reviewer(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        if task.reviewer.session_id.is_none() {
            return Ok(());
        }
        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let current = |kind| {
            reports.iter().any(|report| {
                report.kind == kind
                    && !report.invalidated
                    && report.spec_version == task.spec_version
                    && report.round == task.review_round
                    && report_stage(report) == Some(ReviewStage::FinalDelivery)
            })
        };
        if current(ReportKind::Review) && current(ReportKind::Harness) {
            return Ok(());
        }
        let key = format!(
            "super-agent.reviewer-resume.{}.{}.{}.final-delivery",
            task.task_id, task.spec_version, task.review_round
        );
        if self.context.setting::<serde_json::Value>(&key)?.is_some() {
            return Ok(());
        }
        let outcome = self.launcher().resume_final_reviewer(task).await?;
        self.context.save_setting(
            &key,
            &serde_json::json!({
                "session_id": outcome.session_id,
                "job_id": outcome.job_id,
                "at": chrono::Utc::now().to_rfc3339()
            }),
        )?;
        if outcome.fresh {
            report.launched.push(format!(
                "{} reviewer {} (final delivery)",
                task.task_id, outcome.session_id
            ));
        }
        Ok(())
    }

    fn ingest(
        &self,
        store: &TaskStore,
        task_id: &str,
        event_type: EventType,
        operation_id: String,
        payload: serde_json::Value,
    ) -> Result<()> {
        store
            .ingest_event(
                task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type,
                    causation_id: None,
                    operation_id,
                    payload,
                },
            )
            .map_err(store_error)?;
        Ok(())
    }

    fn block_task(
        &self,
        store: &TaskStore,
        task_id: &str,
        reason: &str,
        reason_key: &str,
    ) -> Result<()> {
        let operation = format!("orchestrator:{task_id}:blocked:{reason_key}");
        store
            .apply_event(
                task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type: EventType::TaskBlocked,
                    causation_id: None,
                    operation_id: operation,
                    payload: serde_json::json!({"reason": reason}),
                },
                &self.policy(),
            )
            .map_err(store_error)?;
        Ok(())
    }
}

/// The plan, receipt and task version must describe the same immutable
/// delivery. This is checked before any operation/job record is trusted.
fn delivery_context_mismatch(
    task: &TaskRecord,
    plan: &crate::super_agent::delivery::DeliveryPlan,
    dispatch: &crate::super_agent::delivery::DeliveryDispatch,
) -> Option<String> {
    let coder = task.coder.session_id.as_deref();
    let Some(repository_id) = dispatch.repository_id.as_deref() else {
        return Some(format!(
            "The delivery receipt {} for task {} does not name the repository it delivers; refusing to adopt or observe it",
            dispatch.plan_operation_id, dispatch.task_id
        ));
    };
    let Some(target) = plan
        .repositories
        .iter()
        .find(|target| target.repository_id == repository_id)
    else {
        return Some(format!(
            "The delivery receipt names repository {repository_id}, which is not part of plan {}; refusing to adopt or observe it",
            plan.operation_id
        ));
    };
    if dispatch.old_oid.as_deref() != Some(target.old_oid.as_str())
        || dispatch.new_oid.as_deref() != Some(target.new_oid.as_str())
        || dispatch.attempt != target.attempt
    {
        return Some(format!(
            "The delivery receipt for repository {repository_id} does not match the approved old/new OIDs or attempt of plan {}; refusing to adopt or observe it",
            plan.operation_id
        ));
    }
    if plan.task_id != task.task_id
        || plan.spec_version != task.spec_version
        || plan.review_round != task.review_round
        || dispatch.task_id != task.task_id
        || dispatch.spec_version != task.spec_version
        || dispatch.review_round != task.review_round
        || dispatch.plan_operation_id != plan.operation_id
        || dispatch.purpose != "delivery"
        || coder != Some(dispatch.coder_session_id.as_str())
    {
        return Some(format!(
            "The delivery receipt for task {} spec {} round {} plan {} purpose {:?} and Coder session {} does not match plan {} spec {} round {} or the current task {} spec {} round {} Coder {:?}; refusing to adopt or observe it",
            dispatch.task_id,
            dispatch.spec_version,
            dispatch.review_round,
            dispatch.plan_operation_id,
            dispatch.purpose,
            dispatch.coder_session_id,
            plan.task_id,
            plan.spec_version,
            plan.review_round,
            task.task_id,
            task.spec_version,
            task.review_round,
            coder
        ));
    }
    None
}

/// A launch record found under the delivery key is only usable when its
/// contents identify the same task, evidence and original Coder session.
fn delivery_operation_mismatch(
    task: &TaskRecord,
    plan: &crate::super_agent::delivery::DeliveryPlan,
    dispatch: &crate::super_agent::delivery::DeliveryDispatch,
    operation: &crate::super_agent::launcher::LaunchOperation,
) -> Option<String> {
    if operation.task_id != task.task_id
        || operation.task_id != plan.task_id
        || operation.role != "coder"
        || operation.kind != LaunchKind::Resume.as_str()
        || operation.spec_version != task.spec_version
        || operation.spec_version != plan.spec_version
        || operation.review_round != task.review_round
        || operation.review_round != plan.review_round
        || operation.purpose != "delivery"
        || operation.session_id != dispatch.coder_session_id
        || operation.resume_session.as_deref() != Some(dispatch.coder_session_id.as_str())
    {
        return Some(format!(
            "The recorded delivery launch belongs to task {} role {} kind {} spec {} round {} purpose {:?} session {} resume {:?}, not to plan {} spec {} round {} task {} spec {} round {} Coder session {}; refusing to adopt it",
            operation.task_id,
            operation.role,
            operation.kind,
            operation.spec_version,
            operation.review_round,
            operation.purpose,
            operation.session_id,
            operation.resume_session,
            plan.task_id,
            plan.spec_version,
            plan.review_round,
            task.task_id,
            task.spec_version,
            task.review_round,
            dispatch.coder_session_id
        ));
    }
    None
}

/// A recorded handle must point at this delivery's reserved job and Coder.
fn delivery_handle_mismatch(
    operation: &crate::super_agent::launcher::LaunchOperation,
    dispatch: &crate::super_agent::delivery::DeliveryDispatch,
    handle: &crate::super_agent::launcher::LaunchRecord,
) -> Option<String> {
    if handle.role != "coder"
        || handle.session_id != dispatch.coder_session_id
        || handle.session_id != operation.session_id
        || handle.job_id != operation.job_id
    {
        return Some(format!(
            "The recorded delivery child {} belongs to session {} role {}, not to the reserved job {} for Coder session {}; refusing to adopt it",
            handle.job_id, handle.session_id, handle.role, operation.job_id, operation.session_id
        ));
    }
    None
}

/// The actual background task must carry the reserved job ID, the original
/// Coder session and this task's Router parent.
fn delivery_job_identity_mismatch(
    task: &TaskRecord,
    expected_job_id: &str,
    expected_session_id: &str,
    job: &crate::services::background_sessions::Job,
) -> Option<String> {
    if job.id != expected_job_id
        || job.session_id != expected_session_id
        || job.parent_session_id != task.router_session_id
    {
        return Some(format!(
            "The delivery background task {} has id {} session {} parent {}, not the recorded job {} for Coder session {} under Router {}; the original files were preserved",
            expected_job_id,
            job.id,
            job.session_id,
            job.parent_session_id,
            expected_job_id,
            expected_session_id,
            task.router_session_id
        ));
    }
    None
}

fn store_error(error: StoreError) -> anyhow::Error {
    match error {
        StoreError::Invalid(message) => anyhow::anyhow!(message),
        other => anyhow::Error::new(other),
    }
}

/// The review stage a report belongs to. A malformed body has no stage and
/// therefore never satisfies a review decision.
fn report_stage(report: &Report) -> Option<ReviewStage> {
    match report.kind {
        ReportKind::Review => {
            crate::super_agent::model::decode_body::<crate::super_agent::TaskReviewReport>(report)
                .ok()
                .map(|body| body.stage)
        }
        ReportKind::Harness => crate::super_agent::model::decode_body::<
            crate::super_agent::HarnessAnalysisReport,
        >(report)
        .ok()
        .map(|body| body.stage),
        _ => None,
    }
}

/// The statuses a Router can usefully summarize.
pub fn is_waiting_for_user(status: Status) -> bool {
    status == Status::WaitingUser
}

#[cfg(test)]
mod tests;
