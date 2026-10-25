//! Line-by-line Markdown rendering for streamed answers: emphasis, inline
//! code, headings, quotes, rules, fenced code without fences, and tables
//! aligned once complete. Model text never supplies terminal escapes.
use unicode_width::UnicodeWidthStr;

const BOLD: (&str, &str) = ("\x1b[1m", "\x1b[22m");
const ITALIC: (&str, &str) = ("\x1b[3m", "\x1b[23m");
const CODE: (&str, &str) = ("\x1b[38;2;175;135;255m", "\x1b[39m");
const DIM: (&str, &str) = ("\x1b[2m", "\x1b[22m");

#[derive(Default)]
pub struct MarkdownStream {
    color: bool,
    code_fence: Option<String>,
    table: Vec<String>,
}

fn strip_controls(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control() || *ch == '\t').map(|ch| if ch == '\t' { ' ' } else { ch }).collect()
}

impl MarkdownStream {
    pub fn new(color: bool) -> Self {
        Self { color, ..Default::default() }
    }

    fn paint(&self, (open, close): (&str, &str), text: &str) -> String {
        if self.color {
            format!("{open}{text}{close}")
        } else {
            text.to_owned()
        }
    }

    /// Renders one complete line; tables are held until they end.
    pub fn line(&mut self, raw: &str) -> Vec<String> {
        let line = strip_controls(raw);
        let trimmed = line.trim_start();
        if let Some(fence) = &self.code_fence {
            if trimmed.starts_with(fence.as_str()) && trimmed.trim_start_matches(fence.chars().next().unwrap_or('`')).trim().is_empty() {
                self.code_fence = None;
                return Vec::new();
            }
            return vec![self.paint(CODE, &format!("  {line}"))];
        }
        let mut output = Vec::new();
        let is_table = trimmed.starts_with('|') && trimmed.trim_end().ends_with('|') && trimmed.len() > 1;
        if !is_table && !self.table.is_empty() {
            output.extend(self.flush_table());
        }
        if is_table {
            self.table.push(trimmed.trim_end().to_owned());
            return output;
        }
        for fence in ["```", "~~~"] {
            if trimmed.starts_with(fence) {
                let marker: String = trimmed.chars().take_while(|ch| *ch == fence.chars().next().unwrap()).collect();
                self.code_fence = Some(marker);
                return output;
            }
        }
        output.push(self.block(&line));
        output
    }

    /// Emits anything still held, such as a table at the end of the answer.
    pub fn finish(&mut self) -> Vec<String> {
        self.code_fence = None;
        self.flush_table()
    }

    fn block(&self, line: &str) -> String {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if !trimmed.is_empty() && trimmed.chars().all(|ch| matches!(ch, '-' | '*' | '_' | ' ')) && trimmed.chars().filter(|ch| !ch.is_whitespace()).count() >= 3 {
            return self.paint(DIM, &"\u{2500}".repeat(40));
        }
        let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            return self.paint(BOLD, &self.inline(trimmed[hashes..].trim()));
        }
        if let Some(quote) = trimmed.strip_prefix("> ").or_else(|| (trimmed == ">").then_some("")) {
            return format!("{indent}{} {}", self.paint(DIM, "\u{258e}"), self.paint(ITALIC, &self.inline(quote)));
        }
        format!("{indent}{}", self.inline(trimmed))
    }

    /// Bold, italic, inline code and links; unmatched markers stay literal.
    fn inline(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let find = |from: usize, marker: &[char]| -> Option<usize> {
            (from..=chars.len().saturating_sub(marker.len())).find(|&at| chars[at..at + marker.len()] == *marker)
        };
        let collect = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
        let mut output = String::new();
        let mut index = 0;
        while index < chars.len() {
            let ch = chars[index];
            let next = chars.get(index + 1).copied();
            if ch == '`' {
                if let Some(end) = find(index + 1, &['`']) {
                    output.push_str(&self.paint(CODE, &collect(index + 1, end)));
                    index = end + 1;
                    continue;
                }
            }
            if (ch == '*' || ch == '_') && next == Some(ch) {
                if let Some(end) = find(index + 2, &[ch, ch]).filter(|end| *end > index + 2) {
                    output.push_str(&self.paint(BOLD, &self.inline(&collect(index + 2, end))));
                    index = end + 2;
                    continue;
                }
            }
            if (ch == '*' || ch == '_')
                && next.is_some_and(|next| !next.is_whitespace() && next != ch)
                && (index == 0 || !chars[index - 1].is_alphanumeric())
            {
                let closing = find(index + 1, &[ch]).filter(|end| {
                    !chars[end - 1].is_whitespace() && chars.get(end + 1).is_none_or(|after| !after.is_alphanumeric())
                });
                if let Some(end) = closing {
                    output.push_str(&self.paint(ITALIC, &collect(index + 1, end)));
                    index = end + 1;
                    continue;
                }
            }
            if ch == '[' {
                if let Some(close) = find(index + 1, &[']', '(']) {
                    if let Some(end) = find(close + 2, &[')']) {
                        let label = collect(index + 1, close);
                        let url = collect(close + 2, end);
                        output.push_str(&label);
                        if label != url {
                            output.push_str(&self.paint(DIM, &format!(" ({url})")));
                        }
                        index = end + 1;
                        continue;
                    }
                }
            }
            output.push(ch);
            index += 1;
        }
        output
    }

    fn flush_table(&mut self) -> Vec<String> {
        let rows: Vec<Vec<String>> = std::mem::take(&mut self.table)
            .iter()
            .map(|row| row.trim().trim_matches('|').split('|').map(|cell| cell.trim().to_owned()).collect())
            .collect();
        let is_separator = |row: &Vec<String>| row.iter().all(|cell| !cell.is_empty() && cell.chars().all(|ch| matches!(ch, '-' | ':' | ' ')));
        let body: Vec<Vec<String>> = rows.iter().filter(|row| !is_separator(row)).map(|row| row.iter().map(|cell| self.inline(cell)).collect()).collect();
        let plain: Vec<Vec<String>> = rows.iter().filter(|row| !is_separator(row)).map(|row| row.iter().map(|cell| {
            MarkdownStream::new(false).inline(cell)
        }).collect()).collect();
        let columns = plain.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|column| plain.iter().filter_map(|row| row.get(column)).map(|cell| cell.width()).max().unwrap_or(0))
            .collect();
        let has_header = rows.get(1).is_some_and(is_separator);
        let mut output = Vec::new();
        for (index, (row, plain_row)) in body.iter().zip(&plain).enumerate() {
            let cells: Vec<String> = (0..columns)
                .map(|column| {
                    let text = row.get(column).cloned().unwrap_or_default();
                    let width = plain_row.get(column).map_or(0, |cell| cell.width());
                    let cell = format!("{text}{}", " ".repeat(widths[column].saturating_sub(width)));
                    if index == 0 && has_header { self.paint(BOLD, &cell) } else { cell }
                })
                .collect();
            output.push(cells.join(&self.paint(DIM, " \u{2502} ")));
            if index == 0 && has_header {
                let rule: Vec<String> = widths.iter().map(|width| "\u{2500}".repeat(*width)).collect();
                output.push(self.paint(DIM, &rule.join("\u{2500}\u{253c}\u{2500}")));
            }
        }
        output
    }
}

/// Wraps one rendered line to `width` cells at spaces, as the reference CLI
/// does. Style escapes count as zero width; styles open at a break are reset
/// at the end of the row and reopened after `indent` on the next. Words longer
/// than a row break at the edge, and spaces at a break are dropped.
pub fn wrap_styled(text: &str, first: &str, indent: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let indent_cells = indent.width();
    let width = width.max(indent_cells + 2);
    let mut rows = Vec::new();
    let mut row = first.to_owned();
    let mut cells = first.width();
    let mut start = row.len();
    let mut open: Vec<String> = Vec::new();
    // Byte offset in `row` after the last space, the styles open there, and the cells before it.
    let mut space: Option<(usize, Vec<String>, usize)> = None;
    let close = |row: &mut String, open: &[String]| {
        let trimmed = row.trim_end_matches(' ').len();
        row.truncate(trimmed);
        if !open.is_empty() {
            row.push_str("\x1b[0m");
        }
    };
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            let mut sequence = String::from(ch);
            for next in chars.by_ref() {
                sequence.push(next);
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            if sequence.ends_with('m') {
                track_style(&mut open, &sequence);
            }
            row.push_str(&sequence);
            continue;
        }
        let size = ch.width().unwrap_or(0);
        if cells + size > width && row.len() > start {
            // A space at the edge is the break itself; otherwise carry the
            // partial word after the row's last space.
            let carried = if ch == ' ' { None } else { space.take() };
            space = None;
            match carried.filter(|(at, _, _)| *at < row.len()) {
                Some((at, styles, before)) => {
                    let tail = row.split_off(at);
                    close(&mut row, &styles);
                    rows.push(std::mem::take(&mut row));
                    row = format!("{indent}{}", styles.concat());
                    start = row.len();
                    row.push_str(&tail);
                    cells = indent_cells + (cells - before);
                }
                None => {
                    close(&mut row, &open);
                    rows.push(std::mem::take(&mut row));
                    row = format!("{indent}{}", open.concat());
                    start = row.len();
                    cells = indent_cells;
                }
            }
        }
        if ch == ' ' && row.len() == start && !rows.is_empty() {
            continue;
        }
        row.push(ch);
        cells += size;
        if ch == ' ' {
            space = Some((row.len(), open.clone(), cells));
        }
    }
    rows.push(row);
    rows
}

/// Keeps the list of SGR sequences still in effect after `sequence`.
fn track_style(open: &mut Vec<String>, sequence: &str) {
    let params = sequence.trim_start_matches("\x1b[").trim_end_matches('m');
    let first = params.split(';').next().unwrap_or("");
    let closes: &[&str] = match first {
        "" | "0" => {
            open.clear();
            return;
        }
        "22" => &["1", "2"],
        "23" => &["3"],
        "24" => &["4"],
        "29" => &["9"],
        "39" => &["30", "31", "32", "33", "34", "35", "36", "37", "38", "90", "91", "92", "93", "94", "95", "96", "97"],
        "49" => &["40", "41", "42", "43", "44", "45", "46", "47", "48", "100", "101", "102", "103", "104", "105", "106", "107"],
        _ => {
            open.push(sequence.to_owned());
            return;
        }
    };
    open.retain(|style| {
        let param = style.trim_start_matches("\x1b[").split([';', 'm']).next().unwrap_or("");
        !closes.contains(&param)
    });
}

/// Cells to indent continuation rows of a rendered line: its own leading
/// spaces plus a list marker such as `- ` or `12. `.
pub fn hanging_indent(rendered: &str) -> usize {
    let mut visible = String::new();
    let mut chars = rendered.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            visible.push(ch);
        }
    }
    let trimmed = visible.trim_start_matches(' ');
    let leading = visible.len() - trimmed.len();
    let marker = if ["- ", "* ", "+ ", "\u{2022} "].iter().any(|marker| trimmed.starts_with(marker)) {
        2
    } else {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") { digits + 2 } else { 0 }
    };
    leading + marker
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[&str]) -> Vec<String> {
        let mut stream = MarkdownStream::new(false);
        let mut output: Vec<String> = lines.iter().flat_map(|line| stream.line(line)).collect();
        output.extend(stream.finish());
        output
    }

    #[test]
    fn inline_markers_are_removed_and_unmatched_ones_kept() {
        assert_eq!(plain(&["**Files read** and `main.rs`, *really* [docs](https://x.y)"]), ["Files read and main.rs, really docs (https://x.y)"]);
        assert_eq!(plain(&["2 * 3 = 6 and a_b_c stays"]), ["2 * 3 = 6 and a_b_c stays"]);
        assert_eq!(plain(&["## Summary", "> quoted", "---"]), ["Summary", "\u{258e} quoted", &"\u{2500}".repeat(40)]);
    }

    #[test]
    fn code_fences_disappear_and_tables_align() {
        assert_eq!(plain(&["```rust", "fn main() {}", "```", "after"]), ["  fn main() {}", "after"]);
        let table = plain(&["| Name | Lines |", "|---|---:|", "| a.rs | 3 |", "| longer.rs | 120 |"]);
        assert_eq!(table[0], "Name      \u{2502} Lines");
        assert_eq!(table[2], "a.rs      \u{2502} 3    ");
        assert_eq!(table.len(), 4);
    }

    #[test]
    fn colored_output_wraps_spans_without_model_escapes() {
        let mut stream = MarkdownStream::new(true);
        let line = stream.line("**bold**\x1b[2J");
        assert_eq!(line, ["\x1b[1mbold\x1b[22m[2J"]);
    }

    #[test]
    fn wraps_rendered_lines_at_spaces_and_reopens_styles() {
        let rows = wrap_styled("one two three four", "\u{25cf} ", "  ", 12);
        assert_eq!(rows, ["\u{25cf} one two", "  three four"]);
        let bold = wrap_styled("\x1b[1mbold words here\x1b[22m done", "  ", "  ", 12);
        assert_eq!(bold, ["  \x1b[1mbold words\x1b[0m", "  \x1b[1mhere\x1b[22m done"]);
        let long = wrap_styled("abcdefghijklmnop", "  ", "  ", 8);
        assert_eq!(long, ["  abcdef", "  ghijkl", "  mnop"]);
        assert_eq!(hanging_indent("- item"), 2);
        assert_eq!(hanging_indent("  12. \x1b[1mstep\x1b[22m"), 6);
        assert_eq!(hanging_indent("plain"), 0);
    }
}
