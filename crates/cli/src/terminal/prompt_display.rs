//! Shared rendering for typed and positional prompts. Only this renderer emits
//! terminal controls; prompt contents cannot inject escape sequences.
use crossterm::style::{ResetColor, SetBackgroundColor, SetForegroundColor};
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

pub fn write_submitted(
    output: &mut impl Write,
    prompt: &str,
    text: &str,
    width: usize,
    color: bool,
    raw: bool,
) -> io::Result<()> {
    let newline = if raw { "\r\n" } else { "\n" };
    if text.is_empty() {
        return write!(output, "{newline}");
    }
    write!(output, "{newline}")?;
    // The submitted prompt keeps the input box's rules, so it stays easy to
    // find in the transcript.
    let rule = "\u{2500}".repeat(width.max(4));
    let write_rule = |output: &mut dyn Write| -> io::Result<()> {
        if color {
            write!(output, "{}{rule}{}", SetForegroundColor(super::style::border()), ResetColor)?;
        } else {
            write!(output, "{rule}")?;
        }
        write!(output, "{newline}")
    };
    write_rule(output)?;
    for line in wrapped_lines(prompt, text, width) {
        if color {
            let cells: usize = line.chars().map(|ch| ch.width().unwrap_or(0)).sum();
            write!(
                output,
                "{}{}{}{}{}",
                SetBackgroundColor(super::style::input_background()),
                SetForegroundColor(super::style::input_foreground()),
                line,
                " ".repeat(width.max(4).saturating_sub(cells)),
                ResetColor
            )?;
        } else {
            write!(output, "{line}")?;
        }
        write!(output, "{newline}")?;
    }
    write_rule(output)?;
    write!(output, "{newline}")
}

fn wrapped_lines(prompt: &str, text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let clean = |value: &str| {
        value
            .replace("\r\n", "\n")
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect::<String>()
    };
    let prompt: String = clean(prompt)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(2)
        .collect();
    let mut row = prompt;
    let mut cells: usize = row.chars().map(|ch| ch.width().unwrap_or(0)).sum();
    let mut rows = Vec::new();
    for ch in clean(text).replace('\t', "    ").chars() {
        let size = ch.width().unwrap_or(0);
        if ch == '\n' || cells + size > width {
            rows.push(row);
            row = "  ".into();
            cells = 2;
        }
        if ch != '\n' {
            row.push(ch);
            cells += size;
        }
    }
    rows.push(row);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submitted_prompts_keep_rules_wrap_unicode_and_cannot_emit_controls() {
        let mut output = Vec::new();
        write_submitted(&mut output, "> ", "你好世界\nnext\x1b[2J", 8, false, true).unwrap();
        let result = String::from_utf8(output).unwrap();
        let rule = "\u{2500}".repeat(8);
        assert_eq!(
            result,
            format!("\r\n{rule}\r\n> 你好世\r\n  界\r\n  next[2\r\n  J\r\n{rule}\r\n\r\n")
        );
        assert!(!result.contains('\x1b'));
        for line in result.lines() {
            assert!(
                line.chars()
                    .map(|ch| ch.width().unwrap_or(0))
                    .sum::<usize>()
                    <= 8
            );
        }
    }
}
