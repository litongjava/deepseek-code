//! Preserve Markdown source and parse complete blocks with CommonMark events.
//! Only stable blocks enter scrollback; the open block remains a mutable preview.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

const BOLD: (&str, &str) = ("\x1b[1m", "\x1b[22m");
const ITALIC: (&str, &str) = ("\x1b[3m", "\x1b[23m");
const CODE: (&str, &str) = ("\x1b[38;2;175;135;255m", "\x1b[39m");
const DIM: (&str, &str) = ("\x1b[2m", "\x1b[22m");

fn display_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        .collect()
}

#[derive(Default)]
pub struct MarkdownStream {
    color: bool,
    source: String,
    committed: usize,
    width: Option<usize>,
}

impl MarkdownStream {
    pub fn new(color: bool) -> Self {
        Self {
            color,
            ..Default::default()
        }
    }
    pub fn set_width(&mut self, width: usize) {
        self.width = Some(width.max(2));
    }
    fn paint(&self, (open, close): (&str, &str), text: &str) -> String {
        if self.color {
            format!("{open}{text}{close}")
        } else {
            text.into()
        }
    }
    pub fn line(&mut self, raw: &str) -> Vec<String> {
        self.source.push_str(raw);
        self.source.push('\n');
        let blocks = self.blocks(&self.source);
        // A following root block stabilizes the preceding block. A table,
        // setext heading or multiline inline span can still change the tail.
        let count = blocks.len().saturating_sub(1);
        let mut lines = Vec::new();
        for (end, block) in blocks.into_iter().take(count) {
            if end > self.committed {
                lines.extend(block);
                self.committed = end;
            }
        }
        lines
    }
    pub fn preview(&self, tail: &str, width: usize) -> Vec<String> {
        let source = format!("{}{tail}", self.source);
        let mut renderer = Self::new(self.color);
        renderer.set_width(width);
        renderer
            .blocks(&source)
            .into_iter()
            .filter(|(end, _)| *end > self.committed)
            .flat_map(|(_, rows)| rows)
            .collect()
    }
    pub fn finish(&mut self) -> Vec<String> {
        let lines = self
            .blocks(&self.source)
            .into_iter()
            .filter(|(end, _)| *end > self.committed)
            .flat_map(|(_, rows)| rows)
            .collect();
        self.source.clear();
        self.committed = 0;
        lines
    }
    fn blocks(&self, source: &str) -> Vec<(usize, Vec<String>)> {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let mut blocks = Vec::new();
        let mut events = Vec::new();
        let mut depth = 0usize;
        let mut start = 0;
        let mut last_end = 0;
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            if depth == 0 {
                start = range.start;
            }
            match &event {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
            events.push(event);
            if depth == 0 {
                let mut rows = self.render_events(std::mem::take(&mut events));
                if last_end > 0 && source[..start].ends_with("\n\n") {
                    rows.insert(0, String::new());
                }
                last_end = range.end;
                blocks.push((range.end, rows));
            }
        }
        blocks
    }
    fn render_events(&self, events: Vec<Event<'_>>) -> Vec<String> {
        let mut rows = Vec::new();
        let mut line = String::new();
        let mut styles: Vec<(&str, &str)> = Vec::new();
        let mut lists: Vec<Option<u64>> = Vec::new();
        let mut quotes = 0;
        let mut code = false;
        let mut links: Vec<String> = Vec::new();
        let mut table: Option<Vec<Vec<String>>> = None;
        let flush = |line: &mut String, rows: &mut Vec<String>| {
            if !line.is_empty() {
                rows.push(std::mem::take(line));
            }
        };
        for event in events {
            match event {
                Event::Start(Tag::CodeBlock(_)) => {
                    flush(&mut line, &mut rows);
                    code = true;
                }
                Event::End(TagEnd::CodeBlock) => {
                    if !line.is_empty() {
                        rows.push(self.paint(CODE, &format!("  {}", std::mem::take(&mut line))));
                    }
                    code = false;
                }
                Event::Start(Tag::Heading { .. }) | Event::Start(Tag::Strong) => styles.push(BOLD),
                Event::Start(Tag::Emphasis) => styles.push(ITALIC),
                Event::Start(Tag::Strikethrough) => styles.push(("\x1b[9m", "\x1b[29m")),
                Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                    styles.pop();
                }
                Event::End(TagEnd::Heading(_)) => {
                    styles.pop();
                    flush(&mut line, &mut rows);
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    flush(&mut line, &mut rows);
                    quotes += 1;
                    styles.push(ITALIC);
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    flush(&mut line, &mut rows);
                    quotes -= 1;
                    styles.pop();
                }
                Event::Start(Tag::List(first)) => {
                    flush(&mut line, &mut rows);
                    lists.push(first);
                }
                Event::End(TagEnd::List(_)) => {
                    flush(&mut line, &mut rows);
                    lists.pop();
                }
                Event::Start(Tag::Item) => {
                    flush(&mut line, &mut rows);
                    line.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                    match lists.last_mut() {
                        Some(Some(n)) => {
                            line.push_str(&format!("{n}. "));
                            *n += 1;
                        }
                        _ => line.push_str("- "),
                    }
                }
                Event::End(TagEnd::Item | TagEnd::Paragraph) => flush(&mut line, &mut rows),
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                    links.push(display_text(&dest_url))
                }
                Event::End(TagEnd::Link | TagEnd::Image) => {
                    if let Some(url) = links.pop() {
                        if !plain_rendered(&line).ends_with(&url) {
                            line.push_str(&self.paint(DIM, &format!(" ({url})")));
                        }
                    }
                }
                Event::Start(Tag::Table(_)) => table = Some(Vec::new()),
                Event::Start(Tag::TableHead | Tag::TableRow) => {
                    if let Some(table) = &mut table {
                        table.push(Vec::new());
                    }
                }
                Event::End(TagEnd::TableCell) => {
                    if let Some(row) = table.as_mut().and_then(|table| table.last_mut()) {
                        row.push(std::mem::take(&mut line));
                    }
                }
                Event::End(TagEnd::Table) => {
                    rows.extend(self.table_rows(table.take().unwrap_or_default()));
                }
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    let text = display_text(&text);
                    if code {
                        // Code whitespace belongs to the command, including blank lines.
                        for part in text.split_inclusive('\n') {
                            line.push_str(part.trim_end_matches(['\r', '\n']));
                            if part.ends_with('\n') {
                                rows.push(
                                    self.paint(CODE, &format!("  {}", std::mem::take(&mut line))),
                                );
                            }
                        }
                    } else {
                        if line.is_empty() && quotes > 0 {
                            line.push_str(&"▎ ".repeat(quotes));
                        }
                        let mut text = text.to_string();
                        for style in styles.iter().rev() {
                            text = self.paint(*style, &text);
                        }
                        line.push_str(&text);
                    }
                }
                Event::Code(text) => line.push_str(&self.paint(CODE, &display_text(&text))),
                Event::SoftBreak | Event::HardBreak => {
                    rows.push(std::mem::take(&mut line));
                }
                Event::Rule => {
                    rows.push(self.paint(DIM, &"─".repeat(self.width.unwrap_or(40).min(40))))
                }
                Event::TaskListMarker(checked) => {
                    line.push_str(if checked { "[x] " } else { "[ ] " })
                }
                _ => {}
            }
        }
        if code && !line.is_empty() {
            line = self.paint(CODE, &format!("  {line}"));
        }
        flush(&mut line, &mut rows);
        // Sanitize only rendered text so parser offsets still address the source.
        rows.into_iter()
            .map(|row| {
                row.chars()
                    .filter(|ch| !ch.is_control() || matches!(ch, '\t' | '\x1b'))
                    .collect::<String>()
                    .replace('\t', "    ")
            })
            .collect()
    }
    fn table_rows(&self, body: Vec<Vec<String>>) -> Vec<String> {
        let plain: Vec<Vec<String>> = body
            .iter()
            .map(|row| row.iter().map(|cell| plain_rendered(cell)).collect())
            .collect();
        let columns = plain.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths: Vec<usize> = (0..columns)
            .map(|column| plain.iter().filter_map(|row| row.get(column)).map(|cell| cell.width()).max().unwrap_or(0).max(2))
            .collect();
        let has_header = true;
        let available = self.width.unwrap_or(usize::MAX);
        let separators = columns.saturating_sub(1) * 3;
        // A wide Unicode character needs two cells. Extremely narrow tables
        // become stacked fields instead of overflowing the terminal.
        if columns * 2 + separators > available {
            return body.iter().flat_map(|row| {
                let mut lines: Vec<String> = row.iter()
                    .flat_map(|cell| wrap_styled(cell, "", "", available)).collect();
                lines.push(String::new());
                lines
            }).collect();
        }
        while widths.iter().sum::<usize>() + separators > available {
            let column = widths.iter().enumerate().max_by_key(|(_, width)| **width).unwrap().0;
            widths[column] -= 1;
        }
        let mut output = Vec::new();
        for (index, row) in body.iter().enumerate() {
            let wrapped: Vec<Vec<String>> = (0..columns).map(|column| {
                wrap_styled(row.get(column).map(String::as_str).unwrap_or(""), "", "", widths[column])
            }).collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            for line in 0..height {
                let cells: Vec<String> = (0..columns)
                    .map(|column| {
                        let text = wrapped[column].get(line).cloned().unwrap_or_default();
                        let mut visible = String::new();
                        let mut escape = false;
                        for ch in text.chars() {
                            if ch == '\x1b' { escape = true; }
                            else if escape { if ch == 'm' { escape = false; } }
                            else { visible.push(ch); }
                        }
                        let width = visible.width();
                        let cell = format!("{text}{}", " ".repeat(widths[column].saturating_sub(width)));
                        if index == 0 && has_header { self.paint(BOLD, &cell) } else { cell }
                    })
                    .collect();
                output.push(cells.join(&self.paint(DIM, " \u{2502} ")));
            }
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

pub(super) fn plain_rendered(rendered: &str) -> String {
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
    visible
}

/// Cells to indent continuation rows of a rendered line: its own leading
/// spaces plus a list marker such as `- ` or `12. `.
pub fn hanging_indent(rendered: &str) -> usize {
    let visible = plain_rendered(rendered);
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

    #[test]
    fn same_line_fences_preserve_content_and_do_not_leak_code_style() {
        let mut stream = MarkdownStream::new(true);
        stream.line("```textAI 请求工具 → Auto 分类器```");
        let line = stream.preview("", 120);
        assert!(plain_rendered(&line.join("")).contains("AI 请求工具"));
        stream.line("");
        stream.line("**一句话：** Auto 正常处理");
        assert_eq!(
            stream.finish().last().unwrap(),
            "\x1b[1m一句话：\x1b[22m Auto 正常处理"
        );
    }

    #[test]
    fn wide_tables_wrap_cells_without_moving_other_columns() {
        for color in [false, true] {
            for width in [28, 60, 100] {
                let mut stream = MarkdownStream::new(color);
                stream.set_width(width);
                for line in ["| 序号 | 模块 | JDK | 耗时 |", "|---|---|---|---|",
                    "| 1 | **java-model** | 21 | 13.1s |",
                    "| 2 | `中文模块` / java-uni-ai-server / java-manin-server / tio-http-file-server | 8 | 24.5s |"] {
                    assert!(stream.line(line).is_empty());
                }
                let output = stream.finish();
                let plain: Vec<_> = output.iter().map(|line| super::super::tool_view::result_text(line)).collect();
                assert!(plain.iter().all(|line| line.width() <= width));
                let positions: Vec<Vec<usize>> = plain.iter().filter(|line| line.contains('│')).map(|line| {
                    line.match_indices('│').map(|(at, _)| line[..at].width()).collect()
                }).collect();
                assert!(positions.len() >= 3);
                assert!(positions.iter().all(|row| row == &positions[0]));
                assert!(plain.iter().any(|line| line.contains("21") && line.contains("13.1s")));
            }
        }
    }

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
        stream.line("**bold**\x1b[2J");
        let line = stream.finish();
        assert_eq!(line, ["\x1b[1mbold\x1b[22m[2J"]);
    }

    #[test]
    fn standard_parser_handles_multiline_spans_backtick_runs_and_setext() {
        assert_eq!(
            plain(&["``a ` b`` and \\*literal\\* &amp; **bold**"]),
            ["a ` b and *literal* & bold"]
        );
        let mut stream = MarkdownStream::new(true);
        assert!(stream.line("Heading").is_empty());
        assert!(stream.line("=======").is_empty());
        assert!(stream.preview("", 80)[0].contains("\x1b[1mHeading"));
        assert!(stream.line("").is_empty());
        let committed = stream.line("**across");
        assert!(committed.iter().any(|line| line.contains("Heading")));
        stream.line("lines**");
        let tail = stream.finish();
        assert!(tail.iter().any(|line| line.contains("\x1b[1macross")));
        assert!(tail.iter().any(|line| line.contains("\x1b[1mlines")));
        assert!(!tail.join("\n").contains("Heading"));
    }

    #[test]
    fn fenced_commands_preserve_spaces_blank_lines_and_literal_markers() {
        let output = plain(&[
            "```bat",
            "git clone --depth 1 repo",
            "",
            "  echo **literal**",
            "```",
            "",
            "after",
        ]);
        assert!(output.contains(&"  git clone --depth 1 repo".into()));
        assert!(output.contains(&"  ".into()));
        assert!(output.contains(&"    echo **literal**".into()));
        assert!(!output.join("\n").contains("batgit"));
    }

    #[test]
    fn control_characters_do_not_shift_stream_commit_offsets() {
        let output = plain(&["first\x1b[2J", "", "second", "", "third"]);
        assert_eq!(
            output.iter().filter(|line| line.contains("first")).count(),
            1
        );
        assert_eq!(
            output.iter().filter(|line| line.contains("second")).count(),
            1
        );
        assert_eq!(
            output.iter().filter(|line| line.contains("third")).count(),
            1
        );
        assert!(!output.join("").contains('\x1b'));
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
