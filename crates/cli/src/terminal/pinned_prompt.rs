use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    queue,
    style::Print,
};
use std::io::{self, Write};
#[derive(Default)]
pub(super) struct PinnedPrompt {
    pub text: String,
    pub expanded: bool,
    pub height: usize,
}
impl PinnedPrompt {
    pub fn draw(&mut self, width: usize, height: usize, background: &[String]) -> io::Result<()> {
        let mut frame = Vec::new();
        self.draw_to(&mut frame, width, height, background)?;
        if frame.is_empty() { return Ok(()); }
        let mut out = io::stderr().lock();
        out.write_all(b"\x1b[?2026h")?;
        out.write_all(&frame)?;
        out.write_all(b"\x1b[?2026l")?;
        out.flush()
    }
    fn draw_to(&mut self, out: &mut impl Write, width: usize, height: usize, background: &[String]) -> io::Result<()> {
        if self.text.is_empty() {
            return Ok(());
        }
        let label = format!("❯ {}", self.text.replace('\r', "").replace('\n', " "));
        let rows = if self.expanded {
            let mut rows = Vec::new();
            for line in self.text.lines() {
                rows.extend(super::markdown::wrap_styled(line, "", "", width));
            }
            rows.truncate(height.saturating_sub(6).max(1));
            rows.push("(click to collapse)".into());
            rows
        } else {
            let mut rows = super::markdown::wrap_styled(&label, "", "", width.saturating_sub(2));
            let mut first = rows.remove(0);
            if !rows.is_empty() {
                first.push('…');
            }
            vec![first]
        };
        let previous_height = self.height;
        self.height = rows.len();
        queue!(out, SavePosition)?;
        // Only restore rows exposed when the overlay shrinks. Repainting the
        // background beneath an unchanged overlay makes it visibly flash.
        for row in self.height..previous_height.min(height) {
            queue!(out, MoveTo(0, row as u16), Print("\x1b[0m"),
                Print(background.get(row).map(String::as_str).unwrap_or_default()),
                Print("\x1b[K"))?;
        }
        for (row, text) in rows.iter().enumerate() {
            queue!(
                out,
                MoveTo(0, row as u16),
                Print("\x1b[0m"),
                Print(text),
                Print("\x1b[K")
            )?;
        }
        queue!(out, RestorePosition)?;
        out.flush()
    }
    pub fn click(&mut self, row: u16) -> bool {
        if !self.text.is_empty() && (row as usize) < self.height {
            self.expanded = !self.expanded;
            true
        } else {
            false
        }
    }
}

static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
pub(super) fn remember(text: &str) { *LAST.lock().unwrap_or_else(|e| e.into_inner()) = text.to_owned(); }
pub(super) fn latest() -> String { LAST.lock().unwrap_or_else(|e| e.into_inner()).clone() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_never_exposes_background_or_clears_before_text() {
        let mut prompt = PinnedPrompt { text: "Pinned request".into(), ..Default::default() };
        let background = vec!["HIDDEN_BODY".into()];
        prompt.draw_to(&mut Vec::new(), 80, 24, &background).unwrap();
        let mut output = Vec::new();
        prompt.draw_to(&mut output, 80, 24, &background).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("HIDDEN_BODY"));
        assert!(!output.contains("\x1b[2K"));
        assert!(output.contains("Pinned request\x1b[K"));
    }

    #[test]
    fn collapsing_restores_messages_under_expanded_prompt() {
        let mut prompt = PinnedPrompt { text: "first\nsecond\nthird".into(), expanded: true, height: 0 };
        let background = vec!["header".into(), "MESSAGE_ONE".into(), "MESSAGE_TWO".into(), "MESSAGE_THREE".into()];
        prompt.draw_to(&mut Vec::new(), 80, 24, &background).unwrap();
        assert!(prompt.click(1));
        let mut output = Vec::new();
        prompt.draw_to(&mut output, 80, 24, &background).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(prompt.height, 1);
        for message in &background[1..] { assert!(output.contains(message)); }
    }
}
