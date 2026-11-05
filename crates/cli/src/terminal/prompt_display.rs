//! Shared rendering for typed and positional prompts. Only this renderer emits
//! terminal controls; prompt contents cannot inject escape sequences.
use crossterm::style::{ResetColor, SetForegroundColor};
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

/// Emit exactly the same rows retained for selection and history navigation.
pub(crate) fn write_recorded(
    output: &mut impl Write,
    history: &mut super::scrollback::Scrollback,
    text: &str,
    width: usize,
    raw: bool,
) -> io::Result<()> {
    let newline = if raw { "\r\n" } else { "\n" };
    if crate::background_commands::is_notification(text) {
        write!(output, "{newline}")?;
        if history.needs_separator() {
            history.push(String::new(), None);
        }
        for row in history.record_prompt(text) {
            write!(output, "{row}{newline}")?;
        }
        return Ok(());
    }
    let rule = "─".repeat(width.max(4));
    for row in [String::new(), rule.clone()] {
        write!(output, "{row}{newline}")?;
        history.push(row, None);
    }
    for row in history.record_prompt(text) {
        write!(output, "{row}{newline}")?;
    }
    write!(output, "{rule}{newline}")?;
    history.push(rule, None);
    Ok(())
}

pub(super) fn viewport(len: usize, cursor_row: usize, height: usize) -> std::ops::Range<usize> {
    let shown = len.min(height.max(1));
    let first = cursor_row.saturating_sub(shown.saturating_sub(1)).min(len - shown);
    first..first + shown
}

pub fn write_submitted(
    output: &mut impl Write,
    prompt: &str,
    text: &str,
    width: usize,
    color: bool,
    raw: bool,
) -> io::Result<()> {
    let newline = if raw { "\r\n" } else { "\n" };
    if let Some(summary) = crate::background_commands::notification_text(text) {
        write!(output, "{newline}")?;
        for line in summary.lines() {
            write!(output, "{line}{newline}")?;
        }
        return Ok(());
    }

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
            // Sent text follows the terminal's contrast settings, including e-ink themes.
            write!(output, "\x1b[0;39;49m{line}\x1b[0m")?;
        } else {
            write!(output, "{line}")?;
        }
        write!(output, "{newline}")?;
    }
    write_rule(output)?;
    // Attached images, as in the reference CLI: `  ⎿  [Image #1]`.
    for number in crate::images::placeholder_numbers(text) {
        write!(output, "  \u{23bf}  [Image #{number}]{newline}")?;
    }
    write!(output, "{newline}")
}

fn wrapped_lines(prompt: &str, text: &str, width: usize) -> Vec<String> {
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
    let chars: Vec<char> = clean(text).chars().collect();
    layout(&prompt, &chars, width.max(4)).0
}

/// Rows of a prompt followed by its text, wrapped at the last space that fits
/// as in the reference CLI; a word longer than a row still breaks. Continuation
/// rows indent two cells and no row exceeds `limit` cells. A space at a break
/// is not drawn. Also returns the (row, cell) of every cursor position,
/// `0..=chars.len()`.
pub fn layout(prompt: &str, chars: &[char], limit: usize) -> (Vec<String>, Vec<(usize, usize)>) {
    let cells = |ch: char| if ch == '\t' { 4 } else { ch.width().unwrap_or(0) };
    let mut positions = Vec::with_capacity(chars.len() + 1);
    let mut hidden = vec![false; chars.len()];
    let mut row = 0;
    let mut cell: usize = prompt.chars().map(cells).sum();
    let mut space: Option<usize> = None;
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '\n' {
            positions.push((row, cell));
            hidden[index] = true;
            row += 1;
            cell = 2;
            space = None;
            continue;
        }
        let size = cells(ch);
        if cell + size > limit {
            if ch == ' ' {
                positions.push((row, cell));
                hidden[index] = true;
                row += 1;
                cell = 2;
                space = None;
                continue;
            }
            row += 1;
            cell = 2;
            // Carry the partial word after the row's last space down with it.
            if let Some(last) = space.filter(|last| last + 1 < index) {
                for moved in last + 1..index {
                    positions[moved] = (row, cell);
                    cell += cells(chars[moved]);
                }
            }
            space = None;
        }
        positions.push((row, cell));
        cell += size;
        if ch == ' ' {
            space = Some(index);
        }
    }
    positions.push((row, cell));
    let mut rows = vec![prompt.to_owned()];
    rows.resize(row + 1, "  ".to_owned());
    for (index, &ch) in chars.iter().enumerate() {
        if !hidden[index] {
            rows[positions[index].0].push_str(&if ch == '\t' { "    ".to_owned() } else { ch.to_string() });
        }
    }
    (rows, positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_prompt_matches_emitted_rows_and_retains_full_text() {
        let mut history = super::super::scrollback::Scrollback::default();
        let mut output = Vec::new();
        let text = "first\nsecond\nthird\nLAST_LINE";
        write_recorded(&mut output, &mut history, text, 40, true).unwrap();
        let emitted = String::from_utf8(output).unwrap();
        assert_eq!(emitted, format!("{}\r\n", history.all().join("\r\n")));
        assert!(!emitted.contains("LAST_LINE"));
        history.expand_prompt(text, 40);
        assert!(history.all().iter().any(|row| row.contains("LAST_LINE")));
    }

    #[test]
    fn submitted_text_uses_terminal_defaults_without_dim_or_fixed_background() {
        let mut output = Vec::new();
        write_submitted(&mut output, "> ", "EINK_USER 中文 123", 40, true, true).unwrap();
        let output = String::from_utf8(output).unwrap();
        let row = output.lines().find(|row| row.contains("EINK_USER")).unwrap();
        assert_eq!(row, "\x1b[0;39;49m> EINK_USER 中文 123\x1b[0m");
    }

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

    #[test]
    fn wraps_at_word_boundaries_and_tracks_every_cursor_position() {
        let chars: Vec<char> = "make divide raise ValueError".chars().collect();
        let (rows, positions) = layout("> ", &chars, 16);
        let trimmed = |rows: &[String]| rows.iter().map(|row| row.trim_end().to_owned()).collect::<Vec<_>>();
        assert_eq!(trimmed(&rows), ["> make divide", "  raise", "  ValueError"]);
        assert_eq!(positions[0], (0, 2));
        assert_eq!(positions[12], (1, 2));
        assert_eq!(positions[chars.len()], (2, 12));
        assert!(rows.iter().all(|row| row.chars().count() <= 16));
        let long: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
        let (rows, _) = layout("> ", &long, 10);
        assert_eq!(rows, ["> abcdefgh", "  ijklmnop", "  qrstuvwx", "  yz"]);
        let wide: Vec<char> = "你好 世界".chars().collect();
        let (rows, positions) = layout("> ", &wide, 7);
        assert_eq!(trimmed(&rows), ["> 你好", "  世界"]);
        assert_eq!(positions[2], (0, 6));
        let lines: Vec<char> = "a\nb".chars().collect();
        assert_eq!(layout("> ", &lines, 10).0, ["> a", "  b"]);
    }
}
