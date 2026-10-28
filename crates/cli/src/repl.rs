//! Local REPL commands use the same stores as Desktop and never call a model.
use crate::session::Session;
use anyhow::{ensure, Result};
use claude_core::{
    db::conversation_repo,
    runtime::{RuntimePaths, SessionStore},
    skills::SkillsManager,
};

/// The Ctrl+O transcript at the prompt; empty until a turn has used a tool,
/// so the prompt offers it only when there is something to expand.
pub fn tool_transcript(session: &Session) -> Result<Vec<crate::terminal::change_view::Line>> {
    let last_tools = session.store.db.with_conn(|conn| -> Result<bool> {
        let rows = claude_core::db::message_repo::get_messages_by_conversation(conn, &session.id)?;
        Ok(match rows.iter().rev().find(|row| row.role == "assistant") {
            Some(message) => !claude_core::db::message_repo::get_tool_calls_for_message(conn, &message.id)?.is_empty(),
            None => false,
        })
    })??;
    if last_tools { transcript(session) } else { Ok(Vec::new()) }
}

/// The whole conversation for the Ctrl+O transcript, as in the reference CLI:
/// each prompt, each answer under its time and model, its thinking and every
/// tool call in full.
pub fn transcript(session: &Session) -> Result<Vec<crate::terminal::change_view::Line>> {
    transcript_of(session, &session.id)
}

/// The transcript of any saved conversation, for the /resume preview.
pub fn transcript_of(session: &Session, id: &str) -> Result<Vec<crate::terminal::change_view::Line>> {
    use crate::terminal::{change_view, tool_view};
    let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols.saturating_sub(1) as usize);
    session.store.db.with_conn(|conn| {
        let rows = claude_core::db::message_repo::get_messages_by_conversation(conn, id)?;
        let mut lines = Vec::new();
        for message in &rows {
            if message.is_compact_boundary {
                lines.push(change_view::muted("\u{273b} Conversation compacted"));
                lines.extend(message.content.lines().map(|line| change_view::muted(&format!("  {line}"))));
                continue;
            }
            if message.role == "user" {
                if let Some(summary) = claude_core::tools::background_shell::notification_summary(&message.content) {
                    lines.push(change_view::body(&format!("\u{25cf} {summary}")));
                    continue;
                }
                let mut text = message.content.lines();
                lines.push(change_view::body(&format!("{} {}", crate::terminal::prompt_glyph(), text.next().unwrap_or_default())));
                lines.extend(text.map(|line| change_view::body(&format!("  {line}"))));
                continue;
            }
            let time = chrono::DateTime::parse_from_rfc3339(&message.created_at)
                .map(|time| time.with_timezone(&chrono::Local).format("%I:%M %p").to_string())
                .unwrap_or_default();
            let header = format!("{time} {}", session.model);
            lines.push(change_view::muted(&format!("{header:>width$}")));
            if let Some(thinking) = message.thinking.as_deref().filter(|text| !text.trim().is_empty()) {
                let mut text = thinking.lines();
                lines.push(change_view::muted(&format!("\u{2234} {}", text.next().unwrap_or_default())));
                lines.extend(text.take(400).map(|line| change_view::muted(&format!("  {line}"))));
            }
            for tool in claude_core::db::message_repo::get_tool_calls_for_message(conn, &message.id)? {
            let input = tool
                .input
                .as_deref()
                .and_then(|input| serde_json::from_str(input).ok())
                .unwrap_or_default();
            lines.extend(tool_view::expanded(
                &tool.name,
                &input,
                tool.output.as_deref().unwrap_or(""),
                tool.is_error,
                Some(&session.workspace),
            ));
            }
            let mut text = message.content.lines();
            if let Some(first) = text.next() {
                lines.push(change_view::body(&format!("\u{25cf} {first}")));
                lines.extend(text.map(|line| change_view::body(&format!("  {line}"))));
            }
        }
        Ok(lines)
    })?
}

pub async fn handle(line: &str, paths: &RuntimePaths, session: &mut Session) -> Result<bool> {
    let line = line.trim();
    let (command, argument) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
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
            let context = crate::services::context(paths)?;
            let report = tokio::select! {
                report = claude_core::services::compaction::compact_with_hooks(
                    &session.engine, &context, &session.workspace, &session.id,
                    &session.provider_id, &session.model, argument.trim(), "manual",
                ) => report?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Compaction interrupted; check /history for a completed summary. Original transcript retained"),
            };
            for warning in &report.warnings { eprintln!("Hook: {warning}"); }
            // As in the reference CLI; the summary is in the Ctrl+O transcript.
            eprintln!(
                "  \u{23bf}  Compacted (ctrl+o to see full summary)\n     {} messages summarized; the original transcript remains in /history.",
                report.summarized_messages
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
                claude_core::db::message_repo::get_messages_by_conversation(conn, &session.id)
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
