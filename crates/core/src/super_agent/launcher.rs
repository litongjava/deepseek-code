//! Starts and resumes the Coder and Reviewer sessions of a task.
//!
//! The launcher reuses the extracted child-session path, so provider pinning,
//! permission inheritance, workspace binding and resume behave exactly like the
//! Agent tool's background children. It adds task-specific instructions, member
//! binding, the Reviewer's fixed validation checkout and durable launch
//! operations.
//!
//! Every initial launch and every resumed instruction has a durable operation
//! that reserves its session and job UUIDs before any external process starts.
//! A host crash can therefore be reconciled by exact identity: an already
//! started child is adopted, a never-started reservation continues with the
//! same IDs, and ambiguous or damaged evidence blocks with the files preserved.
use crate::native_engine::child_session::{
    launch_prepared_child, prepare_child_session, ChildLaunchConfig, ChildSpawn, ChildWorkspace,
    PreparedChildLaunch,
};
use crate::native_engine::provider_manager::ResolvedProvider;
use crate::services::background_sessions::{self, JobSnapshot};
use crate::services::ServiceContext;
use crate::super_agent::config::{AgentModelRole, ModelFallback, SuperAgentConfig};
use crate::super_agent::crp;
use crate::super_agent::delivery::{
    is_delivery_purpose, is_delivery_purpose_namespace, DeliveryOperationIdentity,
    DELIVERY_IDENTITY_VERSION, LEGACY_DELIVERY_PURPOSE,
};
use crate::super_agent::role::MemberRole;
use crate::super_agent::store::TaskStore;
use crate::super_agent::{Phase, TaskRecord};
use anyhow::{anyhow, ensure, Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A host-side test gate. Returning an error after a durable step simulates a
/// host crash at exactly that boundary. Production hosts leave it unset.
pub type LaunchGate = Arc<dyn Fn(&'static str) -> Result<()> + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub role: String,
    pub session_id: String,
    pub job_id: String,
    pub workspace: PathBuf,
    pub model: String,
    /// The repository delivery execution identity this handle belongs to.
    /// Absent for every other launch kind and for genuine legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryOperationIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub created_at: String,
}

/// Durable typed delivery identity stored next to the actual reserved
/// background job, before its supervisor is started. The operation record
/// alone is not proof that the *job* belongs to this delivery: two Coder
/// resumes of one plan share the Coder, workspace and Router, so the job's own
/// files must carry the identity that was reserved for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryJobBinding {
    /// Required. A typed sidecar without a version is incomplete and fails to
    /// deserialize instead of silently becoming the current format.
    pub version: u32,
    pub job_id: String,
    pub session_id: String,
    pub identity: DeliveryOperationIdentity,
}

/// The job-directory file that carries [`DeliveryJobBinding`]. It is written
/// atomically before the child is spawned and validated at every adoption.
const DELIVERY_BINDING_FILE: &str = "delivery-identity.json";

/// One durable reference that names a child process a task owns.
///
/// Completion reads these references directly instead of trusting the
/// background-task discovery list: that list intentionally tolerates missing
/// and malformed unrelated records, so its silence is not proof that a known
/// member job stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberJobReceipt {
    /// The durable storage key or table row the reference came from.
    pub source: String,
    /// `coder` or `reviewer`.
    pub role: String,
    /// The member session the job must belong to.
    pub session_id: String,
    /// The exact reserved background job UUID.
    pub job_id: String,
    /// The evidence purpose the receipt was reserved for. Empty for an initial
    /// launch; delivery purposes carry their typed identity.
    pub purpose: String,
    /// The specification version and review round the receipt belongs to.
    pub spec_version: u32,
    pub review_round: u32,
    /// The persisted delivery plan the receipt is bound to. Empty when the
    /// receipt cannot name one (an initial member launch, or a pre-delivery
    /// final-delivery review reserved before any plan existed).
    pub plan_operation_id: String,
}

impl MemberJobReceipt {
    fn new(source: impl Into<String>, role: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            role: role.into(),
            session_id: session.into(),
            job_id: String::new(),
            purpose: String::new(),
            spec_version: 0,
            review_round: 0,
            plan_operation_id: String::new(),
        }
    }

    fn with_job(mut self, job_id: impl Into<String>) -> Self {
        self.job_id = job_id.into();
        self
    }

    fn with_stage(mut self, purpose: impl Into<String>, spec: u32, round: u32) -> Self {
        self.purpose = purpose.into();
        self.spec_version = spec;
        self.review_round = round;
        self
    }

    fn with_plan(mut self, plan_operation_id: impl Into<String>) -> Self {
        self.plan_operation_id = plan_operation_id.into();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberJobState {
    /// The exact job exists and its supervisor recorded a terminal result.
    Stopped { status: String },
    /// The exact job is still working, including an expired heartbeat.
    Active { status: String },
    /// The reference cannot prove either a terminal or an active job.
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberJobObservation {
    pub receipt: MemberJobReceipt,
    pub state: MemberJobState,
}

/// Every known task-owned member job and what its real durable state proves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionMemberJobs {
    pub observations: Vec<MemberJobObservation>,
}

impl CompletionMemberJobs {
    /// Jobs whose exact process is still working.
    pub fn active(&self) -> impl Iterator<Item = &MemberJobObservation> {
        self.observations
            .iter()
            .filter(|observation| matches!(observation.state, MemberJobState::Active { .. }))
    }

    /// References whose process exit cannot be proven.
    pub fn unknown(&self) -> impl Iterator<Item = &MemberJobObservation> {
        self.observations
            .iter()
            .filter(|observation| matches!(observation.state, MemberJobState::Unknown { .. }))
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }
}

impl MemberJobObservation {
    pub fn describe(&self) -> String {
        let source = &self.receipt.source;
        let role = &self.receipt.role;
        let job = &self.receipt.job_id;
        let session = &self.receipt.session_id;
        match &self.state {
            MemberJobState::Stopped { status } => {
                format!("{source} names {role} job {job} for session {session}: {status}")
            }
            MemberJobState::Active { status } => {
                format!("{source} names {role} job {job} for session {session}: {status}")
            }
            MemberJobState::Unknown { reason } => {
                format!("{source} names {role} job {job} for session {session}: {reason}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOutcome {
    pub session_id: String,
    pub job_id: String,
    pub workspace: PathBuf,
    pub model: String,
    pub warning: Option<String>,
    /// This operation continues an existing member session.
    pub resumed: bool,
    /// This call either started the child or finished adopting one that a
    /// previous host started. A cached operation returns false.
    pub fresh: bool,
    /// Another host currently owns the spawn critical section. No process was
    /// started by this call and the next tick reconciles again.
    pub in_progress: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchKind {
    Initial,
    Resume,
}

impl LaunchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Resume => "resume",
        }
    }
}

/// One durable launch or resume intention. Its identity is scoped to the task,
/// member, specification and review round, and it reserves both the child
/// session and the background job UUID before any external side effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOperation {
    pub operation_id: String,
    pub task_id: String,
    pub role: String,
    pub kind: String,
    pub spec_version: u32,
    pub review_round: u32,
    /// Evidence identity within the spec/round: `implementation`, `changes`,
    /// `integration`, `final-delivery`, or empty for the initial launch. A
    /// final review of the same round must not reuse the implementation
    /// review's reservation or prompt.
    #[serde(default)]
    pub purpose: String,
    /// The complete repository delivery identity. It is the reserved-job
    /// metadata: it is durable before the background job id it carries is
    /// spawned. `None` for other launch kinds and for genuine legacy delivery
    /// records written before the typed identity existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryOperationIdentity>,
    pub workspace: PathBuf,
    pub session_id: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session: Option<String>,
    /// The exact instruction handed to the child. A later host reuses it
    /// instead of deriving a possibly different prompt.
    pub prompt: String,
    pub model: String,
    /// The permission mode reserved for this launch. A later tick must resume
    /// the same contract even if the Router's mode changed in the meantime.
    #[serde(default)]
    pub permission_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default)]
    pub allow_workspace_move: bool,
    /// `reserved` < `session` < `spawned` < `completed`.
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Set once the instruction is durably recorded as handed to a supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<LaunchRecord>,
}

#[derive(Clone)]
pub struct Launcher {
    pub context: ServiceContext,
    pub config: SuperAgentConfig,
    /// The Router's resolved provider and model, inherited by both members.
    pub provider: ResolvedProvider,
    pub permission_mode: String,
    pub executable_override: Option<PathBuf>,
    /// Test-only crash boundary. Hosts pass `None`.
    pub gate: Option<LaunchGate>,
}

struct LaunchDraft<'a> {
    task: &'a TaskRecord,
    member: MemberRole,
    kind: LaunchKind,
    /// Evidence identity within the spec/round; see `LaunchOperation::purpose`.
    purpose: &'a str,
    /// A typed repository delivery identity, when this is a delivery resume.
    delivery: Option<&'a DeliveryOperationIdentity>,
    workspace: PathBuf,
    resume_session: Option<String>,
    prompt: String,
    model: String,
    warning: Option<String>,
    allow_workspace_move: bool,
}

impl Launcher {
    pub fn store(&self) -> TaskStore {
        TaskStore::from_context(&self.context)
    }

    fn gate(&self, stage: &'static str) -> Result<()> {
        match &self.gate {
            Some(gate) => gate(stage),
            None => Ok(()),
        }
    }

    /// Coder's single writable directory: the prepared worktree for a single
    /// target, or the prepared multi-repository workspace.
    pub fn task_workspace(&self, task: &TaskRecord) -> Result<PathBuf> {
        let total = task.repositories.len() + task.non_git_targets.len();
        ensure!(total > 0, "Task execution environment is not prepared");
        let path = if total == 1 {
            task.repositories
                .first()
                .map(|repo| repo.working_path.clone())
                .or_else(|| {
                    task.non_git_targets
                        .first()
                        .map(|target| target.path.clone())
                })
                .context("Task target disappeared")?
        } else {
            self.context
                .paths
                .data_dir
                .join("super-agent")
                .join(&task.task_id)
                .join("workspace")
        };
        ensure!(
            path.is_dir(),
            "Task workspace is unavailable: {}",
            path.display()
        );
        Ok(path)
    }

    /// Launches the Coder in plan mode. Calling this again reconciles the same
    /// durable operation instead of creating a second session.
    pub async fn launch_coder(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        ensure!(
            matches!(task.phase, Phase::Planning | Phase::Discovering),
            "Coder launch expects a task in planning, found {}",
            task.phase.as_str()
        );
        let workspace = canonical(self.task_workspace(task)?)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Coder);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Coder,
            kind: LaunchKind::Initial,
            purpose: "",
            delivery: None,
            workspace,
            resume_session: None,
            prompt: coder_plan_prompt(task),
            model: model.id,
            warning,
            allow_workspace_move: false,
        })
        .await
    }

    /// Continues the same Coder session after the plan is approved.
    pub async fn resume_coder(
        &self,
        task: &TaskRecord,
        instruction: &str,
        purpose: &str,
    ) -> Result<LaunchOutcome> {
        ensure!(
            !is_delivery_purpose_namespace(purpose),
            "A delivery resume must use resume_coder_for_delivery with a validated repository identity, not purpose {purpose:?}"
        );
        let session = task
            .coder
            .session_id
            .clone()
            .context("Task has no Coder session to resume")?;
        let workspace = canonical(self.task_workspace(task)?)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Coder);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Coder,
            kind: LaunchKind::Resume,
            purpose,
            delivery: None,
            workspace,
            resume_session: Some(session),
            prompt: instruction.to_owned(),
            model: model.id,
            warning,
            allow_workspace_move: false,
        })
        .await
    }

    /// Watcher recovery keeps the exact member/workspace and uses the shared
    /// child resume path. Its purpose is the durable interruption identity.
    pub(crate) async fn resume_interrupted(
        &self,
        task: &TaskRecord,
        member: MemberRole,
        instruction: &str,
        operation_id: &str,
    ) -> Result<LaunchOutcome> {
        let session = member_session(task, member).context("Recovery member missing")?;
        let workspace = self
            .context
            .db
            .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, session))??
            .context("Recovery session missing")?
            .workspace_path
            .context("Recovery workspace missing")?;
        let role = match member {
            MemberRole::Coder => AgentModelRole::Coder,
            MemberRole::Reviewer => AgentModelRole::Reviewer,
        };
        let (model, warning) = self.resolve_model(role);
        self.run(LaunchDraft {
            task,
            member,
            kind: LaunchKind::Resume,
            purpose: operation_id,
            delivery: None,
            workspace: canonical(PathBuf::from(workspace))?,
            resume_session: Some(session.to_owned()),
            prompt: instruction.to_owned(),
            model: model.id,
            warning,
            allow_workspace_move: false,
        })
        .await
    }

    /// Continues the same Coder session for one repository delivery.
    ///
    /// The typed identity is the only way to address a delivery resume: the
    /// caller cannot supply a purpose that disagrees with the identity, and two
    /// repositories of one plan (or two attempts) receive distinct durable
    /// operations and reserved job IDs.
    pub async fn resume_coder_for_delivery(
        &self,
        task: &TaskRecord,
        instruction: &str,
        identity: &DeliveryOperationIdentity,
    ) -> Result<LaunchOutcome> {
        ensure!(
            identity.version == DELIVERY_IDENTITY_VERSION,
            "Delivery identity version {} is not supported",
            identity.version
        );
        ensure!(
            identity.task_id == task.task_id
                && identity.spec_version == task.spec_version
                && identity.review_round == task.review_round,
            "Delivery identity for task {} spec {} round {} does not match the current task {} spec {} round {}",
            identity.task_id,
            identity.spec_version,
            identity.review_round,
            task.task_id,
            task.spec_version,
            task.review_round
        );
        let session = task
            .coder
            .session_id
            .clone()
            .context("Task has no Coder session to resume")?;
        ensure!(
            identity.coder_session_id == session,
            "Delivery identity belongs to Coder session {}, not the task's {session}",
            identity.coder_session_id
        );
        // The identity must match the persisted approved plan and its target.
        // A caller cannot manufacture a repository, OID, attempt or plan that
        // the host never approved and thereby authorize a child.
        let store = self.store();
        let plans = store
            .delivery_plans(&task.task_id)
            .map_err(|error| anyhow!(error.to_string()))?;
        let plan = plans
            .iter()
            .find(|plan| plan.operation_id == identity.plan_operation_id)
            .with_context(|| {
                format!(
                    "Delivery identity references plan {}, which is not recorded for task {}",
                    identity.plan_operation_id, task.task_id
                )
            })?;
        let target = plan
            .repositories
            .iter()
            .find(|target| target.repository_id == identity.repository_id)
            .with_context(|| {
                format!(
                    "Delivery identity references repository {}, which is not part of the recorded plan {}",
                    identity.repository_id, plan.operation_id
                )
            })?;
        identity.validate_for(task, plan, target)?;
        let purpose = identity.purpose_key();
        ensure!(
            is_delivery_purpose(&purpose),
            "Derived delivery purpose is malformed"
        );
        let workspace = canonical(self.task_workspace(task)?)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Coder);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Coder,
            kind: LaunchKind::Resume,
            purpose: &purpose,
            delivery: Some(identity),
            workspace,
            resume_session: Some(session),
            prompt: instruction.to_owned(),
            model: model.id,
            warning,
            allow_workspace_move: false,
        })
        .await
    }

    /// Launches the Reviewer in its own fixed checkout of the submitted commit.
    /// It never sees the Coder's writable worktree.
    pub async fn launch_reviewer(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        // The initial Reviewer operation belongs to the round in which it was
        // reserved. A later review round must not create a second initial
        // operation, so an existing one is adopted regardless of the current
        // round.
        if let Some(operation) =
            self.find_operation(&task.task_id, MemberRole::Reviewer, LaunchKind::Initial)?
        {
            let draft = LaunchDraft {
                task,
                member: MemberRole::Reviewer,
                kind: LaunchKind::Initial,
                purpose: "",
                delivery: None,
                workspace: operation.workspace.clone(),
                resume_session: None,
                prompt: operation.prompt.clone(),
                model: operation.model.clone(),
                warning: operation.warning.clone(),
                allow_workspace_move: false,
            };
            if operation.handle.is_some() {
                return self.cached_outcome(task, MemberRole::Reviewer, operation);
            }
            return self.proceed(&draft, operation).await;
        }
        let workspace = canonical(self.prepare_review_workspace(task)?)?;
        let request = crate::super_agent::review::build_review_request(&self.store(), task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Reviewer);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Reviewer,
            kind: LaunchKind::Initial,
            purpose: "",
            delivery: None,
            workspace,
            resume_session: None,
            prompt: reviewer_prompt(task, &request),
            model: model.id,
            warning,
            allow_workspace_move: false,
        })
        .await
    }

    /// Continues the same Reviewer for a new review round after the Coder
    /// resubmitted a corrected implementation.
    pub async fn resume_reviewer(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        let session = task
            .reviewer
            .session_id
            .clone()
            .context("Task has no Reviewer session to resume")?;
        let key = operation_key(
            task,
            MemberRole::Reviewer,
            LaunchKind::Resume,
            "implementation",
        );
        if let Some(operation) = self.load_operation(&key)? {
            if operation.handle.is_some() {
                return self.cached_outcome(task, MemberRole::Reviewer, operation);
            }
        }
        let workspace = canonical(self.prepare_review_workspace(task)?)?;
        let request = crate::super_agent::review::build_review_request(&self.store(), task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Reviewer);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Reviewer,
            kind: LaunchKind::Resume,
            purpose: "implementation",
            delivery: None,
            workspace,
            resume_session: Some(session),
            prompt: reviewer_prompt(task, &request),
            model: model.id,
            warning,
            allow_workspace_move: true,
        })
        .await
    }

    /// Continues the SAME Reviewer session for the final-delivery review of the
    /// host-verified integrated content. It uses its own durable operation
    /// identity so a previous implementation review of the same round is never
    /// reused, and it never creates a replacement Reviewer.
    pub async fn resume_final_reviewer(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        let session = task
            .reviewer
            .session_id
            .clone()
            .context("Task has no Reviewer session to resume")?;
        let key = operation_key(
            task,
            MemberRole::Reviewer,
            LaunchKind::Resume,
            "final-delivery",
        );
        if let Some(operation) = self.load_operation(&key)? {
            if operation.handle.is_some() {
                return self.cached_outcome(task, MemberRole::Reviewer, operation);
            }
        }
        let workspace = canonical(self.prepare_final_review_workspace(task)?)?;
        let request = crate::super_agent::review::build_final_review_request(&self.store(), task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Reviewer);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Reviewer,
            kind: LaunchKind::Resume,
            purpose: "final-delivery",
            delivery: None,
            workspace,
            resume_session: Some(session),
            prompt: reviewer_final_prompt(task, &request),
            model: model.id,
            warning,
            allow_workspace_move: true,
        })
        .await
    }

    /// Resumes the SAME Reviewer once for the post-delivery completion review
    /// of a task whose integration reused the implementation approval. The
    /// purpose is distinct from the pre-delivery final review, so the two
    /// stages can never share an operation or a job.
    pub async fn resume_finalizing_reviewer(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        let session = task
            .reviewer
            .session_id
            .clone()
            .context("Task has no Reviewer session to resume")?;
        let key = operation_key(
            task,
            MemberRole::Reviewer,
            LaunchKind::Resume,
            "final-completion",
        );
        if let Some(operation) = self.load_operation(&key)? {
            if operation.handle.is_some() {
                return self.cached_outcome(task, MemberRole::Reviewer, operation);
            }
        }
        let workspace = canonical(self.prepare_final_review_workspace(task)?)?;
        let request = crate::super_agent::review::build_final_review_request(&self.store(), task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Reviewer);
        self.run(LaunchDraft {
            task,
            member: MemberRole::Reviewer,
            kind: LaunchKind::Resume,
            purpose: "final-completion",
            delivery: None,
            workspace,
            resume_session: Some(session),
            prompt: reviewer_finalizing_prompt(task, &request),
            model: model.id,
            warning,
            allow_workspace_move: true,
        })
        .await
    }

    /// Reads the typed delivery identity stored on one actual job. A missing
    /// job directory is `None`; an unreadable file is an error so callers block
    /// instead of treating damage as absence.
    pub fn delivery_job_binding(&self, job_id: &str) -> Result<Option<DeliveryJobBinding>> {
        let directory = background_sessions::directory(&self.context.paths.data_dir, job_id)?;
        let path = directory.join(DELIVERY_BINDING_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow!(
                    "Reading delivery identity {}: {error}",
                    path.display()
                ))
            }
        };
        let binding = serde_json::from_slice::<DeliveryJobBinding>(&bytes).with_context(|| {
            format!(
                "Parsing delivery identity {}; the original bytes were preserved",
                path.display()
            )
        })?;
        Ok(Some(binding))
    }

    /// Verifies that an actual job carries the typed identity its operation
    /// reserved. A typed record without its durable job binding is incomplete
    /// and blocks; a legacy record must not silently gain a typed binding.
    pub fn delivery_binding_mismatch(
        &self,
        operation: &LaunchOperation,
        job_id: &str,
    ) -> Result<Option<String>> {
        let binding = self.delivery_job_binding(job_id).map_err(|error| {
            anyhow!(
                "The delivery identity of background task {job_id} cannot be read ({error:#}); the original files were preserved"
            )
        })?;
        match (operation.delivery.as_ref(), binding) {
            (Some(identity), Some(binding)) => {
                if binding.version != DELIVERY_IDENTITY_VERSION
                    || binding.job_id != job_id
                    || binding.session_id != operation.session_id
                    || &binding.identity != identity
                {
                    return Ok(Some(format!(
                        "Background task {job_id} carries delivery identity job {} session {} repository {} plan {} old/new {}/{} attempt {}, not the reserved repository {} plan {} old/new {}/{} attempt {}; refusing to adopt it",
                        binding.job_id,
                        binding.session_id,
                        binding.identity.repository_id,
                        binding.identity.plan_operation_id,
                        binding.identity.old_oid,
                        binding.identity.new_oid,
                        binding.identity.attempt,
                        identity.repository_id,
                        identity.plan_operation_id,
                        identity.old_oid,
                        identity.new_oid,
                        identity.attempt
                    )));
                }
                Ok(None)
            }
            (Some(_), None) => Ok(Some(format!(
                "Background task {job_id} is reserved by a typed delivery operation but carries no durable delivery identity; refusing to adopt it without proof"
            ))),
            (None, Some(binding)) => Ok(Some(format!(
                "Background task {job_id} carries typed delivery identity for repository {} but the launch operation has none; refusing to mix identities",
                binding.identity.repository_id
            ))),
            (None, None) => Ok(None),
        }
    }

    /// Writes the typed identity next to the reserved job before the supervisor
    /// starts. Replaying the same identity is idempotent; a different one is a
    /// contradiction and never overwrites the existing file.
    fn write_delivery_job_binding(
        &self,
        operation: &LaunchOperation,
        identity: &DeliveryOperationIdentity,
    ) -> Result<()> {
        let directory =
            background_sessions::directory(&self.context.paths.data_dir, &operation.job_id)?;
        std::fs::create_dir_all(&directory)?;
        let binding = DeliveryJobBinding {
            version: DELIVERY_IDENTITY_VERSION,
            job_id: operation.job_id.clone(),
            session_id: operation.session_id.clone(),
            identity: identity.clone(),
        };
        let path = directory.join(DELIVERY_BINDING_FILE);
        if let Some(existing) = self.delivery_job_binding(&operation.job_id)? {
            if existing == binding {
                return Ok(());
            }
            return Err(anyhow!(
                "Background task {} already carries a different delivery identity; the original file was preserved",
                operation.job_id
            ));
        }
        let bytes = serde_json::to_vec(&binding)?;
        // Publish without clobbering: write our own staging file, then create
        // the destination link atomically. If another host won the race, the
        // destination is never overwritten; compare after losing.
        let temporary = directory.join(format!(
            "{DELIVERY_BINDING_FILE}.tmp-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&temporary, &bytes)?;
        let published = std::fs::hard_link(&temporary, &path);
        // Only our own staging file is removed; the published file stays.
        let _ = std::fs::remove_file(&temporary);
        match published {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = self
                    .delivery_job_binding(&operation.job_id)?
                    .context("The concurrently published delivery identity disappeared")?;
                if existing == binding {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Background task {} already carries a different delivery identity; the original file was preserved",
                        operation.job_id
                    ))
                }
            }
            Err(error) => Err(anyhow!(
                "Publishing delivery identity {}: {error}",
                path.display()
            )),
        }
    }

    fn resolve_model(
        &self,
        role: AgentModelRole,
    ) -> (
        crate::native_engine::provider_manager::ModelConfig,
        Option<String>,
    ) {
        let (model, fallback): (_, Option<ModelFallback>) =
            self.config
                .resolve_agent_model(role, &self.provider.model, &self.provider.provider);
        (model, fallback.map(|warning| warning.message()))
    }

    /// Durable stage of an initial or resume operation, for host gates and
    /// diagnostics. A completed operation is never started again.
    pub fn load_operation(&self, key: &str) -> Result<Option<LaunchOperation>> {
        self.context.setting(key).map_err(Into::into)
    }

    pub fn operation_key(&self, task: &TaskRecord, member: MemberRole, kind: LaunchKind) -> String {
        operation_key(task, member, kind, "")
    }

    /// Stage/purpose-qualified identity for a resume within one spec/round.
    pub fn operation_key_for(
        &self,
        task: &TaskRecord,
        member: MemberRole,
        kind: LaunchKind,
        purpose: &str,
    ) -> String {
        operation_key(task, member, kind, purpose)
    }

    /// Finds the most relevant operation of a kind for this task/member,
    /// preferring an incomplete one so recovery is not hidden by a later
    /// completed operation.
    pub fn find_operation(
        &self,
        task_id: &str,
        member: MemberRole,
        kind: LaunchKind,
    ) -> Result<Option<LaunchOperation>> {
        let prefix = format!(
            "super-agent.launch-op.{task_id}.{}.{}.%",
            member.as_str(),
            kind.as_str()
        );
        let rows: Vec<String> = self.context.db.with_conn(|conn| -> Result<Vec<String>> {
            let mut statement = conn.prepare(
                "SELECT value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key DESC",
            )?;
            let rows = statement.query_map([prefix], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })??;
        let mut operations = rows
            .into_iter()
            .filter_map(|value| serde_json::from_str::<LaunchOperation>(&value).ok())
            .collect::<Vec<_>>();
        operations.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        if let Some(incomplete) = operations
            .iter()
            .find(|operation| operation.handle.is_none())
        {
            return Ok(Some(incomplete.clone()));
        }
        Ok(operations.into_iter().next())
    }

    /// Reads every durable reference that names a Coder or Reviewer child of
    /// this task and inspects the exact reserved job directly.
    ///
    /// This is completion evidence, not discovery: a reference whose job state
    /// is missing, unreadable, foreign or unrecognized is reported as
    /// `Unknown` so the caller blocks instead of concluding that the process
    /// stopped. The required current-stage Reviewer receipt is derived from the
    /// persisted delivery plan's decision and must be covered by a receipt for
    /// this exact spec/round; historical receipts never satisfy it. Discovery
    /// list semantics are deliberately unchanged.
    pub fn completion_member_jobs(
        &self,
        task: &TaskRecord,
        plan: &crate::super_agent::delivery::DeliveryPlan,
        required_reviewer_purpose: &str,
    ) -> Result<CompletionMemberJobs> {
        let coder = task
            .coder
            .session_id
            .clone()
            .filter(|session| !session.trim().is_empty());
        let reviewer = task
            .reviewer
            .session_id
            .clone()
            .filter(|session| !session.trim().is_empty());
        let router = task.router_session_id.as_str();
        let mut result = CompletionMemberJobs::default();

        for (key, value) in
            self.setting_rows(&format!("super-agent.launch-op.{}.", task.task_id))?
        {
            let source = format!("launch operation {key}");
            let operation: LaunchOperation = match serde_json::from_str(&value) {
                Ok(operation) => operation,
                Err(error) => {
                    result.observations.push(unknown_receipt(
                        source,
                        "",
                        "",
                        format!(
                            "the launch operation cannot be decoded ({error}); the original value was preserved"
                        ),
                    ));
                    continue;
                }
            };
            match operation_receipt_session(
                task,
                coder.as_deref(),
                reviewer.as_deref(),
                &key,
                &operation,
            ) {
                Ok(expected_session) => {
                    result.observations.push(
                        self.observe_member_job(
                            router,
                            MemberJobReceipt::new(source, operation.role.clone(), expected_session)
                                .with_job(operation.job_id.clone())
                                .with_stage(
                                    operation.purpose.clone(),
                                    operation.spec_version,
                                    operation.review_round,
                                ),
                        ),
                    );
                }
                Err(reason) => result.observations.push(unknown_job_receipt(
                    source,
                    operation.role.clone(),
                    operation.session_id.clone(),
                    operation.job_id.clone(),
                    reason,
                )),
            }
        }

        for (key, value) in
            self.setting_rows(&format!("super-agent.reviewer-resume.{}.", task.task_id))?
        {
            let source = format!("reviewer resume marker {key}");
            let marker: serde_json::Value = match serde_json::from_str(&value) {
                Ok(marker) => marker,
                Err(error) => {
                    result.observations.push(unknown_receipt(
                        source,
                        "reviewer",
                        "",
                        format!(
                            "the resume marker cannot be decoded ({error}); the original value was preserved"
                        ),
                    ));
                    continue;
                }
            };
            let session = marker["session_id"]
                .as_str()
                .filter(|session| !session.trim().is_empty());
            let job_id = marker["job_id"]
                .as_str()
                .filter(|job| !job.trim().is_empty());
            let (Some(session), Some(job_id)) = (session, job_id) else {
                result.observations.push(unknown_receipt(
                    source,
                    "reviewer",
                    session.unwrap_or(""),
                    "the resume marker does not name both its session and its reserved job",
                ));
                continue;
            };
            if reviewer.as_deref() != Some(session) {
                result.observations.push(unknown_job_receipt(
                    source,
                    "reviewer",
                    session,
                    job_id,
                    format!(
                        "the marker names session {session}, not this task's Reviewer {:?}",
                        reviewer
                    ),
                ));
                continue;
            }
            let stage = match reviewer_marker_identity(
                task,
                reviewer.as_deref(),
                &key,
                &marker,
                session,
                job_id,
            ) {
                Ok(stage) => stage,
                Err(reason) => {
                    result.observations.push(unknown_job_receipt(
                        source, "reviewer", session, job_id, reason,
                    ));
                    continue;
                }
            };
            result.observations.push(
                self.observe_member_job(
                    router,
                    MemberJobReceipt::new(source, "reviewer", session)
                        .with_job(job_id)
                        .with_stage(stage.0, stage.1, stage.2)
                        .with_plan(stage.3),
                ),
            );
        }

        // Every planned repository of the authoritative plan must be covered by
        // a Coder delivery receipt that names *this* plan. Scanning whatever
        // historical rows happen to exist is not proof that the current
        // delivery child stopped: deleting the current receipt must block.
        let required_plan_operation = plan.operation_id.clone();
        let mut covered_coder_targets = std::collections::BTreeSet::new();
        for dispatch in self
            .store()
            .delivery_dispatches(&task.task_id)
            .map_err(|error| anyhow!(error.to_string()))?
        {
            let repository = dispatch
                .repository_id
                .clone()
                .unwrap_or_else(|| "legacy single-repository receipt".into());
            let source = format!(
                "delivery receipt {} ({repository})",
                dispatch.plan_operation_id
            );
            let Some(coder_session) = coder.as_deref() else {
                result.observations.push(unknown_receipt(
                    source,
                    "coder",
                    &dispatch.coder_session_id,
                    "the task has no Coder session for its delivery receipt",
                ));
                continue;
            };
            if dispatch.coder_session_id != coder_session {
                result.observations.push(unknown_receipt(
                    source,
                    "coder",
                    &dispatch.coder_session_id,
                    format!(
                        "the receipt belongs to Coder session {}, not {coder_session}",
                        dispatch.coder_session_id
                    ),
                ));
                continue;
            }
            if !is_delivery_purpose(&dispatch.purpose) {
                result.observations.push(unknown_receipt(
                    source,
                    "coder",
                    coder_session,
                    format!(
                        "the receipt purpose {:?} is not a delivery identity",
                        dispatch.purpose
                    ),
                ));
                continue;
            }
            let Some(job_id) = dispatch
                .job_id
                .clone()
                .filter(|job_id| !job_id.trim().is_empty())
            else {
                result.observations.push(unknown_receipt(
                    source,
                    "coder",
                    coder_session,
                    "the delivery receipt names no reserved job; absence is not proof that its Coder stopped",
                ));
                continue;
            };
            let plan = match self
                .store()
                .delivery_plan(&task.task_id, &dispatch.plan_operation_id)
            {
                Ok(Some(plan)) => plan,
                Ok(None) => {
                    result.observations.push(unknown_receipt(
                        source,
                        "coder",
                        coder_session,
                        format!(
                            "the delivery receipt names plan {}, which is not persisted for this task",
                            dispatch.plan_operation_id
                        ),
                    ));
                    continue;
                }
                Err(error) => {
                    result.observations.push(unknown_receipt(
                        source,
                        "coder",
                        coder_session,
                        format!(
                            "the persisted plan {} could not be read ({error})",
                            dispatch.plan_operation_id
                        ),
                    ));
                    continue;
                }
            };
            if let Err(reason) = dispatch_identity(task, coder_session, &dispatch, &plan) {
                result
                    .observations
                    .push(unknown_receipt(source, "coder", coder_session, reason));
                continue;
            }
            if dispatch.plan_operation_id == required_plan_operation {
                match dispatch.repository_id.as_deref() {
                    Some(repository_id) => {
                        covered_coder_targets.insert(repository_id.to_owned());
                    }
                    // A genuine legacy receipt is the sole identity for the one
                    // repository of a single-repository plan.
                    None if plan.repositories.len() == 1 => {
                        covered_coder_targets.insert(plan.repositories[0].repository_id.clone());
                    }
                    None => {}
                }
            }
            result.observations.push(
                self.observe_member_job(
                    router,
                    MemberJobReceipt::new(source, "coder", coder_session)
                        .with_job(job_id)
                        .with_stage(
                            dispatch.purpose.clone(),
                            dispatch.spec_version,
                            dispatch.review_round,
                        ),
                ),
            );
        }

        // A repository the host found already carrying the approved commit was
        // observed without a delivery child, so it must not be asked for a
        // Coder receipt. That fact is only accepted from the durable
        // observation's own provenance: a missing receipt or equal OIDs are
        // not proof that no child was ever dispatched.
        for target in &plan.repositories {
            if covered_coder_targets.contains(&target.repository_id) {
                continue;
            }
            let observed_without_dispatch = self
                .store()
                .repository_delivery_observation(
                    &task.task_id,
                    &required_plan_operation,
                    &target.repository_id,
                )
                .map_err(|error| anyhow!(error.to_string()))?
                .is_some_and(|observation| {
                    observation.verified
                        && observation.source
                            == crate::super_agent::delivery::DeliveryObservationSource::HostVerifiedAlreadyAtTarget
                        && observation.spec_version == plan.spec_version
                        && observation.review_round == plan.review_round
                        && observation.old_oid == target.old_oid
                        && observation.new_oid == target.new_oid
                        && observation.attempt == target.attempt
                });
            if observed_without_dispatch {
                continue;
            }
            result.observations.push(unknown_receipt(
                format!(
                    "required completion delivery receipt ({})",
                    target.repository_id
                ),
                "coder",
                coder.as_deref().unwrap_or_default(),
                format!(
                    "no durable Coder delivery receipt and no host-verified no-dispatch observation exist for plan {} repository {} attempt {}; a historical receipt, a missing one, or an unproven observation cannot show whether this delivery child stopped",
                    required_plan_operation, target.repository_id, target.attempt
                ),
            ));
        }

        // The completion review is not interchangeable with any historical or
        // different-stage Reviewer receipt. Its expected stage is derived from
        // the persisted delivery plan's decision. If no current receipt covers
        // it, absence becomes an explicit Unknown instead of leaving older
        // terminal receipts to stand in for the job that must be proven.
        let covered = result.observations.iter().any(|observation| {
            observation.receipt.role == "reviewer"
                && observation.receipt.purpose == required_reviewer_purpose
                && observation.receipt.spec_version == task.spec_version
                && observation.receipt.review_round == task.review_round
                && observation.receipt.session_id == reviewer.as_deref().unwrap_or_default()
                // The stage key is shared by every plan of this spec/round, so
                // the post-delivery review must name the plan it reviewed.
                && (required_reviewer_purpose != "final-completion"
                    || observation.receipt.plan_operation_id == required_plan_operation)
        });
        if !covered {
            result.observations.push(MemberJobObservation {
                receipt: MemberJobReceipt::new(
                    "required completion review receipt",
                    "reviewer",
                    reviewer.clone().unwrap_or_default(),
                )
                .with_stage(
                    required_reviewer_purpose,
                    task.spec_version,
                    task.review_round,
                )
                .with_plan(required_plan_operation.clone()),
                state: MemberJobState::Unknown {
                    reason: format!(
                        "no durable {required_reviewer_purpose} completion-review receipt exists for spec {} round {} plan {}; a historical, different-stage or different-plan receipt cannot prove the current Reviewer job",
                        task.spec_version, task.review_round, required_plan_operation
                    ),
                },
            });
        }
        // The required receipt must belong to the delivery plan actually used
        // for completion, so a narrowed or replaced plan cannot reuse a review
        // receipt from a different delivery.
        if plan.task_id != task.task_id {
            result.observations.push(unknown_receipt(
                "required completion review receipt".into(),
                "reviewer",
                reviewer.clone().unwrap_or_default(),
                format!(
                    "the persisted plan {} belongs to task {}, not {}",
                    plan.operation_id, plan.task_id, task.task_id
                ),
            ));
        }

        Ok(result)
    }

    fn setting_rows(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let pattern = format!("{prefix}%");
        let rows = self
            .context
            .db
            .with_conn(|conn| -> Result<Vec<(String, String)>> {
                let mut statement = conn.prepare(
                    "SELECT key,value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key",
                )?;
                let rows = statement.query_map([pattern], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })??;
        Ok(rows)
    }

    fn observe_member_job(&self, router: &str, receipt: MemberJobReceipt) -> MemberJobObservation {
        let state =
            match background_sessions::inspect(&self.context.paths.data_dir, &receipt.job_id) {
                Ok(JobSnapshot::Present(job)) => {
                    if job.id != receipt.job_id {
                        MemberJobState::Unknown {
                            reason: format!(
                                "the job directory holds identity {}, not {}",
                                job.id, receipt.job_id
                            ),
                        }
                    } else if job.session_id != receipt.session_id {
                        MemberJobState::Unknown {
                            reason: format!(
                                "the job belongs to child session {}, not {}",
                                job.session_id, receipt.session_id
                            ),
                        }
                    } else if job.parent_session_id != router {
                        MemberJobState::Unknown {
                            reason: format!(
                            "the job belongs to parent session {}, not this task's Router {router}",
                            job.parent_session_id
                        ),
                        }
                    } else {
                        match job.status.as_str() {
                            "starting" | "running" | "unresponsive" => {
                                MemberJobState::Active { status: job.status }
                            }
                            "completed" | "failed" | "cancelled" => {
                                MemberJobState::Stopped { status: job.status }
                            }
                            other => MemberJobState::Unknown {
                                reason: format!("the job has an unrecognized status {other:?}"),
                            },
                        }
                    }
                }
                Ok(JobSnapshot::Missing) => MemberJobState::Unknown {
                    reason: "no durable task state exists; remove is not proof the process stopped"
                        .into(),
                },
                Ok(JobSnapshot::Unreadable(reason)) => MemberJobState::Unknown {
                    reason: format!(
                        "the job state is unreadable ({reason}); the original files were preserved"
                    ),
                },
                Err(error) => MemberJobState::Unknown {
                    reason: format!("the job state could not be read ({error:#})"),
                },
            };
        MemberJobObservation { receipt, state }
    }

    async fn run(&self, draft: LaunchDraft<'_>) -> Result<LaunchOutcome> {
        self.gate("before-reservation")?;
        let key = operation_key(draft.task, draft.member, draft.kind, draft.purpose);
        let operation = self.reserve_operation(&key, self.new_operation(&draft))?;
        self.verify(&operation, &draft)?;
        self.proceed(&draft, operation).await
    }

    /// Reconciles or starts an already reserved operation. The operation record
    /// is authoritative; callers use this for an initial Reviewer launch whose
    /// reservation predates the current review round.
    async fn proceed(
        &self,
        draft: &LaunchDraft<'_>,
        operation: LaunchOperation,
    ) -> Result<LaunchOutcome> {
        // A task member slot is authoritative. Never attach a second session to
        // a slot that a previous operation already bound.
        if let Some(bound) = member_session(draft.task, draft.member) {
            let expected = operation
                .resume_session
                .as_deref()
                .unwrap_or(&operation.session_id);
            ensure!(
                bound == expected,
                "Task {} already uses {} session {bound}; refusing to start another one ({expected})",
                draft.task.task_id,
                draft.member.as_str()
            );
        }
        if operation.handle.is_some() {
            return self.cached_outcome(draft.task, draft.member, operation);
        }
        self.gate("after-reservation")?;

        match background_sessions::inspect(&self.context.paths.data_dir, &operation.job_id)? {
            JobSnapshot::Unreadable(reason) => {
                let error = anyhow!(
                    "Reserved background task {} for task {} has unreadable state ({}); the original files were preserved. Inspect it before retrying.",
                    operation.job_id,
                    draft.task.task_id,
                    reason
                );
                self.record_error(&key_of(&operation), &error)?;
                Err(error)
            }
            JobSnapshot::Present(job) => self.reconcile_present(draft, operation, job).await,
            JobSnapshot::Missing => self.start_reserved(draft, operation).await,
        }
    }

    /// A job record already exists for this exact operation. Adopt a child that
    /// started, or continue the reservation when no process was ever recorded.
    async fn reconcile_present(
        &self,
        draft: &LaunchDraft<'_>,
        operation: LaunchOperation,
        job: background_sessions::Job,
    ) -> Result<LaunchOutcome> {
        ensure!(
            job.parent_session_id == draft.task.router_session_id,
            "Background task {} belongs to another Router session",
            job.id
        );
        ensure!(
            job.session_id == operation.session_id,
            "Background task {} belongs to another child session",
            job.id
        );
        if let Some(launch) = background_sessions::launch_of(&self.context.paths.data_dir, &job.id)?
        {
            ensure!(
                same_path(&launch.cwd, &operation.workspace),
                "Background task {} has a different workspace than the reserved operation",
                job.id
            );
            ensure!(
                launch.arguments.last().map(String::as_str) == Some(operation.prompt.as_str()),
                "Background task {} carries a different instruction than the reserved operation",
                job.id
            );
        }
        // The job's own durable files must carry the identity this operation
        // reserved, not merely a matching Coder, workspace or instruction.
        if let Some(reason) = self.delivery_binding_mismatch(&operation, &job.id)? {
            self.record_error(&key_of(&operation), &anyhow!("{reason}"))?;
            return Err(anyhow!("{reason}"));
        }
        let progressed = job.launched
            || job.supervisor_pid.is_some()
            || job.child_pid.is_some()
            || matches!(job.status.as_str(), "running" | "completed" | "cancelled");
        if !progressed {
            // The reservation reached the filesystem but no supervisor was ever
            // recorded, or the spawn failed before a process existed. Continuing
            // the same operation is safe: the reserved job identity, the spawn
            // lock and the worker claim prevent a duplicate execution.
            return self.start_reserved(draft, operation).await;
        }

        // Adopt the child a previous host started, then finish recording it.
        self.adopt_session(draft, &operation, &job)?;
        let key = key_of(&operation);
        let now = chrono::Utc::now().to_rfc3339();
        let record = LaunchRecord {
            role: draft.member.as_str().into(),
            session_id: operation.session_id.clone(),
            job_id: job.id.clone(),
            workspace: operation.workspace.clone(),
            model: operation.model.clone(),
            delivery: operation.delivery.clone(),
            warning: operation.warning.clone(),
            created_at: now.clone(),
        };
        self.update_operation(&key, |stored| {
            stored.handle = Some(record.clone());
            stored.state = "completed".into();
            stored.last_job_status = Some(job.status.clone());
            stored.consumed_at = Some(now.clone());
            stored.last_error = None;
        })?;
        self.save_record(&draft.task.task_id, &record)?;
        Ok(outcome_from_record(&operation, record, true, false))
    }

    async fn start_reserved(
        &self,
        draft: &LaunchDraft<'_>,
        operation: LaunchOperation,
    ) -> Result<LaunchOutcome> {
        let key = key_of(&operation);
        let prepared = self.prepare(draft, &operation).await?;
        self.mark_state(&key, "session")?;
        self.gate("after-session")?;
        // The typed identity is durable on the reserved job before any process
        // starts, so every later adoption can prove which delivery owns it.
        if let Some(identity) = operation.delivery.as_ref() {
            self.write_delivery_job_binding(&operation, identity)?;
        }
        let handle = match launch_prepared_child(prepared) {
            Ok(handle) => handle,
            Err(error) if background_sessions::is_launch_in_progress(&error) => {
                return Ok(LaunchOutcome {
                    session_id: operation.session_id.clone(),
                    job_id: operation.job_id.clone(),
                    workspace: operation.workspace.clone(),
                    model: operation.model.clone(),
                    warning: operation.warning.clone(),
                    resumed: operation.kind == LaunchKind::Resume.as_str(),
                    fresh: false,
                    in_progress: true,
                });
            }
            Err(error) => {
                self.record_error(&key, &error)?;
                return Err(error);
            }
        };
        self.gate("after-spawn")?;
        let now = chrono::Utc::now().to_rfc3339();
        let record = LaunchRecord {
            role: draft.member.as_str().into(),
            session_id: handle.session_id.clone(),
            job_id: handle.job_id.clone(),
            workspace: handle.workspace.clone(),
            model: operation.model.clone(),
            delivery: operation.delivery.clone(),
            warning: operation.warning.clone(),
            created_at: now.clone(),
        };
        // The handle is durable before the terminal marker, so a crash here is
        // recovered by adopting the recorded job identity.
        self.update_operation(&key, |stored| {
            stored.handle = Some(record.clone());
            stored.state = "spawned".into();
            stored.last_job_status = Some(handle.status.clone());
            stored.consumed_at = Some(now.clone());
            stored.last_error = None;
        })?;
        self.gate("after-record")?;
        self.update_operation(&key, |stored| stored.state = "completed".into())?;
        self.save_record(&draft.task.task_id, &record)?;
        self.gate("after-completed")?;
        Ok(LaunchOutcome {
            session_id: handle.session_id,
            job_id: handle.job_id,
            workspace: handle.workspace,
            model: operation.model,
            warning: operation.warning,
            resumed: operation.kind == LaunchKind::Resume.as_str(),
            fresh: true,
            in_progress: false,
        })
    }

    fn cached_outcome(
        &self,
        task: &TaskRecord,
        member: MemberRole,
        mut operation: LaunchOperation,
    ) -> Result<LaunchOutcome> {
        if let Some(bound) = member_session(task, member) {
            let expected = operation
                .resume_session
                .as_deref()
                .unwrap_or(&operation.session_id);
            ensure!(
                bound == expected,
                "Task {} no longer uses session {expected} for its {}; refusing to reuse the launch record",
                task.task_id,
                member.as_str()
            );
        }
        // Read the actual job, but persist nothing until every immutable fact
        // is proven. A cached return is an adoption, not a read-only status
        // probe: a contradictory record must stay exactly as recorded.
        let snapshot =
            background_sessions::inspect(&self.context.paths.data_dir, &operation.job_id)?;
        let job = match snapshot {
            JobSnapshot::Present(job) => Some(job),
            JobSnapshot::Unreadable(reason) if operation.delivery.is_some() => {
                return Err(anyhow!(
                    "Background task {} has unreadable state ({reason}); the original files were preserved",
                    operation.job_id
                ))
            }
            _ => None,
        };
        if operation.delivery.is_some() {
            // The sidecar is self-consistency only. The actual background task
            // must still be the one this operation reserved for this Coder
            // under this Router.
            let job = job.as_ref().with_context(|| {
                format!(
                    "Typed delivery operation {} has no background task {}; refusing to reuse its recorded handle",
                    operation.operation_id, operation.job_id
                )
            })?;
            ensure!(
                job.id == operation.job_id,
                "Background task {} is not the reserved delivery job {}",
                job.id,
                operation.job_id
            );
            ensure!(
                job.session_id == operation.session_id,
                "Background task {} belongs to another child session than the reserved {}",
                job.id,
                operation.session_id
            );
            ensure!(
                job.parent_session_id == task.router_session_id,
                "Background task {} belongs to another Router session than {}",
                job.id,
                task.router_session_id
            );
        }
        let record = operation
            .handle
            .clone()
            .context("Completed launch operation has no recorded handle")?;
        // The handle must describe this exact operation; a handle copied from
        // another repository, session, job or workspace is never returned.
        ensure!(
            record.role == operation.role
                && record.session_id == operation.session_id
                && record.job_id == operation.job_id
                && record.delivery == operation.delivery
                && same_path(&record.workspace, &operation.workspace),
            "The recorded {} handle for job {} session {} identity {:?} does not match the reserved operation job {} session {} identity {:?}; refusing to reuse it",
            record.role,
            record.job_id,
            record.session_id,
            record.delivery,
            operation.job_id,
            operation.session_id,
            operation.delivery
        );
        if let Some(reason) = self.delivery_binding_mismatch(&operation, &operation.job_id)? {
            return Err(anyhow!("{reason}"));
        }
        // Only now may truthful observation be persisted.
        let status = job.as_ref().map(|job| job.status.clone());
        if status != operation.last_job_status {
            operation = self.update_operation(&key_of(&operation), |stored| {
                stored.last_job_status = status;
            })?;
        }
        if operation.state != "completed" {
            // A crash between recording the handle and the terminal marker is
            // finished here; the child itself is never restarted.
            operation = self.update_operation(&key_of(&operation), |stored| {
                stored.state = "completed".into();
            })?;
        }
        Ok(outcome_from_record(&operation, record, false, false))
    }

    fn new_operation(&self, draft: &LaunchDraft<'_>) -> LaunchOperation {
        let now = chrono::Utc::now().to_rfc3339();
        LaunchOperation {
            // A typed delivery operation's ID is its full unique storage key,
            // so two repositories of one plan never share an operation identity
            // even when only the opaque purpose differs. Other launch kinds
            // keep their historical ID exactly.
            operation_id: if draft.delivery.is_some() {
                operation_key(draft.task, draft.member, draft.kind, draft.purpose)
            } else {
                format!(
                    "{}:{}:{}",
                    draft.task.task_id,
                    draft.member.as_str(),
                    draft.kind.as_str()
                )
            },
            task_id: draft.task.task_id.clone(),
            role: draft.member.as_str().into(),
            kind: draft.kind.as_str().into(),
            spec_version: draft.task.spec_version,
            review_round: draft.task.review_round,
            purpose: draft.purpose.to_owned(),
            delivery: draft.delivery.cloned(),
            workspace: draft.workspace.clone(),
            // A resume continues an existing member; its durable identity is
            // that session. Only an initial launch reserves a new one.
            session_id: draft
                .resume_session
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            job_id: uuid::Uuid::new_v4().to_string(),
            resume_session: draft.resume_session.clone(),
            prompt: draft.prompt.clone(),
            model: draft.model.clone(),
            permission_mode: self.permission_mode.clone(),
            warning: draft.warning.clone(),
            allow_workspace_move: draft.allow_workspace_move,
            state: "reserved".into(),
            created_at: now.clone(),
            updated_at: now,
            last_job_status: None,
            last_error: None,
            consumed_at: None,
            handle: None,
        }
    }

    fn verify(&self, operation: &LaunchOperation, draft: &LaunchDraft<'_>) -> Result<()> {
        ensure!(
            operation.task_id == draft.task.task_id
                && operation.role == draft.member.as_str()
                && operation.kind == draft.kind.as_str()
                && operation.spec_version == draft.task.spec_version
                && operation.review_round == draft.task.review_round
                && operation.purpose == draft.purpose,
            "Reserved launch operation {} does not match this task, member, specification, round and evidence stage",
            operation.operation_id
        );
        ensure!(
            operation.delivery.as_ref() == draft.delivery,
            "Reserved launch operation {} belongs to a different delivery identity; refusing to reuse it",
            operation.operation_id
        );
        if let Some(identity) = draft.delivery {
            ensure!(
                operation.purpose == identity.purpose_key(),
                "Reserved launch operation {} is not keyed by its delivery identity",
                operation.operation_id
            );
        }
        ensure!(
            same_path(&operation.workspace, &draft.workspace),
            "Task workspace changed from {} to {}; start a new operation",
            operation.workspace.display(),
            draft.workspace.display()
        );
        ensure!(
            operation.resume_session == draft.resume_session,
            "Reserved launch operation {} continues a different session",
            operation.operation_id
        );
        Ok(())
    }

    /// Atomically reserves the operation. Concurrent hosts receive the same
    /// durable record, so they cannot both create a session or a job.
    fn reserve_operation(&self, key: &str, candidate: LaunchOperation) -> Result<LaunchOperation> {
        self.context
            .db
            .with_conn(|conn| -> Result<LaunchOperation> {
                let tx = rusqlite::Transaction::new_unchecked(
                    conn,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT value FROM peripheral_settings WHERE key=?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(value) = existing {
                    let operation = serde_json::from_str(&value)
                        .context("Stored launch operation is invalid")?;
                    tx.commit()?;
                    return Ok(operation);
                }
                let value = serde_json::to_string(&candidate)?;
                tx.execute(
                    "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
                    [key, &value],
                )?;
                tx.commit()?;
                Ok(candidate)
            })?
    }

    fn update_operation<F>(&self, key: &str, apply: F) -> Result<LaunchOperation>
    where
        F: FnOnce(&mut LaunchOperation),
    {
        self.context
            .db
            .with_conn(|conn| -> Result<LaunchOperation> {
                let tx = rusqlite::Transaction::new_unchecked(
                    conn,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let value: Option<String> = tx
                    .query_row(
                        "SELECT value FROM peripheral_settings WHERE key=?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                let value = value.context("Launch operation disappeared")?;
                let mut operation: LaunchOperation =
                    serde_json::from_str(&value).context("Stored launch operation is invalid")?;
                let previous_state = operation.state.clone();
                let previous_handle = operation.handle.clone();
                apply(&mut operation);
                // Durable stages only move forward and a recorded handle is never
                // dropped, so concurrent recovery cannot downgrade a completed run.
                if state_rank(&operation.state) < state_rank(&previous_state) {
                    operation.state = previous_state;
                }
                if operation.handle.is_none() {
                    operation.handle = previous_handle;
                }
                operation.updated_at = chrono::Utc::now().to_rfc3339();
                let value = serde_json::to_string(&operation)?;
                tx.execute(
                    "UPDATE peripheral_settings SET value=?2 WHERE key=?1",
                    [key, &value],
                )?;
                tx.commit()?;
                Ok(operation)
            })?
    }

    fn mark_state(&self, key: &str, state: &str) -> Result<()> {
        self.update_operation(key, |stored| {
            if state_rank(state) > state_rank(&stored.state) {
                stored.state = state.into();
            }
        })?;
        Ok(())
    }

    fn record_error(&self, key: &str, error: &anyhow::Error) -> Result<()> {
        self.update_operation(key, |stored| {
            stored.last_error = Some(format!("{error:#}"));
        })?;
        Ok(())
    }

    fn save_record(&self, task_id: &str, record: &LaunchRecord) -> Result<()> {
        self.context
            .save_setting(&launch_key(task_id, &record.role), record)
    }

    fn adopt_session(
        &self,
        draft: &LaunchDraft<'_>,
        operation: &LaunchOperation,
        job: &background_sessions::Job,
    ) -> Result<()> {
        let row = self.context.db.with_conn(|conn| {
            crate::db::conversation_repo::get_conversation(conn, &operation.session_id)
        })??;
        let row = row.with_context(|| {
            format!(
                "Background task {} has no reserved child session {}",
                job.id, operation.session_id
            )
        })?;
        ensure!(!row.archived, "Reserved child session is archived");
        let stored = row
            .workspace_path
            .as_deref()
            .context("Reserved child session has no workspace")?;
        ensure!(
            same_path(Path::new(stored), &operation.workspace),
            "Reserved child session belongs to another workspace"
        );
        let store = self.store();
        match draft.member {
            MemberRole::Coder => {
                store.set_sessions(&draft.task.task_id, Some(&operation.session_id), None)
            }
            MemberRole::Reviewer => {
                store.set_sessions(&draft.task.task_id, None, Some(&operation.session_id))
            }
        }
        .map_err(|error| anyhow!(error.to_string()))?;
        crate::native_engine::child_session::record_adopted_ownership(
            &self.context,
            &operation.session_id,
            &draft.task.router_session_id,
            &job.status,
            &job.id,
        )
    }

    async fn prepare(
        &self,
        draft: &LaunchDraft<'_>,
        operation: &LaunchOperation,
    ) -> Result<PreparedChildLaunch> {
        let mut extra_env = Vec::new();
        if let Some(warning) = &operation.warning {
            // The member records the fallback in its own transcript evidence.
            extra_env.push((
                "DEEPSEEK_CODE_SUPER_AGENT_MODEL_WARNING".into(),
                warning.clone(),
            ));
        }
        prepare_child_session(
            ChildLaunchConfig {
                provider: self.provider.clone(),
                permission_mode: if operation.permission_mode.is_empty() {
                    self.permission_mode.clone()
                } else {
                    operation.permission_mode.clone()
                },
            },
            ChildSpawn {
                data_dir: self.context.paths.data_dir.clone(),
                parent_session_id: draft.task.router_session_id.clone(),
                parent_workspace: draft.task.spec.workspace.clone(),
                workspace: ChildWorkspace::Directory(operation.workspace.clone()),
                prompt: operation.prompt.clone(),
                description: format!("{} {}", draft.task.task_id, draft.member.as_str()),
                agent_type: "general".into(),
                model: Some(operation.model.clone()),
                effort: None,
                system_prompt_append: Some(member_instructions(draft.task, draft.member)),
                title: format!("{} / {}", draft.task.task_id, draft.member.as_str()),
                resume_session: operation.resume_session.clone(),
                reserved_session_id: (operation.kind == LaunchKind::Initial.as_str())
                    .then(|| operation.session_id.clone()),
                reserved_job_id: Some(operation.job_id.clone()),
                extra_env,
                executable_override: self.executable_override.clone(),
                invocation: None,
                member_binding: Some((draft.task.task_id.clone(), draft.member)),
                allow_workspace_move: operation.allow_workspace_move,
                // Super Agent members are launched or resumed, never forked.
                fork: false,
            },
        )
        .await
    }

    fn prepare_review_workspace(&self, task: &TaskRecord) -> Result<PathBuf> {
        let mut roots = Vec::new();
        for repo in &task.repositories {
            let commit = repo
                .submitted_commit
                .as_deref()
                .context("The task has no submitted commit to review")?;
            let worktree = crate::worktree::session::prepare_review_worktree(
                &repo.repo_root,
                commit,
                &task.task_id,
                task.review_round,
            )?;
            ensure!(
                worktree.base_commit == commit,
                "Review worktree is not pinned at the submitted commit"
            );
            roots.push(worktree.worktree_root);
        }
        for target in &task.non_git_targets {
            roots.push(crate::super_agent::review::prepare_non_git_review(
                &self.context.paths.data_dir,
                &task.task_id,
                task.review_round,
                target,
            )?);
        }
        ensure!(!roots.is_empty(), "The task has no reviewable target");
        if roots.len() == 1 {
            return Ok(roots.remove(0));
        }
        let workspace = self
            .context
            .paths
            .data_dir
            .join("super-agent")
            .join(&task.task_id)
            .join(format!("review-workspace-{}", task.review_round));
        std::fs::create_dir_all(&workspace)?;
        for (index, root) in roots.iter().enumerate() {
            let link = workspace.join(format!("review-{}", index + 1));
            if !link.exists() {
                link_directory(root, &link)?;
            }
        }
        Ok(workspace)
    }

    /// Immutable checkout of the host-verified integrated content, distinct
    /// from the implementation review checkout of the same round.
    fn prepare_final_review_workspace(&self, task: &TaskRecord) -> Result<PathBuf> {
        let mut roots = Vec::new();
        for repo in &task.repositories {
            let commit = repo
                .integrated_commit
                .as_deref()
                .context("The task has no verified integrated commit to review")?;
            roots.push(
                crate::super_agent::integration::prepare_integrated_checkout(
                    repo,
                    commit,
                    &task.task_id,
                    task.spec_version,
                    task.review_round,
                )?,
            );
        }
        for target in &task.non_git_targets {
            roots.push(crate::super_agent::review::prepare_non_git_review(
                &self.context.paths.data_dir,
                &task.task_id,
                task.review_round,
                target,
            )?);
        }
        ensure!(
            !roots.is_empty(),
            "The task has no integrated content to review"
        );
        if roots.len() == 1 {
            return Ok(roots.remove(0));
        }
        let workspace = self
            .context
            .paths
            .data_dir
            .join("super-agent")
            .join(&task.task_id)
            .join(format!(
                "final-review-workspace-s{}-r{}",
                task.spec_version, task.review_round
            ));
        std::fs::create_dir_all(&workspace)?;
        for (index, root) in roots.iter().enumerate() {
            let link = workspace.join(format!("integrated-{}", index + 1));
            if !link.exists() {
                link_directory(root, &link)?;
            }
        }
        Ok(workspace)
    }
}

fn unknown_receipt(
    source: String,
    role: impl Into<String>,
    session: impl Into<String>,
    reason: impl Into<String>,
) -> MemberJobObservation {
    unknown_job_receipt(source, role, session, "", reason)
}

fn unknown_job_receipt(
    source: String,
    role: impl Into<String>,
    session: impl Into<String>,
    job_id: impl Into<String>,
    reason: impl Into<String>,
) -> MemberJobObservation {
    MemberJobObservation {
        receipt: MemberJobReceipt::new(source, role, session).with_job(job_id),
        state: MemberJobState::Unknown {
            reason: reason.into(),
        },
    }
}

/// Proves that a stored launch operation is self-consistent and belongs to the
/// task's current member slot before its job is inspected.
fn operation_receipt_session(
    task: &TaskRecord,
    coder: Option<&str>,
    reviewer: Option<&str>,
    key: &str,
    operation: &LaunchOperation,
) -> std::result::Result<String, String> {
    if operation.task_id != task.task_id {
        return Err(format!(
            "the operation belongs to task {}, not {}",
            operation.task_id, task.task_id
        ));
    }
    let expected = match operation.role.as_str() {
        "coder" => coder,
        "reviewer" => reviewer,
        other => return Err(format!("the operation names unknown member role {other:?}")),
    };
    let Some(expected) = expected else {
        return Err(format!(
            "the operation names a {} but the task has no such session",
            operation.role
        ));
    };
    if operation.session_id != expected {
        return Err(format!(
            "the operation names session {}, not the task's {} session {expected}",
            operation.session_id, operation.role
        ));
    }
    if operation.kind != LaunchKind::Initial.as_str()
        && operation.kind != LaunchKind::Resume.as_str()
    {
        return Err(format!(
            "the operation names unknown launch kind {:?}",
            operation.kind
        ));
    }
    if operation.spec_version > task.spec_version || operation.review_round > task.review_round {
        return Err(format!(
            "the operation names spec {} round {}, which is newer than this task's spec {} round {}",
            operation.spec_version, operation.review_round, task.spec_version, task.review_round
        ));
    }
    let identity = key_of(operation);
    if key != identity {
        return Err(format!(
            "the operation is stored under {key} but identifies itself as {identity}"
        ));
    }
    let expected_operation_id = if operation.delivery.is_some() {
        identity
    } else {
        format!("{}:{}:{}", task.task_id, operation.role, operation.kind)
    };
    if operation.operation_id != expected_operation_id {
        return Err(format!(
            "the operation id {:?} does not match its reserved identity {expected_operation_id:?}",
            operation.operation_id
        ));
    }
    if operation.job_id.trim().is_empty() || uuid::Uuid::parse_str(&operation.job_id).is_err() {
        return Err(format!(
            "the operation names no valid reserved job UUID ({:?})",
            operation.job_id
        ));
    }
    if let Some(handle) = &operation.handle {
        if handle.role != operation.role
            || handle.session_id != operation.session_id
            || handle.job_id != operation.job_id
            || handle.delivery != operation.delivery
        {
            return Err(format!(
                "the recorded handle ({} job {} session {}) does not match the reserved operation ({} job {} session {})",
                handle.role,
                handle.job_id,
                handle.session_id,
                operation.role,
                operation.job_id,
                operation.session_id
            ));
        }
    }
    if let Some(delivery) = &operation.delivery {
        if delivery.version != DELIVERY_IDENTITY_VERSION
            || delivery.task_id != task.task_id
            || delivery.spec_version != operation.spec_version
            || delivery.review_round != operation.review_round
            || delivery.coder_session_id != operation.session_id
            || delivery.purpose_key() != operation.purpose
        {
            return Err(
                "the operation's delivery identity does not match its task, spec, round, Coder session or purpose"
                    .into(),
            );
        }
    }
    Ok(expected.to_owned())
}

/// Proves that a reviewer resume marker belongs to this task's Reviewer and to
/// a real spec/round/purpose key. A `final-completion` marker must also name
/// the persisted delivery plan it was reserved for: the stage key alone is
/// shared by every plan of this spec/round, so a marker from another plan must
/// not be able to prove the current one.
fn reviewer_marker_identity(
    task: &TaskRecord,
    reviewer: Option<&str>,
    key: &str,
    marker: &serde_json::Value,
    session: &str,
    job_id: &str,
) -> std::result::Result<(String, u32, u32, String), String> {
    let Some(reviewer) = reviewer else {
        return Err("the task has no Reviewer session".into());
    };
    if session != reviewer {
        return Err(format!(
            "the marker names session {session}, not this task's Reviewer {reviewer}"
        ));
    }
    let prefix = format!("super-agent.reviewer-resume.{}.", task.task_id);
    let Some(rest) = key.strip_prefix(&prefix) else {
        return Err(format!("the marker key {key} is not scoped to this task"));
    };
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 2
        && !(parts.len() == 3 && matches!(parts[2], "final-delivery" | "final-completion"))
    {
        return Err(format!(
            "the marker key {key} is not a reviewer round or stage marker"
        ));
    }
    let spec = parts[0]
        .parse::<u32>()
        .map_err(|_| format!("the marker key {key} has no numeric spec version"))?;
    let round = parts[1]
        .parse::<u32>()
        .map_err(|_| format!("the marker key {key} has no numeric review round"))?;
    if spec > task.spec_version || round > task.review_round {
        return Err(format!(
            "the marker names spec {spec} round {round}, which is newer than this task's spec {} round {}",
            task.spec_version, task.review_round
        ));
    }
    if uuid::Uuid::parse_str(job_id).is_err() {
        return Err(format!(
            "the marker names no valid reserved job UUID ({job_id:?})"
        ));
    }
    let purpose = parts.get(2).copied().unwrap_or("").to_owned();
    let plan_operation_id = marker["plan_operation_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if purpose == "final-completion" && plan_operation_id.trim().is_empty() {
        return Err(format!(
            "the {purpose} marker for spec {spec} round {round} names no persisted delivery plan; a marker without a plan binding cannot prove the current completion review"
        ));
    }
    Ok((purpose, spec, round, plan_operation_id))
}

/// Proves that a delivery receipt is bound to the persisted immutable plan it
/// names and to a well-formed typed or genuine legacy purpose.
fn dispatch_identity(
    task: &TaskRecord,
    coder_session: &str,
    dispatch: &crate::super_agent::delivery::DeliveryDispatch,
    plan: &crate::super_agent::delivery::DeliveryPlan,
) -> std::result::Result<(), String> {
    if plan.task_id != task.task_id
        || plan.spec_version != dispatch.spec_version
        || plan.review_round != dispatch.review_round
    {
        return Err(format!(
            "the receipt names plan {} spec {} round {}, not this task's persisted plan for spec {} round {}",
            dispatch.plan_operation_id,
            dispatch.spec_version,
            dispatch.review_round,
            task.spec_version,
            task.review_round
        ));
    }
    let target = match dispatch.repository_id.as_deref() {
        Some(repository_id) => plan
            .repositories
            .iter()
            .find(|target| target.repository_id == repository_id),
        None if plan.repositories.len() == 1 => plan.repositories.first(),
        None => None,
    };
    let Some(target) = target else {
        return Err("the receipt cannot be bound to exactly one planned repository".into());
    };
    if dispatch.purpose == LEGACY_DELIVERY_PURPOSE {
        return Ok(());
    }
    if dispatch.repository_id.as_deref() != Some(target.repository_id.as_str())
        || dispatch.old_oid.as_deref() != Some(target.old_oid.as_str())
        || dispatch.new_oid.as_deref() != Some(target.new_oid.as_str())
        || dispatch.attempt != target.attempt
    {
        return Err(format!(
            "the receipt does not match the planned repository {}, its old/new OIDs or attempt",
            target.repository_id
        ));
    }
    let identity = DeliveryOperationIdentity::for_target(plan, target, coder_session);
    if identity.purpose_key() != dispatch.purpose {
        return Err(format!(
            "the receipt purpose {:?} does not match its planned delivery identity",
            dispatch.purpose
        ));
    }
    Ok(())
}

fn operation_key(task: &TaskRecord, member: MemberRole, kind: LaunchKind, purpose: &str) -> String {
    let base = format!(
        "super-agent.launch-op.{}.{}.{}.{}.{}",
        task.task_id,
        member.as_str(),
        kind.as_str(),
        task.spec_version,
        task.review_round
    );
    if purpose.is_empty() {
        base
    } else {
        format!("{base}.{purpose}")
    }
}

fn key_of(operation: &LaunchOperation) -> String {
    let base = format!(
        "super-agent.launch-op.{}.{}.{}.{}.{}",
        operation.task_id,
        operation.role,
        operation.kind,
        operation.spec_version,
        operation.review_round
    );
    if operation.purpose.is_empty() {
        base
    } else {
        format!("{base}.{}", operation.purpose)
    }
}

fn state_rank(state: &str) -> u8 {
    match state {
        "reserved" => 0,
        "session" => 1,
        "spawned" => 2,
        "completed" => 3,
        _ => 0,
    }
}

fn member_session(task: &TaskRecord, member: MemberRole) -> Option<&str> {
    match member {
        MemberRole::Coder => task.coder.session_id.as_deref(),
        MemberRole::Reviewer => task.reviewer.session_id.as_deref(),
    }
}

fn outcome_from_record(
    operation: &LaunchOperation,
    record: LaunchRecord,
    fresh: bool,
    in_progress: bool,
) -> LaunchOutcome {
    LaunchOutcome {
        session_id: record.session_id,
        job_id: record.job_id,
        workspace: record.workspace,
        model: record.model,
        warning: record.warning,
        resumed: operation.kind == LaunchKind::Resume.as_str(),
        fresh,
        in_progress,
    }
}

fn canonical(path: PathBuf) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("Workspace is unavailable: {}", path.display()))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn launch_key(task_id: &str, role: &str) -> String {
    format!("super-agent.launch.{task_id}.{role}")
}

#[cfg(unix)]
fn link_directory(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn link_directory(target: &Path, link: &Path) -> Result<()> {
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .context("Creating a review workspace junction")?;
    ensure!(
        output.status.success(),
        "Creating a review workspace junction failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Appended after the general agent system prompt on every launch, including a
/// resumed turn, so a resumed member keeps its role.
pub fn member_instructions(task: &TaskRecord, member: MemberRole) -> String {
    let spec = serde_json::to_string_pretty(&task.spec).unwrap_or_default();
    let repos = crp::repository_lines(&task.repositories).join("\n");
    match member {
        MemberRole::Coder => format!(
            "## Super Agent Task {} / Coder\n\nYou are the Coder for one task. Work only inside the task workspace. The task specification is authoritative:\n\n```json\n{spec}\n```\n\nRepositories and isolated branches:\n{repos}\n\nRules:\n- Start by calling EnterPlanMode. Investigate before proposing changes.\n- Submit your plan with CrpSubmit using kind \"plan\" and a PlanReport payload containing spec_version, round, understanding, scope, steps, verification and risks.\n- Do not edit business code before the plan is approved by the host.\n- When you implement, commit your work in the task workspace. Submit kind \"implementation\" with an ImplementationReport.\n- When the host asks for integration, merge the requested target ref into your task branch, resolve any conflicts, run the required verification, and submit kind \"integration\" with an IntegrationReport. Do not switch or modify the user's target branch.\n- Report unrelated findings with CrpProposeTask instead of expanding this task.\n",
            task.task_id
        ),
        MemberRole::Reviewer => format!(
            "## Super Agent Task {} / Reviewer\n\nYou review one task independently. You have a read-only checkout of the submitted commit. Do not modify files.\n\nThe task specification is authoritative:\n\n```json\n{spec}\n```\n\nRules:\n- Check the submitted commit against every acceptance criterion.\n- Submit kind \"review\" with a TaskReviewReport whose stage matches the one the host asks for: \"IMPLEMENTATION\" for the submitted candidate, \"FINAL_DELIVERY\" for the verified integrated content.\n- Submit kind \"harness\" with a HarnessAnalysisReport carrying the same stage. A Harness-category finding with severity high or critical blocks delivery; efficiency suggestions do not.\n- If the work is incomplete, use verdict \"CHANGES_REQUESTED\" and list concrete issues. Use \"BLOCKED\" only when the task cannot be verified.\n",
            task.task_id
        ),
    }
}

pub fn coder_plan_prompt(task: &TaskRecord) -> String {
    format!(
        "Task {}: {}\n\nGoal: {}\n\nInvestigate the task workspace and produce an implementation plan. Call EnterPlanMode first, then submit the plan with CrpSubmit(kind=\"plan\"). Do not modify business code until the host approves the plan.",
        task.task_id, task.title, task.spec.goal
    )
}

pub fn coder_implementation_prompt(task: &TaskRecord) -> String {
    format!(
        "The plan for task {} was approved (spec version {}, review round {}). Implement it now in the task workspace, run the verification you planned, and commit the result. Then submit an implementation report with CrpSubmit(kind=\"implementation\") whose spec_version is {} and round is {}.",
        task.task_id,
        task.spec_version,
        task.review_round,
        task.spec_version,
        task.review_round
    )
}

/// Instruction for stage 5B2a delivery. It names the exact approved commit for
/// one repository, the current target object and one bounded command built from
/// the persisted plan. The Coder performs the write; the host only schedules
/// and verifies.
pub fn coder_delivery_prompt(
    task: &TaskRecord,
    target: &crate::super_agent::delivery::DeliveryTargetPlan,
) -> String {
    let destination = match &target.destination {
        crate::super_agent::delivery::DeliveryDestination::CheckedOut { path } => {
            format!("the checked-out target worktree {}", path.display())
        }
        crate::super_agent::delivery::DeliveryDestination::RefOnly => {
            "the unchecked target branch".to_owned()
        }
    };
    format!(
        "Task {}: deliver the host-verified integration (spec version {}, review round {}) to the target branch.\n\nThe host verified integrated commit {} for repository {}. Its target branch {} (at {}) must become exactly {} in {}.\n\nRun exactly this command with the Bash tool. It is already guarded against a moved target and does not overwrite ignored working files:\n\n```sh\n{}\n```\n\nDo not push. Do not reset, clean, stash, switch branches or modify any other file. If the command fails, stop and report the exact error instead of working around it. Then reply with one short sentence describing the observed result.",
        task.task_id,
        task.spec_version,
        task.review_round,
        target.new_oid,
        target.repository_id,
        target.target_ref,
        target.old_oid,
        target.new_oid,
        destination,
        crate::super_agent::delivery::delivery_command(target)
    )
}

/// Instruction for the integration preparation step. It names the exact
/// repository, target reference, reviewed commit and captured baseline so the
/// Coder merges the right content into its own worktree, and it states the
/// verification requirements from the task specification.
pub fn coder_integration_prompt(
    task: &TaskRecord,
    baselines: &[crate::super_agent::integration::IntegrationBaseline],
) -> String {
    let repos: Vec<String> = baselines
        .iter()
        .map(|baseline| {
            format!(
                "- repository {}: merge {} (commit {}) into your task branch; the reviewed commit is {}",
                baseline.repository_id,
                baseline.target_ref,
                baseline.baseline_commit,
                baseline.reviewed_commit
            )
        })
        .collect();
    let verification = if task.spec.requirements.is_empty() {
        "Run the project's own tests.".to_owned()
    } else {
        task.spec
            .requirements
            .iter()
            .map(|requirement| format!("- {requirement}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "The implementation review for task {} was approved (spec version {}, review round {}). Prepare integration inside your own task worktree now. Do not switch, check out or modify the user's target branch.\n\nMerge these target references into your task branch, resolving conflicts if any:\n{}\n\nVerification required:\n{}\n\nCommit the result, leave the worktree clean, then submit CrpSubmit(kind=\"integration\") with an IntegrationReport whose spec_version is {}, round is {}, and targets entries carry repository_id, target_branch, before_commit (the target commit you merged), after_commit (your resulting HEAD), conflict=false and tests_passed=true.",
        task.task_id,
        task.spec_version,
        task.review_round,
        repos.join("\n"),
        verification,
        task.spec_version,
        task.review_round
    )
}

pub fn reviewer_prompt(task: &TaskRecord, review_request: &serde_json::Value) -> String {
    let versions: Vec<String> = task
        .repositories
        .iter()
        .filter_map(|repo| {
            repo.submitted_commit
                .as_deref()
                .map(|commit| format!("{} at {commit}", repo.repository_id))
        })
        .chain(task.non_git_targets.iter().filter_map(|target| {
            target.submission.as_ref().map(|submission| {
                format!("{} at {}", target.path.display(), submission.artifact_id)
            })
        }))
        .collect();
    let request = serde_json::to_string_pretty(review_request).unwrap_or_default();
    format!(
        "Task {}: {}\n\nThe host pinned the submitted implementation ({}). Review every acceptance criterion independently. Use SessionMetadata, SessionMessages, SessionEvents, SessionChildren and ArtifactRead to inspect the Coder session tree; they are read-only and scoped to this task. For a non-Git target, answer only the pinned snapshot shown in review_request and set reviewed_versions[].snapshot_ref to that immutable snapshot_ref. Then submit CrpSubmit(kind=\"review\") and CrpSubmit(kind=\"harness\"). Both reports are required before the host decides the outcome.\n\nreview_request:\n```json\n{request}\n```",
        task.task_id,
        task.title,
        versions.join(", ")
    )
}

/// Final-delivery review of the host-verified integrated content.
pub fn reviewer_final_prompt(task: &TaskRecord, review_request: &serde_json::Value) -> String {
    let versions: Vec<String> = task
        .repositories
        .iter()
        .filter_map(|repo| {
            repo.integrated_commit
                .as_deref()
                .map(|commit| format!("{} at {commit}", repo.repository_id))
        })
        .collect();
    let request = serde_json::to_string_pretty(review_request).unwrap_or_default();
    format!(
        "Task {}: {}\n\nThe host prepared the integration and verified the isolated task worktree ({}). These repositories were checked out read-only at the exact integrated commit. Review the integrated content against every acceptance criterion; do not review an earlier implementation commit. Then submit CrpSubmit(kind=\"review\") with stage \"FINAL_DELIVERY\" and reviewed_versions[].commit equal to the integrated commit, and CrpSubmit(kind=\"harness\") with stage \"FINAL_DELIVERY\". Both new reports are required; earlier implementation reports cannot decide this review.\n\nreview_request:\n```json\n{request}\n```",
        task.task_id,
        task.title,
        versions.join(", ")
    )
}

/// Post-delivery completion review. The verified integrated content has
/// already been written to the user's target branches, so this review decides
/// only whether the task may be completed. A negative verdict blocks
/// completion; it cannot roll back or replay the delivered commits.
pub fn reviewer_finalizing_prompt(task: &TaskRecord, review_request: &serde_json::Value) -> String {
    let versions: Vec<String> = task
        .repositories
        .iter()
        .filter_map(|repo| {
            repo.integrated_commit
                .as_deref()
                .map(|commit| format!("{} at {commit}", repo.repository_id))
        })
        .collect();
    let request = serde_json::to_string_pretty(review_request).unwrap_or_default();
    format!(
        "Task {}: {}\n\nThe host verified the integration ({}) and delivered exactly those commits to the user's target branches. Review that delivered content against every acceptance criterion; do not review an earlier implementation commit. Then submit CrpSubmit(kind=\"review\") with stage \"FINAL_DELIVERY\" and reviewed_versions[].commit equal to the integrated commit, and CrpSubmit(kind=\"harness\") with stage \"FINAL_DELIVERY\". Both new reports are required; earlier implementation reports cannot decide this review. This is the final completion review: a negative verdict blocks completion, but the delivered commits stay in the target branches and cannot be rolled back or replayed.\n\nreview_request:\n```json\n{request}\n```",
        task.task_id,
        task.title,
        versions.join(", ")
    )
}

#[cfg(test)]
mod tests;
