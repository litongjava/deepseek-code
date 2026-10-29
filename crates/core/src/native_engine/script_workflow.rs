//! Script workflows, as in the reference CLI: the Workflow tool persists the
//! script, starts `dcl workflow-run` as a background task and returns at
//! once; the task's notification reports the result. TaskOutput shows
//! progress and TaskStop cancels it.
use super::*;
use crate::tools::ToolDefinition;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MAX_SCRIPT_BYTES: usize = 512 * 1024;

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Workflow".into(),
        description: "Execute a workflow script that orchestrates multiple subagents deterministically. Workflows run in the background — this tool returns immediately with a task ID, and a notification arrives when the workflow completes. Use TaskOutput with the task ID to watch progress and TaskStop to stop it.\n\nONLY call this tool when the user has explicitly opted into multi-agent orchestration. Workflows can spawn many agents and consume a large amount of tokens; the user must request that scale, not have it inferred. Explicit opt-in means the user directly asked you to run a workflow or use multi-agent orchestration in their own words (\"use a workflow\", \"run a workflow\", \"fan out agents\", \"orchestrate this with subagents\"), invoked a skill or slash command whose instructions tell you to call Workflow, or asked you to run a specific named or saved workflow. For any other task, use the Agent tool for individual subagents, or briefly describe what a workflow could do and roughly what it would cost, and ask.\n\nEvery script must begin with `export const meta = {...}`: a PURE LITERAL (no variables, calls or interpolation) giving the workflow's `name`, a one-line `description` and optionally `phases` — one `{ title, detail? }` per phase() call. Pass the script inline via `script`; it is plain JavaScript, not TypeScript. The body may use top-level await and `return` its result.\n\nScript API:\n- `agent(prompt, { label?, phase?, schema?, model? })` runs a fresh subagent with this session's provider and permission mode and resolves to its final text, or to an object matching `schema` (a JSON Schema). It throws if the agent fails.\n- `parallel(tasks)` runs functions (or promises) concurrently and resolves to their results in order.\n- `pipeline(items, ...stages)` passes each item through the stages; items proceed independently.\n- `phase(title)` and `log(...values)` report progress. `args` is the tool's `args` value.\n\nThe canonical multi-stage pattern:\n  export const meta = {\n    name: 'review-changes',\n    description: 'Review changed files across dimensions, verify each finding',\n    phases: [{ title: 'Review' }, { title: 'Verify' }],\n  }\n  const results = await pipeline(\n    DIMENSIONS,\n    d => agent(d.prompt, {label: `review:${d.key}`, phase: 'Review', schema: FINDINGS_SCHEMA}),\n    review => parallel(review.findings.map(f => () =>\n      agent(`Adversarially verify: ${f.title}`, {label: `verify:${f.file}`, phase: 'Verify', schema: VERDICT_SCHEMA})\n        .then(v => ({...f, verdict: v}))\n    ))\n  )\n  return { confirmed: results.flat().filter(f => f.verdict?.isReal) }\n\nAt most four agents run at once. Keep workflows under 10 agents unless the user asks for more.\n\n`name: \"deep-research\"` with `args` runs the built-in research workflow, only after an explicit /deep-research command.".into(),
        input_schema: json!({"type":"object","properties":{
            "script":{"description":"Self-contained workflow script. Must begin with `export const meta = { name, description, phases }` (pure literal, no computed values) followed by the script body using agent()/parallel()/pipeline()/phase().","type":"string","maxLength":MAX_SCRIPT_BYTES},
            "name":{"description":"Name of a predefined workflow (built-in or from .claude/workflows/). Resolves to a self-contained script.","type":"string"},
            "description":{"description":"Ignored — set the workflow description in the script's `meta` block.","type":"string"},
            "title":{"description":"Ignored — set the workflow title in the script's `meta` block.","type":"string"},
            "args":{"description":"Optional input value exposed to the script as the global `args`, verbatim. Pass arrays/objects as actual JSON values, NOT as a JSON-encoded string."},
            "scriptPath":{"description":"Path to a workflow script file on disk. Every Workflow invocation persists its script under the session directory and returns the path in the tool result. To iterate, edit that file with Write/Edit and re-invoke Workflow with the same `scriptPath` instead of re-sending the full script. Takes precedence over `script` and `name`.","type":"string"},
            "resumeFromRunId":{"description":"Run ID of a prior Workflow invocation to resume from. Completed agent() calls with unchanged (prompt, opts) return their cached results instantly; only edited or new calls re-run. Same-session only. Stop the prior run first (TaskStop) before resuming.","type":"string","pattern":"^wf_[a-z0-9-]{6,}$"}
        },"additionalProperties":false}),
    }
}

/// Whether this call is a script workflow rather than the built-in deep research.
pub(super) fn is_script(input: &Value) -> bool {
    !input["script"].is_null() || !input["scriptPath"].is_null() || input["name"].as_str().is_some_and(|name| name != "deep-research")
}

/// `name` and `description` from the script's leading meta literal.
fn meta_field(script: &str, field: &str) -> Option<String> {
    let head: String = script.chars().take(4000).collect();
    let pattern = regex::Regex::new(&format!(r#"\b{field}\s*:\s*(?:'([^']*)'|"([^"]*)"|`([^`]*)`)"#)).ok()?;
    let captures = pattern.captures(&head)?;
    captures.get(1).or(captures.get(2)).or(captures.get(3)).map(|m| m.as_str().to_owned())
}

impl ToolLoopExecutor {
    fn workflow_script(&self, input: &Value) -> Result<String> {
        let script = if let Some(path) = input["scriptPath"].as_str() {
            std::fs::read_to_string(crate::tools::resolve_path(path, &self.workspace_cwd)?).with_context(|| format!("Reading workflow script {path}"))?
        } else if let Some(script) = input["script"].as_str() {
            script.to_owned()
        } else {
            let name = input["name"].as_str().context("Workflow needs script, scriptPath or name")?;
            ensure!(name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')), "Invalid workflow name");
            let root = self.search_config_path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
            let candidates = [Some(PathBuf::from(&self.workspace_cwd).join(".claude/workflows")), root.map(|root| root.join("workflows"))];
            candidates.into_iter().flatten().flat_map(|dir| ["js", "mjs"].map(|ext| dir.join(format!("{name}.{ext}"))))
                .find_map(|path| std::fs::read_to_string(path).ok())
                .with_context(|| format!("No workflow named {name} in .claude/workflows/"))?
        };
        ensure!(script.len() <= MAX_SCRIPT_BYTES, "Workflow script exceeds 512 KiB");
        ensure!(script.trim_start().starts_with("export const meta"), "A workflow script must begin with `export const meta = {{...}}`");
        Ok(script)
    }

    pub(super) async fn execute_script_workflow(&self, input: &Value) -> Result<Value> {
        ensure!(self.delegation_depth == 0, "Workflows start from the main session only");
        let root = self.search_config_path.as_deref().and_then(Path::parent).context("Runtime directory is unavailable")?.to_path_buf();
        let session = self.conv_id.clone().context("Workflows need a session")?;
        let script = self.workflow_script(input)?;
        let sessions = root.join("workflows").join(&session);
        let resume_dir = match input["resumeFromRunId"].as_str() {
            Some(id) => {
                ensure!(id.starts_with("wf_") && id[3..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "Invalid resumeFromRunId");
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

        let (agent_args, environment) = self.child_cli_launch(&root)?;
        let mut arguments = vec![
            "workflow-run".to_owned(),
            "--script".into(), script_path.to_string_lossy().into_owned(),
            "--run-dir".into(), run_dir.to_string_lossy().into_owned(),
            "--agent-args".into(), serde_json::to_string(&agent_args)?,
        ];
        if !input["args"].is_null() {
            arguments.extend(["--args-json".into(), input["args"].to_string()]);
        }
        if let Some(dir) = &resume_dir {
            arguments.extend(["--resume-dir".into(), dir.to_string_lossy().into_owned()]);
        }
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command.args(&arguments).envs(environment).current_dir(&self.workspace_cwd).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        // A REPL turn runs in a session worker whose control channel would make
        // every child agent refuse to start ("does not own this session").
        command.env_remove(crate::services::background_sessions::CONTROL_ENV).env_remove(crate::services::background_sessions::WORKER_ENV);
        let label = format!("Workflow {name}");
        let started = crate::tools::background_shell::start(command, None, "workflow", &label, &label)?;
        let task_id = started["task_id"].clone();
        if task_id.is_null() { bail!("The workflow did not start: {started}"); }
        // What /workflows lists for this run.
        std::fs::write(run_dir.join("run.json"), json!({"run_id": run_id, "task_id": task_id, "name": name, "description": description, "started_ms": chrono::Utc::now().timestamp_millis()}).to_string())?;
        Ok(json!({
            "task_id": task_id,
            "run_id": run_id,
            "script_path": script_path,
            "name": name,
            "description": description,
            "status": "running",
            "message": "The workflow is running in the background. You will be notified when it finishes; do not poll. TaskOutput shows progress, TaskStop stops it. To change the script, edit script_path and re-invoke with scriptPath; pass resumeFromRunId to reuse completed agents."
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_fields_and_script_detection() {
        let script = "export const meta = {\n  name: 'review-changes',\n  description: \"Review the diff\",\n}\nreturn 1";
        assert_eq!(meta_field(script, "name").as_deref(), Some("review-changes"));
        assert_eq!(meta_field(script, "description").as_deref(), Some("Review the diff"));
        assert!(is_script(&json!({"script":script})));
        assert!(is_script(&json!({"name":"saved-flow"})));
        assert!(!is_script(&json!({"name":"deep-research","args":"question"})));
    }
}
