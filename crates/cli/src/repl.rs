//! Local REPL commands use the same stores as Desktop and never call a model.
use crate::session::Session;
use anyhow::{ensure, Result};
use deepseek_code_core::{
    db::conversation_repo,
    runtime::{RuntimePaths, SessionStore},
    skills::SkillsManager,
};

/// The whole conversation for the Ctrl+O transcript, as in the reference CLI:
/// each prompt, each answer under its time and model, its thinking and every
/// tool call in full.
pub fn transcript(session: &Session) -> Result<Vec<crate::terminal::change_view::Line>> {
    transcript_of(session, &session.id)
}

/// The transcript of any saved conversation, for the /resume preview.
pub fn transcript_of(session: &Session, id: &str) -> Result<Vec<crate::terminal::change_view::Line>> {
    conversation(session, id, true, None)
}

/// After a resume, as in the reference CLI: the screen starts over with the
/// banner and the conversation so far, one short line per tool call.
pub fn show_resumed(session: &Session, clear: bool) -> Result<()> {
    *crate::terminal::TURN_COMPLETION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return Ok(());
    }
    let mut out = String::new();
    if clear {
        out.push_str("\x1b[2J\x1b[H");
        out.push_str(&crate::terminal::banner(&session.model, &session.workspace));
        out.push('\n');
    }
    let mut recorded = crate::terminal::scrollback::Scrollback::default();
    conversation(session, &session.id, false, Some(&mut recorded))?;
    for row in recorded.all() { out.push_str(&row); out.push('\n'); }
    eprint!("{out}");
    recorded.save();
    if session.waiting_for_continue() {
        let active = crate::worker_frontend::settle_session(session).ok().flatten();
        if let Some(job) = &active {
            command_eprintln!("{}", resumed_notice(Some(job)));
        } else if crate::observe::execution_lease_active(session).unwrap_or(false) {
            // A direct print/json owner keeps the execution lease. Show that the
            // turn runs elsewhere so /continue cannot start a second one.
            command_eprintln!("{}", crate::observe::running_notice(session).unwrap_or_else(|_| {
                "✻ A turn is still running in another process; observing its published progress. Esc returns without stopping it.".into()
            }));
        } else if crate::observe::latest_interrupted(session).unwrap_or(false) {
            command_eprintln!("Session restored; the previous turn was interrupted before it completed and its request was not replayed. Send a new message or /continue to continue explicitly.");
        } else {
            command_eprintln!("{}", resumed_notice(None));
        }
    }
    Ok(())
}

/// The notice after a resume must offer only what the session can do now.
fn resumed_notice(active: Option<&deepseek_code_core::services::background_sessions::Job>) -> String {
    match active {
        None => "Session restored; waiting for input. Use /continue or send a new task to start the model.".into(),
        Some(job) => format!("Session restored. {}", crate::worker_frontend::active_worker_message(job)),
    }
}

/// Save presentation metadata against the final message, so rewind and session
/// switching cannot accidentally display a different turn's completion status.
pub fn save_completion(session: &Session) -> Result<()> {
    let status = crate::terminal::TURN_COMPLETION.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(status) = status else { return Ok(()); };
    let messages = session.store.db.with_conn(|conn| {
        deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, &session.id)
    })??;
    if let Some(message) = messages.last().filter(|m| m.role == "assistant") {
        let directory = session.paths.data_dir.join("turn-completions");
        std::fs::create_dir_all(&directory)?;
        let key = hex_message_id(&message.id);
        std::fs::write(directory.join(key), status)?;
    }
    Ok(())
}

fn hex_message_id(id: &str) -> String {
    id.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn completion_for_message(
    data_dir: &std::path::Path,
    message: &deepseek_code_core::db::message_repo::MessageRow,
    turn: Option<&deepseek_code_core::db::turn_repo::TurnRecord>,
) -> Option<String> {
    if message.role != "assistant" { return None; }
    let path = data_dir.join("turn-completions").join(hex_message_id(&message.id));
    if let Ok(status) = std::fs::read_to_string(path) {
        return Some(status);
    }
    // The live display writes a sidecar; a headless turn is measured in
    // execution_turns instead. A running turn has no completion line at all, so
    // a resumed screen never presents it as a finished Previous turn.
    use deepseek_code_core::db::turn_repo::TurnStatus;
    if let Some(turn) = turn {
        return match turn.status {
            TurnStatus::Running => None,
            TurnStatus::Completed => Some(format!("✻ Worked for {} · done", format_duration(turn.elapsed_ms.unwrap_or(0)))),
            TurnStatus::Failed => Some(format!("✻ Worked for {} · failed", format_duration(turn.elapsed_ms.unwrap_or(0)))),
            TurnStatus::Cancelled if turn.interrupted() => Some("✻ Previous turn · interrupted · duration unknown".into()),
            TurnStatus::Cancelled => Some("✻ Previous turn · interrupted".into()),
        };
    }
    // Legacy message timestamps are save times, not measured completion times.
    Some(match chrono::DateTime::parse_from_rfc3339(&message.created_at) {
        Ok(time) => format!("✻ Previous turn · saved {} · duration unavailable", time.with_timezone(&chrono::Local).format("%-I:%M %p")),
        Err(_) => "✻ Previous turn · duration unavailable".into(),
    })
}

fn conversation(session: &Session, id: &str, detailed: bool, mut recorded: Option<&mut crate::terminal::scrollback::Scrollback>) -> Result<Vec<crate::terminal::change_view::Line>> {
    use crate::terminal::{change_view, tool_view};
    let width = crate::terminal::size().0.saturating_sub(1) as usize;
    session.store.db.with_conn(|conn| {
        let rows = deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, id)?;
        let mut lines = Vec::new();
        let mut tool_number = 0;
        for message in &rows {
            if detailed { separate_block(&mut lines); }
            let message_start = lines.len();
            if message.is_compact_boundary {
                lines.push(change_view::muted("\u{273b} Conversation compacted"));
                lines.extend(message.content.lines().map(|line| change_view::muted(&format!("  {line}"))));
                if let Some(recorded) = recorded.as_deref_mut() { record_lines(recorded, &lines[message_start..], width); }
                continue;
            }
            if message.role == "user" {
                if let Some(summary) = crate::background_commands::notification_text(&message.content) {
                    lines.extend(summary.lines().map(change_view::body));
                    if let Some(recorded) = recorded.as_deref_mut() { record_lines(recorded, &lines[message_start..], width); }
                continue;
                }
                let mut text = message.content.lines();
                lines.push(change_view::body(&format!(
                    "{} {}",
                    crate::terminal::prompt_glyph(),
                    text.next().unwrap_or_default()
                )));
                lines.extend(text.map(|line| change_view::body(&format!("  {line}"))));
                if let Some(recorded) = recorded.as_deref_mut() {
                    recorded.record_prompt(&message.content);
                }
                continue;
            }
            if detailed {
                let time = chrono::DateTime::parse_from_rfc3339(&message.created_at)
                    .map(|time| time.with_timezone(&chrono::Local).format("%I:%M %p").to_string())
                    .unwrap_or_default();
                let header = format!("{time} {}", session.model);
                lines.push(change_view::muted(&format!("{header:>width$}")));
            }
            if let (Some(recorded), Some(thinking)) = (recorded.as_deref_mut(), message.thinking.as_deref().filter(|text| !text.trim().is_empty())) {
                if recorded.needs_separator() { recorded.push(String::new(), None); }
                let (short, full) = tool_view::thought(thinking, None);
                let (id, rows) = recorded.block(short, full);
                for row in rows { recorded.push(row, Some(id)); }
            }
            if let Some(thinking) = message.thinking.as_deref().filter(|text| detailed && !text.trim().is_empty()) {
                separate_block(&mut lines);
                let mut text = thinking.lines();
                lines.push(change_view::muted(&format!("\u{2234} {}", text.next().unwrap_or_default())));
                lines.extend(text.take(400).map(|line| change_view::muted(&format!("  {line}"))));
            }
            if let Some(recorded) = recorded.as_deref_mut() {
                record_lines(recorded, &lines[message_start..], width);
            }
            for tool in deepseek_code_core::db::message_repo::get_tool_calls_for_message(conn, &message.id)? {
            tool_number += 1;
            let number = Some(tool_number);
            let input = tool
                .input
                .as_deref()
                .and_then(|input| serde_json::from_str(input).ok())
                .unwrap_or_default();
            // Neutral on purpose: the call may have been cut off or may still be running.
            let output = tool.output.as_deref().unwrap_or("No result recorded");
            if let Some(recorded) = recorded.as_deref_mut() {
                // A saved edit looks as it did live: summary and diff preview, click for the rest.
                let change = matches!(tool.name.as_str(), "Write" | "Edit" | "MultiEdit")
                    .then(|| change_view::change_in_workspace(output, tool.is_error, Some(&session.workspace)))
                    .flatten()
                    .filter(|lines| lines.len() > 1);
                let (short, full) = if let Some(lines) = change {
                    let short = change_view::collapsed_change(&lines);
                    let mut full = lines;
                    full.push(change_view::muted(tool_view::COLLAPSE_HINT));
                    (short, full)
                } else {
                    let elapsed = deepseek_code_core::db::message_repo::tool_duration(conn, &tool.id)?.map(std::time::Duration::from_millis);
                    (tool_view::finished(&tool.name, &input, output, tool.is_error, None, Some(&session.workspace)),
                     tool_view::expanded_block(&tool.name, &input, output, tool.is_error, elapsed, Some(&session.workspace)))
                };
                if recorded.needs_separator() { recorded.push(String::new(), None); }
                let (id, rows) = recorded.block(tool_view::numbered(short, number), tool_view::numbered(full, number));
                recorded.bind_copy(id, tool_view::copy_command(&tool.name, &input), tool_view::result_text(output));
                for row in rows { recorded.push(row, Some(id)); }
            }
            if detailed { separate_block(&mut lines); }
            lines.extend(tool_view::numbered(if detailed {
                tool_view::expanded(&tool.name, &input, output, tool.is_error, Some(&session.workspace))
            } else {
                tool_view::finished(&tool.name, &input, output, tool.is_error, None, Some(&session.workspace))
            }, number));
            }
            let answer_start = lines.len();
            let mut text = message.content.lines();
            if let Some(first) = text.next() {
                if detailed { separate_block(&mut lines); }
                lines.push(change_view::body(&format!("\u{25cf} {first}")));
                lines.extend(text.map(|line| change_view::body(&format!("  {line}"))));
            }
            if let Some(recorded) = recorded.as_deref_mut() {
                if lines.len() > answer_start && recorded.needs_separator() {
                    recorded.push(String::new(), None);
                }
                let mut markdown = crate::terminal::markdown::MarkdownStream::new(
                    std::env::var_os("NO_COLOR").is_none(),
                );
                markdown.set_width(width.saturating_sub(2));
                let mut rendered: Vec<_> = message
                    .content
                    .lines()
                    .flat_map(|line| markdown.line(line))
                    .collect();
                rendered.extend(markdown.finish());
                for (index, line) in rendered.iter().enumerate() {
                    let indent = " ".repeat(2 + crate::terminal::markdown::hanging_indent(line));
                    recorded.push_flow(line, if index == 0 { "● " } else { "  " }, &indent, width);
                }
            }
            let saved = session.paths.data_dir.join("turn-completions").join(hex_message_id(&message.id));
            // Only the final legacy message has a fallback; intermediate tool
            // messages must not be presented as completed turns.
            if saved.is_file() || rows.last().is_some_and(|last| last.id == message.id) {
                let turn = deepseek_code_core::db::turn_repo::get_message_turn(conn, &message.id).ok().flatten();
                if let Some(status) = completion_for_message(&session.paths.data_dir, message, turn.as_ref()) {
                    separate_block(&mut lines);
                    lines.push(change_view::muted(&status));
                    if let Some(recorded) = recorded.as_deref_mut() {
                        if recorded.needs_separator() { recorded.push(String::new(), None); }
                        recorded.push(change_view::render(&change_view::muted(&status), width, std::env::var_os("NO_COLOR").is_none()), None);
                    }
                }
            }
        }
        Ok(lines)
    })?
}

#[cfg(test)]
fn rendered_answer(content: &str, width: usize, color: bool) -> Vec<String> {
    let mut markdown = crate::terminal::markdown::MarkdownStream::new(color);
    markdown.set_width(width.saturating_sub(2));
    let mut rendered = Vec::new();
    for line in content.lines() { rendered.extend(markdown.line(line)); }
    rendered.extend(markdown.finish());
    rendered.iter().enumerate().flat_map(|(index, line)| {
        crate::terminal::markdown::wrap_styled(line, if index == 0 { "● " } else { "  " }, "  ", width)
    }).collect()
}

#[test]
fn resumed_answers_keep_markdown_colors_and_flush_tables() {
    let content = "## Heading\nUse `code` and **bold**\n| A | B |\n| --- | --- |\n| one | two |";
    let colored = rendered_answer(content, 80, true).join("\n");
    assert!(colored.contains("\x1b["));
    assert!(colored.contains("one"));
    assert!(!colored.contains("**bold**"));
    assert!(!rendered_answer(content, 80, false).join("\n").contains("\x1b["));
}

#[tokio::test]
async fn all_tools_keep_numbers_in_resume_and_detailed_transcript() {
    use clap::Parser;
    use deepseek_code_core::db::message_repo;
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let provider = serde_json::from_value(serde_json::json!({"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1","api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"fixture","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false}]})).unwrap();
    let selection = crate::config::Selection { providers: vec![provider], provider_id: "fixture".into(), model: "fixture".into() };
    let cli = crate::args::Cli::parse_from(["dcl", "--print"]);
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(root.path(), "fixture", "fixture").unwrap();
    let session = Session::open(&cli, &paths, selection, Some(&id)).await.unwrap();
    let names = ["Read", "Grep", "Edit", "Bash", "mcp__codegraph__explore"];
    session.store.db.with_conn(|conn| -> Result<()> {
        message_repo::insert_message(conn, "answer", &id, "assistant", "done", None, "2026-11-02T00:00:00Z", false, 0)?;
        for (index, name) in names.iter().enumerate() {
            message_repo::insert_tool_call(conn, &format!("call-{index}"), "answer", name, Some("{}"), Some("ok"), false, index as i64)?;
        }
        Ok(())
    }).unwrap().unwrap();
    for detailed in [false, true] {
        let lines = conversation(&session, &id, detailed, None).unwrap();
        let text = crate::terminal::change_view::plain(&lines);
        for (index, name) in names.iter().enumerate() {
            assert!(text.contains(&format!("#{} {name}", index + 1)), "{text}");
        }
    }
    let mut display = crate::terminal::live::TurnDisplay::new(false, "ask", &paths.data_dir.join("keybindings.json"), vec![]).unwrap();
    display.restore_numbers(&session).unwrap();
    let snapshot = serde_json::json!({"id":"request-fixture","startedAt":1000,"model":"fixture","status":"completed","inputTokens":123000,"outputTokens":22600,"usage":{"prompt_tokens":123000,"completion_tokens":22600},"systemPrompt":"must not be copied into live accounting"});
    crate::session_commands::save_live_requests(&session, &[snapshot.clone()]).unwrap();
    let live = crate::session_commands::live_requests(&session).unwrap();
    assert!(live[0].get("systemPrompt").is_none());
    let before = crate::status_commands::text(&session, "").await.unwrap();
    assert!(before.contains("146K total  (123K input + 22.6K output)"), "{before}");
    session.store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, "answer", &serde_json::json!([snapshot]))).unwrap().unwrap();
    let after = crate::status_commands::text(&session, "").await.unwrap();
    assert!(after.contains("146K total  (123K input + 22.6K output)"), "{after}");
    for _ in 0..10 { crate::session_commands::step_effort(&session, true).await.unwrap(); }
    assert_eq!(crate::session_commands::session_effort(&session).unwrap().as_deref(), Some("max"));
    for _ in 0..10 { crate::session_commands::step_effort(&session, false).await.unwrap(); }
    assert_eq!(crate::session_commands::session_effort(&session).unwrap(), None);
    session.close().await.unwrap();
}

/// A finished compaction. `cancelled_after_commit` means the boundary and its
/// file were already saved when the user pressed Esc; only the remaining post
/// steps were stopped.
pub struct Compacted {
    pub report: deepseek_code_core::services::compaction::CompactionResult,
    pub cancelled_after_commit: bool,
}

/// `/compact [instructions]`: summarize the session; returns the committed
/// result with its duration and summary file. Shared by the REPL and print mode.
pub async fn compact(
    paths: &RuntimePaths,
    session: &mut Session,
    instructions: &str,
    mut cancel: Option<crate::terminal::input::CancelWatch>,
) -> Result<Compacted> {
    use deepseek_code_core::services::compaction::CompactionTracker;
    let context = crate::services::context(paths)?;
    let tracker = CompactionTracker::default();
    let compacted = {
        let future = crate::hook_progress::during(&session.id, true, deepseek_code_core::services::compaction::compact_with_hooks_tracked(
            &session.engine, &context, &session.workspace, &session.id,
            &session.provider_id, &session.model, instructions.trim(), "manual", Some(&tracker),
        ));
        tokio::pin!(future);
        loop {
            tokio::select! {
                report = &mut future => break Compacted { report: report?, cancelled_after_commit: false },
                // The input owner forwards Esc/Ctrl+C here while the prompt loop
                // is paused. The branch re-checks the recorded outcome instead
                // of trusting a value captured when the select began: a cancel
                // that arrives after the boundary transaction keeps the real
                // committed result, stops the remaining post steps, and is
                // never reported as an unsaved compaction.
                _ = wait_for_cancel(cancel.as_mut()) => break committed_after_cancel(&tracker, "Compaction interrupted before it was saved; original transcript retained")?,
                _ = crate::signals::interrupted() => break committed_after_cancel(&tracker, "Compaction interrupted; check /history for a completed summary. Original transcript retained")?,
            }
        }
        // The block ends here, so a cancel after the commit drops the future
        // and stops the post-compaction hooks instead of waiting for them.
    };
    for warning in &compacted.report.warnings { command_eprintln!("Hook: {warning}"); }
    Ok(compacted)
}

/// The result to report when the user cancels. A committed compaction keeps
/// its saved boundary, file and count and is never reported as unsaved; its
/// duration is measured when this returns, so the wait after the commit is
/// included. Nothing is regenerated, exported or counted again.
pub(crate) fn committed_after_cancel(
    tracker: &deepseek_code_core::services::compaction::CompactionTracker,
    unsaved: &str,
) -> Result<Compacted> {
    match tracker.outcome() {
        Some(outcome) => {
            let mut report: deepseek_code_core::services::compaction::CompactionResult =
                outcome.into();
            report.duration_ms = tracker.elapsed_ms();
            Ok(Compacted {
                report,
                cancelled_after_commit: true,
            })
        }
        None => anyhow::bail!("{unsaved}"),
    }
}

/// Wait for an interactive cancel key; keep waiting when no watch is armed or
/// when the reader stopped without one.
async fn wait_for_cancel(watch: Option<&mut crate::terminal::input::CancelWatch>) {
    match watch {
        Some(watch) => {
            if !watch.wait().await {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// `12s` under a minute, `<1s` below a second, `1m 23s` or `1h 2m 3s` above.
pub(crate) fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        return "<1s".into();
    }
    let seconds = ms / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

pub async fn handle(
    line: &str,
    paths: &RuntimePaths,
    session: &mut Session,
    input: Option<&mut crate::terminal::input::TerminalInput>,
) -> Result<bool> {
    let line = line.trim();
    let (command, argument) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let command = crate::command_catalog::canonical(command);
    if command == "/worktree" { crate::startup_options::worktree_startup::handle(argument,session).await?;return Ok(true); }
    if crate::feedback_commands::handle(command, argument, session).await? {
        return Ok(true);
    }
    if crate::resource_commands::handle(command, argument, paths).await? {
        return Ok(true);
    }
    if crate::account_commands::handle(command, argument, paths, session).await? {
        return Ok(true);
    }
    if crate::rewind_commands::handle(command, argument, paths, session).await? {
        return Ok(true);
    }
    if crate::display_commands::handle(command, argument, paths, session).await? {
        return Ok(true);
    }
    if crate::session_commands::handle(command, argument, paths, session).await? {
        return Ok(true);
    }
    match command {
        "/subagents" => {
            ensure!(argument.trim().is_empty(), "Usage: /subagents");
            let context = crate::services::context(paths)?;
            for entry in crate::terminal::subagents::entries(&context, &session.id)? {
                command_println!("- {}{}  {}", entry.label, if entry.current { " (current)" } else { "" }, entry.id);
            }
        }
        "/status" => crate::status_commands::run(session, argument).await?,
        "/compact" => {
            let watch = match input {
                Some(input) => input.watch_cancel()?,
                None => None,
            };
            let compacted = compact(paths, session, argument, watch).await?;
            let report = &compacted.report;
            // As in the reference CLI; the summary is in the Ctrl+O transcript.
            command_eprintln!(
                "  \u{23bf}  Compacted in {} (ctrl+o to see full summary)",
                format_duration(report.duration_ms)
            );
            for line in crate::compaction::completion_details(report) {
                command_eprintln!("     {line}");
            }
            if compacted.cancelled_after_commit {
                command_eprintln!(
                    "     Stopped the remaining post-compaction steps by request; the summary above was already saved."
                );
            }
        }
        "/clear" => {
            ensure!(argument.trim().is_empty(), "Usage: /clear");
            let previous = session.id.clone();
            session.clear().await?;
            command_eprintln!(
                "New session: {}. Previous history retained: {previous}",
                session.id
            );
        }
        "/context" => {
            ensure!(argument.trim().is_empty(), "Usage: /context");
            let rows = session.store.db.with_conn(|conn| {
                deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, &session.id)
            })??;
            let start = rows
                .iter()
                .rposition(|row| row.is_compact_boundary)
                .unwrap_or(0);
            let bytes: usize = rows[start..].iter().map(|row| row.content.len()).sum();
            command_eprintln!("Saved messages: {}\nActive messages: {}\nActive message text: {bytes} UTF-8 bytes\nCompacted: {}\nCounts exclude system instructions and tool payloads; this is not a token estimate.", rows.len(), rows.len() - start, rows.iter().any(|row| row.is_compact_boundary));
        }
        "/permissions" => {
            crate::permission_commands::handle(argument, session).await?;
        }
        "/rename" => {
            let title = argument.trim();
            rename(&session.store, &session.id, title)?;
            command_eprintln!("Session renamed: {title}");
        }
        "/sessions" => {
            ensure!(argument.trim().is_empty(), "Usage: /sessions");
            let sessions = session
                .store
                .db
                .with_conn(conversation_repo::list_conversations)??;
            command_println!("{}", serde_json::to_string_pretty(&sessions)?);
        }
        "/skills" => {
            ensure!(argument.trim().is_empty(), "Usage: /skills");
            let manager = SkillsManager::with_path(paths.data_dir.join("skills"));
            command_println!(
                "{}",
                serde_json::to_string_pretty(&manager.load_skills().await?)?
            );
        }
        "/mcp" if argument.trim().is_empty() && crate::terminal::picker::available() => {
            crate::mcp_menu::run(session).await?;
        }
        "/mcp" => {
            use clap::Parser;
            let mut words = crate::display_commands::split_arguments(argument)?;
            if words.is_empty() { words.push("list".into()); }
            let parsed = crate::args::Cli::try_parse_from(["dcl".into(), "mcp".into()].into_iter().chain(words))?;
            let Some(crate::args::Commands::Mcp { action }) = parsed.command else { unreachable!() };
            crate::mcp(paths, &action).await?;
        }
        "/end" | "/cancel" => command_eprintln!("No unfinished prompt. Use /multiline to compose one."),
        _ => return crate::extended_commands::handle(command, argument, paths, session).await,
    }
    Ok(true)
}

fn rename(store: &SessionStore, id: &str, title: &str) -> Result<()> {
    ensure!(!title.is_empty(), "Usage: /rename TITLE");
    ensure!(
        title.chars().count() <= 200,
        "Session titles must be at most 200 characters"
    );
    store
        .db
        .with_conn(|conn| conversation_repo::update_conversation_title(conn, id, title))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_use_seconds_minutes_and_hours() {
        assert_eq!(format_duration(0), "<1s");
        assert_eq!(format_duration(999), "<1s");
        assert_eq!(format_duration(1_000), "1s");
        assert_eq!(format_duration(59_999), "59s");
        assert_eq!(format_duration(60_000), "1m 0s");
        assert_eq!(format_duration(83_000), "1m 23s");
        assert_eq!(format_duration(3_723_000), "1h 2m 3s");
    }

    #[test]
    fn resumed_notice_offers_only_what_the_session_can_do() {
        assert!(resumed_notice(None).contains("Use /continue or send a new task"));
        let job = |status: &str| serde_json::from_value::<deepseek_code_core::services::background_sessions::Job>(serde_json::json!({
            "id": "task-1", "parent_session_id": "s", "session_id": "s", "title": "t", "status": status,
            "supervisor_pid": null, "child_pid": null, "started_ms": 0, "heartbeat_ms": 0,
            "finished_ms": null, "exit_code": null, "error": null,
        })).unwrap();
        let running = resumed_notice(Some(&job("running")));
        assert!(running.contains("Use /continue to return") && running.contains("/tasks cancel task-1"));
        assert!(!running.contains("send a new task"));
        let stuck = resumed_notice(Some(&job("unresponsive")));
        assert!(stuck.contains("/tasks show task-1") && !stuck.contains("/continue"));
    }

    #[test]
    fn completion_restores_exact_status_and_never_leaks_to_another_message() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(dir.path(), "model", "provider").unwrap();
        store.append(&id, "assistant", "Finished").unwrap();
        store.append(&id, "user", "Next turn").unwrap();
        let messages = store.db.with_conn(|conn| deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, &id)).unwrap().unwrap();
        assert!(completion_for_message(&paths.data_dir, &messages[0], None).unwrap().contains("duration unavailable"));
        let directory = paths.data_dir.join("turn-completions");
        std::fs::create_dir_all(&directory).unwrap();
        let status = "✻ Worked for 1m 30s · done 5:45 PM";
        std::fs::write(directory.join(hex_message_id(&messages[0].id)), status).unwrap();
        assert_eq!(completion_for_message(&paths.data_dir, &messages[0], None).as_deref(), Some(status));
        assert_eq!(completion_for_message(&paths.data_dir, &messages[1], None), None);
    }

    #[test]
    fn rename_persists_without_losing_history_and_rejects_empty_titles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(dir.path(), "model", "provider").unwrap();
        store.append(&id, "user", "Keep this message").unwrap();
        let before = store.history(&id).unwrap();
        rename(&store, &id, "新的会话").unwrap();
        assert!(rename(&store, &id, "").is_err());
        let reopened = SessionStore::new(paths.open_database().unwrap());
        assert_eq!(
            reopened.resume(&id).unwrap().title.as_deref(),
            Some("新的会话")
        );
        assert_eq!(reopened.history(&id).unwrap(), before);
    }
}

fn record_lines(recorded: &mut crate::terminal::scrollback::Scrollback, lines: &[crate::terminal::change_view::Line], width: usize) {
    for line in lines {
        let text = crate::terminal::change_view::render(line, usize::MAX, std::env::var_os("NO_COLOR").is_none());
        for row in crate::terminal::markdown::wrap_styled(&text, "", "", width) { recorded.push(row, None); }
    }
}

fn separate_block(lines: &mut Vec<crate::terminal::change_view::Line>) {
    if lines.last().is_some_and(|line| !crate::terminal::change_view::plain(std::slice::from_ref(line)).trim().is_empty()) {
        lines.push(crate::terminal::change_view::body(""));
    }
}

#[cfg(test)]
mod block_spacing_tests {
    #[test]
    fn separators_are_single_and_do_not_add_a_leading_blank() {
        let mut lines = Vec::new();
        super::separate_block(&mut lines);
        assert!(lines.is_empty());
        lines.push(crate::terminal::change_view::body("thought"));
        super::separate_block(&mut lines);
        super::separate_block(&mut lines);
        lines.push(crate::terminal::change_view::body("answer"));
        assert_eq!(lines.len(), 3);
        assert!(crate::terminal::change_view::plain(&lines[1..2]).trim().is_empty());
    }
}
