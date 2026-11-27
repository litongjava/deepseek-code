//! Transfer an interactive session to a replacement dcl process.

use crate::{args::Cli, session::Session};
use anyhow::{Context, Result};
use std::{ffi::OsString, path::Path};

pub struct Launch {
    pub arguments: Vec<OsString>,
    pub inherit_api_base_url: bool,
    /// A relay resuming an explicit session preserves the terminal presentation.
    pub resume_terminal: bool,
}

/// The launcher has already resolved these paths against its original cwd.
fn workspace_args(arguments: &mut Vec<OsString>, data_dir: &Path, workspace: Option<&Path>) {
    arguments.extend(["--data-dir".into(), data_dir.as_os_str().into()]);
    if let Some(workspace) = workspace {
        arguments.extend(["-C".into(), workspace.as_os_str().into()]);
    }
}

pub fn fresh(cli: &Cli, data_dir: &Path, workspace: &Path) -> Launch {
    let mut arguments =
        cli.launch_options(&["path", "data_dir", "coordination_dir", "prompt_origin"]);
    // Only an explicit -C constrains a resumed session to the launch directory.
    workspace_args(
        &mut arguments,
        data_dir,
        cli.path.as_ref().map(|_| workspace),
    );
    if let Some(prompt) = &cli.prompt {
        use deepseek_code_core::services::background_sessions::PromptOrigin;
        let origin = if cli.generated_continuation.is_some() {
            PromptOrigin::Generated
        } else {
            cli.prompt_origin
        };
        arguments.extend(origin.arguments(prompt).map(OsString::from));
    }
    Launch {
        arguments,
        inherit_api_base_url: true,
        resume_terminal: cli.resume.is_some(),
    }
}

pub fn resume(cli: &Cli, session: &Session) -> Result<Launch> {
    // Use the active session for identity, model and permissions. Replaying
    // the original startup mutations would overwrite changes made in the REPL.
    let omit = [
        "path",
        "data_dir",
        "coordination_dir",
        "resume",
        "continue_session",
        "session_id",
        "fork_session",
        "worktree",
        "name",
        "add_dir",
        "role",
        "init",
        "init_only",
        "maintenance",
        "auto_route",
        "binding_generation",
        "effort",
        "max_tokens",
        "base_url",
        "api_format",
        "api_key_env",
        "bound_builtin",
        "model",
        "provider",
        "permission_mode",
        "dangerously_skip_permissions",
        "full_access",
    ];
    let state = session.binding_state();
    let mut arguments = cli.launch_options(&omit);
    workspace_args(
        &mut arguments,
        &session.paths.data_dir,
        Some(&session.workspace),
    );
    arguments.extend([
        "--resume".into(),
        session.id.as_str().into(),
        "--binding-generation".into(),
        state.generation.to_string().into(),
    ]);
    // The normal resume path reads the current durable mode. Bypass requires
    // explicit opt-in, and Full Access needs a mode to avoid implying bypass.
    let mode = session.permission_mode();
    if mode == "bypass" || session.full_access {
        arguments.extend(["--permission-mode".into(), mode.into()]);
    }
    if session.full_access {
        arguments.push("--full-access".into());
    }
    Ok(Launch {
        arguments,
        inherit_api_base_url: false,
        resume_terminal: true,
    })
}

fn command(launch: Launch) -> Result<std::process::Command> {
    let executable =
        std::env::current_exe().context("Cannot find the dcl executable to restart")?;
    let mut command = std::process::Command::new(executable);
    command.args(launch.arguments);
    if !launch.inherit_api_base_url {
        command.env_remove("API_BASE_URL");
    }
    Ok(command)
}

/// Replaces this process so stdin, the console and signals stay with one dcl.
#[cfg(unix)]
pub fn relaunch(launch: Launch) -> Result<()> {
    use std::os::unix::process::CommandExt;
    Err(command(launch)?.exec()).context("Cannot restart dcl")
}

/// Windows stays as a thin wrapper so the shell waits for the restarted dcl.
#[cfg(not(unix))]
pub fn relaunch(launch: Launch) -> Result<()> {
    let status = command(launch)?.status().context("Cannot restart dcl")?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::PermissionModeArg;

    fn parse(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> Cli {
        Cli::try_parse_with_mode_source(args).unwrap()
    }

    #[test]
    fn remote_launch_round_trips_options_and_uses_resolved_directories() {
        let cli = parse([
            "dcl",
            "--add-dir",
            "extra",
            "-Cproject",
            "--data-dir=data",
            "--rc",
            "listen",
            "--model",
            "fixture",
            "--permission-mode",
            "acceptEdits",
            "--port=0",
            "--allowed-tools",
            "Read",
            "Bash(echo a,b)",
            "--allowedTools",
            "Glob",
        ]);
        let workspace = std::env::temp_dir().join("absolute workspace");
        let data = std::env::temp_dir().join("absolute data");
        let child =
            parse(std::iter::once("dcl".into()).chain(fresh(&cli, &data, &workspace).arguments));
        assert!(child.command.is_none());
        assert_eq!(child.path.as_deref(), Some(workspace.as_path()));
        assert_eq!(child.data_dir.as_deref(), Some(data.as_path()));
        assert_eq!(child.add_dir, [Path::new("extra")]);
        assert_eq!(child.model.as_deref(), Some("fixture"));
        assert!(matches!(
            child.permission_mode,
            PermissionModeArg::AcceptEdits
        ));
        assert_eq!(child.allowed_tools, ["Read", "Bash(echo a,b)", "Glob"]);
    }

    #[test]
    fn remote_resume_selectors_preserve_terminal_and_saved_workspace() {
        let cwd = std::env::temp_dir();
        for selector in [
            vec!["--resume", "saved-id"],
            vec!["--resume=saved-id"],
            vec!["-r", "saved-id"],
            vec!["-rsaved-id"],
            vec!["-r=saved-id"],
        ] {
            let cli = parse(["dcl", "--rc", "listen"].into_iter().chain(selector));
            let launch = fresh(&cli, &cwd.join("data"), &cwd);
            assert!(launch.resume_terminal);
            let child = parse(std::iter::once("dcl".into()).chain(launch.arguments));
            assert_eq!(child.resume.as_deref(), Some("saved-id"));
            assert!(child.path.is_none());
        }
        for args in [
            vec!["dcl", "--rc", "listen"],
            vec!["dcl", "--continue", "--rc", "listen"],
            vec!["dcl", "-c", "--rc", "listen"],
        ] {
            let cli = parse(args);
            assert!(!fresh(&cli, &cwd.join("data"), &cwd).resume_terminal);
        }
    }

    #[tokio::test]
    async fn replacement_transfers_identity_and_generation_without_binding_arguments() {
        use crate::config::Selection;
        use deepseek_code_core::runtime::RuntimePaths;
        use serde_json::json;

        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(directory.path().join("data")).unwrap();
        let cli = parse([
            "dcl",
            "--base-url",
            "http://127.0.0.1:1",
            "--api-key-env",
            "FIXTURE_KEY",
            "--api-format",
            "openai",
        ]);
        let selection = || Selection {
            provider_id: "fixture".into(),
            model: "model".into(),
            providers: vec![serde_json::from_value(json!({
                "id":"fixture", "name":"Fixture", "enabled":true, "api_format":"openai",
                "api_key":"fixture-only", "base_url":"http://127.0.0.1:1",
                "models":[{"id":"model","name":"Model","enabled":true,"supports_web_search":false}]
            }))
            .unwrap()],
        };
        let mut session = Session::open(&cli, &paths, selection(), None)
            .await
            .unwrap();
        session.startup_provider = Some(crate::config::StartupProvider::new(
            &session.binding_state().provider_id,
            &cli,
        ));
        let inspect = |launch| {
            let command = command(launch).unwrap();
            assert!(command.get_envs().all(|(_, value)| value.is_none()));
            parse(std::iter::once(OsString::from("dcl")).chain(command.get_args().map(Into::into)))
        };

        let kept = inspect(resume(&cli, &session).unwrap());
        assert!(kept.api_key_env.is_none() && kept.base_url.is_none() && kept.api_format.is_none());
        assert!(kept.model.is_none() && kept.provider.is_none() && kept.max_tokens.is_none());
        assert_eq!(
            kept.binding_generation,
            Some(session.binding_state().generation)
        );

        // An explicit provider/model switch reloads even the same provider from disk.
        session.switch(&paths, selection()).await.unwrap();
        let launch = resume(&cli, &session).unwrap();
        assert!(!launch.inherit_api_base_url);
        let reloaded = inspect(launch);
        assert!(reloaded.api_key_env.is_none());
        assert!(reloaded.api_format.is_none());
        assert!(reloaded
            .launch_options(&[])
            .iter()
            .all(|arg| !arg.to_string_lossy().starts_with("--base-url=")));
        session.close().await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn launch_preserves_native_paths_and_positional_flag_text() {
        use std::os::unix::ffi::OsStringExt;
        let native_root = OsString::from_vec(b"coord-\xff".to_vec());
        let workspace = std::env::temp_dir().join(OsString::from_vec(b"project-\xff".to_vec()));
        let cli = parse([
            "dcl".into(),
            "--coordination-dir".into(),
            native_root,
            "-C".into(),
            workspace.as_os_str().into(),
            "--".into(),
            "--rc".into(),
        ]);
        let child = parse(
            std::iter::once("dcl".into())
                .chain(fresh(&cli, &std::env::temp_dir(), &workspace).arguments),
        );
        assert_eq!(child.path.as_deref(), Some(workspace.as_path()));
        assert!(child.coordination_dir.is_none());
        assert_eq!(child.prompt.as_deref(), Some("--rc"));
    }
}
