//! Deterministic task state transitions and the completion gate.
//!
//! A model may submit evidence, but only these pure functions decide whether a
//! task moves. Keeping them pure makes every rejection and every unmet
//! completion condition testable without a database or a model call.
use super::model::{
    decode_body, AgentRole, AgentStatus, Approval, ApprovalStatus, ApprovalType, CleanupStatus,
    Event, EventType, GateEvidence, HarnessAnalysisReport, ImplementationReport, NewEvent,
    NonGitStatus, Phase, RepoTarget, Report, ReportKind, ReviewStage, ReviewVerdict, SessionRef,
    Status, TaskRecord, TaskReviewReport, Unmet,
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
        EventType::NewTaskProposed
        | EventType::MessageSent
        | EventType::ReviewSubmitted
        | EventType::HarnessSubmitted => {
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

pub fn non_git_repository_id(path: &std::path::Path) -> String {
    format!("non-git:{}", path.display())
}

fn bind_report(
    record: &TaskRecord,
    report: &Report,
    spec_version: u32,
    round: u32,
    unmet: &mut Vec<Unmet>,
) -> bool {
    if spec_version != record.spec_version {
        unmet.push(Unmet::ReportSpecMismatch {
            report_id: report.report_id.clone(),
            kind: report.kind,
            expected: record.spec_version,
            found: spec_version,
        });
        return false;
    }
    if round != report.round {
        unmet.push(Unmet::ReportRoundMismatch {
            report_id: report.report_id.clone(),
            kind: report.kind,
            expected: report.round,
            found: round,
        });
        return false;
    }
    true
}

fn acceptance_unmet(record: &TaskRecord, body: &TaskReviewReport) -> Vec<Unmet> {
    let expected: std::collections::BTreeSet<&str> = record
        .spec
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.id.as_str())
        .collect();
    let mut unmet = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for result in &body.acceptance_results {
        if !expected.contains(result.criterion.as_str()) {
            unmet.push(Unmet::UnknownAcceptance {
                criterion: result.criterion.clone(),
            });
            continue;
        }
        if !seen.insert(result.criterion.as_str()) {
            unmet.push(Unmet::DuplicateAcceptance {
                criterion: result.criterion.clone(),
            });
            continue;
        }
        if !result.passed {
            unmet.push(Unmet::FailedAcceptance {
                criterion: result.criterion.clone(),
                reason: "the reviewer marked it failed".into(),
            });
        } else if !result
            .evidence
            .iter()
            .any(|evidence| !evidence.trim().is_empty())
        {
            unmet.push(Unmet::FailedAcceptance {
                criterion: result.criterion.clone(),
                reason: "no evidence was recorded".into(),
            });
        }
    }
    for criterion in expected {
        if !seen.contains(criterion) {
            unmet.push(Unmet::MissingAcceptance {
                criterion: criterion.into(),
            });
        }
    }
    unmet
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The eight documented completion conditions. Every condition is evaluated
/// even when an earlier one fails, so the caller can show all remaining work.
///
/// Only the current specification version and current review round decide the
/// result. An earlier BLOCKED or rejected round stays in history without
/// permanently poisoning a later approved round. Durable facts that the gate
/// cannot obtain itself (existing sessions, persisted trace, verified non-Git
/// snapshots) arrive through `evidence`; the gate never touches the database
/// or filesystem.
pub fn completion_gate(
    record: &TaskRecord,
    repos: &[RepoTarget],
    reports: &[Report],
    approvals: &[Approval],
    evidence: &GateEvidence,
) -> Result<(), Vec<Unmet>> {
    let mut unmet = Vec::new();
    // Historical rounds and superseded specification versions are evidence,
    // not active requirements.
    let round_reports: Vec<&Report> = reports
        .iter()
        .filter(|report| {
            !report.invalidated
                && report.spec_version == record.spec_version
                && report.round == record.review_round
        })
        .collect();

    let mut final_reviews: Vec<(&Report, TaskReviewReport)> = Vec::new();
    let mut harness_reports: Vec<(&Report, HarnessAnalysisReport)> = Vec::new();
    let mut implementation_present = false;
    for report in &round_reports {
        match report.kind {
            ReportKind::Review => match decode_body::<TaskReviewReport>(report) {
                Ok(body) => {
                    if bind_report(record, report, body.spec_version, body.round, &mut unmet)
                        && body.stage == ReviewStage::FinalDelivery
                    {
                        final_reviews.push((report, body));
                    }
                }
                Err(_) => unmet.push(Unmet::MalformedReport {
                    report_id: report.report_id.clone(),
                    kind: ReportKind::Review,
                }),
            },
            ReportKind::Harness => match decode_body::<HarnessAnalysisReport>(report) {
                Ok(body) => {
                    if bind_report(record, report, body.spec_version, body.round, &mut unmet) {
                        harness_reports.push((report, body));
                    }
                }
                Err(_) => unmet.push(Unmet::MalformedReport {
                    report_id: report.report_id.clone(),
                    kind: ReportKind::Harness,
                }),
            },
            ReportKind::Implementation => match decode_body::<ImplementationReport>(report) {
                Ok(body) => {
                    if bind_report(record, report, body.spec_version, body.round, &mut unmet) {
                        implementation_present = true;
                    }
                }
                Err(_) => unmet.push(Unmet::MalformedReport {
                    report_id: report.report_id.clone(),
                    kind: ReportKind::Implementation,
                }),
            },
            ReportKind::Plan | ReportKind::Integration => {}
        }
    }
    let final_review = final_reviews.into_iter().max_by(|left, right| {
        left.0
            .created_at
            .cmp(&right.0.created_at)
            .then_with(|| left.0.report_id.cmp(&right.0.report_id))
    });

    // 1. The final delivery review must describe the current contract version.
    if final_review.is_none() {
        let all_reviews: Vec<&Report> = reports
            .iter()
            .filter(|report| report.kind == ReportKind::Review && !report.invalidated)
            .collect();
        match latest_report(&all_reviews) {
            Some(report) if report.spec_version != record.spec_version => {
                unmet.push(Unmet::SpecMismatch {
                    expected: record.spec_version,
                    found: Some(report.spec_version),
                });
            }
            Some(report) if report.round != record.review_round => {
                if let Ok(body) = decode_body::<TaskReviewReport>(report) {
                    if body.spec_version == record.spec_version && body.round == report.round {
                        unmet.push(Unmet::ReportRoundMismatch {
                            report_id: report.report_id.clone(),
                            kind: ReportKind::Review,
                            expected: record.review_round,
                            found: report.round,
                        });
                    } else {
                        unmet.push(Unmet::FinalReviewMissing);
                    }
                } else {
                    unmet.push(Unmet::MalformedReport {
                        report_id: report.report_id.clone(),
                        kind: ReportKind::Review,
                    });
                }
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
        // 2. The final task acceptance passed, with evidence for every
        //    acceptance criterion and no fabricated or duplicate results.
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
        unmet.extend(acceptance_unmet(record, body));

        // 3. Final delivered content equals the reviewed version.
        for repo in repos {
            let Some(reviewed) = body
                .reviewed_versions
                .iter()
                .find(|version| version.repository_id == repo.repository_id)
                .and_then(|version| nonempty(&version.commit))
            else {
                unmet.push(Unmet::DeliveryVersionMissing {
                    repository_id: repo.repository_id.clone(),
                    version: "reviewed".into(),
                });
                continue;
            };
            let delivered = nonempty(&repo.integrated_commit);
            let recorded = nonempty(&repo.reviewed_commit);
            if delivered.is_none() || recorded.is_none() {
                unmet.push(Unmet::DeliveryVersionMissing {
                    repository_id: repo.repository_id.clone(),
                    version: if delivered.is_none() {
                        "integrated".into()
                    } else {
                        "reviewed".into()
                    },
                });
                continue;
            }
            if delivered != recorded || delivered != Some(reviewed) {
                unmet.push(Unmet::VersionMismatch {
                    repository_id: repo.repository_id.clone(),
                    expected: repo.reviewed_commit.clone(),
                    found: repo.integrated_commit.clone(),
                });
            }
        }
        for target in &record.non_git_targets {
            let id = non_git_repository_id(&target.path);
            let reviewed = body
                .reviewed_versions
                .iter()
                .find(|version| version.repository_id == id)
                .and_then(|version| nonempty(&version.snapshot_ref));
            if reviewed.is_none() {
                unmet.push(Unmet::DeliveryVersionMissing {
                    repository_id: id,
                    version: "snapshot".into(),
                });
            } else if reviewed != nonempty(&target.diff_ref) {
                unmet.push(Unmet::VersionMismatch {
                    repository_id: id,
                    expected: target.diff_ref.clone(),
                    found: reviewed.map(str::to_owned),
                });
            }
        }
        // 5. Independent verification is recorded by the reviewer.
        if body
            .verification
            .iter()
            .all(|verification| verification.trim().is_empty())
        {
            unmet.push(Unmet::ReviewVerificationMissing);
        }
    }

    // 4. Every Git target was integrated by the Coder with a real version.
    for repo in repos {
        for (label, value) in [
            ("submitted", &repo.submitted_commit),
            ("reviewed", &repo.reviewed_commit),
            ("integrated", &repo.integrated_commit),
        ] {
            if nonempty(value).is_none() {
                unmet.push(Unmet::DeliveryVersionMissing {
                    repository_id: repo.repository_id.clone(),
                    version: label.into(),
                });
            }
        }
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

    // 6. Non-Git targets have a recorded diff and a verified snapshot.
    for target in &record.non_git_targets {
        let path = target.path.display().to_string();
        let complete = target.status == NonGitStatus::Completed
            && nonempty(&target.diff_ref).is_some()
            && !target.snapshot_manifest_json.trim().is_empty()
            && !target.snapshot_dir.as_os_str().is_empty();
        if !complete {
            unmet.push(Unmet::NonGitTargetIncomplete {
                path: path.clone(),
                status: target.status,
            });
        }
        match evidence.non_git_manifests.get(&path) {
            Some(verification) if verification.complete() => {}
            Some(verification) => unmet.push(Unmet::NonGitManifestInvalid {
                path,
                reason: verification
                    .error
                    .clone()
                    .unwrap_or_else(|| "snapshot contents failed verification".into()),
            }),
            None => unmet.push(Unmet::NonGitManifestInvalid {
                path,
                reason: "the stored snapshot was not verified".into(),
            }),
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
    // Only the current round's harness analysis can block. An older blocking
    // finding is resolved evidence once a newer round has been analysed.
    let harness_present = !harness_reports.is_empty();
    if let Some((report, body)) = harness_reports.into_iter().max_by(|left, right| {
        left.0
            .created_at
            .cmp(&right.0.created_at)
            .then_with(|| left.0.report_id.cmp(&right.0.report_id))
    }) {
        if body.blocking() {
            unmet.push(Unmet::BlockingReport {
                report_id: report.report_id.clone(),
            });
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
    if !implementation_present {
        unmet.push(Unmet::EvidenceMissing {
            report: ReportKind::Implementation,
        });
    }
    if !harness_present {
        unmet.push(Unmet::EvidenceMissing {
            report: ReportKind::Harness,
        });
    }
    if final_review.is_none() {
        unmet.push(Unmet::EvidenceMissing {
            report: ReportKind::Review,
        });
    }
    for (role, session) in [
        (AgentRole::Coder, &record.coder),
        (AgentRole::Reviewer, &record.reviewer),
    ] {
        match session.session_id.as_deref().map(str::trim) {
            None | Some("") => unmet.push(Unmet::SessionMissing { role }),
            Some(id) if !evidence.verified_sessions.contains(id) => {
                unmet.push(Unmet::SessionUnverified { role })
            }
            Some(_) => {}
        }
    }
    for event in [
        EventType::ImplementationSubmitted,
        EventType::ReviewCompleted,
        EventType::HarnessAnalysisCompleted,
    ] {
        if !evidence.event_types.contains(&event) {
            unmet.push(Unmet::TraceMissing { event });
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
        let step = |state: &mut TaskRecord, draft: NewEvent| {
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

    fn evidence_for(record: &TaskRecord) -> GateEvidence {
        let mut evidence = GateEvidence::default();
        for id in [
            record.coder.session_id.as_deref(),
            record.reviewer.session_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            evidence.verified_sessions.insert(id.to_owned());
        }
        evidence.event_types = [
            EventType::ImplementationSubmitted,
            EventType::ReviewCompleted,
            EventType::HarnessAnalysisCompleted,
        ]
        .into_iter()
        .collect();
        evidence
    }

    fn gate(
        record: &TaskRecord,
        repos: &[RepoTarget],
        reports: &[Report],
        approvals: &[Approval],
    ) -> Result<(), Vec<Unmet>> {
        completion_gate(record, repos, reports, approvals, &evidence_for(record))
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
        gate(&state, &repos, &reports_ok(), &[]).unwrap();
    }

    #[test]
    fn every_completion_condition_can_fail_independently() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();
        let reports = reports_ok();

        // 1. Stale specification.
        let mut stale = reports.clone();
        stale[0].spec_version = 2;
        let unmet = gate(&state, &repos, &stale, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::SpecMismatch { .. })));

        // 2. Rejected verdict.
        let mut rejected = reports.clone();
        let mut body = approved_review("integrated");
        body.verdict = ReviewVerdict::ChangesRequested;
        rejected[0].body = serde_json::to_value(body).unwrap();
        let unmet = gate(&state, &repos, &rejected, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::FinalReviewNotApproved { .. })));

        // 3. Delivered commit differs from the reviewed commit.
        let mut version = reports.clone();
        let mut body = approved_review("submitted");
        body.reviewed_versions[0].commit = Some("submitted".into());
        version[0].body = serde_json::to_value(body).unwrap();
        let unmet = gate(&state, &repos, &version, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::VersionMismatch { .. })));

        // 4. Integration incomplete.
        let mut integration = state.clone();
        integration.repositories[0].integration_status = IntegrationStatus::InProgress;
        let unmet = gate(&integration, &integration.repositories, &reports, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::IntegrationIncomplete { .. })));

        // 5. Verification incomplete.
        let mut verification = state.clone();
        verification.repositories[0].verification_status = VerificationStatus::Failed;
        let unmet = gate(&verification, &verification.repositories, &reports, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::VerificationIncomplete { .. })));
        let mut no_verification = reports.clone();
        let mut body = approved_review("integrated");
        body.verification.clear();
        no_verification[0].body = serde_json::to_value(body).unwrap();
        let unmet = gate(&state, &repos, &no_verification, &[]).unwrap_err();
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
        let unmet = gate(&non_git, &non_git.repositories, &reports, &[]).unwrap_err();
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
        let unmet = gate(&state, &repos, &reports, &[approval]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::UnresolvedApproval { .. })));
        let mut blocking = reports.clone();
        blocking[1].body = json!({"spec_version":1,"findings":[{"category":"HARNESS","severity":"HIGH","phenomenon":"lost output"}]});
        let unmet = gate(&state, &repos, &blocking, &[]).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::BlockingReport { .. })));

        // 8. Missing evidence and sessions.
        let mut evidence = reports.clone();
        evidence.remove(2);
        let unmet = gate(&state, &repos, &evidence, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| matches!(
            item,
            Unmet::EvidenceMissing {
                report: ReportKind::Implementation
            }
        )));
        let mut no_session = state.clone();
        no_session.reviewer.session_id = None;
        let unmet = gate(&no_session, &repos, &reports, &[]).unwrap_err();
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
        let unmet = gate(&state, &repos, &invalidated, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| *item == Unmet::FinalReviewMissing));
    }

    /// An APPROVED verdict alone must not bypass per-criterion evidence.
    #[test]
    fn acceptance_results_must_cover_each_criterion_with_evidence() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();

        let gate_with = |body: TaskReviewReport| {
            let mut reports = reports_ok();
            reports[0].body = serde_json::to_value(body).unwrap();
            gate(&state, &repos, &reports, &[]).unwrap_err()
        };

        let mut missing = approved_review("integrated");
        missing.acceptance_results.clear();
        assert!(gate_with(missing).iter().any(
            |item| matches!(item, Unmet::MissingAcceptance { criterion } if criterion == "AC-001")
        ));

        let mut failed = approved_review("integrated");
        failed.acceptance_results[0].passed = false;
        assert!(gate_with(failed)
            .iter()
            .any(|item| matches!(item, Unmet::FailedAcceptance { criterion, .. } if criterion == "AC-001")));

        let mut no_evidence = approved_review("integrated");
        no_evidence.acceptance_results[0].evidence = vec!["  ".into()];
        assert!(gate_with(no_evidence).iter().any(
            |item| matches!(item, Unmet::FailedAcceptance { reason, .. } if reason.contains("evidence"))
        ));

        let mut duplicate = approved_review("integrated");
        duplicate
            .acceptance_results
            .push(duplicate.acceptance_results[0].clone());
        assert!(gate_with(duplicate).iter().any(
            |item| matches!(item, Unmet::DuplicateAcceptance { criterion } if criterion == "AC-001")
        ));

        let mut unknown = approved_review("integrated");
        unknown.acceptance_results.push(AcceptanceResult {
            criterion: "AC-999".into(),
            passed: true,
            evidence: vec!["fabricated".into()],
        });
        assert!(gate_with(unknown).iter().any(
            |item| matches!(item, Unmet::UnknownAcceptance { criterion } if criterion == "AC-999")
        ));
    }

    #[test]
    fn report_envelopes_and_bodies_must_match_the_active_version_and_round() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();

        let mut body_version = reports_ok();
        body_version[0].body =
            serde_json::to_value(approved_review("integrated").with_spec(2)).unwrap();
        let unmet = gate(&state, &repos, &body_version, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| matches!(
            item,
            Unmet::ReportSpecMismatch {
                kind: ReportKind::Review,
                ..
            }
        )));

        let mut body_round = reports_ok();
        let mut review = approved_review("integrated");
        review.round = 1;
        body_round[0].body = serde_json::to_value(review).unwrap();
        let unmet = gate(&state, &repos, &body_round, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| matches!(
            item,
            Unmet::ReportRoundMismatch {
                kind: ReportKind::Review,
                ..
            }
        )));

        // A review for an older round cannot satisfy the current round.
        let mut old_round = reports_ok();
        old_round[0].round = 1;
        let mut review = approved_review("integrated");
        review.round = 1;
        old_round[0].body = serde_json::to_value(review).unwrap();
        let unmet = gate(&state, &repos, &old_round, &[]).unwrap_err();
        assert!(unmet.iter().any(|item| matches!(
            item,
            Unmet::ReportRoundMismatch {
                expected: 0,
                found: 1,
                ..
            }
        )));
    }

    trait WithSpec {
        fn with_spec(self, spec: u32) -> Self;
    }
    impl WithSpec for TaskReviewReport {
        fn with_spec(mut self, spec: u32) -> Self {
            self.spec_version = spec;
            self
        }
    }

    #[test]
    fn empty_delivery_versions_are_rejected_even_when_they_compare_equal() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        for field in ["submitted", "reviewed", "integrated"] {
            let mut repo = state.repositories[0].clone();
            match field {
                "submitted" => repo.submitted_commit = Some(String::new()),
                "reviewed" => repo.reviewed_commit = Some(String::new()),
                _ => repo.integrated_commit = Some(String::new()),
            }
            let mut review = approved_review("integrated");
            if field != "submitted" {
                review.reviewed_versions[0].commit = Some(String::new());
            }
            let mut reports = reports_ok();
            reports[0].body = serde_json::to_value(review).unwrap();
            let unmet = gate(&state, &[repo], &reports, &[]).unwrap_err();
            assert!(
                unmet
                    .iter()
                    .any(|item| matches!(item, Unmet::DeliveryVersionMissing { .. })),
                "empty {field} version was accepted: {unmet:?}"
            );
        }
    }

    #[test]
    fn sessions_and_trace_must_be_durable_not_just_nonempty() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();
        let reports = reports_ok();
        let mut evidence = evidence_for(&state);
        evidence.verified_sessions.clear();
        let unmet = completion_gate(&state, &repos, &reports, &[], &evidence).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::SessionUnverified { .. })));
        assert!(!unmet
            .iter()
            .any(|item| matches!(item, Unmet::SessionMissing { .. })));

        let mut evidence = evidence_for(&state);
        evidence.event_types.clear();
        let unmet = completion_gate(&state, &repos, &reports, &[], &evidence).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::TraceMissing { .. })));
    }

    #[test]
    fn a_later_approved_round_supersedes_an_earlier_blocked_round() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.review_round = 1;
        state.repositories = vec![repo()];
        let repos = state.repositories.clone();

        let mut old_review = approved_review("integrated");
        old_review.round = 0;
        old_review.verdict = ReviewVerdict::Blocked;
        let mut old_report = report(
            ReportKind::Review,
            serde_json::to_value(old_review).unwrap(),
        );
        old_report.round = 0;
        old_report.created_at = "2026-01-01T00:00:00Z".into();
        let mut old_harness = report(
            ReportKind::Harness,
            json!({"spec_version":1,"round":0,"findings":[{"category":"HARNESS","severity":"HIGH","phenomenon":"lost output"}]}),
        );
        old_harness.round = 0;
        old_harness.created_at = "2026-01-01T00:00:00Z".into();

        let mut fresh_review = approved_review("integrated");
        fresh_review.round = 1;
        let mut fresh = vec![
            report(
                ReportKind::Review,
                serde_json::to_value(fresh_review).unwrap(),
            ),
            report(
                ReportKind::Harness,
                json!({"spec_version":1,"round":1,"findings":[]}),
            ),
            report(
                ReportKind::Implementation,
                json!({"spec_version":1,"round":1,"summary":"fixed"}),
            ),
        ];
        for item in &mut fresh {
            item.round = 1;
            item.created_at = "2026-01-02T00:00:00Z".into();
        }
        fresh.push(old_report);
        fresh.push(old_harness);
        gate(&state, &repos, &fresh, &[]).unwrap();
    }

    #[test]
    fn non_git_targets_require_a_verified_snapshot() {
        let mut state = record(Phase::Finalizing, Status::Running);
        state.repositories.clear();
        let path = std::path::PathBuf::from("/workspace/scripts");
        state.non_git_targets = vec![NonGitTarget {
            task_id: "task-001".into(),
            path: path.clone(),
            snapshot_dir: "/data/snapshots/1".into(),
            snapshot_manifest_json: "{\"source\":\"/workspace/scripts\"}".into(),
            diff_ref: Some("diff-1".into()),
            status: NonGitStatus::Completed,
            lock_key: "path:/workspace/scripts".into(),
        }];
        let mut review = approved_review("integrated");
        review.reviewed_versions = vec![ReviewedVersion {
            repository_id: non_git_repository_id(&path),
            commit: None,
            snapshot_ref: Some("diff-1".into()),
        }];
        let mut reports = reports_ok();
        reports[0].body = serde_json::to_value(review).unwrap();

        let evidence = evidence_for(&state);
        let unmet = completion_gate(&state, &[], &reports, &[], &evidence).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::NonGitManifestInvalid { .. })));

        let mut evidence = evidence_for(&state);
        evidence.non_git_manifests.insert(
            path.display().to_string(),
            super::super::model::NonGitVerification {
                valid: true,
                source_matches: true,
                payload_verified: true,
                skipped: vec!["node_modules".into()],
                error: None,
            },
        );
        completion_gate(&state, &[], &reports, &[], &evidence).unwrap();

        let mut corrupt = evidence;
        corrupt
            .non_git_manifests
            .get_mut(&path.display().to_string())
            .unwrap()
            .payload_verified = false;
        let unmet = completion_gate(&state, &[], &reports, &[], &corrupt).unwrap_err();
        assert!(unmet
            .iter()
            .any(|item| matches!(item, Unmet::NonGitManifestInvalid { .. })));
    }
}
