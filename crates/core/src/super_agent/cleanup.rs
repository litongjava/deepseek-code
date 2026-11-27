//! Restartable, host-owned cleanup for a completed single Git repository.
//! No inferred ownership from absence: a durable removal intent is required.
//! External Git writers do not honor our lock; every destructive step therefore
//! rechecks the assets, and Git itself must accept a non-forced removal.
use crate::db::session_store::TaskState;
use super::{
    delivery::{self, DeliveryPlan},
    launcher::Launcher,
    model::{CleanupStatus, EventType, NewEvent, RepoTarget, Status, TaskRecord},
    state::StatePolicy,
    store::TaskStore,
};
use crate::{
    services::background_sessions::{self, JobSnapshot},
    worktree::session::{command, git},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    version: u32,
    task_id: String,
    spec_version: u32,
    round: u32,
    repository: RepoTarget,
    plan_operation: String,
    completion_event: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Archive {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Operation {
    identity: Identity,
    operation_id: String,
    reports: Archive,
    diff: Archive,
    // Intent is synced to SQLite before each Git command. A crash after a Git
    // success but before the following save resumes from this ownership proof.
    worktree_intent: bool,
    worktree_removed: bool,
    branch_intent: bool,
    branch_removed: bool,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct CleanupLock(fs::File);
impl Drop for CleanupLock {
    fn drop(&mut self) {
        let _ = crate::fs::unlock_file(&self.0);
    }
}

pub fn cleanup_completed(launcher: &Launcher, task_id: &str, policy: &StatePolicy) -> Result<()> {
    run(launcher, task_id, policy, &mut |_| Ok(()))
}

// The hook is only used by local tests to fail precisely between durable steps.
pub(crate) fn run(
    launcher: &Launcher,
    task_id: &str,
    policy: &StatePolicy,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    crate::db::session_store::without_lifecycle_wait(|| {
        run_cleanup(launcher, task_id, policy, hook)
    })
}

fn run_cleanup(
    launcher: &Launcher,
    task_id: &str,
    policy: &StatePolicy,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    let context = &launcher.context;
    let store = TaskStore::from_context(context);
    let task = store
        .get_task(task_id)?
        .context("Cleanup task disappeared")?;
    ensure!(
        task.status == Status::Completed,
        "Cleanup requires COMPLETED"
    );
    if task.cleanup_status == CleanupStatus::Completed {
        return Ok(());
    }
    // Deterministic unsupported shapes stop here, before liveness/Git/archive
    // checks. Repeated ticks only revisit this cheap persisted-shape gate.
    ensure!(task.repositories.len() == 1 && task.non_git_targets.is_empty(),
        "Only single Git repository cleanup is supported; multi-repository/non-Git cleanup is PENDING");
    let directory = store.data_dir().join("tasks/super-agent").join(task_id);
    fs::create_dir_all(&directory)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("cleanup.lock"))?;
    crate::fs::try_lock_file(&lock)
        .map_err(|e| anyhow::anyhow!("Cleanup is already owned: {e}"))?;
    let _lock = CleanupLock(lock);
    // Another host may have finished while we acquired the file lock.
    let task = store
        .get_task(task_id)?
        .context("Cleanup task disappeared")?;
    ensure!(
        task.status == Status::Completed,
        "Cleanup requires COMPLETED"
    );
    if task.cleanup_status == CleanupStatus::Completed {
        return Ok(());
    }
    store.validate_cleanup_evidence(task_id)?;
    let events = store.events(task_id)?;
    let completed = events
        .iter()
        .find(|event| {
            event.processed
                && event.event_type == EventType::TaskCompleted
                && event.spec_version == task.spec_version
                && event.review_round == task.review_round
        })
        .context("No current host completion event")?;
    let plan_id = completed.payload["plan_operation_id"]
        .as_str()
        .context("Completion event has no delivery plan identity")?;
    let plan = store
        .delivery_plan(task_id, plan_id)?
        .context("Completion plan missing")?;
    if let Some(reason) = delivery::verify_plan_binding(&store, &task, &plan)? {
        anyhow::bail!(reason);
    }
    ensure!(
        store.delivery_plan_observations_complete(&plan)?,
        "Delivery observations incomplete"
    );
    let purpose = super::orchestrator::required_completion_reviewer_purpose(&events, &plan)
        .context("Completion review purpose is unknown")?;
    let members = launcher.completion_member_jobs(&task, &plan, purpose)?;
    for observation in members.unknown().chain(members.active()) {
        anyhow::bail!(
            "Cleanup cannot prove member stopped: {}",
            observation.describe()
        );
    }
    let _publication = background_sessions::job_publication_fence(&context.paths.data_dir, true)?;
    let _workspace = context
        .db
        .hold_worktree_removal(&task.repositories[0].worktree_path)?;
    let _execution_leases = descendants_stopped(launcher, &task)?;
    let verification = delivery::verify_delivery_for_cleanup(&task, &plan)?;
    ensure!(
        verification.complete,
        "Delivered targets changed: {}",
        verification.failures.join("; ")
    );
    let repo = &task.repositories[0];
    validate_paths(&task, repo, &plan)?;
    let identity = Identity {
        version: 1,
        task_id: task_id.into(),
        spec_version: task.spec_version,
        round: task.review_round,
        repository: repo.clone(),
        plan_operation: plan.operation_id.clone(),
        completion_event: completed.event_id.clone(),
    };
    let operation_id = format!(
        "cleanup.v1.{}",
        hash(&serde_json::to_vec(&serde_json::json!([
            task_id,
            task.spec_version,
            task.review_round,
            repo.repository_id
        ]))?)
    );
    let key = format!("super-agent.{operation_id}");
    let mut operation = if let Some(saved) = context.db.task_state::<Operation>(&key)? {
        ensure!(
            saved.identity == identity && saved.operation_id == operation_id,
            "Cleanup operation identity changed; refusing to adopt assets"
        );
        saved
    } else {
        validate_worktree(repo)?;
        validate_branch(repo)?;
        let index = serde_json::to_vec_pretty(&serde_json::json!({
            "identity": identity, "task": task, "plan": plan,
            "reports": store.reports(task_id)?, "events": events,
            "base_oid": repo.base_commit, "delivered_oid": repo.integrated_commit,
        }))?;
        let diff = git_bytes(
            &repo.repo_root,
            &[
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                &repo.base_commit,
                repo.integrated_commit
                    .as_deref()
                    .context("Delivered OID missing")?,
                "--",
            ],
        )?;
        let reports = Archive {
            path: store.write_artifact(task_id, &format!("{operation_id}.reports.json"), &index)?,
            sha256: hash(&index),
        };
        let diff = Archive {
            path: store.write_artifact(task_id, &format!("{operation_id}.diff"), &diff)?,
            sha256: hash(&diff),
        };
        let saved = Operation {
            identity,
            operation_id,
            reports,
            diff,
            worktree_intent: false,
            worktree_removed: false,
            branch_intent: false,
            branch_removed: false,
        };
        context.db.save_task_state(&key, &saved)?;
        saved
    };
    for (archive, suffix) in [
        (&operation.reports, "reports.json"),
        (&operation.diff, "diff"),
    ] {
        let expected = directory
            .join("artifacts")
            .join(format!("{}.{suffix}", operation.operation_id));
        ensure!(
            archive.path == expected && hash(&fs::read(&archive.path)?) == archive.sha256,
            "Cleanup archive path/hash mismatch: {}",
            archive.path.display()
        );
    }
    hook("archived")?;
    let entries = delivery::worktree_entries(&repo.repo_root)?;
    let registered = entries.iter().any(|entry| entry.path == repo.worktree_path);
    let exists = repo.worktree_path.try_exists()?;
    // symlink_metadata also detects a dangling replacement symlink.
    let present = exists || fs::symlink_metadata(&repo.worktree_path).is_ok();
    if operation.worktree_removed {
        ensure!(
            !present && !registered,
            "Removed worktree was recreated; refusing to delete it"
        );
    } else if present || registered {
        validate_worktree(repo)?;
        validate_branch(repo)?;
        operation.worktree_intent = true;
        context.db.save_task_state(&key, &operation)?;
        hook("before-worktree-remove")?;
        git(
            &repo.repo_root,
            &[
                "worktree",
                "remove",
                "--",
                repo.worktree_path.to_str().context("Non-UTF8 worktree")?,
            ],
        )?;
        hook("after-worktree-remove")?;
        operation.worktree_removed = true;
        context.db.save_task_state(&key, &operation)?;
    } else {
        ensure!(
            operation.worktree_intent,
            "Worktree missing without persisted removal intent"
        );
        operation.worktree_removed = true;
        context.db.save_task_state(&key, &operation)?;
    }
    let branch = branch_oid(repo)?;
    if operation.branch_removed {
        ensure!(
            branch.is_none(),
            "Removed task branch was recreated; refusing to delete it"
        );
    } else if branch.is_some() {
        validate_branch(repo)?;
        ensure!(
            !delivery::worktree_entries(&repo.repo_root)?
                .iter()
                .any(|entry| entry.branch.as_deref()
                    == Some(format!("refs/heads/{}", repo.task_branch).as_str())),
            "Task branch is checked out elsewhere"
        );
        operation.branch_intent = true;
        context.db.save_task_state(&key, &operation)?;
        hook("before-branch-delete")?;
        // -d normally compares against repo_root's HEAD, which need not be the
        // delivered target. Select the already verified target as the command's
        // upstream without changing persistent config or the source checkout.
        git(
            &repo.repo_root,
            &[
                "-c",
                &format!("branch.{}.remote=.", repo.task_branch),
                "-c",
                &format!(
                    "branch.{}.merge=refs/heads/{}",
                    repo.task_branch, repo.target_branch
                ),
                "branch",
                "-d",
                "--",
                &repo.task_branch,
            ],
        )?;
        hook("after-branch-delete")?;
        operation.branch_removed = true;
        context.db.save_task_state(&key, &operation)?;
    } else {
        ensure!(
            operation.branch_intent,
            "Task branch missing without persisted removal intent"
        );
        operation.branch_removed = true;
        context.db.save_task_state(&key, &operation)?;
    }
    ensure!(
        !repo.worktree_path.try_exists()?
            && fs::symlink_metadata(&repo.worktree_path).is_err()
            && !delivery::worktree_entries(&repo.repo_root)?
                .iter()
                .any(|entry| entry.path == repo.worktree_path)
            && branch_oid(repo)?.is_none(),
        "Cleanup assets were recreated before completion"
    );
    // The conditional DELETE cannot release another task's or attempt's lock.
    for target in &plan.repositories {
        store.unlock(&target.lock_key, task_id, &target.lock_operation_id)?;
    }
    store.apply_event(
        task_id,
        NewEvent {
            session_id: None,
            run_id: None,
            event_type: EventType::CleanupCompleted,
            causation_id: Some(completed.event_id.clone()),
            operation_id: operation.operation_id.clone(),
            payload: serde_json::to_value(&operation)?,
        },
        policy,
    )?;
    Ok(())
}

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()?;
    ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn validate_paths(task: &TaskRecord, repo: &RepoTarget, plan: &DeliveryPlan) -> Result<()> {
    ensure!(
        plan.repositories.len() == 1,
        "Cleanup plan is not single-repository"
    );
    let target = &plan.repositories[0];
    ensure!(
        matches!(repo.base_commit.len(), 40 | 64)
            && repo
                .base_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Base commit is not an immutable Git OID"
    );
    ensure!(
        git(
            &repo.repo_root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{commit}}", repo.base_commit)
            ]
        )? == repo.base_commit,
        "Base commit identity changed"
    );
    let common = delivery::canonical_common_dir(&repo.repo_root)?;
    ensure!(
        repo.repo_root.canonicalize()?
            == PathBuf::from(git(&repo.repo_root, &["rev-parse", "--show-toplevel"])?)
                .canonicalize()?,
        "Repository root changed"
    );
    ensure!(
        common == repo.git_common_dir
            && common == target.git_common_dir
            && repo.repo_root == target.repo_root
            && repo.task_id == task.task_id,
        "Repository/common directory identity mismatch"
    );
    ensure!(
        repo.task_branch == format!("agent/{}", task.task_id)
            && repo.task_branch != repo.target_branch,
        "Task branch is not owned by this task"
    );
    ensure!(
        repo.worktree_path == common.join("desktop-worktrees").join(&task.task_id),
        "Worktree path is not owned by this task"
    );
    ensure!(
        repo.worktree_path
            .parent()
            .context("Worktree has no parent")?
            .canonicalize()?
            == common.join("desktop-worktrees"),
        "Worktree parent is redirected"
    );
    ensure!(
        repo.worktree_path != repo.repo_root.canonicalize()?
            && repo.worktree_path != repo.target_path.canonicalize()?,
        "Cleanup cannot remove the source workspace"
    );
    ensure!(
        repo.integrated_commit.as_deref() == Some(target.new_oid.as_str()),
        "Delivered OID mismatch"
    );
    Ok(())
}

fn branch_oid(repo: &RepoTarget) -> Result<Option<String>> {
    let reference = format!("refs/heads/{}", repo.task_branch);
    // for-each-ref omits dangling symbolic refs, so inspect the ref itself
    // first. A missing ordinary ref returns 1; any symbolic or unreadable ref
    // must remain pending even after a prior removal intent.
    let symbolic = command(
        &repo.repo_root,
        &["symbolic-ref", "--quiet", "--no-recurse", &reference],
    )?;
    ensure!(
        symbolic.status.code() == Some(1),
        "Task branch is symbolic or unreadable; refusing deletion: {}",
        String::from_utf8_lossy(&symbolic.stderr)
    );
    let bytes = git_bytes(
        &repo.repo_root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
            &reference,
        ],
    )?;
    let text = String::from_utf8(bytes)?;
    for line in text.lines() {
        let mut fields = line.splitn(3, ' ');
        if fields.next() != Some(reference.as_str()) {
            continue;
        }
        let oid = fields.next().context("Task branch has no OID")?;
        ensure!(
            fields.next().unwrap_or_default().is_empty(),
            "Task branch is symbolic; refusing to delete a reference to another branch"
        );
        return Ok(Some(oid.into()));
    }
    Ok(None)
}

fn validate_branch(repo: &RepoTarget) -> Result<()> {
    let delivered = repo
        .integrated_commit
        .as_deref()
        .context("Delivered OID missing")?;
    ensure!(
        branch_oid(repo)?.as_deref() == Some(delivered),
        "Task branch moved or disappeared from delivered OID"
    );
    let target = format!("refs/heads/{}", repo.target_branch);
    git_bytes(
        &repo.repo_root,
        &["merge-base", "--is-ancestor", delivered, &target],
    )?;
    Ok(())
}

fn validate_worktree(repo: &RepoTarget) -> Result<()> {
    ensure!(
        repo.worktree_path.canonicalize()? == repo.worktree_path,
        "Worktree path is redirected"
    );
    let entries = delivery::worktree_entries(&repo.repo_root)?;
    let matching: Vec<_> = entries
        .iter()
        .filter(|entry| entry.path == repo.worktree_path)
        .collect();
    ensure!(
        matching.len() == 1,
        "Worktree registration is missing or ambiguous"
    );
    let entry = matching[0];
    let reference = format!("refs/heads/{}", repo.task_branch);
    ensure!(
        entry.branch.as_deref() == Some(reference.as_str())
            && entry.locked.is_none()
            && !entry.prunable
            && !entry.bare,
        "Worktree registration/branch is foreign, locked or prunable"
    );
    ensure!(
        entries
            .iter()
            .filter(|entry| entry.branch.as_deref() == Some(reference.as_str()))
            .count()
            == 1,
        "Task branch is registered to another worktree"
    );
    ensure!(
        delivery::canonical_common_dir(&repo.worktree_path)? == repo.git_common_dir,
        "Worktree belongs to another repository"
    );
    ensure!(
        PathBuf::from(git(&repo.worktree_path, &["rev-parse", "--show-toplevel"])?)
            .canonicalize()?
            == repo.worktree_path,
        "Worktree root identity mismatch"
    );
    ensure!(
        git(&repo.worktree_path, &["symbolic-ref", "HEAD"])? == reference,
        "Worktree HEAD branch changed"
    );
    ensure!(
        Some(git(&repo.worktree_path, &["rev-parse", "HEAD"])?.as_str())
            == repo.integrated_commit.as_deref(),
        "Worktree HEAD moved from delivered OID"
    );
    // Index flags can hide user edits from porcelain and from Git's own
    // removal check. Refuse those worktrees rather than trusting a clean view.
    let tracked = git_bytes(&repo.worktree_path, &["ls-files", "-v", "-z"])?;
    ensure!(
        !tracked.split(|byte| *byte == 0).any(|entry| entry
            .first()
            .is_some_and(|tag| tag.is_ascii_lowercase() || *tag == b'S')),
        "Worktree has assume-unchanged or skip-worktree entries; cannot prove it clean"
    );
    ensure!(
        git_bytes(
            &repo.worktree_path,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored"
            ]
        )?
        .is_empty(),
        "Worktree has dirty, untracked or ignored content; preserving user files"
    );
    Ok(())
}

fn descendants_stopped(
    launcher: &Launcher,
    task: &TaskRecord,
) -> Result<Vec<crate::services::session_messages::ExecutionLease>> {
    let context = &launcher.context;
    let mut participants: BTreeSet<_> = context.db.cleanup_members(task)?.into_iter().collect();
    let mut unresolved = BTreeSet::new();
    let mut historical_coders = Vec::new();
    historical_coders.extend(
        task.coder
            .previous_session_ids
            .iter()
            .map(|id| (&task.task_id, id)),
    );
    unresolved.extend(task.reviewer.previous_session_ids.clone());
    // Child tasks can have their own Coder/Reviewer and durable launch receipts.
    // A terminal task flag alone is not proof those processes have stopped.
    let store = TaskStore::from_context(context);
    let related = store.list_tasks(&task.router_session_id, None)?;
    let mut task_ids = BTreeSet::from([task.task_id.clone()]);
    loop {
        let mut changed = false;
        for child in &related {
            if child
                .parent_task_id
                .as_ref()
                .is_some_and(|parent| task_ids.contains(parent))
                && !task_ids.contains(&child.task_id)
            {
                ensure!(
                    child.status == Status::Completed,
                    "Child task {} has no proven completed state",
                    child.task_id
                );
                let plans = store.delivery_plans(&child.task_id)?;
                let events = store.events(&child.task_id)?;
                let completed = events
                    .iter()
                    .find(|event| {
                        event.event_type == EventType::TaskCompleted
                            && event.spec_version == child.spec_version
                            && event.review_round == child.review_round
                    })
                    .context("Child task has no completion evidence")?;
                let plan = plans
                    .iter()
                    .find(|plan| {
                        Some(plan.operation_id.as_str())
                            == completed.payload["plan_operation_id"].as_str()
                    })
                    .context("Child task completion plan is unknown")?;
                let purpose =
                    super::orchestrator::required_completion_reviewer_purpose(&events, plan)
                        .context("Child task review purpose is unknown")?;
                let members = launcher.completion_member_jobs(child, plan, purpose)?;
                for observation in members.unknown().chain(members.active()) {
                    anyhow::bail!("Child task {}: {}", child.task_id, observation.describe());
                }
                participants.extend(context.db.cleanup_members(child)?);
                historical_coders.extend(
                    child
                        .coder
                        .previous_session_ids
                        .iter()
                        .map(|id| (&child.task_id, id)),
                );
                unresolved.extend(child.reviewer.previous_session_ids.clone());
                task_ids.insert(child.task_id.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for (task_id, id) in historical_coders {
        let recorded = context.db.former_coder_incarnations(task_id, id)?;
        if recorded.is_empty() {
            // Recover older history from durable job markers below.
            unresolved.insert(id.clone());
        }
        participants.extend(recorded.into_iter().map(|inc| (id.clone(), inc)));
    }
    // Worktree occupancy independently includes unrelated current incarnations.
    let worktree = &task.repositories[0].worktree_path;
    for (id, incarnation, workspace) in context.db.workspace_sessions()? {
        let path = Path::new(&workspace);
        if path.starts_with(worktree) || path.canonicalize().is_ok_and(|p| p.starts_with(worktree))
        {
            participants.insert((id, incarnation));
        }
    }
    // Completed launches retain both endpoints even after G retires an intermediate
    // session. Index all edges before traversal so filesystem ordering is irrelevant.
    let mut descendants = std::collections::BTreeMap::<_, Vec<_>>::new();
    let mut idle_children = std::collections::BTreeMap::<_, Vec<_>>::new();
    let mut records = Vec::new();
    let jobs = context.paths.data_dir.join("tasks/background");
    if jobs.try_exists()? {
        for entry in fs::read_dir(&jobs)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Invalid job directory"))?;
            let identity: BackgroundIdentity = (|| -> Result<_> {
                Ok(serde_json::from_slice(&fs::read(
                    entry.path().join("state.json"),
                )?)?)
            })()
            .with_context(|| {
                format!("Job {id}: cannot read identity to exclude a worktree occupant")
            })?;
            identity.validate(&id)?;
            let owners = background_sessions::JobOwners::load_for_id(&context.paths.data_dir, &id);
            if let Ok(Some(marker)) = &owners {
                let parent = (identity.parent_session_id.clone(), marker.parent.clone());
                let execution = (identity.session_id.clone(), marker.execution.clone());
                if unresolved.contains(&parent.0) {
                    participants.insert(parent.clone());
                }
                if unresolved.contains(&execution.0) {
                    participants.insert(execution.clone());
                }
                descendants
                    .entry(parent)
                    .or_default()
                    .push(execution.clone());
                idle_children.entry(execution).or_default().push(id.clone());
            }
            records.push((identity, owners));
        }
    }
    for id in unresolved {
        ensure!(
            participants.iter().any(|(known, _)| known == &id),
            "Missing historical participant incarnation for {id}"
        );
    }
    let mut parents: Vec<_> = participants.iter().cloned().collect();
    while let Some((parent, incarnation)) = parents.pop() {
        for job in idle_children
            .remove(&(parent.clone(), incarnation.clone()))
            .unwrap_or_default()
        {
            if let Some(child) =
                crate::services::session_worker::recorded_job_child(&context.paths.data_dir, &job)?
            {
                if participants.insert(child.clone()) {
                    parents.push(child);
                }
            }
        }
        for child in descendants
            .remove(&(parent.clone(), incarnation.clone()))
            .unwrap_or_default()
        {
            if participants.insert(child.clone()) {
                parents.push(child);
            }
        }
        for (id, inc, owner) in context.db.recorded_child_jobs(&parent, &incarnation)? {
            let job = owner
                .job_id
                .as_deref()
                .context("Descendant has no proven job identity")?;
            require_stopped(&context.paths.data_dir, job, &id, Some(&parent))?;
            let child = (id, inc);
            if participants.insert(child.clone()) {
                parents.push(child);
            }
        }
    }
    for (identity, owners) in records {
        if !participants
            .iter()
            .any(|(id, _)| id == &identity.parent_session_id || id == &identity.session_id)
        {
            continue;
        }
        let related = match owners? {
            Some(marker) => {
                participants.contains(&(identity.parent_session_id.clone(), marker.parent))
                    || participants.contains(&(identity.session_id.clone(), marker.execution))
            }
            None => {
                // A legacy marker cannot distinguish a retired endpoint from its replacement.
                anyhow::ensure!(
                    !participants
                        .iter()
                        .any(|(id, _)| id == &identity.parent_session_id)
                        || participants
                            .iter()
                            .any(|(id, _)| id == &identity.session_id),
                    "Cannot establish descendant identity for job {}",
                    identity.id
                );
                true
            }
        };
        if related {
            require_stopped(
                &context.paths.data_dir,
                &identity.id,
                &identity.session_id,
                Some(&identity.parent_session_id),
            )?;
        }
    }
    participants
        .into_iter()
        .map(|(id, incarnation)| context.db.hold_stopped_execution(&id, &incarnation))
        .collect()
}

#[derive(Deserialize)]
struct BackgroundIdentity {
    id: String,
    session_id: String,
    parent_session_id: String,
}

impl BackgroundIdentity {
    fn validate(&self, directory_id: &str) -> Result<()> {
        ensure!(
            self.id == directory_id
                && !self.session_id.trim().is_empty()
                && !self.parent_session_id.trim().is_empty(),
            "Job {directory_id} has missing/conflicting identity; cannot exclude a worktree occupant"
        );
        Ok(())
    }
}

fn require_stopped(data: &Path, id: &str, session: &str, parent: Option<&str>) -> Result<()> {
    match background_sessions::inspect(data, id)? {
        JobSnapshot::Present(job)
            if job.id == id
                && job.session_id == session
                && parent.is_none_or(|parent| job.parent_session_id == parent)
                && matches!(job.status.as_str(), "completed" | "failed" | "cancelled") =>
        {
            Ok(())
        }
        _ => anyhow::bail!("Job {id} is active, unknown or has a conflicting identity"),
    }
}
