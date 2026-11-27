//! Script workflows, as in the reference CLI: the Workflow tool persists the
//! script, starts `dcl workflow-run` as a background task and returns at
//! once; the task's notification reports the result. TaskGet shows
//! progress and TaskStop cancels it.
use super::*;
use crate::tools::ToolDefinition;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MAX_SCRIPT_BYTES: usize = 512 * 1024;

/// A workflow's child agent is a separate session. A REPL turn runs in a session
/// worker whose control channel would make every child refuse to start ("does
/// not own this session"), so the whole inherited worker state is stripped; the
/// explicit environment applied by the caller then re-adds the snapshot this
/// delegation deliberately forwards.
fn workflow_child_command(workspace: &Path) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    crate::services::background_sessions::strip_internal_env(&mut command);
    command.current_dir(workspace);
    Ok(command)
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        summary: Some("Run a workflow script in a background child process".into()),
        name: "Workflow".into(),
        description: "A workflow script is plain JavaScript that a `workflow-run` child process executes in the background. The call returns a task ID and run ID at once; TaskGet shows progress and TaskStop cancels. Scripts begin with an object literal `export const meta = { name, description, phases? }`, stay within 512 KiB, and are supplied inline, from a file path, or as a saved name. The async body can call agent(), parallel(), pipeline(), phase(), log() and read args; agent() starts a child dcl session with this session's provider and permission mode, optionally shaped by a JSON schema. Only the main session may start a workflow, at most four agents run at once, and resumeFromRunId reuses completed agents.".into(),
        input_schema: json!({"type":"object","properties":{
            "script":{"description":"Inline workflow script as plain JavaScript, up to 512 KiB, opening with an `export const meta = {...}` object literal.","type":"string","maxLength":MAX_SCRIPT_BYTES},
            "name":{"description":"Load a saved workflow: `deep-research` after an explicit /deep-research command, or `<name>.js`/`<name>.mjs` from `.claude/workflows/` or the runtime workflows directory.","type":"string"},
            "description":{"description":"Unused; the description is read from the script's `meta` block.","type":"string"},
            "title":{"description":"Unused; the name is read from the script's `meta` block.","type":"string"},
            "args":{"description":"Value passed to the script as the global `args`; give objects and arrays as JSON values, not as an encoded string."},
            "scriptPath":{"description":"File to read the script from instead of `script`, taking precedence over `script` and `name`. Each run saves its script under the session directory and returns that path for editing and reuse.","type":"string"},
            "resumeFromRunId":{"description":"`wf_...` run ID from an earlier workflow in this session; agents whose prompt and options are unchanged are reused from that run's cache.","type":"string","pattern":"^wf_[a-z0-9-]{6,}$"}
        },"additionalProperties":false}),
    }
}

/// Whether this call is a script workflow rather than the built-in deep research.
pub(super) fn is_script(input: &Value) -> bool {
    !input["script"].is_null()
        || !input["scriptPath"].is_null()
        || input["name"]
            .as_str()
            .is_some_and(|name| name != "deep-research")
}

/// `name` and `description` from the script's leading meta literal.
fn meta_field(script: &str, field: &str) -> Option<String> {
    let head: String = script.chars().take(4000).collect();
    let pattern = regex::Regex::new(&format!(
        r#"\b{field}\s*:\s*(?:'([^']*)'|"([^"]*)"|`([^`]*)`)"#
    ))
    .ok()?;
    let captures = pattern.captures(&head)?;
    captures
        .get(1)
        .or(captures.get(2))
        .or(captures.get(3))
        .map(|m| m.as_str().to_owned())
}

impl ToolLoopExecutor {
    fn workflow_script(&self, input: &Value) -> Result<String> {
        let script = if let Some(path) = input["scriptPath"].as_str() {
            std::fs::read_to_string(crate::tools::resolve_path(path, &self.workspace_cwd)?)
                .with_context(|| format!("Reading workflow script {path}"))?
        } else if let Some(script) = input["script"].as_str() {
            script.to_owned()
        } else {
            let name = input["name"]
                .as_str()
                .context("Workflow needs script, scriptPath or name")?;
            ensure!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
                "Invalid workflow name"
            );
            let root = self
                .search_config_path
                .as_deref()
                .and_then(crate::data_layout::data_root_of)
                .map(Path::to_path_buf);
            let candidates = [
                Some(PathBuf::from(&self.workspace_cwd).join(".claude/workflows")),
                root.map(|root| root.join("tasks/workflows")),
            ];
            candidates
                .into_iter()
                .flatten()
                .flat_map(|dir| ["js", "mjs"].map(|ext| dir.join(format!("{name}.{ext}"))))
                .find_map(|path| std::fs::read_to_string(path).ok())
                .with_context(|| format!("No workflow named {name} in .claude/workflows/"))?
        };
        ensure!(
            script.len() <= MAX_SCRIPT_BYTES,
            "Workflow script exceeds 512 KiB"
        );
        ensure!(
            script.trim_start().starts_with("export const meta"),
            "A workflow script must begin with `export const meta = {{...}}`"
        );
        Ok(script)
    }

    pub(super) async fn execute_script_workflow(&self, input: &Value) -> Result<Value> {
        ensure!(
            self.delegation_depth == 0,
            "Workflows start from the main session only"
        );
        let root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of)
            .context("Runtime directory is unavailable")?
            .to_path_buf();
        let session = self.conv_id.clone().context("Workflows need a session")?;
        let script = self.workflow_script(input)?;
        let sessions = root.join("tasks/workflows").join(&session);
        let resume_dir = match input["resumeFromRunId"].as_str() {
            Some(id) => {
                ensure!(
                    id.starts_with("wf_")
                        && id[3..]
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                    "Invalid resumeFromRunId"
                );
                let dir = sessions.join(id);
                ensure!(dir.is_dir(), "No workflow run {id} in this session");
                Some(dir)
            }
            None => None,
        };
        let run_id = format!("wf_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let run_dir = sessions.join(&run_id);
        std::fs::create_dir_all(&run_dir)?;
        let script_path = run_dir.join("script.js");
        std::fs::write(&script_path, &script)?;
        let name = meta_field(&script, "name").unwrap_or_else(|| "workflow".into());
        let description = meta_field(&script, "description").unwrap_or_default();

        let (mut agent_args, environment) = self.child_cli_launch(&root)?;
        agent_args.extend([
            "--worker-model-snapshot".into(),
            format!("{}::{}", self.provider.provider.id, self.model_selector),
        ]);
        let mut arguments = vec![
            "workflow-run".to_owned(),
            "--script".into(),
            script_path.to_string_lossy().into_owned(),
            "--run-dir".into(),
            run_dir.to_string_lossy().into_owned(),
            "--agent-args".into(),
            serde_json::to_string(&agent_args)?,
        ];
        if !input["args"].is_null() {
            arguments.extend(["--args-json".into(), input["args"].to_string()]);
        }
        if let Some(dir) = &resume_dir {
            arguments.extend(["--resume-dir".into(), dir.to_string_lossy().into_owned()]);
        }
        let mut command = workflow_child_command(Path::new(&self.workspace_cwd))?;
        command
            .args(&arguments)
            .envs(environment)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let label = format!("Workflow {name}");
        let started = crate::tools::background_shell::start(
            command,
            None,
            "workflow",
            &label,
            &label,
            None,
            crate::tools::background_shell::Interaction::None,
        )?;
        let task_id = started["task_id"].clone();
        if task_id.is_null() {
            bail!("The workflow did not start: {started}");
        }
        // What /workflows lists for this run.
        std::fs::write(run_dir.join("run.json"), json!({"run_id": run_id, "task_id": task_id, "name": name, "description": description, "started_ms": chrono::Utc::now().timestamp_millis()}).to_string())?;
        Ok(json!({
            "task_id": task_id,
            "run_id": run_id,
            "script_path": script_path,
            "name": name,
            "description": description,
            "status": "running",
            "message": "The workflow is running in the background. You will be notified when it finishes; do not poll. TaskGet shows progress, TaskStop stops it. To change the script, edit script_path and re-invoke with scriptPath; pass resumeFromRunId to reuse completed agents."
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_child_drops_inherited_worker_state() {
        let command = workflow_child_command(Path::new("fixture-workspace")).unwrap();
        let stripped: Vec<_> = command.as_std().get_envs().collect();
        for name in crate::services::background_sessions::INTERNAL_ENV {
            assert!(
                stripped
                    .iter()
                    .any(|(entry, value)| *entry == name && value.is_none()),
                "{name} was not stripped"
            );
        }
    }

    #[test]
    fn meta_fields_and_script_detection() {
        let script = "export const meta = {\n  name: 'review-changes',\n  description: \"Review the diff\",\n}\nreturn 1";
        assert_eq!(
            meta_field(script, "name").as_deref(),
            Some("review-changes")
        );
        assert_eq!(
            meta_field(script, "description").as_deref(),
            Some("Review the diff")
        );
        assert!(is_script(&json!({"script":script})));
        assert!(is_script(&json!({"name":"saved-flow"})));
        assert!(!is_script(
            &json!({"name":"deep-research","args":"question"})
        ));
    }
}
