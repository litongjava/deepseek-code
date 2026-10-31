//! Inline terminal editor. The normal screen preserves transcript scrollback;
//! clicks toggle tool results; dragging selects and copies visible text. No alternate screen is used; all modes are restored on exit.
use crossterm::{
    cursor::{position, MoveDown, MoveTo, MoveToColumn, MoveUp},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute, queue,
    style::{Attribute, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Default)]
struct CommandOverlay {
    previous: Vec<DisplayLine>,
    saved: Vec<String>,
    top: usize,
}
impl CommandOverlay {
    fn draw_to(&mut self, out: &mut impl Write, menu: &[DisplayLine], bottom: usize, background: &[String]) -> io::Result<()> {
        let shown = menu.len().min(bottom);
        let menu = &menu[..shown];
        let top = bottom.saturating_sub(shown);
        if self.previous == menu && (menu.is_empty() || self.top == top) { return Ok(()); }
        out.write_all(b"\x1b[?2026h\x1b7")?;
        for (index, text) in self.saved.iter().enumerate() {
            write!(out, "\x1b[{};1H\x1b[0m{}\x1b[K", self.top + index + 1, text)?;
        }
        self.saved = background.iter().skip(top).take(shown).cloned().collect();
        self.top = top;
        for (index, (text, style)) in menu.iter().enumerate() {
            let color = match style { TextStyle::Selected => crate::terminal::style::selected(), _ => crate::terminal::style::muted() };
            write!(out, "\x1b[{};1H\x1b[0m", top + index + 1)?;
            if std::env::var_os("NO_COLOR").is_none() { queue!(out, SetForegroundColor(color))?; }
            write!(out, "{text}\x1b[0m\x1b[K")?;
        }
        out.write_all(b"\x1b8\x1b[?2026l")?;
        out.flush()?;
        self.previous = menu.to_vec();
        Ok(())
    }
}

pub struct PromptEditor {
    notices: Option<std::sync::mpsc::Receiver<String>>,
    history: Vec<String>,
    pending_draft: String,
    pending_cursor: Option<usize>,
    permission_mode: String,
    commands: Vec<(String, String)>,
    details: DetailsPanel,
    /// The conversation for keyboard scrolling and Ctrl+O.
    transcript: Vec<crate::terminal::change_view::Line>,
    transcript_printed: bool,
    /// Suggested next prompts, generated after a turn; Tab accepts one.
    suggestions: Option<std::sync::mpsc::Receiver<String>>,
    suggestion: Option<String>,
    /// Project root for `@` file completion, and its files once listed.
    workspace: Option<std::path::PathBuf>,
    files: Option<Vec<String>>,
    /// "12% until auto-compact" once the context budget runs low.
    context_note: Option<String>,
    /// Background shells still running, shown as "1 shell · ↓ to manage".
    shells: usize,
    shell_counter: Option<Box<dyn Fn() -> usize + Send>>,
    /// Rows for running background agents at a width, shown under the footer.
    agent_rows: Option<Box<dyn Fn(usize) -> Vec<String> + Send>>,
    /// The model and reasoning effort, at the right of the footer.
    effort: Option<String>,
    /// The todo list that ctrl+t shows above the input.
    todos: Option<Box<dyn Fn() -> Vec<(String, String)> + Send>>,
    /// Vim-style editing (`/vim`).
    vim: bool,
    keybindings: crate::keybindings::Keybindings,
}
impl Default for PromptEditor {
    fn default() -> Self {
        Self {
            notices: None,
            history: Vec::new(),
            pending_draft: String::new(),
            pending_cursor: None,
            permission_mode: "ask".into(),
            details: DetailsPanel::default(),
            transcript: Vec::new(),
            transcript_printed: false,
            suggestions: None,
            suggestion: None,
            workspace: None,
            files: None,
            context_note: None,
            shells: 0,
            shell_counter: None,
            agent_rows: None,
            todos: None,
            effort: None,
            vim: false,
            keybindings: crate::keybindings::Keybindings::default(),
            commands: crate::command_catalog::COMMANDS
                .iter()
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .collect(),
        }
    }
}
struct RawMode;
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), DisableBracketedPaste, DisableMouseCapture, ResetColor);
        let _ = terminal::disable_raw_mode();
    }
}
#[derive(Default)]
struct Buffer {
    chars: Vec<char>,
    cursor: usize,
    pasted: bool,
    paste_end: usize,
}
impl Buffer {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn set(&mut self, text: &str) {
        self.pasted = false;
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }
    fn insert(&mut self, text: &str) {
        // Never allow pasted escape sequences to become terminal commands.
        for ch in text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        {
            self.chars.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }
}
/// Directories never offered by `@` file completion.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "dist", "build", "out", "__pycache__", "vendor"];

/// Project files and directories (with a trailing `/`) relative to `root`,
/// with `/` separators, for `@` mentions.
fn project_files(root: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if !SKIPPED_DIRS.contains(&name.as_str()) {
                        // Directories can be mentioned too, as in the reference CLI.
                        if let Ok(relative) = path.strip_prefix(root) {
                            files.push(format!("{}/", relative.to_string_lossy().replace('\\', "/")));
                        }
                        pending.push(path);
                    }
                }
                Ok(kind) if kind.is_file() => {
                    if let Ok(relative) = path.strip_prefix(root) {
                        files.push(relative.to_string_lossy().replace('\\', "/"));
                    }
                }
                _ => {}
            }
            if files.len() >= 20_000 {
                return files;
            }
        }
    }
    files.sort();
    files
}

/// The `@` word being typed at the end of the prompt, if any.
fn mention(text: &str) -> Option<&str> {
    let word = text.rsplit(char::is_whitespace).next()?;
    word.strip_prefix('@')
}

/// Files for an `@` mention: path prefix, then file name prefix, then
/// substring; shallower paths first within each, so `src/` leads `src/...`.
fn mention_matches(files: &[String], query: &str) -> Vec<(String, String)> {
    let query = query.to_lowercase();
    let mut found: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|file| {
            let lower = file.to_lowercase();
            let name = lower.trim_end_matches('/').rsplit('/').next().unwrap_or(&lower);
            let rank = if lower.starts_with(&query) {
                0
            } else if name.starts_with(&query) {
                1
            } else if lower.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, file.trim_end_matches('/').matches('/').count(), file))
        })
        .collect();
    found.sort();
    found.into_iter().take(50).map(|(_, _, file)| (format!("@{file}"), String::new())).collect()
}

pub(crate) fn matches(commands: &[(String, String)], text: &str) -> Vec<usize> {
    if !text.starts_with('/') || text.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let query = text.trim_start_matches('/').to_lowercase();
    let mut found: Vec<_> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, (name, description))| {
            let name = name.to_lowercase();
            let mut letters = name.chars();
            let rank = if name == text.to_lowercase() {
                0
            } else if name.starts_with(&format!("/{query}")) {
                1
            } else if name.split(['-', '_']).skip(1).any(|word| word.starts_with(&query)) {
                // A word inside the name: `/auto-mode-setup` for `mo`.
                2
            } else if name.contains(&query) {
                3
            } else if name[1..].starts_with(&query[..query.len().min(1)])
                && query.chars().all(|wanted| letters.any(|ch| ch == wanted))
            {
                // Letters in order from the first one: `/memory` for `mo`.
                4
            } else if description.to_lowercase().contains(&query) {
                5
            } else {
                return None;
            };
            // As in the reference CLI: a bare `/` lists commands alphabetically;
            // a typed prefix puts the shortest names first (`/copy` before
            // `/config`).
            let length = if query.is_empty() { 0 } else { name.len() };
            Some((rank, length, name, i))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, _, _, index)| index).collect()
}
/// Completion entries shown at once.
const MENU_ROWS: usize = 5;
fn menu_page(heights: &[usize], selected: usize, budget: usize) -> (usize, usize) {
    if budget == 0 {
        return (0, 0);
    }
    let mut start = 0;
    let mut used = 0;
    for (index, height) in heights.iter().enumerate() {
        let height = (*height).min(budget);
        if used > 0 && used + height > budget {
            if index > selected {
                return (start, index);
            }
            start = index;
            used = 0;
        }
        used += height;
    }
    (start, heights.len())
}

pub(crate) fn clipped(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .filter(|c| !c.is_control())
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextStyle {
    /// A permission mode label in its own colour.
    Colored(crossterm::style::Color),
    Accent,
    Success,
    Border,
    Plain,
    Muted,
    Selected,
}
pub(crate) type DisplayLine = (String, TextStyle);

#[derive(Clone, Copy, Debug)]
pub(crate) struct HitTarget {
    pub(crate) row: usize,
    start: usize,
    end: usize,
}
impl HitTarget {
    pub(crate) fn contains(self, row: usize, column: usize) -> bool {
        row == self.row && column >= self.start && column < self.end
    }
}
#[derive(Default)]
pub(crate) struct DetailsPanel {
    entries: Vec<(String, String)>,
    pub(crate) expanded: bool,
    scroll: usize,
    cached_width: usize,
    wrapped: Vec<String>,
}
pub(crate) struct PanelFrame {
    pub(crate) lines: Vec<DisplayLine>,
    pub(crate) toggle: Option<HitTarget>,
    pub(crate) viewport: usize,
}
/// Display untrusted tool output as text, never terminal escape instructions.
fn wrap_detail(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut cells = 0;
    for ch in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
        if ch == '\n' {
            lines.push(String::new());
            cells = 0;
            continue;
        }
        if ch.is_control() && ch != '\t' {
            continue;
        }
        let value = if ch == '\t' {
            "    ".to_string()
        } else {
            ch.to_string()
        };
        for visible in value.chars() {
            let size = visible.width().unwrap_or(0);
            if size > width {
                continue;
            }
            if cells + size > width {
                lines.push(String::new());
                cells = 0;
            }
            lines.last_mut().unwrap().push(visible);
            cells += size;
        }
    }
    lines
}
impl DetailsPanel {
    pub(crate) fn update_entries(&mut self, entries: Vec<(String, String)>) {
        self.entries = entries;
        self.cached_width = 0;
        self.wrapped.clear();
    }

    /// Append streamed detail without cloning unrelated tool output. Wrapping
    /// remains lazy and happens at most once when the next expanded frame draws.
    pub(crate) fn append_body(&mut self, index: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some((_, body)) = self.entries.get_mut(index) {
            body.push_str(text);
            self.cached_width = 0;
            self.wrapped.clear();
        }
    }

    pub(crate) fn toggle(&mut self) {
        if !self.entries.is_empty() {
            self.expanded = !self.expanded;
        }
    }
    pub(crate) fn scroll_by(&mut self, amount: isize, viewport: usize) {
        if self.expanded {
            self.scroll = self
                .scroll
                .saturating_add_signed(amount)
                .min(self.wrapped.len().saturating_sub(viewport.max(1)));
        }
    }
    pub(crate) fn render(&mut self, width: usize, budget: usize) -> PanelFrame {
        let mut frame = PanelFrame {
            lines: Vec::new(),
            toggle: None,
            viewport: 0,
        };
        if self.entries.is_empty() || budget == 0 {
            return frame;
        }
        let entry_label = if self.entries.len() == 1 {
            "entry"
        } else {
            "entries"
        };
        if !self.expanded || budget < 3 {
            let prefix = format!("{} {entry_label} \u{00b7} ", self.entries.len());
            let label = clipped(&format!("{prefix}Show more (Ctrl+O)"), width);
            frame.toggle = Some(HitTarget {
                row: 0,
                start: prefix.chars().count(),
                end: label.chars().count(),
            });
            frame.lines.push((label, TextStyle::Selected));
            return frame;
        }
        if self.cached_width != width || self.wrapped.is_empty() {
            self.wrapped.clear();
            for (title, body) in &self.entries {
                self.wrapped.extend(wrap_detail(title, width));
                self.wrapped.extend(wrap_detail(body, width));
                self.wrapped.push(String::new());
            }
            self.cached_width = width;
        }
        frame.viewport = budget.saturating_sub(2).min(self.wrapped.len()).max(1);
        self.scroll = self
            .scroll
            .min(self.wrapped.len().saturating_sub(frame.viewport));
        frame.lines.push((
            clipped(
                &format!(
                    "Details \u{00b7} {} {entry_label} \u{00b7} lines {}-{} / {}",
                    self.entries.len(),
                    self.scroll + 1,
                    (self.scroll + frame.viewport).min(self.wrapped.len()),
                    self.wrapped.len()
                ),
                width,
            ),
            TextStyle::Muted,
        ));
        frame.lines.extend(
            self.wrapped
                .iter()
                .skip(self.scroll)
                .take(frame.viewport)
                .map(|line| (line.clone(), TextStyle::Plain)),
        );
        let label = clipped(
            "Show less (Ctrl+O) \u{00b7} PgUp/PgDn scroll",
            width,
        );
        frame.toggle = Some(HitTarget {
            row: frame.lines.len(),
            start: 0,
            end: "Show less (Ctrl+O)".len().min(width),
        });
        frame.lines.push((label, TextStyle::Selected));
        frame
    }
}

#[derive(Default)]
pub(crate) struct Screen {
    cursor_row: usize,
    height: usize,
    previous: Vec<DisplayLine>,
    invalidated: bool,
    cursor_col: usize,
    /// The prompt follows transcript output and scrolls only when needed instead of
    /// following the cursor. Modals that follow output stay unpinned.
    pinned: bool,
    /// Screen row of the frame's first line while pinned.
    top: Option<u16>,
    /// End of transcript before the composer was pinned below empty screen rows.
    transcript_end: Option<usize>,
}
impl Screen {
    /// A frame pinned to the bottom rows, as the prompt is.
    pub(crate) fn pinned() -> Self {
        Self {
            pinned: true,
            ..Self::default()
        }
    }
    pub(crate) fn resized(&mut self, rows: u16) -> io::Result<()> {
        let (_, cursor_y) = position()?;
        self.cursor_row = self.cursor_row.min(cursor_y as usize);
        self.height = self.height.min(rows.max(1) as usize);
        self.invalidated = true;
        Ok(())
    }
    /// The terminal reflowed the frame; its screen row is unknown until the
    /// next draw places it again.
    pub(crate) fn reflow(&mut self) {
        self.invalidated = true;
        self.previous.clear();
        self.top = None;
    }
    pub(crate) fn draw(&mut self, lines: &[DisplayLine], row: usize, col: usize) -> io::Result<()> {
        // The editor redraws on every poll; an unchanged frame writes nothing.
        if !self.invalidated && self.previous == lines && self.cursor_row == row && self.cursor_col == col {
            return Ok(());
        }
        if self.pinned {
            return self.draw_pinned(lines, row, col);
        }
        self.cursor_col = col;
        let mut out = io::stderr().lock();
        // Synchronized output keeps a large repaint from showing half drawn.
        out.write_all(b"\x1b[?2026h")?;
        let result = self.draw_to(&mut out, lines, row, col);
        out.write_all(b"\x1b[?2026l")?;
        out.flush()?;
        result
    }

    fn draw_pinned(&mut self, lines: &[DisplayLine], row: usize, col: usize) -> io::Result<()> {
        let rows = terminal::size().map(|(_, rows)| rows).unwrap_or(24).max(1);
        let (_, cursor_y) = position()?;
        self.cursor_col = col;
        let mut out = io::stderr().lock();
        out.write_all(b"\x1b[?2026h")?;
        let result = self.draw_pinned_to(&mut out, lines, row, col, rows as usize, cursor_y as usize);
        out.write_all(b"\x1b[?2026l")?;
        out.flush()?;
        result
    }

    /// Draws `lines` at the bottom, leaving short transcripts at the top. Content above the
    /// frame is scrolled up rather than overwritten, so the transcript (and
    /// the terminal's scrollback) keeps everything said.
    fn draw_pinned_to(
        &mut self,
        out: &mut impl Write,
        lines: &[DisplayLine],
        row: usize,
        col: usize,
        rows: usize,
        cursor_y: usize,
    ) -> io::Result<()> {
        let viewport = crate::terminal::prompt_display::viewport(lines.len(), row, rows);
        let dropped = viewport.start;
        let lines = &lines[viewport];
        let row = row.saturating_sub(dropped).min(lines.len().saturating_sub(1));
        let height = lines.len().max(1);
        let bottom_limit = rows - height;
        // Rows below the old frame's top belong to the frame. Before the first
        // draw, and after a resize moved it, the cursor shows where it starts.
        let first = self.height == 0;
        let anchor = self
            .top
            .map(|old| old as usize)
            .unwrap_or_else(|| if first { cursor_y } else { cursor_y.saturating_sub(row) })
            .min(rows - 1);
        let top = bottom_limit;
        if first { self.transcript_end = Some(cursor_y.min(rows - 1)); }
        if anchor > top {
            self.transcript_end = self.transcript_end.map(|end| end.saturating_sub(anchor - top));
        }
        let repaint = self.invalidated || anchor != top || self.height != height;
        let mut frame = Vec::new();
        if anchor > top {
            // The frame grew: scroll the rows above it into scrollback so the
            // whole frame fits on screen.
            queue!(frame, MoveTo(0, (rows - 1) as u16))?;
            for _ in top..anchor {
                frame.extend_from_slice(b"\r\n");
            }
        } else if self.height > height || first || self.invalidated {
            // The frame shrank: erase the rows it no longer covers.
            queue!(frame, MoveTo(0, anchor.min(top) as u16), Clear(ClearType::FromCursorDown))?;
        }
        for (offset, (text, style)) in lines.iter().enumerate() {
            if !repaint && self.previous.get(offset) == Some(&(text.clone(), *style)) {
                continue;
            }
            queue!(frame, MoveTo(0, (top + offset) as u16))?;
            if std::env::var_os("NO_COLOR").is_none() {
                match style {
                    TextStyle::Colored(color) => queue!(frame, SetForegroundColor(*color))?,
                    TextStyle::Accent => queue!(
                        frame,
                        SetAttribute(Attribute::Bold),
                        SetForegroundColor(crate::terminal::style::accent())
                    )?,
                    TextStyle::Success => {
                        queue!(frame, SetForegroundColor(crate::terminal::style::success()))?
                    }
                    TextStyle::Border => {
                        queue!(frame, SetForegroundColor(crate::terminal::style::border()))?
                    }
                    TextStyle::Plain => queue!(frame, ResetColor)?,
                    TextStyle::Muted => queue!(frame, SetForegroundColor(crate::terminal::style::muted()))?,
                    TextStyle::Selected => queue!(frame, SetForegroundColor(crate::terminal::style::selected()))?,
                }
            }
            write!(frame, "{text}")?;
            // Overwrite first, then erase only the old trailing cells.
            if matches!(style, TextStyle::Accent) {
                queue!(frame, SetAttribute(Attribute::Reset))?;
            }
            queue!(frame, ResetColor, Clear(ClearType::UntilNewLine))?;
        }
        queue!(frame, MoveTo(col as u16, (top + row) as u16))?;
        out.write_all(&frame)?;
        out.flush()?;
        self.cursor_row = row;
        self.height = height;
        self.previous = lines.to_vec();
        self.top = Some(top as u16);
        self.invalidated = false;
        Ok(())
    }

    fn draw_to(
        &mut self,
        out: &mut impl Write,
        lines: &[DisplayLine],
        row: usize,
        col: usize,
    ) -> io::Result<()> {
        // Assemble a complete frame before touching the terminal. Repainting must
        // not briefly erase the input/menu or scroll an already allocated region.
        let mut frame = Vec::new();
        let mut current_row = self.cursor_row;
        let height = lines.len().max(self.height).max(1);
        if height > self.height {
            let bottom = self.height.saturating_sub(1);
            move_screen_row(&mut frame, &mut current_row, bottom)?;
            queue!(frame, MoveToColumn(0))?;
            for _ in self.height.max(1)..height {
                write!(frame, "\r\n")?;
                current_row += 1;
            }
        }
        for i in 0..height {
            let next = lines.get(i);
            let previous = self.previous.get(i);
            if !self.invalidated && next == previous {
                continue;
            }
            move_screen_row(&mut frame, &mut current_row, i)?;
            queue!(frame, MoveToColumn(0))?;
            if let Some((text, style)) = next {
                if std::env::var_os("NO_COLOR").is_none() {
                    match style {
                        TextStyle::Colored(color) => queue!(frame, SetForegroundColor(*color))?,
                        TextStyle::Accent => queue!(
                            frame,
                            SetAttribute(Attribute::Bold),
                            SetForegroundColor(crate::terminal::style::accent())
                        )?,
                        TextStyle::Success => {
                            queue!(frame, SetForegroundColor(crate::terminal::style::success()))?
                        }
                        TextStyle::Border => {
                            queue!(frame, SetForegroundColor(crate::terminal::style::border()))?
                        }
                        TextStyle::Plain => queue!(frame, ResetColor)?,
                        TextStyle::Muted => {
                            queue!(frame, SetForegroundColor(crate::terminal::style::muted()))?
                        }
                        TextStyle::Selected => queue!(
                            frame,
                            SetForegroundColor(crate::terminal::style::selected())
                        )?,
                    }
                }
                write!(frame, "{text}")?;
            }
            // Overwrite first, then erase only the old trailing cells. Clearing
            // the whole line before writing causes visible flashes on Windows.
            if matches!(next, Some((_, TextStyle::Accent))) {
                queue!(frame, SetAttribute(Attribute::Reset))?;
            }
            queue!(frame, ResetColor, Clear(ClearType::UntilNewLine))?;
        }
        move_screen_row(&mut frame, &mut current_row, row)?;
        queue!(frame, MoveToColumn(col as u16))?;
        out.write_all(&frame)?;
        out.flush()?;
        self.cursor_row = row;
        self.height = height;
        self.previous = lines.to_vec();
        self.invalidated = false;
        Ok(())
    }
    pub(crate) fn shortcut_popup(&mut self, context: crate::terminal::shortcuts::Context, next: &mut dyn FnMut() -> io::Result<Option<Event>>) -> io::Result<()> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        // A terminal that does not answer the cursor query still gets the popup.
        let top = self.top.unwrap_or_else(|| {
            position()
                .map_or(0, |(_, row)| row)
                .saturating_sub(self.cursor_row as u16)
        });
        let mut background = crate::terminal::scrollback::Scrollback::snapshot().screen(top, rows);
        for (index, (text, _)) in self.previous.iter().enumerate() {
            if let Some(row) = background.get_mut(top as usize + index) { *row = clipped(text, cols as usize); }
        }
        crate::terminal::shortcuts::show(context, &background, next)?;
        self.invalidated = true;
        Ok(())
    }

    pub(crate) fn suspend(&mut self) -> io::Result<()> {
        let mut out = io::stderr();
        self.erase(&mut out)?;
        out.flush()
    }

    /// Remove the editor from the screen without echoing a submitted line.
    fn clear(&mut self) -> io::Result<()> {
        let mut out = io::stderr();
        self.erase(&mut out)?;
        out.flush()
    }
    /// Erases the frame, leaving the cursor on its first row.
    fn erase(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.pinned {
            if let Some(top) = self.top.take() {
                queue!(out, MoveTo(0, top), Clear(ClearType::FromCursorDown))?;
            }
            queue!(out, ResetColor)?;
            self.previous.clear();
            return Ok(());
        }
        queue!(out, MoveToColumn(0))?;
        if self.cursor_row > 0 {
            queue!(out, MoveUp(self.cursor_row as u16))?;
        }
        queue!(out, Clear(ClearType::FromCursorDown), ResetColor)?;
        Ok(())
    }
    fn finish(&mut self, text: &str, prompt: &str) -> io::Result<()> {
        // Clear menu and borders, retaining just submitted input in scrollback.
        let mut out = io::stderr();
        out.write_all(b"\x1b[?2026h")?;
        self.erase(&mut out)?;
        if let Some(end) = self.transcript_end.take() {
            queue!(out, MoveTo(0, end as u16), Clear(ClearType::FromCursorDown))?;
        }
        let mut submitted = Vec::new();
        crate::terminal::prompt_display::write_submitted(
            &mut submitted,
            prompt,
            text,
            terminal::size()
                .map(|(cols, _)| cols.saturating_sub(1) as usize)
                .unwrap_or(79),
            std::env::var_os("NO_COLOR").is_none(),
            true,
        )?;
        out.write_all(&submitted)?;
        if !text.trim().is_empty() {
            let mut history = crate::terminal::scrollback::Scrollback::snapshot();
            for line in String::from_utf8_lossy(&submitted).split_terminator("\r\n") {
                history.push(line.to_owned(), None);
            }
            history.save();
            crate::terminal::pinned_prompt::remember(text);
        }
        out.write_all(b"\x1b[?2026l")?;
        out.flush()
    }
}
fn move_screen_row(out: &mut impl Write, current: &mut usize, target: usize) -> io::Result<()> {
    if target < *current {
        queue!(out, MoveUp((*current - target) as u16))?;
    } else if target > *current {
        queue!(out, MoveDown((target - *current) as u16))?;
    }
    *current = target;
    Ok(())
}

fn read_editor_event(
    reader: &mut crate::terminal::paste_input::Reader,
) -> io::Result<Option<Event>> {
    loop {
        let Some(event) = reader.next(std::time::Duration::from_millis(100))? else {
            return Ok(None);
        };
        if let Event::Mouse(mouse) = &event {
            if !matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
            ) {
                continue;
            }
        }
        return Ok(Some(event));
    }
}

impl PromptEditor {
    pub fn set_permission_mode(&mut self, mode: String) {
        self.permission_mode = mode;
    }

    fn preserve_cycle_draft(&mut self, buffer: &Buffer) {
        self.pending_draft = buffer.chars.iter().collect();
        self.pending_cursor = Some(buffer.cursor);
    }

    fn resume_buffer(&mut self) -> Buffer {
        let mut buffer = Buffer::default();
        buffer.set(&std::mem::take(&mut self.pending_draft));
        if let Some(cursor) = self.pending_cursor.take() {
            buffer.cursor = cursor.min(buffer.chars.len());
        }
        buffer
    }

    pub fn set_notices(&mut self, notices: std::sync::mpsc::Receiver<String>) {
        self.notices = Some(notices);
    }
    pub fn set_workspace(&mut self, workspace: std::path::PathBuf) {
        if self.workspace.as_ref() != Some(&workspace) {
            self.workspace = Some(workspace);
            self.files = None;
        }
    }
    pub fn set_vim(&mut self, on: bool) {
        self.vim = on;
    }
    pub fn set_context_note(&mut self, note: Option<String>) {
        self.context_note = note;
    }
    pub fn set_shells(&mut self, count: usize) {
        self.shells = count;
    }
    pub fn set_shell_counter(&mut self, counter: Box<dyn Fn() -> usize + Send>) {
        self.shell_counter = Some(counter);
    }
    pub fn set_agent_rows(&mut self, rows: Box<dyn Fn(usize) -> Vec<String> + Send>) {
        self.agent_rows = Some(rows);
    }
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort;
    }
    pub fn set_todos(&mut self, todos: Box<dyn Fn() -> Vec<(String, String)> + Send>) {
        self.todos = Some(todos);
    }
    pub fn set_suggestions(&mut self, suggestions: std::sync::mpsc::Receiver<String>) {
        self.suggestions = Some(suggestions);
    }
    pub fn set_keybindings(&mut self, bindings: crate::keybindings::Keybindings) {
        self.keybindings = bindings;
    }
    pub fn set_draft(&mut self, draft: String) {
        self.pending_draft = draft;
        self.pending_cursor = None;
    }
    pub fn set_commands(&mut self, commands: Vec<(String, String)>) {
        self.commands = commands;
    }
    pub fn set_panel(&mut self, details: Vec<(String, String)>, expanded: bool) {
        self.set_details(details);
        self.details.expanded = expanded;
    }

    pub fn set_details(&mut self, details: Vec<(String, String)>) {
        self.details = DetailsPanel {
            entries: details,
            ..Default::default()
        };
    }
    pub fn set_transcript(&mut self, lines: Vec<crate::terminal::change_view::Line>) {
        self.transcript = lines;
        self.transcript_printed = false;
    }
    pub fn readline(&mut self, prompt: &str) -> io::Result<Option<super::PromptEvent>> {
        self.readline_until(prompt, &std::sync::atomic::AtomicBool::new(false))
    }

    /// Like `readline`, but returns `WouldBlock` once `yielded` is set,
    /// keeping the unfinished text as the next prompt's draft.
    pub fn readline_until(
        &mut self,
        prompt: &str,
        yielded: &std::sync::atomic::AtomicBool,
    ) -> io::Result<Option<super::PromptEvent>> {
        terminal::enable_raw_mode()?;
        let _raw = RawMode;
        // Capture both tool disclosure clicks and drag selection.
        execute!(io::stderr(), EnableBracketedPaste, EnableMouseCapture)?;
        let mut input_reader = crate::terminal::paste_input::Reader::new()?;
        let mut above = crate::terminal::scrollback::Scrollback::take();
        let mut buffer = self.resume_buffer();
        // As in the reference CLI, a stashed draft returns after the prompt
        // that was sent instead.
        let mut restored = false;
        {
            let mut stash = crate::terminal::stash();
            if stash.1 && buffer.chars.is_empty() {
                if let Some(text) = stash.0.take() {
                    buffer.set(&text);
                    restored = true;
                }
                stash.1 = false;
            }
        }
        // Undo history: each change pushes the state before it.
        let mut undo: Vec<(Vec<char>, usize)> = Vec::new();
        let mut last_state = (buffer.chars.clone(), buffer.cursor);
        let mut kill_hint = false;
        let mut clear_esc: Option<std::time::Instant> = None;
        // Read once per prompt; ctrl+t toggles whether they show.
        let todo_list = self.todos.as_ref().map(|todos| todos()).unwrap_or_default();
        let mut keybinding_pending = crate::keybindings::Pending::default();
        let mut screen = Screen::pinned();
        let mut command_overlay = CommandOverlay::default();
        let mut selection = crate::terminal::mouse_selection::Selection::default();
        let mut pinned = crate::terminal::pinned_prompt::PinnedPrompt { text: { let latest = crate::terminal::pinned_prompt::latest(); if latest.is_empty() { self.history.last().cloned().unwrap_or_default() } else { latest } }, ..Default::default() };
        let mut history_view = crate::terminal::transcript_pager::HistoryView::default();
        let mut selected = 0usize;
        let mut history_pos = self.history.len();
        let mut draft = String::new();
        let mut menu_hidden = false;
        // `?` on an empty prompt shows shortcuts in place of the footer.
        let mut shortcuts = false;
        let mut last_shell_count = std::time::Instant::now();
        let mut agents: Vec<String> = Vec::new();
        let mut last_agents = std::time::Instant::now() - std::time::Duration::from_secs(5);
        // ← twice on an empty prompt opens the agents and tasks view.
        let mut left_press: Option<std::time::Instant> = None;
        let mut search: Option<String> = None;
        let mut search_pos = self.history.len();
        // A second Esc on an empty prompt within a second opens /rewind.
        let mut last_esc: Option<std::time::Instant> = None;
        // With /vim on, Some while in normal mode; each prompt starts inserting.
        let mut vim_normal: Option<crate::terminal::vim::Normal> = None;
        // A suggestion belongs to the turn before this prompt; drop stale ones.
        self.suggestion = None;
        // Files may have changed since the last prompt; list them again on `@`.
        self.files = None;
        if let Some(suggestions) = &self.suggestions {
            while suggestions.try_recv().is_ok() {}
        }
        loop {
            if yielded.load(std::sync::atomic::Ordering::Acquire) {
                self.pending_draft = buffer.text();
                self.pending_cursor = Some(buffer.cursor);
                history_view.close()?;
                screen.clear()?;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "Prompt yielded"));
            }
            if let Some(notices) = &self.notices {
                while let Ok(notice) = notices.try_recv() {
                    history_view.close()?;
                    screen.finish("", "")?;
                    let safe: String = notice
                        .chars()
                        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                        .collect();
                    write!(io::stderr(), "{}\r\n", safe.replace('\n', "\r\n"))?;
                    io::stderr().flush()?;
                    screen = Screen::pinned();
                }
            }
            if let Some(suggestions) = &self.suggestions {
                while let Ok(suggestion) = suggestions.try_recv() {
                    self.suggestion = Some(suggestion);
                }
            }
            if self.shells > 0 && last_shell_count.elapsed() >= std::time::Duration::from_secs(1) {
                last_shell_count = std::time::Instant::now();
                if let Some(counter) = &self.shell_counter {
                    self.shells = counter();
                }
            }
            if buffer.chars != last_state.0 {
                undo.push(std::mem::replace(&mut last_state, (buffer.chars.clone(), buffer.cursor)));
                if undo.len() > 200 {
                    undo.remove(0);
                }
            }
            last_state.1 = buffer.cursor;
            let (cols, rows) = terminal::size()?;
            let width = cols.saturating_sub(1).max(4) as usize;
            if last_agents.elapsed() >= std::time::Duration::from_secs(1) {
                last_agents = std::time::Instant::now();
                if let Some(rows) = &self.agent_rows {
                    agents = rows(width);
                }
            }
            let text = buffer.text();
            let ghost = self.suggestion.clone().filter(|_| buffer.chars.is_empty());
            let mention_query = mention(&text).filter(|_| buffer.cursor == buffer.chars.len()).map(str::to_owned);
            let mentioned: Vec<(String, String)> = match (&mention_query, &self.workspace) {
                (Some(query), Some(root)) if !menu_hidden => {
                    let root = root.clone();
                    let files = self.files.get_or_insert_with(|| project_files(&root));
                    mention_matches(files, query)
                }
                _ => Vec::new(),
            };
            let entries: &[(String, String)] = if mention_query.is_some() { &mentioned } else { &self.commands };
            let items = if menu_hidden {
                vec![]
            } else if mention_query.is_some() {
                (0..entries.len()).collect()
            } else {
                matches(entries, &text)
            };
            selected = selected.min(items.len().saturating_sub(1));
            // Wrap by display cells, never by UTF-8 bytes. Keep cursor in viewport.
            let prompt = clipped(prompt, width.saturating_sub(2));
            let prefix: String = buffer.chars.iter().take(buffer.paste_end).collect();
            let label = buffer.pasted.then(|| crate::terminal::paste_input::paste_label(&prefix)).flatten().map(|label| format!("{}{}", label, buffer.chars.iter().skip(buffer.paste_end).collect::<String>()));
            let shown = label.as_ref().map(|label| label.chars().collect::<Vec<_>>());
            let chars = shown.as_ref().unwrap_or(&buffer.chars);
            let (input_lines, positions) = crate::terminal::prompt_display::layout(&prompt, chars, width.saturating_sub(1));
            let (cursor_row, cursor_col) = positions[if shown.is_some() { chars.len() } else { buffer.cursor.min(chars.len()) }];
            let max_input = (rows as usize / 3).max(1);
            // As in the reference CLI one footer line suffices; a tip helps only
            // before the first prompt.
            let show_tip = items.is_empty() && search.is_none() && rows >= 12 && self.history.is_empty();
            let first = cursor_row.saturating_sub(max_input - 1);
            let input_height = input_lines.len().saturating_sub(first).min(max_input);
            let panel_budget = (rows as usize)
                .saturating_sub(
                    input_height + 5 + usize::from(show_tip) + if items.is_empty() { 0 } else { 4 },
                )
                .min(12);
            let panel = self.details.render(width, panel_budget);
            let panel_height = panel.lines.len();
            let mut lines = panel.lines;
            let footer_rows = usize::from(show_tip) + 1 + usize::from(!self.details.entries.is_empty());
            // Five one-line entries at a time above the input box, as in the
            // reference CLI.
            let available = (rows as usize)
                .saturating_sub(panel_height + input_height + 2 + footer_rows + 1)
                .min(MENU_ROWS);
            let heights = vec![1; items.len()];
            let (start, end) = menu_page(&heights, selected, available);
            let longest = items[start..end].iter().map(|index| entries[*index].0.width()).max().unwrap_or(0);
            let name_width = (longest + 2).clamp(12, (width / 3).max(12)).min(width / 2);
            for (position, index) in items.iter().enumerate().take(end).skip(start) {
                let (name, desc) = &entries[*index];
                let prefix = if position == selected { "  \u{276f}" } else { "   " };
                let label = clipped(name, name_width);
                let style = if position == selected {
                    TextStyle::Selected
                } else {
                    TextStyle::Muted
                };
                let room = width.saturating_sub(name_width + 6);
                let first = desc.lines().next().unwrap_or_default();
                let description = if first.width() > room || desc.lines().nth(1).is_some() {
                    format!("{}\u{2026}", clipped(first, room.saturating_sub(1)))
                } else {
                    first.to_owned()
                };
                let pad = name_width.saturating_sub(label.width());
                lines.push((clipped(&format!("{prefix} {label}{}  {description}", " ".repeat(pad)), width), style));
            }
            if items.is_empty() && mention_query.is_none() && !menu_hidden && text.len() > 1 && text.starts_with('/') && !text.contains(char::is_whitespace) {
                lines.push((clipped(&format!("  No commands match \"{text}\""), width), TextStyle::Muted));
            }
            let command_menu: Vec<DisplayLine> = lines.drain(panel_height..).collect();
            if crate::terminal::TODOS_VISIBLE.load(std::sync::atomic::Ordering::Relaxed) {
                for (row, done, current) in crate::terminal::todo_rows(&todo_list, width) {
                    lines.push((row, if done { TextStyle::Muted } else if current { TextStyle::Selected } else { TextStyle::Plain }));
                }
            }
            let menu_height = lines.len() - panel_height;
            lines.push(("\u{2500}".repeat(width), TextStyle::Border));
            match &ghost {
                // The suggestion is placeholder text: dim, with the cursor before it.
                Some(suggestion) => lines.push((
                    clipped(&format!("{prompt}{}", suggestion.replace('\n', " ")), width),
                    TextStyle::Muted,
                )),
                None => lines.extend(
                    input_lines
                        .iter()
                        .skip(first)
                        .take(max_input)
                        .map(|s| (s.clone(), TextStyle::Plain)),
                ),
            }
            lines.push(("\u{2500}".repeat(width), TextStyle::Border));
            let footer = if let Some(query) = search.as_ref() {
                format!("Search history: {query}  (Ctrl+R next · Enter accept · Esc close)")
            } else {
                // As in the reference CLI: the mode label, or "? for shortcuts" in ask mode.
                let mut parts = vec![if text.starts_with('!') {
                    "! shell mode: runs in your shell, output joins the conversation".to_owned()
                } else if text.starts_with("# ") && !text.contains('\n') {
                    "# memory: saves this line to the project's AGENTS.md or CLAUDE.md".to_owned()
                } else {
                    crate::terminal::style::mode_indicator(&self.permission_mode)
                        .map_or_else(|| "? for shortcuts".to_owned(), |(label, _)| label)
                }];
                if ghost.is_some() {
                    parts.push("tab to accept suggestion".into());
                }
                if self.details.entries.is_empty() && !self.transcript.is_empty() && !self.transcript_printed {
                    parts.push("ctrl+o transcript".into());
                }
                if let Some(note) = &self.context_note {
                    parts.push(note.clone());
                }
                if !agents.is_empty() && self.shells == 0 {
                    parts.insert(1, "\u{2193} to manage".into());
                }
                parts.push("\u{2190} for agents".into());
                if left_press.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                    parts = vec!["Press \u{2190} again to open agents".into()];
                }
                if self.shells > 0 {
                    // Right after the mode label, as in the reference CLI.
                    parts.insert(1, format!("{} shell{} \u{b7} \u{2193} to manage", self.shells, if self.shells == 1 { "" } else { "s" }));
                }
                if kill_hint {
                    parts.push("Ctrl+Y to paste deleted text".into());
                }
                if crate::terminal::stash().0.is_some() {
                    parts.push("\u{203a} stashed".into());
                }
                if restored {
                    parts.push("Draft restored".into());
                }
                if buffer.chars.contains(&'\n') {
                    parts.push(format!("ctrl+g to edit in {}", external_editor()));
                }
                if clear_esc.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                    parts = vec!["Esc again to clear".into()];
                }
                if last_esc.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                    parts = vec!["esc again to rewind".into()];
                }
                // As in the reference CLI: `-- INSERT -- ` leads the footer in
                // insert mode; normal mode has no label.
                let left = if self.vim && vim_normal.is_none() {
                    format!("-- INSERT -- {}", parts.join(" \u{b7} "))
                } else {
                    parts.join(" \u{b7} ")
                };
                match self.effort.as_ref().filter(|label| left.width() + 2 + label.width() <= width) {
                    Some(label) => format!("{left}{}{label}", " ".repeat(width - left.width() - label.width())),
                    None => left,
                }
            };
            let footer = if search.is_none() && !text.starts_with('!') && !text.starts_with("# ") {
                format!("{} {}", crate::terminal::style::status_mode(&self.permission_mode), self.effort.as_deref().unwrap_or_default())
            } else {
                footer
            };
            let footer_style = match crate::terminal::style::mode_indicator(&self.permission_mode) {
                Some((_, color)) if search.is_none() => TextStyle::Colored(color),
                _ => TextStyle::Muted,
            };
            if shortcuts && items.is_empty() && search.is_none() {
                lines.extend(crate::terminal::style::shortcut_rows(width).into_iter().map(|row| (clipped(&row, width), TextStyle::Muted)));
            } else {
                lines.push((clipped(&footer, width), footer_style));
            }
            // Running background agents, as in the reference CLI.
            lines.extend(agents.iter().map(|row| (clipped(row, width), TextStyle::Muted)));
            if !self.details.entries.is_empty() {
                lines.push((
                    clipped("Ctrl+O details \u{00b7} PgUp/PgDn scroll", width),
                    TextStyle::Muted,
                ));
            }
            if show_tip {
                let tips = [
                    "Tip: Type / to browse available commands.",
                    "Tip: Use /color and /rename to tell multiple sessions apart.",
                    "Tip: Use /btw to ask a side question while the model works.",
                ];
                lines.push((
                    clipped(tips[self.history.len() % tips.len()], width),
                    TextStyle::Muted,
                ));
            }
            let input_row = panel_height + menu_height + cursor_row - first + 1;
            if history_view.active() {
                history_view.refresh(&[], &above.all(), width);
                let color = std::env::var_os("NO_COLOR").is_none();
                let composer: Vec<String> = lines.iter().map(|(text, style)| {
                    let foreground = match style {
                        TextStyle::Colored(color) => Some(*color),
                        TextStyle::Accent => Some(crate::terminal::style::accent()),
                        TextStyle::Success => Some(crate::terminal::style::success()),
                        TextStyle::Border => Some(crate::terminal::style::border()),
                        TextStyle::Plain => None,
                        TextStyle::Muted => Some(crate::terminal::style::muted()),
                        TextStyle::Selected => Some(crate::terminal::style::selected()),
                    };
                    match foreground.filter(|_| color) {
                        Some(foreground) => format!("{}{text}{}", SetForegroundColor(foreground), ResetColor),
                        None => text.clone(),
                    }
                }).collect();
                if !selection.retained() { history_view.draw(&composer, input_row, cursor_col, (cols, rows))?; }
            } else if !selection.retained() {
                if screen.height == 0 && above.needs_separator() {
                    write!(io::stderr(), "\r\n")?;
                    io::stderr().flush()?;
                    above.push(String::new(), None);
                }
                screen.draw(&lines, input_row, cursor_col)?;
            }
            if !selection.retained() {
                let composer: Vec<String> = lines.iter().map(|(text, _)| text.clone()).collect();
                let background = if history_view.active() {
                    history_view.screen_rows(rows as usize, &composer)
                } else {
                    let origin = screen.top.unwrap_or(0);
                    let mut background = above.screen(origin, rows);
                    for (index, text) in composer.iter().enumerate() {
                        if let Some(row) = background.get_mut(origin as usize + index) { *row = text.clone(); }
                    }
                    background
                };
                let bottom = if history_view.active() { (rows as usize).saturating_sub(lines.len()) } else { screen.top.unwrap_or(0) as usize };
                command_overlay.draw_to(&mut io::stderr().lock(), &command_menu, bottom, &background)?;
                pinned.draw(width, rows as usize, &background)?;
            }
            above.clone().save();
            let Some(event) = read_editor_event(&mut input_reader)? else {
                continue;
            };
            if matches!(&event, Event::Key(key) if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab)) {
                command_overlay.draw_to(&mut io::stderr().lock(), &[], 0, &[])?;
            }
            if crate::terminal::shortcuts::requested_in_input(&event, buffer.chars.is_empty()) {
                selection.clear()?;
                let composer: Vec<String> = lines.iter().map(|(text, _)| text.clone()).collect();
                let background = if history_view.active() {
                    history_view.screen_rows(rows as usize, &composer)
                } else {
                    let origin = screen.top.unwrap_or(0);
                    let mut background = above.screen(origin, rows);
                    for (index, text) in composer.iter().enumerate() {
                        if let Some(row) = background.get_mut(origin as usize + index) { *row = text.clone(); }
                    }
                    background
                };
                crate::terminal::shortcuts::show(crate::terminal::shortcuts::Context::Input, &background, &mut || read_editor_event(&mut input_reader))?;
                screen.invalidated = true;
                continue;
            }
            match event {
                Event::Resize(_, _) => {
                    // The terminal reflowed the prompt; the next draw places it
                    // on the last rows again.
                    screen.reflow();
                }
                Event::Mouse(mouse) => {
                    if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
                        selection.clear()?;
                        pinned.expanded = false;
                        screen.invalidated = true;
                        history_view.refresh(&[], &above.all(), width);
                        history_view.wheel(mouse.kind == MouseEventKind::ScrollUp, (rows as usize).saturating_sub(lines.len()))?;
                        continue;
                    }
                    if history_view.active() {
                        let composer = lines.iter().map(|(text, _)| text.clone()).collect::<Vec<_>>();
                        let snapshot = history_view.screen_rows(rows as usize, &composer);
                        if let Some(click) = selection.event(mouse, snapshot)? {
                            if click.kind == MouseEventKind::Down(MouseButton::Left) && pinned.click(click.row) { continue; }
                            if click.kind == MouseEventKind::Down(MouseButton::Left) {
                                if let Some(index) = history_view.row_index(click.row as usize, (rows as usize).saturating_sub(lines.len())) { above.toggle_history(index); }
                            }
                        }
                        continue;
                    }
                    let (_, cursor_y) = position()?;
                    let origin = cursor_y.saturating_sub(screen.cursor_row as u16);
                    let mut snapshot = above.screen(origin, rows);
                    for (index, (text, _)) in screen.previous.iter().enumerate() {
                        if let Some(row) = snapshot.get_mut(origin as usize + index) { *row = text.clone(); }
                    }
                    let Some(mouse) = selection.event(mouse, snapshot)? else { continue; };
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) && pinned.click(mouse.row) { screen.invalidated = true; continue; }
                    let toggled = (mouse.kind == MouseEventKind::Down(MouseButton::Left))
                        .then(|| above.toggle(origin, mouse.row))
                        .flatten();
                    if let Some((start, lines)) = toggled {
                        // Redraw from the clicked tool result down, then the prompt.
                        let mut out = format!("\x1b[?25l\x1b[{};1H\x1b[J", start + 1);
                        for (line, block) in lines {
                            out.push_str(&line);
                            out.push_str("\r\n");
                            above.push(line, block);
                        }
                        let mut stderr = io::stderr().lock();
                        stderr.write_all(out.as_bytes())?;
                        stderr.flush()?;
                        screen = Screen::pinned();
                    } else if let Some(row) = mouse.row.checked_sub(origin) {
                        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                            && panel.toggle.is_some_and(|hit| {
                                hit.contains(row as usize, mouse.column as usize)
                            })
                        {
                            self.details.toggle();
                        } else if (row as usize) < panel_height {
                            match mouse.kind {
                                MouseEventKind::ScrollDown => {
                                    self.details.scroll_by(1, panel.viewport)
                                }
                                MouseEventKind::ScrollUp => {
                                    self.details.scroll_by(-1, panel.viewport)
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Event::Paste(value) => {
                    selection.clear()?;
                    buffer.pasted = true;
                    history_view.close()?;
                    screen.invalidated = true;
                    // A dragged or pasted image file shows as `[Image #N]`, as in the reference CLI.
                    match crate::images::pasted_path(&value) {
                        Some(path) => buffer.insert(&format!("{} ", crate::images::attach(path))),
                        None => buffer.insert(&value),
                    }
                    buffer.paste_end = buffer.chars.len();
                    selected = 0;
                    menu_hidden = false;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    selection.clear()?;
                    if buffer.cursor < buffer.paste_end || matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Home) { buffer.pasted = false; }
                    let key = match self.keybindings.resolve(key, &mut keybinding_pending) {
                        crate::keybindings::Resolution::Key(key) => key,
                        crate::keybindings::Resolution::Consumed => continue,
                    };
                    let panel_scroll = self.details.expanded
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown);
                    if !panel_scroll && crate::terminal::transcript_pager::HistoryView::is_navigation(key) {
                        history_view.refresh(&[], &above.all(), width);
                        history_view.navigate(key, (rows as usize).saturating_sub(lines.len()))?;
                        screen.invalidated = true;
                        continue;
                    }
                    if history_view.active() {
                        history_view.close()?;
                        screen.invalidated = true;
                        if key.code == KeyCode::Esc {
                            continue;
                        }
                    }
                    if crate::keybindings::is_cycle_permission(key) {
                        self.preserve_cycle_draft(&buffer);
                        screen.suspend()?;
                        return Ok(Some(super::PromptEvent::CyclePermission));
                    }
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    if control && key.code == KeyCode::Char('l') {
                        screen.invalidated = true;
                        continue;
                    }
                    restored = false;
                    let killing = control && matches!(key.code, KeyCode::Char('u' | 'k'));
                    if killing {
                        let cut: String = if key.code == KeyCode::Char('u') {
                            buffer.chars[..buffer.cursor].iter().collect()
                        } else {
                            buffer.chars[buffer.cursor..].iter().collect()
                        };
                        if !cut.is_empty() {
                            *crate::terminal::killed() = cut;
                        }
                    }
                    kill_hint = killing && !crate::terminal::killed().is_empty();
                    // Shortcuts from the reference CLI's `?` panel.
                    if (control && matches!(key.code, KeyCode::Char('_' | '7' | '/' | '-'))) || key.code == KeyCode::Char('\x1f') {
                        if let Some((chars, cursor)) = undo.pop() {
                            buffer.chars = chars;
                            buffer.cursor = cursor.min(buffer.chars.len());
                            last_state = (buffer.chars.clone(), buffer.cursor);
                        }
                        continue;
                    }
                    if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::ALT) {
                        // alt+p opens the model picker, as in the reference CLI.
                        let model = "/model".to_owned();
                        screen.finish(&model, &prompt)?;
                        return Ok(Some(super::PromptEvent::Line(model)));
                    }
                    if control && key.code == KeyCode::Char('t') {
                        // As in the reference CLI, ctrl+t shows or hides the tasks.
                        crate::terminal::TODOS_VISIBLE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    if control && key.code == KeyCode::Char('y') {
                        let killed = crate::terminal::killed().clone();
                        buffer.insert(&killed);
                        continue;
                    }
                    if control && key.code == KeyCode::Char('s') {
                        let mut stash = crate::terminal::stash();
                        if buffer.chars.is_empty() {
                            if let Some(text) = stash.0.take() {
                                buffer.set(&text);
                            }
                        } else {
                            *stash = (Some(buffer.text()), false);
                            buffer.set("");
                        }
                        continue;
                    }
                    if control && key.code == KeyCode::Char('g') {
                        screen.suspend()?;
                        let edited = edit_externally(&buffer.text())?;
                        buffer.set(&edited);
                        screen = Screen::pinned();
                        continue;
                    }
                    if control && key.code == KeyCode::Char('z') && cfg!(unix) {
                        screen.suspend()?;
                        suspend_process()?;
                        screen = Screen::pinned();
                        continue;
                    }
                    if key.code == KeyCode::Esc && items.is_empty() && !buffer.chars.is_empty() && !self.vim {
                        if clear_esc.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                            buffer.set("");
                            clear_esc = None;
                        } else {
                            clear_esc = Some(std::time::Instant::now());
                        }
                        continue;
                    }
                    if key.code == KeyCode::Enter
                        && key.modifiers.is_empty()
                        && items.is_empty()
                        && buffer.cursor == buffer.chars.len()
                        && buffer.chars.last() == Some(&'\\')
                    {
                        // A trailing backslash continues on a new line.
                        buffer.chars.pop();
                        buffer.cursor = buffer.chars.len();
                        buffer.insert("\n");
                        continue;
                    }
                    if control && key.code == KeyCode::Char('c') {
                        screen.finish("", &prompt)?;
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Input cancelled",
                        ));
                    }
                    if control && key.code == KeyCode::Char('d') && buffer.chars.is_empty() {
                        screen.finish("", &prompt)?;
                        return Ok(None);
                    }
                    if self.vim && !control {
                        if vim_normal.is_none() && key.code == KeyCode::Esc && items.is_empty() && !buffer.chars.is_empty() {
                            vim_normal = Some(crate::terminal::vim::Normal::default());
                            crate::terminal::vim::leave_insert(&mut buffer.cursor);
                            continue;
                        }
                        if let (Some(normal), KeyCode::Char(ch)) = (vim_normal.as_mut(), key.code) {
                            use crate::terminal::vim::Outcome;
                            match normal.key(ch, &mut buffer.chars, &mut buffer.cursor) {
                                Outcome::Insert => vim_normal = None,
                                Outcome::HistoryPrevious if history_pos > 0 => {
                                    if history_pos == self.history.len() {
                                        draft = buffer.text();
                                    }
                                    history_pos -= 1;
                                    buffer.set(&self.history[history_pos]);
                                }
                                Outcome::HistoryNext if history_pos < self.history.len() => {
                                    history_pos += 1;
                                    buffer.set(if history_pos == self.history.len() { &draft } else { &self.history[history_pos] });
                                }
                                _ => {}
                            }
                            continue;
                        }
                    }
                    if control && key.code == KeyCode::Char('v') {
                        // Ctrl+V with an image on the clipboard attaches it as
                        // [Image #N]; text pastes arrive as bracketed paste instead.
                        if let Ok(Some(path)) = crate::images::paste_clipboard() {
                            buffer.insert(&format!("{} ", crate::images::attach(path)));
                        }
                        continue;
                    }
                    if key.code == KeyCode::Char('?') && !control && buffer.chars.is_empty() {
                        // As in the reference CLI, `?` toggles the shortcuts panel
                        // below the input; any other key closes it.
                        shortcuts = !shortcuts;
                        continue;
                    }
                    shortcuts = false;
                    if control && key.code == KeyCode::Char('o') {
                        if self.details.entries.is_empty() && !self.transcript.is_empty() {
                            // As in the reference CLI: a full-screen pager over the
                            // whole conversation; `[` prints it into scrollback.
                            let outcome = crate::terminal::transcript_pager::run(&self.transcript, &mut || read_editor_event(&mut input_reader))?;
                            screen.invalidated = true;
                            self.transcript_printed = true;
                            if matches!(outcome, crate::terminal::transcript_pager::Outcome::Closed) {
                                continue;
                            }
                            screen.finish("", "")?;
                            let width = terminal::size()
                                .map(|(cols, _)| cols.saturating_sub(1) as usize)
                                .unwrap_or(79);
                            let color = std::env::var_os("NO_COLOR").is_none();
                            let mut text = String::from("\x1b[?2026h");
                            for line in &self.transcript {
                                text.push_str(&crate::terminal::change_view::render(line, width, color));
                                text.push_str("\r\n");
                            }
                            text.push_str("\x1b[?2026l");
                            let mut out = io::stderr();
                            out.write_all(text.as_bytes())?;
                            out.flush()?;
                            screen = Screen::pinned();
                            self.transcript_printed = true;
                        } else {
                            self.details.toggle();
                        }
                        continue;
                    }
                    if self.details.expanded
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                    {
                        let delta = panel.viewport.max(1) as isize;
                        self.details.scroll_by(
                            if key.code == KeyCode::PageUp {
                                -delta
                            } else {
                                delta
                            },
                            panel.viewport,
                        );
                        continue;
                    }
                    if control && key.code == KeyCode::Char('r') {
                        let query = search.get_or_insert_with(String::new);
                        if let Some(i) = (0..search_pos)
                            .rev()
                            .find(|i| self.history[*i].contains(query.as_str()))
                        {
                            search_pos = i;
                            buffer.set(&self.history[i]);
                        }
                        continue;
                    }
                    if let Some(query) = search.as_mut() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter => {
                                search = None;
                                continue;
                            }
                            KeyCode::Backspace => {
                                query.pop();
                            }
                            KeyCode::Char(ch) if !control => query.push(ch),
                            _ => continue,
                        }
                        search_pos = self.history.len();
                        if let Some(i) = (0..search_pos)
                            .rev()
                            .find(|i| self.history[*i].contains(query.as_str()))
                        {
                            search_pos = i;
                            buffer.set(&self.history[i]);
                        }
                        continue;
                    }
                    if let Some(suggestion) = ghost.as_ref().filter(|_| {
                        matches!(key.code, KeyCode::Tab | KeyCode::Right) && !control
                    }) {
                        buffer.insert(suggestion);
                        self.suggestion = None;
                        continue;
                    }
                    if matches!(key.code, KeyCode::Char(_) | KeyCode::Enter) {
                        self.suggestion = None;
                    }
                    match key.code {
                        KeyCode::Enter
                            if key
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                        {
                            buffer.insert("\n")
                        }
                        KeyCode::Tab | KeyCode::Enter
                            if !items.is_empty()
                                && (key.code == KeyCode::Tab
                                    || text != entries[items[selected]].0) =>
                        {
                            let choice = &entries[items[selected]].0;
                            if let Some(query) = &mention_query {
                                // Replace only the `@` word; keep the rest of the prompt.
                                let keep = text.len() - query.len() - 1;
                                buffer.set(&format!("{}{choice} ", &text[..keep]));
                            } else {
                                buffer.set(&format!("{choice} "));
                            }
                            menu_hidden = true;
                        }
                        KeyCode::Enter => {
                            // Restore the main screen before clearing its composer.
                            // Otherwise dropping HistoryView restores a stale input frame.
                            history_view.close()?;
                            screen.finish(&text, &prompt)?;
                            if !text.trim().is_empty() && self.history.last() != Some(&text) {
                                self.history.push(text.clone());
                            }
                            {
                                let mut stash = crate::terminal::stash();
                                stash.1 = stash.0.is_some();
                            }
                            return Ok(Some(super::PromptEvent::Line(text)));
                        }
                        KeyCode::Esc if items.is_empty() && buffer.chars.is_empty() => {
                            if last_esc.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                                // As in the reference CLI, Esc Esc goes back to an earlier turn.
                                let rewind = "/rewind".to_owned();
                                screen.finish(&rewind, &prompt)?;
                                return Ok(Some(super::PromptEvent::Line(rewind)));
                            }
                            last_esc = Some(std::time::Instant::now());
                        }
                        KeyCode::Esc => menu_hidden = true,
                        KeyCode::Up if !items.is_empty() => {
                            selected = if selected == 0 {
                                items.len() - 1
                            } else {
                                selected - 1
                            }
                        }
                        KeyCode::Down if !items.is_empty() => {
                            selected = (selected + 1) % items.len()
                        }
                        KeyCode::Up => {
                            if history_pos == self.history.len() {
                                draft = buffer.text();
                            }
                            if history_pos > 0 {
                                history_pos -= 1;
                                buffer.set(&self.history[history_pos]);
                            }
                        }
                        // As in the reference CLI, ↓ past the newest prompt manages
                        // the running background shells.
                        KeyCode::Left if buffer.chars.is_empty() && items.is_empty() => {
                            if left_press.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                                let tasks = "/tasks".to_owned();
                                screen.finish(&tasks, &prompt)?;
                                return Ok(Some(super::PromptEvent::Line(tasks)));
                            }
                            left_press = Some(std::time::Instant::now());
                        }
                        KeyCode::Down if (self.shells > 0 || !agents.is_empty()) && buffer.chars.is_empty() && history_pos == self.history.len() => {
                            let tasks = "/tasks".to_owned();
                            screen.finish(&tasks, &prompt)?;
                            return Ok(Some(super::PromptEvent::Line(tasks)));
                        }
                        KeyCode::Down => {
                            if history_pos < self.history.len() {
                                history_pos += 1;
                                buffer.set(if history_pos == self.history.len() {
                                    &draft
                                } else {
                                    &self.history[history_pos]
                                });
                            }
                        }
                        KeyCode::Left => buffer.cursor = buffer.cursor.saturating_sub(1),
                        KeyCode::Right => {
                            buffer.cursor = (buffer.cursor + 1).min(buffer.chars.len())
                        }
                        KeyCode::Home => buffer.cursor = 0,
                        KeyCode::End => buffer.cursor = buffer.chars.len(),
                        KeyCode::Backspace => {
                            if buffer.cursor > 0 {
                                buffer.cursor -= 1;
                                buffer.chars.remove(buffer.cursor);
                            }
                            selected = 0;
                            menu_hidden = false;
                        }
                        KeyCode::Delete => {
                            if buffer.cursor < buffer.chars.len() {
                                buffer.chars.remove(buffer.cursor);
                            }
                        }
                        KeyCode::Char('a') if control => buffer.cursor = 0,
                        KeyCode::Char('e') if control => buffer.cursor = buffer.chars.len(),
                        KeyCode::Char('u') if control => {
                            buffer.chars.drain(..buffer.cursor);
                            buffer.cursor = 0;
                        }
                        KeyCode::Char('k') if control => buffer.chars.truncate(buffer.cursor),
                        KeyCode::Char(ch) if !control => {
                            buffer.insert(&ch.to_string());
                            selected = 0;
                            menu_hidden = false;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}
#[cfg(test)]
mod mention_tests {
    use super::*;

    #[test]
    fn mentions_complete_project_files_and_skip_build_output() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["src/main.rs", "src/lib.rs", "README.md", "node_modules/x/index.js", ".git/config", "target/debug/app"] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let files = project_files(dir.path());
        assert_eq!(files, ["README.md", "src/", "src/lib.rs", "src/main.rs"]);
        assert_eq!(mention("explain @src/ma"), Some("src/ma"));
        assert_eq!(mention("no mention here"), None);
        let found = mention_matches(&files, "ma");
        assert_eq!(found[0].0, "@src/main.rs");
        // The directory comes first, then what it holds.
        let names: Vec<String> = mention_matches(&files, "src").into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["@src/", "@src/lib.rs", "@src/main.rs"]);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn slash_menu_overlay_restores_background_without_scrolling() {
        let mut overlay = CommandOverlay::default();
        let background: Vec<String> = (0..24).map(|i| format!("message {i}")).collect();
        let menu = vec![("/help".into(), TextStyle::Selected), ("/resume".into(), TextStyle::Muted)];
        let mut output = Vec::new();
        overlay.draw_to(&mut output, &menu, 20, &background).unwrap();
        assert!(!output.contains(&b'\n'));
        assert_eq!(overlay.top, 18);
        output.clear();
        overlay.draw_to(&mut output, &menu, 20, &background).unwrap();
        assert!(output.is_empty());
        overlay.draw_to(&mut output, &[], 0, &[]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("message 18"));
        assert!(output.contains("message 19"));
        assert!(!output.contains('\n'));
    }

    #[test]
    fn permission_cycle_preserves_multiline_unicode_draft_cursor_and_history() {
        let mut editor = PromptEditor::default();
        editor.history.push("previous".into());
        let mut buffer = Buffer::default();
        buffer.set("hello \u{4e2d}\u{6587}\nworld");
        buffer.cursor = 3;
        editor.preserve_cycle_draft(&buffer);
        editor.set_permission_mode("plan".into());
        let restored = editor.resume_buffer();
        assert_eq!(restored.chars, buffer.chars);
        assert_eq!(restored.cursor, 3);
        assert_eq!(editor.history, vec!["previous"]);
        assert!(editor.pending_draft.is_empty());
    }

    #[test]
    fn streaming_detail_appends_preserve_other_allocations_and_defer_wrapping() {
        let mut panel = super::DetailsPanel::default();
        panel.update_entries(vec![
            ("Read".into(), "tool output".repeat(4096)),
            ("Thinking".into(), String::new()),
        ]);
        let tool_pointer = panel.entries[0].1.as_ptr();
        let mut expected = String::new();
        for _ in 0..3000 {
            panel.append_body(1, "中文🦀\n");
            expected.push_str("中文🦀\n");
            assert_eq!(panel.entries[0].1.as_ptr(), tool_pointer);
            assert!(panel.wrapped.is_empty());
        }
        assert_eq!(panel.entries[1].1, expected);
        panel.toggle();
        panel.render(80, 12);
        assert_eq!(panel.cached_width, 80);
        let cached_lines = panel.wrapped.len();
        panel.append_body(1, "");
        assert_eq!(panel.cached_width, 80);
        assert_eq!(panel.wrapped.len(), cached_lines);
        panel.append_body(1, "latest");
        panel.render(80, 12);
        assert!(panel.wrapped.iter().any(|line| line == "latest"));
        assert_eq!(panel.entries[0].1.as_ptr(), tool_pointer);
    }

    use super::*;
    fn frame_lines() -> Vec<DisplayLine> {
        vec![
            ("????".into(), TextStyle::Muted),
            ("> /".into(), TextStyle::Plain),
            ("/help".into(), TextStyle::Selected),
            ("/model".into(), TextStyle::Muted),
        ]
    }

    #[test]
    fn unchanged_editor_frame_never_clears_or_scrolls() {
        let mut screen = Screen::default();
        let lines = frame_lines();
        screen.draw_to(&mut Vec::new(), &lines, 1, 3).unwrap();
        let mut output = Vec::new();
        screen.draw_to(&mut output, &lines, 1, 2).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b[3G");
    }

    #[test]
    fn shrinking_editor_clears_removed_menu_once_without_newlines() {
        let mut screen = Screen::default();
        let lines = frame_lines();
        screen.draw_to(&mut Vec::new(), &lines, 1, 3).unwrap();
        let mut output = Vec::new();
        screen.draw_to(&mut output, &lines[..2], 1, 3).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains('\n'));
        assert_eq!(output.matches("\x1b[K").count(), 2);
        let mut output = Vec::new();
        screen.draw_to(&mut output, &lines[..2], 1, 3).unwrap();
        assert!(!String::from_utf8(output).unwrap().contains("\x1b[K"));
    }

    #[test]
    fn selection_repaints_changed_rows_and_growth_allocates_only_extra_rows() {
        let mut screen = Screen::default();
        let mut lines = frame_lines();
        screen.draw_to(&mut Vec::new(), &lines[..3], 1, 3).unwrap();
        lines[2].1 = TextStyle::Muted;
        lines[3].1 = TextStyle::Selected;
        let mut output = Vec::new();
        screen.draw_to(&mut output, &lines, 1, 3).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches('\n').count(), 1);
        assert!(output.contains("/help"));
        assert!(output.contains("/model"));
        assert!(!output.contains("> /"));
    }

    #[test]
    fn resize_invalidates_rows_including_old_menu_tail() {
        let mut screen = Screen::default();
        let lines = frame_lines();
        screen.draw_to(&mut Vec::new(), &lines, 1, 3).unwrap();
        screen.invalidated = true;
        let mut output = Vec::new();
        screen.draw_to(&mut output, &lines[..2], 1, 3).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("\x1b[K").count(), 4);
        assert!(!output.contains('\n'));
    }

    #[test]
    fn submission_anchor_excludes_bottom_padding_and_tracks_scrolling() {
        let mut screen = Screen::pinned();
        let lines = frame_lines();
        screen.draw_pinned_to(&mut Vec::new(), &lines, 1, 3, 30, 2).unwrap();
        assert_eq!(screen.transcript_end, Some(2));
        screen.draw_pinned_to(&mut Vec::new(), &lines, 1, 4, 30, 27).unwrap();
        assert_eq!(screen.transcript_end, Some(2));
        let mut screen = Screen::pinned();
        screen.draw_pinned_to(&mut Vec::new(), &lines, 1, 3, 10, 9).unwrap();
        assert_eq!(screen.transcript_end, Some(6));
    }

    #[test]
    fn composer_stays_at_bottom_without_moving_short_transcript() {
        let mut screen = Screen::pinned();
        let lines = frame_lines();
        let mut output = Vec::new();
        // Keep the short transcript at the top and the composer at the bottom.
        screen.draw_pinned_to(&mut output, &lines, 1, 3, 10, 2).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("\x1b[4T"), "transcript must not scroll down");
        assert!(output.contains("\x1b[7;1H"), "first line is pinned above the footer");
        assert!(output.contains("\x1b[10;1H"), "footer occupies bottom row");
        assert!(output.ends_with("\x1b[8;4H"), "cursor on the input row");
        assert!(!output.contains("\r\n"), "nothing scrolls when it already fits");
        assert_eq!(screen.top, Some(6));
    }

    #[test]
    fn growing_pinned_prompt_scrolls_only_the_rows_it_overshoots() {
        let mut screen = Screen::pinned();
        let mut lines = frame_lines();
        screen.draw_pinned_to(&mut Vec::new(), &lines, 1, 3, 10, 9).unwrap();
        lines.push(("/status".into(), TextStyle::Muted));
        let mut output = Vec::new();
        screen.draw_pinned_to(&mut output, &lines, 1, 3, 10, 9).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("\r\n").count(), 1);
        assert!(output.starts_with("\x1b[10;1H\r\n"));
        assert!(output.contains("\x1b[6;1H"), "the taller frame starts a row up");
        assert_eq!(screen.top, Some(5));
    }

    #[test]
    fn shrinking_pinned_prompt_erases_the_rows_it_leaves() {
        let mut screen = Screen::pinned();
        let mut lines = frame_lines();
        screen.draw_pinned_to(&mut Vec::new(), &lines, 1, 3, 10, 9).unwrap();
        lines.pop();
        let mut output = Vec::new();
        screen.draw_pinned_to(&mut output, &lines, 1, 3, 10, 9).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\x1b[7;1H\x1b[J"), "shrinking clears the old frame without moving the transcript");
        assert_eq!(output.matches("\x1b[K").count(), 3);
        assert!(!output.contains("\r\n"));
        assert_eq!(screen.top, Some(7));
    }

    #[test]
    fn tiny_pinned_prompt_keeps_input_and_cursor_visible() {
        let lines = vec![
            ("top border".into(), TextStyle::Border),
            ("INPUT_SENTINEL".into(), TextStyle::Plain),
            ("bottom border".into(), TextStyle::Border),
            ("footer".into(), TextStyle::Muted),
        ];
        for rows in 1..=3 {
            let mut screen = Screen::pinned();
            let mut output = Vec::new();
            screen.draw_pinned_to(&mut output, &lines, 1, 4, rows, 0).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("INPUT_SENTINEL"));
            let input_row = if rows == 1 { 1 } else { 2 };
            assert!(output.ends_with(&format!("\x1b[{input_row};5H")));
        }
    }

    #[test]
    fn pinned_prompt_taller_than_the_screen_keeps_its_bottom_rows() {
        let mut screen = Screen::pinned();
        let lines = vec![("row".into(), TextStyle::Plain); 12];
        let mut output = Vec::new();
        screen.draw_pinned_to(&mut output, &lines, 11, 0, 10, 9).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[1;1H"), "the frame fills the screen");
        assert!(!output.contains("\x1b[11;1H"), "rows past the screen are dropped");
        assert!(output.ends_with("\x1b[10;1H"));
        assert_eq!(screen.top, Some(0));
    }

    #[test]
    fn related_description_matches_follow_exact_model_and_menu_keeps_selection_visible() {
        let commands = vec![
            ("/status".into(), "Inspect model state".into()),
            ("/model".into(), "Change model".into()),
            ("/model-test".into(), "Test".into()),
            ("/exit".into(), "Exit".into()),
        ];
        assert_eq!(matches(&commands, "/model"), vec![1, 2, 0]);
        // The reference order for `/co` and `/mo`: shortest prefix matches
        // first, then names containing the text, then letters in order.
        let names = ["/compact", "/context", "/config", "/color", "/copy", "/model", "/mobile", "/memory", "/auto-mode-setup"];
        let commands: Vec<(String, String)> = names.iter().map(|name| (name.to_string(), String::new())).collect();
        let order = |text| matches(&commands, text).into_iter().map(|index| names[index]).collect::<Vec<_>>();
        assert_eq!(order("/co"), ["/copy", "/color", "/config", "/compact", "/context"]);
        assert_eq!(order("/mo"), ["/model", "/mobile", "/auto-mode-setup", "/memory"]);
        assert_eq!(order("/")[..3], ["/auto-mode-setup", "/color", "/compact"]);
        // Scattered letters elsewhere in a long name are no match.
        let skill = vec![("/synced:54534282-ab-x9x".to_string(), String::new())];
        assert!(matches(&skill, "/xx").is_empty());
        assert_eq!(menu_page(&[1, 3, 1, 2, 1], 1, 4), (0, 2));
        assert_eq!(menu_page(&[1, 3, 1, 2, 1], 3, 4), (2, 5));
        assert_eq!(menu_page(&[3], 0, 0), (0, 0));
    }
    #[test]
    fn detail_disclosure_has_separate_expand_and_bottom_collapse_targets() {
        let mut details = DetailsPanel {
            entries: vec![(
                "Read src/main.rs".into(),
                "one\ntwo\nthree\nfour\nfive\nsix".into(),
            )],
            ..Default::default()
        };
        let collapsed = details.render(80, 5);
        assert_eq!(collapsed.lines.len(), 1);
        let hit = collapsed.toggle.unwrap();
        assert!(!hit.contains(0, 0));
        assert!(hit.contains(0, hit.start));
        assert!(!hit.contains(1, hit.start));
        assert!(!hit.contains(0, hit.end));
        details.toggle();
        let expanded = details.render(80, 5);
        assert_eq!(expanded.lines.len(), 5);
        assert_eq!(expanded.viewport, 3);
        assert_eq!(expanded.toggle.unwrap().row, 4);
        assert!(expanded.lines.last().unwrap().0.starts_with("Show less"));
        details.toggle();
        assert_eq!(details.render(80, 5).lines.len(), 1);
    }
    #[test]
    fn detail_scroll_clamps_and_resize_reflows_full_output() {
        let mut details = DetailsPanel {
            entries: vec![(
                "Read".into(),
                "\u{4e2d}\u{6587}abcdef\nsecond\nthird\nfourth".into(),
            )],
            expanded: true,
            ..Default::default()
        };
        let frame = details.render(5, 4);
        assert_eq!(frame.viewport, 2);
        assert!(details
            .wrapped
            .iter()
            .any(|line| line == "\u{4e2d}\u{6587}a"));
        details.scroll_by(1, frame.viewport);
        assert_eq!(details.scroll, 1);
        details.scroll_by(-1, frame.viewport);
        assert_eq!(details.scroll, 0);
        details.scroll_by(100, frame.viewport);
        assert_eq!(details.scroll, details.wrapped.len() - frame.viewport);
        details.scroll_by(-100, frame.viewport);
        assert_eq!(details.scroll, 0);
        let narrow_len = details.wrapped.len();
        details.render(80, 4);
        assert!(details.wrapped.len() < narrow_len);
        assert!(details
            .wrapped
            .iter()
            .any(|line| line == "\u{4e2d}\u{6587}abcdef"));
    }
    #[test]
    fn details_strip_terminal_controls_but_preserve_newlines_and_tabs() {
        let lines = wrap_detail("ok\u{1b}[2J\r\n\t\u{4e2d}\u{6587}\u{009b}", 80);
        assert_eq!(lines, ["ok[2J", "    \u{4e2d}\u{6587}"]);
        assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
    }
    #[test]
    fn empty_or_short_terminal_details_do_not_overflow_budget() {
        assert!(DetailsPanel::default().render(80, 4).lines.is_empty());
        let mut panel = DetailsPanel {
            entries: vec![("Read".into(), "output".into())],
            expanded: true,
            ..Default::default()
        };
        assert!(panel.render(80, 0).lines.is_empty());
        assert_eq!(panel.render(80, 2).lines.len(), 1);
    }
    #[test]
    fn pasted_unicode_is_editable_and_control_codes_are_removed() {
        let mut b = Buffer::default();
        b.insert("中🙂\r\ntext\u{1b}");
        assert_eq!(b.text(), "中🙂\ntext");
        b.cursor = 1;
        b.insert("文");
        assert_eq!(b.text(), "中文🙂\ntext");
    }
    #[test]
    fn filters_builtins_and_dynamic_commands_without_matching_arguments() {
        let c = vec![
            ("/help".into(), "Help".into()),
            ("/skill:review".into(), "Review".into()),
        ];
        assert_eq!(matches(&c, "/"), vec![0, 1]);
        assert_eq!(matches(&c, "/skill"), vec![1]);
        assert!(matches(&c, "/help arg").is_empty());
        let unsorted = vec![
            ("/worktree".into(), "Worktrees".into()),
            ("/compact".into(), "Compact".into()),
            ("/add-dir".into(), "Add a directory to compact".into()),
            ("/color".into(), "Color".into()),
        ];
        assert_eq!(matches(&unsorted, "/"), vec![2, 3, 1, 0]);
        // Name prefixes first, then substring and description matches, each alphabetical.
        assert_eq!(matches(&unsorted, "/co"), vec![3, 1, 2]);
    }
    #[test]
    fn display_width_handles_wide_and_combining_characters() {
        assert_eq!(clipped("中文abc", 5), "中文a");
        assert_eq!(clipped("e\u{301}x", 1), "e\u{301}");
        assert_eq!(clipped("\u{1b}hi", 2), "hi");
    }
}

/// $VISUAL, $EDITOR or vi, for Ctrl+G.
fn external_editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .and_then(|editor| editor.split_whitespace().next().map(|name| name.rsplit(['/', '\\']).next().unwrap_or(name).to_owned()))
        .unwrap_or_else(|| "vi".into())
}

/// Ctrl+G: the input in an external editor, as in the reference CLI.
pub(crate) fn edit_externally(text: &str) -> io::Result<String> {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new().prefix("dcl-prompt-").suffix(".md").tempfile()?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_owned();
    terminal::disable_raw_mode()?;
    let status = std::process::Command::new(&program).args(parts).arg(file.path()).status();
    terminal::enable_raw_mode()?;
    match status {
        Ok(_) => Ok(std::fs::read_to_string(file.path())?.trim_end_matches('\n').to_owned()),
        Err(error) => {
            eprint!("Could not open {program}: {error}\r\n");
            Ok(text.to_owned())
        }
    }
}

/// Ctrl+Z: stop like a shell job until `fg`, as in the reference CLI.
fn suspend_process() -> io::Result<()> {
    terminal::disable_raw_mode()?;
    eprintln!("DeepSeek Code has been suspended. Run `fg` to bring DeepSeek Code back.");
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").arg("-TSTP").arg(std::process::id().to_string()).status();
    }
    terminal::enable_raw_mode()?;
    Ok(())
}
