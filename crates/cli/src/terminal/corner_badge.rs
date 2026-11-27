//! A right-aligned transient badge at the bottom-right of the transcript.
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    queue,
    style::{Attribute, Print, SetAttribute},
};
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

/// The shortcut hint follows the application language (English or Chinese).
pub fn copy_hint() -> String {
    deepseek_code_core::config::ui_language::text("Press c to copy", "按 c 复制").to_owned()
}

/// The copy result follows the application language (English or Chinese).
pub fn copy_result(characters: usize, copied: bool) -> String {
    if !copied {
        return deepseek_code_core::config::ui_language::text("Copy failed", "复制失败").to_owned();
    }
    let english = if characters == 1 {
        "Copied 1 character".to_owned()
    } else {
        format!("Copied {characters} characters")
    };
    let chinese = format!("已复制 {characters} 个字符");
    deepseek_code_core::config::ui_language::text(&english, &chinese).to_owned()
}

#[derive(Default)]
pub(super) struct CornerBadge {
    row: Option<u16>,
    column: u16,
    width: usize,
    text: String,
}

impl CornerBadge {
    /// Paints `text` as a reverse-video badge one cell from the right edge.
    /// Repainting the same cells is intentional: a frame may have overwritten
    /// the row since the last loop iteration.
    pub fn show(&mut self, row: u16, text: &str) -> io::Result<()> {
        let (columns, _) = crate::terminal::size();
        let width = UnicodeWidthStr::width(text);
        if width + 1 > columns as usize {
            return self.clear();
        }
        let column = (columns as usize - width - 1) as u16;
        if self.row != Some(row) || self.column != column || self.text != text {
            self.clear()?;
        }
        let mut out = crate::terminal_output::stderr_locked();
        queue!(
            out,
            SavePosition,
            MoveTo(column, row),
            SetAttribute(Attribute::Reset),
            Print(text),
            SetAttribute(Attribute::Reset),
            RestorePosition
        )?;
        out.flush()?;
        self.row = Some(row);
        self.column = column;
        self.width = width;
        self.text = text.to_owned();
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        let Some(row) = self.row.take() else {
            return Ok(());
        };
        let mut out = crate::terminal_output::stderr_locked();
        queue!(
            out,
            SavePosition,
            MoveTo(self.column, row),
            Print(" ".repeat(self.width)),
            RestorePosition
        )?;
        out.flush()?;
        self.width = 0;
        self.text.clear();
        Ok(())
    }
}
