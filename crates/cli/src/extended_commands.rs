//! Local commands backed by the shared conversation store and OS clipboard.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use claude_core::{db::conversation_repo, runtime::RuntimePaths};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub async fn handle(
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    let argument = argument.trim();
    match command {
        "/copy" => {
            let number = if argument.is_empty() {
                1
            } else {
                argument
                    .parse::<usize>()
                    .context("Usage: /copy [positive response number]")?
            };
            let history = session.store.history(&session.id)?;
            let text = response(&history, number)?;
            copy_text(text)?;
            eprintln!("Copied {} chars to clipboard.", text.chars().count());
        }
        "/export" => {
            ensure!(
                !argument.is_empty(),
                "Usage: /export FILE or /export clipboard"
            );
            let history = session.store.history(&session.id)?;
            let text = transcript(&session.id, &history);
            if argument == "clipboard" {
                copy_text(&text)?;
                eprintln!("Copied {} chars to clipboard.", text.chars().count());
            } else {
                let path = Path::new(argument.trim_matches('"'));
                export_file(path, &text)?;
                eprintln!("Exported conversation to {}", path.display());
            }
        }
        "/branch" => {
            let id = session.branch_saved(paths, argument).await?;
            eprintln!("Switched to branch {id}; original conversation retained.");
        }
        "/resume" => {
            if argument.is_empty() && crate::terminal::picker::available() {
                // As in the reference CLI: this project's other sessions in a
                // searchable picker, newest first; Ctrl+A shows every project,
                // Space previews and Ctrl+R renames.
                use crate::terminal::picker::{Entry, Key, Picked, Picker};
                let mut all = false;
                let mut this_branch = false;
                let current_branch = git_branch(&session.workspace);
                let mut selected = 0;
                loop {
                    let mut found = resumable(session, all)?;
                    if this_branch {
                        found.retain(|(_, entry)| entry.branch.is_some() && entry.branch == current_branch);
                    }
                    if found.is_empty() && !all && !this_branch {
                        eprintln!("No other conversations in this project. Ctrl+A in /resume shows every project once there are some.");
                        return Ok(true);
                    }
                    let entries: Vec<_> = found.iter().map(|(_, entry)| Entry { title: entry.title.clone(), detail: entry.detail.clone() }).collect();
                    let project = if all { "All projects".to_owned() } else { session.workspace.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default() };
                    let hint = format!(
                        "Ctrl+A to {} \u{b7} Ctrl+B to {} \u{b7} Space to preview \u{b7} Ctrl+R to rename \u{b7} Type to search \u{b7} Esc to cancel",
                        if all { "only show current project" } else { "show all projects" },
                        if this_branch { "show all branches" } else { "only show current branch" },
                    );
                    let picker = Picker { heading: "Resume session", subtitle: Some(&project), entries: &entries, search: true, hint: &hint, selected };
                    match picker.run_keys(&[Key::Ctrl('a'), Key::Ctrl('b'), Key::Space, Key::Ctrl('r')])? {
                        Picked::Entry(index) => {
                            session.resume_saved(paths, &found[index].0).await?;
                            eprintln!("Resumed {} using saved provider {} and model {}.", session.id, session.provider_id, session.model);
                            return Ok(true);
                        }
                        Picked::Key(Key::Ctrl('a'), _) => {
                            all = !all;
                            selected = 0;
                        }
                        Picked::Key(Key::Ctrl('b'), _) => {
                            this_branch = !this_branch;
                            selected = 0;
                        }
                        Picked::Key(Key::Space, Some(index)) => {
                            selected = index;
                            crate::terminal::transcript_pager::view(&crate::repl::transcript_of(session, &found[index].0)?)?;
                        }
                        Picked::Key(Key::Ctrl('r'), Some(index)) => {
                            selected = index;
                            if let Some(title) = crate::terminal::picker::text_input("Rename session:", &found[index].1.title)?.filter(|title| !title.is_empty()) {
                                let title: String = title.chars().take(200).collect();
                                session.store.db.with_conn(|conn| conversation_repo::update_conversation_title(conn, &found[index].0, &title))??;
                            }
                        }
                        Picked::Key(..) => {}
                        Picked::Cancel => return Ok(true),
                    }
                }
            } else if argument.is_empty() {
                let sessions = session
                    .store
                    .db
                    .with_conn(conversation_repo::list_conversations)??;
                println!("{}", serde_json::to_string_pretty(&sessions)?);
                eprintln!("Use /resume ID to switch conversations.");
            } else {
                session.resume_saved(paths, argument).await?;
                eprintln!(
                    "Resumed {} using saved provider {} and model {}.",
                    session.id, session.provider_id, session.model
                );
            }
        }
        "/plan" => {
            ensure!(
                matches!(argument, "" | "show" | "open"),
                "Usage: /plan [show|open|DESCRIPTION]"
            );
            if argument.is_empty() && session.permission_mode() != "plan" {
                session.set_permission_mode("plan").await?;
                let path = claude_core::services::plans::path(&paths.data_dir, &session.id)?;
                eprintln!("Plan mode enabled. Plan file: {}. Claude will submit it for approval before implementation.", path.display());
            } else {
                let path = claude_core::services::plans::path(&paths.data_dir, &session.id)?;
                let plan = claude_core::services::plans::read(&paths.data_dir, &session.id)?;
                if argument == "open" {
                    ensure!(plan.is_some(), "No plan written yet. Describe the task in plan mode first.");
                    crate::display_commands::edit_file(&path, "").await?;
                    eprintln!("Edited plan: {}. Claude must request approval of the updated document.", path.display());
                } else if let Some(plan) = plan {
                    println!("Current Plan\n{}\n\n{plan}", path.display());
                } else {
                    eprintln!("No plan written yet. Plan file: {}. Describe the task in plan mode first.", path.display());
                }
            }
        }
        "/terminal-setup" => {
            ensure!(argument.is_empty(), "Usage: /terminal-setup");
            use std::io::IsTerminal;
            eprintln!("Interactive input: {}\nInteractive output: {}\nTab: complete commands; arrow keys: edit/history.\nMultiline: /multiline followed by /end, or a trailing backslash.\nClipboard: /copy [N]. Mouse selection and automatic copy are controlled by your terminal.", std::io::stdin().is_terminal(), std::io::stdout().is_terminal());
        }
        "/agents" => {
            // As in the reference CLI, subagents are chosen by asking for one;
            // this lists what is available.
            let context = crate::services::context(paths)?;
            let agents = claude_core::services::agents::list(&context)?;
            let width = agents.iter().map(|agent| agent.id.chars().count()).max().unwrap_or(0) + 2;
            let mut text = String::from("  \u{23bf}  Ask DeepSeek Code to hand work to a subagent (e.g. \"use the explore agent to find where errors are logged\").\n     Available subagents:\n");
            for agent in agents {
                let description = agent.description.lines().next().unwrap_or_default().chars().take(100).collect::<String>();
                text.push_str(&format!("       \u{2022} {:<width$}{description}\n", agent.id));
            }
            text.push_str("     /list-agents shows the sessions running now.");
            eprintln!("{text}");
        }
        "/list-agents" => {
            if argument == "definitions" {
                let context = crate::services::context(paths)?;
                for agent in claude_core::services::agents::list(&context)? {
                    println!("{}: {}", agent.id, agent.description);
                }
            } else {
                let peers = peer_sessions(
                    &session.store,
                    &paths.data_dir,
                    &session.workspace,
                    &session.id,
                )?;
                let (verb, rest) = argument
                    .split_once(char::is_whitespace)
                    .unwrap_or((argument, ""));
                let request = match verb {
                    "" | "list" => {
                        ensure!(rest.trim().is_empty(), "Usage: /list-agents [list]");
                        let mut observed=peers.clone();
                        for peer in &mut observed {
                            let id=peer["id"].as_str().unwrap_or_default();
                            let runtime=claude_core::services::session_worker::runtime_status(&paths.data_dir,id).await;
                            peer["worker"]=serde_json::to_value(runtime)?;
                        }
                        println!("{}", serde_json::to_string_pretty(&observed)?);
                        eprintln!("Trusted source-project sessions, including worktrees. kind distinguishes teammate, team-lead, subagent and independent session; teammates also report team, teammate_name and member_status. receiver_active is an OS-locked model consumer; worker reports a bounded live probe. Saved alone does not mean running. /list-agents message SESSION_ID TEXT queues context; inbox/sent show delivery state, read MESSAGE_ID marks human read only. Idle recipients consume messages on their next explicit turn. /list-agents inbox legacy reads the old project mailbox.");
                        None
                    }
                    "message" => {
                        let (to, content) = rest.trim().split_once(char::is_whitespace).context("Usage: /list-agents message SESSION_ID TEXT")?;
                        ensure!(peers.iter().any(|peer| peer["id"] == to), "Recipient must be a listed session in this project");
                        ensure!(!content.trim().is_empty(), "Message must not be empty");
                        Some(serde_json::json!({"operation":"send","to":to,"content":content.trim()}))
                    }
                    "inbox" if rest.trim()=="legacy" => {
                        let result=claude_core::tools::agent_messages::execute_for_session(serde_json::json!({"operation":"receive"}),&session.workspace.to_string_lossy(),&session.id)?;
                        println!("{}",serde_json::to_string_pretty(&result)?);
                        eprintln!("Legacy repository mailbox; these rows are not automatically injected into live context.");
                        None
                    }
                    "inbox" => {
                        ensure!(rest.trim().is_empty(), "Usage: /list-agents inbox [legacy]");
                        Some(serde_json::json!({"operation":"receive","unread_only":false}))
                    }
                    "sent" => {
                        ensure!(rest.trim().is_empty(), "Usage: /list-agents sent");
                        Some(serde_json::json!({"operation":"sent"}))
                    }
                    "read" if rest.trim().starts_with("legacy ") => {
                        let id=rest.trim().strip_prefix("legacy ").unwrap().trim();
                        ensure!(!id.is_empty() && !id.contains(char::is_whitespace), "Usage: /list-agents read legacy MESSAGE_ID");
                        let result=claude_core::tools::agent_messages::execute_for_session(serde_json::json!({"operation":"mark_read","message_id":id}),&session.workspace.to_string_lossy(),&session.id)?;
                        println!("{}",serde_json::to_string_pretty(&result)?);
                        None
                    }
                    "read" => {
                        ensure!(!rest.trim().is_empty() && !rest.trim().contains(char::is_whitespace), "Usage: /list-agents read MESSAGE_ID");
                        Some(serde_json::json!({"operation":"mark_read","message_id":rest.trim()}))
                    }
                    _ => anyhow::bail!("Usage: /list-agents [list|definitions|message SESSION_ID TEXT|inbox [legacy]|sent|read MESSAGE_ID]"),
                };
                if let Some(request) = request {
                    let result = claude_core::tools::agent_messages::execute_live(
                        &crate::services::context(paths)?, &session.id, request,
                    )?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                    if verb == "message" {
                        eprintln!("Queued durably. Active recipients receive it at the next safe model boundary; idle/offline recipients need their next explicit turn. context_committed means saved context, not proof of model processing. Check /list-agents sent.");
                    }
                }
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn peer_sessions(
    _store: &claude_core::runtime::SessionStore,
    root: &Path,
    _workspace: &Path,
    current: &str,
) -> Result<Vec<Value>> {
    let context=claude_core::services::session_messages::context(root)?;
    let mut peers=claude_core::services::session_messages::peers(&context,current)?;
    for peer in &mut peers {
        let id=peer["id"].as_str().unwrap_or_default().to_owned();
        peer["status"]=serde_json::json!(if id==current {"attached"} else if peer["receiver_active"]==true {"active_receiver"} else {"saved"});
        peer["kind"]=serde_json::json!("session");
    }
    claude_core::services::teams::classify_peers(root,&context,&mut peers)?;
    Ok(peers)
}

/// `/plan DESCRIPTION` enables plan mode and returns the task to submit once,
/// as upstream does. Already in plan mode, the description is not submitted.
pub async fn plan_task(argument: &str, paths: &RuntimePaths, session: &mut Session) -> Result<Option<String>> {
    let description = argument.trim();
    if matches!(description, "" | "show" | "open") {
        handle("/plan", description, paths, session).await?;
        return Ok(None);
    }
    if session.permission_mode() == "plan" {
        handle("/plan", "show", paths, session).await?;
        eprintln!("Already in plan mode; the description was not submitted. Send it as a normal message.");
        return Ok(None);
    }
    handle("/plan", "", paths, session).await?;
    Ok(Some(description.to_owned()))
}

fn response(history: &[Value], number: usize) -> Result<&str> {
    ensure!(number > 0, "Response number must be positive");
    history
        .iter()
        .rev()
        .filter(|row| row["role"] == "assistant")
        .filter_map(|row| row["content"].as_str())
        .filter(|text| !text.is_empty())
        .nth(number - 1)
        .context("No matching assistant response in this conversation")
}

fn transcript(id: &str, history: &[Value]) -> String {
    let mut text = format!("# Conversation {id}\n\n");
    for row in history {
        text.push_str(&format!(
            "## {}\n\n{}\n\n",
            row["role"].as_str().unwrap_or("unknown"),
            row["content"].as_str().unwrap_or("")
        ));
    }
    text
}

fn export_file(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "Cannot create {}; existing files are never overwritten",
                path.display()
            )
        })?;
    file.write_all(text.as_bytes())
        .context("Writing transcript")?;
    file.flush()?;
    Ok(())
}

fn copy_text(text: &str) -> Result<()> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("powershell.exe");
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference = 'Stop'; [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false); Set-Clipboard -Value ([Console]::In.ReadToEnd())"]);
        command.creation_flags(0x08000000);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = Command::new("pbcopy");
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut command = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        Command::new("wl-copy")
    } else {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard"]);
        command
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Cannot start OS clipboard helper")?;
    let written = child
        .stdin
        .take()
        .context("Clipboard stdin unavailable")?
        .write_all(text.as_bytes());
    let output = child
        .wait_with_output()
        .context("Waiting for clipboard helper")?;
    written.context("Sending text to clipboard helper")?;
    ensure!(
        output.status.success(),
        "Clipboard helper failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn peer_list_scopes_projects_and_mailboxes_do_not_spoof_identity() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let store = claude_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let project = dir.path().join("project");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let parent = store.create(&project, "test", "cli-runtime").unwrap();
        let child = store.create(&project, "test", "cli-runtime").unwrap();
        store.create(&other, "test", "cli-runtime").unwrap();
        let peers = peer_sessions(&store, &paths.data_dir, &project, &parent).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(
            peers.iter().find(|p| p["id"] == parent).unwrap()["status"],
            "attached"
        );
        assert_eq!(
            peers.iter().find(|p| p["id"] == child).unwrap()["status"],
            "saved"
        );
        let sent = claude_core::tools::agent_messages::execute_for_session(
            json!({"operation":"send","to":child,"from":"spoof","content":"check"}),
            project.to_str().unwrap(),
            &parent,
        )
        .unwrap();
        assert_eq!(sent["from"], parent);
        let inbox = claude_core::tools::agent_messages::execute_for_session(
            json!({"operation":"receive"}),
            project.to_str().unwrap(),
            &child,
        )
        .unwrap();
        assert_eq!(inbox["messages"][0]["id"], sent["id"]);
    }

    #[test]
    fn copy_uses_nth_nonempty_assistant_response() {
        let history = vec![
            json!({"role":"assistant","content":"第一条"}),
            json!({"role":"user","content":"question"}),
            json!({"role":"assistant","content":""}),
            json!({"role":"assistant","content":"last"}),
        ];
        assert_eq!(response(&history, 1).unwrap(), "last");
        assert_eq!(response(&history, 2).unwrap(), "第一条");
        assert!(response(&history, 0).is_err());
        assert!(response(&history, 3).is_err());
    }

    #[test]
    fn export_preserves_utf8_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.md");
        let text = transcript("test", &[json!({"role":"assistant","content":"中文 🚀"})]);
        export_file(&path, &text).unwrap();
        assert!(export_file(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }
}

/// `24 minutes ago`, as session lists in the reference CLI show it.
pub(crate) fn ago(time: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(time) else {
        return String::new();
    };
    let seconds = (chrono::Utc::now() - time.with_timezone(&chrono::Utc)).num_seconds().max(0);
    let (count, unit) = match seconds {
        0..=59 => (seconds, "second"),
        60..=3599 => (seconds / 60, "minute"),
        3600..=86_399 => (seconds / 3600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" })
}

pub(crate) struct Resumable {
    pub title: String,
    pub detail: String,
    pub branch: Option<String>,
}

/// The workspace's git branch, as the reference CLI lists sessions by it.
fn git_branch(workspace: &Path) -> Option<String> {
    claude_core::git::GitIntegration::new(workspace.to_path_buf())
        .get_current_branch()
        .ok()
        .filter(|branch| !branch.is_empty())
}

/// Remembers the branch a session is working on, at each turn.
pub(crate) fn record_branch(session: &Session) {
    let Some(branch) = git_branch(&session.workspace) else {
        return;
    };
    let _ = session.store.db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [format!("cli.git-branch.{}", session.id), branch],
        )
    });
}

/// `168KB` or `167.2KB`, as the reference CLI shows a session's size.
fn short_size(bytes: i64) -> String {
    let format = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    };
    match bytes {
        ..=1023 => format!("{}B", bytes.max(0)),
        1024..=1_048_575 => format(bytes as f64 / 1024.0, "KB"),
        _ => format(bytes as f64 / 1_048_576.0, "MB"),
    }
}

/// This workspace's (or with `all` every project's) other unarchived
/// sessions, newest first, titled by their name or first prompt, with the
/// branch and saved size of each (messages, tool calls and the request
/// snapshots, as the reference CLI's transcript files hold them).
fn resumable(session: &Session, all: bool) -> Result<Vec<(String, Resumable)>> {
    let here = session.workspace.canonicalize().unwrap_or_else(|_| session.workspace.clone());
    session.store.db.with_conn(|conn| -> Result<Vec<(String, Resumable)>> {
        let mut found = Vec::new();
        for row in conversation_repo::list_conversations(conn)? {
            let same = row.workspace_path.as_deref().map(|path| Path::new(path).canonicalize().unwrap_or_else(|_| Path::new(path).to_path_buf())) == Some(here.clone());
            if row.id == session.id || row.archived || !(same || all) || row.message_count == 0 {
                continue;
            }
            let named = row.title.clone().filter(|title| !title.trim().is_empty() && title != "CLI session");
            let title = match named {
                Some(title) => title,
                None => claude_core::db::message_repo::get_messages_by_conversation(conn, &row.id)?
                    .into_iter()
                    .find(|message| message.role == "user" && !crate::background_commands::is_notification(&message.content))
                    .map(|message| message.content.lines().next().unwrap_or_default().to_owned())
                    .unwrap_or_else(|| "(no prompt)".into()),
            };
            // No row: the session was never used in a git repository.
            let branch: Option<String> = conn
                .query_row("SELECT value FROM peripheral_settings WHERE key=?1", [format!("cli.git-branch.{}", row.id)], |r| r.get(0))
                .ok();
            let size: i64 = conn.query_row(
                "SELECT (SELECT COALESCE(SUM(length(CAST(content AS BLOB)) + COALESCE(length(CAST(thinking AS BLOB)),0)),0) FROM messages WHERE conversation_id=?1)
                      + (SELECT COALESCE(SUM(COALESCE(length(CAST(t.input AS BLOB)),0) + COALESCE(length(CAST(t.output AS BLOB)),0)),0) FROM tool_calls t JOIN messages m ON t.message_id=m.id WHERE m.conversation_id=?1)
                      + (SELECT COALESCE(SUM(length(CAST(r.requests_json AS BLOB))),0) FROM message_trajectory r JOIN messages m ON r.message_id=m.id WHERE m.conversation_id=?1)",
                [&row.id],
                |r| r.get(0),
            )?;
            let mut detail = ago(&row.updated_at);
            if let Some(branch) = &branch {
                detail.push_str(&format!(" \u{b7} {branch}"));
            }
            detail.push_str(&format!(" \u{b7} {}", short_size(size)));
            if all {
                if let Some(path) = &row.workspace_path {
                    detail.push_str(&format!(" \u{b7} {}", crate::terminal::home_path(Path::new(path))));
                }
            }
            found.push((row.id, Resumable { title, detail, branch }));
            if found.len() == 50 {
                break;
            }
        }
        Ok(found)
    })?
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    #[test]
    fn sizes_and_times_read_like_the_reference_list() {
        assert_eq!(short_size(512), "512B");
        assert_eq!(short_size(168 * 1024), "168KB");
        assert_eq!(short_size(171_213), "167.2KB");
        assert_eq!(short_size(3 * 1_048_576 / 2), "1.5MB");
        let now = chrono::Utc::now();
        assert_eq!(ago(&(now - chrono::Duration::seconds(32)).to_rfc3339()), "32 seconds ago");
        assert_eq!(ago(&(now - chrono::Duration::hours(1)).to_rfc3339()), "1 hour ago");
    }
}
