//! Super Agent routing, task persistence, isolation and orchestration.
//!
//! The deterministic parts (task state, store, discovery, isolation) live here
//! and are shared by the CLI and Desktop hosts. Model-facing orchestration is
//! layered on top and never decides task state itself.
pub mod cleanup;
pub mod config;
pub mod crp;
pub mod delivery;
pub mod discovery;
pub mod integration;
pub mod isolation;
pub mod launcher;
pub mod model;
pub mod orchestrator;
pub mod prompt;
pub mod review;
pub mod role;
pub mod state;
pub mod store;
pub mod watcher;

/// Absolute, slash-separated fixture root for logical task specs. Windows has
/// no POSIX `/workspace` root, so tests use a drive-qualified path there.
#[cfg(all(test, windows))]
pub(crate) const TEST_WORKSPACE: &str = "C:/workspace";
#[cfg(all(test, not(windows)))]
pub(crate) const TEST_WORKSPACE: &str = "/workspace";

/// Git for Windows rejects the extended-length prefix that canonicalization
/// adds. Test helpers pass these normalized arguments to Git; product paths
/// remain canonical so identity checks still compare like with like.
#[cfg(all(test, windows))]
pub(crate) fn test_git_text(value: &str) -> String {
    value.strip_prefix(r"\\?\").unwrap_or(value).to_string()
}
#[cfg(all(test, not(windows)))]
pub(crate) fn test_git_text(value: &str) -> String {
    value.to_string()
}
#[cfg(all(test, windows))]
pub(crate) fn test_git_path(path: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(test_git_text(&path.to_string_lossy()))
}
#[cfg(all(test, not(windows)))]
pub(crate) fn test_git_path(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

pub use config::{PlanApprovalPolicy, SuperAgentConfig};
pub use model::{
    AcceptanceCriterion, AgentRole, AgentStatus, Approval, ApprovalStatus, ApprovalType,
    CleanupStatus, Event, EventType, HarnessAnalysisReport, IntegrationStatus, NewEvent,
    NonGitStatus, NonGitTarget, Phase, RepoTarget, Report, ReportKind, ReviewStage, ReviewVerdict,
    SessionRef, Status, TaskRecord, TaskReviewReport, TaskSpec, Unmet, VerificationStatus,
};
pub use role::{MemberRole, Role};
pub use state::{completion_gate, StatePolicy};
pub use store::{EventAppend, LockOutcome, StoreError, TaskStore};
