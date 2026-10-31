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

pub(crate) fn requested_in_input(event: &Event, empty: bool) -> bool {
    empty && requested(event)
}

fn rows(context: Context) -> Vec<String> {
    let mut entries = vec!["Keyboard shortcuts", "?\tShow shortcuts (empty input only)"];
    entries.extend_from_slice(match context {
        Context::Input => &["Enter\tSend message", "Shift+Enter / Alt+Enter\tNew line", "Tab\tComplete command", "Shift+Tab\tCycle permissions", "Ctrl+O\tDetailed transcript", "PageUp / PageDown\tScroll messages", "Ctrl+R\tSearch input history", "Ctrl+G\tExternal editor", "Ctrl+U\tDelete to start", "Ctrl+Y\tPaste deleted text", "Alt+P\tModel picker", "Ctrl+T\tToggle tasks", "Mouse wheel\tScroll", "Drag\tSelect and copy text"][..],
        Context::Running => &["Tab\tQueue message", "Enter\tSteer message / submit answer", "Esc\tInterrupt", "Shift+Tab\tCycle permissions", "Ctrl+O\tDetailed transcript", "Ctrl+B\tBackground task", "Ctrl+T\tToggle tasks", "Mouse wheel\tScroll", "Drag\tSelect and copy text"][..],
        Context::Pager => &["Up / Down / j / k\tScroll", "PageUp / PageDown\tPage", "g / G\tTop / bottom", "/\tSearch", "n / N\tNext / previous match", "v\tOpen in editor", "Esc / q / Ctrl+O\tClose page"][..],
        Context::Picker => &["Up / Down\tSelect", "Enter\tConfirm", "Type\tSearch", "Tab / Ctrl+A\tcwd / all (resume)", "Esc\tCancel"][..],
        Context::Text => &["Enter\tSave", "Esc\tCancel", "Backspace\tDelete", "Ctrl+U\tClear text"][..],
        Context::Answer => &["Enter\tSubmit answer", "Ctrl+C\tCancel", "Backspace\tDelete"][..],
        Context::Details => &["Left / Right\tSwitch tabs", "Esc / Enter / Space\tClose"][..],
    });
    entries.push("Esc / ?\tClose shortcuts");
    entries.into_iter().map(|row| {
        match row.split_once('\t') {
            Some((key, description)) => format!(" {key:<28}{description}"),
            None => format!(" {row}"),
        }
    }).collect()
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
    fn question_mark_opens_only_empty_input_and_columns_align() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(requested_in_input(&event, true));
        assert!(!requested_in_input(&event, false));
        let rows = rows(Context::Running);
        let tab = rows.iter().find(|row| row.contains("Queue message")).unwrap();
        let enter = rows.iter().find(|row| row.contains("Steer message")).unwrap();
        assert_eq!(tab.find("Queue message"), enter.find("Steer message"));
    }

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
