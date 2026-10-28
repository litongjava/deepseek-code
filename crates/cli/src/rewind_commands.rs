//! Explicit, previewable code/conversation rewind without destroying the source transcript.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use claude_core::{
    db::{conversation_repo, message_repo},
    runtime::RuntimePaths,
    services::checkpoints,
};

pub async fn handle(
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    if command != "/rewind" {
        return Ok(false);
    }
    if argument.trim().is_empty() && crate::terminal::picker::available() {
        return Box::pin(pick(paths, session)).await;
    }
    let mut args = argument.split_whitespace();
    let action = args.next().unwrap_or("list");
    if action == "list" {
        ensure!(
            args.next().is_none(),
            "Usage: /rewind [list|preview ID|code ID|conversation ID|both ID]"
        );
        let saved = checkpoints::list(&paths.data_dir, &session.id)?;
        if saved.is_empty() {
            eprintln!("No checkpoints captured for this session. Checkpoints are created around turns in supported Git worktrees.");
        }
        for checkpoint in saved {
            eprintln!(
                "{}  {}  {} file changes{}\n  message: {}",
                checkpoint.id,
                checkpoint.created_at,
                checkpoint.changes.len(),
                if checkpoint.restored {
                    " (code restored)"
                } else {
                    ""
                },
                checkpoint.message_id
            );
        }
        eprintln!("/rewind preview ID previews conflicts. Choose /rewind code ID, conversation ID, or both ID. Code restores only this turn's recorded file-edit tool targets. Bash and unrelated workspace changes are not tracked; later edits to the same files are protected.");
        return Ok(true);
    }
    ensure!(
        ["preview", "code", "conversation", "both"].contains(&action),
        "Choose preview, code, conversation, or both"
    );
    let id = args.next().context("Checkpoint ID required")?;
    ensure!(
        args.next().is_none(),
        "Unexpected argument after checkpoint ID"
    );
    let checkpoint = checkpoints::get(&paths.data_dir, &session.id, id)?;
    let workspace = session
        .store
        .workspace(&session.id, &paths.data_dir.join("workspaces"))?
        .canonicalize()?;
    ensure!(
        workspace == checkpoint.workspace,
        "Checkpoint belongs to a different working directory"
    );
    if action == "preview" {
        println!(
            "{}",
            serde_json::to_string_pretty(&checkpoints::preview(&paths.data_dir, &session.id, id)?)?
        );
        return Ok(true);
    }
    if action == "code" || action == "both" {
        let report = checkpoints::preview(&paths.data_dir, &session.id, id)?;
        ensure!(!report.restored, "Checkpoint code was already restored");
        ensure!(
            report.conflicts.is_empty(),
            "Files changed after the checkpoint: {}",
            report.conflicts.join(", ")
        );
    }
    let target = if action == "conversation" || action == "both" {
        let messages = session
            .store
            .db
            .with_conn(|conn| message_repo::get_messages_by_conversation(conn, &session.id))??;
        let index = messages
            .iter()
            .position(|message| message.id == checkpoint.message_id)
            .context("Checkpoint message is not present in this conversation")?;
        Some(if index == 0 {
            session.store.clear(&session.id)?
        } else {
            session.store.db.with_conn(|conn| {
                conversation_repo::branch_conversation(conn, &session.id, None, Some(index - 1))
            })??
        })
    } else {
        None
    };
    if action == "code" || action == "both" {
        let report = checkpoints::restore(&paths.data_dir, &session.id, id)?;
        eprintln!(
            "Restored {} file changes from checkpoint {id}.",
            report.paths.len()
        );
    }
    if let Some(target) = target {
        crate::background_commands::copy_settings(session, &target)?;
        let previous = session.id.clone();
        session.resume_saved(paths, &target).await?;
        eprintln!("Conversation rewound to before message {} in session {target}. Original transcript retained as {previous}.", checkpoint.message_id);
    }
    Ok(true)
}

/// As in the reference CLI: choose the prompt to go back before, then what
/// to restore. Each choice runs the matching `/rewind ACTION ID`.
async fn pick(paths: &RuntimePaths, session: &mut Session) -> Result<bool> {
    use crate::terminal::picker::{Entry, Picker};
    let saved = checkpoints::list(&paths.data_dir, &session.id)?;
    if saved.is_empty() {
        eprintln!("No checkpoints captured for this session. Checkpoints are created around turns in supported Git worktrees.");
        return Ok(true);
    }
    let messages = session
        .store
        .db
        .with_conn(|conn| message_repo::get_messages_by_conversation(conn, &session.id))??;
    let mut entries: Vec<Entry> = saved
        .iter()
        .map(|checkpoint| {
            let prompt = messages
                .iter()
                .find(|message| message.id == checkpoint.message_id)
                .map(|message| message.content.lines().next().unwrap_or_default().to_owned())
                .unwrap_or_else(|| checkpoint.message_id.clone());
            let detail = match (checkpoint.changes.len(), checkpoint.restored) {
                (_, true) => "Code already restored".to_owned(),
                (0, _) => "No code changes".to_owned(),
                (1, _) => "1 file changed".to_owned(),
                (count, _) => format!("{count} files changed"),
            };
            Entry { title: prompt, detail }
        })
        .collect();
    entries.push(Entry { title: "(current)".into(), detail: String::new() });
    let current = entries.len() - 1;
    let picker = Picker {
        heading: "Rewind",
        subtitle: Some("Restore the code and/or conversation to the point before\u{2026}"),
        entries: &entries,
        search: false,
        hint: "Enter to continue \u{b7} Esc to cancel",
        selected: current,
    };
    let Some(index) = picker.run()?.filter(|index| *index != current) else {
        return Ok(true);
    };
    let checkpoint = &saved[index];
    let code = !checkpoint.changes.is_empty() && !checkpoint.restored;
    let mut choices = Vec::new();
    if code {
        choices.push(("both", "Restore code and conversation"));
    }
    choices.push(("conversation", "Restore conversation"));
    if code {
        choices.push(("code", "Restore code"));
    }
    choices.push(("", "Never mind"));
    let options: Vec<Entry> = choices.iter().map(|(_, label)| Entry { title: label.to_string(), detail: String::new() }).collect();
    let title = format!("Rewind to before: {}", entries[index].title);
    let confirm = Picker {
        heading: &title,
        subtitle: Some(&entries[index].detail),
        entries: &options,
        search: false,
        hint: "Enter to confirm \u{b7} Esc to cancel",
        selected: 0,
    };
    match confirm.run()?.map(|choice| choices[choice].0) {
        Some(action) if !action.is_empty() => {
            Box::pin(handle("/rewind", &format!("{action} {}", checkpoint.id), paths, session)).await
        }
        _ => Ok(true),
    }
}
