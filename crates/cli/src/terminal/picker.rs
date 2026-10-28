//! An inline picker for commands such as /resume and /rewind, as in the
//! reference CLI: a heading, an optional search box, two-line entries with a
//! `❯` marker, and a key hint. Enter chooses, Esc cancels.
use super::input::editor::{clipped, DisplayLine, Screen, TextStyle};
use crossterm::{
    event::{Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use std::io::{self, IsTerminal};

pub struct Entry {
    pub title: String,
    /// Shown muted under the title, such as `24 minutes ago · main · 168KB`.
    pub detail: String,
}

pub struct Picker<'a> {
    pub heading: &'a str,
    pub subtitle: Option<&'a str>,
    pub entries: &'a [Entry],
    /// Typing filters the entries by title and detail.
    pub search: bool,
    pub hint: &'a str,
    pub selected: usize,
}

/// Whether the terminal can show a picker; otherwise commands print text.
pub fn available() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal() && !crate::terminal::screen_reader()
}

impl Picker<'_> {
    /// The chosen entry's index, or None after Esc or Ctrl+C.
    pub fn run(&self) -> io::Result<Option<usize>> {
        let raw = terminal::is_raw_mode_enabled()?;
        if !raw {
            terminal::enable_raw_mode()?;
        }
        let mut reader = super::paste_input::Reader::new()?;
        let result = self.drive(&mut || reader.next(std::time::Duration::from_millis(100)));
        if !raw {
            terminal::disable_raw_mode()?;
        }
        result
    }

    fn drive(&self, next_event: &mut dyn FnMut() -> io::Result<Option<Event>>) -> io::Result<Option<usize>> {
        let mut screen = Screen::default();
        let mut query = String::new();
        let mut selected = self.selected;
        loop {
            let shown = self.matching(&query);
            selected = selected.min(shown.len().saturating_sub(1));
            let (cols, rows) = terminal::size().unwrap_or((80, 24));
            let (lines, cursor) = self.lines(&shown, selected, &query, cols as usize, rows as usize);
            screen.draw(&lines, cursor, 3)?;
            let Some(Event::Key(key)) = next_event()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => break,
                KeyCode::Char('c') if control => break,
                KeyCode::Enter => {
                    if let Some(index) = shown.get(selected) {
                        screen.suspend()?;
                        return Ok(Some(*index));
                    }
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(shown.len().saturating_sub(1)),
                KeyCode::Char('k') if !self.search => selected = selected.saturating_sub(1),
                KeyCode::Char('j') if !self.search => selected = (selected + 1).min(shown.len().saturating_sub(1)),
                KeyCode::Backspace if self.search => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Char(ch) if self.search && !control => {
                    query.push(ch);
                    selected = 0;
                }
                _ => {}
            }
        }
        screen.suspend()?;
        Ok(None)
    }

    fn matching(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        (0..self.entries.len())
            .filter(|index| {
                let entry = &self.entries[*index];
                query.is_empty() || entry.title.to_lowercase().contains(&query) || entry.detail.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// The frame and the row of the selected entry.
    fn lines(&self, shown: &[usize], selected: usize, query: &str, width: usize, height: usize) -> (Vec<DisplayLine>, usize) {
        let width = width.saturating_sub(1).max(20);
        let mut lines = vec![(clipped(&format!(" {}", self.heading), width), TextStyle::Accent)];
        if let Some(subtitle) = self.subtitle {
            lines.push((clipped(&format!(" {subtitle}"), width), TextStyle::Muted));
        }
        if self.search {
            let text = if query.is_empty() { "\u{2315} Search\u{2026}".to_owned() } else { format!("\u{2315} {query}") };
            lines.push((clipped(&format!(" {text}"), width), if query.is_empty() { TextStyle::Muted } else { TextStyle::Plain }));
        }
        // Two rows an entry; the page follows the selection.
        let page = (height.saturating_sub(lines.len() + 3) / 2).clamp(1, 8);
        let start = selected.saturating_sub(page - 1).min(shown.len().saturating_sub(page));
        let mut cursor = lines.len();
        if shown.is_empty() {
            lines.push((clipped("   No matches", width), TextStyle::Muted));
        }
        for (position, index) in shown.iter().enumerate().skip(start).take(page) {
            let entry = &self.entries[*index];
            let current = position == selected;
            if current {
                cursor = lines.len();
            }
            let marker = if current { " \u{276f} " } else { "   " };
            lines.push((clipped(&format!("{marker}{}", entry.title), width), if current { TextStyle::Selected } else { TextStyle::Plain }));
            if !entry.detail.is_empty() {
                lines.push((clipped(&format!("   {}", entry.detail), width), TextStyle::Muted));
            }
        }
        lines.push((clipped(&format!(" {}", self.hint), width), TextStyle::Muted));
        (lines, cursor)
    }
}

/// A details view such as the reference CLI's "Shell details": plain lines
/// and a hint. Esc, Enter or Space close it (None); `←` returns `'<'` and
/// any of `keys` returns that key.
pub fn details(heading: &str, rows: &[String], hint: &str, keys: &[char]) -> io::Result<Option<char>> {
    let raw = terminal::is_raw_mode_enabled()?;
    if !raw {
        terminal::enable_raw_mode()?;
    }
    let mut reader = super::paste_input::Reader::new()?;
    let result = show_details(heading, rows, hint, keys, &mut || reader.next(std::time::Duration::from_millis(100)));
    if !raw {
        terminal::disable_raw_mode()?;
    }
    result
}

fn show_details(
    heading: &str,
    rows: &[String],
    hint: &str,
    keys: &[char],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> io::Result<Option<char>> {
    let mut screen = Screen::default();
    let (cols, height) = terminal::size().unwrap_or((80, 24));
    let width = (cols as usize).saturating_sub(1).max(20);
    let mut lines = vec![(clipped(&format!(" {heading}"), width), TextStyle::Accent)];
    let room = (height as usize).saturating_sub(4).max(1);
    let skip = rows.len().saturating_sub(room);
    lines.extend(rows.iter().skip(skip).map(|row| (clipped(&format!(" {row}"), width), TextStyle::Plain)));
    lines.push((clipped(&format!(" {hint}"), width), TextStyle::Muted));
    let result = loop {
        screen.draw(&lines, lines.len() - 1, 1)?;
        let Some(Event::Key(key)) = next_event()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => break None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
            KeyCode::Left => break Some('<'),
            KeyCode::Char(ch) if keys.contains(&ch) => break Some(ch),
            _ => {}
        }
    };
    screen.suspend()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn entries() -> Vec<Entry> {
        ["fix the parser", "add tests", "refactor parser"]
            .iter()
            .map(|title| Entry { title: title.to_string(), detail: "1 minute ago".into() })
            .collect()
    }

    #[test]
    fn typing_filters_and_enter_returns_the_original_index() {
        let entries = entries();
        let picker = Picker { heading: "Resume session", subtitle: None, entries: &entries, search: true, hint: "Esc to cancel", selected: 0 };
        let mut keys: Vec<Event> = "refac".chars().map(|ch| Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)))
            .chain([Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))])
            .collect();
        keys.reverse();
        assert_eq!(picker.drive(&mut || Ok(keys.pop())).unwrap(), Some(2));
        let (lines, cursor) = picker.lines(&picker.matching("parser"), 1, "parser", 80, 24);
        assert_eq!(lines[1].0, " \u{2315} parser");
        assert!(lines[cursor].0.starts_with(" \u{276f} refactor parser"));
    }

    #[test]
    fn details_close_or_return_their_keys() {
        let rows = vec!["Status:   running".to_owned()];
        let mut keys = vec![Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)), Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))];
        assert_eq!(show_details("Shell details", &rows, "", &['x'], &mut || Ok(keys.pop())).unwrap(), Some('x'));
        let mut keys = vec![Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))];
        assert_eq!(show_details("Shell details", &rows, "", &['x'], &mut || Ok(keys.pop())).unwrap(), None);
    }

    #[test]
    fn esc_cancels_and_arrows_stay_in_range() {
        let entries = entries();
        let picker = Picker { heading: "Rewind", subtitle: Some("Restore"), entries: &entries, search: false, hint: "", selected: 2 };
        let mut keys = vec![
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        ];
        assert_eq!(picker.drive(&mut || Ok(keys.pop())).unwrap(), None);
        let mut keys = vec![
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
        ];
        assert_eq!(picker.drive(&mut || Ok(keys.pop())).unwrap(), Some(1));
    }
}
