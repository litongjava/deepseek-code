//! Lines logged above the input box that are still on screen, so a click on a
//! tool result can show it in full and a second click can fold it again. Only
//! rows below the clicked block are redrawn; scrollback stays as it was.
use super::change_view::{self, Line};
use crossterm::terminal;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

/// One logged line as written, the screen rows it takes and the tool result
/// it belongs to.
#[derive(Clone)]
struct Logged {
    text: String,
    rows: usize,
    block: Option<usize>,
}
/// A tool result with hidden lines, shown short or in full.
#[derive(Clone)]
struct Block {
    short: Vec<Line>,
    full: Vec<Line>,
    header_open: bool,
    body_open: bool,
    whole: bool,
    compact_level: u8,
}

#[derive(Default, Clone)]
pub(crate) struct Scrollback {
    logged: VecDeque<Logged>,
    blocks: Vec<Block>,
}

/// The last turn's lines, handed from the live view to the prompt after it.
static LAST: std::sync::Mutex<Option<Scrollback>> = std::sync::Mutex::new(None);

static COMPACT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn compact() -> bool { COMPACT.load(std::sync::atomic::Ordering::Relaxed) }
pub(crate) fn toggle_compact() { COMPACT.fetch_xor(true, std::sync::atomic::Ordering::Relaxed); }
pub(crate) fn compact_key(key: crossterm::event::KeyEvent) -> bool {
    key.modifiers.contains(crossterm::event::KeyModifiers::ALT) && key.code == crossterm::event::KeyCode::Char('m')
}

impl Scrollback {
    pub(crate) fn save(self) {
        *LAST.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(self);
    }
    pub(super) fn snapshot() -> Self {
        LAST.lock().unwrap_or_else(|poison| poison.into_inner()).clone().unwrap_or_default()
    }
    pub(super) fn take() -> Self {
        LAST.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .unwrap_or_default()
    }
    pub(super) fn clear(&mut self) {
        self.logged.clear();
    }
    /// Records a written line; returns the screen rows it takes.
    pub(crate) fn push(&mut self, text: String, block: Option<usize>) -> usize {
        let (columns, _height) = terminal::size().unwrap_or((80, 24));
        let rows = screen_rows(&text, columns as usize);
        self.logged.push_back(Logged { text, rows, block });
        rows
    }
    /// A tool result's short lines, to be written and recorded with `push`.
    pub(crate) fn block(&mut self, short: Vec<Line>, full: Vec<Line>) -> (usize, Vec<String>) {
        let lines = collapsed(&short);
        let plain = change_view::plain(&short);
        let whole = plain.trim_start().starts_with("Thought") || plain.contains("Updated ");
        self.blocks.push(Block {
            short,
            full,
            header_open: false,
            body_open: false,
            whole,
            compact_level: 0,
        });
        (self.blocks.len() - 1, lines)
    }
    pub(crate) fn needs_separator(&self) -> bool {
        self.logged.back().is_some_and(|line| !super::markdown::plain_rendered(&line.text).trim().is_empty())
    }
    pub(crate) fn all(&self) -> Vec<String> {
        self.logged.iter().map(|line| line.text.clone()).collect()
    }
    /// A lossless presentation over logged blocks, with one target per physical row.
    fn compact_rows(&self) -> Vec<(String, Option<usize>)> {
        let mut result = Vec::new();
        let mut index = 0;
        while index < self.logged.len() {
            let Some(first) = self.logged[index].block else {
                result.push((self.logged[index].text.clone(), None));
                index += 1;
                continue;
            };
            let mut ids = vec![first];
            index += 1;
            while index < self.logged.len() {
                if let Some(id) = self.logged[index].block {
                    if !ids.contains(&id) { ids.push(id); }
                    index += 1;
                } else if self.logged[index].text.trim().is_empty()
                    && self.logged.get(index + 1).is_some_and(|line| line.block.is_some()) {
                    index += 1;
                } else { break; }
            }
            if self.blocks[first].compact_level == 0 {
                let mut counts = [0usize; 6];
                for id in &ids {
                    let title = change_view::plain(&self.blocks[*id].short[..1]);
                    let kind = if title.contains("Bash(") || title.contains("PowerShell(") { 0 }
                        else if title.contains("Read(") { 1 }
                        else if title.contains("Grep(") { 2 }
                        else if title.contains("Glob(") { 3 }
                        else if title.contains("Updated ") || title.contains("Write(") || title.contains("Edit(") { 4 }
                        else { 5 };
                    counts[kind] += 1;
                }
                let forms = [("ran", "shell command", "shell commands"), ("read", "file", "files"),
                    ("searched for", "pattern", "patterns"), ("listed", "directory", "directories"),
                    ("updated", "file", "files"), ("used", "tool", "tools")];
                let mut summary = forms.iter().enumerate().filter(|(i, _)| counts[*i] > 0)
                    .map(|(i, (verb, one, many))| format!("{verb} {} {}", counts[i], if counts[i] == 1 { one } else { many }))
                    .collect::<Vec<_>>().join(", ");
                if let Some(first) = summary.get_mut(..1) { first.make_ascii_uppercase(); }
                result.push((format!("  {summary}  [click to expand]"), Some(first)));
            } else {
                for (position, id) in ids.into_iter().enumerate() {
                    if position > 0 { result.push((String::new(), None)); }
                    let block = &self.blocks[id];
                    let rows = if block.compact_level == 2 { rendered(&block.full) } else { collapsed(&block.short) };
                    result.extend(rows.into_iter().map(|row| (row, Some(id))));
                }
            }
        }
        result
    }
    pub(crate) fn presentation(&self) -> Vec<String> {
        if compact() { self.compact_rows().into_iter().map(|(text, _)| text).collect() } else { self.all() }
    }
    pub(crate) fn click_presentation(&mut self, row: usize) -> bool {
        if !compact() { return self.toggle_history(row); }
        let Some(id) = self.compact_rows().get(row).and_then(|(_, id)| *id) else { return false; };
        if self.blocks[id].compact_level == 0 {
            let mut index = self.logged.iter().position(|line| line.block == Some(id)).unwrap();
            while index < self.logged.len() {
                if let Some(member) = self.logged[index].block {
                    self.blocks[member].compact_level = 1;
                    index += 1;
                } else if self.logged[index].text.trim().is_empty()
                    && self.logged.get(index + 1).is_some_and(|line| line.block.is_some()) { index += 1; }
                else { break; }
            }
        } else {
            self.blocks[id].compact_level = if self.blocks[id].compact_level == 1 { 2 } else { 1 };
        }
        true
    }
    pub(super) fn toggle_history(&mut self, row: usize) -> bool {
        // Each logged entry is a physical row in the normal view.
        let Some(line) = self.logged.get(row) else {
            return false;
        };
        if line.block.is_none() {
            return false;
        }
        let bottom = self.logged.len();
        if bottom > u16::MAX as usize {
            return false;
        }
        let Some((_, rows)) = self.toggle(bottom as u16, row as u16) else {
            return false;
        };
        for (text, block) in rows {
            self.push(text, block);
        }
        true
    }
    /// The logged line on screen row `row`, given the row below the last one.
    fn line_at(&self, bottom: u16, row: u16) -> Option<usize> {
        let mut bottom = bottom;
        for (index, line) in self.logged.iter().enumerate().rev() {
            let first = bottom.checked_sub(line.rows as u16)?;
            if row >= first && row < bottom {
                return Some(index);
            }
            bottom = first;
        }
        None
    }
    pub(super) fn screen(&self, bottom: u16, height: u16) -> Vec<String> {
        let mut rows = vec![String::new(); height as usize];
        let mut end = bottom as usize;
        for line in self.logged.iter().rev() {
            let width = terminal::size().unwrap_or((80, 24)).0 as usize;
            let wrapped = super::markdown::wrap_styled(&line.text, "", "", width);
            for text in wrapped.iter().rev() {
                if end == 0 {
                    return rows;
                }
                end -= 1;
                if end < rows.len() {
                    rows[end] = text.clone();
                }
            }
        }
        rows
    }
    /// Switches the tool result on `row` between its short and full form.
    /// Returns the screen row to redraw from and the lines to write there.
    pub(super) fn toggle(
        &mut self,
        bottom: u16,
        row: u16,
    ) -> Option<(u16, Vec<(String, Option<usize>)>)> {
        let index = self.line_at(bottom, row)?;
        let block = self.logged[index].block?;
        let first = self
            .logged
            .iter()
            .position(|line| line.block == Some(block))?;
        let above: usize = self.logged.iter().skip(first).map(|line| line.rows).sum();
        let start = (bottom as usize).saturating_sub(above) as u16;
        let entry = &mut self.blocks[block];
        let header_rows = if entry.header_open {
            rendered(&entry.full[..1]).len()
        } else {
            1
        };
        if entry.whole {
            entry.header_open = !entry.header_open;
            entry.body_open = entry.header_open;
        } else if index - first < header_rows {
            entry.header_open = !entry.header_open;
        } else {
            entry.body_open = !entry.body_open;
        }
        let mut lines = if entry.header_open {
            rendered(&entry.full[..1])
        } else {
            collapsed(&entry.short[..1])
        };
        lines.extend(if entry.body_open {
            rendered(&entry.full[1..])
        } else {
            collapsed(&entry.short[1..])
        });
        let after = self
            .logged
            .drain(first..)
            .filter(|line| line.block != Some(block))
            .map(|line| (line.text, line.block));
        let rows = lines
            .into_iter()
            .map(|line| (line, Some(block)))
            .chain(after)
            .collect();
        Some((start, rows))
    }
}

/// A tool result's lines as written above the input box.
fn collapsed(lines: &[Line]) -> Vec<String> {
    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
    lines
        .iter()
        .map(|line| {
            let color = std::env::var_os("NO_COLOR").is_none();
            let plain = change_view::plain(std::slice::from_ref(line));
            if unicode_width::UnicodeWidthStr::width(plain.trim_end()) > width {
                format!(
                    "{}…",
                    change_view::render(line, width.saturating_sub(1), color)
                )
            } else {
                change_view::render(line, width, color)
            }
        })
        .collect()
}

fn rendered(lines: &[Line]) -> Vec<String> {
    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
    let color = std::env::var_os("NO_COLOR").is_none();
    lines
        .iter()
        .flat_map(|line| {
            let text = change_view::render(line, usize::MAX, color);
            super::markdown::wrap_styled(&text, "", "", width)
        })
        .collect()
}

/// Screen rows a written line takes at `columns`, style escapes excluded.
fn screen_rows(text: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    text.split("\r\n")
        .map(|line| {
            let mut cells = 0;
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                if ch == '\x1b' {
                    // CSI ends at its final byte, OSC at BEL or ST.
                    match chars.next() {
                        Some('[') => {
                            for ch in chars.by_ref() {
                                if ('@'..='~').contains(&ch) {
                                    break;
                                }
                            }
                        }
                        Some(']') => {
                            for ch in chars.by_ref() {
                                if ch == '\x07' || ch == '\\' {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    cells += ch.width().unwrap_or(0);
                }
            }
            cells.div_ceil(columns).max(1)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_rows_count_wrapped_rows_without_style_escapes() {
        assert_eq!(screen_rows("", 10), 1);
        assert_eq!(screen_rows("\x1b[38;2;1;2;3mabcdefghij\x1b[0m", 10), 1);
        assert_eq!(screen_rows("abcdefghijk", 10), 2);
        assert_eq!(screen_rows("ab\r\ncd", 10), 2);
        assert_eq!(screen_rows("\u{4f60}\u{597d}\u{4f60}", 4), 2);
    }

    #[test]
    fn thought_header_content_and_footer_toggle_the_entire_block() {
        let mut history = Scrollback::default();
        let (short, full) = super::super::tool_view::thought("one\ntwo\nthree\nfour\nfive", Some(12));
        let (id, rows) = history.block(short, full);
        for row in rows { history.push(row, Some(id)); }
        assert!(history.all()[0].contains("Thought for 12s 5 lines (click to expand)"));
        assert!(history.toggle_history(0));
        assert_eq!(history.all().len(), 7);
        assert!(history.all()[6].contains("(click to collapse)"));
        assert!(history.toggle_history(2));
        assert_eq!(history.all().len(), 1);
        assert!(history.toggle_history(0));
        assert!(history.toggle_history(6));
        assert_eq!(history.all().len(), 1);
    }

    #[test]
    fn compact_groups_preserve_commentary_and_have_three_levels() {
        let mut history = Scrollback::default();
        for n in 0..2 {
            let short = vec![change_view::body(&format!("● Bash(command {n})"))];
            let full = vec![short[0].clone(), change_view::body("secret output")];
            let (id, rows) = history.block(short, full);
            for row in rows { history.push(row, Some(id)); }
            history.push(String::new(), None);
        }
        history.push("Assistant explanation".into(), None);
        let (id, rows) = history.block(vec![change_view::body("● Read(file)")], vec![change_view::body("file contents")]);
        for row in rows { history.push(row, Some(id)); }
        let rows = history.compact_rows();
        assert!(rows[0].0.contains("Ran 2 shell commands"));
        assert!(rows.iter().any(|(row, _)| row == "Assistant explanation"));
        assert!(rows.last().unwrap().0.contains("Read 1 file"));
        history.blocks[0].compact_level = 1;
        let rows = history.compact_rows();
        assert!(rows[0].0.contains("command 0"));
        assert!(rows[2].0.contains("command 1"));
        assert!(!rows.iter().any(|(row, _)| row.contains("secret output")));
        history.blocks[0].compact_level = 2;
        assert!(history.compact_rows().iter().any(|(row, _)| row.contains("secret output")));
        history.blocks[0].compact_level = 0;
        assert!(history.compact_rows()[0].0.contains("Ran 2 shell commands"));
        assert_eq!(history.all().len(), 6);
    }

    #[test]
    fn file_change_header_expands_all_diff_rows_and_body_collapses() {
        let mut history = Scrollback::default();
        let mut full = vec![change_view::success("Updated test.rs (+12 -0)")];
        full.extend((0..12).map(|i| change_view::body(&format!("line {i}"))));
        let (id, rows) = history.block(full[..5].to_vec(), full);
        for row in rows { history.push(row, Some(id)); }
        assert_eq!(history.all().len(), 5);
        assert!(history.toggle_history(0));
        assert_eq!(history.all().len(), 13);
        assert!(history.toggle_history(3));
        assert_eq!(history.all().len(), 5);
    }

    #[test]
    fn header_and_body_toggle_independently_and_collapse_again() {
        let mut lines = Scrollback::default();
        let short = vec![
            change_view::body("short header"),
            change_view::body("short body"),
        ];
        let full = vec![
            change_view::body("full header"),
            change_view::body("full body 1"),
            change_view::body("full body 2"),
        ];
        let (block, rows) = lines.block(short, full);
        for row in rows {
            lines.push(row, Some(block));
        }
        assert!(lines.toggle_history(1));
        assert!(lines.all()[0].contains("short header"));
        assert_eq!(lines.all().len(), 3);
        assert!(lines.toggle_history(0));
        assert!(lines.all()[0].contains("full header"));
        assert!(lines.toggle_history(1));
        assert_eq!(lines.all().len(), 2);
        assert!(lines.all()[0].contains("full header"));
        assert!(lines.toggle_history(0));
        assert!(lines.all()[0].contains("short header"));
    }
}
