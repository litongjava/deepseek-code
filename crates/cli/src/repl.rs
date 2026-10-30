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
    Ok(())
}

fn conversation(session: &Session, id: &str, detailed: bool, mut recorded: Option<&mut crate::terminal::scrollback::Scrollback>) -> Result<Vec<crate::terminal::change_view::Line>> {
    use crate::terminal::{change_view, tool_view};
    let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols.saturating_sub(1) as usize);
    session.store.db.with_conn(|conn| {
        let rows = deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, id)?;
        let mut lines = Vec::new();
        for message in &rows {
            let message_start = lines.len();
            if message.is_compact_boundary {
                lines.push(change_view::muted("\u{273b} Conversation compacted"));
                lines.extend(message.content.lines().map(|line| change_view::muted(&format!("  {line}"))));
                if let Some(recorded) = recorded.as_deref_mut() { record_lines(recorded, &lines[message_start..], width); }
                continue;
            }
            if message.role == "user" {
                if let Some(summary) = deepseek_code_core::tools::background_shell::notification_summary(&message.content) {
                    lines.push(change_view::body(&format!("\u{25cf} {summary}")));
                    if let Some(recorded) = recorded.as_deref_mut() { record_lines(recorded, &lines[message_start..], width); }
                continue;
                }
                let mut text = message.content.lines();
                lines.push(change_view::body(&format!("{} {}", crate::terminal::prompt_glyph(), text.next().unwrap_or_default())));
                lines.extend(text.map(|line| change_view::body(&format!("  {line}"))));
                if let Some(recorded) = recorded.as_deref_mut() { record_lines(recorded, &lines[message_start..], width); }
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
                let mut text = thinking.lines();
                lines.push(change_view::muted(&format!("\u{2234} {}", text.next().unwrap_or_default())));
                lines.extend(text.take(400).map(|line| change_view::muted(&format!("  {line}"))));
            }
            if let Some(recorded) = recorded.as_deref_mut() {
                record_lines(recorded, &lines[message_start..], width);
            }
            for tool in deepseek_code_core::db::message_repo::get_tool_calls_for_message(conn, &message.id)? {
            let input = tool
                .input
                .as_deref()
                .and_then(|input| serde_json::from_str(input).ok())
                .unwrap_or_default();
            let output = tool.output.as_deref().unwrap_or("");
            if let Some(recorded) = recorded.as_deref_mut() {
                let short = tool_view::finished(&tool.name, &input, output, tool.is_error, None, Some(&session.workspace));
                let elapsed = deepseek_code_core::db::message_repo::tool_duration(conn, &tool.id)?.map(std::time::Duration::from_millis);
                let full = tool_view::expanded_block(&tool.name, &input, output, tool.is_error, elapsed, Some(&session.workspace));
                if recorded.needs_separator() { recorded.push(String::new(), None); }
                let (id, rows) = recorded.block(short, full);
                for row in rows { recorded.push(row, Some(id)); }
            }
            lines.extend(if detailed {
                tool_view::expanded(&tool.name, &input, output, tool.is_error, Some(&session.workspace))
            } else {
                tool_view::finished(&tool.name, &input, output, tool.is_error, None, Some(&session.workspace))
            });
            }
            let answer_start = lines.len();
            let mut text = message.content.lines();
            if let Some(first) = text.next() {
                lines.push(change_view::body(&format!("\u{25cf} {first}")));
                lines.extend(text.map(|line| change_view::body(&format!("  {line}"))));
            }
            if let Some(recorded) = recorded.as_deref_mut() { record_lines(recorded, &lines[answer_start..], width); }
        }
        Ok(lines)
    })?
}

/// `/compact [instructions]`: summarize the session; returns how many
/// messages were summarized. Shared by the REPL and print mode.
pub async fn compact(paths: &RuntimePaths, session: &mut Session, instructions: &str) -> Result<usize> {
    let context = crate::services::context(paths)?;
    let report = tokio::select! {
        report = deepseek_code_core::services::compaction::compact_with_hooks(
            &session.engine, &context, &session.workspace, &session.id,
            &session.provider_id, &session.model, instructions.trim(), "manual",
        ) => report?,
        _ = tokio::signal::ctrl_c() => anyhow::bail!("Compaction interrupted; check /history for a completed summary. Original transcript retained"),
    };
    for warning in &report.warnings { eprintln!("Hook: {warning}"); }
    Ok(report.summarized_messages)
}

pub async fn handle(line: &str, paths: &RuntimePaths, session: &mut Session) -> Result<bool> {
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
        "/status" => crate::status_commands::run(session, argument).await?,
        "/compact" => {
            let summarized = compact(paths, session, argument).await?;
            // As in the reference CLI; the summary is in the Ctrl+O transcript.
            eprintln!(
                "  \u{23bf}  Compacted (ctrl+o to see full summary)\n     {summarized} messages summarized; the original transcript remains in /history."
            );
        }
        "/clear" => {
            ensure!(argument.trim().is_empty(), "Usage: /clear");
            let previous = session.id.clone();
            session.clear().await?;
            eprintln!(
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
            eprintln!("Saved messages: {}\nActive messages: {}\nActive message text: {bytes} UTF-8 bytes\nCompacted: {}\nCounts exclude system instructions and tool payloads; this is not a token estimate.", rows.len(), rows.len() - start, rows.iter().any(|row| row.is_compact_boundary));
        }
        "/permissions" => {
            crate::permission_commands::handle(argument, session).await?;
        }
        "/rename" => {
            let title = argument.trim();
            rename(&session.store, &session.id, title)?;
            eprintln!("Session renamed: {title}");
        }
        "/sessions" => {
            ensure!(argument.trim().is_empty(), "Usage: /sessions");
            let sessions = session
                .store
                .db
                .with_conn(conversation_repo::list_conversations)??;
            println!("{}", serde_json::to_string_pretty(&sessions)?);
        }
        "/skills" => {
            ensure!(argument.trim().is_empty(), "Usage: /skills");
            let manager = SkillsManager::with_path(paths.data_dir.join("skills"));
            println!(
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
        "/end" | "/cancel" => eprintln!("No unfinished prompt. Use /multiline to compose one."),
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
