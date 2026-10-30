//! CLI worktrees use the shared Desktop conversation binding and resolver.
use crate::{args::Cli, session::Session};
use anyhow::{Context, Result};
use deepseek_code_core::worktree::session as worktrees;

pub fn validate(cli: &Cli, source: &std::path::Path) -> Result<()> {
    if let Some(name) = &cli.worktree {
        let root = cli
            .data_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(deepseek_code_core::runtime::RuntimePaths::default_data_dir)?;
        if deepseek_code_core::worktree::hooks::configured(&root, source)? {
            if !name.is_empty() {
                worktrees::validate_name(name)?;
            }
        } else {
            worktrees::validate_named(source, (!name.is_empty()).then_some(name.as_str()))?;
        }
    }
    Ok(())
}

pub async fn apply(cli: &Cli, session: &mut Session) -> Result<()> {
    let Some(name) = &cli.worktree else {
        return Ok(());
    };
    let source = session.workspace.clone();
    if let Some(path) = crate::hook_progress::during(&session.id, true, deepseek_code_core::worktree::hooks::create(
        &session.paths.data_dir,
        &session.store.db,
        &session.id,
        &source,
        (!name.is_empty()).then_some(name.as_str()),
    ))
    .await?
    {
        session.workspace = path;
        std::env::set_current_dir(&session.workspace)?;
        eprintln!("Hook worktree: {}. Retained on exit; /worktree status, detach or remove manages this binding.",session.workspace.display());
        return Ok(());
    }
    let prepared = worktrees::prepare_named(&source, (!name.is_empty()).then_some(name.as_str()))?;
    let saved = session
        .store
        .db
        .with_conn(|conn| -> Result<()> {
            let transaction = deepseek_code_core::db::write_transaction(&conn)?;
            let project: String = transaction.query_row(
                "SELECT project_id FROM conversations WHERE id=?1",
                [&session.id],
                |row| row.get(0),
            )?;
            prepared.save(
                &transaction,
                &session.id,
                &project,
                &source.to_string_lossy(),
            )?;
            transaction.execute(
                "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
                [prepared.path.to_string_lossy().as_ref(), &session.id],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .and_then(|result| result);
    if let Err(error) = saved {
        prepared.cleanup();
        return Err(error);
    }
    session.workspace = session
        .store
        .workspace(&session.id, &session.paths.data_dir.join("workspaces"))?;
    std::env::set_current_dir(&session.workspace).context("Cannot enter managed worktree")?;
    eprintln!("Worktree: {}\nSession: {}. This worktree is retained on exit; use --resume {} to continue.",session.workspace.display(),session.id,session.id);
    Ok(())
}

pub async fn handle(argument: &str, session: &mut Session) -> Result<()> {
    use deepseek_code_core::worktree::hooks;
    match argument.trim() {
        "" | "status" => println!(
            "{}",
            serde_json::to_string_pretty(&hooks::status(&session.store.db, &session.id)?)?
        ),
        action @ ("detach" | "remove") => {
            let source = hooks::status(&session.store.db, &session.id)?
                .context("This session has no managed worktree")?["source"]
                .as_str()
                .context("Source missing")?
                .to_owned();
            hooks::ensure_idle(&session.paths.data_dir, &session.id)?;
            deepseek_code_core::services::session_worker::shutdown(&session.paths.data_dir, &session.id)
                .await?;
            let previous = std::env::current_dir()?;
            std::env::set_current_dir(&source)?;
            match crate::hook_progress::during(&session.id, true, hooks::release(
                &session.paths.data_dir,
                &session.store.db,
                &session.id,
                action == "remove",
            ))
            .await
            {
                Ok(path) => {
                    session.workspace = path;
                    eprintln!(
                        "Worktree binding released. Working directory: {}",
                        session.workspace.display()
                    );
                }
                Err(error) => {
                    let _ = std::env::set_current_dir(previous);
                    return Err(error);
                }
            }
        }
        _ => anyhow::bail!("Usage: /worktree [status|detach|remove]"),
    }
    Ok(())
}
