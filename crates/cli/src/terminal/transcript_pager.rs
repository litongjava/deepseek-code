//! The detailed transcript (Ctrl+O), as in the reference CLI: a full-screen
//! pager over the whole conversation with less-style keys, search, jumps
//! between prompts, printing to scrollback and opening in an editor.
use super::change_view::{self, Line};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

pub enum Outcome {
    Closed,
    /// `[`: the caller prints the transcript into the terminal's scrollback.
    PrintToScrollback,
}

const FOOTER: &str = "  Showing detailed transcript \u{b7} ctrl+o to toggle \u{b7} \u{2191}\u{2193} scroll \u{b7} v to open in vi \u{b7} ? for shortcuts";
const HELP: &[&str] = &[
    "  \u{2191}\u{2193} j/k   scroll              /    search                 ctrl+o toggle transcript",
    "  ctrl+u/d half page           n/N  next/prev match        q      exit",
    "  space b  page                [    print to scrollback    ?      close help",
    "  g/G      top/bottom          v    open in vi",
    "  {/}      prev/next prompt",
];

/// One screen row: its styled text and plain text for search and jumps.
struct Row {
    styled: String,
    plain: String,
}

/// Wraps each line at `width` cells; continuation rows keep the line's style.
fn rows(lines: &[Line], width: usize, color: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in lines {
        let plain = change_view::plain(std::slice::from_ref(line));
        let plain = plain.trim_end_matches('\n');
        let mut cells = 0;
        for ch in plain.chars() {
            cells += ch.width().unwrap_or(0);
        }
        if cells <= width {
            rows.push(Row { styled: change_view::render(line, width, color), plain: plain.to_owned() });
            continue;
        }
        let style = line.0.first().map(|span| span.style).unwrap_or(change_view::Style::Body);
        // Continuation rows keep the line's indent: two cells after a `●` or
        // `❯` marker, otherwise its leading spaces.
        let indent = if plain.starts_with('\u{25cf}') || plain.starts_with(crate::terminal::prompt_glyph()) {
            2
        } else {
            plain.chars().take_while(|ch| *ch == ' ').count()
        }
        .min(width / 2);
        let mut current = String::new();
        let mut used = 0;
        for ch in plain.chars() {
            let size = ch.width().unwrap_or(0);
            if used + size > width {
                rows.push(Row { styled: change_view::render(&change_view::styled(&current, style), width, color), plain: std::mem::take(&mut current) });
                current = " ".repeat(indent);
                used = indent;
            }
            current.push(ch);
            used += size;
        }
        rows.push(Row { styled: change_view::render(&change_view::styled(&current, style), width, color), plain: current });
    }
    rows
}

/// The pager outside the prompt, such as the /resume preview.
pub fn view(lines: &[Line]) -> io::Result<Outcome> {
    let raw = terminal::is_raw_mode_enabled()?;
    if !raw {
        terminal::enable_raw_mode()?;
    }
    let mut reader = super::paste_input::Reader::new()?;
    let result = run(lines, &mut || reader.next(std::time::Duration::from_millis(100)));
    if !raw {
        terminal::disable_raw_mode()?;
    }
    result
}

pub fn run(
    lines: &[Line],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> io::Result<Outcome> {
    let mut out = io::stderr();
    // This full-screen pager is the one place the wheel belongs to the app:
    // it scrolls the page. The prompt itself never captures the mouse.
    execute!(out, EnterAlternateScreen, Hide, EnableMouseCapture)?;
    let result = pager(lines, next_event, &mut out);
    execute!(out, Show, DisableMouseCapture, LeaveAlternateScreen)?;
    result
}

fn pager(
    lines: &[Line],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
    out: &mut impl Write,
) -> io::Result<Outcome> {
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut size = terminal::size().unwrap_or((80, 24));
    let mut all = rows(lines, (size.0 as usize).saturating_sub(1).max(10), color);
    // Opens at the end, where the latest turn is.
    let mut top = usize::MAX;
    let mut help = false;
    let mut search: Option<String> = None;
    let mut query = String::new();
    let mut redraw = true;
    loop {
        let footer_rows = if help { HELP.len() + 1 } else { 2 };
        let page = (size.1 as usize).saturating_sub(footer_rows).max(1);
        top = top.min(all.len().saturating_sub(page));
        if redraw {
            draw(out, &all, top, page, size, help, search.as_deref(), color)?;
        }
        // Only an event changes the page; idle polls draw nothing.
        let Some(event) = next_event()? else {
            redraw = false;
            continue;
        };
        redraw = true;
        let key = match event {
            Event::Resize(cols, rows_count) => {
                size = (cols, rows_count);
                all = self::rows(lines, (cols as usize).saturating_sub(1).max(10), color);
                continue;
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => top = top.saturating_sub(super::SCROLL_SPEED.load(std::sync::atomic::Ordering::Relaxed)),
                    MouseEventKind::ScrollDown => top = top.saturating_add(super::SCROLL_SPEED.load(std::sync::atomic::Ordering::Relaxed)),
                    _ => {}
                }
                continue;
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            _ => continue,
        };
        if let Some(text) = search.as_mut() {
            match key.code {
                KeyCode::Esc => search = None,
                KeyCode::Enter => {
                    query = std::mem::take(text).to_lowercase();
                    search = None;
                    if let Some(row) = find(&all, &query, top, true) {
                        top = row;
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(ch) => text.push(ch),
                _ => {}
            }
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, control) {
            (KeyCode::Char('o' | 'c'), true) | (KeyCode::Char('q') | KeyCode::Esc, false) => {
                if help && key.code == KeyCode::Esc {
                    help = false;
                    continue;
                }
                return Ok(Outcome::Closed);
            }
            (KeyCode::Up | KeyCode::Char('k'), false) => top = top.saturating_sub(1),
            (KeyCode::Down | KeyCode::Char('j'), false) => top = top.saturating_add(1),
            (KeyCode::Char('u'), true) => top = top.saturating_sub(page / 2),
            (KeyCode::Char('d'), true) => top = top.saturating_add(page / 2),
            (KeyCode::Char(' ') | KeyCode::PageDown, false) => top = top.saturating_add(page),
            (KeyCode::Char('b') | KeyCode::PageUp, false) => top = top.saturating_sub(page),
            (KeyCode::Char('g') | KeyCode::Home, false) => top = 0,
            (KeyCode::Char('G') | KeyCode::End, false) => top = usize::MAX,
            (KeyCode::Char('{'), false) => {
                top = (0..top.min(all.len())).rev().find(|row| is_prompt(&all[*row])).unwrap_or(0);
            }
            (KeyCode::Char('}'), false) => {
                if let Some(row) = (top + 1..all.len()).find(|row| is_prompt(&all[*row])) {
                    top = row;
                }
            }
            (KeyCode::Char('/'), false) => search = Some(String::new()),
            (KeyCode::Char('n'), false) if !query.is_empty() => {
                if let Some(row) = find(&all, &query, top, true) {
                    top = row;
                }
            }
            (KeyCode::Char('N'), false) if !query.is_empty() => {
                if let Some(row) = find(&all, &query, top, false) {
                    top = row;
                }
            }
            (KeyCode::Char('?'), false) => help = !help,
            (KeyCode::Char('['), false) => return Ok(Outcome::PrintToScrollback),
            (KeyCode::Char('v'), false) => open_in_editor(&all, out)?,
            _ => {}
        }
    }
}

fn is_prompt(row: &Row) -> bool {
    row.plain.starts_with(crate::terminal::prompt_glyph())
}

/// The next (or previous) row after `from` containing `query`, wrapping around.
fn find(rows: &[Row], query: &str, from: usize, forward: bool) -> Option<usize> {
    let count = rows.len();
    (1..=count)
        .map(|step| if forward { (from + step) % count } else { (from + count * 2 - step) % count })
        .find(|row| rows[*row].plain.to_lowercase().contains(query))
}

#[allow(clippy::too_many_arguments)]
fn draw(
    out: &mut impl Write,
    rows: &[Row],
    top: usize,
    page: usize,
    size: (u16, u16),
    help: bool,
    search: Option<&str>,
    color: bool,
) -> io::Result<()> {
    let width = (size.0 as usize).saturating_sub(1);
    let muted = |text: &str| if color { format!("\x1b[2m{text}\x1b[0m") } else { text.to_owned() };
    // Rows overwrite in place inside one synchronized frame: clearing the
    // screen first would show it blank between frames.
    let mut frame = String::from("\x1b[?2026h\x1b[H");
    for row in rows.iter().skip(top).take(page) {
        frame.push_str(&row.styled);
        frame.push_str("\x1b[K\r\n");
    }
    for _ in rows.len().saturating_sub(top).min(page)..page {
        frame.push_str("\x1b[K\r\n");
    }
    frame.push_str(&muted(&"\u{2500}".repeat(width)));
    frame.push_str("\r\n");
    if let Some(text) = search {
        frame.push_str(&format!("/{text}"));
    } else if help {
        frame.push_str(&HELP.iter().map(|line| muted(line)).collect::<Vec<_>>().join("\r\n"));
    } else {
        frame.push_str(&muted(&clip(FOOTER, width)));
    }
    frame.push_str("\x1b[J\x1b[?2026l");
    out.write_all(frame.as_bytes())?;
    out.flush()
}

fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|ch| {
            used += ch.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

/// `v`: the plain transcript in $VISUAL, $EDITOR or vi, then back to the pager.
fn open_in_editor(rows: &[Row], out: &mut impl Write) -> io::Result<()> {
    let mut file = tempfile::Builder::new().prefix("dcl-transcript-").suffix(".txt").tempfile()?;
    for row in rows {
        writeln!(file, "{}", row.plain)?;
    }
    file.flush()?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_owned();
    execute!(out, Show, DisableMouseCapture, LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    let status = std::process::Command::new(&program).args(parts).arg(file.path()).status();
    terminal::enable_raw_mode()?;
    execute!(out, EnableMouseCapture, EnterAlternateScreen, Hide)?;
    if let Err(error) = status {
        eprint!("Could not open {program}: {error}\r\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn long_lines_wrap_and_search_and_prompt_jumps_find_rows() {
        let lines = vec![
            change_view::body(&format!("{} first prompt", crate::terminal::prompt_glyph())),
            change_view::body(&"x".repeat(25)),
            change_view::body("needle here"),
            change_view::body(&format!("{} second prompt", crate::terminal::prompt_glyph())),
        ];
        let rows = rows(&lines, 20, false);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[1].plain, "x".repeat(20));
        assert_eq!(rows[2].plain, "x".repeat(5));
        let needle = find(&rows, "needle", 0, true).unwrap();
        assert!(rows[needle].plain.contains("needle"));
        assert_eq!(find(&rows, "needle", needle, false), Some(needle));
        assert!(is_prompt(&rows[0]));
        assert!(rows.iter().filter(|row| is_prompt(row)).count() == 2);
    }

    #[test]
    fn keys_scroll_search_and_close() {
        let lines: Vec<Line> = (0..100).map(|row| change_view::body(&format!("line {row}"))).collect();
        let mut keys = vec![
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
        ]
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
        let mut next = || Ok(keys.pop().map(Event::Key));
        let mut screen = Vec::new();
        let outcome = pager(&lines, &mut next, &mut screen).unwrap();
        assert!(matches!(outcome, Outcome::PrintToScrollback));
        let screen = String::from_utf8_lossy(&screen);
        assert!(screen.contains("line 90"), "search moved to the match");
        assert!(screen.contains("Showing detailed transcript"));
    }
}
