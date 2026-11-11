//! Reusable launch and resume of independent `dcl` child sessions.
//!
//! The `Agent` tool and the Super Agent orchestrator both need a child process
//! that inherits the parent's provider, model snapshot and permission mode.
//! This module owns that launch path so a later feature cannot silently drift
//! from the delegation behavior. Callers remain responsible for their own
//! entry-point limits, such as the delegation depth check.
use crate::native_engine::provider_manager::{ApiFormat, ResolvedProvider};
use crate::runtime::{RuntimePaths, SessionStore};
use crate::services::{
    agents, background_sessions, subagents, EnvironmentCredentials, ServiceContext,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Prefix of the durable ownership record written for every launched child.
pub const OWNER_KEY_PREFIX: &str = "child-owner.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildWorkspace {
    /// Launch in a directory the caller already prepared, such as a task worktree.
    Directory(PathBuf),
    /// Create a managed worktree for this child from the parent's workspace.
    Worktree,
}

/// Everything about the parent that a child inherits. Resolved by the caller
/// from the live session so a child cannot silently pick a different provider.
#[derive(Debug, Clone)]
pub struct ChildLaunchConfig {
    pub provider: ResolvedProvider,
    pub permission_mode: String,
}

#[derive(Debug, Clone)]
pub struct ChildInvocation {
    pub tool_use_id: String,
    pub description: String,
    pub mode: String,
}

#[derive(Debug, Clone)]
pub struct ChildSpawn {
    pub data_dir: PathBuf,
    /// Must be an existing, unarchived session. Checked before any row is created.
    pub parent_session_id: String,
    /// The parent's working directory; also the source for `Worktree`.
    pub parent_workspace: PathBuf,
    pub workspace: ChildWorkspace,
    pub prompt: String,
    /// Short human description used for the job title and invocation record.
    pub description: String,
    pub agent_type: String,
    /// A configured model ID for the parent provider. Omitted keeps the parent model.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Appended after the agent definition's system prompt on every launch,
    /// including resume, so a resumed child keeps its role instructions.
    pub system_prompt_append: Option<String>,
    pub title: String,
    /// Continue this session instead of creating a new one. Its ownership
    /// record (or a durable subagent invocation) must name the same parent.
    pub resume_session: Option<String>,
    pub extra_env: Vec<(String, String)>,
    /// Host-supplied executable, mainly for tests and hosts that already know
    /// the matching standalone CLI path.
    pub executable_override: Option<PathBuf>,
    pub invocation: Option<ChildInvocation>,
    /// Bind this child as the current Coder or Reviewer of a task before its
    /// process starts, so the first turn already has member tools.
    pub member_binding: Option<(String, crate::super_agent::MemberRole)>,
}

#[derive(Debug, Clone)]
pub struct ChildHandle {
    pub job_id: String,
    pub session_id: String,
    pub description: String,
    pub agent_type: String,
    pub workspace: PathBuf,
    pub resumed: bool,
    pub status: String,
}

/// Durable record that ties a child session to its owner. It records process
/// state, never a fabricated success: a launch that fails is `failed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChildOwnership {
    pub parent_session_id: String,
    pub agent_type: String,
    pub description: String,
    pub mode: String,
    pub status: String,
    #[serde(default)]
    pub job_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Launches or resumes one child session in the background.
///
/// The parent is validated first, but no delegation-depth check happens here:
/// the orchestrator is a program, not a model tool call. `spawn_background_agent`
/// keeps its own `delegation_depth == 0` rule before calling this function.
pub async fn spawn_child_session(
    config: ChildLaunchConfig,
    spawn: ChildSpawn,
) -> Result<ChildHandle> {
    ensure!(
        !config.provider.provider.id.trim().is_empty(),
        "Child provider is required"
    );
    ensure!(
        !spawn.prompt.trim().is_empty() && spawn.prompt.len() <= 64_000,
        "Child prompt must contain 1-64000 bytes"
    );
    if let Some(effort) = spawn.effort.as_deref() {
        ensure!(
            ["low", "medium", "high", "xhigh", "max"].contains(&effort),
            "Invalid effort"
        );
    }
    if let Some(model) = spawn.model.as_deref() {
        ensure!(!model.trim().is_empty(), "Child model must not be empty");
    }
    ensure!(
        spawn.data_dir.is_absolute(),
        "Data directory must be absolute"
    );

    let context = ServiceContext::open(
        RuntimePaths::new(spawn.data_dir.clone())?,
        std::sync::Arc::new(EnvironmentCredentials),
    )?;
    let parent = require_session(&context, &spawn.parent_session_id, "Parent session")?;
    ensure!(!parent.archived, "Parent session is archived");
    ensure!(
        spawn.resume_session.as_deref() != Some(spawn.parent_session_id.as_str()),
        "A session cannot resume itself"
    );
    ensure!(
        spawn.parent_workspace.is_dir(),
        "Parent workspace is unavailable"
    );

    let agent_type = if spawn.agent_type.trim().is_empty() {
        "general"
    } else {
        spawn.agent_type.as_str()
    };
    let definition = agents::resolve(&context, agent_type)?;
    let description = describe(&spawn.description, &spawn.prompt);

    let sessions = SessionStore::new(context.db.clone());
    // A directory child is created in its prepared workspace so its durable
    // `conversations.workspace_path` is the isolated directory, not the parent.
    // A worktree child necessarily starts in the parent workspace because the
    // worktree binds to an existing session.
    let requested_workspace = match &spawn.workspace {
        ChildWorkspace::Directory(path) => Some(canonical_directory(path)?),
        ChildWorkspace::Worktree => None,
    };
    let (session_id, resumed) = match spawn.resume_session.as_deref() {
        Some(session) => {
            let child = require_session(&context, session, "Child session")?;
            ensure!(!child.archived, "Child session is archived");
            let owner = owner_record(&context, session)?;
            ensure!(
                child_belongs_to_parent(
                    &context,
                    &spawn.parent_session_id,
                    session,
                    owner.as_ref()
                )?,
                "Child belongs to another owner"
            );
            // Never start a second process for a session that is still executing.
            ensure!(
                !crate::services::session_messages::execution_active(
                    &context.paths.data_dir,
                    session
                )?,
                "Child session still has an active execution; stop it before resuming"
            );
            let stored = child
                .workspace_path
                .as_deref()
                .context("Child session has no recorded workspace")?;
            let stored = canonical_directory(Path::new(stored))?;
            if let Some(requested) = &requested_workspace {
                ensure!(
                    &stored == requested,
                    "A resumed child keeps its existing workspace at {}; refusing to move it to {}",
                    stored.display(),
                    requested.display()
                );
            }
            (session.to_owned(), true)
        }
        None => {
            // Preserve the Agent-tool behavior: the session row keeps the
            // parent's model and an explicit `--model` only changes the launch.
            let initial = requested_workspace
                .clone()
                .unwrap_or_else(|| spawn.parent_workspace.clone());
            let child = sessions.create(
                &initial,
                &config.provider.model.id,
                &config.provider.provider.id,
            )?;
            crate::services::permission_authority::record_user(
                &context.db,
                &child,
                &json!(spawn.prompt),
            )?;
            agents::bind(&context.db, &child, &definition)?;
            (child, false)
        }
    };

    let workspace = match &spawn.workspace {
        ChildWorkspace::Directory(_) => requested_workspace
            .clone()
            .context("Child workspace is unavailable")?,
        ChildWorkspace::Worktree => {
            ensure!(
                !resumed,
                "A resumed child keeps its existing worktree; pass its directory explicitly"
            );
            let input = json!({"name":format!("agent-{}", &session_id[..8.min(session_id.len())])});
            let worktree: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(Value, PathBuf)>> + Send + '_>,
            > = Box::pin(crate::tools::session_worktree::execute(
                "EnterWorktree",
                &input,
                &spawn.data_dir,
                &session_id,
                &spawn.parent_workspace,
            ));
            worktree.await?.1
        }
    };

    let (mut arguments, mut environment) =
        child_cli_arguments(&config.provider, &config.permission_mode, &spawn.data_dir);
    if let Some(model) = spawn.model.as_deref() {
        let at = arguments
            .iter()
            .position(|arg| arg == "--model")
            .context("child model argument")?
            + 1;
        arguments[at] = model.to_owned();
        // The parent's snapshot describes the parent's model; the explicit
        // `--model` argument is authoritative for this child.
        environment.retain(|(key, _)| key != background_sessions::WORKER_MODEL_ENV);
    }
    arguments.extend([
        "--resume".into(),
        session_id.clone(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
    ]);
    let mut system = definition.system_prompt.clone();
    if let Some(extra) = spawn
        .system_prompt_append
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        if !system.trim().is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(extra);
    }
    if !system.trim().is_empty() {
        arguments.extend(["--append-system-prompt".into(), system]);
    }
    if let Some(effort) = spawn.effort.as_deref() {
        arguments.extend(["--effort".into(), effort.to_owned()]);
    }
    arguments.extend(["--".into(), spawn.prompt.clone()]);
    // Explicit caller values win over inherited launch values.
    for (key, value) in &spawn.extra_env {
        environment.retain(|(existing, _)| existing != key);
        environment.push((key.clone(), value.clone()));
    }

    let executable = match &spawn.executable_override {
        Some(path) => path.clone(),
        None => standalone_child_executable().await?,
    };
    let title = if spawn.title.trim().is_empty() {
        format!("Agent: {description}")
    } else {
        spawn.title.clone()
    };
    let now = chrono::Utc::now().to_rfc3339();
    let mut ownership = match &spawn.resume_session {
        Some(_) => owner_record(&context, &session_id)?.unwrap_or(ChildOwnership {
            parent_session_id: spawn.parent_session_id.clone(),
            agent_type: definition.id.clone(),
            description: description.clone(),
            mode: "background".into(),
            status: "starting".into(),
            job_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_error: None,
        }),
        None => ChildOwnership {
            parent_session_id: spawn.parent_session_id.clone(),
            agent_type: definition.id.clone(),
            description: description.clone(),
            mode: "background".into(),
            status: "starting".into(),
            job_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_error: None,
        },
    };
    ownership.status = "starting".into();
    ownership.updated_at = now;
    ownership.last_error = None;
    save_owner(&context, &session_id, &ownership)?;

    if let Some((task_id, member)) = &spawn.member_binding {
        let store = crate::super_agent::TaskStore::new(context.db.clone(), spawn.data_dir.clone());
        match member {
            crate::super_agent::MemberRole::Coder => {
                store.set_sessions(task_id, Some(&session_id), None)
            }
            crate::super_agent::MemberRole::Reviewer => {
                store.set_sessions(task_id, None, Some(&session_id))
            }
        }
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }

    let mut invocation = match &spawn.invocation {
        Some(invocation) => record_child_invocation(
            &context,
            &spawn.parent_session_id,
            &session_id,
            &invocation.description,
            Some(&invocation.tool_use_id),
            &invocation.mode,
        )?,
        None => None,
    };

    let job = background_sessions::spawn(
        &spawn.data_dir,
        &executable,
        background_sessions::Launch {
            executable: executable.clone(),
            arguments,
            cwd: workspace.clone(),
        },
        &spawn.parent_session_id,
        &session_id,
        &title,
        &environment,
    );
    let job = match job {
        Ok(job) => job,
        Err(error) => {
            ownership.status = "failed".into();
            ownership.updated_at = chrono::Utc::now().to_rfc3339();
            ownership.last_error = Some(format!("{error:#}"));
            let _ = save_owner(&context, &session_id, &ownership);
            if let Some(invocation) = invocation.as_mut() {
                let _ = invocation.finish(&Err(anyhow::anyhow!(error.to_string())));
            }
            return Err(error);
        }
    };
    ownership.status = job.status.clone();
    ownership.job_id = Some(job.id.clone());
    ownership.updated_at = chrono::Utc::now().to_rfc3339();
    save_owner(&context, &session_id, &ownership)?;
    if let Some(invocation) = invocation.as_mut() {
        invocation.finish(&Ok(json!({"backgrounded":true})))?;
    }

    Ok(ChildHandle {
        job_id: job.id,
        session_id,
        description,
        agent_type: definition.id,
        workspace,
        resumed,
        status: job.status,
    })
}

/// Arguments and environment shared by every child launch. Preserves the
/// delegation contract: the child is pinned to the parent's resolved provider
/// and model, forwards the model snapshot and credential, and never inherits
/// the parent worker's auto-route decision.
pub(crate) fn child_cli_arguments(
    provider: &ResolvedProvider,
    permission_mode: &str,
    data_dir: &Path,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut arguments = vec![
        "--data-dir".to_owned(),
        data_dir.to_string_lossy().into_owned(),
        "--provider".into(),
        provider.provider.id.clone(),
        "--model".into(),
        provider.model.id.clone(),
        "--permission-mode".into(),
        permission_mode.to_owned(),
    ];
    if provider.provider.builtin_id.is_none() {
        arguments.extend(["--base-url".into(), provider.provider.base_url.clone()]);
        arguments.extend([
            "--api-format".into(),
            match provider.provider.api_format {
                ApiFormat::Anthropic => "anthropic",
                ApiFormat::OpenAI => "openai",
            }
            .into(),
        ]);
    }
    let mut environment = vec![(
        background_sessions::WORKER_MODEL_ENV.to_owned(),
        serde_json::to_string(&provider.model).unwrap_or_default(),
    )];
    if !provider.provider.api_key.is_empty() {
        arguments.extend(["--api-key-env".into(), "DEEPSEEK_CODE_CHILD_API_KEY".into()]);
        environment.push((
            "DEEPSEEK_CODE_CHILD_API_KEY".into(),
            provider.provider.api_key.clone(),
        ));
    }
    (arguments, environment)
}

pub(crate) struct InvocationGuard {
    context: ServiceContext,
    key: String,
    value: Value,
}

impl InvocationGuard {
    pub(crate) fn finish(&mut self, result: &Result<Value>) -> Result<()> {
        self.value["status"] = json!(match result {
            Ok(value) if value["backgrounded"] == true => "running",
            Ok(_) => "completed",
            Err(_) => "failed",
        });
        self.context.save_setting(&self.key, &self.value)
    }
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if self.value["status"] == "starting" {
            self.value["status"] = json!("cancelled");
            let _ = self.context.save_setting(&self.key, &self.value);
        }
    }
}

/// Records an Agent-tool invocation so the parent transcript can show which
/// child it started. Super Agent launches have no tool-use row and skip this.
pub(crate) fn record_child_invocation(
    context: &ServiceContext,
    parent: &str,
    child: &str,
    description: &str,
    tool_use_id: Option<&str>,
    mode: &str,
) -> Result<Option<InvocationGuard>> {
    let Some(tool_use_id) = tool_use_id else {
        return Ok(None);
    };
    let parent_message_id = context.db.with_conn(|conn| -> Result<String> {
        use rusqlite::OptionalExtension;
        Ok(conn.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND role='assistant' ORDER BY sort_order DESC LIMIT 1", [parent], |row| row.get(0)).optional()?.unwrap_or_default())
    })??;
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let key = format!("subagent_invocation:{parent}:{tool_use_id}:{attempt_id}");
    let value = json!({"parent_session_id":parent,"parent_message_id":parent_message_id,"tool_use_id":tool_use_id,"child_session_id":child,"attempt_id":attempt_id,"mode":mode,"status":"starting","description":description});
    context.save_setting(&key, &value)?;
    Ok(Some(InvocationGuard {
        context: context.clone(),
        key,
        value,
    }))
}

/// The matching standalone CLI for this host. Desktop may not be beside an
/// installed `dcl`, so the caller can also pass an explicit executable.
pub(crate) async fn standalone_child_executable() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    if matches!(
        current.file_stem().and_then(|name| name.to_str()),
        Some("dcl" | "deepseek-code-cli")
    ) {
        return Ok(current);
    }
    let name = if cfg!(windows) { "dcl.exe" } else { "dcl" };
    let mut candidates = Vec::new();
    if let Some(directory) = current.parent() {
        candidates.push(directory.join(name));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(name)),
        );
    }
    for candidate in candidates
        .into_iter()
        .filter(|path| path.is_file() && path != &current)
    {
        let mut command = tokio::process::Command::new(&candidate);
        command.arg("--version").kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        if let Ok(Ok(output)) =
            tokio::time::timeout(std::time::Duration::from_secs(3), command.output()).await
        {
            let version = String::from_utf8_lossy(&output.stdout);
            if output.status.success()
                && version.split_whitespace().next() == Some("dcl")
                && version.split_whitespace().nth(1) == Some(env!("CARGO_PKG_VERSION"))
            {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("Background agent launcher unavailable: install the matching standalone dcl CLI beside Desktop or on PATH")
}

fn describe(description: &str, prompt: &str) -> String {
    let description = description.trim();
    if !description.is_empty() {
        return description.chars().take(160).collect();
    }
    prompt
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(60)
        .collect()
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Child workspace is unavailable: {}", path.display()))?;
    ensure!(
        canonical.is_dir(),
        "Child workspace is not a directory: {}",
        canonical.display()
    );
    Ok(canonical)
}

fn owner_key(session: &str) -> String {
    format!("{OWNER_KEY_PREFIX}{session}")
}

pub fn owner_record(context: &ServiceContext, session: &str) -> Result<Option<ChildOwnership>> {
    context.setting(&owner_key(session))
}

fn save_owner(context: &ServiceContext, session: &str, owner: &ChildOwnership) -> Result<()> {
    context.save_setting(&owner_key(session), owner)
}

fn require_session(
    context: &ServiceContext,
    session: &str,
    label: &str,
) -> Result<crate::db::conversation_repo::ConversationRow> {
    ensure!(!session.trim().is_empty(), "{label} ID is required");
    let row = context
        .db
        .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, session))??;
    row.with_context(|| format!("{label} not found"))
}

/// A resume is valid only when a durable association names this same parent.
/// An ownership record is authoritative; older Agent-tool children carry a
/// subagent invocation or an owned private-workspace binding instead.
fn child_belongs_to_parent(
    context: &ServiceContext,
    parent: &str,
    child: &str,
    owner: Option<&ChildOwnership>,
) -> Result<bool> {
    if let Some(owner) = owner {
        return Ok(owner.parent_session_id == parent);
    }
    let invoked = context
        .db
        .with_conn(|conn| subagents::list_invocations(conn, parent, None))??
        .into_iter()
        .any(|invocation| invocation.child_session_id == child);
    if invoked {
        return Ok(true);
    }
    let bound = context.db.with_conn(|conn| -> Result<bool> {
        use rusqlite::OptionalExtension;
        Ok(conn
            .query_row(
                "SELECT 1 FROM conversation_private_workspace_bindings WHERE child_id=?1 AND parent_id=?2",
                rusqlite::params![child, parent],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    })??;
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ResolvedProvider {
        ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"memory-only-secret","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        }
    }

    fn service_context(root: &Path) -> ServiceContext {
        ServiceContext::open(
            RuntimePaths::new(root.to_path_buf()).unwrap(),
            std::sync::Arc::new(EnvironmentCredentials),
        )
        .unwrap()
    }

    fn spawn(root: &Path, parent: &str, prompt: &str) -> ChildSpawn {
        ChildSpawn {
            data_dir: root.to_path_buf(),
            parent_session_id: parent.into(),
            parent_workspace: root.to_path_buf(),
            workspace: ChildWorkspace::Directory(root.to_path_buf()),
            prompt: prompt.into(),
            description: "fixture child".into(),
            agent_type: "general".into(),
            model: None,
            effort: None,
            system_prompt_append: None,
            title: "fixture".into(),
            resume_session: None,
            extra_env: Vec::new(),
            executable_override: None,
            invocation: None,
            member_binding: None,
        }
    }

    fn failing_executable() -> PathBuf {
        std::env::temp_dir().join("dcl-super-agent-missing-executable")
    }

    fn quiet_executable() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("cmd.exe")
        } else {
            PathBuf::from("/bin/true")
        }
    }

    fn launch_arguments(root: &Path, job: &str) -> Vec<String> {
        let launch =
            std::fs::read_to_string(root.join("background-tasks").join(job).join("launch.json"))
                .unwrap();
        let launch: Value = serde_json::from_str(&launch).unwrap();
        launch["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn pair(arguments: &[String], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair[0] == name && pair[1] == value)
    }

    #[tokio::test]
    async fn launch_preserves_provider_and_writes_no_success_before_the_job_starts() {
        let temp = tempfile::tempdir().unwrap();
        let context = service_context(temp.path());
        let parent = SessionStore::new(context.db.clone())
            .create(temp.path(), "fixture", "fixture")
            .unwrap();
        let mut request = spawn(temp.path(), &parent, "inspect");
        request.executable_override = Some(quiet_executable());
        let handle = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            request,
        )
        .await
        .unwrap();
        assert_eq!(handle.status, "starting");
        assert!(!handle.resumed);
        let owner = owner_record(&context, &handle.session_id).unwrap().unwrap();
        assert_eq!(owner.parent_session_id, parent);
        assert_eq!(owner.status, "starting");
        assert_eq!(owner.job_id.as_deref(), Some(handle.job_id.as_str()));
        assert_ne!(owner.status, "completed");
        assert_ne!(owner.status, "running");
        let arguments = launch_arguments(temp.path(), &handle.job_id);
        assert!(pair(&arguments, "--provider", "fixture"));
        assert!(pair(&arguments, "--model", "fixture"));
        assert!(pair(&arguments, "--permission-mode", "bypass"));
        assert!(pair(&arguments, "--resume", &handle.session_id));
        assert!(pair(&arguments, "--output-format", "stream-json"));
        assert!(arguments.iter().any(|arg| arg == "inspect"));
    }

    #[tokio::test]
    async fn launch_failure_is_recorded_as_failed_without_a_completed_job() {
        let temp = tempfile::tempdir().unwrap();
        let context = service_context(temp.path());
        let parent = SessionStore::new(context.db.clone())
            .create(temp.path(), "fixture", "fixture")
            .unwrap();
        let mut request = spawn(temp.path(), &parent, "inspect");
        request.executable_override = Some(failing_executable());
        let error = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "ask".into(),
            },
            request,
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        let children: Vec<String> = context
            .db
            .with_conn(|conn| -> Result<Vec<String>> {
                let mut statement = conn.prepare("SELECT id FROM conversations WHERE id <> ?1")?;
                let ids = statement
                    .query_map([&parent], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ids)
            })
            .unwrap()
            .unwrap();
        assert_eq!(children.len(), 1);
        let owner = owner_record(&context, &children[0]).unwrap().unwrap();
        assert_eq!(owner.status, "failed");
        assert!(owner.job_id.is_none());
        assert!(owner.last_error.is_some());
        let jobs = background_sessions::list(temp.path(), Some(&parent)).unwrap();
        assert!(jobs.iter().all(|job| job.status != "completed"));
    }

    #[tokio::test]
    async fn resume_reuses_the_same_session_and_rejects_another_parent() {
        let temp = tempfile::tempdir().unwrap();
        let context = service_context(temp.path());
        let sessions = SessionStore::new(context.db.clone());
        let parent = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let other = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let child = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        save_owner(
            &context,
            &child,
            &ChildOwnership {
                parent_session_id: parent.clone(),
                agent_type: "general".into(),
                description: "fixture".into(),
                mode: "background".into(),
                status: "completed".into(),
                job_id: None,
                created_at: now.clone(),
                updated_at: now,
                last_error: None,
            },
        )
        .unwrap();
        let before = conversation_count(&context);
        let mut request = spawn(temp.path(), &parent, "continue");
        request.resume_session = Some(child.clone());
        request.executable_override = Some(quiet_executable());
        let handle = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            request,
        )
        .await
        .unwrap();
        assert!(handle.resumed);
        assert_eq!(handle.session_id, child);
        assert_eq!(before, conversation_count(&context));
        let arguments = launch_arguments(temp.path(), &handle.job_id);
        assert!(pair(&arguments, "--resume", &child));

        let mut wrong = spawn(temp.path(), &other, "continue");
        wrong.resume_session = Some(child);
        wrong.executable_override = Some(quiet_executable());
        let error = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            wrong,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("another owner"));
    }

    fn conversation_count(context: &ServiceContext) -> i64 {
        context
            .db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            })
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn unknown_or_archived_parents_are_rejected_before_any_launch() {
        let temp = tempfile::tempdir().unwrap();
        let context = service_context(temp.path());
        let mut request = spawn(temp.path(), "missing-parent", "inspect");
        request.executable_override = Some(quiet_executable());
        let error = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "ask".into(),
            },
            request,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Parent session not found"));

        let sessions = SessionStore::new(context.db.clone());
        let parent = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        context
            .db
            .with_conn(|conn| {
                conn.execute("UPDATE conversations SET archived=1 WHERE id=?1", [&parent])
            })
            .unwrap()
            .unwrap();
        let error = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "ask".into(),
            },
            spawn(temp.path(), &parent, "inspect"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("archived"));
        let jobs = background_sessions::list(temp.path(), Some(&parent)).unwrap();
        assert!(jobs.is_empty());
    }

    #[test]
    fn child_arguments_keep_the_explicit_snapshot_and_credential_without_auto_route() {
        let (arguments, environment) =
            child_cli_arguments(&provider(), "ask", Path::new("/tmp/data"));
        assert!(pair(&arguments, "--provider", "fixture"));
        assert!(pair(&arguments, "--model", "fixture"));
        let names: Vec<&str> = environment.iter().map(|(key, _)| key.as_str()).collect();
        assert!(names.contains(&background_sessions::WORKER_MODEL_ENV));
        assert!(names.contains(&"DEEPSEEK_CODE_CHILD_API_KEY"));
        assert!(!names.contains(&background_sessions::WORKER_AUTO_ROUTE_ENV));
    }

    #[tokio::test]
    async fn a_directory_child_persists_the_prepared_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let workspace = temp.path().join("task-workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("marker.txt"), "isolated").unwrap();
        let data = temp.path().join("data");
        let context = service_context(&data);
        let parent = SessionStore::new(context.db.clone())
            .create(&root, "fixture", "fixture")
            .unwrap();
        let mut request = spawn(&data, &parent, "inspect");
        request.parent_workspace = root.clone();
        request.workspace = ChildWorkspace::Directory(workspace.clone());
        request.executable_override = Some(quiet_executable());
        let handle = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            request,
        )
        .await
        .unwrap();
        let canonical = workspace.canonicalize().unwrap();
        assert_eq!(handle.workspace, canonical);
        let row = context
            .db
            .with_conn(|conn| {
                crate::db::conversation_repo::get_conversation(conn, &handle.session_id)
            })
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            row.workspace_path.as_deref(),
            Some(canonical.to_string_lossy().as_ref())
        );
        let launch = std::fs::read_to_string(
            data.join("background-tasks")
                .join(&handle.job_id)
                .join("launch.json"),
        )
        .unwrap();
        let launch: Value = serde_json::from_str(&launch).unwrap();
        assert_eq!(
            launch["cwd"].as_str().map(std::path::PathBuf::from),
            Some(canonical.clone())
        );
        let job = background_sessions::list(&data, Some(&parent))
            .unwrap()
            .into_iter()
            .find(|job| job.id == handle.job_id)
            .unwrap();
        assert_eq!(job.parent_session_id, parent);
        let owner = owner_record(&context, &handle.session_id).unwrap().unwrap();
        assert_eq!(owner.parent_session_id, parent);
    }

    #[tokio::test]
    async fn resume_cannot_move_a_child_to_a_different_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        for path in [&root, &first, &second] {
            std::fs::create_dir(path).unwrap();
        }
        let data = temp.path().join("data");
        let context = service_context(&data);
        let parent = SessionStore::new(context.db.clone())
            .create(&root, "fixture", "fixture")
            .unwrap();
        let mut request = spawn(&data, &parent, "first run");
        request.parent_workspace = root.clone();
        request.workspace = ChildWorkspace::Directory(first.clone());
        request.executable_override = Some(quiet_executable());
        let handle = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            request,
        )
        .await
        .unwrap();

        let mut moved = spawn(&data, &parent, "second run");
        moved.parent_workspace = root;
        moved.workspace = ChildWorkspace::Directory(second);
        moved.resume_session = Some(handle.session_id.clone());
        moved.executable_override = Some(quiet_executable());
        let error = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            moved,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("keeps its existing workspace"),
            "{error:#}"
        );
        assert_eq!(
            background_sessions::list(&data, Some(&parent))
                .unwrap()
                .len(),
            1
        );

        let mut same = spawn(&data, &parent, "continue");
        same.parent_workspace = temp.path().join("project");
        same.workspace = ChildWorkspace::Directory(first.clone());
        same.resume_session = Some(handle.session_id.clone());
        same.executable_override = Some(quiet_executable());
        let resumed = spawn_child_session(
            ChildLaunchConfig {
                provider: provider(),
                permission_mode: "bypass".into(),
            },
            same,
        )
        .await
        .unwrap();
        assert_eq!(resumed.workspace, first.canonicalize().unwrap());
        assert_eq!(resumed.session_id, handle.session_id);
    }
}
