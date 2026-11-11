//! Starts and resumes the Coder and Reviewer sessions of a task.
//!
//! The launcher reuses the extracted child-session path, so provider pinning,
//! permission inheritance, workspace binding and resume behave exactly like the
//! Agent tool's background children. It only adds task-specific instructions,
//! member binding and the Reviewer's fixed validation checkout.
use crate::native_engine::child_session::{
    spawn_child_session, ChildHandle, ChildLaunchConfig, ChildSpawn, ChildWorkspace,
};
use crate::native_engine::provider_manager::ResolvedProvider;
use crate::services::ServiceContext;
use crate::super_agent::config::{AgentModelRole, ModelFallback, SuperAgentConfig};
use crate::super_agent::crp;
use crate::super_agent::role::MemberRole;
use crate::super_agent::store::TaskStore;
use crate::super_agent::{Phase, TaskRecord};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub role: String,
    pub session_id: String,
    pub job_id: String,
    pub workspace: PathBuf,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub session_id: String,
    pub job_id: String,
    pub workspace: PathBuf,
    pub model: String,
    pub warning: Option<String>,
    pub resumed: bool,
}

#[derive(Clone)]
pub struct Launcher {
    pub context: ServiceContext,
    pub config: SuperAgentConfig,
    /// The Router's resolved provider and model, inherited by both members.
    pub provider: ResolvedProvider,
    pub permission_mode: String,
    pub executable_override: Option<PathBuf>,
}

impl Launcher {
    pub fn store(&self) -> TaskStore {
        TaskStore::from_context(&self.context)
    }

    fn record(&self, task: &TaskRecord, role: &str) -> Result<Option<LaunchRecord>> {
        self.context
            .setting(&launch_key(&task.task_id, role))
            .map_err(Into::into)
    }

    fn save_record(&self, task_id: &str, record: &LaunchRecord) -> Result<()> {
        self.context
            .save_setting(&launch_key(task_id, &record.role), record)
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

    /// Launches the Coder in plan mode. A Coder already recorded for this task
    /// is resumed instead of creating a second session.
    pub async fn launch_coder(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        let workspace = self.task_workspace(task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Coder);
        ensure!(
            matches!(task.phase, Phase::Planning | Phase::Discovering),
            "Coder launch expects a task in planning, found {}",
            task.phase.as_str()
        );
        let existing = task.coder.session_id.clone();
        let prompt = coder_plan_prompt(task);
        self.spawn(
            task,
            MemberRole::Coder,
            &workspace,
            existing.as_deref(),
            &prompt,
            model.id,
            warning,
        )
        .await
    }

    /// Continues the same Coder session after the plan is approved.
    pub async fn resume_coder(
        &self,
        task: &TaskRecord,
        instruction: &str,
    ) -> Result<LaunchOutcome> {
        let session = task
            .coder
            .session_id
            .clone()
            .context("Task has no Coder session to resume")?;
        let workspace = self.task_workspace(task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Coder);
        self.spawn(
            task,
            MemberRole::Coder,
            &workspace,
            Some(&session),
            instruction,
            model.id,
            warning,
        )
        .await
    }

    /// Launches the Reviewer in its own fixed checkout of the submitted commit.
    /// It never sees the Coder's writable worktree.
    pub async fn launch_reviewer(&self, task: &TaskRecord) -> Result<LaunchOutcome> {
        let workspace = self.prepare_review_workspace(task)?;
        let (model, warning) = self.resolve_model(AgentModelRole::Reviewer);
        let prompt = reviewer_prompt(task);
        self.spawn(
            task,
            MemberRole::Reviewer,
            &workspace,
            task.reviewer.session_id.as_deref(),
            &prompt,
            model.id,
            warning,
        )
        .await
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

    async fn spawn(
        &self,
        task: &TaskRecord,
        member: MemberRole,
        workspace: &Path,
        resume: Option<&str>,
        prompt: &str,
        model: String,
        warning: Option<String>,
    ) -> Result<LaunchOutcome> {
        // A recorded launch for the same member and workspace is not repeated:
        // a tick must never create a duplicate session or job.
        if let Some(previous) = self.record(task, member.as_str())? {
            let still_there = task
                .coder
                .session_id
                .as_deref()
                .or(task.reviewer.session_id.as_deref())
                .is_some_and(|session| session == previous.session_id);
            if still_there && previous.workspace == workspace && resume.is_none() {
                return Ok(LaunchOutcome {
                    session_id: previous.session_id,
                    job_id: previous.job_id,
                    workspace: previous.workspace,
                    model: previous.model,
                    warning: previous.warning,
                    resumed: false,
                });
            }
        }
        let mut extra_env = Vec::new();
        if let Some(warning) = &warning {
            // The member records the fallback in its own transcript evidence.
            extra_env.push((
                "DEEPSEEK_CODE_SUPER_AGENT_MODEL_WARNING".into(),
                warning.clone(),
            ));
        }
        let handle: ChildHandle = spawn_child_session(
            ChildLaunchConfig {
                provider: self.provider.clone(),
                permission_mode: self.permission_mode.clone(),
            },
            ChildSpawn {
                data_dir: self.context.paths.data_dir.clone(),
                parent_session_id: task.router_session_id.clone(),
                parent_workspace: task.spec.workspace.clone(),
                workspace: ChildWorkspace::Directory(workspace.to_path_buf()),
                prompt: prompt.to_owned(),
                description: format!("{} {}", task.task_id, member.as_str()),
                agent_type: "general".into(),
                model: Some(model.clone()),
                effort: None,
                system_prompt_append: Some(member_instructions(task, member)),
                title: format!("{} / {}", task.task_id, member.as_str()),
                resume_session: resume.map(str::to_owned),
                extra_env,
                executable_override: self.executable_override.clone(),
                invocation: None,
                member_binding: resume.is_none().then(|| (task.task_id.clone(), member)),
            },
        )
        .await?;
        let record = LaunchRecord {
            role: member.as_str().into(),
            session_id: handle.session_id.clone(),
            job_id: handle.job_id.clone(),
            workspace: handle.workspace.clone(),
            model: model.clone(),
            warning: warning.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        self.save_record(&task.task_id, &record)?;
        Ok(LaunchOutcome {
            session_id: handle.session_id,
            job_id: handle.job_id,
            workspace: handle.workspace,
            model,
            warning,
            resumed: handle.resumed,
        })
    }

    fn prepare_review_workspace(&self, task: &TaskRecord) -> Result<PathBuf> {
        ensure!(
            !task.repositories.is_empty(),
            "Reviewing a non-Git task needs the snapshot evidence path, which is not implemented yet"
        );
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
            )?;
            ensure!(
                worktree.base_commit == commit,
                "Review worktree is not pinned at the submitted commit"
            );
            roots.push(worktree.worktree_root);
        }
        if roots.len() == 1 {
            return Ok(roots.remove(0));
        }
        let workspace = self
            .context
            .paths
            .data_dir
            .join("super-agent")
            .join(&task.task_id)
            .join("review-workspace");
        std::fs::create_dir_all(&workspace)?;
        for (index, root) in roots.iter().enumerate() {
            let link = workspace.join(format!("review-{}", index + 1));
            if !link.exists() {
                link_directory(root, &link)?;
            }
        }
        Ok(workspace)
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
            "## Super Agent Task {} / Coder\n\nYou are the Coder for one task. Work only inside the task workspace. The task specification is authoritative:\n\n```json\n{spec}\n```\n\nRepositories and isolated branches:\n{repos}\n\nRules:\n- Start by calling EnterPlanMode. Investigate before proposing changes.\n- Submit your plan with CrpSubmit using kind \"plan\" and a PlanReport payload containing spec_version, round, understanding, scope, steps, verification and risks.\n- Do not edit business code before the plan is approved by the host.\n- When you implement, commit your work in the task workspace. Submit kind \"implementation\" with an ImplementationReport, then kind \"integration\" with an IntegrationReport when the host asks for integration.\n- Report unrelated findings with CrpProposeTask instead of expanding this task.\n",
            task.task_id
        ),
        MemberRole::Reviewer => format!(
            "## Super Agent Task {} / Reviewer\n\nYou review one task independently. You have a read-only checkout of the submitted commit. Do not modify files.\n\nThe task specification is authoritative:\n\n```json\n{spec}\n```\n\nRules:\n- Check the submitted commit against every acceptance criterion.\n- Submit kind \"review\" with a TaskReviewReport containing verdict, spec_version, round, stage \"IMPLEMENTATION\", reviewed_versions, acceptance_results with evidence, and verification.\n- Submit kind \"harness\" with a HarnessAnalysisReport. A Harness-category finding with severity high or critical blocks delivery; efficiency suggestions do not.\n- If the work is incomplete, use verdict \"CHANGES_REQUESTED\" and list concrete issues. Use \"BLOCKED\" only when the task cannot be verified.\n",
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
        "The plan for task {} was approved. Implement it now in the task workspace, run the verification you planned, and commit the result. Then submit an implementation report with CrpSubmit(kind=\"implementation\").",
        task.task_id
    )
}

pub fn reviewer_prompt(task: &TaskRecord) -> String {
    let versions: Vec<String> = task
        .repositories
        .iter()
        .filter_map(|repo| {
            repo.submitted_commit
                .as_deref()
                .map(|commit| format!("{} at {commit}", repo.repository_id))
        })
        .collect();
    format!(
        "Task {}: {}\n\nReview the submitted implementation ({}). Verify every acceptance criterion against the code, then submit CrpSubmit(kind=\"review\") and CrpSubmit(kind=\"harness\").",
        task.task_id,
        task.title,
        versions.join(", ")
    )
}
