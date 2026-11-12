//! Stage 5B1: delivery preflight and cooperative branch ownership.
//!
//! Stage 5A leaves a task in `DELIVERY_PENDING` with a host-verified integrated
//! commit. This module prepares the durable, reviewable plan for moving the
//! user's target reference there. It **changes no Git ref and no worktree
//! file**: it resolves the exact target, records the expected old/new object
//! IDs, and takes cooperative ownership of the branch so two Super Agent
//! deliveries cannot move it at the same time.
//!
//! The cooperative `branch:` lock only arbitrates callers of this store. It
//! cannot stop an arbitrary external `git` command from moving the branch, so
//! every later delivery step must still use the recorded old OID as a CAS
//! expectation and re-verify the real target afterwards.
use crate::super_agent::integration::{
    is_ancestor, resolve_ref, status_entries, tree_of, VerifiedIntegration,
};
use crate::super_agent::model::{
    Event, EventType, HarnessAnalysisReport, IntegrationStatus, RepoTarget, ReviewStage,
    ReviewVerdict, TaskRecord, TaskReviewReport,
};
use crate::super_agent::store::{LockOutcome, StoreError, TaskStore};
use crate::worktree::session::{command, git};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Current version of the repository delivery execution identity. Bump it when
/// the identity fields or their meaning change; a record of another version is
/// never silently adopted.
pub const DELIVERY_IDENTITY_VERSION: u32 = 1;

/// The legacy evidence purpose used by deliveries recorded before the typed
/// identity existed. It is still accepted for reading and for one uniquely
/// bound single-repository recovery, never for a new dispatch.
pub const LEGACY_DELIVERY_PURPOSE: &str = "delivery";

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The complete execution identity of one repository delivery.
///
/// Every field is a durable host fact. The struct is stored as-is in launch
/// operations, handles and receipts, so a reviewer can inspect all of it; the
/// derived purpose is a stable, collision-resistant key, never a concatenation
/// of unescaped identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryOperationIdentity {
    /// Required. A typed record that lost this field is incomplete and must
    /// fail to deserialize; only the outer `Option` being `None` means legacy.
    pub version: u32,
    pub task_id: String,
    pub spec_version: u32,
    pub review_round: u32,
    /// The immutable approved plan this execution belongs to.
    pub plan_operation_id: String,
    pub evidence_event_id: String,
    pub review_event_id: String,
    pub content_fingerprint: String,
    pub repository_id: String,
    pub old_oid: String,
    pub new_oid: String,
    pub attempt: u32,
    /// The original Coder session is the only session allowed to execute it.
    pub coder_session_id: String,
}

impl DeliveryOperationIdentity {
    /// Derives the execution identity from the persisted plan target and the
    /// original Coder session. Callers never supply these fields separately.
    pub fn for_target(
        plan: &DeliveryPlan,
        target: &DeliveryTargetPlan,
        coder_session_id: &str,
    ) -> Self {
        Self {
            version: DELIVERY_IDENTITY_VERSION,
            task_id: plan.task_id.clone(),
            spec_version: plan.spec_version,
            review_round: plan.review_round,
            plan_operation_id: plan.operation_id.clone(),
            evidence_event_id: plan.evidence_event_id.clone(),
            review_event_id: plan.review_event_id.clone(),
            content_fingerprint: plan.content_fingerprint.clone(),
            repository_id: target.repository_id.clone(),
            old_oid: target.old_oid.clone(),
            new_oid: target.new_oid.clone(),
            attempt: target.attempt,
            coder_session_id: coder_session_id.to_owned(),
        }
    }

    /// Deterministic, collision-resistant evidence purpose. Two repositories of
    /// the same plan (even with identical OIDs) and two attempts differ.
    pub fn purpose_key(&self) -> String {
        let canonical = serde_json::to_vec(self)
            .expect("a delivery operation identity always serializes");
        let digest = Sha256::digest(canonical);
        format!(
            "{LEGACY_DELIVERY_PURPOSE}.v{}.{}",
            self.version,
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    /// Checks every identity field against the persisted plan, its target and
    /// the current Coder. Historical records are not authorization: callers
    /// still re-read the live Git state separately.
    pub fn validate_for(
        &self,
        task: &TaskRecord,
        plan: &DeliveryPlan,
        target: &DeliveryTargetPlan,
    ) -> Result<()> {
        ensure!(
            self.version == DELIVERY_IDENTITY_VERSION,
            "delivery identity version {} is not supported (expected {DELIVERY_IDENTITY_VERSION})",
            self.version
        );
        ensure!(
            self.task_id == task.task_id
                && self.task_id == plan.task_id
                && self.spec_version == task.spec_version
                && self.spec_version == plan.spec_version
                && self.review_round == task.review_round
                && self.review_round == plan.review_round,
            "delivery identity does not match the current task {} spec {} round {}",
            task.task_id,
            task.spec_version,
            task.review_round
        );
        ensure!(
            self.plan_operation_id == plan.operation_id
                && self.evidence_event_id == plan.evidence_event_id
                && self.review_event_id == plan.review_event_id
                && self.content_fingerprint == plan.content_fingerprint,
            "delivery identity does not match the persisted approved plan {}",
            plan.operation_id
        );
        ensure!(
            self.repository_id == target.repository_id
                && self.old_oid == target.old_oid
                && self.new_oid == target.new_oid
                && self.attempt == target.attempt,
            "delivery identity does not match the approved repository {}, old/new OIDs and attempt",
            target.repository_id
        );
        ensure!(
            task.coder.session_id.as_deref() == Some(self.coder_session_id.as_str()),
            "delivery identity belongs to Coder session {}, not the task's {:?}",
            self.coder_session_id,
            task.coder.session_id
        );
        Ok(())
    }
}

/// True for the legacy evidence purpose and for well-formed typed delivery
/// purposes. An arbitrary purpose is never accepted as a delivery.
pub fn is_delivery_purpose(purpose: &str) -> bool {
    if purpose == LEGACY_DELIVERY_PURPOSE {
        return true;
    }
    let prefix = format!("{LEGACY_DELIVERY_PURPOSE}.v{DELIVERY_IDENTITY_VERSION}.");
    purpose
        .strip_prefix(&prefix)
        .is_some_and(|digest| digest.len() == 64 && is_lower_hex(digest))
}

/// True when the purpose belongs to the typed delivery namespace, including a
/// malformed or unsupported version. A resume must not bypass the typed
/// identity by choosing such a purpose directly.
pub fn is_delivery_purpose_namespace(purpose: &str) -> bool {
    purpose == LEGACY_DELIVERY_PURPOSE
        || purpose.starts_with(&format!("{LEGACY_DELIVERY_PURPOSE}."))
}

/// Where the prepared delivery has to write the new object ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeliveryDestination {
    /// The target branch is checked out in this worktree. It was verified
    /// clean, on that branch and at the captured baseline when the plan was
    /// built. A later delivery fast-forwards it (round 5B2).
    CheckedOut { path: PathBuf },
    /// No worktree has the target branch checked out. A later delivery must use
    /// a compare-and-swap `update-ref` with `old_oid` (round 5B2).
    RefOnly,
}

/// One repository's host-verified delivery facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTargetPlan {
    pub repository_id: String,
    pub repo_root: PathBuf,
    pub git_common_dir: PathBuf,
    pub target_branch: String,
    pub target_ref: String,
    pub destination: DeliveryDestination,
    /// Baseline captured before the Coder integrated.
    pub baseline_commit: String,
    pub reviewed_commit: String,
    pub integrated_commit: String,
    pub integrated_tree: String,
    /// Compare-and-swap expectation (must equal the current target ref).
    pub old_oid: String,
    /// The approved integrated commit the target must become.
    pub new_oid: String,
    /// Which host-decided review covers the content being delivered.
    pub review_stage: ReviewStage,
    /// Cooperative ownership key: `branch:<common-dir>:<full-ref>`.
    pub lock_key: String,
    pub lock_operation_id: String,
    /// Explicit delivery attempt within the immutable content identity. Zero
    /// only for a plan recorded before repository-scoped attempts existed.
    #[serde(default)]
    pub attempt: u32,
}

/// The persisted delivery plan. It is derived only from host facts: the
/// verified `INTEGRATION_PREPARED` event and the host-decided approved review +
/// harness. A Coder's or Reviewer's "success"/"ready" boolean is never read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPlan {
    pub operation_id: String,
    pub task_id: String,
    pub spec_version: u32,
    pub review_round: u32,
    /// The host-verified preparation event this plan is bound to.
    pub evidence_event_id: String,
    /// The host-decided review event that approved the delivered content.
    pub review_event_id: String,
    /// Fingerprint over the approved content identity.
    pub content_fingerprint: String,
    pub created_at: String,
    pub repositories: Vec<DeliveryTargetPlan>,
}

impl DeliveryPlan {
    /// Equality of every host fact that makes the plan valid. `created_at` and
    /// the cooperative lock identities are bookkeeping: a blocked attempt gets
    /// a fresh attempt identity without changing the approved content facts.
    pub fn same_facts(&self, other: &Self) -> bool {
        self.operation_id == other.operation_id
            && self.task_id == other.task_id
            && self.spec_version == other.spec_version
            && self.review_round == other.review_round
            && self.evidence_event_id == other.evidence_event_id
            && self.review_event_id == other.review_event_id
            && self.content_fingerprint == other.content_fingerprint
            && self.repositories.len() == other.repositories.len()
            && self
                .repositories
                .iter()
                .zip(other.repositories.iter())
                .all(|(left, right)| target_facts_equal(left, right))
    }
}

fn target_facts_equal(left: &DeliveryTargetPlan, right: &DeliveryTargetPlan) -> bool {
    // The attempt number is bookkeeping: a legacy plan has zero and a new plan
    // starts at one without changing the approved content.
    left.repository_id == right.repository_id
        && left.repo_root == right.repo_root
        && left.git_common_dir == right.git_common_dir
        && left.target_branch == right.target_branch
        && left.target_ref == right.target_ref
        && left.destination == right.destination
        && left.baseline_commit == right.baseline_commit
        && left.reviewed_commit == right.reviewed_commit
        && left.integrated_commit == right.integrated_commit
        && left.integrated_tree == right.integrated_tree
        && left.old_oid == right.old_oid
        && left.new_oid == right.new_oid
        && left.review_stage == right.review_stage
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryPreflight {
    /// A durable plan was recorded and every target is cooperatively owned.
    Planned(DeliveryPlan),
    /// The task has no Git target, so there is no branch to deliver.
    NotApplicable,
    /// The target reference moved after the integration baseline was captured.
    /// The old plan is invalid; a new integration is required. Nothing was
    /// overwritten and the old OID was not refreshed under the stale approval.
    ReintegrationNeeded { reason: String },
    /// Delivery must not proceed. The reason is user-readable and no part of
    /// the target was changed.
    Blocked { reason: String },
}

/// Per-repository outcome of the host's post-delivery verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTargetVerification {
    pub repository_id: String,
    pub expected_commit: String,
    pub actual_commit: Option<String>,
    pub verified: bool,
    pub reason: String,
}

/// Result of checking the real target refs against a plan. A partial result
/// keeps the repositories that did verify and reports exactly which are still
/// pending, so the host never claims all-or-nothing after a first delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryVerification {
    pub repositories: Vec<DeliveryTargetVerification>,
    pub pending: Vec<String>,
    pub failures: Vec<String>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRelease {
    pub released: Vec<String>,
    pub failures: Vec<String>,
}

impl DeliveryRelease {
    pub fn complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Cooperative ownership of the planned branches, read from the lock table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryLockState {
    pub repository_id: String,
    pub lock_key: String,
    pub owned: bool,
    pub holder: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryOwnership {
    pub repositories: Vec<DeliveryLockState>,
}

impl DeliveryOwnership {
    pub fn complete(&self) -> bool {
        !self.repositories.is_empty() && self.repositories.iter().all(|state| state.owned)
    }

    pub fn reason(&self) -> Option<String> {
        if self.complete() {
            return None;
        }
        let details: Vec<String> = self
            .repositories
            .iter()
            .filter(|state| !state.owned)
            .map(|state| match &state.holder {
                Some((task, operation)) => format!(
                    "{} is held by task {task} operation {operation}",
                    state.lock_key
                ),
                None => format!("{} is not owned", state.lock_key),
            })
            .collect();
        Some(format!(
            "cooperative branch ownership is not held by this delivery: {}",
            details.join("; ")
        ))
    }
}

/// Reads current ownership for every branch in the plan. It never acquires.
pub fn delivery_ownership(store: &TaskStore, plan: &DeliveryPlan) -> Result<DeliveryOwnership> {
    let mut repositories = Vec::new();
    for target in &plan.repositories {
        let holder = store.lock_holder(&target.lock_key)?;
        let owned = holder.as_ref().is_some_and(|(task, operation)| {
            task == &plan.task_id && operation == &target.lock_operation_id
        });
        repositories.push(DeliveryLockState {
            repository_id: target.repository_id.clone(),
            lock_key: target.lock_key.clone(),
            owned,
            holder,
        });
    }
    Ok(DeliveryOwnership { repositories })
}

/// Re-checks that the stored plan still names the current host evidence for
/// this task version. It does not touch Git or locks.
pub fn verify_plan_binding(
    store: &TaskStore,
    task: &TaskRecord,
    plan: &DeliveryPlan,
) -> Result<Option<String>> {
    let events = store.events(&task.task_id)?;
    Ok(verify_plan_binding_events(task, plan, &events))
}

/// The same immutable-plan binding rules over already-loaded durable evidence.
/// The store calls this inside its completion transaction, so a direct
/// `TASK_COMPLETED` is held to exactly the rules a delivery dispatch uses and
/// cannot rely on an unapproved or stale lineage.
pub fn verify_plan_binding_events(
    task: &TaskRecord,
    plan: &DeliveryPlan,
    events: &[Event],
) -> Option<String> {
    if plan.task_id != task.task_id
        || plan.spec_version != task.spec_version
        || plan.review_round != task.review_round
    {
        return Some("the delivery plan does not belong to the current task version".into());
    }
    // Exact, unique membership. A plan may neither narrow the task's
    // repositories nor name one twice; every planned repository must be the
    // task's own.
    let planned: BTreeSet<&str> = plan
        .repositories
        .iter()
        .map(|target| target.repository_id.as_str())
        .collect();
    let current: BTreeSet<&str> = task
        .repositories
        .iter()
        .map(|repo| repo.repository_id.as_str())
        .collect();
    if planned.len() != plan.repositories.len() {
        return Some("the delivery plan lists a repository more than once".into());
    }
    if planned != current {
        return Some(format!(
            "the delivery plan repository set {planned:?} does not match the task's {current:?}"
        ));
    }
    for target in &plan.repositories {
        let Some(repo) = task
            .repositories
            .iter()
            .find(|repo| repo.repository_id == target.repository_id)
        else {
            return Some(format!(
                "the task no longer lists repository {}",
                target.repository_id
            ));
        };
        if repo.reviewed_commit.as_deref() != Some(target.reviewed_commit.as_str())
            || repo.integrated_commit.as_deref() != Some(target.new_oid.as_str())
        {
            return Some(format!(
                "{}: the task's reviewed/integrated commits no longer match the planning approval",
                target.repository_id
            ));
        }
    }
    let Some(prepared) = events
        .iter()
        .find(|event| event.event_id == plan.evidence_event_id)
    else {
        return Some("the host integration preparation event for this plan is missing".into());
    };
    if prepared.event_type != EventType::IntegrationPrepared
        || prepared.spec_version != task.spec_version
        || prepared.review_round != task.review_round
    {
        return Some("the recorded integration preparation is no longer current".into());
    }
    let Some(decision) = events
        .iter()
        .find(|event| event.event_id == plan.review_event_id)
    else {
        return Some("the host review decision for this plan is missing".into());
    };
    let stage = plan
        .repositories
        .first()
        .map(|target| target.review_stage.as_str())
        .unwrap_or_default();
    if decision.event_type != EventType::ReviewCompleted
        || decision.spec_version != task.spec_version
        || decision.review_round != task.review_round
        || decision.payload["verdict"].as_str() != Some(ReviewVerdict::Approved.as_str())
        || decision.payload["stage"].as_str() != Some(stage)
    {
        return Some("the approval that produced the delivery plan is no longer current".into());
    }
    None
}

/// Durable delivery dispatch intent. It is written **before** the Coder is
/// resumed, binds the immutable delivery plan to the original Coder session,
/// and records the reserved job once the launcher returns it. Replaying the
/// same plan reuses this record instead of launching another child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryDispatch {
    pub task_id: String,
    pub spec_version: u32,
    pub review_round: u32,
    pub plan_operation_id: String,
    pub coder_session_id: String,
    /// Evidence identity passed to the launcher, e.g. `delivery`.
    pub purpose: String,
    /// The repository this receipt delivers. `None` only for a receipt recorded
    /// before repository-scoped delivery identities existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_oid: Option<String>,
    /// Explicit attempt within the immutable content identity; zero only for a
    /// legacy receipt.
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub created_at: String,
}

/// How a repository delivery observation was produced. Completion needs this
/// distinction: a target the host found already carrying the approved commit
/// never had a delivery child, so it must not be asked for a Coder receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryObservationSource {
    /// Recorded before this field existed. The source cannot be proven, so
    /// completion treats a receipt-less observation conservatively.
    #[default]
    Unknown,
    /// The host verified the target already carried the approved commit; no
    /// delivery child was ever dispatched for it.
    HostVerifiedAlreadyAtTarget,
    /// The host observed the result of a real delivery job.
    DeliveryJob,
}

/// One repository's host-verified delivery observation. It is durable evidence
/// that this repository reached the approved integrated commit under the
/// persisted plan. It never marks the whole task delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDeliveryObservation {
    pub task_id: String,
    pub spec_version: u32,
    pub review_round: u32,
    pub plan_operation_id: String,
    pub repository_id: String,
    pub old_oid: String,
    pub new_oid: String,
    pub target_ref: String,
    pub coder_session_id: String,
    pub attempt: u32,
    /// Only a real host verification may be recorded.
    pub verified: bool,
    /// Which path produced this observation. Legacy rows default to Unknown.
    #[serde(default)]
    pub source: DeliveryObservationSource,
    pub observed_at: String,
}

/// Quotes one shell word for a POSIX shell. Delivery paths can contain spaces
/// and the exact bytes must survive, so the command is never built by plain
/// interpolation.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn quoted_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

/// The exact Git command that moves the target reference to `new_oid`.
///
/// A checked-out target is fast-forwarded in that real worktree and refuses to
/// overwrite ignored working files (`--no-overwrite-ignore`) or run when the
/// checkout switched branch or moved since preflight. An unchecked target uses
/// `update-ref` with the captured OID as a compare-and-swap expectation. The
/// host never runs these commands itself; the Coder does.
pub fn delivery_command(target: &DeliveryTargetPlan) -> String {
    match &target.destination {
        // Every observation must succeed before the reference is moved: a
        // failed `worktree list` would otherwise look like "not checked out".
        // `grep` exit 1 means "no matching line", 0 means "checked out" and 2
        // means the inspection itself failed.
        DeliveryDestination::RefOnly => format!(
            "listing=$(git -C {root} worktree list --porcelain) || exit 1\nprintf '%s\\n' \"$listing\" | grep -Fxq -- {branch_line}\nstatus=$?\nif [ \"$status\" -eq 0 ]; then echo 'the target branch is checked out; re-run the preflight' >&2; exit 1; fi\nif [ \"$status\" -ne 1 ]; then echo 'cannot inspect the Git worktrees' >&2; exit 1; fi\ngit -C {root} update-ref {target_ref} {new} {old}",
            root = quoted_path(&target.repo_root),
            branch_line = shell_quote(&format!("branch {}", target.target_ref)),
            target_ref = shell_quote(&target.target_ref),
            new = target.new_oid,
            old = target.old_oid
        ),
        // A failed `symbolic-ref`, `rev-parse` or `status` aborts before the
        // merge; an empty stdout is never treated as a clean checkout.
        DeliveryDestination::CheckedOut { path } => format!(
            "branch=$(git -C {path} symbolic-ref --quiet --short HEAD) || exit 1\n[ \"$branch\" = {branch} ] || exit 1\nhead=$(git -C {path} rev-parse HEAD) || exit 1\n[ \"$head\" = {old} ] || exit 1\ndirty=$(git -C {path} status --porcelain=v1 --untracked-files=all) || exit 1\n[ -z \"$dirty\" ] || exit 1\ngit -C {path} merge --ff-only --no-overwrite-ignore {new}",
            path = quoted_path(path),
            branch = shell_quote(&target.target_branch),
            old = target.old_oid,
            new = target.new_oid
        ),
    }
}

/// Outcome of re-checking a persisted plan immediately before dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchRevalidation {
    /// The target is still exactly what the plan captured.
    Ready,
    /// The target reference moved after the plan was recorded.
    ReintegrationNeeded(String),
    /// A checkout appeared, disappeared, moved, changed branch or is dirty.
    Blocked(String),
}

/// Re-checks the live target against an already recorded plan, without
/// recomputing its baseline. The plan is immutable evidence; this only decides
/// whether sending it now is still safe.
pub fn revalidate_for_dispatch(
    task: &TaskRecord,
    plan: &DeliveryPlan,
) -> Result<DispatchRevalidation> {
    for target in &plan.repositories {
        let Some(repo) = task
            .repositories
            .iter()
            .find(|repo| repo.repository_id == target.repository_id)
        else {
            return Ok(DispatchRevalidation::Blocked(format!(
                "the task no longer lists repository {}",
                target.repository_id
            )));
        };
        match revalidate_target(repo, target)? {
            DispatchRevalidation::Ready => {}
            other => return Ok(other),
        }
    }
    Ok(DispatchRevalidation::Ready)
}

/// Validates one named repository of a complete plan before dispatching it.
///
/// Another repository may already be at its approved `new_oid`; that is not
/// drift for this repository. The named repository's identity, OID, checkout
/// topology, cleanliness and cooperative lock ownership are all re-read now; a
/// persisted observation is never used as authorization.
pub fn revalidate_repository_for_dispatch(
    store: &TaskStore,
    task: &TaskRecord,
    plan: &DeliveryPlan,
    repository_id: &str,
) -> Result<DispatchRevalidation> {
    if plan.task_id != task.task_id
        || plan.spec_version != task.spec_version
        || plan.review_round != task.review_round
    {
        return Ok(DispatchRevalidation::Blocked(
            "the delivery plan does not belong to the current task version".into(),
        ));
    }
    // The persisted immutable plan is the authority. A caller cannot pass a
    // refreshed old OID (or another plan) and turn a real target drift into a
    // ready dispatch.
    let Some(persisted) = store.delivery_plan(&task.task_id, &plan.operation_id)? else {
        return Ok(DispatchRevalidation::Blocked(format!(
            "the delivery plan {} is not recorded for this task",
            plan.operation_id
        )));
    };
    if !persisted.same_facts(plan) {
        return Ok(DispatchRevalidation::Blocked(format!(
            "the supplied delivery plan {} is not the persisted immutable plan",
            plan.operation_id
        )));
    }
    // `same_facts` treats attempt numbers and cooperative lock identities as
    // bookkeeping. Everything below reads the persisted plan, so a caller that
    // tampers with another target's attempt or lock identity cannot influence
    // this decision; the named target is compared exactly (including attempt
    // and lock identity) before its live Git state is read.
    if let Some(reason) = verify_plan_binding(store, task, &persisted)? {
        return Ok(DispatchRevalidation::Blocked(reason));
    }
    let planned: BTreeSet<&str> = persisted
        .repositories
        .iter()
        .map(|target| target.repository_id.as_str())
        .collect();
    let current: BTreeSet<&str> = task
        .repositories
        .iter()
        .map(|repo| repo.repository_id.as_str())
        .collect();
    if planned != current {
        return Ok(DispatchRevalidation::Blocked(format!(
            "the delivery plan repository set {planned:?} does not match the task's {current:?}"
        )));
    }
    let Some(target) = persisted
        .repositories
        .iter()
        .find(|target| target.repository_id == repository_id)
    else {
        return Ok(DispatchRevalidation::Blocked(format!(
            "the delivery plan does not contain repository {repository_id}"
        )));
    };
    // The named target's attempt and cooperative lock identity are part of the
    // persisted plan too, not caller-supplied bookkeeping.
    if plan
        .repositories
        .iter()
        .find(|supplied| supplied.repository_id == repository_id)
        != Some(target)
    {
        return Ok(DispatchRevalidation::Blocked(format!(
            "{repository_id}: the supplied target identity is not the persisted immutable target"
        )));
    }
    let Some(repo) = task
        .repositories
        .iter()
        .find(|repo| repo.repository_id == repository_id)
    else {
        return Ok(DispatchRevalidation::Blocked(format!(
            "the task no longer lists repository {repository_id}"
        )));
    };
    if repo.reviewed_commit.as_deref() != Some(target.reviewed_commit.as_str())
        || repo.integrated_commit.as_deref() != Some(target.new_oid.as_str())
        || repo.target_branch != target.target_branch
    {
        return Ok(DispatchRevalidation::Blocked(format!(
            "{repository_id}: the task's reviewed/integrated commits or target branch no longer match the approved plan"
        )));
    }
    let common = match canonical_common_dir(&repo.repo_root) {
        Ok(common) => common,
        Err(error) => {
            return Ok(DispatchRevalidation::Blocked(format!(
                "{repository_id}: cannot identify the repository: {error:#}"
            )))
        }
    };
    if common != target.git_common_dir {
        return Ok(DispatchRevalidation::Blocked(format!(
            "{repository_id}: the repository at {} belongs to {} but the plan recorded {}",
            repo.repo_root.display(),
            common.display(),
            target.git_common_dir.display()
        )));
    }
    let holder = store.lock_holder(&target.lock_key)?;
    let owned = holder
        .as_ref()
        .is_some_and(|(holder_task, holder_operation)| {
            holder_task == &task.task_id && holder_operation == &target.lock_operation_id
        });
    if !owned {
        return Ok(DispatchRevalidation::Blocked(match holder {
            Some((task, operation)) => format!(
                "{repository_id}: the cooperative branch ownership {} is held by task {task} operation {operation}",
                target.lock_key
            ),
            None => format!(
                "{repository_id}: the cooperative branch ownership {} is not owned by this delivery",
                target.lock_key
            ),
        }));
    }
    revalidate_target(repo, target)
}

fn revalidate_target(
    repo: &RepoTarget,
    target: &DeliveryTargetPlan,
) -> Result<DispatchRevalidation> {
    let actual = match resolve_ref(repo, &target.target_ref) {
        Ok(actual) => actual,
        Err(error) => {
            return Ok(DispatchRevalidation::Blocked(format!(
                "{}: cannot resolve the target reference: {error:#}",
                target.repository_id
            )))
        }
    };
    if actual != target.old_oid {
        return Ok(DispatchRevalidation::ReintegrationNeeded(format!(
            "{}: target branch {} moved from the planned {} to {actual}; the approved plan is stale and requires a new integration",
            target.repository_id, target.target_branch, target.old_oid
        )));
    }
    let checkouts = match current_target_checkouts(&repo.repo_root, &target.target_ref) {
        Ok(checkouts) => checkouts,
        Err(error) => {
            return Ok(DispatchRevalidation::Blocked(format!(
                "{}: {error}",
                target.repository_id
            )))
        }
    };
    match &target.destination {
        DeliveryDestination::RefOnly => {
            if let Some(entry) = checkouts.first() {
                return Ok(DispatchRevalidation::Blocked(format!(
                    "{}: the target branch is now checked out at {}; re-run the preflight before delivering so the working tree is not bypassed",
                    target.repository_id,
                    entry.path.display()
                )));
            }
        }
        DeliveryDestination::CheckedOut { path } => {
            if checkouts.len() != 1 {
                return Ok(DispatchRevalidation::Blocked(format!(
                    "{}: expected one checkout of {} at {}, found {}",
                    target.repository_id,
                    target.target_branch,
                    path.display(),
                    checkouts.len()
                )));
            }
            let entry = &checkouts[0];
            let planned = path.canonicalize().unwrap_or_else(|_| path.clone());
            let current = entry
                .path
                .canonicalize()
                .unwrap_or_else(|_| entry.path.clone());
            if current != planned {
                return Ok(DispatchRevalidation::Blocked(format!(
                    "{}: the target checkout moved from {} to {}",
                    target.repository_id,
                    path.display(),
                    current.display()
                )));
            }
            if let Err(reason) = validate_delivery_checkout(
                &DeliveryTargetPlan {
                    old_oid: target.old_oid.clone(),
                    new_oid: target.old_oid.clone(),
                    ..target.clone()
                },
                entry,
            ) {
                // A clean checkout at the captured baseline is required;
                // the temporary plan above only rewrites the HEAD target.
                return Ok(DispatchRevalidation::Blocked(format!(
                    "{}: {reason}",
                    target.repository_id
                )));
            }
        }
    }
    Ok(DispatchRevalidation::Ready)
}

struct PreparationEvidence {
    event_id: String,
    verified: Vec<VerifiedIntegration>,
}

struct DecisionEvidence {
    event_id: String,
    stage: ReviewStage,
}

struct TargetWork {
    repo: RepoTarget,
    entry: VerifiedIntegration,
    branch: String,
    target_ref: String,
    common: PathBuf,
    key: String,
    lock_operation_id: String,
    destination: DeliveryDestination,
}

/// Canonical identity of the repository's common Git directory.
pub fn canonical_common_dir(repo_root: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(git(
        repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    path.canonicalize().with_context(|| {
        format!(
            "Cannot canonicalize the git common directory of {}",
            repo_root.display()
        )
    })
}

/// Cooperative ownership key for one branch.
pub fn branch_lock_key(common: &Path, target_ref: &str) -> String {
    format!("branch:{}:{}", common.display(), target_ref)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn blocked(reason: impl Into<String>) -> DeliveryPreflight {
    DeliveryPreflight::Blocked {
        reason: reason.into(),
    }
}

fn validate_target_branch(repo: &RepoTarget) -> std::result::Result<(String, String), String> {
    let branch = repo.target_branch.trim();
    if branch.is_empty() {
        return Err(format!(
            "{}: the task has no target branch; delivery never defaults to main",
            repo.repository_id
        ));
    }
    if branch != repo.target_branch {
        return Err(format!(
            "{}: the target branch {:?} has surrounding whitespace",
            repo.repository_id, repo.target_branch
        ));
    }
    if branch == "HEAD" || branch.starts_with("refs/") {
        return Err(format!(
            "{}: {:?} is not a plain branch name",
            repo.repository_id, branch
        ));
    }
    let output =
        command(&repo.repo_root, &["check-ref-format", "--branch", branch]).map_err(|error| {
            format!(
                "{}: cannot validate the target branch: {error:#}",
                repo.repository_id
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "{}: {:?} is not a valid Git branch name",
            repo.repository_id, branch
        ));
    }
    Ok((branch.to_owned(), format!("refs/heads/{branch}")))
}

#[derive(Debug, Default, Clone)]
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
    locked: Option<String>,
    prunable: bool,
    bare: bool,
}

/// Every worktree entry Git reports, parsed from NUL-separated porcelain so a
/// path with spaces (or other special characters) is not split.
fn worktree_entries(repo_root: &Path) -> Result<Vec<WorktreeEntry>> {
    let output = command(repo_root, &["worktree", "list", "--porcelain", "-z"])?;
    anyhow::ensure!(
        output.status.success(),
        "Cannot list the worktrees of {}: {}",
        repo_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text =
        String::from_utf8(output.stdout).context("Git worktree output is not valid UTF-8")?;
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in text.split('\0') {
        if field.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                ..WorktreeEntry::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(branch) = field.strip_prefix("branch ") {
            entry.branch = Some(branch.to_owned());
        } else if field == "locked" || field.starts_with("locked ") {
            entry.locked = Some(
                field
                    .strip_prefix("locked")
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            );
        } else if field == "prunable" || field.starts_with("prunable ") {
            entry.prunable = true;
        } else if field == "bare" {
            entry.bare = true;
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    Ok(entries)
}

/// Finds the real checkout of the target branch across every linked worktree.
fn discover_checkout(
    repo: &RepoTarget,
    common: &Path,
    target_branch: &str,
    target_ref: &str,
    expected_head: &str,
) -> std::result::Result<DeliveryDestination, String> {
    let entries = worktree_entries(&repo.repo_root).map_err(|error| {
        format!(
            "{}: cannot read the Git worktree list: {error:#}",
            repo.repository_id
        )
    })?;
    let matching: Vec<&WorktreeEntry> = entries
        .iter()
        .filter(|entry| !entry.bare && entry.branch.as_deref() == Some(target_ref))
        .collect();
    let Some(entry) = matching.first() else {
        return Ok(DeliveryDestination::RefOnly);
    };
    if matching.len() > 1 {
        return Err(format!(
            "{}: target branch {target_branch} is checked out in more than one worktree; resolve that before delivery",
            repo.repository_id
        ));
    }
    if let Some(reason) = &entry.locked {
        return Err(format!(
            "{}: the worktree {} that has {target_branch} checked out is locked{}; unlock it before delivery",
            repo.repository_id,
            entry.path.display(),
            if reason.is_empty() {
                String::new()
            } else {
                format!(" ({reason})")
            }
        ));
    }
    if entry.prunable {
        return Err(format!(
            "{}: the worktree {} that has {target_branch} checked out is prunable; repair it before delivery",
            repo.repository_id,
            entry.path.display()
        ));
    }
    if !entry.path.is_dir() {
        return Err(format!(
            "{}: the worktree {} that has {target_branch} checked out is missing; restore it before delivery",
            repo.repository_id,
            entry.path.display()
        ));
    }
    let path = entry.path.canonicalize().map_err(|error| {
        format!(
            "{}: cannot resolve the target worktree {}: {error}",
            repo.repository_id,
            entry.path.display()
        )
    })?;
    let actual_common = canonical_common_dir(&path).map_err(|error| {
        format!(
            "{}: cannot identify the target worktree {}: {error:#}",
            repo.repository_id,
            path.display()
        )
    })?;
    if actual_common != common {
        return Err(format!(
            "{}: the worktree {} belongs to {}, not to this task's repository {}",
            repo.repository_id,
            path.display(),
            actual_common.display(),
            common.display()
        ));
    }
    let current =
        command(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|error| {
            format!(
                "{}: cannot read the branch of {}: {error:#}",
                repo.repository_id,
                path.display()
            )
        })?;
    if !current.status.success() || String::from_utf8_lossy(&current.stdout).trim() != target_branch
    {
        return Err(format!(
            "{}: the worktree {} is not actually on {target_branch}",
            repo.repository_id,
            path.display()
        ));
    }
    let head = git(&path, &["rev-parse", "HEAD"]).map_err(|error| {
        format!(
            "{}: cannot read the HEAD of {}: {error:#}",
            repo.repository_id,
            path.display()
        )
    })?;
    if head != expected_head {
        return Err(format!(
            "{}: the target checkout {} is at {head} but the captured baseline is {expected_head}; it must be clean at the expected commit",
            repo.repository_id,
            path.display()
        ));
    }
    let dirty = status_entries(&path).map_err(|error| {
        format!(
            "{}: cannot read the status of {}: {error:#}",
            repo.repository_id,
            path.display()
        )
    })?;
    if !dirty.is_empty() {
        return Err(format!(
            "{}: the target checkout {} has uncommitted or untracked changes; clean it yourself before delivery (nothing was stashed, reset or checked out)",
            repo.repository_id,
            path.display()
        ));
    }
    Ok(DeliveryDestination::CheckedOut { path })
}

fn host_preparation(
    events: &[Event],
    task: &TaskRecord,
) -> std::result::Result<PreparationEvidence, String> {
    let event = events
        .iter()
        .rev()
        .find(|event| {
            event.event_type == EventType::IntegrationPrepared
                && event.spec_version == task.spec_version
                && event.review_round == task.review_round
        })
        .ok_or_else(|| {
            format!(
                "no host-verified integration preparation exists for spec version {} round {}",
                task.spec_version, task.review_round
            )
        })?;
    let verified: Vec<VerifiedIntegration> =
        serde_json::from_value(event.payload["repositories"].clone()).map_err(|error| {
            format!("the host integration preparation event is invalid: {error}")
        })?;
    if verified.is_empty() {
        return Err("the host integration preparation event lists no repository".into());
    }
    Ok(PreparationEvidence {
        event_id: event.event_id.clone(),
        verified,
    })
}

fn host_decision(
    store: &TaskStore,
    task: &TaskRecord,
    events: &[Event],
    verified: &[VerifiedIntegration],
) -> std::result::Result<DecisionEvidence, String> {
    let changed = verified.iter().any(|entry| entry.tree_changed);
    let stage = if changed {
        ReviewStage::FinalDelivery
    } else {
        ReviewStage::Implementation
    };
    let decision = events
        .iter()
        .rev()
        .find(|event| {
            event.event_type == EventType::ReviewCompleted
                && event.spec_version == task.spec_version
                && event.review_round == task.review_round
                && event.payload["stage"].as_str() == Some(stage.as_str())
        })
        .ok_or_else(|| {
            format!(
                "no host-decided {} review exists for spec version {} round {}",
                stage.as_str(),
                task.spec_version,
                task.review_round
            )
        })?;
    if decision.payload["verdict"].as_str() != Some(ReviewVerdict::Approved.as_str()) {
        return Err(format!(
            "the {} review did not approve delivery",
            stage.as_str()
        ));
    }
    let review_id = decision.payload["review_report_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "the review decision names no review report".to_owned())?;
    let harness_id = decision.payload["harness_report_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "the review decision names no harness report".to_owned())?;
    let reports = store
        .reports(&task.task_id)
        .map_err(|error| format!("cannot read the task reports: {error}"))?;
    let review_report = reports
        .iter()
        .find(|report| report.report_id == review_id)
        .ok_or_else(|| "the approved review report is missing".to_owned())?;
    let review: TaskReviewReport = crate::super_agent::model::decode_body(review_report)
        .map_err(|error| format!("the approved review report is invalid: {error:#}"))?;
    if review.spec_version != task.spec_version
        || review.round != task.review_round
        || review.stage != stage
        || review.verdict != ReviewVerdict::Approved
    {
        return Err(
            "the stored review report does not approve this exact version and stage".into(),
        );
    }
    let harness_report = reports
        .iter()
        .find(|report| report.report_id == harness_id)
        .ok_or_else(|| "the paired harness report is missing".to_owned())?;
    let harness: HarnessAnalysisReport = crate::super_agent::model::decode_body(harness_report)
        .map_err(|error| format!("the paired harness report is invalid: {error:#}"))?;
    if harness.spec_version != task.spec_version
        || harness.round != task.review_round
        || harness.stage != stage
    {
        return Err(
            "the paired harness report does not belong to this exact version and stage".into(),
        );
    }
    for entry in verified {
        // A changed integrated tree was reviewed as the integrated commit. An
        // unchanged tree reuses the implementation review of the reviewed
        // commit; no FINAL_DELIVERY report is fabricated for it.
        let expected = if changed {
            &entry.integrated_commit
        } else {
            &entry.reviewed_commit
        };
        let approved = review
            .reviewed_versions
            .iter()
            .find(|version| version.repository_id == entry.repository_id)
            .and_then(|version| version.commit.as_deref())
            .map(str::trim)
            .filter(|commit| !commit.is_empty());
        if approved != Some(expected.as_str()) {
            return Err(format!(
                "the approved {} review covers {:?} for {} but delivery would carry {}",
                stage.as_str(),
                approved,
                entry.repository_id,
                expected
            ));
        }
    }
    Ok(DecisionEvidence {
        event_id: decision.event_id.clone(),
        stage,
    })
}

fn delivery_fingerprint(verified: &[VerifiedIntegration], decision_event_id: &str) -> String {
    let mut entries: Vec<serde_json::Value> = verified
        .iter()
        .map(|entry| {
            serde_json::json!({
                "repository_id": entry.repository_id,
                "target_branch": entry.target_branch,
                "target_ref": entry.target_ref,
                "baseline": entry.baseline_commit,
                "reviewed": entry.reviewed_commit,
                "integrated": entry.integrated_commit,
                "tree": entry.integrated_tree,
            })
        })
        .collect();
    entries.sort_by(|left, right| {
        left["repository_id"]
            .as_str()
            .cmp(&right["repository_id"].as_str())
    });
    let bytes = serde_json::to_vec(&serde_json::json!({
        "repositories": entries,
        "decision": decision_event_id,
    }))
    .unwrap_or_default();
    sha256_hex(&bytes)
}

fn delivery_operation_id(task: &TaskRecord, fingerprint: &str) -> String {
    format!(
        "super-agent.delivery.{}.s{}.r{}.{}",
        task.task_id,
        task.spec_version,
        task.review_round,
        &fingerprint[..24]
    )
}

fn release_pairs(
    store: &TaskStore,
    task_id: &str,
    pairs: &[(String, String)],
) -> (Vec<String>, Vec<String>) {
    let mut released = Vec::new();
    let mut failures = Vec::new();
    for (key, operation_id) in pairs {
        match store.unlock(key, task_id, operation_id) {
            Ok(true) => released.push(key.clone()),
            Ok(false) => {}
            Err(error) => failures.push(format!("{key}: {error}")),
        }
    }
    (released, failures)
}

fn release_note(released: &[String], failures: &[String]) -> String {
    let mut note = String::new();
    if !released.is_empty() {
        note.push_str(&format!("; released locks {:?}", released));
    }
    if !failures.is_empty() {
        note.push_str(&format!(
            "; releasing locks also failed: {}",
            failures.join("; ")
        ));
    }
    note
}

/// A failed attempt consumed its lock operation IDs. The immutable content
/// fingerprint and plan operation ID stay the same; only the durable attempt
/// number advances, so a retry acquires fresh operation IDs instead of being
/// stuck on a released one.
fn advance_attempt_note(store: &TaskStore, task_id: &str, fingerprint: &str) -> String {
    match store.advance_delivery_attempt(task_id, fingerprint) {
        Ok(attempt) => format!("; the next prepare uses delivery attempt {attempt}"),
        Err(error) => format!("; advancing the delivery attempt also failed: {error}"),
    }
}

/// Acquires every lock in the given deterministic order. On failure only the
/// locks this attempt newly acquired are released; ownership recorded by an
/// earlier attempt of the same delivery operation is preserved.
fn acquire_locks(
    store: &TaskStore,
    task_id: &str,
    pairs: &[(String, String)],
) -> std::result::Result<Vec<(String, String)>, String> {
    let mut newly = Vec::new();
    for (key, operation_id) in pairs {
        match store.try_lock(operation_id, key, task_id) {
            Ok(LockOutcome::Acquired) => newly.push((key.clone(), operation_id.clone())),
            Ok(LockOutcome::AlreadyOwned) => {}
            Ok(LockOutcome::Released) => {
                let (released, failures) = release_pairs(store, task_id, &newly);
                return Err(format!(
                    "cooperative ownership for {key} was released by an earlier attempt; a new delivery attempt is required{}",
                    release_note(&released, &failures)
                ));
            }
            Err(StoreError::LockBusy(conflict)) => {
                let (released, failures) = release_pairs(store, task_id, &newly);
                return Err(format!(
                    "target branch ownership {key} is held by another delivery ({conflict}){}",
                    release_note(&released, &failures)
                ));
            }
            Err(error) => {
                let (released, failures) = release_pairs(store, task_id, &newly);
                return Err(format!(
                    "cannot acquire target branch ownership {key}: {error}{}",
                    release_note(&released, &failures)
                ));
            }
        }
    }
    Ok(newly)
}

/// Releases exactly the locks recorded in the plan, matching task and owner
/// operation. It never scans or deletes another operation's lock.
pub fn release_delivery(store: &TaskStore, plan: &DeliveryPlan) -> DeliveryRelease {
    let pairs: Vec<(String, String)> = plan
        .repositories
        .iter()
        .map(|target| (target.lock_key.clone(), target.lock_operation_id.clone()))
        .collect();
    let (released, failures) = release_pairs(store, &plan.task_id, &pairs);
    DeliveryRelease { released, failures }
}

/// Builds, verifies and persists the delivery plan without changing Git.
///
/// Domain failures are returned as typed `Blocked`/`ReintegrationNeeded`
/// values. Store failures are returned as errors because the host cannot claim
/// any durable fact when the database did not accept it.
pub fn prepare_delivery(store: &TaskStore, task: &TaskRecord) -> Result<DeliveryPreflight> {
    prepare_delivery_observed(store, task, &mut || {})
}

/// Same as [`prepare_delivery`] with a hook that runs after cooperative
/// ownership was acquired and before the under-lock re-check. Production passes
/// a no-op; tests use it to model an external process moving the target or a
/// failure in that window.
pub fn prepare_delivery_observed<F: FnMut()>(
    store: &TaskStore,
    task: &TaskRecord,
    after_locks: &mut F,
) -> Result<DeliveryPreflight> {
    if task.repositories.is_empty() {
        return Ok(DeliveryPreflight::NotApplicable);
    }
    let mut ids = BTreeSet::new();
    for repo in &task.repositories {
        if !ids.insert(repo.repository_id.as_str()) {
            return Ok(blocked(format!(
                "the task lists repository {} more than once",
                repo.repository_id
            )));
        }
    }

    let events = store.events(&task.task_id)?;
    let preparation = match host_preparation(&events, task) {
        Ok(preparation) => preparation,
        Err(reason) => return Ok(blocked(reason)),
    };
    let decision = match host_decision(store, task, &events, &preparation.verified) {
        Ok(decision) => decision,
        Err(reason) => return Ok(blocked(reason)),
    };
    let fingerprint = delivery_fingerprint(&preparation.verified, &decision.event_id);
    let operation_id = delivery_operation_id(task, &fingerprint);

    let mut targets: Vec<TargetWork> = Vec::new();
    for repo in &task.repositories {
        let Some(entry) = preparation
            .verified
            .iter()
            .find(|entry| entry.repository_id == repo.repository_id)
        else {
            return Ok(blocked(format!(
                "{}: the host-verified integration does not describe this repository",
                repo.repository_id
            )));
        };
        let (branch, target_ref) = match validate_target_branch(repo) {
            Ok(value) => value,
            Err(reason) => return Ok(blocked(reason)),
        };
        let common = match canonical_common_dir(&repo.repo_root) {
            Ok(common) => common,
            Err(error) => {
                return Ok(blocked(format!(
                    "{}: cannot identify its Git common directory: {error:#}",
                    repo.repository_id
                )))
            }
        };
        let recorded = repo
            .git_common_dir
            .canonicalize()
            .unwrap_or_else(|_| repo.git_common_dir.clone());
        if common != recorded {
            return Ok(blocked(format!(
                "{}: the repository at {} belongs to {} but the task recorded {}; refusing a foreign repository",
                repo.repository_id,
                repo.repo_root.display(),
                common.display(),
                recorded.display()
            )));
        }
        if entry.target_branch != branch || entry.target_ref != target_ref {
            return Ok(blocked(format!(
                "{}: the host-verified integration names {:?} / {:?} but the task targets {:?} / {:?}",
                repo.repository_id,
                entry.target_branch,
                entry.target_ref,
                branch,
                target_ref
            )));
        }
        targets.push(TargetWork {
            repo: repo.clone(),
            entry: entry.clone(),
            branch,
            target_ref: target_ref.clone(),
            key: branch_lock_key(&common, &target_ref),
            common,
            lock_operation_id: String::new(),
            destination: DeliveryDestination::RefOnly,
        });
    }
    targets.sort_by(|left, right| left.repo.repository_id.cmp(&right.repo.repository_id));

    let mut keys = BTreeSet::new();
    for target in &targets {
        if !keys.insert(target.key.clone()) {
            return Ok(blocked(format!(
                "two task repositories resolve to the same branch ownership {}",
                target.key
            )));
        }
    }
    // The durable plan for this immutable content identity, if one exists. Its
    // lock identities belong to the successful attempt and must be reused.
    let existing_plan = store.delivery_plan(&task.task_id, &operation_id)?;
    if let Some(plan) = &existing_plan {
        let planned: BTreeSet<&str> = plan
            .repositories
            .iter()
            .map(|target| target.repository_id.as_str())
            .collect();
        let current: BTreeSet<&str> = task
            .repositories
            .iter()
            .map(|repo| repo.repository_id.as_str())
            .collect();
        if planned != current {
            return Ok(blocked(format!(
                "the existing delivery plan names a different repository set than the task"
            )));
        }
    }
    // Deterministic acquisition order by canonical lock key, independent of
    // the task's repository order.
    let mut sorted_targets: Vec<&TargetWork> = targets.iter().collect();
    sorted_targets.sort_by(|left, right| left.key.cmp(&right.key));
    // A new plan records this attempt; a loaded plan keeps the attempt that is
    // already bound to its lock identities.
    let current_attempt = if existing_plan.is_some() {
        0
    } else {
        store.current_delivery_attempt(&task.task_id, &fingerprint)?
    };
    let mut lock_operations: BTreeMap<String, (String, String)> = BTreeMap::new();
    if let Some(existing) = &existing_plan {
        for target in &existing.repositories {
            lock_operations.insert(
                target.repository_id.clone(),
                (target.lock_key.clone(), target.lock_operation_id.clone()),
            );
        }
    } else {
        for (index, target) in sorted_targets.iter().enumerate() {
            lock_operations.insert(
                target.repo.repository_id.clone(),
                (
                    target.key.clone(),
                    format!("{operation_id}.attempt.{current_attempt}.lock.{index}"),
                ),
            );
        }
    }
    for target in &mut targets {
        let (key, operation) = lock_operations
            .get(&target.repo.repository_id)
            .cloned()
            .expect("every repository was assigned an ownership identity");
        if key != target.key {
            return Ok(blocked(format!(
                "{}: the existing delivery plan uses ownership key {key}, but the target now resolves to {}",
                target.repo.repository_id, target.key
            )));
        }
        target.lock_operation_id = operation;
    }
    let mut pairs: Vec<(String, String)> = targets
        .iter()
        .map(|target| (target.key.clone(), target.lock_operation_id.clone()))
        .collect();
    pairs.sort_by(|left, right| left.0.cmp(&right.0));

    let mut stale = Vec::new();
    for target in &mut targets {
        let actual = match resolve_ref(&target.repo, &target.target_ref) {
            Ok(actual) => actual,
            Err(error) => {
                return Ok(blocked(format!(
                    "{}: cannot resolve the target reference: {error:#}",
                    target.repo.repository_id
                )))
            }
        };
        if actual != target.entry.baseline_commit {
            stale.push(format!(
                "{}: target branch {} moved from the captured baseline {} to {actual}",
                target.repo.repository_id, target.branch, target.entry.baseline_commit
            ));
            continue;
        }
        if target.repo.integration_status != IntegrationStatus::Prepared {
            return Ok(blocked(format!(
                "{}: the task records integration status {} instead of PREPARED",
                target.repo.repository_id,
                target.repo.integration_status.as_str()
            )));
        }
        if target.repo.integrated_commit.as_deref() != Some(target.entry.integrated_commit.as_str())
            || target.repo.reviewed_commit.as_deref() != Some(target.entry.reviewed_commit.as_str())
        {
            return Ok(blocked(format!(
                "{}: the stored reviewed/integrated commits do not match the host-verified preparation",
                target.repo.repository_id
            )));
        }
        let integrated = match resolve_ref(&target.repo, &target.entry.integrated_commit) {
            Ok(integrated) => integrated,
            Err(error) => {
                return Ok(blocked(format!(
                    "{}: cannot resolve the integrated commit {}: {error:#}",
                    target.repo.repository_id, target.entry.integrated_commit
                )))
            }
        };
        if integrated != target.entry.integrated_commit {
            return Ok(blocked(format!(
                "{}: the integrated commit {:?} does not resolve to the exact recorded object {integrated}",
                target.repo.repository_id, target.entry.integrated_commit
            )));
        }
        let tree = match tree_of(&target.repo.repo_root, &integrated) {
            Ok(tree) => tree,
            Err(error) => {
                return Ok(blocked(format!(
                    "{}: cannot read the integrated tree: {error:#}",
                    target.repo.repository_id
                )))
            }
        };
        if tree != target.entry.integrated_tree {
            return Ok(blocked(format!(
                "{}: the integrated tree {tree} is not the host-verified tree {}",
                target.repo.repository_id, target.entry.integrated_tree
            )));
        }
        for (label, ancestor) in [
            ("reviewed", &target.entry.reviewed_commit),
            ("baseline", &target.entry.baseline_commit),
        ] {
            if !is_ancestor(&target.repo.repo_root, ancestor, &integrated).unwrap_or(false) {
                return Ok(blocked(format!(
                    "{}: the {label} commit {ancestor} is not an ancestor of the integrated commit {integrated}",
                    target.repo.repository_id
                )));
            }
        }
        target.destination = match discover_checkout(
            &target.repo,
            &target.common,
            &target.branch,
            &target.target_ref,
            &target.entry.baseline_commit,
        ) {
            Ok(destination) => destination,
            Err(reason) => return Ok(blocked(reason)),
        };
    }
    let has_existing = existing_plan.is_some();
    if !stale.is_empty() {
        let (released, failures) = release_pairs(store, &task.task_id, &pairs);
        return Ok(DeliveryPreflight::ReintegrationNeeded {
            reason: format!(
                "{}; the prepared delivery was not recorded or refreshed.{}",
                stale.join("; "),
                release_note(&released, &failures)
            ),
        });
    }

    let newly = match acquire_locks(store, &task.task_id, &pairs) {
        Ok(newly) => newly,
        Err(reason) => {
            // Without a persisted plan every lock belongs to this attempt, so
            // abandoning it releases the whole set. With an existing plan only
            // ownership acquired in this call is released and the plan's
            // earlier ownership is preserved.
            let release_targets: &[(String, String)] = if has_existing { &[] } else { &pairs };
            let (released, failures) = release_pairs(store, &task.task_id, release_targets);
            let advance = if has_existing {
                String::new()
            } else {
                advance_attempt_note(store, &task.task_id, &fingerprint)
            };
            return Ok(blocked(format!(
                "{reason}{}{advance}",
                release_note(&released, &failures)
            )));
        }
    };
    let abandon = || {
        let release_targets: &[(String, String)] = if has_existing { &newly } else { &pairs };
        let (released, failures) = release_pairs(store, &task.task_id, release_targets);
        let mut note = release_note(&released, &failures);
        if !has_existing {
            note.push_str(&advance_attempt_note(store, &task.task_id, &fingerprint));
        }
        note
    };

    // Re-check under ownership: a cooperative lock cannot stop an external Git
    // command, and a checkout may have become dirty while the plan was built.
    after_locks();
    for target in &targets {
        let actual = match resolve_ref(&target.repo, &target.target_ref) {
            Ok(actual) => actual,
            Err(error) => {
                return Ok(blocked(format!(
                    "{}: cannot resolve the target reference under ownership: {error:#}{}",
                    target.repo.repository_id,
                    abandon()
                )))
            }
        };
        if actual != target.entry.baseline_commit {
            let (released, failures) = release_pairs(store, &task.task_id, &pairs);
            let advance = if has_existing {
                String::new()
            } else {
                advance_attempt_note(store, &task.task_id, &fingerprint)
            };
            return Ok(DeliveryPreflight::ReintegrationNeeded {
                reason: format!(
                    "{}: target branch {} moved to {actual} while the delivery plan was being prepared; the approved integration is stale.{}{advance}",
                    target.repo.repository_id,
                    target.branch,
                    release_note(&released, &failures)
                ),
            });
        }
        match discover_checkout(
            &target.repo,
            &target.common,
            &target.branch,
            &target.target_ref,
            &target.entry.baseline_commit,
        ) {
            Ok(destination) if destination == target.destination => {}
            Ok(_) => {
                return Ok(blocked(format!(
                    "{}: the target checkout changed while the delivery plan was being prepared; re-run the preflight{}",
                    target.repo.repository_id,
                    abandon()
                )))
            }
            Err(reason) => return Ok(blocked(format!("{reason}{}", abandon()))),
        }
    }

    let plan = DeliveryPlan {
        operation_id,
        task_id: task.task_id.clone(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        evidence_event_id: preparation.event_id,
        review_event_id: decision.event_id,
        content_fingerprint: fingerprint.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        repositories: targets
            .iter()
            .map(|target| DeliveryTargetPlan {
                repository_id: target.repo.repository_id.clone(),
                repo_root: target.repo.repo_root.clone(),
                git_common_dir: target.common.clone(),
                target_branch: target.branch.clone(),
                target_ref: target.target_ref.clone(),
                destination: target.destination.clone(),
                baseline_commit: target.entry.baseline_commit.clone(),
                reviewed_commit: target.entry.reviewed_commit.clone(),
                integrated_commit: target.entry.integrated_commit.clone(),
                integrated_tree: target.entry.integrated_tree.clone(),
                old_oid: target.entry.baseline_commit.clone(),
                new_oid: target.entry.integrated_commit.clone(),
                review_stage: decision.stage,
                lock_key: target.key.clone(),
                lock_operation_id: target.lock_operation_id.clone(),
                attempt: existing_plan
                    .as_ref()
                    .and_then(|plan| {
                        plan.repositories
                            .iter()
                            .find(|planned| planned.repository_id == target.repo.repository_id)
                    })
                    .map(|planned| planned.attempt)
                    .unwrap_or(current_attempt),
            })
            .collect(),
    };
    if let Some(existing) = &existing_plan {
        if existing.same_facts(&plan) {
            return Ok(DeliveryPreflight::Planned(existing.clone()));
        }
        return Ok(blocked(format!(
            "a delivery plan for operation {} already exists with different facts; refusing to refresh it under the same approval{}",
            plan.operation_id,
            abandon()
        )));
    }
    match store.save_delivery_plan(&plan) {
        Ok(()) => Ok(DeliveryPreflight::Planned(plan)),
        Err(StoreError::Invalid(reason)) => Ok(blocked(format!("{reason}{}", abandon()))),
        Err(error) => Err(anyhow::Error::new(error).context(abandon())),
    }
}

/// Validates one worktree that currently has the target branch checked out.
/// Every fact is read from Git and the filesystem at verification time; the
/// persisted destination is not trusted on its own.
fn validate_delivery_checkout(
    target: &DeliveryTargetPlan,
    entry: &WorktreeEntry,
) -> std::result::Result<PathBuf, String> {
    if let Some(reason) = &entry.locked {
        return Err(format!(
            "the target checkout {} is locked{}; the delivered state cannot be verified",
            entry.path.display(),
            if reason.is_empty() {
                String::new()
            } else {
                format!(" ({reason})")
            }
        ));
    }
    if entry.prunable {
        return Err(format!(
            "the target checkout {} is prunable; the delivered state cannot be verified",
            entry.path.display()
        ));
    }
    if !entry.path.is_dir() {
        return Err(format!(
            "the target checkout {} is missing; the delivered state cannot be verified",
            entry.path.display()
        ));
    }
    let canonical = entry.path.canonicalize().map_err(|error| {
        format!(
            "cannot resolve the target checkout {}: {error}",
            entry.path.display()
        )
    })?;
    let common = canonical_common_dir(&canonical).map_err(|error| {
        format!(
            "cannot identify the target checkout {}: {error:#}",
            canonical.display()
        )
    })?;
    if common != target.git_common_dir {
        return Err(format!(
            "the target checkout {} belongs to {}, not to the planned repository {}",
            canonical.display(),
            common.display(),
            target.git_common_dir.display()
        ));
    }
    let current =
        command(&canonical, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|error| {
            format!(
                "cannot read the branch of {}: {error:#}",
                canonical.display()
            )
        })?;
    if !current.status.success()
        || String::from_utf8_lossy(&current.stdout).trim() != target.target_branch
    {
        return Err(format!(
            "the target checkout {} is not on {}",
            canonical.display(),
            target.target_branch
        ));
    }
    let head = git(&canonical, &["rev-parse", "HEAD"])
        .map_err(|error| format!("cannot read the HEAD of {}: {error:#}", canonical.display()))?;
    if head != target.new_oid {
        return Err(format!(
            "the checkout {} is at {head}, not at the approved integrated commit {}",
            canonical.display(),
            target.new_oid
        ));
    }
    let dirty = status_entries(&canonical).map_err(|error| {
        format!(
            "cannot read the status of {}: {error:#}",
            canonical.display()
        )
    })?;
    if !dirty.is_empty() {
        return Err(format!(
            "the checkout {} still has uncommitted or untracked changes after delivery ({})",
            canonical.display(),
            dirty.join(", ")
        ));
    }
    Ok(canonical)
}

/// Rediscovers which worktrees currently have the target branch checked out.
/// A stale `RefOnly` plan cannot skip this: a checkout may have appeared after
/// the plan was persisted.
fn current_target_checkouts(
    repo_root: &Path,
    target_ref: &str,
) -> std::result::Result<Vec<WorktreeEntry>, String> {
    let entries = worktree_entries(repo_root)
        .map_err(|error| format!("cannot read the Git worktree list: {error:#}"))?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.bare && entry.branch.as_deref() == Some(target_ref))
        .collect())
}

/// Decides whether the current checkout topology proves a delivered working
/// tree. `Ok(None)` means the target is genuinely not checked out. Any other
/// unprovable or inconsistent topology is an explicit failure.
fn verify_current_checkout(
    target: &DeliveryTargetPlan,
    repo: &RepoTarget,
) -> std::result::Result<Option<PathBuf>, String> {
    let mut matches = current_target_checkouts(&repo.repo_root, &target.target_ref)?;
    if matches.len() > 1 {
        let paths: Vec<String> = matches
            .iter()
            .map(|entry| entry.path.display().to_string())
            .collect();
        return Err(format!(
            "the target branch {} is checked out in {} worktrees ({}); the delivered checkout is ambiguous",
            target.target_branch,
            matches.len(),
            paths.join(", ")
        ));
    }
    match &target.destination {
        DeliveryDestination::RefOnly => match matches.pop() {
            Some(entry) => validate_delivery_checkout(target, &entry).map(Some),
            None => Ok(None),
        },
        DeliveryDestination::CheckedOut { path: planned } => {
            let planned_canonical = planned.canonicalize().ok();
            let Some(entry) = matches.pop() else {
                return Err(if planned_canonical.is_some() {
                    format!(
                        "the planned target checkout {} is no longer on {}; the delivered working tree cannot be verified",
                        planned.display(),
                        target.target_branch
                    )
                } else {
                    format!(
                        "the planned target checkout {} is missing or was moved; the delivered working tree cannot be verified",
                        planned.display()
                    )
                });
            };
            let actual = entry
                .path
                .canonicalize()
                .unwrap_or_else(|_| entry.path.clone());
            if planned_canonical.as_deref() != Some(actual.as_path()) {
                return Err(format!(
                    "the target branch is now checked out at {} instead of the planned checkout {}; the planned checkout moved or was replaced",
                    actual.display(),
                    planned.display()
                ));
            }
            validate_delivery_checkout(target, &entry).map(Some)
        }
    }
}

/// Host verification after a delivery step. It reads the real target reference
/// and the current worktree topology on every call; no agent report and no
/// stale preflight classification is an input. A partial multi-repository
/// result keeps every verified repository and names the ones still pending.
pub fn verify_delivery(task: &TaskRecord, plan: &DeliveryPlan) -> Result<DeliveryVerification> {
    anyhow::ensure!(
        plan.task_id == task.task_id,
        "The delivery plan belongs to another task"
    );
    anyhow::ensure!(
        plan.spec_version == task.spec_version && plan.review_round == task.review_round,
        "The delivery plan belongs to spec version {} round {}, but the task is spec version {} round {}; a stale approval cannot verify a delivery",
        plan.spec_version,
        plan.review_round,
        task.spec_version,
        task.review_round
    );
    let planned: BTreeSet<&str> = plan
        .repositories
        .iter()
        .map(|target| target.repository_id.as_str())
        .collect();
    let current: BTreeSet<&str> = task
        .repositories
        .iter()
        .map(|repo| repo.repository_id.as_str())
        .collect();
    anyhow::ensure!(
        planned == current,
        "The delivery plan repository set {planned:?} does not match the task's {current:?}"
    );
    let mut repositories = Vec::new();
    let mut pending = Vec::new();
    let mut failures = Vec::new();
    for target in &plan.repositories {
        let Some(repo) = task
            .repositories
            .iter()
            .find(|repo| repo.repository_id == target.repository_id)
        else {
            let reason = "the task no longer lists this repository".to_owned();
            repositories.push(DeliveryTargetVerification {
                repository_id: target.repository_id.clone(),
                expected_commit: target.new_oid.clone(),
                actual_commit: None,
                verified: false,
                reason: reason.clone(),
            });
            pending.push(target.repository_id.clone());
            failures.push(format!("{}: {reason}", target.repository_id));
            continue;
        };
        let mut reason = String::new();
        let mut actual = None;
        if let Ok(common) = canonical_common_dir(&repo.repo_root) {
            if common != target.git_common_dir {
                reason = format!(
                    "the repository identity changed to {}; expected {}",
                    common.display(),
                    target.git_common_dir.display()
                );
            }
        } else {
            reason = "the repository is no longer readable".to_owned();
        }
        if reason.is_empty() {
            match resolve_ref(repo, &target.target_ref) {
                Ok(oid) => {
                    if oid == target.new_oid {
                        actual = Some(oid);
                    } else {
                        reason = format!(
                            "the target branch is at {oid} but the approved integrated commit is {}",
                            target.new_oid
                        );
                        actual = Some(oid);
                    }
                }
                Err(error) => reason = format!("cannot resolve the target reference: {error:#}"),
            }
        }
        if reason.is_empty() {
            if let Err(error) = verify_current_checkout(target, repo) {
                reason = error;
            }
        }
        let verified = reason.is_empty();
        if !verified {
            pending.push(target.repository_id.clone());
            failures.push(format!("{}: {reason}", target.repository_id));
        }
        repositories.push(DeliveryTargetVerification {
            repository_id: target.repository_id.clone(),
            expected_commit: target.new_oid.clone(),
            actual_commit: actual,
            verified,
            reason,
        });
    }
    Ok(DeliveryVerification {
        complete: failures.is_empty() && !repositories.is_empty(),
        repositories,
        pending,
        failures,
    })
}

#[cfg(test)]
mod tests;
