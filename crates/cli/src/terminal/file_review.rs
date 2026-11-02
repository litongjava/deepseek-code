//! Read-only file review, shared by the live turn and idle prompt.
use super::change_view::{self, Line};
use crossterm::{
    cursor::{Hide, MoveTo, RestorePosition, SavePosition, Show},
    event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType},
};
use std::{
    io::{self, Write},
    sync::Arc,
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub(super) struct File {
    pub label: String,
    pub summary: Line,
    pub details: Vec<Line>,
}

#[derive(Clone)]
pub(super) struct Review {
    pub files: Arc<Vec<File>>,
    pub selected: usize,
}

struct View {
    review: Review,
    top: usize,
}

impl View {
    fn select(&mut self, index: usize) {
        if index < self.review.files.len() && index != self.review.selected {
            self.review.selected = index;
            self.top = 0;
        }
    }

    fn layout(&self, width: usize, height: usize) -> (usize, usize, usize) {
        let sidebar = (width / 3).clamp(12, 38).min(width.saturating_sub(12));
        let page = height.saturating_sub(3).max(1);
        let first = self.review.selected.saturating_sub(page.saturating_sub(1));
        (sidebar, page, first)
    }

    fn render(&mut self, width: usize, height: usize, color: bool) -> Vec<String> {
        let (sidebar, page, first) = self.layout(width, height);
        let file = &self.review.files[self.review.selected];
        let detail_width = width.saturating_sub(sidebar + 3).max(1);
        let details: Vec<String> = file
            .details
            .iter()
            .flat_map(|line| {
                super::markdown::wrap_styled(
                    &change_view::render(line, usize::MAX, color),
                    "",
                    "",
                    detail_width,
                )
            })
            .collect();
        self.top = self.top.min(details.len().saturating_sub(page));
        let clip = |text: &str| change_view::render(&change_view::body(text), width, color);
        let mut rows = vec![clip(&format!("File changes | {}", file.label))];
        rows.push(clip("Files | Before (- red) / After (+ green)"));
        for row in 0..page {
            let label = self
                .review
                .files
                .get(first + row)
                .map(|file| {
                    format!(
                        "{} {}",
                        if first + row == self.review.selected {
                            '>'
                        } else {
                            ' '
                        },
                        file.label
                    )
                })
                .unwrap_or_default();
            let label = change_view::render(&change_view::body(&label), sidebar, false);
            let padding = sidebar.saturating_sub(UnicodeWidthStr::width(label.as_str()));
            let detail = details
                .get(self.top + row)
                .map(String::as_str)
                .unwrap_or("");
            if width < 15 {
                rows.push(clip(&super::markdown::plain_rendered(detail)));
            } else {
                rows.push(format!("{label}{} | {detail}", " ".repeat(padding)));
            }
        }
        rows.push(clip(
            "Esc close | Tab/Shift+Tab files | Up/Down/PgUp/PgDn scroll",
        ));
        rows.truncate(height);
        rows
    }
}

/// Paint over the current surface, including an already-open history view.
/// Restore it on every exit, without nesting alternate-screen transitions.
pub(super) fn show(
    review: Review,
    background: &[String],
    next: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> io::Result<()> {
    if review.files.is_empty() || review.selected >= review.files.len() {
        return Ok(());
    }
    let mut out = io::stderr();
    execute!(out, SavePosition, Hide)?;
    let mut view = View { review, top: 0 };
    let result = (|| {
        let mut size = terminal::size().unwrap_or((80, 24));
        let mut selection = super::mouse_selection::Selection::default();
        let mut redraw = true;
        let mut rows = Vec::new();
        loop {
            let width = size.0.saturating_sub(1) as usize;
            let height = size.1 as usize;
            if redraw {
                rows = view.render(width, height, std::env::var_os("NO_COLOR").is_none());
                paint(&mut out, &rows, size)?;
                redraw = false;
            }
            let Some(event) = next()? else {
                continue;
            };
            match event {
                Event::Resize(w, h) => {
                    selection.clear()?;
                    size = (w, h);
                }
                Event::Mouse(mouse) => {
                    let Some(mouse) = selection.event(mouse, rows.clone())? else {
                        continue;
                    };
                    let (sidebar, page, first) = view.layout(width, height);
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left)
                            if (mouse.column as usize) < sidebar
                                && mouse.row >= 2
                                && (mouse.row as usize) < page + 2 =>
                        {
                            view.select(first + mouse.row as usize - 2);
                        }
                        MouseEventKind::ScrollUp => view.top = view.top.saturating_sub(3),
                        MouseEventKind::ScrollDown => view.top = view.top.saturating_add(3),
                        _ => continue,
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    selection.clear()?;
                    let page = height.saturating_sub(3).max(1);
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Tab | KeyCode::Right => {
                            view.select((view.review.selected + 1) % view.review.files.len())
                        }
                        KeyCode::BackTab | KeyCode::Left => view.select(
                            (view.review.selected + view.review.files.len() - 1)
                                % view.review.files.len(),
                        ),
                        KeyCode::Up | KeyCode::Char('k') => view.top = view.top.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => view.top = view.top.saturating_add(1),
                        KeyCode::PageUp => view.top = view.top.saturating_sub(page),
                        KeyCode::PageDown | KeyCode::Char(' ') => {
                            view.top = view.top.saturating_add(page)
                        }
                        KeyCode::Home => view.top = 0,
                        KeyCode::End => view.top = usize::MAX,
                        _ => continue,
                    }
                }
                _ => continue,
            }
            redraw = true;
        }
        selection.clear()
    })();
    let size = terminal::size().unwrap_or((80, 24));
    let background = &background[background.len().saturating_sub(size.1 as usize)..];
    let restored =
        paint(&mut out, background, size).and_then(|_| execute!(out, RestorePosition, Show));
    result.and(restored)
}

fn paint(out: &mut impl Write, rows: &[String], size: (u16, u16)) -> io::Result<()> {
    for row in 0..size.1 {
        queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        if let Some(text) = rows.get(row as usize) {
            let clipped = super::markdown::wrap_styled(
                text,
                "",
                "",
                size.0.saturating_sub(1).max(1) as usize,
            );
            queue!(
                out,
                Print(clipped.first().map(String::as_str).unwrap_or(""))
            )?;
        }
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            review: Review {
                files: Arc::new(vec![
                    File {
                        label: "src/中文.rs".into(),
                        summary: change_view::body("first"),
                        details: vec![
                            change_view::body("- old value"),
                            change_view::body("+ new value"),
                        ],
                    },
                    File {
                        label: "new.txt".into(),
                        summary: change_view::body("second"),
                        details: (0..50)
                            .map(|i| change_view::body(&format!("+ line {i}")))
                            .collect(),
                    },
                ]),
                selected: 0,
            },
            top: 0,
        }
    }

    #[test]
    fn selected_file_shows_before_and_after_on_the_right() {
        let mut view = view();
        let rows = view.render(100, 20, false);
        assert!(rows[2].contains("> src/中文.rs"));
        assert!(rows[2].find("- old value").unwrap() > 30);
        assert!(rows[3].contains("+ new value"));
        view.select(1);
        view.top = usize::MAX;
        assert!(view
            .render(100, 20, false)
            .iter()
            .any(|row| row.contains("line 49")));
        view.select(0);
        assert_eq!(view.top, 0);
    }

    #[test]
    fn unicode_and_narrow_resize_stay_within_the_viewport() {
        for width in [1, 15, 40, 80] {
            let rows = view().render(width, 10, false);
            // Tiny terminals can only retain a clipped content column.
            if width >= 15 {
                assert!(rows
                    .iter()
                    .all(|row| UnicodeWidthStr::width(row.as_str()) <= width));
            }
            assert!(rows.len() <= 10);
        }
    }
}
