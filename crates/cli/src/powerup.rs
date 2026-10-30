//! Interactive lessons use the prompt's input owner and incremental inline renderer.
use crate::{
    resource_commands::LESSONS,
    terminal::input::editor::{clipped, DisplayLine, Screen, TextStyle},
};
use anyhow::Result;
use deepseek_code_core::runtime::RuntimePaths;
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    style::ResetColor,
    terminal,
};
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal},
    time::Duration,
};
use unicode_width::UnicodeWidthChar;

pub fn available() -> bool {
    !crate::terminal::screen_reader()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

/// Some(None) opens the menu; text-only administrative commands keep their old path.
pub fn interactive_topic(argument: &str) -> Option<Option<usize>> {
    let words: Vec<_> = argument.split_whitespace().collect();
    match words.as_slice() {
        [] => Some(None),
        [id] | ["read", id] => LESSONS.iter().position(|lesson| lesson.0 == *id).map(Some),
        _ => None,
    }
}

#[derive(Default)]
struct Tour {
    selected: usize,
    lesson: Option<usize>,
    completed: BTreeSet<String>,
    scroll: usize,
    demo_mode: usize,
}

impl Tour {
    fn enter(&mut self) -> Option<&'static str> {
        self.scroll = 0;
        match self.lesson.take() {
            Some(index) => Some(LESSONS[index].0),
            None => {
                self.lesson = Some(self.selected);
                None
            }
        }
    }
    fn back(&mut self) -> bool {
        self.scroll = 0;
        self.lesson.take().is_none()
    }
    fn frame(&self, width: usize, height: usize) -> Vec<DisplayLine> {
        let width = width.saturating_sub(1).max(1);
        let budget = height.saturating_sub(5).max(1);
        let mut rows = Vec::new();
        if let Some(index) = self.lesson {
            let (id, title, text) = LESSONS[index];
            rows.push((
                format!(
                    "{} {title}",
                    if self.completed.contains(id) {
                        "✓"
                    } else {
                        "○"
                    }
                ),
                TextStyle::Accent,
            ));
            rows.push((String::new(), TextStyle::Plain));
            let mut body = wrap(text, width);
            body.push(String::new());
            if id == "modes" {
                let modes = [
                    "default · ask before editing",
                    "accept edits · ask before commands",
                    "plan · research and propose",
                    "auto · classify tool requests",
                ];
                body.extend(wrap(
                    &format!("Try Shift+Tab: {} (lesson preview)", modes[self.demo_mode]),
                    width,
                ));
            } else {
                body.extend(wrap(example(id), width));
            }
            let start = self.scroll.min(body.len().saturating_sub(budget));
            rows.extend(
                body.into_iter()
                    .skip(start)
                    .take(budget)
                    .map(|text| (text, TextStyle::Plain)),
            );
            rows.push((String::new(), TextStyle::Plain));
            rows.push((
                "Enter mark done · Esc back · ↑↓ / PgUp/PgDn scroll".into(),
                TextStyle::Muted,
            ));
        } else {
            let done = self.completed.len();
            let filled = done * 16 / LESSONS.len();
            rows.push((
                format!(
                    "{} · {done}/{} unlocked  {}{}",
                    if done == LESSONS.len() {
                        "All powered up"
                    } else {
                        "Power-ups"
                    },
                    LESSONS.len(),
                    "━".repeat(filled),
                    "─".repeat(16 - filled)
                ),
                TextStyle::Accent,
            ));
            rows.push((
                "Open a lesson, try it, and mark it done.".into(),
                TextStyle::Muted,
            ));
            rows.push((String::new(), TextStyle::Plain));
            let start = self.selected.saturating_sub(budget.saturating_sub(1));
            for (index, (id, title, _)) in LESSONS.iter().enumerate().skip(start).take(budget) {
                rows.push((
                    format!(
                        "{} {} {title}",
                        if index == self.selected { "❯" } else { " " },
                        if self.completed.contains(*id) {
                            "✓"
                        } else {
                            "○"
                        }
                    ),
                    if index == self.selected {
                        TextStyle::Selected
                    } else if self.completed.contains(*id) {
                        TextStyle::Success
                    } else {
                        TextStyle::Plain
                    },
                ));
            }
            rows.push((
                "↑↓ select · Enter open · Esc close".into(),
                TextStyle::Muted,
            ));
        }
        // Keep a usable footer even in a very short terminal.
        let footer = rows.pop();
        rows.truncate(height.saturating_sub(1));
        if height > 0 {
            rows.extend(footer);
        }
        rows.into_iter()
            .map(|(text, style)| (clipped(&text, width), style))
            .collect()
    }
}

fn example(id: &str) -> &'static str {
    match id {
        "context" => "Try: Explain src/main.rs:42\nThen run /context to inspect the request context.",
        "rewind" => "Try: /rewind list → /rewind preview ID\nReview a checkpoint before restoring it.",
        "background" => "Try: /subtask inspect the test layout\nUse /tasks to follow progress and read the result.",
        "memory" => "Example CLAUDE.md:\nRun tests with cargo test.\nKeep shared business logic independent of the UI.",
        "mcp" => "Inspect: /mcp\nConfigure from your shell: dcl mcp add --help",
        "skills" => "Example: .claude/skills/review/SKILL.md\n---\nname: review\ndescription: Review local changes\n---\nCheck the diff for correctness.\nThen run /reload-skills and /review.",
        "sessions" => "Try: /rename investigate-tests\nUse /branch alternative to try another approach.",
        "details" => "After a tool finishes, press Ctrl+O for its details.\nPgUp/PgDn scrolls; Ctrl+O closes.",
        "language" => "Try: /ide status\nThen: /ide lsp src/main.rs documentSymbol",
        _ => "",
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for ch in paragraph.chars().filter(|c| !c.is_control()) {
            let cells = ch.width().unwrap_or(0);
            if used + cells > width && !line.is_empty() {
                result.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += cells;
        }
        result.push(line);
    }
    result
}

struct Modal {
    screen: Screen,
}
impl Drop for Modal {
    fn drop(&mut self) {
        let _ = self.screen.suspend();
        let _ = execute!(io::stderr(), Show, ResetColor);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run(paths: &RuntimePaths, argument: &str) -> Result<()> {
    let context = crate::services::context(paths)?;
    let completed = context
        .setting::<BTreeSet<String>>("cli.powerup.completed")?
        .unwrap_or_default()
        .into_iter()
        .filter(|id| LESSONS.iter().any(|lesson| lesson.0 == id))
        .collect();
    let topic = interactive_topic(argument).flatten();
    let mut tour = Tour {
        completed,
        lesson: topic,
        selected: topic.unwrap_or(0),
        ..Default::default()
    };
    terminal::enable_raw_mode()?;
    let mut modal = Modal {
        screen: Screen::default(),
    };
    execute!(io::stderr(), Hide)?;
    let mut reader = crate::terminal::paste_input::Reader::new()?;
    loop {
        let (columns, rows) = terminal::size()?;
        let frame = tour.frame(columns as usize, rows.saturating_sub(1).max(1) as usize);
        modal
            .screen
            .draw(&frame, frame.len().saturating_sub(1), 0)?;
        let event = loop {
            if let Some(event) = reader.next(Duration::from_millis(200))? {
                break event;
            }
        };
        match event {
            Event::Resize(_, rows) => modal.screen.resized(rows)?,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'd'))
                {
                    break;
                }
                match key.code {
                    KeyCode::Esc => {
                        if tour.back() {
                            break;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(id) = tour.enter() {
                            // Publish progress only after saving succeeds.
                            let mut completed = context
                                .setting::<BTreeSet<String>>("cli.powerup.completed")?
                                .unwrap_or_default();
                            completed.insert(id.into());
                            context.save_setting("cli.powerup.completed", &completed)?;
                            tour.completed.insert(id.into());
                        }
                    }
                    KeyCode::Up if tour.lesson.is_none() => {
                        tour.selected = (tour.selected + LESSONS.len() - 1) % LESSONS.len()
                    }
                    KeyCode::Down if tour.lesson.is_none() => {
                        tour.selected = (tour.selected + 1) % LESSONS.len()
                    }
                    KeyCode::Up => tour.scroll = tour.scroll.saturating_sub(1),
                    KeyCode::Down => tour.scroll = tour.scroll.saturating_add(1).min(4096),
                    KeyCode::PageUp => tour.scroll = tour.scroll.saturating_sub(8),
                    KeyCode::PageDown => tour.scroll = tour.scroll.saturating_add(8).min(4096),
                    KeyCode::BackTab if tour.lesson.is_some_and(|i| LESSONS[i].0 == "modes") => {
                        tour.demo_mode = (tour.demo_mode + 1) % 4
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lesson_back_does_not_mark_done_and_enter_returns_exact_topic() {
        let mut tour = Tour {
            selected: 1,
            ..Default::default()
        };
        assert_eq!(tour.enter(), None);
        assert!(!tour.back());
        assert!(tour.completed.is_empty());
        assert_eq!(tour.enter(), None);
        assert_eq!(tour.enter(), Some("modes"));
        assert!(tour.back());
    }
    #[test]
    fn short_and_narrow_frames_keep_selection_and_footer_within_viewport() {
        for lesson in [None, Some(1), Some(6)] {
            let tour = Tour {
                selected: 9,
                lesson,
                ..Default::default()
            };
            for height in [1, 3, 8, 30] {
                let frame = tour.frame(22, height);
                assert!(frame.len() <= height);
                assert!(frame.last().unwrap().0.contains("Enter"));
                assert!(frame
                    .iter()
                    .all(|(line, _)| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 21));
            }
        }
    }
}
