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
}
