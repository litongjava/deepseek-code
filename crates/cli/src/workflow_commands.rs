//! Local workflow files. Hosted workflows need a vendor account and are not faked.
use crate::{args::Cli, session::Session};
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{
    runtime::RuntimePaths,
    services::{background_sessions, permission_authority},
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct LocalWorkflow {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn description(content: &str) -> String {
    let mut lines = content.lines();
    if lines.next().is_some_and(|line| line.trim() == "---") {
        for line in lines.by_ref() {
            let line = line.trim();
            if line == "---" {
                break;
            }
            if let Some(value) = line.strip_prefix("description:") {
                let value = value.trim().trim_matches(['"', '\'']);
                if !value.is_empty() {
                    return value.into();
                }
            }
        }
        return String::new();
    }
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim().to_owned())
        .unwrap_or_default()
}

fn directory(root: &Path, source: &str) -> Result<Vec<LocalWorkflow>> {
    let mut workflows = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(workflows),
        Err(error) => return Err(error).with_context(|| format!("Reading {source}")),
    };
    for entry in entries {
        let path = entry?.path();
        if !path.is_file() || path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if !valid_name(name) {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        ensure!(
            content.len() <= 256 * 1024,
            "Workflow {name} exceeds 256 KiB"
        );
        workflows.push(LocalWorkflow {
            name: name.into(),
            description: description(&content),
            path,
            content,
        });
    }
    workflows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(workflows)
}

/// Discovers user workflows first, then project workflows (project wins on name).
pub fn discover(data_dir: &Path, workspace: &Path) -> Result<Vec<LocalWorkflow>> {
    let mut workflows = directory(&data_dir.join("workflows"), "user workflows")?;
    for workflow in directory(&workspace.join(".claude/workflows"), "project workflows")? {
        workflows.retain(|existing| existing.name != workflow.name);
        workflows.push(workflow);
    }
    workflows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(workflows)
}

pub fn compose(workflow: &LocalWorkflow, arguments: &str) -> Result<String> {
    ensure!(
        arguments.len() <= 64 * 1024,
        "Workflow arguments exceed 64 KiB"
    );
    let mut prompt = format!("# Workflow: {}\n\n", workflow.name);
    if !workflow.description.is_empty() {
        prompt.push_str(&workflow.description);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&workflow.content);
    prompt.push_str("\n\n## Arguments\n");
    if arguments.trim().is_empty() {
        prompt.push_str("(none)");
    } else {
        prompt.push_str(arguments.trim());
    }
    ensure!(prompt.len() <= 512 * 1024, "Workflow prompt is too large");
    Ok(prompt)
}

pub fn print(workflow: &LocalWorkflow) -> String {
    format!(
        "{}  {}\n  {}\n  {}",
        workflow.name,
        workflow.description,
        workflow.path.display(),
        "(/workflows run NAME [ARGS])"
    )
}

pub async fn run(
    cli: &Cli,
    paths: &RuntimePaths,
    session: &Session,
    name: &str,
    arguments: &str,
) -> Result<background_sessions::Job> {
    ensure!(
        !name.starts_with("hosted:"),
        "Hosted workflows require the vendor account and are not available in the standalone CLI"
    );
    let workflow = discover(&paths.data_dir, &session.workspace)?
        .into_iter()
        .find(|workflow| workflow.name == name)
        .with_context(|| {
            format!("Unknown local workflow: {name}. Run /workflows list to see discovered files")
        })?;
    let prompt = compose(&workflow, arguments)?;
    let child_id = session.store.fork(&session.id)?;
    crate::session_commands::copy_settings(&session.store.db, &session.id, &child_id)?;
    permission_authority::record_user(
        &session.store.db,
        &child_id,
        &serde_json::json!(prompt),
    )?;
    let provider = session.selected_provider().await?;
    let arguments = crate::background_commands::arguments(
        cli,
        paths,
        session,
        &child_id,
        &prompt,
        &provider,
    )?;
    let environment = crate::background_commands::worker_environment(session.auto_route, &provider)?;
    let executable = std::env::current_exe()?;
    background_sessions::spawn(
        &paths.data_dir,
        &executable,
        background_sessions::Launch {
            executable: executable.clone(),
            arguments,
            cwd: session.workspace.clone(),
        },
        &session.id,
        &child_id,
        &format!("Workflow {name}"),
        &environment,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_prefers_project_files_and_ignores_invalid_names() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(data.join("workflows")).unwrap();
        std::fs::create_dir_all(workspace.join(".claude/workflows")).unwrap();
        std::fs::write(
            data.join("workflows/deploy.md"),
            "---\ndescription: user deploy\n---\nUSER_BODY",
        )
        .unwrap();
        std::fs::write(
            workspace.join(".claude/workflows/deploy.md"),
            "---\ndescription: project deploy\n---\nPROJECT_BODY",
        )
        .unwrap();
        std::fs::write(workspace.join(".claude/workflows/review.md"), "# Review\nBody")
            .unwrap();
        std::fs::write(workspace.join(".claude/workflows/bad name.md"), "ignored").unwrap();
        std::fs::write(workspace.join(".claude/workflows/notes.txt"), "ignored").unwrap();
        let workflows = discover(&data, &workspace).unwrap();
        assert_eq!(workflows.len(), 2);
        let deploy = workflows.iter().find(|w| w.name == "deploy").unwrap();
        assert_eq!(deploy.description, "project deploy");
        assert!(deploy.content.contains("PROJECT_BODY"));
        let review = workflows.iter().find(|w| w.name == "review").unwrap();
        assert_eq!(review.description, "Review");
        let prompt = compose(deploy, "production").unwrap();
        assert!(prompt.contains("PROJECT_BODY") && prompt.contains("production"));
        assert!(compose(deploy, &"x".repeat(65 * 1024)).is_err());
    }
}
