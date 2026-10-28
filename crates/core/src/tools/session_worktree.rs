//! EnterWorktree and ExitWorktree, as in the reference CLI. They reuse the
//! session worktree binding behind `dcl --worktree` and `/worktree`, so the
//! prompt, tools and later turns all resolve the same working directory.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::db::DbManager;
use crate::worktree::{hooks, session as worktrees};

pub fn definitions() -> Vec<super::ToolDefinition> {
    vec![
        super::ToolDefinition {
            name: "EnterWorktree".into(),
            description: "Use this tool ONLY when explicitly instructed to work in a worktree — either by the user directly, or by project instructions (AGENTS.md / CLAUDE.md / memory). This tool creates an isolated git worktree and switches the current session into it.\n\n## When to Use\n\n- The user explicitly says \"worktree\" (e.g., \"start a worktree\", \"work in a worktree\", \"create a worktree\", \"use a worktree\")\n- Project or memory instructions direct you to work in a worktree for the current task\n\n## When NOT to Use\n\n- The user asks to create a branch, switch branches, or work on a different branch — use git commands instead\n- The user asks to fix a bug or work on a feature — use normal git workflow unless worktrees are explicitly requested\n\n## Requirements\n\n- Must be in a git repository of a trusted project\n- Must not already be in a worktree session\n\n## Behavior\n\n- Creates a new git worktree on a new branch and switches the session's working directory to it\n- Use ExitWorktree to leave the worktree mid-session (keep or remove). The worktree is kept when the session ends\n\n## Entering an existing worktree\n\nPass `path` instead of `name` to switch the session into a worktree that already exists (e.g., one you just created with `git worktree add`). The path must appear in `git worktree list` for the current repository. ExitWorktree will not remove a worktree entered this way; use `action: \"keep\"` to return to the original directory.\n\n## Parameters\n\n- `name` (optional): A name for a new worktree. If neither `name` nor `path` is provided, a random name is generated.\n- `path` (optional): Path to an existing worktree to enter instead of creating one. Mutually exclusive with `name`.".into(),
            input_schema: json!({"type":"object","properties":{
                "name":{"description":"Optional name for a new worktree. Each \"/\"-separated segment may contain only letters, digits, dots, underscores, and dashes; max 64 chars total. A random name is generated if not provided. Mutually exclusive with `path`.","type":"string"},
                "path":{"description":"Path to an existing worktree to switch into instead of creating a new one. Must appear in `git worktree list` for the current repo. Mutually exclusive with `name`.","type":"string"}
            },"additionalProperties":false}),
        },
        super::ToolDefinition {
            name: "ExitWorktree".into(),
            description: "Exit a worktree session created by EnterWorktree and return the session to the original working directory.\n\n## Scope\n\nThis tool ONLY operates on the worktree this session entered with EnterWorktree (or `dcl --worktree`). If no worktree session is active, the tool is a no-op: it reports that and takes no action.\n\n## When to Use\n\n- The user explicitly asks to \"exit the worktree\", \"leave the worktree\", \"go back\", or otherwise end the worktree session\n- Do NOT call this proactively — only when the user asks\n\n## Parameters\n\n- `action` (required): `\"keep\"` or `\"remove\"`\n  - `\"keep\"` — leave the worktree directory and branch intact on disk.\n  - `\"remove\"` — delete the worktree directory. Its branch is kept so committed work is never lost.\n- `discard_changes` (optional, default false): only meaningful with `action: \"remove\"`. If the worktree has uncommitted or ignored files, the tool will REFUSE to remove it unless this is set to `true`. If the tool returns an error listing changes, confirm with the user before re-invoking with `discard_changes: true`.".into(),
            input_schema: json!({"type":"object","properties":{
                "action":{"description":"\"keep\" leaves the worktree and branch on disk; \"remove\" deletes the worktree directory.","type":"string","enum":["keep","remove"]},
                "discard_changes":{"description":"Required true when action is \"remove\" and the worktree has uncommitted or ignored files. The tool will refuse and list them otherwise.","type":"boolean"}
            },"required":["action"],"additionalProperties":false}),
        },
    ]
}

pub fn is_worktree_tool(name: &str) -> bool {
    matches!(name, "EnterWorktree" | "ExitWorktree")
}

fn entered_key(session: &str) -> String {
    format!("entered-worktree.{session}")
}

/// Returns the tool result and the session's new working directory.
pub async fn execute(name: &str, input: &Value, root: &Path, session: &str, cwd: &Path) -> Result<(Value, PathBuf)> {
    let context = crate::services::ServiceContext::open(crate::runtime::RuntimePaths::new(root.to_path_buf())?, std::sync::Arc::new(crate::services::EnvironmentCredentials))?;
    let db = &context.db;
    match name {
        "EnterWorktree" => {
            ensure!(hooks::status(db, session)?.is_none(), "This session is already in a worktree; use ExitWorktree first");
            ensure!(input["name"].is_null() || input["path"].is_null(), "name and path are mutually exclusive");
            let project = db.with_conn(|conn| crate::native_engine::workspace::conversation_project(conn, session))??
                .context("Worktrees require a trusted project")?;
            let (path, created) = match input["path"].as_str() {
                Some(path) => (bind_existing(db, session, &project.id, cwd, Path::new(path))?, false),
                None => (create(db, session, &project.id, cwd, input["name"].as_str())?, true),
            };
            context.save_setting(&entered_key(session), &json!({"created":created}))?;
            Ok((json!({"worktree_path":path,"content":format!("Switched to worktree {}. The session now works there; use ExitWorktree to return.", path.display())}), path))
        }
        "ExitWorktree" => {
            let Some(state) = hooks::status(db, session)? else {
                return Ok((json!({"content":"No worktree session is active; nothing was changed."}), cwd.to_path_buf()));
            };
            let action = input["action"].as_str().context("action is required")?;
            let path = PathBuf::from(state["path"].as_str().unwrap_or_default());
            let created = context.setting::<Value>(&entered_key(session))?.map_or(true, |entered| entered["created"] != false);
            let source = match action {
                "keep" => hooks::release(root, db, session, false).await?,
                "remove" => {
                    ensure!(created, "This worktree was entered by path, so ExitWorktree will not remove it; use action \"keep\"");
                    let dirty = worktrees::git(&path, &["status", "--porcelain", "--untracked-files=all"])?;
                    if !dirty.is_empty() && input["discard_changes"] != true {
                        bail!("The worktree has uncommitted files; confirm with the user, then pass discard_changes: true to discard them:\n{dirty}");
                    }
                    if input["discard_changes"] == true {
                        let source = hooks::release(root, db, session, false).await?;
                        let repo = worktrees::git(&source, &["rev-parse", "--show-toplevel"])?;
                        worktrees::git(Path::new(&repo), &["worktree", "remove", "--force", &path.to_string_lossy()])?;
                        source
                    } else {
                        hooks::release(root, db, session, true).await?
                    }
                }
                other => bail!("Unsupported action: {other}"),
            };
            context.save_setting(&entered_key(session), &Value::Null)?;
            Ok((json!({"working_directory":source,"content":format!("Left the worktree ({action}). Working directory: {}", source.display())}), source))
        }
        _ => bail!("Unknown worktree tool {name}"),
    }
}

fn create(db: &DbManager, session: &str, project: &str, source: &Path, name: Option<&str>) -> Result<PathBuf> {
    let prepared = worktrees::prepare_named(source, name.filter(|n| !n.is_empty()))?;
    let saved = db.with_conn(|conn| -> Result<()> {
        let transaction = crate::db::write_transaction(&conn)?;
        prepared.save(&transaction, session, project, &source.to_string_lossy())?;
        transaction.execute("UPDATE conversations SET workspace_path=?1 WHERE id=?2", [prepared.path.to_string_lossy().as_ref(), session])?;
        transaction.commit()?;
        Ok(())
    }).and_then(|result| result);
    if let Err(error) = saved {
        prepared.cleanup();
        return Err(error);
    }
    Ok(prepared.path.clone())
}

fn bind_existing(db: &DbManager, session: &str, project: &str, source: &Path, path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().with_context(|| format!("Worktree path does not exist: {}", path.display()))?;
    let repo = worktrees::git(source, &["rev-parse", "--show-toplevel"])?;
    let listed = worktrees::git(Path::new(&repo), &["worktree", "list", "--porcelain"])?;
    let registered = listed.lines().filter_map(|line| line.strip_prefix("worktree ")).any(|line| Path::new(line).canonicalize().ok().as_deref() == Some(path.as_path()));
    ensure!(registered, "{} is not listed by `git worktree list` for this repository", path.display());
    ensure!(path != Path::new(&repo).canonicalize()?, "That path is the main checkout, not a worktree");
    let branch = worktrees::git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    db.with_conn(|conn| -> Result<()> {
        let transaction = crate::db::write_transaction(&conn)?;
        transaction.execute("INSERT INTO conversation_worktrees (conversation_id, project_id, source_path, repo_path, path, branch) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![session, project, source.to_string_lossy(), repo, path.to_string_lossy(), branch])?;
        transaction.execute("UPDATE conversations SET workspace_path=?1 WHERE id=?2", [path.to_string_lossy().as_ref(), session])?;
        transaction.commit()?;
        Ok(())
    })??;
    Ok(path)
}
