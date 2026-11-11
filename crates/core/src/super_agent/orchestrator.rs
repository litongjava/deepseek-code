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
use crate::super_agent::launcher::{coder_implementation_prompt, Launcher};
use crate::super_agent::model::{
    ApprovalStatus, ApprovalType, EventType, NewEvent, Phase, Report, ReportKind, ReviewVerdict,
    Status, TaskRecord,
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

pub struct Orchestrator {
    pub context: ServiceContext,
    pub config: SuperAgentConfig,
    pub provider: ResolvedProvider,
    pub permission_mode: String,
    pub executable_override: Option<PathBuf>,
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
        }
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
        }
    }

    /// The Router's real workspace plus the additional directories the user
    /// granted through the supported settings path. A task specification can
    /// only narrow this scope, never widen it.
    fn authorized_roots(&self, router_session: &str) -> Result<Vec<PathBuf>> {
        let workspace: Option<String> =
            self.context.db.with_conn(|conn| -> Result<Option<String>> {
                Ok(crate::db::conversation_repo::get_conversation(conn, router_session)?
                    .and_then(|row| row.workspace_path))
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
                    let _ = self.block_task(&store, &task.task_id, &reason);
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

        // 2. Start the Coder in plan mode once its workspace exists.
        if matches!(task.phase, Phase::Discovering | Phase::Planning)
            && task.status.is_active()
            && task.coder.session_id.is_none()
        {
            let outcome = self.launcher().launch_coder(&task).await?;
            report
                .launched
                .push(format!("{} coder {}", task.task_id, outcome.session_id));
            if let Some(warning) = &outcome.warning {
                report.warnings.push(format!("{}: {warning}", task.task_id));
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

        // 6. Start the Reviewer once, after the submission is pinned.
        if task.phase == Phase::Reviewing
            && task.reviewer.session_id.is_none()
            && (!task.repositories.is_empty() || !task.non_git_targets.is_empty())
            && task
                .repositories
                .iter()
                .all(|repo| repo.submitted_commit.is_some())
        {
            let outcome = self.launcher().launch_reviewer(&task).await?;
            report
                .launched
                .push(format!("{} reviewer {}", task.task_id, outcome.session_id));
            if let Some(warning) = &outcome.warning {
                report.warnings.push(format!("{}: {warning}", task.task_id));
            }
        }

        // 7. Corrections go back to the same Coder session with the exact
        // issues from the ChangesRequested review.
        if task.phase == Phase::Implementing && task.status.is_active() {
            self.resume_after_changes(store, &task, report).await?;
        }
        Ok(())
    }

    fn prepare(&self, store: &TaskStore, task: &TaskRecord, report: &mut TickReport) -> Result<()> {
        // Authorization comes from the Router session's real persisted
        // workspace and the directories the user granted in settings, never
        // from the model-supplied specification itself.
        let roots = self.authorized_roots(&task.router_session_id)?;
        let workspace = crate::super_agent::discovery::resolve_authorized_path(
            &task.spec.workspace,
            &roots,
        )
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
            .resume_coder(task, &coder_implementation_prompt(task))
            .await?;
        self.context.save_setting(
            &key,
            &serde_json::json!({
                "session_id": outcome.session_id,
                "job_id": outcome.job_id,
                "at": chrono::Utc::now().to_rfc3339()
            }),
        )?;
        report.launched.push(format!(
            "{} coder {} (resumed)",
            task.task_id, outcome.session_id
        ));
        if let Some(warning) = outcome.warning {
            report.warnings.push(format!("{}: {warning}", task.task_id));
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
            let head = crate::worktree::session::command(&repo.working_path, &["rev-parse", "HEAD"])?;
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
        report.launched.push(format!(
            "{} reviewer {} (round {})",
            task.task_id, reviewer, task.review_round
        ));
        Ok(())
    }

    fn decide_review(
        &self,
        store: &TaskStore,
        task: &TaskRecord,
        report: &mut TickReport,
    ) -> Result<()> {
        if task.phase != Phase::Reviewing {
            return Ok(());
        }
        let reports = store.reports(&task.task_id).map_err(store_error)?;
        let current: Vec<&Report> = reports
            .iter()
            .filter(|report| {
                !report.invalidated
                    && report.spec_version == task.spec_version
                    && report.round == task.review_round
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
        self.ingest(
            store,
            &task.task_id,
            EventType::ReviewCompleted,
            format!(
                "orchestrator:{}:review-completed:{}:{}",
                task.task_id, task.spec_version, task.review_round
            ),
            serde_json::json!({
                "verdict": verdict.as_str(),
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
            "The Reviewer requested changes for task {}. Fix exactly these issues in the same task workspace, rerun verification, commit, and submit CrpSubmit(kind=\"implementation\") again:\n{}",
            task.task_id,
            issues.join("\n")
        );
        let outcome = self.launcher().resume_coder(task, &instruction).await?;
        self.context.save_setting(
            &key,
            &serde_json::json!({
                "session_id": outcome.session_id,
                "job_id": outcome.job_id,
                "at": chrono::Utc::now().to_rfc3339()
            }),
        )?;
        report.launched.push(format!(
            "{} coder {} (changes)",
            task.task_id, outcome.session_id
        ));
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

    fn block_task(&self, store: &TaskStore, task_id: &str, reason: &str) -> Result<()> {
        let operation = format!("orchestrator:{task_id}:blocked");
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

fn store_error(error: StoreError) -> anyhow::Error {
    match error {
        StoreError::Invalid(message) => anyhow::anyhow!(message),
        other => anyhow::Error::new(other),
    }
}

/// The statuses a Router can usefully summarize.
pub fn is_waiting_for_user(status: Status) -> bool {
    status == Status::WaitingUser
}

#[cfg(test)]
mod tests;
