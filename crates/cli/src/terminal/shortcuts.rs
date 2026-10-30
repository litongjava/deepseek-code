//! Bottom-anchored shortcut popup shared by every interactive view.
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    queue,
    style::Print,
    terminal,
};
use std::io::{self, Write};

#[derive(Clone, Copy)]
pub(crate) enum Context {
    Input,
    Running,
    Pager,
    Picker,
    Text,
    Details,
    Answer,
}

pub(crate) fn requested(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Release
        && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && (key.code == KeyCode::Char('?') || (key.code == KeyCode::Char('/') && key.modifiers.contains(KeyModifiers::SHIFT))))
}

fn rows(context: Context) -> Vec<&'static str> {
    let mut rows = vec![
        " Keyboard shortcuts",
        " Shift+/ (?)  Show or close this popup",
    ];
    rows.extend_from_slice(match context {
        Context::Input => &[
            " Enter  Send message     Shift+Enter / Alt+Enter  New line",
            " Tab  Complete command     Shift+Tab  Cycle permissions",
            " Ctrl+O  Detailed transcript     PageUp/PageDown  Scroll messages",
            " Ctrl+R  Search input history     Ctrl+G  External editor",
            " Ctrl+U  Delete to start     Ctrl+Y  Paste deleted text",
            " Alt+P  Model picker     Ctrl+T  Toggle tasks",
            " Mouse wheel  Scroll     Drag  Select and copy text",
        ][..],
        Context::Running => &[
            " Esc  Interrupt     Shift+Tab  Cycle permissions",
            " Enter  Queue message or submit answer",
            " Ctrl+O  Detailed transcript     PageUp/PageDown  Scroll messages",
            " Ctrl+B  Background task     Ctrl+T  Toggle tasks",
            " Mouse wheel  Scroll     Drag  Select and copy text",
        ][..],
        Context::Pager => &[
            " Up/Down, j/k, mouse wheel  Scroll",
            " PageUp/PageDown, b/Space  Page     g/G  Top/bottom",
            " /  Search     n/N  Next/previous match",
            " { / }  Previous/next prompt     v  Open in editor",
            " Drag  Select and copy text",
            " Esc / q / Ctrl+O  Close page",
        ][..],
        Context::Picker => &[
            " Up/Down  Select     Enter  Confirm     Esc  Cancel",
            " Type  Filter available entries     Backspace  Delete",
        ][..],
        Context::Text => &[
            " Enter  Save     Esc  Cancel     Backspace  Delete",
            " Ctrl+U  Clear text",
        ][..],
        Context::Answer => &[
            " Enter  Submit answer     Ctrl+C  Cancel",
            " Backspace  Delete",
        ][..],
        Context::Details => &[
            " Left/Right  Switch tabs (when available)",
            " Esc / Enter / Space  Close",
            " Additional actions are shown in the page footer",
        ][..],
    });
    rows.push(" Esc / ?  Close     Up/Down or mouse wheel  Scroll shortcuts");
    rows
}

pub(crate) fn show(
    context: Context,
    background: &[String],
    next: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> io::Result<()> {
    let mut out = io::stderr();
    run(context, background, next, &mut out)
}

fn run(
    context: Context,
    background: &[String],
    next: &mut dyn FnMut() -> io::Result<Option<Event>>,
    out: &mut impl Write,
) -> io::Result<()> {
    let content = rows(context);
    let mut offset = 0usize;
    let mut touched = usize::MAX;
    queue!(out, SavePosition)?;
    let result = (|| loop {
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let height = height.max(1) as usize;
        let count = content.len().min(height);
        offset = offset.min(content.len().saturating_sub(count));
        let top = height - count;
        touched = touched.min(top);
        for (row, text) in content.iter().skip(offset).take(count).enumerate() {
            let text = super::input::editor::clipped(text, width.saturating_sub(1) as usize);
            queue!(
                out,
                MoveTo(0, (top + row) as u16),
                Print("\x1b[0m\x1b[2K"),
                Print(text)
            )?;
        }
        out.flush()?;
        loop {
            let Some(event) = next()? else {
                continue;
            };
            if requested(&event) {
                return Ok(());
            }
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Esc | KeyCode::Enter => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Up | KeyCode::PageUp => offset = offset.saturating_sub(1),
                    KeyCode::Down | KeyCode::PageDown => offset = offset.saturating_add(1),
                    _ => continue,
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => offset = offset.saturating_sub(1),
                    MouseEventKind::ScrollDown => offset = offset.saturating_add(1),
                    _ => continue,
                },
                Event::Resize(_, _) => {}
                _ => continue,
            }
            break;
        }
    })();
    let height = terminal::size().unwrap_or((80, 24)).1 as usize;
    for row in touched..height {
        queue!(
            out,
            MoveTo(0, row as u16),
            Print("\x1b[0m\x1b[2K"),
            Print(background.get(row).map(String::as_str).unwrap_or_default())
        )?;
    }
    queue!(out, RestorePosition)?;
    out.flush()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    #[test]
    fn question_shortcuts_do_not_capture_pastes_or_modified_commands() {
        for (code, modifiers) in [
            (KeyCode::Char('?'), KeyModifiers::NONE),
            (KeyCode::Char('?'), KeyModifiers::SHIFT),
            (KeyCode::Char('/'), KeyModifiers::SHIFT),
        ] {
            assert!(requested(&Event::Key(KeyEvent::new(code, modifiers))));
        }
        assert!(!requested(&Event::Paste("why?".into())));
        assert!(!requested(&Event::Key(KeyEvent::new(
            KeyCode::Char('?'),
            KeyModifiers::ALT
        ))));
    }
    #[test]
    fn closing_popup_restores_background_without_alternate_screen_switch() {
        let background = vec!["ORIGINAL_ROW".into(); 200];
        let mut out = Vec::new();
        run(
            Context::Input,
            &background,
            &mut || {
                Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                ))))
            },
            &mut out,
        )
        .unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Keyboard shortcuts"));
        assert!(output.contains("ORIGINAL_ROW"));
        assert!(!output.contains("1049"));
        assert!(output.ends_with("\x1b8"));
    }
}
