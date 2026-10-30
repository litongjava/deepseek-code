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
    pub fn draw(&mut self, width: usize, height: usize) -> io::Result<()> {
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
        self.height = rows.len();
        let mut out = io::stderr().lock();
        queue!(out, SavePosition)?;
        for (row, text) in rows.iter().enumerate() {
            queue!(
                out,
                MoveTo(0, row as u16),
                Print("\x1b[0m\x1b[2K"),
                Print(text)
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
