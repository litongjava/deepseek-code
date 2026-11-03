//! Read-only session-family browser shared by idle and running terminal views.
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use deepseek_code_core::services::ServiceContext;
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct Entry {
    pub id: String,
    pub label: String,
    pub current: bool,
}

pub(crate) fn entries(context: &ServiceContext, session: &str) -> anyhow::Result<Vec<Entry>> {
    let peers = deepseek_code_core::services::session_messages::session_peers(context, session)?;
    let mut background = std::collections::HashMap::new();
    context.db.with_conn(|conn| -> anyhow::Result<()> {
        let mut query = conn
            .prepare("SELECT value FROM peripheral_settings WHERE key LIKE 'background-agent.%'")?;
        for raw in query.query_map([], |row| row.get::<_, String>(0))? {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw?) {
                if let Some(id) = value["session_id"].as_str() {
                    background.insert(id.to_owned(), value);
                }
            }
        }
        Ok(())
    })??;
    let mut entries = Vec::new();
    for peer in peers {
        let Some(id) = peer["id"].as_str() else {
            continue;
        };
        let record = context
            .setting::<serde_json::Value>(&format!("delegation.{id}"))?
            .or_else(|| background.remove(id));
        let description = context.setting::<String>(&format!("agent-description.{id}"))?;
        let kind = record
            .as_ref()
            .and_then(|v| v["agent_type"].as_str())
            .unwrap_or("default");
        let parent = record.as_ref().and_then(|v| {
            v["parent_session_id"]
                .as_str()
                .or_else(|| v["parent"].as_str())
        });
        let label = if parent.is_none() {
            "Main [default]".to_owned()
        } else {
            format!(
                "{} [{kind}]",
                description
                    .as_deref()
                    .or_else(|| record.as_ref().and_then(|v| v["description"].as_str()))
                    .or_else(|| peer["title"].as_str())
                    .unwrap_or("Agent")
            )
        };
        entries.push(Entry {
            id: id.into(),
            label: label.chars().filter(|ch| !ch.is_control()).collect(),
            current: id == session,
        });
    }
    entries.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(a.label.cmp(&b.label))
            .then(a.id.cmp(&b.id))
    });
    if entries.is_empty() {
        entries.push(Entry {
            id: session.into(),
            label: "Main [default]".into(),
            current: true,
        });
    }
    Ok(entries)
}

pub(crate) struct View {
    context: ServiceContext,
    session: String,
    entries: Vec<Entry>,
    selected: usize,
    watching: bool,
    scroll: usize,
    text: String,
    history: super::scrollback::Scrollback,
    first_row: usize,
    refreshed: Instant,
    entered: bool,
    last_frame: String,
    last_size: (u16, u16),
}

impl View {
    pub(crate) fn new(context: ServiceContext, session: String) -> Self {
        let entries = entries(&context, &session).unwrap_or_else(|error| {
            vec![Entry {
                id: session.clone(),
                label: format!("Main · {error}"),
                current: true,
            }]
        });
        Self {
            context,
            session,
            entries,
            selected: 0,
            watching: false,
            scroll: 0,
            text: String::new(),
            history: Default::default(),
            first_row: 0,
            refreshed: Instant::now() - Duration::from_secs(2),
            entered: false,
            last_frame: String::new(),
            last_size: (0, 0),
        }
    }

    pub(crate) fn enter(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stderr(), crossterm::terminal::EnterAlternateScreen)?;
        self.entered = true;
        Ok(())
    }

    fn watch(&mut self) -> bool {
        if self.entries[self.selected].current {
            return true;
        }
        self.watching = true;
        self.scroll = 0;
        self.text.clear();
        self.refreshed = Instant::now() - Duration::from_secs(2);
        false
    }

    /// Returns true only when the user returns to the original conversation.
    pub(crate) fn event(&mut self, event: &Event) -> bool {
        self.last_frame.clear();
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if super::scrollback::compact_key(*key) {
                    super::scrollback::toggle_compact();
                    return false;
                }
                if key.modifiers.contains(KeyModifiers::ALT)
                    && matches!(key.code, KeyCode::Left | KeyCode::Right)
                {
                    let count = self.entries.len();
                    self.selected = (self.selected
                        + if key.code == KeyCode::Left {
                            count - 1
                        } else {
                            1
                        })
                        % count;
                    return self.watch();
                }
                match key.code {
                    KeyCode::Esc if self.watching => {
                        self.watching = false;
                        self.scroll = 0;
                    }
                    KeyCode::Esc => return true,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return true
                    }
                    KeyCode::Enter => return self.watch(),
                    KeyCode::Up if self.watching => self.scroll = self.scroll.saturating_add(1),
                    KeyCode::Down if self.watching => self.scroll = self.scroll.saturating_sub(1),
                    KeyCode::PageUp if self.watching => {
                        self.scroll = self.scroll.saturating_add(10)
                    }
                    KeyCode::PageDown if self.watching => {
                        self.scroll = self.scroll.saturating_sub(10)
                    }
                    KeyCode::Up => {
                        self.selected =
                            (self.selected + self.entries.len() - 1) % self.entries.len()
                    }
                    KeyCode::Down => self.selected = (self.selected + 1) % self.entries.len(),
                    _ => {}
                }
            }
            Event::Mouse(mouse) if self.watching => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_add(3),
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
                MouseEventKind::Down(crossterm::event::MouseButton::Left) if mouse.row >= 3 => {
                    self.history.click_presentation(
                        self.first_row + mouse.row as usize - 3,
                        self.last_size.0.saturating_sub(2).max(1) as usize,
                    );
                }
                _ => {}
            },
            _ => {}
        }
        false
    }

    pub(crate) fn draw(&mut self) -> io::Result<()> {
        let refresh_due = self.refreshed.elapsed() >= Duration::from_secs(1);
        if refresh_due {
            self.refreshed = Instant::now();
            super::title::background(
                !crate::background_commands::running_agents(
                    &self.context.paths,
                    &self.context.db,
                    &self.session,
                )
                .is_empty(),
            );
            let selected = self.entries[self.selected].id.clone();
            if let Ok(entries) = entries(&self.context, &self.session) {
                self.selected = entries
                    .iter()
                    .position(|entry| entry.id == selected)
                    .unwrap_or(0);
                self.entries = entries;
            }
            if self.watching {
                let history = crate::background_commands::agent_transcript(
                    &self.context,
                    &self.entries[self.selected].id,
                );
                let text = history.all().join("\n");
                if text != self.text {
                    self.text = text;
                    self.history = history;
                }
            }
        }
        let (cols, rows) = crossterm::terminal::size()?;
        if !refresh_due && self.last_size == (cols, rows) && !self.last_frame.is_empty() {
            return Ok(());
        }
        if self.last_size.0 != cols {
            self.history.reflow(cols.saturating_sub(1).max(2) as usize);
        }
        self.last_size = (cols, rows);
        let width = cols.saturating_sub(2).max(1) as usize;
        let height = rows.saturating_sub(5).max(1) as usize;
        let mut lines = vec![
            "Subagents".to_owned(),
            "Select an agent to watch. alt+← previous, alt+→ next.".into(),
            String::new(),
        ];
        if self.watching {
            lines[0] = format!(
                "Subagents · {} · {}",
                self.entries[self.selected].label, self.entries[self.selected].id
            );
            let body = self
                .history
                .presentation()
                .iter()
                .flat_map(|line| super::markdown::wrap_styled(line, "", "", width))
                .collect::<Vec<_>>();
            let start = body
                .len()
                .saturating_sub(height)
                .saturating_sub(self.scroll);
            self.first_row = start;
            lines.extend(body.into_iter().skip(start).take(height));
        } else {
            let start = self.selected.saturating_sub(height - 1);
            for (index, entry) in self.entries.iter().enumerate().skip(start).take(height) {
                let line = format!(
                    "{} {}. ● {}{}  {}",
                    if index == self.selected { "❯" } else { " " },
                    index + 1,
                    entry.label,
                    if entry.current { " (current)" } else { "" },
                    entry.id
                );
                let line = super::input::editor::clipped(&line, width);
                lines.push(if index == self.selected {
                    format!("\x1b[30;104m{line}\x1b[0m")
                } else {
                    line
                });
            }
        }
        let mut output = String::from("\x1b7");
        for row in 0..rows as usize {
            let line = if row == rows.saturating_sub(1) as usize {
                if self.watching {
                    "↑/↓ scroll · click expand · alt+m compact · alt+←/→ switch · esc list"
                } else {
                    "enter select · esc back"
                }
            } else {
                lines.get(row).map(String::as_str).unwrap_or("")
            };
            output.push_str(&format!(
                "\x1b[{};1H\x1b[2K{}",
                row + 1,
                super::markdown::wrap_styled(line, "", "", width)
                    .first()
                    .map(String::as_str)
                    .unwrap_or("")
            ));
        }
        output.push_str("\x1b8");
        if self.last_frame == output {
            return Ok(());
        }
        self.last_frame.clone_from(&output);
        let mut out = io::stderr().lock();
        out.write_all(output.as_bytes())?;
        out.flush()
    }
}

impl Drop for View {
    fn drop(&mut self) {
        if self.entered {
            let _ = crossterm::execute!(io::stderr(), crossterm::terminal::LeaveAlternateScreen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use deepseek_code_core::runtime::{RuntimePaths, SessionStore};

    fn fixture() -> (tempfile::TempDir, ServiceContext, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let context = crate::services::context(&paths).unwrap();
        let store = SessionStore::new(context.db.clone());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let child = store.fork(&parent).unwrap();
        let unrelated = store.fork(&parent).unwrap();
        context.save_setting(&format!("delegation.{child}"), &serde_json::json!({"session_id":child,"parent_session_id":parent,"agent_type":"general","status":"completed","started_at":chrono::Utc::now().to_rfc3339()})).unwrap();
        context
            .save_setting(&format!("agent-description.{child}"), &"Inspect child")
            .unwrap();
        store.append(&child, "user", "CHILD_USER_SENTINEL").unwrap();
        let assistant = store
            .append(
                &child,
                "assistant",
                "CHILD_ANSWER_SENTINEL **bold** and `code`",
            )
            .unwrap();
        let output = (1..=9)
            .map(|number| format!("CHILD_TOOL_LINE_{number}"))
            .collect::<Vec<_>>()
            .join("\n");
        context
            .db
            .with_conn(|conn| {
                deepseek_code_core::db::message_repo::insert_tool_call(
                    conn,
                    "child-tool",
                    &assistant,
                    "Read",
                    Some(r#"{"file_path":"fixture.md"}"#),
                    Some(&output),
                    false,
                    0,
                )
            })
            .unwrap()
            .unwrap();
        assert!(!entries(&context, &parent)
            .unwrap()
            .iter()
            .any(|e| e.id == unrelated));
        (temp, context, parent, child)
    }

    #[test]
    fn family_picker_preserves_current_session_and_completed_child_messages() {
        let (_temp, context, parent, child) = fixture();
        let mut view = View::new(context.clone(), parent.clone());
        assert_eq!(view.entries.len(), 2);
        assert!(view.entries[0].current);
        assert!(view.entries[0].label.starts_with("Main"));
        assert!(view.entries[1].label.contains("Inspect child [general]"));
        assert!(!view.event(&Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE
        ))));
        assert!(!view.event(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        ))));
        assert!(view.watching);
        assert_eq!(view.entries[view.selected].id, child);
        let body = crate::background_commands::agent_execution(&context, &child);
        assert!(body.contains(&format!(
            "{} CHILD_USER_SENTINEL",
            crate::terminal::prompt_glyph()
        )));
        assert!(body.contains("● CHILD_ANSWER_SENTINEL"));
        assert!(!view.event(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))));
        assert!(!view.watching);
        assert!(view.event(&Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))));
        assert_eq!(view.session, parent);
        let child_entries = entries(&context, &child).unwrap();
        assert!(child_entries[0].current);
        assert!(child_entries
            .iter()
            .any(|e| e.id == parent && e.label.starts_with("Main")));
    }

    #[test]
    #[ignore = "interactive ConPTY acceptance"]
    fn terminal_fixture() {
        let (_temp, context, parent, _) = fixture();
        let keys = context.paths.data_dir.join("keys.json");
        let mut display = super::super::live::TurnDisplay::new(true, "ask", &keys, vec![]).unwrap();
        display.set_shell_context(&context.paths.data_dir, &parent);
        display.before_text("MAIN_TRANSCRIPT_SENTINEL\n");
        super::super::title::start();
        let mut notifications = super::super::scrollback::Scrollback::default();
        super::super::prompt_display::write_recorded(&mut io::stderr(), &mut notifications,
            "<task-notification>\n<summary>Agent NOTICE_SENTINEL completed</summary>\n</task-notification>", 130, true).unwrap();
        notifications.save();
        display.queued_delivered("<task-notification>\n<summary>Agent INTERNAL_NOTICE completed</summary>\n</task-notification>");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            while let Some(action) = display.action().await {
                match action {
                    super::super::live::Action::Submit(text) if text == "/tasks" => break,
                    super::super::live::Action::Cancel => break,
                    _ => {}
                }
            }
        });
        display.finish(None);
        let mut editor = super::super::input::editor::PromptEditor::default();
        editor.set_subagents(context, parent);
        editor.set_agent_rows(Box::new(|| {
            vec![crate::background_commands::Agent {
                session_id: "fixture-background".into(),
                agent_type: "general".into(),
                description: "RUNNING_BACKGROUND_SENTINEL".into(),
                started_ms: chrono::Utc::now().timestamp_millis() as u64,
                output_tokens: 0,
                tokens_estimated: false,
            }]
        }));
        editor.readline("IDLE_SENTINEL ❯ ").unwrap();
        super::super::title::background(false);
    }
}
