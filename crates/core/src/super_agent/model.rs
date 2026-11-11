//! Typed Super Agent task records, reports and approvals.
//!
//! These structures are the JSON contract shared by the store, the deterministic
//! state machine, the CLI and the Desktop adapters. A model reports facts in a
//! typed body; only the orchestrator decides task state.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub fn parse(value: &str) -> Result<Self> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    other => anyhow::bail!("Unknown {} value: {other}", stringify!($name)),
                }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Phase {
    Discovering,
    Planning,
    Implementing,
    Reviewing,
    Integrating,
    FinalReview,
    Finalizing,
}
text_enum!(Phase {
    Discovering => "DISCOVERING",
    Planning => "PLANNING",
    Implementing => "IMPLEMENTING",
    Reviewing => "REVIEWING",
    Integrating => "INTEGRATING",
    FinalReview => "FINAL_REVIEW",
    Finalizing => "FINALIZING",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Queued,
    Running,
    WaitingUser,
    Blocked,
    Recovering,
    Paused,
    Completed,
    Failed,
    Cancelled,
}
text_enum!(Status {
    Queued => "QUEUED",
    Running => "RUNNING",
    WaitingUser => "WAITING_USER",
    Blocked => "BLOCKED",
    Recovering => "RECOVERING",
    Paused => "PAUSED",
    Completed => "COMPLETED",
    Failed => "FAILED",
    Cancelled => "CANCELLED",
});

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::WaitingUser | Self::Recovering
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentStatus {
    #[default]
    Idle,
    Running,
    WaitingTool,
    WaitingApproval,
    Interrupted,
    Stopped,
}
text_enum!(AgentStatus {
    Idle => "IDLE",
    Running => "RUNNING",
    WaitingTool => "WAITING_TOOL",
    WaitingApproval => "WAITING_APPROVAL",
    Interrupted => "INTERRUPTED",
    Stopped => "STOPPED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CleanupStatus {
    #[default]
    NotStarted,
    Pending,
    Completed,
}
text_enum!(CleanupStatus {
    NotStarted => "NOT_STARTED",
    Pending => "PENDING",
    Completed => "COMPLETED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReportKind {
    Review,
    Harness,
    Integration,
    Implementation,
    Plan,
}
text_enum!(ReportKind {
    Review => "REVIEW",
    Harness => "HARNESS",
    Integration => "INTEGRATION",
    Implementation => "IMPLEMENTATION",
    Plan => "PLAN",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
    Blocked,
}
text_enum!(ReviewVerdict {
    Approved => "APPROVED",
    ChangesRequested => "CHANGES_REQUESTED",
    Blocked => "BLOCKED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewStage {
    /// Review of the Coder's candidate implementation.
    #[default]
    Implementation,
    /// Review of the final delivered content after integration or snapshotting.
    FinalDelivery,
}
text_enum!(ReviewStage {
    Implementation => "IMPLEMENTATION",
    FinalDelivery => "FINAL_DELIVERY",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IntegrationStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}
text_enum!(IntegrationStatus {
    Pending => "PENDING",
    InProgress => "IN_PROGRESS",
    Completed => "COMPLETED",
    Failed => "FAILED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationStatus {
    #[default]
    Pending,
    Passed,
    Failed,
}
text_enum!(VerificationStatus {
    Pending => "PENDING",
    Passed => "PASSED",
    Failed => "FAILED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NonGitStatus {
    #[default]
    Prepared,
    InProgress,
    Completed,
    Failed,
}
text_enum!(NonGitStatus {
    Prepared => "PREPARED",
    InProgress => "IN_PROGRESS",
    Completed => "COMPLETED",
    Failed => "FAILED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalType {
    Plan,
    Tool,
    DeliveryBlocker,
    Integration,
}
text_enum!(ApprovalType {
    Plan => "PLAN",
    Tool => "TOOL",
    DeliveryBlocker => "DELIVERY_BLOCKER",
    Integration => "INTEGRATION",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Cancelled,
    Expired,
}
text_enum!(ApprovalStatus {
    Pending => "PENDING",
    Approved => "APPROVED",
    Rejected => "REJECTED",
    Cancelled => "CANCELLED",
    Expired => "EXPIRED",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentRole {
    Coder,
    Reviewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}
text_enum!(Severity {
    Low => "LOW",
    Medium => "MEDIUM",
    High => "HIGH",
    Critical => "CRITICAL",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HarnessCategory {
    Model,
    Harness,
    Tool,
    Environment,
    RequirementAmbiguity,
}
text_enum!(HarnessCategory {
    Model => "MODEL",
    Harness => "HARNESS",
    Tool => "TOOL",
    Environment => "ENVIRONMENT",
    RequirementAmbiguity => "REQUIREMENT_AMBIGUITY",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    pub workspace: PathBuf,
    #[serde(default)]
    pub targets: Vec<String>,
    pub goal: String,
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    /// Applies to every Git target unless `repository_target_branches` names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
    /// Keyed by target path or repository root as written in the specification.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repository_target_branches: BTreeMap<String, String>,
}

impl TaskSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.title.trim().is_empty() && self.title.chars().count() <= 200,
            "Task title must contain 1-200 characters"
        );
        ensure!(
            self.workspace.is_absolute(),
            "Task workspace must be an absolute path"
        );
        ensure!(
            !self.goal.trim().is_empty() && self.goal.chars().count() <= 4_000,
            "Task goal must contain 1-4000 characters"
        );
        ensure!(
            self.targets.len() <= 64,
            "A task may have at most 64 targets"
        );
        for target in &self.targets {
            ensure!(
                !target.trim().is_empty() && target.len() <= 4_096,
                "Task targets must be non-empty paths"
            );
            ensure!(
                PathBuf::from(target).is_absolute(),
                "Task targets must be absolute paths"
            );
        }
        for (name, values) in [
            ("requirements", &self.requirements),
            ("constraints", &self.constraints),
            ("out_of_scope", &self.out_of_scope),
        ] {
            ensure!(values.len() <= 64, "Too many {name} entries");
            for value in values {
                ensure!(
                    !value.trim().is_empty() && value.chars().count() <= 2_000,
                    "{name} entries must contain 1-2000 characters"
                );
            }
        }
        ensure!(
            self.acceptance_criteria.len() <= 64,
            "A task may have at most 64 acceptance criteria"
        );
        let mut ids = std::collections::BTreeSet::new();
        for criterion in &self.acceptance_criteria {
            ensure!(
                !criterion.id.is_empty()
                    && criterion
                        .id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "Acceptance criterion IDs must be non-empty and use letters, digits, dashes or underscores"
            );
            ensure!(
                ids.insert(criterion.id.clone()),
                "Duplicate acceptance criterion: {}",
                criterion.id
            );
            ensure!(
                !criterion.description.trim().is_empty()
                    && criterion.description.chars().count() <= 2_000,
                "Acceptance criterion descriptions must contain 1-2000 characters"
            );
        }
        if let Some(branch) = &self.target_branch {
            ensure!(
                !branch.trim().is_empty(),
                "Configured target branch must not be empty"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub previous_session_ids: Vec<String>,
    #[serde(default)]
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTarget {
    pub task_id: String,
    pub repository_id: String,
    pub repo_root: PathBuf,
    pub git_common_dir: PathBuf,
    /// The requested modification path inside this repository.
    pub target_path: PathBuf,
    pub target_branch: String,
    pub task_branch: String,
    /// Root of the managed worktree.
    pub worktree_path: PathBuf,
    /// The working directory inside the worktree; equals `worktree_path` at the root.
    pub working_path: PathBuf,
    pub base_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrated_commit: Option<String>,
    #[serde(default)]
    pub integration_status: IntegrationStatus,
    #[serde(default)]
    pub verification_status: VerificationStatus,
    /// Porcelain entries present in the source worktree at task setup.
    #[serde(default)]
    pub source_state: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonGitTarget {
    pub task_id: String,
    pub path: PathBuf,
    pub snapshot_dir: PathBuf,
    pub snapshot_manifest_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_ref: Option<String>,
    #[serde(default)]
    pub status: NonGitStatus,
    pub lock_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub pair_id: String,
    pub router_session_id: String,
    pub title: String,
    pub spec: TaskSpec,
    pub spec_version: u32,
    pub phase: Phase,
    pub status: Status,
    pub review_round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub coder: SessionRef,
    #[serde(default)]
    pub reviewer: SessionRef,
    #[serde(default)]
    pub cleanup_status: CleanupStatus,
    #[serde(default = "default_policy_version")]
    pub permission_policy_version: u32,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub repositories: Vec<RepoTarget>,
    #[serde(default)]
    pub non_git_targets: Vec<NonGitTarget>,
}

fn default_policy_version() -> u32 {
    1
}

impl TaskRecord {
    pub fn has_git_targets(&self) -> bool {
        !self.repositories.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    TaskCreated,
    DiscoveryCompleted,
    PlanSubmitted,
    ApprovalRequested,
    ApprovalResolved,
    ImplementationSubmitted,
    ReviewCompleted,
    HarnessAnalysisCompleted,
    IntegrationCompleted,
    NewTaskProposed,
    MessageSent,
    SessionInterrupted,
    SessionResumed,
    SpecUpdated,
    TaskPaused,
    TaskResumed,
    TaskBlocked,
    TaskCancelled,
    TaskFailed,
    TaskCompleted,
    CleanupCompleted,
}
text_enum!(EventType {
    TaskCreated => "TASK_CREATED",
    DiscoveryCompleted => "DISCOVERY_COMPLETED",
    PlanSubmitted => "PLAN_SUBMITTED",
    ApprovalRequested => "APPROVAL_REQUESTED",
    ApprovalResolved => "APPROVAL_RESOLVED",
    ImplementationSubmitted => "IMPLEMENTATION_SUBMITTED",
    ReviewCompleted => "REVIEW_COMPLETED",
    HarnessAnalysisCompleted => "HARNESS_ANALYSIS_COMPLETED",
    IntegrationCompleted => "INTEGRATION_COMPLETED",
    NewTaskProposed => "NEW_TASK_PROPOSED",
    MessageSent => "MESSAGE_SENT",
    SessionInterrupted => "SESSION_INTERRUPTED",
    SessionResumed => "SESSION_RESUMED",
    SpecUpdated => "SPEC_UPDATED",
    TaskPaused => "TASK_PAUSED",
    TaskResumed => "TASK_RESUMED",
    TaskBlocked => "TASK_BLOCKED",
    TaskCancelled => "TASK_CANCELLED",
    TaskFailed => "TASK_FAILED",
    TaskCompleted => "TASK_COMPLETED",
    CleanupCompleted => "CLEANUP_COMPLETED",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub event_type: EventType,
    #[serde(default)]
    pub causation_id: Option<String>,
    pub operation_id: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub task_id: String,
    pub pair_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub sequence: u64,
    pub event_type: EventType,
    #[serde(default)]
    pub causation_id: Option<String>,
    pub operation_id: String,
    pub timestamp: String,
    pub payload: Value,
    #[serde(default)]
    pub processed: bool,
    /// The specification version and review round the event belongs to. Gate
    /// evidence only counts events from the task's current contract.
    #[serde(default = "default_spec_version")]
    pub spec_version: u32,
    #[serde(default)]
    pub review_round: u32,
}

fn default_spec_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub approval_id: String,
    pub task_id: String,
    pub pair_id: String,
    pub source_session_id: String,
    pub parent_session_id: String,
    pub request_type: ApprovalType,
    pub request: Value,
    pub task_spec_version: u32,
    pub permission_policy_version: u32,
    pub status: ApprovalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub report_id: String,
    pub task_id: String,
    pub kind: ReportKind,
    pub spec_version: u32,
    pub round: u32,
    pub body: Value,
    pub created_at: String,
    #[serde(default)]
    pub invalidated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedVersion {
    pub repository_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceResult {
    pub criterion: String,
    pub passed: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub severity: Severity,
    pub location: String,
    pub description: String,
    #[serde(default)]
    pub expected: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskReviewReport {
    pub verdict: ReviewVerdict,
    pub spec_version: u32,
    #[serde(default)]
    pub round: u32,
    #[serde(default)]
    pub stage: ReviewStage,
    #[serde(default)]
    pub reviewed_versions: Vec<ReviewedVersion>,
    #[serde(default)]
    pub acceptance_results: Vec<AcceptanceResult>,
    #[serde(default)]
    pub issues: Vec<ReviewIssue>,
    #[serde(default)]
    pub verification: Vec<String>,
    #[serde(default)]
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessFinding {
    pub category: HarnessCategory,
    pub severity: Severity,
    pub phenomenon: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub probable_cause: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub suggestion: String,
    #[serde(default)]
    pub validation: String,
    #[serde(default)]
    pub create_task: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessAnalysisReport {
    pub spec_version: u32,
    #[serde(default)]
    pub round: u32,
    #[serde(default)]
    pub findings: Vec<HarnessFinding>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl HarnessAnalysisReport {
    /// A harness fault that undermines trust in the result blocks delivery.
    /// Ordinary efficiency suggestions do not.
    pub fn blocking(&self) -> bool {
        self.findings.iter().any(|finding| {
            finding.category == HarnessCategory::Harness && finding.severity >= Severity::High
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationTargetReport {
    pub repository_id: String,
    pub target_branch: String,
    #[serde(default)]
    pub before_commit: String,
    #[serde(default)]
    pub after_commit: String,
    #[serde(default)]
    pub conflict: bool,
    #[serde(default)]
    pub tests_passed: bool,
    #[serde(default)]
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationReport {
    pub spec_version: u32,
    #[serde(default)]
    pub round: u32,
    #[serde(default)]
    pub targets: Vec<IntegrationTargetReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    pub command: String,
    pub passed: bool,
    #[serde(default)]
    pub evidence_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImplementationReport {
    pub spec_version: u32,
    #[serde(default)]
    pub round: u32,
    pub summary: String,
    #[serde(default)]
    pub tests: Vec<TestResult>,
    #[serde(default)]
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReport {
    pub spec_version: u32,
    #[serde(default)]
    pub round: u32,
    pub understanding: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub verification: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
}

/// One unmet completion condition. The order is stable so reports and tests
/// can compare it without depending on hash-map iteration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Unmet {
    SpecMismatch {
        expected: u32,
        found: Option<u32>,
    },
    FinalReviewMissing,
    FinalReviewNotApproved {
        verdict: Option<ReviewVerdict>,
    },
    VersionMismatch {
        repository_id: String,
        expected: Option<String>,
        found: Option<String>,
    },
    IntegrationIncomplete {
        repository_id: String,
        status: IntegrationStatus,
    },
    VerificationIncomplete {
        repository_id: String,
        status: VerificationStatus,
    },
    ReviewVerificationMissing,
    NonGitTargetIncomplete {
        path: String,
        status: NonGitStatus,
    },
    UnresolvedApproval {
        approval_id: String,
        request_type: ApprovalType,
    },
    BlockingReport {
        report_id: String,
    },
    EvidenceMissing {
        report: ReportKind,
    },
    SessionMissing {
        role: AgentRole,
    },
    SessionUnverified {
        role: AgentRole,
    },
    MalformedReport {
        report_id: String,
        kind: ReportKind,
    },
    ReportSpecMismatch {
        report_id: String,
        kind: ReportKind,
        expected: u32,
        found: u32,
    },
    ReportRoundMismatch {
        report_id: String,
        kind: ReportKind,
        expected: u32,
        found: u32,
    },
    DeliveryVersionMissing {
        repository_id: String,
        version: String,
    },
    MissingAcceptance {
        criterion: String,
    },
    DuplicateAcceptance {
        criterion: String,
    },
    UnknownAcceptance {
        criterion: String,
    },
    FailedAcceptance {
        criterion: String,
        reason: String,
    },
    NonGitManifestInvalid {
        path: String,
        reason: String,
    },
    TraceMissing {
        event: EventType,
    },
}

impl Unmet {
    pub fn describe(&self) -> String {
        match self {
            Self::SpecMismatch { expected, found } => {
                format!("review spec version {found:?} does not match current version {expected}")
            }
            Self::FinalReviewMissing => "no final delivery review is recorded".into(),
            Self::FinalReviewNotApproved { verdict } => {
                format!("final delivery review verdict is {verdict:?}")
            }
            Self::VersionMismatch {
                repository_id,
                expected,
                found,
            } => format!("repository {repository_id} delivered {found:?}, reviewed {expected:?}"),
            Self::IntegrationIncomplete {
                repository_id,
                status,
            } => format!("repository {repository_id} integration is {status}"),
            Self::VerificationIncomplete {
                repository_id,
                status,
            } => format!("repository {repository_id} verification is {status}"),
            Self::NonGitTargetIncomplete { path, status } => {
                format!("non-Git target {path} is {status}")
            }
            Self::ReviewVerificationMissing => {
                "the final review records no independent verification".into()
            }
            Self::UnresolvedApproval {
                approval_id,
                request_type,
            } => format!("approval {approval_id} ({request_type}) is still pending"),
            Self::BlockingReport { report_id } => {
                format!("report {report_id} blocks delivery")
            }
            Self::EvidenceMissing { report } => format!("missing {report} evidence"),
            Self::SessionMissing { role } => format!("missing {role:?} session"),
            Self::MalformedReport { report_id, kind } => {
                format!("report {report_id} is not a valid {kind} body")
            }
            Self::SessionUnverified { role } => {
                format!("{role:?} session does not exist in the durable store")
            }
            Self::ReportSpecMismatch {
                report_id,
                kind,
                expected,
                found,
            } => format!(
                "{kind} report {report_id} describes spec version {found}, current version is {expected}"
            ),
            Self::ReportRoundMismatch {
                report_id,
                kind,
                expected,
                found,
            } => format!(
                "{kind} report {report_id} describes round {found}, current round is {expected}"
            ),
            Self::DeliveryVersionMissing {
                repository_id,
                version,
            } => format!("repository {repository_id} has no {version} version"),
            Self::MissingAcceptance { criterion } => {
                format!("acceptance criterion {criterion} has no result")
            }
            Self::DuplicateAcceptance { criterion } => {
                format!("acceptance criterion {criterion} is reported more than once")
            }
            Self::UnknownAcceptance { criterion } => {
                format!("acceptance result names unknown criterion {criterion}")
            }
            Self::FailedAcceptance { criterion, reason } => {
                format!("acceptance criterion {criterion} did not pass: {reason}")
            }
            Self::NonGitManifestInvalid { path, reason } => {
                format!("non-Git target {path} has no verified snapshot: {reason}")
            }
            Self::TraceMissing { event } => {
                format!("the durable task trace has no {event} event")
            }
        }
    }
}

/// Filesystem and database facts that the pure completion gate cannot obtain
/// itself. The store fills it inside the completion transaction; tests supply
/// it explicitly. It is never produced by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateEvidence {
    /// Sessions that exist, are not archived, and are readable in the store.
    pub verified_sessions: BTreeSet<String>,
    /// Event types persisted for this task.
    pub event_types: BTreeSet<EventType>,
    /// Verified snapshot state keyed by the target path as stored on the task.
    pub non_git_manifests: BTreeMap<String, NonGitVerification>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NonGitVerification {
    pub valid: bool,
    pub source_matches: bool,
    pub payload_verified: bool,
    /// Entries the snapshot could not protect (build output, non-UTF-8 names).
    pub skipped: Vec<String>,
    pub error: Option<String>,
}

impl NonGitVerification {
    pub fn complete(&self) -> bool {
        self.valid && self.source_matches && self.payload_verified
    }
}

/// Decode a typed report body, preserving the report ID for diagnostics.
pub fn decode_body<T: for<'de> Deserialize<'de>>(report: &Report) -> Result<T> {
    serde_json::from_value(report.body.clone())
        .with_context(|| format!("Invalid {} report {}", report.kind, report.report_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> TaskSpec {
        TaskSpec {
            title: "Fix login".into(),
            workspace: PathBuf::from("/workspace"),
            targets: vec!["/workspace/code".into()],
            goal: "Show accurate errors".into(),
            requirements: vec!["Distinguish credential and service errors".into()],
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC-001".into(),
                description: "Errors match the contract".into(),
            }],
            constraints: vec!["Do not touch unrelated logic".into()],
            out_of_scope: vec!["Rewriting auth".into()],
            target_branch: Some("develop".into()),
            repository_target_branches: BTreeMap::new(),
        }
    }

    #[test]
    fn spec_round_trips_and_rejects_empty_or_duplicate_fields() {
        let spec = spec();
        spec.validate().unwrap();
        let encoded = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<TaskSpec>(&encoded).unwrap(), spec);
        let mut duplicate = spec.clone();
        duplicate.acceptance_criteria.push(AcceptanceCriterion {
            id: "AC-001".into(),
            description: "duplicate".into(),
        });
        assert!(duplicate.validate().is_err());
        let mut relative = spec.clone();
        relative.workspace = PathBuf::from("relative");
        assert!(relative.validate().is_err());
        let mut target = spec;
        target.targets = vec!["relative".into()];
        assert!(target.validate().is_err());
    }

    #[test]
    fn report_bodies_are_typed_and_harness_blocking_depends_on_category_and_severity() {
        let report = Report {
            report_id: "r1".into(),
            task_id: "task-001".into(),
            kind: ReportKind::Harness,
            spec_version: 1,
            round: 0,
            body: json!({"spec_version":1,"findings":[{"category":"HARNESS","severity":"HIGH","phenomenon":"lost output"}]}),
            created_at: "now".into(),
            invalidated: false,
        };
        let body: HarnessAnalysisReport = decode_body(&report).unwrap();
        assert!(body.blocking());
        let advisory: HarnessAnalysisReport = serde_json::from_value(json!({
            "spec_version":1,
            "findings":[{"category":"HARNESS","severity":"LOW","phenomenon":"slow search"}]
        }))
        .unwrap();
        assert!(!advisory.blocking());
        let model_issue: HarnessAnalysisReport = serde_json::from_value(json!({
            "spec_version":1,
            "findings":[{"category":"MODEL","severity":"CRITICAL","phenomenon":"wrong edit"}]
        }))
        .unwrap();
        assert!(!model_issue.blocking());
    }

    #[test]
    fn enum_text_round_trips_for_persistence() {
        for phase in [
            Phase::Discovering,
            Phase::Planning,
            Phase::Implementing,
            Phase::Reviewing,
            Phase::Integrating,
            Phase::FinalReview,
            Phase::Finalizing,
        ] {
            assert_eq!(Phase::parse(phase.as_str()).unwrap(), phase);
        }
        for status in [
            Status::Queued,
            Status::Running,
            Status::WaitingUser,
            Status::Blocked,
            Status::Recovering,
            Status::Paused,
            Status::Completed,
            Status::Failed,
            Status::Cancelled,
        ] {
            assert_eq!(Status::parse(status.as_str()).unwrap(), status);
        }
        assert!(Status::parse("running").is_err());
    }
}
