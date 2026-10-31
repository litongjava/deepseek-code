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
pub(super) struct Selection {
    start: Option<(u16, u16)>,
    end: (u16, u16),
    rows: Vec<String>,
    dragged: bool,
    held: bool,
    painted: std::cell::RefCell<Vec<String>>,
}
impl Selection {
    pub fn active(&self) -> bool {
        self.held
    }
    pub fn retained(&self) -> bool {
        self.start.is_some()
    }
    pub fn clear(&mut self) -> io::Result<()> {
        if self.start.is_some() {
            self.paint(false)?;
        }
        self.start = None;
        self.held = false;
        Ok(())
    }
    /// Returns a click only after release without motion. A drag copies text.
    pub fn event(
        &mut self,
        mouse: MouseEvent,
        rows: Vec<String>,
    ) -> io::Result<Option<MouseEvent>> {
        let point = (mouse.row, mouse.column);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.clear()?;
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
    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
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
        for (row, text) in self.rows.iter().enumerate() {
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
                MoveTo(0, row as u16),
                Print(rendered),
                Print("\x1b[0m\x1b[K")
            )?;
        }
        queue!(out, RestorePosition)?;
        out.write_all(b"\x1b[?2026l")?;
        Ok(())
    }
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
        };
        assert!(selection.retained());
        assert!(!selection.active());
        assert_eq!(selection.text(), "text");
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
