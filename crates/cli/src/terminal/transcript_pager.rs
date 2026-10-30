//! The detailed transcript (Ctrl+O), as in the reference CLI: a full-screen
//! pager over the whole conversation with less-style keys, search, jumps
//! between prompts, printing to scrollback and opening in an editor.
use super::change_view::{self, Line};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind},
    execute, queue,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
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

fn append_rows(rows: &mut Vec<Row>, text: &str, width: usize) {
    if text.is_empty() {
        rows.push(Row { styled: String::new(), plain: String::new() });
    }
    for line in text.lines() {
        for styled in super::markdown::wrap_styled(line, "", "", width.max(1)) {
            let plain = super::markdown::plain_rendered(&styled);
            rows.push(Row { styled, plain });
        }
    }
}

fn transcript_rows(lines: &[Line], logged: &[String], width: usize, color: bool) -> Vec<Row> {
    let mut all = rows(lines, width, color);
    if !all.is_empty() && !logged.is_empty() && all.last().is_some_and(|row| !row.plain.trim().is_empty()) && !super::markdown::plain_rendered(&logged[0]).trim().is_empty() {
        append_rows(&mut all, "", width);
    }
    for text in logged {
        append_rows(&mut all, text, width);
    }
    all
}

#[derive(Default)]
pub(super) struct HistoryView {
    top: Option<usize>,
    all: Vec<Row>,
    previous: Vec<u8>,
}

impl HistoryView {
    pub(super) fn screen_rows(&self, height: usize, composer: &[String]) -> Vec<String> {
        let page = height.saturating_sub(composer.len());
        let mut result = vec![String::new(); height];
        for (index, row) in self.all.iter().skip(self.top.unwrap_or(0)).take(page).enumerate() { result[index] = row.styled.clone(); }
        for (index, row) in composer.iter().take(height).enumerate() { result[page + index] = row.clone(); }
        result
    }
    pub(super) fn row_index(&self, row: usize, page: usize) -> Option<usize> {
        (row < page).then(|| self.top.unwrap_or(0) + row)
    }
    pub(super) fn wheel(&mut self, up: bool, page: usize) -> io::Result<()> {
        let was_active = self.active();
        let end = self.all.len().saturating_sub(page);
        let top = self.top.unwrap_or(end);
        let next = if up { top.saturating_sub(3) } else { top.saturating_add(3).min(end) };
        if !up && next >= end {
            self.close()?;
            self.previous.clear();
            return Ok(());
        }
        self.top = Some(next);
        if !was_active { self.previous.clear(); execute!(io::stderr(), EnterAlternateScreen)?; }
        Ok(())
    }
    pub(super) fn active(&self) -> bool {
        self.top.is_some()
    }

    pub(super) fn refresh(&mut self, lines: &[Line], logged: &[String], width: usize) {
        self.all = transcript_rows(lines, logged, width.max(1), std::env::var_os("NO_COLOR").is_none());
    }

    pub(super) fn append(&mut self, text: &str, width: usize) {
        append_rows(&mut self.all, text, width);
    }

    pub(super) fn is_navigation(key: KeyEvent) -> bool {
        matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Home | KeyCode::End))
    }

    pub(super) fn navigate(&mut self, key: KeyEvent, page: usize) -> io::Result<bool> {
        if !Self::is_navigation(key) {
            return Ok(false);
        }
        let was_active = self.active();
        self.move_page(key.code, page.max(1));
        if !was_active && self.active() {
            self.previous.clear();
            // Keep the main screen's scrollback intact without capturing the mouse.
            execute!(io::stderr(), EnterAlternateScreen)?;
        } else if was_active && !self.active() {
            execute!(io::stderr(), LeaveAlternateScreen)?;
        }
        Ok(true)
    }

    fn move_page(&mut self, key: KeyCode, page: usize) {
        let end = self.all.len().saturating_sub(page);
        match key {
            KeyCode::PageUp if !self.all.is_empty() => {
                self.top = Some(self.top.unwrap_or(end).saturating_sub(page));
            }
            KeyCode::Home if !self.all.is_empty() => self.top = Some(0),
            KeyCode::PageDown => {
                self.top = self.top.and_then(|top| {
                    let next = top.saturating_add(page);
                    (next < end).then_some(next)
                });
            }
            KeyCode::End => self.top = None,
            _ => {}
        }
    }

    pub(super) fn close(&mut self) -> io::Result<()> {
        if self.top.take().is_some() {
            execute!(io::stderr(), LeaveAlternateScreen)?;
        }
        Ok(())
    }

    pub(super) fn draw(
        &mut self,
        composer: &[String],
        cursor_row: usize,
        cursor_col: usize,
        size: (u16, u16),
    ) -> io::Result<()> {
        let mut frame = Vec::new();
        self.draw_to(&mut frame, composer, cursor_row, cursor_col, size)?;
        if frame == self.previous {
            return Ok(());
        }
        let mut out = io::stderr().lock();
        out.write_all(b"\x1b[?2026h")?;
        out.write_all(&frame)?;
        out.write_all(b"\x1b[?2026l")?;
        out.flush()?;
        self.previous = frame;
        Ok(())
    }

    fn draw_to(
        &mut self,
        out: &mut impl Write,
        composer: &[String],
        cursor_row: usize,
        cursor_col: usize,
        size: (u16, u16),
    ) -> io::Result<()> {
        let height = size.1.max(1) as usize;
        let viewport = super::prompt_display::viewport(composer.len(), cursor_row, height);
        let dropped = viewport.start;
        let composer = &composer[viewport];
        let page = height.saturating_sub(composer.len());
        let top = self.top.unwrap_or(0).min(self.all.len().saturating_sub(page));
        self.top = Some(top);
        for row in 0..height {
            queue!(out, MoveTo(0, row as u16))?;
            if row < page {
                if let Some(line) = self.all.get(top + row) {
                    out.write_all(line.styled.as_bytes())?;
                }
            } else {
                out.write_all(composer[row - page].as_bytes())?;
            }
            queue!(out, Clear(ClearType::UntilNewLine))?;
        }
        let cursor_row = cursor_row.saturating_sub(dropped).min(composer.len().saturating_sub(1));
        queue!(out, MoveTo(cursor_col as u16, (page + cursor_row).min(height - 1) as u16))?;
        Ok(())
    }
}

impl Drop for HistoryView {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// The pager outside the prompt, such as the /resume preview.
pub fn view(lines: &[Line]) -> io::Result<Outcome> {
    view_page(lines, false, FOOTER)
}

/// A standalone help page opens at the beginning of its command list.
pub fn view_help(lines: &[Line]) -> io::Result<Outcome> {
    view_page(lines, true, "  Help · ↑↓ / PgUp / PgDn scroll · / search · Esc / q close")
}

fn view_page(lines: &[Line], from_start: bool, footer: &str) -> io::Result<Outcome> {
    let raw = terminal::is_raw_mode_enabled()?;
    if !raw {
        terminal::enable_raw_mode()?;
    }
    let mut reader = super::paste_input::Reader::new()?;
    let result = run_page(lines, &[], &mut || reader.next(std::time::Duration::from_millis(100)), from_start, footer);
    drop(reader);
    if !raw {
        execute!(io::stderr(), DisableMouseCapture)?;
        terminal::disable_raw_mode()?;
    }
    result
}

pub fn run(
    lines: &[Line],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> io::Result<Outcome> {
    run_with_logged(lines, &[], next_event)
}

pub(super) fn run_with_logged(
    lines: &[Line],
    logged: &[String],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> io::Result<Outcome> {
    run_page(lines, logged, next_event, false, FOOTER)
}

fn run_page(
    lines: &[Line],
    logged: &[String],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
    from_start: bool,
    footer: &str,
) -> io::Result<Outcome> {
    let mut out = io::stderr();
    // Capture scrolling and use the shared retained drag selection.
    execute!(out, EnterAlternateScreen, Hide, EnableMouseCapture)?;
    let result = pager(lines, logged, next_event, &mut out, from_start, footer);
    execute!(out, Show, LeaveAlternateScreen, EnableMouseCapture)?;
    result
}

fn pager(
    lines: &[Line],
    logged: &[String],
    next_event: &mut dyn FnMut() -> io::Result<Option<Event>>,
    out: &mut impl Write,
    from_start: bool,
    footer: &str,
) -> io::Result<Outcome> {
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut size = terminal::size().unwrap_or((80, 24));
    let mut all = transcript_rows(lines, logged, (size.0 as usize).saturating_sub(1).max(10), color);
    // Opens at the end, where the latest turn is.
    let mut top = if from_start { 0 } else { usize::MAX };
    let mut help = false;
    let mut search: Option<String> = None;
    let mut query = String::new();
    let mut redraw = true;
    let mut selection = super::mouse_selection::Selection::default();
    loop {
        let footer_rows = if help { HELP.len() + 1 } else { 2 };
        let page = (size.1 as usize).saturating_sub(footer_rows).max(1);
        top = top.min(all.len().saturating_sub(page));
        if redraw {
            draw(out, &all, top, page, size, help, search.as_deref(), color, footer)?;
        }
        // Only an event changes the page; idle polls draw nothing.
        let Some(event) = next_event()? else {
            redraw = false;
            continue;
        };
        if super::shortcuts::requested(&event) {
            selection.clear()?;
            let mut background = vec![String::new(); size.1 as usize];
            for (index, row) in all.iter().skip(top).take(page).enumerate() { background[index] = row.styled.clone(); }
            super::shortcuts::show(super::shortcuts::Context::Pager, &background, next_event)?;
            redraw = true;
            continue;
        }
        redraw = true;
        let key = match event {
            Event::Resize(cols, rows_count) => {
                size = (cols, rows_count);
                all = transcript_rows(lines, logged, (cols as usize).saturating_sub(1).max(10), color);
                continue;
            }
            Event::Mouse(mouse) => {
                if !matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
                    let mut snapshot = vec![String::new(); size.1 as usize];
                    for (index, row) in all.iter().skip(top).take(page).enumerate() { snapshot[index] = row.styled.clone(); }
                    selection.event(mouse, snapshot)?;
                    redraw = false;
                    continue;
                }
                selection.clear()?;
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
        selection.clear()?;
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
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => top = 0,
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => top = usize::MAX,
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
    footer: &str,
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
        frame.push_str(&muted(&clip(footer, width)));
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
    execute!(out, DisableMouseCapture, EnterAlternateScreen, Hide)?;
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
    fn selection_snapshot_uses_scrolled_rows_without_changing_position() {
        let mut view = HistoryView::default();
        view.refresh(&[], &(0..30).map(|n| format!("row {n}")).collect::<Vec<_>>(), 80);
        view.move_page(KeyCode::PageUp, 5);
        let top = view.top;
        let snapshot = view.screen_rows(6, &["prompt".into()]);
        assert_eq!(snapshot[0], "row 20");
        assert_eq!(snapshot[5], "prompt");
        assert_eq!(view.top, top);
        assert_eq!(view.row_index(2, 5), Some(22));
    }

    #[test]
    fn logged_transcript_rows_keep_styles_and_are_searchable() {
        let all = transcript_rows(
            &[change_view::body("previous turn")],
            &["\x1b[1mANSWER_SENTINEL\x1b[22m".into(), "NOTICE_SENTINEL".into()],
            80,
            true,
        );
        assert_eq!(all[1].plain, "");
        assert_eq!(all[2].plain, "ANSWER_SENTINEL");
        assert!(all[2].styled.contains("\x1b[1m"));
        assert_eq!(find(&all, "answer_sentinel", 0, true), Some(2));
        assert_eq!(all[3].plain, "NOTICE_SENTINEL");
    }

    #[test]
    fn history_page_keys_and_control_home_end_navigate_without_editing() {
        let mut history = HistoryView::default();
        let lines: Vec<Line> = (0..100).map(|row| change_view::body(&format!("line {row}"))).collect();
        history.refresh(&lines, &[], 40);
        assert!(HistoryView::is_navigation(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        assert!(HistoryView::is_navigation(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)));
        assert!(HistoryView::is_navigation(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)));
        assert!(HistoryView::is_navigation(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)));
        assert!(!HistoryView::is_navigation(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
        assert!(!HistoryView::is_navigation(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
        history.move_page(KeyCode::PageDown, 10);
        assert_eq!(history.top, None);
        history.move_page(KeyCode::PageUp, 10);
        assert_eq!(history.top, Some(80));
        history.move_page(KeyCode::PageUp, 10);
        assert_eq!(history.top, Some(70));
        history.move_page(KeyCode::PageDown, 10);
        assert_eq!(history.top, Some(80));
        history.move_page(KeyCode::PageDown, 10);
        assert_eq!(history.top, None);
        history.move_page(KeyCode::Home, 10);
        assert_eq!(history.top, Some(0));
        history.move_page(KeyCode::End, 10);
        assert_eq!(history.top, None);
    }

    #[test]
    fn history_draw_keeps_the_composer_on_the_bottom_and_clamps_after_resize() {
        let mut history = HistoryView::default();
        let lines: Vec<Line> = (0..20).map(|row| change_view::body(&format!("line {row}"))).collect();
        history.refresh(&lines, &[], 40);
        history.move_page(KeyCode::Home, 6);
        let composer = vec!["border".into(), "prompt draft".into(), "border".into(), "footer".into()];
        let mut output = Vec::new();
        history.draw_to(&mut output, &composer, 1, 5, (40, 10)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("line 0"));
        assert!(output.contains("line 5"));
        assert!(!output.contains("line 6"));
        assert!(output.contains("\x1b[8;1Hprompt draft"));
        assert!(output.ends_with("\x1b[8;6H"));
        assert!(!output.contains('\n'));
        history.top = Some(18);
        history.draw_to(&mut Vec::new(), &composer, 1, 5, (40, 12)).unwrap();
        assert_eq!(history.top, Some(12));
        history.top = None;
    }

    #[test]
    fn tiny_history_view_keeps_the_composer_input_visible() {
        let composer = vec!["border".into(), "INPUT_SENTINEL".into(), "border".into(), "footer".into()];
        for height in 1..=3 {
            let mut history = HistoryView::default();
            let mut output = Vec::new();
            history.draw_to(&mut output, &composer, 1, 4, (80, height)).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("INPUT_SENTINEL"));
            let input_row = if height == 1 { 1 } else { 2 };
            assert!(output.ends_with(&format!("\x1b[{input_row};5H")));
            history.top = None;
        }
    }

    #[test]
    fn history_handles_empty_short_and_wrapped_streamed_content() {
        let mut history = HistoryView::default();
        history.move_page(KeyCode::PageUp, 10);
        history.move_page(KeyCode::Home, 10);
        assert_eq!(history.top, None);
        history.refresh(&[change_view::body("first")], &["\x1b[32m中文中文中文\x1b[0m".into()], 4);
        assert!(history.all.len() >= 4);
        history.move_page(KeyCode::Home, 10);
        assert_eq!(history.top, Some(0));
        history.move_page(KeyCode::PageDown, 10);
        assert_eq!(history.top, None);
    }

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
    fn help_opens_at_first_command_and_closes() {
        let lines: Vec<Line> = (0..100).map(|row| change_view::body(&format!("command {row}"))).collect();
        let mut next = || Ok(Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))));
        let mut screen = Vec::new();
        let outcome = pager(&lines, &[], &mut next, &mut screen, true, "Help · Esc to close").unwrap();
        assert!(matches!(outcome, Outcome::Closed));
        let screen = String::from_utf8_lossy(&screen);
        assert!(screen.contains("command 0"));
        assert!(!screen.contains("command 99"));
        assert!(screen.contains("Help · Esc to close"));
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
        let outcome = pager(&lines, &[], &mut next, &mut screen, false, FOOTER).unwrap();
        assert!(matches!(outcome, Outcome::PrintToScrollback));
        let screen = String::from_utf8_lossy(&screen);
        assert!(screen.contains("line 90"), "search moved to the match");
        assert!(screen.contains("Showing detailed transcript"));
    }
}
