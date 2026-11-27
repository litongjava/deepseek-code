//! Local commands backed by the shared conversation store and OS clipboard.
#[cfg(test)]
use crate::fixture_sql::FixtureSql;
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use deepseek_code_core::runtime::RuntimePaths;
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
        "/copy-session-id" => {
            ensure!(argument.is_empty(), "Usage: /copy-session-id");
            command_eprintln!("{}", copy_session_id(&session.id)?);
        }
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
            command_eprintln!("{}", copy_with_file(text)?);
        }
        "/export" => {
            let history = session.store.history(&session.id)?;
            let text = transcript(&session.id, &history);
            let argument = if argument.is_empty() {
                // As in the reference CLI: choose how to export.
                ensure!(crate::terminal::picker::available(), "Usage: /export FILE or /export clipboard");
                use crate::terminal::picker::{Entry, Picker};
                let entries = [
                    Entry { title: "Copy to clipboard".into(), detail: "Copy the conversation to your system clipboard".into() },
                    Entry { title: "Save to file".into(), detail: "Save the conversation to a file in the current directory".into() },
                ];
                let picker = Picker { heading: "Export conversation", subtitle: Some("Select export method"), entries: &entries, search: false, hint: "Enter to confirm \u{b7} Esc to cancel", selected: 0 };
                match picker.run()? {
                    Some(0) => "clipboard".to_owned(),
                    Some(_) => {
                        let default = format!("conversation-{}.txt", chrono::Local::now().format("%Y-%m-%d-%H%M%S"));
                        match crate::terminal::picker::text_input("Save to file:", &default)?.filter(|name| !name.is_empty()) {
                            Some(name) => name,
                            None => {
                                command_eprintln!("  \u{23bf}  Export cancelled");
                                return Ok(true);
                            }
                        }
                    }
                    None => {
                        command_eprintln!("  \u{23bf}  Export cancelled");
                        return Ok(true);
                    }
                }
            } else {
                argument.to_owned()
            };
            let argument = argument.as_str();
            if argument == "clipboard" {
                command_eprintln!("{}", copy_with_file(&text)?);
            } else {
                let path = Path::new(argument.trim_matches('"'));
                export_file(path, &text)?;
                command_eprintln!("Exported conversation to {}", path.display());
            }
        }
        "/branch" => {
            let id = session.branch_saved(paths, argument).await?;
            command_eprintln!("Switched to branch {id}; original conversation retained.");
        }
        "/resume" => {
            if argument.is_empty() && crate::terminal::picker::available() {
                // As in the reference CLI: this project's other sessions in a
                // searchable picker, newest first; Ctrl+A shows every project,
                // Space previews and Ctrl+R renames.
                use crate::terminal::picker::{Entry, Key, Picked, Picker};
                // Inside a read-only missing-directory session, show every
                // project first so switching back to a valid session is one step.
                let mut all = session.is_missing_workspace();
                let mut this_branch = false;
                let current_branch = git_branch(&session.workspace);
                let mut selected = 0;
                loop {
                    let mut found = resumable(session, all)?;
                    if this_branch {
                        found.retain(|(_, entry)| entry.branch.is_some() && entry.branch == current_branch);
                    }
                    let entries: Vec<_> = found.iter().map(|(_, entry)| Entry { title: entry.title.clone(), detail: entry.detail.clone() }).collect();
                    let project = format!("{}   {}", if all { "cwd  [all]" } else { "[cwd]  all" }, session.workspace.display());
                    let rename = if session.is_missing_workspace() { "" } else { "Ctrl+R: rename · " };
                    let hint = format!("Tab: cwd/all · Ctrl+A: cwd/all · Ctrl+B: {} · Space: preview · {rename}Type to search · Esc: cancel", if this_branch { "all branches" } else { "current branch" });
                    let picker = Picker { heading: "Resume session", subtitle: Some(&project), entries: &entries, search: true, hint: &hint, selected };
                    let mut keys = vec![Key::Tab, Key::Ctrl('a'), Key::Ctrl('b'), Key::Space];
                    if !session.is_missing_workspace() {
                        keys.push(Key::Ctrl('r'));
                    }
                    match picker.run_keys(&keys)? {
                        Picked::Entry(index) => {
                            session.resume_saved(paths, &found[index].0).await?;
                            crate::repl::show_resumed(session, true)?;
                            return Ok(true);
                        }
                        Picked::Key(Key::Ctrl('a') | Key::Tab, _) => {
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
                        Picked::Key(Key::Ctrl('r'), Some(index))
                            if !session.is_missing_workspace() =>
                        {
                            selected = index;
                            if let Some(title) = crate::terminal::picker::text_input("Rename session:", &found[index].1.title)?.filter(|title| !title.is_empty()) {
                                let title: String = title.chars().take(200).collect();
                                session.store.db.rename_session(&found[index].0, &title)?;
                            }
                        }
                        Picked::Key(..) => {}
                        Picked::Cancel => {
                            command_eprintln!("  \u{23bf}  Resume cancelled");
                            return Ok(true);
                        }
                    }
                }
            } else if argument.is_empty() {
                let sessions = sessions_with_running(session)?;
                command_println!("{}", serde_json::to_string_pretty(&sessions)?);
                command_eprintln!("Use /resume ID to switch conversations.");
            } else {
                session.resume_saved(paths, argument).await?;
                crate::repl::show_resumed(session, true)?;
            }
        }
        "/plan" => {
            ensure!(
                matches!(argument, "" | "show" | "open"),
                "Usage: /plan [show|open|DESCRIPTION]"
            );
            if argument.is_empty() && session.permission_mode() != "plan" {
                session.set_permission_mode("plan").await?;
                let path = deepseek_code_core::services::plans::path(&paths.data_dir, &session.id)?;
                command_eprintln!("Plan mode enabled. Plan file: {}. DeepSeek Code will submit it for approval before implementation.", path.display());
            } else {
                let path = deepseek_code_core::services::plans::path(&paths.data_dir, &session.id)?;
                let plan = deepseek_code_core::services::plans::read(&paths.data_dir, &session.id)?;
                if argument == "open" {
                    ensure!(plan.is_some(), "No plan written yet. Describe the task in plan mode first.");
                    crate::display_commands::edit_file(&path, "").await?;
                    command_eprintln!("Edited plan: {}. DeepSeek Code must request approval of the updated document.", path.display());
                } else if let Some(plan) = plan {
                    command_println!("Current Plan\n{}\n\n{plan}", path.display());
                } else {
                    command_eprintln!("No plan written yet. Plan file: {}. Describe the task in plan mode first.", path.display());
                }
            }
        }
        "/terminal-setup" => {
            ensure!(argument.is_empty(), "Usage: /terminal-setup");
            command_eprintln!("{}", terminal_setup());
        }
        "/agents" => {
            let usage = "Usage: /agents [definitions|model [TYPE [MODEL|default]]]";
            let (verb, rest) = argument
                .split_once(char::is_whitespace)
                .unwrap_or((argument, ""));
            match verb {
                "" => command_eprintln!(
                    "{}",
                    active_agents_for(paths, &session.selected_session()?)?
                ),
                "definitions" if rest.trim().is_empty() => {
                    for agent in
                        deepseek_code_core::services::agents::list(&crate::services::context(paths)?)?
                    {
                        command_eprintln!("- {}: {}", agent.id, agent.description);
                    }
                }
                "model" => {
                    let mut parts = rest.split_whitespace();
                    let agent = parts.next();
                    let model = parts.next();
                    ensure!(parts.next().is_none(), "{usage}");
                    command_eprintln!(
                        "{}",
                        crate::agent_commands::model_command(paths, agent, model)?
                    );
                }
                _ => anyhow::bail!("{usage}"),
            }
        }
        "/list-agents" => {
            if argument == "definitions" {
                let context = crate::services::context(paths)?;
                for agent in deepseek_code_core::services::agents::list(&context)? {
                    command_println!("{}: {}", agent.id, agent.description);
                }
            } else {
                let owner = session.selected_session()?;
                let _owner = owner.hold_incarnation()?;
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
                            let peer_session = session.store.db.open_recorded(id,peer["incarnation"].as_str().context("Missing peer incarnation")?)?;
                            let _peer = peer_session.hold_incarnation()?;
                            let runtime=deepseek_code_core::services::session_worker::runtime_status(&paths.data_dir,id).await;
                            peer["worker"]=serde_json::to_value(runtime)?;
                        }
                        command_println!("{}", serde_json::to_string_pretty(&observed)?);
                        command_eprintln!("Current session family, including its parent and child agents. kind distinguishes teammate, team-lead, subagent and independent session; teammates also report team, teammate_name and member_status. receiver_active is an OS-locked model consumer; worker reports a bounded live probe. Saved alone does not mean running. /list-agents message SESSION_ID TEXT queues context; inbox/sent show delivery state, read MESSAGE_ID marks human read only. Idle recipients consume messages on their next explicit turn. /list-agents inbox legacy reads the old project mailbox.");
                        None
                    }
                    "message" => {
                        let (to, content) = rest.trim().split_once(char::is_whitespace).context("Usage: /list-agents message SESSION_ID TEXT")?;
                        ensure!(peers.iter().any(|peer| peer["id"] == to), "Recipient must be a listed session in this project");
                        ensure!(!content.trim().is_empty(), "Message must not be empty");
                        Some(serde_json::json!({"operation":"send","to":to,"content":content.trim()}))
                    }
                    "inbox" if rest.trim()=="legacy" => {
                        let result=deepseek_code_core::tools::agent_messages::execute_for_session(serde_json::json!({"operation":"receive"}),&session.workspace.to_string_lossy(),&session.id)?;
                        command_println!("{}",serde_json::to_string_pretty(&result)?);
                        command_eprintln!("Legacy repository mailbox; these rows are not automatically injected into live context.");
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
                        let result=deepseek_code_core::tools::agent_messages::execute_for_session(serde_json::json!({"operation":"mark_read","message_id":id}),&session.workspace.to_string_lossy(),&session.id)?;
                        command_println!("{}",serde_json::to_string_pretty(&result)?);
                        None
                    }
                    "read" => {
                        ensure!(!rest.trim().is_empty() && !rest.trim().contains(char::is_whitespace), "Usage: /list-agents read MESSAGE_ID");
                        Some(serde_json::json!({"operation":"mark_read","message_id":rest.trim()}))
                    }
                    _ => anyhow::bail!("Usage: /list-agents [list|definitions|message SESSION_ID TEXT|inbox [legacy]|sent|read MESSAGE_ID]"),
                };
                if let Some(request) = request {
                    let result = deepseek_code_core::tools::agent_messages::execute_live(
                        &crate::services::context(paths)?, &session.id, request,
                    )?;
                    command_println!("{}", serde_json::to_string_pretty(&result)?);
                    if verb == "message" {
                        command_eprintln!("Queued durably. Active recipients receive it at the next safe model boundary; idle/offline recipients need their next explicit turn. context_committed means saved context, not proof of model processing. Check /list-agents sent.");
                    }
                }
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(crate) fn peer_sessions(
    _store: &deepseek_code_core::runtime::SessionStore,
    root: &Path,
    _workspace: &Path,
    current: &str,
) -> Result<Vec<Value>> {
    let context = deepseek_code_core::services::session_messages::context(root)?;
    let mut peers =
        deepseek_code_core::services::session_messages::session_peers(&context, current)?;
    for peer in &mut peers {
        let id=peer["id"].as_str().unwrap_or_default().to_owned();
        peer["status"]=serde_json::json!(if id==current {"attached"} else if peer["receiver_active"]==true {"active_receiver"} else {"saved"});
        peer["kind"]=serde_json::json!("session");
    }
    deepseek_code_core::services::teams::classify_peers(root,&context,&mut peers)?;
    Ok(peers)
}

/// Show runtime activity, not merely saved agent definitions or old session rows.
#[cfg(test)]
pub(crate) fn active_agents_text(paths: &RuntimePaths, current: &str) -> Result<String> {
    active_agents_for(paths, &paths.open_database()?.open_session(current)?)
}

pub(crate) fn active_agents_for(
    paths: &RuntimePaths,
    owner: &deepseek_code_core::db::session_store::SessionDb,
) -> Result<String> {
    let _owner = owner.hold_incarnation()?;
    let current = owner.id();
    use deepseek_code_core::services::{background_sessions, session_messages};
    let context = crate::services::context(paths)?;
    let peers = session_messages::session_peers(&context, current)?;

    let mut rows = Vec::new();
    for peer in peers {
        let id = peer["id"].as_str().unwrap_or_default();
        let peer_session = context.db.open_recorded(
            id,
            peer["incarnation"]
                .as_str()
                .context("Missing peer incarnation")?,
        )?;
        let _peer = peer_session.hold_incarnation()?;
        let jobs = background_sessions::executing_in_session(&paths.data_dir, &peer_session)?;
        let job = jobs.iter().find(|job| {
            matches!(job.status.as_str(), "starting" | "running")
        });
        if peer["receiver_active"] != true && job.is_none() {
            continue;
        }
        let delegation = context
            .child_progress(&id)?
            .unwrap_or_default();
        let title = delegation["description"]
            .as_str()
            .or_else(|| peer["title"].as_str().filter(|title| !title.is_empty() && *title != "CLI session"))
            .or_else(|| job.map(|job| if crate::background_commands::is_notification(&job.title) {
                "Processing task notifications"
            } else { job.title.as_str() }))
            .unwrap_or("Agent");
        let title: String = title.chars().filter(|ch| !ch.is_control()).collect();
        let status = job.map_or("running", |job| job.status.as_str());
        rows.push(format!(
            "- {title}{}\n  Session: {id}\n  Status: {status}",
            if id == current { " (current)" } else { "" }
        ));
        if let Some(job) = job {
            rows.last_mut()
                .unwrap()
                .push_str(&format!("\n  Task: {} · /tasks show {}", job.id, job.id));
            if let Some(question) = background_sessions::pending_question(
                &paths.data_dir,
                &job.id,
                &job.parent_session_id,
            )? {
                rows.last_mut()
                    .unwrap()
                    .push_str(&format!("\n  Waiting for {}", question.kind));
            }
        }
        if let Some(tool) = delegation["current_tool"]
            .as_str()
            .filter(|tool| !tool.is_empty())
        {
            let tool: String = tool.chars().filter(|ch| !ch.is_control()).collect();
            rows.last_mut()
                .unwrap()
                .push_str(&format!("\n  Tool: {tool}"));
        }
    }
    let mut text = if rows.is_empty() {
        "No active agents in this session family.".to_owned()
    } else {
        format!(
            "Active agents in this session family: {}\n\n{}",
            rows.len(),
            rows.join("\n\n")
        )
    };
    text.push_str("\n\n/agents definitions lists available agent types; /list-agents includes saved sessions and message delivery.");
    Ok(text)
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
        command_eprintln!("Already in plan mode; the description was not submitted. Send it as a normal message.");
        return Ok(None);
    }
    handle("/plan", "", paths, session).await?;
    Ok(Some(description.to_owned()))
}

pub(crate) fn copy_session_id(id: &str) -> Result<String> {
    copy_text(id).context("Could not copy session ID to clipboard")?;
    Ok(format!("Copied session ID to clipboard: {id}"))
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

/// Copies like the reference CLI: the system clipboard, or the terminal's
/// own clipboard (OSC 52) when no clipboard program is available, and always
/// a file with the text too.
fn copy_with_file(text: &str) -> Result<String> {
    let copied = copy_text(text).is_ok();
    let directory = std::env::temp_dir().join("deepseek-code");
    std::fs::create_dir_all(&directory)?;
    let file = directory.join("response.md");
    std::fs::write(&file, text)?;
    let lines = text.lines().count().max(1);
    let summary = format!("({} characters, {lines} lines)", text.chars().count());
    Ok(if copied {
        format!("  \u{23bf}  Copied to clipboard {summary}\n     Also written to {}", file.display())
    } else {
        format!("  \u{23bf}  No clipboard available {summary}\n     Written to {}", file.display())
    })
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| value | (*byte as u32) << (16 - index * 8));
        for index in 0..4 {
            encoded.push(if index <= chunk.len() { ALPHABET[(value >> (18 - index * 6)) as usize & 63] as char } else { '=' });
        }
    }
    encoded
}

/// Sends the text to the terminal's clipboard with OSC 52.
fn copy_osc52(text: &str) -> bool {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return false;
    }
    let Some(sequence) = clipboard_sequence(text) else {
        return false;
    };
    crate::terminal::write_clipboard(&sequence).is_ok()
}

fn clipboard_sequence(text: &str) -> Option<String> {
    (text.len() <= 74_994).then(|| format!("\x1b]52;c;{}\x07", base64(text.as_bytes())))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardTarget {
    Tmux,
    Osc52,
    System,
}

/// Route the text to the clipboard the user can actually paste from. Shell
/// helpers write the machine running dcl, which is the wrong computer when the
/// session is remote; tmux and OSC 52 hand the text to the terminal on the
/// other end of the SSH connection instead.
fn clipboard_order(tmux: bool, ssh: bool) -> Vec<ClipboardTarget> {
    if tmux {
        vec![
            ClipboardTarget::Tmux,
            ClipboardTarget::Osc52,
            ClipboardTarget::System,
        ]
    } else if ssh {
        vec![ClipboardTarget::Osc52, ClipboardTarget::System]
    } else {
        vec![ClipboardTarget::System, ClipboardTarget::Osc52]
    }
}

pub(crate) fn copy_text(text: &str) -> Result<()> {
    let tmux = std::env::var_os("TMUX").is_some();
    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    let mut error = None;
    for target in clipboard_order(tmux, ssh) {
        let copied = match target {
            ClipboardTarget::Tmux => copy_tmux(text).is_ok(),
            ClipboardTarget::Osc52 => copy_osc52(text),
            ClipboardTarget::System => match copy_system(text) {
                Ok(()) => true,
                Err(cause) => {
                    error = Some(cause);
                    false
                }
            },
        };
        if copied {
            return Ok(());
        }
    }
    Err(error.unwrap_or_else(|| anyhow::anyhow!("No clipboard is available")))
}

/// `load-buffer -w` also forwards the text to the outer terminal's clipboard.
fn copy_tmux(text: &str) -> Result<()> {
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = child.stdin.take().context("tmux stdin unavailable")?.write_all(text.as_bytes());
    let status = child.wait()?;
    written?;
    ensure!(status.success(), "tmux load-buffer failed");
    Ok(())
}

fn copy_system(text: &str) -> Result<()> {
    // In-process first: a drag selection copies without a PowerShell start-up pause.
    #[cfg(windows)]
    if crate::images::native_clipboard::set_text(text) {
        return Ok(());
    }
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
    fn clipboard_route_reaches_the_terminal_on_the_other_end_of_ssh() {
        use ClipboardTarget::{Osc52, System, Tmux};
        assert_eq!(clipboard_order(true, true), vec![Tmux, Osc52, System]);
        assert_eq!(clipboard_order(true, false), vec![Tmux, Osc52, System]);
        assert_eq!(clipboard_order(false, true), vec![Osc52, System]);
        assert_eq!(clipboard_order(false, false), vec![System, Osc52]);
    }

    #[test]
    fn clipboard_sequence_never_contains_display_controls_from_the_payload() {
        let text = "session\x1b[2J\x1b[H\n\x07中文";
        let sequence = clipboard_sequence(text).unwrap();
        assert_eq!(
            sequence,
            format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
        );
        assert_eq!(sequence.matches('\x1b').count(), 1);
        assert_eq!(sequence.matches('\x07').count(), 1);
        assert!(!sequence.contains('\n'));
        assert!(clipboard_sequence(&"x".repeat(74_994)).is_some());
        assert!(clipboard_sequence(&"x".repeat(74_995)).is_none());
    }

    #[test]
    fn active_agents_require_live_execution_and_exclude_unrelated_sessions() {
        use deepseek_code_core::services::session_messages::Consumer;
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(dir.path(), "test", "cli-runtime").unwrap();
        let child = store.create(dir.path(), "test", "cli-runtime").unwrap();
        crate::fixture_sql::register_child(&store.db,&parent,&child);
        let unrelated = store.create(dir.path(), "test", "cli-runtime").unwrap();
        crate::services::context(&paths).unwrap().db.seed_preference(&child, deepseek_code_core::db::session_store::settings::CHILD_PROGRESS,
            &json!({"session_id":child,"parent_session_id":parent,"description":"Inspect resume","current_tool":"Read","status":"running"}),
        ).unwrap();
        // Persisted running metadata and an open terminal are not active execution.
        let _presence = store.db.acquire_presence(&child).unwrap();
        let _unrelated = Consumer::attach(&paths.data_dir, &unrelated)
            .unwrap()
            .unwrap();
        assert!(active_agents_text(&paths, &parent)
            .unwrap()
            .starts_with("No active agents"));
        let consumer = Consumer::attach(&paths.data_dir, &child).unwrap().unwrap();
        let report = active_agents_text(&paths, &parent).unwrap();
        assert!(report.contains("Active agents in this session family: 1"));
        assert!(report.contains(&child));
        assert!(report.contains("Inspect resume") && report.contains("Tool: Read"));
        assert!(!report.contains(&parent) && !report.contains(&unrelated));
        let self_report = active_agents_text(&paths, &child).unwrap();
        assert!(self_report.contains("(current)"));
        drop(consumer);
        assert!(active_agents_text(&paths, &parent)
            .unwrap()
            .starts_with("No active agents"));
    }

    #[test]
    fn peer_list_scopes_projects_and_mailboxes_do_not_spoof_identity() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let project = dir.path().join("project");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let parent = store.create(&project, "test", "cli-runtime").unwrap();
        let child = store.create(&project, "test", "cli-runtime").unwrap();
        crate::fixture_sql::register_child(&store.db,&parent,&child);
        let unrelated = store.create(&project, "test", "cli-runtime").unwrap();
        crate::services::context(&paths)
            .unwrap()
            .db.seed_preference(&child, deepseek_code_core::db::session_store::settings::CHILD_PROGRESS,
                &json!({"session_id":child,"parent_session_id":parent}),
            )
            .unwrap();
        store.create(&other, "test", "cli-runtime").unwrap();
        let peers = peer_sessions(&store, &paths.data_dir, &project, &parent).unwrap();
        assert_eq!(peers.len(), 2);
        assert!(!peers.iter().any(|peer| peer["id"] == unrelated));
        assert_eq!(
            peers.iter().find(|p| p["id"] == parent).unwrap()["status"],
            "attached"
        );
        assert_eq!(
            peers.iter().find(|p| p["id"] == child).unwrap()["status"],
            "saved"
        );
        let sent = deepseek_code_core::tools::agent_messages::execute_for_session(
            json!({"operation":"send","to":child,"from":"spoof","content":"check"}),
            project.to_str().unwrap(),
            &parent,
        )
        .unwrap();
        assert_eq!(sent["from"], parent);
        let inbox = deepseek_code_core::tools::agent_messages::execute_for_session(
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
    deepseek_code_core::git::GitIntegration::new(workspace.to_path_buf())
        .get_current_branch()
        .ok()
        .filter(|branch| !branch.is_empty())
}

/// Remembers the branch a session is working on, at each turn.
pub(crate) fn record_branch(session: &Session) {
    let Some(branch) = git_branch(&session.workspace) else {
        return;
    };
    let _ = session.store.db.record_git_branch(&session.id,&branch);
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
fn resume_title(title: &str, latest: Option<&str>) -> String {
    let latest = latest.unwrap_or_default().lines().next().unwrap_or_default().trim();
    if latest.is_empty() || latest == title || crate::background_commands::is_notification(latest) {
        title.to_owned()
    } else {
        format!("{title} ⑂ {latest}")
    }
}

fn resumable(session: &Session, all: bool) -> Result<Vec<(String, Resumable)>> {
    let entries = session.store.db.resume_entries(&session.id, (!all).then_some(&session.workspace))?;
    // One scan for every row; the execution lease is an O(1) read and never
    // creates the lock file or settles another worker's state.
    let active = deepseek_code_core::services::background_sessions::active_session_ids(
        &session.paths.data_dir,
    )?;
    let mut result = Vec::new();
    for entry in entries {
        let row = entry.conversation;
        let running = active.contains(&row.id)
            || session.store.db.execution_active_for(&row.id)?;
        let title = resume_title(row.title.as_deref().filter(|s| !s.trim().is_empty() && *s!="CLI session").unwrap_or("(no prompt)"),entry.latest.as_deref());
        let title = if running { format!("[running] {title}") } else { title };
        let mut detail = format!("{} · {} · {} · {}",row.id,ago(&row.updated_at),entry.branch.as_deref().unwrap_or("HEAD"),short_size(entry.bytes));
        if all {
            if let Some(path) = row.workspace_path.as_deref() {
                detail.push_str(&format!(" · {path}"));
                if !Path::new(path).is_dir() {
                    detail.push_str(" · [missing]");
                }
            }
        }
        result.push((row.id,Resumable {title,detail,branch:entry.branch}));
    }
    Ok(result)
}

/// The non-TTY JSON list keeps every stored field and adds the same `running`
/// fact the picker shows.
fn sessions_with_running(session: &Session) -> Result<Vec<Value>> {
    let active = deepseek_code_core::services::background_sessions::active_session_ids(
        &session.paths.data_dir,
    )?;
    let mut values = session
        .store
        .db
        .conversations()?
        .into_iter()
        .map(|row| serde_json::to_value(row).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    for row in &mut values {
        let running = match row["id"].as_str() {
            Some(id) => active.contains(id) || session.store.db.execution_active_for(id)?,
            None => false,
        };
        row["running"] = Value::Bool(running);
    }
    Ok(values)
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use deepseek_code_core::runtime::RuntimePaths;

    async fn running_fixture() -> (
        tempfile::TempDir,
        RuntimePaths,
        Session,
        String,
        String,
    ) {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let selection = || crate::config::Selection {
            providers: vec![serde_json::from_value(serde_json::json!({
                "id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1",
                "api_key":"fixture-only","api_format":"openai","enabled":true,
                "models":[{"id":"fixture","name":"Fixture","enabled":true,
                    "supports_vision":false,"supports_web_search":false}]
            }))
            .unwrap()],
            provider_id: "fixture".into(),
            model: "fixture".into(),
        };
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let running = store.create(&workspace, "fixture", "fixture").unwrap();
        store.append(&running, "user", "older running task").unwrap();
        let current = store.create(&workspace, "fixture", "fixture").unwrap();
        store.append(&current, "user", "current task").unwrap();
        let cli = crate::args::Cli::parse_from(["dcl"]);
        let session = Session::open(&cli, &paths, selection(), Some(&current))
            .await
            .unwrap();
        (temp, paths, session, running, current)
    }

    fn append_job(paths: &RuntimePaths, session: &str) {
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("tasks/background").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let job = deepseek_code_core::services::background_sessions::Job {
            id,
            foreground: false,
            parent_session_id: session.into(),
            session_id: session.into(),
            title: "running task".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: now,
            heartbeat_ms: now,
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
    }

    fn entry<'a>(
        entries: &'a [(String, Resumable)],
        id: &str,
    ) -> &'a Resumable {
        &entries
            .iter()
            .find(|(entry, _)| entry.as_str() == id)
            .unwrap()
            .1
    }

    #[tokio::test]
    async fn idle_sessions_show_the_full_id_without_a_running_marker() {
        let (_temp, _paths, session, running, _current) = running_fixture().await;
        let entries = resumable(&session, false).unwrap();
        let entry = entry(&entries, &running);
        assert!(!entry.title.starts_with("[running]"), "{}", entry.title);
        assert!(
            entry.detail.starts_with(&format!("{running} · ")),
            "{}",
            entry.detail
        );
    }

    #[tokio::test]
    async fn an_execution_lease_marks_the_session_running_and_keeps_the_full_id() {
        let (_temp, _paths, session, running, _current) = running_fixture().await;
        let lease = session
            .store
            .db
            .open_session(&running)
            .unwrap()
            .acquire_execution()
            .unwrap();
        let entries = resumable(&session, false).unwrap();
        let entry = entry(&entries, &running);
        assert!(entry.title.starts_with("[running] "), "{}", entry.title);
        assert!(
            entry.detail.starts_with(&format!("{running} · ")),
            "{}",
            entry.detail
        );
        let values = sessions_with_running(&session).unwrap();
        assert_eq!(
            values
                .iter()
                .find(|row| row["id"] == running.as_str())
                .unwrap()["running"],
            true
        );
        drop(lease);
    }

    #[tokio::test]
    async fn an_active_managed_job_marks_the_session_running() {
        let (_temp, paths, session, running, _current) = running_fixture().await;
        append_job(&paths, &running);
        let entries = resumable(&session, false).unwrap();
        assert!(entry(&entries, &running).title.starts_with("[running] "));
        let values = sessions_with_running(&session).unwrap();
        let row = values
            .iter()
            .find(|row| row["id"] == running.as_str())
            .unwrap();
        assert_eq!(row["running"], true);
        // Every stored field is retained.
        assert!(row.get("updated_at").is_some(), "{row}");
    }

    #[tokio::test]
    async fn the_json_fallback_marks_and_preserves_idle_sessions() {
        let (_temp, _paths, session, _running, current) = running_fixture().await;
        let values = sessions_with_running(&session).unwrap();
        let row = values
            .iter()
            .find(|row| row["id"] == current.as_str())
            .unwrap();
        assert_eq!(row["running"], false);
        assert_eq!(row["id"], current);
    }

    #[test]
    fn osc52_payload_is_standard_base64() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64("中".as_bytes()), "5Lit");
    }

    #[tokio::test]
    async fn the_all_projects_picker_marks_missing_directories() {
        use deepseek_code_core::runtime::SessionStore;
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let first_id = store.create(&first, "fixture", "fixture").unwrap();
        let second_id = store.create(&second, "fixture", "fixture").unwrap();
        store.append(&second_id, "user", "saved history").unwrap();
        let provider = serde_json::from_value(serde_json::json!({
            "id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1",
            "api_key":"unused","api_format":"openai","enabled":true,
            "models":[{"id":"fixture","name":"Fixture","enabled":true,
                "supports_vision":false,"supports_web_search":false}]
        }))
        .unwrap();
        let cli = crate::args::Cli::try_parse_with_mode_source(["dcl"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                provider_id: "fixture".into(),
                model: "fixture".into(),
            },
            Some(&first_id),
        )
        .await
        .unwrap();
        std::fs::remove_dir_all(&second).unwrap();
        let entries = resumable(&session, true).unwrap();
        let entry = entries
            .iter()
            .find(|(id, _)| id == &second_id)
            .map(|(_, entry)| entry)
            .expect("missing session listed");
        assert!(entry.detail.contains("[missing]"), "{}", entry.detail);
        assert!(
            entry.detail.contains(&second.canonicalize().unwrap_or(second.clone()).to_string_lossy().to_string())
                || entry.detail.contains(&second.to_string_lossy().to_string()),
            "{}",
            entry.detail
        );
        session.close().await.unwrap();
    }

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

/// `/terminal-setup` as in the reference CLI: how to add newlines to a
/// prompt in this terminal. The prompt takes Shift+Enter when the terminal
/// reports it, Alt+Enter, and a trailing backslash.
fn terminal_setup() -> String {
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let native = match program.as_str() {
        "iTerm.app" => Some("iTerm2"),
        "WezTerm" => Some("WezTerm"),
        "ghostty" => Some("Ghostty"),
        "WarpTerminal" => Some("Warp"),
        _ if std::env::var("TERM").is_ok_and(|term| term.contains("kitty")) => Some("Kitty"),
        _ if std::env::var_os("WT_SESSION").is_some() => Some("Windows Terminal"),
        _ => None,
    };
    if let Some(name) = native {
        return format!("  \u{23bf}  {name} supports Shift+Enter for newlines natively; nothing to set up.\n     You can also use backslash (\\) + Enter or Alt+Enter.");
    }
    let mut text = String::from("  \u{23bf}  This command explains the Shift+Enter shortcut for multi-line prompts.\n     Note: You can already use backslash (\\) + Enter, or Alt+Enter, to add newlines.\n");
    if matches!(program.as_str(), "vscode" | "cursor" | "zed") || std::env::var_os("VSCODE_PID").is_some() {
        text.push_str("     To set up the shortcut in this editor's terminal, bind Shift+Enter to send the sequence \\u001b\\r\n     (Alt+Enter), e.g. in VS Code with workbench.action.terminal.sendSequence.");
    } else {
        text.push_str("     Shift+Enter works where the terminal reports it: iTerm2, WezTerm, Ghostty, Kitty, Warp and Windows\n     Terminal do so natively. In other terminals, bind Shift+Enter to send \\u001b\\r (Alt+Enter).");
    }
    text
}
