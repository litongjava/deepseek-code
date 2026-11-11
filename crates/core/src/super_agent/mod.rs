//! Super Agent routing, task persistence, isolation and orchestration.
//!
//! The deterministic parts (task state, store, discovery, isolation) live here
//! and are shared by the CLI and Desktop hosts. Model-facing orchestration is
//! layered on top and never decides task state itself.
pub mod config;
pub mod discovery;
pub mod isolation;
pub mod model;
pub mod role;
pub mod state;
pub mod store;

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
