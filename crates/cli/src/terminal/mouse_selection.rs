//! Shared click versus drag handling for inline terminal views.
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    event::{MouseButton, MouseEvent, MouseEventKind},
    queue,
    style::Print,
};
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub(super) struct CopyClick(Option<(u16, u16, std::time::Instant, String)>);
impl CopyClick {
    pub fn event(&mut self, mouse: MouseEvent, text: Option<String>) -> bool {
        if matches!(mouse.kind, MouseEventKind::Drag(_)) {
            self.0 = None;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        if let Some((row, col, when, text)) = self.0.take() {
            if row == mouse.row
                && col.abs_diff(mouse.column) <= 3
                && when.elapsed() < std::time::Duration::from_millis(500)
            {
                let _ = crate::extended_commands::copy_text(&text);
                return true;
            }
        }
        self.0 = text.map(|text| (mouse.row, mouse.column, std::time::Instant::now(), text));
        false
    }
}

#[derive(Default)]
pub(super) struct Selection {
    start: Option<(usize, usize)>,
    end: (usize, usize),
    rows: Vec<String>,
    dragged: bool,
    held: bool,
    painted: std::cell::RefCell<Vec<String>>,
    viewport: Option<(usize, usize)>,
    viewport_offset: usize,
}
impl Selection {
    pub fn active(&self) -> bool {
        self.held
    }
    pub fn retained(&self) -> bool {
        self.start.is_some()
    }
    /// Old coordinates are invalid after a resize; do not repaint their snapshot.
    pub fn discard(&mut self) {
        *self = Self::default();
    }
    pub fn clear(&mut self) -> io::Result<()> {
        if self.start.is_some() {
            self.paint(false)?;
        }
        self.start = None;
        self.held = false;
        self.viewport = None;
        self.viewport_offset = 0;
        Ok(())
    }
    /// Returns a click only after release without motion. A drag copies text.
    pub fn event(
        &mut self,
        mouse: MouseEvent,
        rows: Vec<String>,
    ) -> io::Result<Option<MouseEvent>> {
        let point = (
            self.viewport.map_or(mouse.row as usize, |view| {
                view.0
                    + (mouse.row as usize)
                        .saturating_sub(self.viewport_offset)
                        .min(view.1.saturating_sub(1))
            }),
            mouse.column as usize,
        );
        match mouse.kind {
            // Passive mouse movement must not trigger a history redraw over
            // the retained highlight after the button has been released.
            MouseEventKind::Moved if self.retained() => Ok(None),
            MouseEventKind::Down(MouseButton::Left) => {
                let viewport = self.viewport;
                let offset = self.viewport_offset;
                self.clear()?;
                self.viewport = viewport;
                self.viewport_offset = offset;
                self.held = true;
                self.start = Some(point);
                self.end = point;
                self.rows = rows;
                self.painted.borrow_mut().clear();
                self.dragged = false;
                Ok(None)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.active() => {
                self.dragged |= self.start != Some(point);
                self.end = point;
                self.paint(true)?;
                Ok(None)
            }
            MouseEventKind::Up(MouseButton::Left) if self.active() => {
                self.dragged |= self.start != Some(point);
                self.end = point;
                if self.dragged {
                    let text = self.text();
                    self.paint(true)?;
                    self.held = false;
                    if !text.is_empty() {
                        let _ = crate::extended_commands::copy_text(&text);
                    }
                    Ok(None)
                } else {
                    self.held = false;
                    self.start = None;
                    Ok(Some(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        ..mouse
                    }))
                }
            }
            _ => Ok(Some(mouse)),
        }
    }
    /// Selects one whole row, as a drag across it would, and copies its text.
    pub fn select_row(&mut self, row: u16, rows: Vec<String>) -> io::Result<()> {
        let width = rows
            .get(row as usize)
            .map_or(0, |text| unicode_width::UnicodeWidthStr::width(super::markdown::plain_rendered(text).trim_end()));
        if width == 0 {
            return Ok(());
        }
        self.clear()?;
        self.rows = rows;
        self.painted.borrow_mut().clear();
        self.viewport = None;
        self.start = Some((row as usize, 0));
        self.end = (row as usize, width);
        self.dragged = true;
        self.held = false;
        self.paint(true)?;
        let text = self.text();
        if !text.is_empty() {
            let _ = crate::extended_commands::copy_text(&text);
        }
        Ok(())
    }
    /// Use document coordinates so scrolling cannot discard the selection anchor.
    pub fn document_event(
        &mut self,
        mouse: MouseEvent,
        rows: Vec<String>,
        top: usize,
        page: usize,
        offset: usize,
    ) -> io::Result<Option<MouseEvent>> {
        self.viewport = Some((top, page.saturating_sub(offset)));
        self.viewport_offset = offset;
        self.event(mouse, rows)
    }
    pub fn screen_event(
        &mut self,
        mouse: MouseEvent,
        rows: Vec<String>,
    ) -> io::Result<Option<MouseEvent>> {
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.clear()?;
        }
        self.event(mouse, rows)
    }
    pub fn document(&self) -> bool {
        self.viewport.is_some()
    }
    pub fn promote(&mut self, rows: Vec<String>, page: usize, offset: usize) -> Option<usize> {
        let start = self.start?;
        let visible: Vec<_> = self.rows.iter().map(|row|
            super::markdown::plain_rendered(row).trim_end().to_owned()).collect();
        let document: Vec<_> = rows.iter().map(|row|
            super::markdown::plain_rendered(row).trim_end().to_owned()).collect();
        let anchor = visible.get(start.0)?;
        // Repeated code lines need their surrounding rows to identify the anchor.
        let index = document.iter().enumerate().filter(|(_, row)| *row == anchor)
            .max_by_key(|(index, _)| {
                visible.iter().enumerate()
                    .filter(|(row, text)| !text.is_empty() && row.abs_diff(start.0) <= 12)
                    .filter(|(row, text)| index.checked_add(*row)
                        .and_then(|value| value.checked_sub(start.0))
                        .and_then(|value| document.get(value)) == Some(*text))
                    .count()
            }).map(|(index, _)| index)?;
        let top = index
            .saturating_sub(start.0.saturating_sub(offset))
            .min(rows.len().saturating_sub(page.saturating_sub(offset)));
        self.start = Some((index, start.1));
        self.end.0 = top
            + self
                .end
                .0
                .saturating_sub(offset)
                .min(page.saturating_sub(offset + 1));
        self.rows = rows;
        self.viewport = Some((top, page.saturating_sub(offset)));
        self.viewport_offset = offset;
        self.painted.borrow_mut().clear();
        Some(top)
    }
    pub fn scroll_to(
        &mut self,
        top: usize,
        page: usize,
        offset: usize,
        mouse: MouseEvent,
    ) -> io::Result<()> {
        self.viewport = Some((top, page.saturating_sub(offset)));
        self.viewport_offset = offset;
        self.end = (
            top + (mouse.row as usize)
                .saturating_sub(offset)
                .min(page.saturating_sub(offset + 1)),
            mouse.column as usize,
        );
        self.dragged = true;
        self.repaint()
    }
    pub fn repaint(&self) -> io::Result<()> {
        self.painted.borrow_mut().clear();
        self.paint(true)
    }
    fn bounds(&self) -> ((usize, usize), (usize, usize)) {
        let start = self.start.unwrap_or(self.end);
        (start.min(self.end), start.max(self.end))
    }
    fn slice(&self, row: usize) -> String {
        let (start, end) = self.bounds();
        if row < start.0 as usize || row > end.0 as usize {
            return String::new();
        }
        let text = super::markdown::plain_rendered(&self.rows[row]);
        let left = if row == start.0 as usize {
            start.1 as usize
        } else {
            0
        };
        let right = if row == end.0 as usize {
            end.1 as usize
        } else {
            usize::MAX
        };
        let mut cell = 0;
        text.chars()
            .filter(|ch| {
                let begin = cell;
                cell += ch.width().unwrap_or(0);
                cell > left && begin < right
            })
            .collect()
    }
    fn text(&self) -> String {
        let (start, end) = self.bounds();
        (start.0 as usize..=end.0 as usize)
            .take_while(|row| *row < self.rows.len())
            .map(|row| self.slice(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn paint(&self, selected: bool) -> io::Result<()> {
        let mut out = io::stderr().lock();
        self.paint_to(&mut out, selected)?;
        out.flush()
    }
    fn paint_to(&self, out: &mut impl Write, selected: bool) -> io::Result<()> {
        out.write_all(b"\x1b[?2026h")?;
        queue!(out, SavePosition)?;
        let mut painted = self.painted.borrow_mut();
        painted.resize(self.rows.len(), String::new());
        let (top, page) = self.viewport.unwrap_or((0, self.rows.len()));
        for (row, text) in self.rows.iter().enumerate().skip(top).take(page) {
            // Empty snapshot entries may be untracked terminal content. Never erase them.
            if text.is_empty() {
                continue;
            }
            let part = if selected {
                self.slice(row)
            } else {
                String::new()
            };
            let mark = if part.is_empty() {
                String::new()
            } else {
                format!(
                    "{}:{part}",
                    if row == self.bounds().0 .0 as usize {
                        self.bounds().0 .1
                    } else {
                        0
                    }
                )
            };
            if painted[row] == mark {
                continue;
            }
            painted[row] = mark;
            let rendered = if selected && !part.is_empty() {
                let (start, end) = self.bounds();
                let left = if row == start.0 as usize {
                    start.1 as usize
                } else {
                    0
                };
                let right = if row == end.0 as usize {
                    end.1 as usize
                } else {
                    usize::MAX
                };
                highlight_row(text, left, right)
            } else {
                text.clone()
            };
            // Publish each row with its highlight already applied. Never paint
            // an unselected copy first: terminals without synchronized output
            // support would briefly erase the retained selection on every drag.
            queue!(
                out,
                MoveTo(0, (row - top + self.viewport_offset) as u16),
                Print(rendered),
                Print("\x1b[0m\x1b[K")
            )?;
        }
        queue!(out, RestorePosition)?;
        out.write_all(b"\x1b[?2026l")?;
        Ok(())
    }
}

/// Whether a screen row is a turn's closing status, `✻ Worked for 4s · done`.
pub(super) fn is_status_row(rows: &[String], row: u16) -> bool {
    rows.get(row as usize)
        .is_some_and(|text| super::markdown::plain_rendered(text).starts_with("\u{273b} "))
}

fn highlight_row(text: &str, left: usize, right: usize) -> String {
    let plain = super::markdown::plain_rendered(text);
    let mut rendered = String::from("\x1b[0m");
    let mut cell = 0;
    let mut inverse = false;
    for ch in plain.chars() {
        let width = ch.width().unwrap_or(0);
        let selected = if width == 0 {
            inverse
        } else {
            cell + width > left && cell < right
        };
        if selected != inverse {
            rendered.push_str(if selected { "\x1b[7m" } else { "\x1b[27m" });
            inverse = selected;
        }
        rendered.push(ch);
        cell += width;
    }
    rendered.push_str("\x1b[0m");
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_selection_accounts_for_pinned_header_in_hit_testing_and_painting() {
        let mut selection = Selection::default();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let rows = (0..20).map(|n| format!("ROW_{n:02}")).collect::<Vec<_>>();
        selection.document_event(mouse, rows, 5, 6, 1).unwrap();
        assert_eq!(selection.start, Some((5, 0)));
        selection.end = (6, 6);
        let mut output = Vec::new();
        selection.paint_to(&mut output, true).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[2;1H"));
        assert!(!output.contains("\x1b[1;1H"));
        assert_eq!(selection.text(), "ROW_05\nROW_06");
    }
    #[test]
    fn promotion_uses_context_for_repeated_code_lines() {
        let mut selection = Selection {
            start: Some((1, 0)), end: (0, 0),
            rows: vec!["first function".into(), "}".into(), "first result".into()],
            ..Default::default()
        };
        let rows = ["prefix", "first function", "}", "first result", "second function", "}", "second result"]
            .map(str::to_owned).to_vec();
        assert_eq!(selection.promote(rows, 3, 0), Some(1));
        assert_eq!(selection.start, Some((2, 0)));
    }

    #[test]
    fn document_selection_spans_screens_in_both_directions() {
        let rows = (0..80)
            .map(|index| format!("ROW_{index:02} 中文"))
            .collect::<Vec<_>>();
        let mut selection = Selection {
            start: Some((65, 0)),
            end: (12, 0),
            rows,
            dragged: true,
            viewport: Some((10, 20)),
            ..Default::default()
        };
        let text = selection.text();
        assert!(text.starts_with("ROW_12 中文\nROW_13"));
        assert!(text.contains("ROW_40 中文"));
        let mut painted = Vec::new();
        selection.paint_to(&mut painted, true).unwrap();
        let painted = String::from_utf8(painted).unwrap();
        assert!(!painted.contains("ROW_40"));
        assert!(painted.contains("ROW_12"));
        selection.start = Some((12, 0));
        selection.end = (65, 0);
        assert_eq!(selection.text(), text);
    }

    #[test]
    fn resize_discards_stale_coordinates_without_repainting_old_rows() {
        let mut selection = Selection {
            start: Some((1, 0)),
            end: (1, 20),
            rows: vec!["old".into(); 3],
            held: true,
            ..Default::default()
        };
        selection.discard();
        assert!(!selection.active() && !selection.retained());
        let mut output = Vec::new();
        selection.paint_to(&mut output, false).unwrap();
        assert!(!String::from_utf8(output).unwrap().contains("old"));
    }
    #[test]
    fn highlight_is_applied_before_text_without_repainting_the_row_twice() {
        assert_eq!(
            highlight_row("abcdef", 0, 3),
            "\x1b[0m\x1b[7mabc\x1b[27mdef\x1b[0m"
        );
        let row = highlight_row("a中文z", 2, 4);
        assert_eq!(super::super::markdown::plain_rendered(&row), "a中文z");
        assert!(row.contains("\x1b[7m中文\x1b[27mz"));
    }
    #[test]
    fn selection_repaints_only_changed_rows() {
        let mut selection = Selection {
            start: Some((1, 0)),
            end: (1, 2),
            rows: vec![
                "untouched".into(),
                "selected".into(),
                "also untouched".into(),
            ],
            ..Default::default()
        };
        let mut output = Vec::new();
        selection.paint_to(&mut output, true).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("untouched"));
        assert!(super::super::markdown::plain_rendered(&text).contains("selected"));
        let mut output = Vec::new();
        selection.paint_to(&mut output, true).unwrap();
        assert!(!String::from_utf8(output).unwrap().contains("selected"));
        selection.end = (1, 1);
        let mut output = Vec::new();
        selection.paint_to(&mut output, true).unwrap();
        assert!(
            super::super::markdown::plain_rendered(&String::from_utf8(output).unwrap())
                .contains("selected")
        );
    }

    #[test]
    fn reverse_selection_uses_display_cells_and_preserves_unicode() {
        let selection = Selection {
            start: Some((1, 3)),
            end: (0, 1),
            rows: vec!["a中文z".into(), "xyz123".into()],
            dragged: true,
            held: false,
            painted: Default::default(),
            viewport: None,
            viewport_offset: 0,
        };
        assert_eq!(selection.text(), "中文z\nxyz");
    }
    #[test]
    fn selection_does_not_erase_untracked_history() {
        let selection = Selection {
            start: Some((0, 0)),
            end: (1, 4),
            rows: vec![String::new(), "known history".into()],
            dragged: true,
            held: false,
            painted: Default::default(),
            viewport: None,
            viewport_offset: 0,
        };
        let mut output = Vec::new();
        selection.paint_to(&mut output, true).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("\x1b[1;1H"));
        assert!(super::super::markdown::plain_rendered(&output).contains("known history"));
    }

    #[test]
    fn released_selection_keeps_its_bounds_without_holding_input() {
        let selection = Selection {
            start: Some((0, 0)),
            end: (0, 4),
            rows: vec!["text".into()],
            dragged: true,
            held: false,
            painted: Default::default(),
            viewport: None,
            viewport_offset: 0,
        };
        assert!(selection.retained());
        assert!(!selection.active());
        assert_eq!(selection.text(), "text");
    }

    #[test]
    fn passive_motion_preserves_released_selection_and_suppresses_redraw() {
        let mut selection = Selection {
            start: Some((0, 0)),
            end: (0, 4),
            rows: vec!["text".into()],
            dragged: true,
            held: false,
            painted: Default::default(),
            viewport: None,
            viewport_offset: 0,
        };
        for column in [5, 20, 3] {
            assert!(selection.event(MouseEvent {
                kind: MouseEventKind::Moved, column, row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }, vec!["new streamed output".into()]).unwrap().is_none());
            assert!(selection.retained());
            assert_eq!(selection.text(), "text");
        }
    }

    #[test]
    fn click_waits_for_release() {
        let mut selection = Selection::default();
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(selection.event(event, vec![]).unwrap().is_none());
        assert!(selection
            .event(
                MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    ..event
                },
                vec![]
            )
            .unwrap()
            .is_some());
    }
}
