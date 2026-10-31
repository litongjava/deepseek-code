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

/// An extra key a picker hands back with the entry it was pressed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    /// Space, while the search box is empty.
    Space,
    Ctrl(char),
    Left,
    Right,
}

#[derive(Debug, PartialEq)]
pub enum Picked {
    Entry(usize),
    Key(Key, Option<usize>),
    Cancel,
}

fn with_raw<T>(run: impl FnOnce(&mut dyn FnMut() -> io::Result<Option<Event>>) -> io::Result<T>) -> io::Result<T> {
    let raw = terminal::is_raw_mode_enabled()?;
    if !raw {
        terminal::enable_raw_mode()?;
    }
    let mut reader = super::paste_input::Reader::new()?;
    let result = run(&mut || reader.next(std::time::Duration::from_millis(100)));
    drop(reader);
    if !raw {
        terminal::disable_raw_mode()?;
    }
    result
}

impl Picker<'_> {
    /// The chosen entry's index, or None after Esc or Ctrl+C.
    pub fn run(&self) -> io::Result<Option<usize>> {
        Ok(match with_raw(|next| self.drive_keys(&[], next))? {
            Picked::Entry(index) => Some(index),
            _ => None,
        })
    }

    /// Like `run`, also returning any of `keys`.
    pub fn run_keys(&self, keys: &[Key]) -> io::Result<Picked> {
        with_raw(|next| self.drive_keys(keys, next))
    }

    fn drive(&self, next_event: &mut dyn FnMut() -> io::Result<Option<Event>>) -> io::Result<Option<usize>> {
        Ok(match self.drive_keys(&[], next_event)? {
            Picked::Entry(index) => Some(index),
            _ => None,
        })
    }

    fn drive_keys(&self, keys: &[Key], next_event: &mut dyn FnMut() -> io::Result<Option<Event>>) -> io::Result<Picked> {
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
            if query.is_empty() && super::shortcuts::requested(&Event::Key(key)) {
                screen.shortcut_popup(super::shortcuts::Context::Picker, next_event)?;
                continue;
            }
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            let extra = match key.code {
                KeyCode::Char(' ') if query.is_empty() && !control => Some(Key::Space),
                KeyCode::Char(ch) if control => Some(Key::Ctrl(ch)),
                KeyCode::Tab => Some(Key::Tab),
                KeyCode::Left => Some(Key::Left),
                KeyCode::Right => Some(Key::Right),
                _ => None,
            };
            if let Some(extra) = extra.filter(|extra| keys.contains(extra)) {
                screen.suspend()?;
                return Ok(Picked::Key(extra, shown.get(selected).copied()));
            }
            match key.code {
                KeyCode::Esc => break,
                KeyCode::Char('c') if control => break,
                KeyCode::Enter => {
                    if let Some(index) = shown.get(selected) {
                        screen.suspend()?;
                        return Ok(Picked::Entry(*index));
                    }
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(shown.len().saturating_sub(1)),
                KeyCode::Char('k') if !self.search => selected = selected.saturating_sub(1),
                KeyCode::Char('j') if !self.search => selected = (selected + 1).min(shown.len().saturating_sub(1)),
                // In a numbered list, as in the reference CLI, a digit picks its option.
                KeyCode::Char(ch) if !self.search && !control && ch.is_ascii_digit() => {
                    if let Some(index) = shown.iter().find(|index| self.entries[**index].title.starts_with(&format!("{ch}. "))) {
                        screen.suspend()?;
                        return Ok(Picked::Entry(*index));
                    }
                }
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
        Ok(Picked::Cancel)
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
            // Several paragraphs wrap at word boundaries.
            for paragraph in subtitle.split('\n') {
                for row in wrap_words(paragraph, width.saturating_sub(1)) {
                    lines.push((clipped(&format!(" {row}"), width), TextStyle::Muted));
                }
            }
        }
        if self.search {
            let text = if query.is_empty() { "\u{2315} Search\u{2026}".to_owned() } else { format!("\u{2315} {query}") };
            lines.push((clipped(&format!(" {text}"), width), if query.is_empty() { TextStyle::Muted } else { TextStyle::Plain }));
        }
        let spaced = self.heading == "Resume session";
        let page = (height.saturating_sub(lines.len() + 3) / if spaced { 3 } else { 2 }).clamp(1, 8);
        let start = selected.saturating_sub(page - 1).min(shown.len().saturating_sub(page));
        let mut cursor = lines.len();
        if shown.is_empty() {
            lines.push((clipped("   No matches", width), TextStyle::Muted));
        }
        for (position, index) in shown.iter().enumerate().skip(start).take(page) {
            if spaced && position > start { lines.push((String::new(), TextStyle::Plain)); }
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
        for row in wrap_hint(self.hint, width) {
            lines.push((row, TextStyle::Muted));
        }
        (lines, cursor)
    }
}

/// One line of text input, as the reference CLI's "Rename session:".
/// Enter returns the text, Esc returns None.
pub fn text_input(heading: &str, initial: &str) -> io::Result<Option<String>> {
    with_raw(|next| edit_text(heading, initial, next))
}

fn edit_text(heading: &str, initial: &str, next_event: &mut dyn FnMut() -> io::Result<Option<Event>>) -> io::Result<Option<String>> {
    let mut screen = Screen::default();
    let mut text = initial.to_owned();
    let result = loop {
        let width = terminal::size().map_or(80, |(cols, _)| cols as usize).saturating_sub(1).max(20);
        let lines = vec![
            (clipped(&format!(" {heading}"), width), TextStyle::Accent),
            (clipped(&format!(" {text}"), width), TextStyle::Plain),
            (clipped(" Enter to save \u{b7} Esc to cancel", width), TextStyle::Muted),
        ];
        let column = (1 + unicode_width::UnicodeWidthStr::width(text.as_str())).min(width);
        screen.draw(&lines, 1, column)?;
        let Some(Event::Key(key)) = next_event()? else {
            continue;
        };
        if super::shortcuts::requested(&Event::Key(key)) {
            screen.shortcut_popup(super::shortcuts::Context::Text, next_event)?;
            continue;
        }
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Char('c') if control => break None,
            KeyCode::Enter => break Some(text.trim().to_owned()),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if control => text.clear(),
            KeyCode::Char(ch) if !control => text.push(ch),
            _ => {}
        }
    };
    screen.suspend()?;
    Ok(result)
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
    drop(reader);
    if !raw {
        terminal::disable_raw_mode()?;
    }
    result
}

/// Like [`details`], redrawn from `rows` about twice a second, for views of
/// work that is still running.
pub fn details_live(heading: &str, rows: &mut dyn FnMut() -> Vec<String>, hint: &str, keys: &[char]) -> io::Result<Option<char>> {
    let raw = terminal::is_raw_mode_enabled()?;
    if !raw {
        terminal::enable_raw_mode()?;
    }
    let mut reader = super::paste_input::Reader::new()?;
    let mut screen = Screen::default();
    let mut ticks = 0;
    let mut current = rows();
    let result = loop {
        let (cols, height) = terminal::size().unwrap_or((80, 24));
        let width = (cols as usize).saturating_sub(1).max(20);
        let mut lines = vec![(clipped(&format!(" {heading}"), width), TextStyle::Accent)];
        let room = (height as usize).saturating_sub(4).max(1);
        let skip = current.len().saturating_sub(room);
        lines.extend(current.iter().skip(skip).map(|row| (clipped(&format!(" {row}"), width), TextStyle::Plain)));
        lines.push((clipped(&format!(" {hint}"), width), TextStyle::Muted));
        screen.draw(&lines, lines.len() - 1, 1)?;
        let event = reader.next(std::time::Duration::from_millis(100))?;
        let Some(Event::Key(key)) = event else {
            ticks += 1;
            if ticks % 5 == 0 { current = rows(); }
            continue;
        };
        if super::shortcuts::requested(&Event::Key(key)) {
            screen.shortcut_popup(super::shortcuts::Context::Details, &mut || reader.next(std::time::Duration::from_millis(100)))?;
            continue;
        }
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
    drop(reader);
    if !raw {
        terminal::disable_raw_mode()?;
    }
    Ok(result)
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
        if super::shortcuts::requested(&Event::Key(key)) {
            screen.shortcut_popup(super::shortcuts::Context::Details, next_event)?;
            continue;
        }
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => break None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
            KeyCode::Left => break Some('<'),
            KeyCode::Right => break Some('>'),
            KeyCode::Char(ch) if keys.contains(&ch) => break Some(ch),
            _ => {}
        }
    };
    screen.suspend()?;
    Ok(result)
}

/// A paragraph over as many rows as it needs, broken between words.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for word in text.split(' ') {
        let row = rows.last_mut().unwrap();
        if !row.is_empty() && unicode_width::UnicodeWidthStr::width(row.as_str()) + 1 + unicode_width::UnicodeWidthStr::width(word) > width {
            rows.push(word.to_owned());
        } else {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
        }
    }
    rows
}

/// A key hint over as many rows as it needs, broken between its `·` parts.
fn wrap_hint(hint: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for part in hint.split(" \u{b7} ") {
        let row = rows.last_mut().unwrap();
        let joined = if row.is_empty() { format!(" {part}") } else { format!("{row} \u{b7} {part}") };
        if row.is_empty() || unicode_width::UnicodeWidthStr::width(joined.as_str()) <= width {
            *row = joined;
        } else {
            rows.push(format!(" {part}"));
        }
    }
    rows.into_iter().map(|row| clipped(&row, width)).collect()
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
    fn empty_resume_allows_tab_scope_switch() {
        let picker = Picker { heading: "Resume session", subtitle: Some("[cwd] all"), entries: &[], search: true, hint: "Tab: cwd/all", selected: 0 };
        let mut events = vec![Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))].into_iter();
        assert_eq!(picker.drive_keys(&[Key::Tab], &mut || Ok(events.next())).unwrap(), Picked::Key(Key::Tab, None));
    }

    #[test]
    fn shortcuts_preserve_text_draft_and_do_not_submit_it() {
        let mut events = vec![
            Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ].into_iter();
        assert_eq!(edit_text("Rename", "original", &mut || Ok(events.next())).unwrap(), Some("original".into()));
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
    fn extra_keys_return_with_the_selected_entry_and_text_input_edits() {
        let entries = entries();
        let picker = Picker { heading: "Resume session", subtitle: None, entries: &entries, search: true, hint: "", selected: 1 };
        let mut keys = vec![Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))];
        assert_eq!(picker.drive_keys(&[Key::Space], &mut || Ok(keys.pop())).unwrap(), Picked::Key(Key::Space, Some(1)));
        // Once searching, Space is text.
        let mut keys = vec![
            Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        ];
        assert_eq!(picker.drive_keys(&[Key::Space, Key::Ctrl('r')], &mut || Ok(keys.pop())).unwrap(), Picked::Key(Key::Ctrl('r'), Some(0)));
        let mut keys: Vec<Event> = [KeyCode::Enter, KeyCode::Char('x'), KeyCode::Backspace].into_iter().map(|code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE))).collect();
        assert_eq!(edit_text("Rename session:", "old", &mut || Ok(keys.pop())).unwrap().as_deref(), Some("olx"));
    }

    #[test]
    fn long_hints_wrap_between_parts() {
        assert_eq!(wrap_hint("Ctrl+A to show all projects \u{b7} Space to preview \u{b7} Esc to cancel", 50), [" Ctrl+A to show all projects \u{b7} Space to preview", " Esc to cancel"]);
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
