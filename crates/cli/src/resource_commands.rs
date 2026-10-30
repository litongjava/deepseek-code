//! Terminal adapters for public resources and persistent feature lessons.
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{runtime::RuntimePaths, services::cli_resources};
use std::collections::BTreeSet;

pub(crate) const LESSONS: &[(&str, &str, &str)] = &[
    ("context", "Explore a codebase", "Ask a question about a file by including its relative path. Use /context to inspect active history and /compact to summarize a long conversation."),
    ("modes", "Choose permissions", "Use /plan for read-only planning, /permissions ask for approval before tools, or /permissions accept-edits for routine edits. Explicit deny rules remain in force in every mode."),
    ("rewind", "Restore a checkpoint", "After an editing turn, run /rewind list, then /rewind preview ID. /rewind code ID restores recorded file edits; /rewind conversation ID branches the conversation."),
    ("background", "Run a background task", "Use /subtask PROMPT to start a worker with the current context. /tasks lists its state; /tasks show ID reads its result; /tasks cancel ID stops it."),
    ("memory", "Keep project instructions", "Use /memory edit to write CLAUDE.md or /memory local for personal project instructions. They are loaded on each turn."),
    ("mcp", "Connect external tools", "Use /mcp to inspect configured tool servers. The dcl mcp add command registers a stdio server; plugin-provided servers follow the plugin enabled state."),
    ("skills", "Create a reusable skill", "Create .claude/skills/NAME/SKILL.md with YAML name and description, followed by instructions. /reload-skills refreshes the menu; /NAME invokes the skill."),
    ("sessions", "Continue a conversation", "Use /rename TITLE to label this session, /resume to browse saved sessions, and /branch TITLE to continue a copy."),
    ("details", "Inspect work in progress", "Press Ctrl+O to expand tool details; PgUp/PgDn scrolls and Ctrl+O closes. Esc interrupts the active turn."),
    ("language", "Navigate code with LSP", "Install a language-service plugin, then inspect /ide status. /ide lsp FILE documentSymbol lists symbols. Position-based operations use 1-based line and character values."),
];

pub async fn handle(command: &str, argument: &str, paths: &RuntimePaths) -> Result<bool> {
    let argument = argument.trim();
    match command {
        "/radio" | "/stickers" => {
            ensure!(argument.is_empty(), "This command takes no arguments");
            let url = if command == "/radio" {
                cli_resources::RADIO
            } else {
                cli_resources::STICKERS
            };
            eprintln!("Opening {url}");
            cli_resources::open_link(url)?;
        }
        "/mobile" => {
            ensure!(
                ["", "ios", "android"].contains(&argument),
                "Usage: /mobile [ios|android]"
            );
            for android in [false, true]
                .into_iter()
                .filter(|android| argument.is_empty() || *android == (argument == "android"))
            {
                let (platform, url) = if android {
                    ("Android", cli_resources::ANDROID)
                } else {
                    ("iOS", cli_resources::IOS)
                };
                eprintln!("{platform}: {url}\n{}", cli_resources::mobile_qr(android)?);
            }
        }
        "/release-notes" => {
            ensure!(
                ["", "refresh", "all"].contains(&argument),
                "Usage: /release-notes [refresh|all]"
            );
            let notes = tokio::select! {
                notes = cli_resources::release_notes(&paths.data_dir, argument == "refresh") => notes?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Release notes request cancelled"),
            };
            eprintln!(
                "Official Claude Code release notes · {} · fetched {}\n{}",
                if notes.cached { "cached" } else { "live" },
                notes.fetched_at,
                cli_resources::CHANGELOG
            );
            let limit = if argument == "all" { usize::MAX } else { 100 };
            for line in notes.text.lines().take(limit) {
                // Remote Markdown may contain terminal escape sequences.
                println!(
                    "{}",
                    line.chars()
                        .filter(|ch| !ch.is_control() || *ch == '\t')
                        .collect::<String>()
                );
            }
            if notes.text.lines().count() > limit {
                eprintln!("Use /release-notes all to show the complete changelog.");
            }
        }
        "/powerup" => {
            let context = crate::services::context(paths)?;
            let mut completed = context
                .setting::<BTreeSet<String>>("cli.powerup.completed")?
                .unwrap_or_default();
            let words = argument.split_whitespace().collect::<Vec<_>>();
            match words.as_slice() {
                [] | ["list"] => {
                    eprintln!(
                        "Discover CLI features · {}/{} completed",
                        completed.len(),
                        LESSONS.len()
                    );
                    for (id, title, _) in LESSONS {
                        eprintln!(
                            "  {} {id:<12} {title}",
                            if completed.contains(*id) {
                                "✓"
                            } else {
                                "○"
                            }
                        );
                    }
                    eprintln!("/powerup TOPIC opens a lesson; /powerup done TOPIC records completion; /powerup reset clears progress.");
                }
                ["reset"] => {
                    completed.clear();
                    context.save_setting("cli.powerup.completed", &completed)?;
                    eprintln!("Lesson progress reset.");
                }
                [id] | ["read", id] | ["done", id] => {
                    let (_, title, text) = LESSONS
                        .iter()
                        .find(|(key, _, _)| key == id)
                        .context("Unknown lesson; use /powerup to list topics")?;
                    if words.first() == Some(&"done") {
                        completed.insert((*id).into());
                        context.save_setting("cli.powerup.completed", &completed)?;
                        eprintln!("Completed: {title}");
                    } else {
                        eprintln!("{title}\n\n{text}\n\nWhen finished: /powerup done {id}");
                    }
                }
                _ => anyhow::bail!("Usage: /powerup [TOPIC|read TOPIC|done TOPIC|reset]"),
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}
