//! Explicit, previewable code/conversation rewind without destroying the source transcript.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{
    db::{conversation_repo, message_repo},
    runtime::RuntimePaths,
    services::{checkpoints, compaction},
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
    if action == "preview" {
        let checkpoint = checkpoints::get(&paths.data_dir, &session.id, id)?;
        ensure_workspace(paths, session, &checkpoint)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&checkpoints::preview(&paths.data_dir, &session.id, id)?)?
        );
        return Ok(true);
    }
    let (restored, rewound) = apply(action, id, paths, session).await?;
    if let Some(count) = restored {
        eprintln!("Restored {count} file changes from checkpoint {id}.");
    }
    if let Some((message, previous)) = rewound {
        eprintln!("Conversation rewound to before message {message} in session {}. Original transcript retained as {previous}.", session.id);
    }
    Ok(true)
}

fn ensure_workspace(paths: &RuntimePaths, session: &Session, checkpoint: &checkpoints::Checkpoint) -> Result<()> {
    let workspace = session
        .store
        .workspace(&session.id, &paths.data_dir.join("workspaces"))?
        .canonicalize()?;
    ensure!(
        workspace == checkpoint.workspace,
        "Checkpoint belongs to a different working directory"
    );
    Ok(())
}

/// Restores a checkpoint's code, conversation or both. Returns how many
/// files were restored, and for a conversation the message it went back
/// before and the session it left.
async fn apply(action: &str, id: &str, paths: &RuntimePaths, session: &mut Session) -> Result<(Option<usize>, Option<(String, String)>)> {
    let checkpoint = checkpoints::get(&paths.data_dir, &session.id, id)?;
    ensure_workspace(paths, session, &checkpoint)?;
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
    let restored = if action == "code" || action == "both" {
        Some(checkpoints::restore(&paths.data_dir, &session.id, id)?.paths.len())
    } else {
        None
    };
    let mut rewound = None;
    if let Some(target) = target {
        crate::background_commands::copy_settings(session, &target)?;
        let previous = session.id.clone();
        session.resume_saved(paths, &target).await?;
        rewound = Some((checkpoint.message_id.clone(), previous));
    }
    Ok((restored, rewound))
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
                // As in the reference CLI: `app.py +5 -1`.
                _ => changed_files(checkpoint, false),
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
    choices.push(("from", "Summarize from here"));
    if messages.iter().position(|message| message.id == checkpoint.message_id).is_some_and(|position| position > 0) {
        choices.push(("up-to", "Summarize up to here"));
    }
    choices.push(("", "Never mind"));
    // Numbered as in the reference CLI; the digit picks the option.
    let options: Vec<Entry> = choices.iter().enumerate().map(|(number, (_, label))| Entry { title: format!("{}. {label}", number + 1), detail: String::new() }).collect();
    // The confirmation as in the reference CLI: the message, its age, and
    // what restoring removes and undoes.
    let mut subtitle = format!(
        "Confirm you want to restore to the point before you sent this message:\n\u{2502} {}\n\u{2502} ({})\n\nThis message and everything after it will be removed.",
        entries[index].title,
        ago(messages.iter().find(|message| message.id == checkpoint.message_id).map_or(&checkpoint.created_at, |message| &message.created_at))
    );
    if code {
        subtitle.push_str(&format!("\nThe code will be restored {}.", changed_files(checkpoint, true)));
    }
    let confirm = Picker {
        heading: "Rewind",
        subtitle: Some(&subtitle),
        entries: &options,
        search: false,
        hint: "\u{26a0} Restored files go back whole, losing later changes by anyone. Bash changes to other files are not undone.",
        selected: 0,
    };
    let Some(action) = confirm.run()?.map(|choice| choices[choice].0).filter(|action| !action.is_empty()) else {
        return Ok(true);
    };
    let prompt = messages.iter().find(|message| message.id == checkpoint.message_id).map(|message| message.content.clone()).unwrap_or_default();
    if action == "from" || action == "up-to" {
        let part = if action == "from" { compaction::Part::From } else { compaction::Part::UpTo };
        let (branch, count) = tokio::select! {
            result = compaction::summarize_part(&session.engine, &session.store.db, &session.id, &checkpoint.message_id, part, &session.provider_id, &session.model) => result?,
            _ = tokio::signal::ctrl_c() => anyhow::bail!("Summary interrupted; the conversation is unchanged"),
        };
        crate::background_commands::copy_settings(session, &branch)?;
        session.resume_saved(paths, &branch).await?;
        redraw(session)?;
        eprintln!(
            "  \u{23bf}  Summarized {count} message{} {} (ctrl+o to see full summary)",
            if count == 1 { "" } else { "s" },
            if part == compaction::Part::From { "from here" } else { "up to here" }
        );
        return Ok(true);
    }
    let (restored, rewound) = apply(action, &checkpoint.id, paths, session).await?;
    if rewound.is_some() {
        // As in the reference CLI: the screen shows the conversation as it
        // now is, and the message is back in the input box to edit or resend.
        redraw(session)?;
        *crate::terminal::NEXT_DRAFT.lock().unwrap_or_else(|error| error.into_inner()) = Some(prompt);
    } else if let Some(count) = restored {
        eprintln!("  \u{23bf}  Restored {count} file{} to before: {}", if count == 1 { "" } else { "s" }, entries[index].title);
    }
    Ok(true)
}

/// `app.py +5 -1`, or for a restore the reverse, `+1 -5 in app.py`.
fn changed_files(checkpoint: &checkpoints::Checkpoint, restore: bool) -> String {
    let files: Vec<(String, (usize, usize))> = checkpoint
        .changes
        .iter()
        .map(|change| {
            let (added, removed) = checkpoints::line_counts(change).unwrap_or_default();
            (change.path.clone(), if restore { (removed, added) } else { (added, removed) })
        })
        .collect();
    if restore {
        let (added, removed) = files.iter().fold((0, 0), |sum, (_, (a, r))| (sum.0 + a, sum.1 + r));
        let target = match files.as_slice() {
            [(path, _)] => path.clone(),
            _ => format!("{} files", files.len()),
        };
        return format!("+{added} -{removed} in {target}");
    }
    let mut shown: Vec<String> = files.iter().take(3).map(|(path, (added, removed))| format!("{path} +{added} -{removed}")).collect();
    if files.len() > 3 {
        shown.push(format!("{} more", files.len() - 3));
    }
    shown.join(", ")
}

/// `28s ago`, `5m ago`, `2h ago` or `3d ago`.
fn ago(created_at: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return created_at.to_owned();
    };
    let seconds = (chrono::Utc::now() - time.with_timezone(&chrono::Utc)).num_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Clears the screen and shows the conversation as it now is.
fn redraw(session: &Session) -> Result<()> {
    eprint!("\x1b[2J\x1b[3J\x1b[H");
    eprintln!("{}", crate::terminal::banner(&session.model, &session.workspace));
    let color = std::env::var_os("NO_COLOR").is_none();
    let width = crossterm::terminal::size().map_or(80, |(columns, _)| columns as usize).saturating_sub(1);
    for line in crate::repl::transcript_of(session, &session.id)? {
        eprintln!("{}", crate::terminal::change_view::render(&line, width, color));
    }
    Ok(())
}
