//! Host-owned status line setup: model output is data, never a tool invocation.
use crate::{session::Session, terminal::input::TerminalInput};
use anyhow::{bail, ensure, Context, Result};
use deepseek_code_core::{
    permissions::{PermissionContext, PermissionResult},
    services::statusline_setup as setup,
};
use serde_json::{json, Value};

fn permission(session: &Session, tool: &str, input: Value) -> PermissionResult {
    session.permissions.check_permission(&PermissionContext {
        tool_name: tool.into(),
        tool_input: input,
        conversation_id: session.id.clone(),
        user_id: None,
        workspace_path: Some(session.workspace.to_string_lossy().into()),
    })
}
async fn authorize(
    session: &Session,
    checks: Vec<(&str, Value)>,
    input: &mut TerminalInput,
    preview: &str,
) -> Result<()> {
    let expected_mode = session.permissions.mode_snapshot();
    let mut confirmation = false;
    for (tool, value) in &checks {
        match permission(session, tool, value.clone()) {
            PermissionResult::Granted => {}
            PermissionResult::Denied(reason) => bail!("Status line operation denied: {reason}"),
            PermissionResult::RequiresConfirmation(_) => confirmation = true,
        }
    }
    eprintln!("{preview}");
    if confirmation {
        let answer = tokio::select! {
            line = input.read_answer("Allow this status line operation? [y/N] ") => line?.context("Input closed")?,
            _ = tokio::signal::ctrl_c() => bail!("Status line setup cancelled"),
        };
        ensure!(
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            "Status line setup cancelled; settings unchanged"
        );
        ensure!(
            session.permissions.mode_snapshot() == expected_mode,
            "Permission mode changed during review; run setup again"
        );
        for (tool, value) in checks {
            if let PermissionResult::Denied(reason) = permission(session, tool, value) {
                bail!("Status line operation denied: {reason}");
            }
        }
    }
    Ok(())
}
pub async fn handle(argument: &str, session: &Session, input: &mut TerminalInput) -> Result<()> {
    let argument = argument.trim();
    if matches!(argument, "" | "show") {
        let current = crate::display_commands::load(session)?;
        eprintln!("Status line command: {:?}\nRust user settings: {}\n/statusline setup [description] imports PS1/PowerShell prompt or describes a new display.\n/statusline command <shell script> sets a command; /statusline off disables it.", current.statusline.as_deref().unwrap_or("off"), setup::path(&session.paths.data_dir).display());
        return Ok(());
    }
    if argument == "off" {
        setup::save(&session.paths.data_dir, None)?;
        eprintln!("Status line disabled for this Rust data directory.");
        return Ok(());
    }
    let (verb, rest) = argument
        .split_once(char::is_whitespace)
        .unwrap_or((argument, ""));
    let command = if verb == "command" {
        let command = rest.trim_start();
        setup::validate_command(command)?;
        command.to_owned()
    } else {
        let request = if verb == "setup" {
            rest.trim()
        } else {
            argument
        };
        let lower = request.to_ascii_lowercase();
        let importing = request.is_empty()
            || ["ps1", "profile", "powershell", "shell prompt", "提示符"]
                .iter()
                .any(|term| lower.contains(term));
        let mut profiles = Vec::new();
        if importing {
            let home = dirs::home_dir()
                .context("Home directory unavailable")?
                .canonicalize()?;
            for candidate in setup::profile_candidates(&home, cfg!(windows)) {
                if !candidate.try_exists()? {
                    continue;
                }
                let canonical = candidate.canonicalize()?;
                ensure!(
                    canonical.starts_with(&home),
                    "Profile resolves outside the home directory: {}",
                    candidate.display()
                );
                authorize(
                    session,
                    vec![("Read", json!({"file_path":canonical}))],
                    input,
                    &format!(
                        "Read shell profile as text for the selected model: {}",
                        canonical.display()
                    ),
                )
                .await?;
                let text = setup::bounded_read(&canonical, 16384)?;
                // The original preference order is preserved; use the first profile
                // that contains a prompt definition rather than merging startup scripts.
                if setup::has_prompt_definition(&text) {
                    profiles.push(json!({"path":candidate,"text":text}));
                    break;
                }
                eprintln!("No prompt definition in {}", candidate.display());
            }
        }
        if request.is_empty()
            && !profiles
                .iter()
                .any(|profile| setup::has_prompt_definition(profile["text"].as_str().unwrap_or("")))
        {
            eprintln!("No PS1 or PowerShell prompt definition found. Use /statusline setup <desired fields and colors> to describe a status line.");
            return Ok(());
        }
        let payload = payload(session).await?;
        eprintln!(
            "Statusline setup: generating a proposal with {} (no shell or file-write tools).",
            session.model
        );
        let current_command = crate::display_commands::load(session)?.statusline;
        let request = json!({"request":if request.is_empty() {"Import my shell prompt without its trailing prompt marker"} else {request}, "shell":if cfg!(windows) {"Windows PowerShell"} else {"POSIX sh"}, "profiles":profiles, "current_command_untrusted_reference":current_command, "status_json_example":payload});
        let provider = session.selected_provider().await?;
        let request = request.to_string();
        let result = tokio::select! {
            result = deepseek_code_core::native_engine::generation::generate(provider, setup::SYSTEM, &request) => result?,
            _ = tokio::signal::ctrl_c() => bail!("Status line setup cancelled; settings unchanged"),
        };
        let proposal = setup::parse_proposal(&result)?;
        eprintln!(
            "{}",
            proposal
                .explanation
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .collect::<String>()
        );
        let Some(command) = proposal.command else {
            return Ok(());
        };
        command
    };
    let destination = setup::path(&session.paths.data_dir);
    authorize(session, vec![
        ("Bash", json!({"command":command})),
        ("Write", json!({"file_path":destination,"content":serde_json::to_string(&setup::Settings{command:Some(command.clone())})?})),
    ], input, &format!("Configure recurring status line command for this Rust data directory:\n{}\nCommand (escaped):\n{command:?}", destination.display())).await?;
    setup::save(&session.paths.data_dir, Some(command))?;
    eprintln!("Status line saved. It will run at the next prompt. Use /statusline setup <changes> to refine it.");
    Ok(())
}

pub async fn payload(session: &Session) -> Result<Value> {
    let usage = crate::display_commands::usage(session)?;
    let selected = session.selected_provider().await?;
    let title = session.store.resume(&session.id)?.title;
    let (current, added, project_dir) = session.store.db.with_conn(|conn| -> Result<_> {
        let current = setup::current_usage(conn, &session.id)?;
        let project =
            deepseek_code_core::native_engine::workspace::conversation_project(conn, &session.id)?;
        let project_dir = project
            .as_ref()
            .and_then(|p| p.workspace_path.clone())
            .unwrap_or_else(|| session.workspace.to_string_lossy().into());
        let added = project
            .map(|project| {
                project
                    .folders
                    .into_iter()
                    .filter(|f| f.path != project_dir)
                    .map(|f| f.path)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok((current, added, project_dir))
    })??;
    let context_size = selected.model.context_window;
    let used = current.as_ref().and_then(|v| {
        v["input_tokens"].as_u64().map(|input| {
            input
                + v["cache_read_input_tokens"].as_u64().unwrap_or(0)
                + v["cache_creation_input_tokens"].as_u64().unwrap_or(0)
        })
    });
    let percentage = used
        .zip(context_size.filter(|size| *size > 0))
        .map(|(used, size)| (used as f64 / size as f64 * 100.0).clamp(0.0, 100.0));
    Ok(
        json!({"session_id":session.id,"session_name":title,"cwd":session.workspace,"model":{"id":session.model,"display_name":selected.model.name},"workspace":{"current_dir":session.workspace,"project_dir":project_dir,"added_dirs":added},"version":env!("CARGO_PKG_VERSION"),"output_style":{"name":crate::display_commands::load(session)?.output_style},"context_window":{"total_input_tokens":usage["input_tokens"].as_i64().unwrap_or(0),"total_output_tokens":usage["output_tokens"].as_i64().unwrap_or(0),"context_window_size":context_size,"current_usage":current,"used_percentage":percentage,"remaining_percentage":percentage.map(|v|100.0-v)},"usage":usage}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepseek_code_core::{
        permissions::{rules::PermissionAction, PermissionMode},
        runtime::{RuntimePaths, SessionStore},
    };
    use std::sync::{Arc, Mutex};

    async fn session(root: &std::path::Path, base_url: &str) -> Session {
        let paths = RuntimePaths::new(root.join("data")).unwrap();
        let selection = crate::config::Selection{providers:vec![serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":base_url,"api_key":"unused","api_format":"openai","enabled":true,"models":[{"id":"fixture","name":"Fixture Model","enabled":true,"context_window":10000,"supports_vision":false,"supports_web_search":false}]})).unwrap()],provider_id:"fixture".into(),model:"fixture".into()};
        let cli = crate::args::Cli::try_parse_with_mode_source(["dcl"]).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(root, "fixture", "fixture").unwrap();
        Session::open(&cli, &paths, selection, Some(&id))
            .await
            .unwrap()
    }
    fn answers(values: &[&str]) -> TerminalInput {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for value in values {
            tx.send((*value).into()).unwrap();
        }
        TerminalInput::from(rx)
    }
    #[tokio::test]
    async fn statusline_manual_authorization_denial_off_and_global_persistence() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "http://127.0.0.1:1").await;
        assert!(
            handle("command echo denied", &session, &mut answers(&["no"]))
                .await
                .is_err()
        );
        assert!(setup::load(&session.paths.data_dir).unwrap().is_none());
        handle("command echo accepted", &session, &mut answers(&["yes"]))
            .await
            .unwrap();
        assert_eq!(
            setup::load(&session.paths.data_dir)
                .unwrap()
                .unwrap()
                .command
                .as_deref(),
            Some("echo accepted")
        );
        session
            .permissions
            .set_mode(PermissionMode::BypassPermissions);
        session.permissions.set_runtime_rules(
            &session.id,
            vec![std::collections::HashMap::from([(
                "Bash".into(),
                PermissionAction::Deny,
            )])],
        );
        assert!(handle("command echo blocked", &session, &mut answers(&[]))
            .await
            .is_err());
        assert_eq!(
            setup::load(&session.paths.data_dir)
                .unwrap()
                .unwrap()
                .command
                .as_deref(),
            Some("echo accepted")
        );
        session.permissions.clear_runtime_rules(&session.id);
        for mode in [PermissionMode::PlanMode, PermissionMode::DontAsk] {
            session.permissions.set_mode(mode);
            assert!(
                handle("command echo blocked-mode", &session, &mut answers(&[]))
                    .await
                    .is_err()
            );
            assert_eq!(
                setup::load(&session.paths.data_dir)
                    .unwrap()
                    .unwrap()
                    .command
                    .as_deref(),
                Some("echo accepted")
            );
        }
        session.permissions.set_mode(PermissionMode::AskPermissions);
        assert!(
            handle("command echo closed-input", &session, &mut answers(&[]))
                .await
                .is_err()
        );
        session
            .permissions
            .set_mode(PermissionMode::BypassPermissions);
        handle("command echo full-access", &session, &mut answers(&[]))
            .await
            .unwrap();
        handle("off", &session, &mut answers(&[])).await.unwrap();
        assert_eq!(
            crate::display_commands::load(&session).unwrap().statusline,
            None
        );
        let another = self::session(root.path(), "http://127.0.0.1:1").await;
        assert_eq!(
            crate::display_commands::load(&another).unwrap().statusline,
            None
        );
        session.close().await.unwrap();
        another.close().await.unwrap();
    }
    #[tokio::test]
    async fn statusline_natural_language_proposal_is_tool_free_and_not_executed_before_approval() {
        use axum::{extract::State, routing::post, Json, Router};
        let observed = Arc::new(Mutex::new(Vec::<Value>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/chat/completions",post(|State(observed):State<Arc<Mutex<Vec<Value>>>>,Json(body):Json<Value>|async move{
            observed.lock().unwrap().push(body);
            Json(json!({"id":"fixture","object":"chat.completion","created":0,"model":"fixture","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":json!({"command":"echo SETUP_PROPOSED","explanation":"Show a short label"}).to_string()}}]}))
        })).with_state(observed.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), &address).await;
        assert!(
            handle("Show a short label", &session, &mut answers(&["no"]))
                .await
                .is_err()
        );
        assert!(setup::load(&session.paths.data_dir).unwrap().is_none());
        handle("setup Show a short label", &session, &mut answers(&["yes"]))
            .await
            .unwrap();
        assert_eq!(
            setup::load(&session.paths.data_dir)
                .unwrap()
                .unwrap()
                .command
                .as_deref(),
            Some("echo SETUP_PROPOSED")
        );
        let payload = payload(&session).await.unwrap();
        assert_eq!(payload["model"]["display_name"], "Fixture Model");
        assert_eq!(payload["context_window"]["current_usage"], Value::Null);
        handle(
            "setup Make the text green",
            &session,
            &mut answers(&["yes"]),
        )
        .await
        .unwrap();
        let calls = observed.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.get("tools").is_none()));
        assert!(calls[0]["messages"]
            .to_string()
            .contains("Show a short label"));
        let refinement: Value =
            serde_json::from_str(calls[2]["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            refinement["current_command_untrusted_reference"],
            "echo SETUP_PROPOSED"
        );
        drop(calls);
        task.abort();
        session.close().await.unwrap();
    }
}
