//! Deterministic task state transitions and the completion gate.
//!
//! A model may submit evidence, but only these pure functions decide whether a
//! task moves. Keeping them pure makes every rejection and every unmet
//! completion condition testable without a database or a model call.
use super::model::{
    decode_body, AcceptanceResult, AgentRole, AgentStatus, Approval, ApprovalStatus, ApprovalType,
    CleanupStatus, Event, EventType, HarnessAnalysisReport, NewEvent, NonGitStatus, Phase,
    RepoTarget, Report, ReportKind, ReviewStage, ReviewVerdict, SessionRef, Status, TaskRecord,
    TaskReviewReport, Unmet,
};
use serde_json::Value;
use std::collections::BTreeSet;
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub struct StatePolicy {
    pub max_rounds: u32,
}

impl Default for StatePolicy {
    fn default() -> Self {
        Self { max_rounds: 5 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub phase: Phase,
    pub status: Status,
    pub review_round: u32,
    pub spec: Option<super::model::TaskSpec>,
    pub spec_version: u32,
    pub invalidated: Vec<ReportKind>,
    pub agent_updates: Vec<(AgentRole, AgentStatus)>,
    pub cleanup_status: Option<CleanupStatus>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StateError {
    #[error("event {event} is not allowed in {phase}/{status}: {reason}")]
    InvalidTransition {
        task_id: String,
        phase: Phase,
        status: Status,
        event: EventType,
        reason: String,
    },
    #[error("event {event} payload is invalid: {reason}")]
    InvalidPayload { event: EventType, reason: String },
}

fn invalid(record: &TaskRecord, event: &EventType, reason: impl Into<String>) -> StateError {
    StateError::InvalidTransition {
        task_id: record.task_id.clone(),
        phase: record.phase,
        status: record.status,
        event: *event,
        reason: reason.into(),
    }
}

fn payload_error(event: &EventType, reason: impl Into<String>) -> StateError {
    StateError::InvalidPayload {
        event: *event,
        reason: reason.into(),
    }
}

fn require_phase(
    record: &TaskRecord,
    event: &EventType,
    phases: &[Phase],
) -> Result<(), StateError> {
    if phases.contains(&record.phase) {
        Ok(())
    } else {
        Err(invalid(record, event, format!("expected phase {phases:?}")))
    }
}

fn require_status(
    record: &TaskRecord,
    event: &EventType,
    statuses: &[Status],
) -> Result<(), StateError> {
    if statuses.contains(&record.status) {
        Ok(())
    } else {
        Err(invalid(
            record,
            event,
            format!("expected status {statuses:?}"),
        ))
    }
}

fn verdict(payload: &Value) -> Result<ReviewVerdict, String> {
    let value = payload["verdict"]
        .as_str()
        .ok_or_else(|| "verdict is required".to_owned())?;
    match value.to_ascii_uppercase().as_str() {
        "APPROVED" => Ok(ReviewVerdict::Approved),
        "CHANGES_REQUESTED" | "CHANGES" => Ok(ReviewVerdict::ChangesRequested),
        "BLOCKED" => Ok(ReviewVerdict::Blocked),
        other => Err(format!("unknown verdict {other}")),
    }
}

fn decision(payload: &Value) -> Result<ApprovalStatus, String> {
    let value = payload["decision"]
        .as_str()
        .ok_or_else(|| "decision is required".to_owned())?;
    match value.to_ascii_uppercase().as_str() {
        "APPROVED" | "APPROVE" | "ALLOW" => Ok(ApprovalStatus::Approved),
        "REJECTED" | "REJECT" | "DENY" => Ok(ApprovalStatus::Rejected),
        other => Err(format!("unknown decision {other}")),
    }
}

fn approval_type(payload: &Value) -> Result<ApprovalType, String> {
    match payload["request_type"].as_str() {
        None => Ok(ApprovalType::Plan),
        Some(value) => ApprovalType::parse(&value.to_ascii_uppercase())
            .map_err(|_| format!("unknown approval request type {value}")),
    }
}

fn role(record: &TaskRecord, event: &NewEvent) -> Result<AgentRole, StateError> {
    if let Some(value) = event.payload["role"].as_str() {
        return match value.to_ascii_uppercase().as_str() {
            "CODER" => Ok(AgentRole::Coder),
            "REVIEWER" => Ok(AgentRole::Reviewer),
            other => Err(payload_error(
                &event.event_type,
                format!("unknown agent role {other}"),
            )),
        };
    }
    let session = event.session_id.as_deref();
    let coder = record.coder.session_id.as_deref();
    let reviewer = record.reviewer.session_id.as_deref();
    match (session, coder, reviewer) {
        (Some(session), Some(coder), _) if session == coder => Ok(AgentRole::Coder),
        (Some(session), _, Some(reviewer)) if session == reviewer => Ok(AgentRole::Reviewer),
        _ => Err(payload_error(
            &event.event_type,
            "the event must identify the coder or reviewer session",
        )),
    }
}

/// Applies one event to a record and returns the resulting state. It does not
/// mutate or persist anything; the store commits the returned values together
/// with the event in one transaction.
pub fn apply(
    record: &TaskRecord,
    event: &NewEvent,
    policy: &StatePolicy,
) -> Result<Transition, StateError> {
    if record.status.is_terminal() {
        return Err(invalid(
            record,
            &event.event_type,
            format!("task is already {}", record.status),
        ));
    }
    let mut transition = Transition {
        phase: record.phase,
        status: record.status,
        review_round: record.review_round,
        spec: None,
        spec_version: record.spec_version,
        invalidated: Vec::new(),
        agent_updates: Vec::new(),
        cleanup_status: None,
    };
    let event_type = event.event_type;
    match event_type {
        EventType::TaskCreated => {
            return Err(invalid(
                record,
                &event_type,
                "tasks are created by the store, not by an applied event",
            ))
        }
        EventType::DiscoveryCompleted => {
            require_phase(record, &event_type, &[Phase::Discovering])?;
            require_status(record, &event_type, &[Status::Queued, Status::Running])?;
            transition.phase = Phase::Planning;
            transition.status = Status::Running;
        }
        EventType::PlanSubmitted => {
            require_phase(record, &event_type, &[Phase::Planning])?;
            require_status(record, &event_type, &[Status::Running, Status::WaitingUser])?;
            transition.status = Status::Running;
        }
        EventType::ApprovalRequested => {
            let request_type = approval_type(&event.payload)
                .map_err(|reason| payload_error(&event_type, reason))?;
            if request_type == ApprovalType::Plan {
                require_phase(record, &event_type, &[Phase::Planning])?;
                require_status(record, &event_type, &[Status::Running, Status::WaitingUser])?;
                transition.status = Status::WaitingUser;
                transition
                    .agent_updates
                    .push((AgentRole::Coder, AgentStatus::WaitingApproval));
            } else if record.status.is_active() || record.status == Status::Blocked {
                // Tool and delivery blockers do not change the task phase.
            } else {
                return Err(invalid(record, &event_type, "task is not active"));
            }
        }
        EventType::ApprovalResolved => {
            let request_type = approval_type(&event.payload)
                .map_err(|reason| payload_error(&event_type, reason))?;
            let outcome =
                decision(&event.payload).map_err(|reason| payload_error(&event_type, reason))?;
            if request_type == ApprovalType::Plan {
                require_phase(record, &event_type, &[Phase::Planning])?;
                require_status(record, &event_type, &[Status::WaitingUser])?;
                transition.phase = if outcome == ApprovalStatus::Approved {
                    Phase::Implementing
                } else {
                    Phase::Planning
                };
                transition.status = Status::Running;
                transition
                    .agent_updates
                    .push((AgentRole::Coder, AgentStatus::Running));
            } else if record.status == Status::Blocked {
                if outcome == ApprovalStatus::Approved {
                    transition.status = Status::Running;
                }
            } else {
                require_status(record, &event_type, &[Status::Running, Status::WaitingUser])?;
                if record.status == Status::WaitingUser {
                    transition.status = Status::Running;
                }
            }
        }
        EventType::ImplementationSubmitted => {
            require_phase(record, &event_type, &[Phase::Implementing])?;
            require_status(record, &event_type, &[Status::Running])?;
            transition.phase = Phase::Reviewing;
            transition.status = Status::Running;
            transition
                .agent_updates
                .push((AgentRole::Coder, AgentStatus::Idle));
        }
        EventType::ReviewCompleted => {
            let verdict =
                verdict(&event.payload).map_err(|reason| payload_error(&event_type, reason))?;
            transition
                .agent_updates
                .push((AgentRole::Reviewer, AgentStatus::Idle));
            match record.phase {
                Phase::Reviewing => match verdict {
                    ReviewVerdict::Approved => {
                        transition.phase = if record.has_git_targets() {
                            Phase::Integrating
                        } else {
                            Phase::FinalReview
                        };
                        transition.status = Status::Running;
                    }
                    ReviewVerdict::ChangesRequested => {
                        let round = record.review_round.saturating_add(1);
                        transition.review_round = round;
                        transition.phase = Phase::Implementing;
                        transition.status = if round > policy.max_rounds {
                            Status::Blocked
                        } else {
                            Status::Running
                        };
                    }
                    ReviewVerdict::Blocked => {
                        transition.status = Status::Blocked;
                    }
                },
                Phase::FinalReview => match verdict {
                    ReviewVerdict::Approved => {
                        transition.phase = Phase::Finalizing;
                        transition.status = Status::Running;
                    }
                    ReviewVerdict::ChangesRequested | ReviewVerdict::Blocked => {
                        let round = record.review_round.saturating_add(1);
                        transition.review_round = round;
                        if verdict == ReviewVerdict::Blocked || round > policy.max_rounds {
                            transition.status = Status::Blocked;
                        } else {
                            transition.phase = Phase::Implementing;
                            transition.status = Status::Running;
                        }
                    }
                },
                _ => {
                    return Err(invalid(
                        record,
                        &event_type,
                        "reviews apply only in REVIEWING or FINAL_REVIEW",
                    ))
                }
            }
        }
        EventType::HarnessAnalysisCompleted => {
            require_phase(
                record,
                &event_type,
                &[
                    Phase::Reviewing,
                    Phase::FinalReview,
                    Phase::Integrating,
                    Phase::Finalizing,
                ],
            )?;
            require_status(record, &event_type, &[Status::Running, Status::WaitingUser])?;
        }
        EventType::IntegrationCompleted => {
            require_phase(record, &event_type, &[Phase::Integrating])?;
            require_status(record, &event_type, &[Status::Running])?;
            if event.payload["complete"].as_bool() == Some(false) {
                transition.status = Status::Blocked;
            } else {
                transition.phase = Phase::FinalReview;
                transition.status = Status::Running;
            }
        }
        EventType::NewTaskProposed => {
            if record.status.is_terminal() {
                return Err(invalid(record, &event_type, "task is already terminal"));
            }
        }
        EventType::SessionInterrupted => {
            let role = role(record, event)?;
            match record.status {
                Status::Queued | Status::Running | Status::Recovering => {
                    transition.status = Status::Recovering;
                }
                Status::WaitingUser | Status::Blocked | Status::Paused => {}
                _ => return Err(invalid(record, &event_type, "task cannot be interrupted")),
            }
            transition
                .agent_updates
                .push((role, AgentStatus::Interrupted));
        }
        EventType::SessionResumed => {
            let role = role(record, event)?;
            let previous = match role {
                AgentRole::Coder => record.coder.agent_status,
                AgentRole::Reviewer => record.reviewer.agent_status,
            };
            if record.status != Status::Recovering
                && !matches!(previous, AgentStatus::Interrupted | AgentStatus::Stopped)
            {
                return Err(invalid(
                    record,
                    &event_type,
                    "the session is not interrupted",
                ));
            }
            if record.status == Status::Recovering {
                transition.status = Status::Running;
            }
            transition.agent_updates.push((role, AgentStatus::Running));
        }
        EventType::SpecUpdated => {
            let spec: super::model::TaskSpec =
                serde_json::from_value(event.payload["spec"].clone()).map_err(|error| {
                    payload_error(&event_type, format!("invalid spec: {error}"))
                })?;
            spec.validate()
                .map_err(|error| payload_error(&event_type, format!("invalid spec: {error}")))?;
            transition.spec_version = record
                .spec_version
                .checked_add(1)
                .ok_or_else(|| payload_error(&event_type, "spec version overflow"))?;
            transition.spec = Some(spec);
            transition.phase = if record.phase == Phase::Discovering {
                Phase::Discovering
            } else {
                Phase::Planning
            };
            transition.status = Status::Running;
            transition.invalidated = vec![
                ReportKind::Plan,
                ReportKind::Implementation,
                ReportKind::Review,
                ReportKind::Harness,
                ReportKind::Integration,
            ];
            transition
                .agent_updates
                .push((AgentRole::Coder, AgentStatus::Idle));
            transition
                .agent_updates
                .push((AgentRole::Reviewer, AgentStatus::Idle));
        }
        EventType::TaskPaused => {
            require_status(
                record,
                &event_type,
                &[
                    Status::Queued,
                    Status::Running,
                    Status::WaitingUser,
                    Status::Recovering,
                ],
            )?;
            transition.status = Status::Paused;
        }
        EventType::TaskResumed => {
            require_status(record, &event_type, &[Status::Paused])?;
            transition.status = Status::Running;
        }
        EventType::TaskBlocked => {
            transition.status = Status::Blocked;
        }
        EventType::TaskCancelled => {
            transition.status = Status::Cancelled;
        }
        EventType::TaskFailed => {
            transition.status = Status::Failed;
        }
        EventType::TaskCompleted => {
            require_phase(record, &event_type, &[Phase::Finalizing])?;
            require_status(record, &event_type, &[Status::Running])?;
            // The store runs `completion_gate` in the same transaction before
            // it calls this transition.
            transition.status = Status::Completed;
        }
        EventType::CleanupCompleted => {
            require_status(record, &event_type, &[Status::Completed])?;
            transition.cleanup_status = Some(CleanupStatus::Completed);
        }
    }
    Ok(transition)
}

fn latest_report<'a>(reports: &'a [&'a Report]) -> Option<&'a Report> {
    reports.iter().copied().max_by(|left, right| {
        left.round
            .cmp(&right.round)
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.report_id.cmp(&right.report_id))
    })
}

fn current<'a>(reports: &'a [Report], spec_version: u32) -> Vec<&'a Report> {
    reports
        .iter()
        .filter(|report| !report.invalidated && report.spec_version == spec_version)
        .collect()
}

pub fn non_git_repository_id(path: &std::path::Path) -> String {
    format!("non-git:{}", path.display())
}

/// The eight documented completion conditions. Every condition is evaluated
/// even when an earlier one fails, so the caller can show all remaining work.
pub fn completion_gate(
    record: &TaskRecord,
    repos: &[RepoTarget],
    reports: &[Report],
    approvals: &[Approval],
) -> Result<(), Vec<Unmet>> {
    let mut unmet = Vec::new();
    let reports_current = current(reports, record.spec_version);
    let review_reports: Vec<&Report> = reports_current
        .iter()
        .copied()
        .filter(|report| report.kind == ReportKind::Review)
        .collect();
    let latest_review = latest_report(&review_reports);
    let final_review = latest_review
        .map(|report| (report, decode_body::<TaskReviewReport>(report)))
        .and_then(|(report, body)| match body {
            Ok(body) if body.stage == ReviewStage::FinalDelivery => Some((report, body)),
            Ok(_) => None,
            Err(_) => None,
        });

    // 1. The review must describe the current specification.
    if final_review.is_none() {
        let all_reviews: Vec<&Report> = reports
            .iter()
            .filter(|report| report.kind == ReportKind::Review)
            .collect();
        match latest_report(&all_reviews) {
            Some(report) if report.spec_version != record.spec_version => {
                unmet.push(Unmet::SpecMismatch {
                    expected: record.spec_version,
                    found: Some(report.spec_version),
                });
            }
            Some(report) => {
                if decode_body::<TaskReviewReport>(report).is_err() {
                    unmet.push(Unmet::MalformedReport {
                        report_id: report.report_id.clone(),
                        kind: ReportKind::Review,
                    });
                } else {
                    unmet.push(Unmet::FinalReviewMissing);
                }
            }
            None => unmet.push(Unmet::FinalReviewMissing),
        }
    }

    if let Some((report, body)) = &final_review {
        // 2. The final task acceptance passed.
        if body.verdict != ReviewVerdict::Approved {
            unmet.push(Unmet::FinalReviewNotApproved {
                verdict: Some(body.verdict),
            });
        }
        if body.verdict == ReviewVerdict::Blocked {
            unmet.push(Unmet::BlockingReport {
                report_id: report.report_id.clone(),
            });
        }
        // 3. Final delivered content equals the reviewed version.
        for repo in repos {
            if repo.integrated_commit.is_some() || repo.integration_status != Default::default() {
                let reviewed = body
                    .reviewed_versions
                    .iter()
                    .find(|version| version.repository_id == repo.repository_id)
                    .and_then(|version| version.commit.clone());
                if repo.reviewed_commit != repo.integrated_commit
                    || reviewed != repo.integrated_commit
                {
                    unmet.push(Unmet::VersionMismatch {
                        repository_id: repo.repository_id.clone(),
                        expected: repo.reviewed_commit.clone(),
                        found: repo.integrated_commit.clone(),
                    });
                }
            }
        }
        for target in &record.non_git_targets {
            let reviewed = body
                .reviewed_versions
                .iter()
                .find(|version| version.repository_id == non_git_repository_id(&target.path))
                .and_then(|version| version.snapshot_ref.clone());
            if reviewed.is_none() || reviewed != target.diff_ref {
                unmet.push(Unmet::VersionMismatch {
                    repository_id: non_git_repository_id(&target.path),
                    expected: target.diff_ref.clone(),
                    found: reviewed,
                });
            }
        }
        // 5. Independent verification is recorded by the reviewer.
        if body.verification.is_empty() {
            unmet.push(Unmet::ReviewVerificationMissing);
        }
    }

    // 4. Every Git target was integrated by the Coder.
    for repo in repos {
        if repo.integration_status != super::model::IntegrationStatus::Completed {
            unmet.push(Unmet::IntegrationIncomplete {
                repository_id: repo.repository_id.clone(),
                status: repo.integration_status,
            });
        }
        // 5. Verification passed for every integrated repository.
        if repo.verification_status != super::model::VerificationStatus::Passed {
            unmet.push(Unmet::VerificationIncomplete {
                repository_id: repo.repository_id.clone(),
                status: repo.verification_status,
            });
        }
    }

    // 6. Non-Git targets have a recorded diff and completed verification.
    for target in &record.non_git_targets {
        let complete = target.status == NonGitStatus::Completed
            && target
                .diff_ref
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            && !target.snapshot_manifest_json.trim().is_empty()
            && !target.snapshot_dir.as_os_str().is_empty();
        if !complete {
            unmet.push(Unmet::NonGitTargetIncomplete {
                path: target.path.display().to_string(),
                status: target.status,
            });
        }
    }

    // 7. No unresolved delivery blocker.
    for approval in approvals {
        if approval.status == ApprovalStatus::Pending
            && approval.task_spec_version == record.spec_version
        {
            unmet.push(Unmet::UnresolvedApproval {
                approval_id: approval.approval_id.clone(),
                request_type: approval.request_type,
            });
        }
    }
    for report in &reports_current {
        match report.kind {
            ReportKind::Harness => match decode_body::<HarnessAnalysisReport>(report) {
                Ok(body) if body.blocking() => unmet.push(Unmet::BlockingReport {
                    report_id: report.report_id.clone(),
                }),
                Err(_) => unmet.push(Unmet::MalformedReport {
                    report_id: report.report_id.clone(),
                    kind: ReportKind::Harness,
                }),
                Ok(_) => {}
            },
            ReportKind::Review => {
                if let Ok(body) = decode_body::<TaskReviewReport>(report) {
                    if body.verdict == ReviewVerdict::Blocked {
                        unmet.push(Unmet::BlockingReport {
                            report_id: report.report_id.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if matches!(
        record.status,
        Status::Blocked | Status::Failed | Status::Cancelled
    ) {
        unmet.push(Unmet::BlockingReport {
            report_id: format!("task-status:{}", record.status),
        });
    }

    // 8. Durable evidence and the sessions it refers to exist.
    for (kind, present) in [
        (
            ReportKind::Implementation,
            reports_current
                .iter()
                .any(|report| report.kind == ReportKind::Implementation),
        ),
        (
            ReportKind::Harness,
            reports_current
                .iter()
                .any(|report| report.kind == ReportKind::Harness),
        ),
        (
            ReportKind::Review,
            reports_current
                .iter()
                .any(|report| report.kind == ReportKind::Review),
        ),
    ] {
        if !present {
            unmet.push(Unmet::EvidenceMissing { report: kind });
        }
    }
    for (role, session) in [
        (AgentRole::Coder, &record.coder),
        (AgentRole::Reviewer, &record.reviewer),
    ] {
        if session
            .session_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
        {
            unmet.push(Unmet::SessionMissing { role });
        }
    }
    if unmet.is_empty() {
        Ok(())
    } else {
        Err(unmet)
    }
}

/// Convenience for the in-memory tests and for callers that already hold a
/// `TaskRecord`: apply and project the new state onto the record.
pub fn apply_to_record(
    record: &mut TaskRecord,
    event: &Event,
    policy: &StatePolicy,
) -> Result<Transition, StateError> {
    let draft = NewEvent {
        session_id: event.session_id.clone(),
        run_id: event.run_id.clone(),
        event_type: event.event_type,
        causation_id: event.causation_id.clone(),
        operation_id: event.operation_id.clone(),
        payload: event.payload.clone(),
    };
    let transition = apply(record, &draft, policy)?;
    record.phase = transition.phase;
    record.status = transition.status;
    record.review_round = transition.review_round;
    if let Some(spec) = transition.spec.clone() {
        record.spec = spec;
    }
    record.spec_version = transition.spec_version;
    for (role, status) in &transition.agent_updates {
        match role {
            AgentRole::Coder => record.coder.agent_status = *status,
            AgentRole::Reviewer => record.reviewer.agent_status = *status,
        }
    }
    if let Some(cleanup) = transition.cleanup_status {
        record.cleanup_status = cleanup;
    }
    Ok(transition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::super_agent::model::*;
    use serde_json::json;

    fn spec() -> TaskSpec {
        TaskSpec {
            title: "Task".into(),
            workspace: std::path::PathBuf::from("/workspace"),
            targets: vec!["/workspace/code".into()],
            goal: "Do the thing".into(),
            requirements: vec![],
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC-001".into(),
                description: "done".into(),
            }],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: None,
            repository_target_branches: Default::default(),
        }
    }

    fn record(phase: Phase, status: Status) -> TaskRecord {
        TaskRecord {
            task_id: "task-001".into(),
            pair_id: "crp-001".into(),
            router_session_id: "router".into(),
            title: "Task".into(),
            spec: spec(),
            spec_version: 1,
            phase,
            status,
            review_round: 0,
            parent_task_id: None,
            depends_on: vec![],
            coder: SessionRef {
                session_id: Some("coder".into()),
                previous_session_ids: vec![],
                agent_status: AgentStatus::Idle,
            },
            reviewer: SessionRef {
                session_id: Some("reviewer".into()),
                previous_session_ids: vec![],
                agent_status: AgentStatus::Idle,
            },
            cleanup_status: CleanupStatus::NotStarted,
            permission_policy_version: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            repositories: vec![],
            non_git_targets: vec![],
        }
    }

    fn event(event_type: EventType, payload: Value) -> NewEvent {
        NewEvent {
            session_id: None,
            run_id: None,
            event_type,
            causation_id: None,
            operation_id: "op".into(),
            payload,
        }
    }

    #[test]
    fn legal_flow_reaches_finalizing_and_rounds_reject_over_limit() {
        let policy = StatePolicy { max_rounds: 2 };
        let mut state = record(Phase::Discovering, Status::Queued);
        let mut step = |state: &mut TaskRecord, draft: NewEvent| {
            let transition = apply(state, &draft, &policy).unwrap();
            state.phase = transition.phase;
            state.status = transition.status;
            state.review_round = transition.review_round;
            transition
        };
        assert_eq!(
            step(&mut state, event(EventType::DiscoveryCompleted, json!({}))).phase,
            Phase::Planning
        );
        step(&mut state, event(EventType::PlanSubmitted, json!({})));
        step(
            &mut state,
            event(EventType::ApprovalRequested, json!({"request_type":"PLAN"})),
        );
        assert_eq!(state.status, Status::WaitingUser);
        step(
            &mut state,
            event(
                EventType::ApprovalResolved,
                json!({"request_type":"PLAN","decision":"APPROVED"}),
            ),
        );
        assert_eq!(
            (state.phase, state.status),
            (Phase::Implementing, Status::Running)
        );
        state.repositories = vec![repo()];
        step(
            &mut state,
            event(EventType::ImplementationSubmitted, json!({})),
        );
        assert_eq!(state.phase, Phase::Reviewing);
        let transition = step(
            &mut state,
            event(
                EventType::ReviewCompleted,
                json!({"verdict":"CHANGES_REQUESTED"}),
            ),
        );
        assert_eq!(state.review_round, 1);
        assert_eq!(state.phase, Phase::Implementing);
        assert_eq!(transition.status, Status::Running);
        step(
            &mut state,
            event(EventType::ImplementationSubmitted, json!({})),
        );
        step(
            &mut state,
            event(EventType::ReviewCompleted, json!({"verdict":"APPROVED"})),
        );
        assert_eq!(state.phase, Phase::Integrating);
        step(
            &mut state,
            event(EventType::IntegrationCompleted, json!({})),
        );
        assert_eq!(state.phase, Phase::FinalReview);
        step(
            &mut state,
            event(EventType::ReviewCompleted, json!({"verdict":"APPROVED"})),
        );
        assert_eq!(state.phase, Phase::Finalizing);
        step(&mut state, event(EventType::TaskCompleted, json!({})));
        assert_eq!(state.status, Status::Completed);

        let mut limited = record(Phase::Reviewing, Status::Running);
        limited.review_round = 2;
        let error = apply(
            &mut limited,
            &event(
                EventType::ReviewCompleted,
                json!({"verdict":"CHANGES_REQUESTED"}),
            ),
            &policy,
        )
        .unwrap();
        assert_eq!(error.status, Status::Blocked);
    }

    fn repo() -> RepoTarget {
        RepoTarget {
            task_id: "task-001".into(),
            repository_id: "repo-code".into(),
            repo_root: "/workspace/code".into(),
            git_common_dir: "/workspace/code/.git".into(),
            target_path: "/workspace/code".into(),
            target_branch: "develop".into(),
            task_branch: "agent/task-001".into(),
            worktree_path: "/data/worktrees/task-001".into(),
            working_path: "/data/worktrees/task-001".into(),
            base_commit: "base".into(),
            submitted_commit: Some("submitted".into()),
            reviewed_commit: Some("integrated".into()),
            integrated_commit: Some("integrated".into()),
            integration_status: IntegrationStatus::Completed,
            verification_status: VerificationStatus::Passed,
            source_state: vec![],
        }
    }

    #[test]
    fn illegal_transitions_are_rejected_with_the_observed_state() {
        let policy = StatePolicy::default();
        let state = record(Phase::Planning, Status::Running);
        for (event_type, payload) in [
            (EventType::ImplementationSubmitted, json!({})),
            (EventType::ReviewCompleted, json!({"verdict":"APPROVED"})),
            (
                EventType::ApprovalResolved,
                json!({"request_type":"PLAN","decision":"APPROVED"}),
            ),
            (EventType::IntegrationCompleted, json!({})),
            (EventType::CleanupCompleted, json!({})),
        ] {
            let error = apply(&state, &event(event_type, payload), &policy).unwrap_err();
            match error {
                StateError::InvalidTransition {
                    phase,
                    status,
                    event,
                    ..
                } => {
                    assert_eq!((phase, status), (Phase::Planning, Status::Running));
                    assert_eq!(event, event_type);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }
        let integrated = record(Phase::Integrating, Status::Running);
        let error = apply(
            &integrated,
            &event(EventType::TaskCompleted, json!({})),
            &policy,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            StateError::InvalidTransition {
                event: EventType::TaskCompleted,
                ..
            }
        ));
        let mut completed = record(Phase::Finalizing, Status::Running);
        completed.status = Status::Completed;
        let error = apply(
            &completed,
            &event(EventType::TaskPaused, json!({})),
            &policy,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            StateError::InvalidTransition {
                status: Status::Completed,
                ..
            }
        ));
    }

    #[test]
    fn paused_tasks_reject_work_and_resume_only_from_paused() {
        let policy = StatePolicy::default();
        let mut state = record(Phase::Implementing, Status::Paused);
        assert!(apply(
            &state,
            &event(EventType::ImplementationSubmitted, json!({})),
            &policy
        )
        .is_err());
        let transition = apply(&state, &event(EventType::TaskResumed, json!({})), &policy).unwrap();
        assert_eq!(transition.status, Status::Running);
        state.status = Status::Running;
        assert!(apply(&state, &event(EventType::TaskResumed, json!({})), &policy).is_err());
    }

    #[test]
    fn session_events_need_a_known_role_and_update_only_that_agent() {
        let policy = StatePolicy::default();
        let state = record(Phase::Implementing, Status::Running);
        let mut draft = event(EventType::SessionInterrupted, json!({}));
        draft.session_id = Some("coder".into());
        let transition = apply(&state, &draft, &policy).unwrap();
        assert_eq!(transition.status, Status::Recovering);
        assert_eq!(
            transition.agent_updates,
            vec![(AgentRole::Coder, AgentStatus::Interrupted)]
        );
        let unknown = NewEvent {
            session_id: Some("somebody".into()),
            ..event(EventType::SessionInterrupted, json!({}))
        };
        assert!(apply(&state, &unknown, &policy).is_err());
        let mut resumed = state.clone();
        resumed.status = Status::Recovering;
        resumed.coder.agent_status = AgentStatus::Interrupted;
        let mut draft = event(EventType::SessionResumed, json!({"role":"CODER"}));
        draft.session_id = Some("coder".into());
        let transition = apply(&resumed, &draft, &policy).unwrap();
        assert_eq!(transition.status, Status::Running);
        assert_eq!(
            transition.agent_updates,
            vec![(AgentRole::Coder, AgentStatus::Running)]
        );
        assert!(apply(&state, &draft, &policy).is_err());
    }

    #[test]
    fn spec_update_bumps_version_invalidates_reports_and_returns_to_planning() {
        let policy = StatePolicy::default();
        let state = record(Phase::Implementing, Status::Running);
        let mut next = spec();
        next.goal = "Changed goal".into();
        let transition = apply(
            &state,
            &event(EventType::SpecUpdated, json!({"spec":next})),
            &policy,
        )
        .unwrap();
        assert_eq!(transition.spec_version, 2);
        assert_eq!(transition.phase, Phase::Planning);
        assert_eq!(transition.status, Status::Running);
        assert_eq!(
            transition.invalidated,
            vec![
                ReportKind::Plan,
                ReportKind::Implementation,
                ReportKind::Review,
                ReportKind::Harness,
                ReportKind::Integration
            ]
        );
        assert!(matches!(
            apply(
                &state,
                &event(EventType::SpecUpdated, json!({"spec": {"title": ""}})),
                &policy
            ),
            Err(StateError::InvalidPayload { .. })
        ));
    }

    fn approved_review(commit: &str) -> TaskReviewReport {
        TaskReviewReport {
            verdict: ReviewVerdict::Approved,
            spec_version: 1,
            round: 0,
            stage: ReviewStage::FinalDelivery,
            reviewed_versions: vec![ReviewedVersion {
                repository_id: "repo-code".into(),
                commit: Some(commit.into()),
                snapshot_ref: None,
            }],
            acceptance_results: vec![AcceptanceResult {
                criterion: "AC-001".into(),
                passed: true,
                evidence: vec!["test".into()],
            }],
            issues: vec![],
            verification: vec!["cargo test".into()],
            limitations: vec![],
        }
    }

    fn report(kind: ReportKind, body: Value) -> Report {
        Report {
            report_id: format!("{kind:?}"),
            task_id: "task-001".into(),
            kind,
            spec_version: 1,
            round: 0,
            body,
            created_at: "2026-01-01T00:00:00Z".into(),
            invalidated: false,
        }
    }

    fn reports_ok() -> Vec<Report> {
        vec![
            report(
                ReportKind::Review,
                serde_json::to_value(approved_review("integrated")).unwrap(),
            ),
            report(ReportKind::Harness, json!({"spec_version":1,"findings":[]})),
            report(
                ReportKind::Implementation,
                json!({"spec_version":1,"summary":"done"}),
            ),
        ]
    }

    #[test]
    fn completion_gate_passes_only_when_all_eight_conditions_hold() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();
        completion_gate(&state, &repos, &reports_ok(), &[]).unwrap();
    }

    #[test]
    fn every_completion_condition_can_fail_independently() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();
        let mut reports = reports_ok();

        // 1. Stale specification.
        let mut stale = reports.clone();
        stale[0].spec_version = 2;
        let unmet = completion_gate(&state, &repos, &stale, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::SpecMismatch { .. })));

        // 2. Rejected verdict.
        let mut rejected = reports.clone();
        let mut body = approved_review("integrated");
        body.verdict = ReviewVerdict::ChangesRequested;
        rejected[0].body = serde_json::to_value(body).unwrap();
        let unmet = completion_gate(&state, &repos, &rejected, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::FinalReviewNotApproved { .. })));

        // 3. Delivered commit differs from the reviewed commit.
        let mut version = reports.clone();
        let mut body = approved_review("submitted");
        body.reviewed_versions[0].commit = Some("submitted".into());
        version[0].body = serde_json::to_value(body).unwrap();
        let unmet = completion_gate(&state, &repos, &version, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::VersionMismatch { .. })));

        // 4. Integration incomplete.
        let mut integration = state.clone();
        integration.repositories[0].integration_status = IntegrationStatus::InProgress;
        let unmet =
            completion_gate(&integration, &integration.repositories, &reports, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::IntegrationIncomplete { .. })));

        // 5. Verification incomplete.
        let mut verification = state.clone();
        verification.repositories[0].verification_status = VerificationStatus::Failed;
        let unmet =
            completion_gate(&verification, &verification.repositories, &reports, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::VerificationIncomplete { .. })));
        let mut no_verification = reports.clone();
        let mut body = approved_review("integrated");
        body.verification.clear();
        no_verification[0].body = serde_json::to_value(body).unwrap();
        let unmet = completion_gate(&state, &repos, &no_verification, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| *item == Unmet::ReviewVerificationMissing));

        // 6. Non-Git target incomplete.
        let mut non_git = state.clone();
        non_git.non_git_targets = vec![NonGitTarget {
            task_id: "task-001".into(),
            path: "/workspace/scripts".into(),
            snapshot_dir: "/data/snapshots/1".into(),
            snapshot_manifest_json: "{}".into(),
            diff_ref: None,
            status: NonGitStatus::Prepared,
            lock_key: "path:/workspace/scripts".into(),
        }];
        let unmet = completion_gate(&non_git, &non_git.repositories, &reports, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::NonGitTargetIncomplete { .. })));

        // 7. Pending approval and a blocking harness report.
        let approval = Approval {
            approval_id: "approval-1".into(),
            task_id: "task-001".into(),
            pair_id: "crp-001".into(),
            source_session_id: "coder".into(),
            parent_session_id: "router".into(),
            request_type: ApprovalType::DeliveryBlocker,
            request: json!({}),
            task_spec_version: 1,
            permission_policy_version: 1,
            status: ApprovalStatus::Pending,
            answer: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            resolved_at: None,
        };
        let unmet = completion_gate(&state, &repos, &reports, &[approval]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::UnresolvedApproval { .. })));
        let mut blocking = reports.clone();
        blocking[1].body = json!({"spec_version":1,"findings":[{"category":"HARNESS","severity":"HIGH","phenomenon":"lost output"}]});
        let unmet = completion_gate(&state, &repos, &blocking, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::BlockingReport { .. })));

        // 8. Missing evidence and sessions.
        let mut evidence = reports.clone();
        evidence.remove(2);
        let unmet = completion_gate(&state, &repos, &evidence, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| matches!(
            item,
            Unmet::EvidenceMissing {
                report: ReportKind::Implementation
            }
        )));
        let mut no_session = state.clone();
        no_session.reviewer.session_id = None;
        let unmet = completion_gate(&no_session, &repos, &reports, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| matches!(
            item,
            Unmet::SessionMissing {
                role: AgentRole::Reviewer
            }
        )));

        // A stale report is not silently reused after a spec update.
        let mut invalidated = reports.clone();
        invalidated[0].invalidated = true;
        invalidated[1].invalidated = true;
        invalidated[2].invalidated = true;
        let unmet = completion_gate(&state, &repos, &invalidated, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| *item == Unmet::FinalReviewMissing));
    }
}
