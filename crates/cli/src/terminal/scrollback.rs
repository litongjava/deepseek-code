//! Lines logged above the input box that are still on screen, so a click on a
//! tool result can show it in full and a second click can fold it again. Only
//! rows below the clicked block are redrawn; scrollback stays as it was.
use super::change_view::{self, Line};
use crossterm::terminal;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

/// One logged line as written, the screen rows it takes and the tool result
/// it belongs to.
struct Logged {
    text: String,
    rows: usize,
    block: Option<usize>,
}
/// A tool result with hidden lines, shown short or in full.
struct Block {
    short: Vec<Line>,
    full: Vec<Line>,
    open: bool,
}

#[derive(Default)]
pub(super) struct Scrollback {
    logged: VecDeque<Logged>,
    blocks: Vec<Block>,
}

/// The last turn's lines, handed from the live view to the prompt after it.
static LAST: std::sync::Mutex<Option<Scrollback>> = std::sync::Mutex::new(None);

impl Scrollback {
    pub(super) fn save(self) {
        *LAST.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(self);
    }
    pub(super) fn take() -> Self {
        LAST.lock().unwrap_or_else(|poison| poison.into_inner()).take().unwrap_or_default()
    }
    pub(super) fn clear(&mut self) {
        self.logged.clear();
    }
    /// Records a written line; returns the screen rows it takes.
    pub(super) fn push(&mut self, text: String, block: Option<usize>) -> usize {
        let (columns, height) = terminal::size().unwrap_or((80, 24));
        let rows = screen_rows(&text, columns as usize);
        self.logged.push_back(Logged { text, rows, block });
        // Only lines still on screen can be clicked.
        while self.logged.iter().map(|line| line.rows).sum::<usize>() > height as usize * 2 {
            self.logged.pop_front();
        }
        rows
    }
    /// A tool result's short lines, to be written and recorded with `push`.
    pub(super) fn block(&mut self, short: Vec<Line>, full: Vec<Line>) -> (usize, Vec<String>) {
        let lines = rendered(&short);
        self.blocks.push(Block { short, full, open: false });
        (self.blocks.len() - 1, lines)
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
    /// Switches the tool result on `row` between its short and full form.
    /// Returns the screen row to redraw from and the lines to write there.
    pub(super) fn toggle(&mut self, bottom: u16, row: u16) -> Option<(u16, Vec<(String, Option<usize>)>)> {
        let index = self.line_at(bottom, row)?;
        let block = self.logged[index].block?;
        let first = self.logged.iter().position(|line| line.block == Some(block))?;
        let above: usize = self.logged.iter().skip(first).map(|line| line.rows).sum();
        let start = (bottom as usize).saturating_sub(above) as u16;
        let entry = &mut self.blocks[block];
        entry.open = !entry.open;
        let lines = rendered(if entry.open { &entry.full } else { &entry.short });
        let after = self.logged.drain(first..).filter(|line| line.block != Some(block)).map(|line| (line.text, line.block));
        let rows = lines.into_iter().map(|line| (line, Some(block))).chain(after).collect();
        Some((start, rows))
    }
}

/// A tool result's lines as written above the input box.
fn rendered(lines: &[Line]) -> Vec<String> {
    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
    let color = std::env::var_os("NO_COLOR").is_none();
    lines.iter().map(|line| change_view::render(line, width, color)).collect()
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
    fn clicks_switch_the_tool_result_on_that_row() {
        let mut lines = Scrollback::default();
        lines.push("answer".into(), None);
        let (block, short) = lines.block(vec![change_view::body("short 1"), change_view::body("short 2")], vec![change_view::body("full")]);
        for line in short {
            lines.push(line, Some(block));
        }
        lines.push("after".into(), None);
        // With the input box on row 10, rows 6..10 hold answer, short 1, short 2, after.
        assert_eq!(lines.line_at(10, 6), Some(0));
        assert_eq!(lines.line_at(10, 8), Some(2));
        assert_eq!(lines.line_at(10, 10), None);
        assert!(lines.toggle(10, 6).is_none());
        let (start, rows) = lines.toggle(10, 8).unwrap();
        assert_eq!(start, 7);
        let shown: Vec<_> = rows.iter().map(|(text, block)| (text.contains("full"), *block)).collect();
        assert_eq!(shown, vec![(true, Some(0)), (false, None)]);
        assert_eq!(lines.logged.len(), 1);
    }
}
