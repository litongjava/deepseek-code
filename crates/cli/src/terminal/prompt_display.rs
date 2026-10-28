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
