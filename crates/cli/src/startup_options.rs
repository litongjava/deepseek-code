//! Startup options reuse the same durable state as interactive commands.
use crate::{args::Cli, session::Session};
use anyhow::{ensure, Context, Result};
#[path = "worktree_startup.rs"]
pub(crate) mod worktree_startup;

pub fn validate(cli: &Cli, workspace: &std::path::Path) -> Result<()> {
    worktree_startup::validate(cli, workspace)?;
    if let Some(name) = &cli.name {
        ensure!(
            !name.trim().is_empty()
                && name.chars().count() <= 200
                && !name.chars().any(char::is_control),
            "--name needs 1..200 characters without control characters"
        );
    }
    for directory in &cli.add_dir {
        let path = workspace
            .join(directory)
            .canonicalize()
            .with_context(|| format!("--add-dir is unavailable: {}", directory.display()))?;
        ensure!(
            path.is_dir(),
            "--add-dir must be a directory: {}",
            directory.display()
        );
    }
    Ok(())
}

pub async fn apply(cli: &Cli, session: &mut Session) -> Result<()> {
    validate(cli, &session.workspace)?;
    for directory in &cli.add_dir {
        deepseek_code_core::services::session_workspace::change(
            &session.store.db,
            &session.id,
            &session.workspace,
            &directory.to_string_lossy(),
            true,
        )?;
    }
    worktree_startup::apply(cli, session).await?;
    if let Some(name) = &cli.name {
        session.store.db.with_conn(|conn| {
            deepseek_code_core::db::conversation_repo::update_conversation_title(conn, &session.id, name)
        })??;
    }
    if cli.ide {
        let editors = deepseek_code_core::services::ide::discover(
            &deepseek_code_core::services::ide::lock_directory()?,
            &session.workspace,
        )?;
        let matches: Vec<_> = editors
            .iter()
            .filter(|editor| editor.matches_workspace)
            .collect();
        match matches.as_slice() {
            [editor] => {
                if let Err(error) =
                    crate::ide_commands::handle(session, &format!("connect {}", editor.port)).await
                {
                    eprintln!("IDE auto-connect: {error:#}");
                }
            }
            [] => eprintln!("IDE auto-connect: no matching editor extension is running."),
            _ => eprintln!(
                "IDE auto-connect: multiple editors match; choose one with /ide connect PORT."
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn worktree_optional_name_and_resume_conflicts() {
        for option in ["-w", "--worktree"] {
            let cli = Cli::try_parse_from(["dcl", option]).unwrap();
            assert_eq!(cli.worktree.as_deref(), Some(""));
            let cli = Cli::try_parse_from(["dcl", option, "review/issue"]).unwrap();
            assert_eq!(cli.worktree.as_deref(), Some("review/issue"));
        }
        assert!(Cli::try_parse_from([
            "dcl",
            "--worktree",
            "review",
            "--resume",
            "session"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["dcl", "--worktree", "--continue"]).is_err());
    }
    #[test]
    fn startup_identity_and_paths_are_validated_before_creation() {
        let temp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let cli = Cli::try_parse_from([
            "dcl",
            "--session-id",
            &id,
            "--name",
            "中文 workspace",
            "--add-dir",
            temp.path().to_str().unwrap(),
            "--ide",
        ])
        .unwrap();
        assert!(cli.ide);
        validate(&cli, temp.path()).unwrap();
        assert_eq!(cli.session_id.as_deref(), Some(id.as_str()));
        assert!(Cli::try_parse_from(["dcl", "--session-id", "invalid"]).is_err());
        assert!(
            Cli::try_parse_from(["dcl", "--session-id", &id, "--resume", &id]).is_err()
        );
        let mut invalid = cli;
        invalid.name = Some("\n".into());
        assert!(validate(&invalid, temp.path()).is_err());
        invalid.name = None;
        invalid.add_dir.push(temp.path().join("absent"));
        assert!(validate(&invalid, temp.path()).is_err());
    }
}
