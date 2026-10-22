//! Trusted style tags; file text cannot supply terminal escapes.
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
#[derive(Clone, Copy, Debug)]
pub enum Style {
    Body,
    Muted,
    Success,
    Failure,
    Removed,
    Added,
    RemovedWord,
    AddedWord,
    RemovedGutter,
    AddedGutter,
}
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
    syntax: Option<Syntax>,
}
#[derive(Clone, Debug)]
pub struct Line(pub Vec<Span>);
fn span(text: impl Into<String>, style: Style) -> Span {
    Span {
        text: text.into(),
        style,
        syntax: None,
    }
}
#[derive(Clone, Copy, Debug)]
enum Syntax {
    Keyword,
    Type,
    Function,
    Number,
    String,
}
fn syntax_code(syntax: Syntax) -> &'static str {
    match syntax {
        Syntax::Keyword => "38;2;249;38;114",
        Syntax::Type => "38;2;102;217;239",
        Syntax::Function => "38;2;166;226;46",
        Syntax::Number => "38;2;190;132;255",
        Syntax::String => "38;2;230;219;116",
    }
}
fn painted(
    text: &str,
    base: Style,
    word: Style,
    changed: Option<(usize, usize)>,
    syntax: bool,
) -> Vec<Span> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let start = index;
        let ch = text[index..].chars().next().unwrap();
        index += ch.len_utf8();
        let category = if syntax && matches!(ch, '\'' | '"' | '`') {
            let mut escaped = false;
            while index < text.len() {
                let next = text[index..].chars().next().unwrap();
                index += next.len_utf8();
                if next == ch && !escaped {
                    break;
                }
                if next == '\\' {
                    escaped = !escaped;
                } else {
                    escaped = false;
                }
            }
            Some(Syntax::String)
        } else if syntax && ch.is_ascii_digit() {
            while index < text.len() && text.as_bytes()[index].is_ascii_alphanumeric() {
                index += 1;
            }
            Some(Syntax::Number)
        } else if syntax && (ch.is_alphabetic() || ch == '_') {
            while index < text.len() {
                let next = text[index..].chars().next().unwrap();
                if !(next.is_alphanumeric() || next == '_') {
                    break;
                }
                index += next.len_utf8();
            }
            let token = &text[start..index];
            if [
                "export", "function", "return", "const", "let", "var", "if", "else", "import",
                "from", "async", "await", "fn", "pub", "use", "mod", "impl", "struct", "enum",
                "match", "mut", "for", "while", "break", "continue", "class", "new", "throw",
                "try", "catch",
            ]
            .contains(&token)
            {
                Some(Syntax::Keyword)
            } else if [
                "number",
                "string",
                "boolean",
                "void",
                "null",
                "undefined",
                "true",
                "false",
                "bool",
                "str",
                "String",
                "usize",
                "u8",
                "u16",
                "u32",
                "u64",
                "i8",
                "i16",
                "i32",
                "i64",
                "f32",
                "f64",
                "Self",
            ]
            .contains(&token)
            {
                Some(Syntax::Type)
            } else if text[index..].trim_start().starts_with('(') {
                Some(Syntax::Function)
            } else {
                None
            }
        } else {
            None
        };
        let mut boundaries = vec![start, index];
        if let Some((a, b)) = changed {
            if a > start && a < index {
                boundaries.push(a);
            }
            if b > start && b < index {
                boundaries.push(b);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for pair in boundaries.windows(2) {
            let active = changed.is_some_and(|(a, b)| pair[0] >= a && pair[0] < b);
            let mut segment = span(&text[pair[0]..pair[1]], if active { word } else { base });
            segment.syntax = category;
            result.push(segment);
        }
    }
    result
}
fn codes(style: Style, theme: u8) -> &'static str {
    if theme >= 4 {
        return match style {
            Style::Body => "39",
            Style::Muted => "90",
            Style::Success => "32",
            Style::Failure => "31",
            Style::Removed | Style::RemovedGutter => "31",
            Style::Added | Style::AddedGutter => "32",
            Style::RemovedWord => "31;47",
            Style::AddedWord => "32;47",
        };
    }
    if theme == 2 {
        match style {
            Style::Added => return "38;2;248;248;242;48;2;0;25;62",
            Style::AddedWord => return "38;2;248;248;242;48;2;0;45;95",
            Style::AddedGutter => return "38;2;80;160;255;48;2;0;25;62",
            _ => {}
        }
    }
    if theme == 3 {
        match style {
            Style::Added => return "38;2;32;32;32;48;2;220;235;255",
            Style::AddedWord => return "38;2;32;32;32;48;2;170;205;245",
            Style::AddedGutter => return "38;2;24;70;150;48;2;220;235;255",
            _ => {}
        }
    }
    if theme % 2 == 1 {
        return match style {
            Style::Body => "38;2;32;32;32",
            Style::Muted => "38;2;102;102;102",
            Style::Success => "38;2;32;128;64",
            Style::Failure => "38;2;176;32;32",
            Style::Removed => "38;2;32;32;32;48;2;255;224;224",
            Style::Added => "38;2;32;32;32;48;2;220;245;220",
            Style::RemovedWord => "38;2;32;32;32;48;2;255;180;180",
            Style::AddedWord => "38;2;32;32;32;48;2;170;225;170",
            Style::RemovedGutter => "38;2;160;32;32;48;2;255;224;224",
            Style::AddedGutter => "38;2;24;120;24;48;2;220;245;220",
        };
    }
    match style {
        Style::Body => "38;2;248;248;242",
        Style::Muted => "38;2;153;153;153",
        Style::Success => "38;2;78;186;101",
        Style::Failure => "38;2;255;107;107",
        Style::Removed => "38;2;248;248;242;48;2;61;1;0",
        Style::Added => "38;2;248;248;242;48;2;2;40;0",
        Style::RemovedWord => "38;2;248;248;242;48;2;92;2;0",
        Style::AddedWord => "38;2;248;248;242;48;2;4;71;0",
        Style::RemovedGutter => "38;2;220;90;90;48;2;61;1;0",
        Style::AddedGutter => "38;2;80;200;80;48;2;2;40;0",
    }
}
pub fn render(line: &Line, width: usize, color: bool) -> String {
    render_with_theme(line, width, color, super::style::theme_variant())
}

fn render_with_theme(line: &Line, width: usize, color: bool, theme: u8) -> String {
    let mut result = String::new();
    let mut cells = 0;
    for segment in &line.0 {
        let mut text = String::new();
        let mut clipped = false;
        for ch in segment.text.chars().filter(|ch| {
            !ch.is_control() && !matches!(*ch,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
        }) {
            let size = ch.width().unwrap_or(0);
            if cells + size > width {
                clipped = true;
                break;
            }
            cells += size;
            text.push(ch);
        }
        if !text.is_empty() {
            if color {
                result.push_str(&format!("\x1b[{}m", codes(segment.style, theme)));
                if theme == 0 || theme == 2 {
                    if let Some(syntax) = segment.syntax {
                        result.push_str(&format!("\x1b[{}m", syntax_code(syntax)));
                    }
                }
            }
            result.push_str(&text);
            if color {
                result.push_str("\x1b[0m");
            }
        }
        if clipped || cells >= width {
            break;
        }
    }
    // Only finite physical terminal rows receive background padding. Plain
    // details use usize::MAX and must never allocate a terminal-width suffix.
    if color && width <= u16::MAX as usize && cells < width {
        let background = line.0.first().and_then(|span| match span.style {
            Style::RemovedGutter => Some(Style::Removed),
            Style::AddedGutter => Some(Style::Added),
            _ => None,
        });
        if let Some(background) = background {
            result.push_str(&format!("\x1b[{}m", codes(background, theme)));
            result.push_str(&" ".repeat(width - cells));
            result.push_str("\x1b[0m");
        }
    }
    result
}
pub fn muted(summary: &str) -> Line {
    Line(vec![span(summary, Style::Muted)])
}
pub fn success(summary: &str) -> Line {
    Line(vec![
        span("\u{25cf} ", Style::Success),
        span(summary, Style::Muted),
    ])
}
/// The header of a finished tool call: `● Bash(ls -la) · 0.1s`.
pub fn tool_call(label: &str, detail: &str, failed: bool) -> Line {
    let mut spans = vec![
        span("\u{25cf} ", if failed { Style::Failure } else { Style::Success }),
        span(label, Style::Body),
    ];
    if !detail.is_empty() {
        spans.push(span(format!(" \u{b7} {detail}"), Style::Muted));
    }
    Line(spans)
}
/// One line of a tool result under its header, as `  ⎿  text`.
pub fn tool_output(text: &str, first: bool, failed: bool) -> Line {
    Line(vec![
        span(if first { "  \u{23bf}  " } else { "     " }, Style::Muted),
        span(text, if failed { Style::Failure } else { Style::Muted }),
    ])
}
fn changed_span(text: &str, other: &str) -> (usize, usize) {
    let prefix = text
        .chars()
        .zip(other.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    let tail = text[prefix..]
        .chars()
        .rev()
        .zip(other[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    (prefix, text.len() - tail)
}
pub fn change(output: &str, failed: bool) -> Option<Vec<Line>> {
    change_in_workspace(output, failed, None)
}

pub fn change_in_workspace(
    output: &str,
    failed: bool,
    workspace: Option<&std::path::Path>,
) -> Option<Vec<Line>> {
    if failed {
        return None;
    }
    let output: Value = serde_json::from_str(output).ok()?;
    if output["success"] != true
        || output["file_change"]["verified"] != true
        || output["file_change"]["version"] != 1
    {
        return None;
    }
    let change = &output["file_change"];
    let path = change["path"].as_str()?;
    let display_path = super::display_path::label(path, workspace);
    let mut result = vec![Line(vec![
        span("\u{25cf} ", Style::Success),
        span(format!("Update({display_path})"), Style::Body),
    ])];
    if let Some(notice) = change["notice"].as_str() {
        result.push(Line(vec![span(format!("  {notice}"), Style::Muted)]));
        return Some(result);
    }
    result.push(Line(vec![span(
        format!(
            "  Added {} {}, removed {} {}",
            change["added"].as_u64().unwrap_or(0),
            if change["added"].as_u64() == Some(1) {
                "line"
            } else {
                "lines"
            },
            change["removed"].as_u64().unwrap_or(0),
            if change["removed"].as_u64() == Some(1) {
                "line"
            } else {
                "lines"
            }
        ),
        Style::Muted,
    )]));
    if let Some(notes) = change["format_notes"].as_array() {
        for note in notes.iter().filter_map(Value::as_str) {
            result.push(muted(&format!("  {note}")));
        }
    }
    let rows = change["rows"].as_array()?;
    for (index, row) in rows.iter().take(120).enumerate() {
        let kind = row["kind"].as_str().unwrap_or("context");
        let text = row["text"].as_str().unwrap_or("");
        let (base, word, gutter, sign) = match kind {
            "removed" => (
                Style::Removed,
                Style::RemovedWord,
                Style::RemovedGutter,
                "-",
            ),
            "added" => (Style::Added, Style::AddedWord, Style::AddedGutter, "+"),
            _ => (Style::Body, Style::Body, Style::Muted, " "),
        };
        let number = row[if kind == "removed" {
            "old_line"
        } else {
            "new_line"
        }]
        .as_u64()
        .unwrap_or(0);
        let mut spans = vec![span(format!("  {number:>5} {sign}"), gutter)];
        let other = if kind == "removed" {
            rows.get(index + 1).filter(|next| next["kind"] == "added")
        } else if kind == "added" {
            index
                .checked_sub(1)
                .and_then(|prev| rows.get(prev))
                .filter(|prev| prev["kind"] == "removed")
        } else {
            None
        };
        let changed = other
            .and_then(|row| row["text"].as_str())
            .map(|other| changed_span(text, other));
        let syntax = kind != "removed"
            && std::path::Path::new(path)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ["js", "jsx", "ts", "tsx", "rs", "json"].contains(&ext));
        spans.extend(painted(text, base, word, changed, syntax));
        result.push(Line(spans));
    }
    if change["truncated"] == true {
        result.push(Line(vec![span(
            "  Diff truncated to 120 lines / 32 KiB",
            Style::Muted,
        )]));
    }
    Some(result)
}
pub fn details(name: &str, output: &str, failed: bool) -> Option<String> {
    matches!(name, "Write" | "Edit" | "MultiEdit")
        .then(|| change(output, failed))
        .flatten()
        .map(|lines| plain(&lines))
}

pub fn plain(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| render(line, usize::MAX, false))
        .collect::<Vec<_>>()
        .join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    // Palette assertions are independent of concurrently running /config tests.
    fn render(line: &Line, width: usize, color: bool) -> String {
        render_with_theme(line, width, color, 0)
    }

    #[test]
    fn selected_palette_controls_text_highlights_and_padding() {
        let line = Line(vec![
            span("+", Style::AddedGutter),
            span("42", Style::AddedWord),
        ]);
        for (theme, base, word) in [
            (0, "48;2;2;40;0", "48;2;4;71;0"),
            (1, "48;2;220;245;220", "48;2;170;225;170"),
            (2, "48;2;0;25;62", "48;2;0;45;95"),
            (3, "48;2;220;235;255", "48;2;170;205;245"),
        ] {
            let rendered = render_with_theme(&line, 8, true, theme);
            assert!(rendered.contains(word));
            assert!(rendered.ends_with(&format!("{base}m     \x1b[0m")));
            assert_eq!(render_with_theme(&line, 8, false, theme), "+42");
        }
        for theme in [4, 5] {
            let rendered = render_with_theme(&line, 8, true, theme);
            assert!(rendered.contains("\x1b[32;47m42"));
            assert!(rendered.ends_with("\x1b[32m     \x1b[0m"));
            assert!(!rendered.contains(";2;"));
        }
    }

    #[test]
    fn typed_colors_sanitize_controls_and_bound_unicode_width() {
        let line = Line(vec![span("\x1b[2J\u{4e2d}ab", Style::AddedWord)]);
        assert_eq!(render(&line, 6, false), "[2J\u{4e2d}a");
        let colored = render(&Line(vec![span("42", Style::AddedWord)]), 8, true);
        assert!(colored.contains("48;2;4;71;0"));
        assert!(colored.ends_with("\x1b[0m"));
        assert_eq!(
            changed_span("\u{4e2d}\u{6587}41!", "\u{4e2d}\u{6587}42!"),
            (7, 8)
        );
    }
    #[test]
    fn verified_replacement_uses_line_and_changed_character_backgrounds() {
        let output = serde_json::json!({"success":true,"file_change":{"version":1,"verified":true,"path":"a.ts","added":1,"removed":1,"rows":[{"kind":"removed","old_line":1,"text":"return 41;"},{"kind":"added","new_line":1,"text":"return 42;"}]}});
        let lines = change(&output.to_string(), false).unwrap();
        assert_eq!(plain(&lines),"\u{25cf} Update(a.ts)\n  Added 1 line, removed 1 line\n      1 -return 41;\n      1 +return 42;");
        let removed = render(&lines[2], 80, true);
        let added = render(&lines[3], 80, true);
        assert!(removed.contains("48;2;61;1;0"));
        assert!(removed.contains("48;2;92;2;0"));
        assert!(added.contains("48;2;2;40;0"));
        assert!(added.contains("48;2;4;71;0"));
        assert!(change(&output.to_string(), true).is_none());
        for width in [0, 1, 8, 12] {
            let plain = render(&lines[2], width, false);
            assert!(unicode_width::UnicodeWidthStr::width(plain.as_str()) <= width);
            assert!(!plain.contains('\x1b'));
        }
    }
    #[test]
    fn workspace_diff_title_keeps_full_detail_and_styling() {
        let workspace = std::env::temp_dir().join("diff workspace");
        let path = workspace.join("中文 file.ts");
        let output = serde_json::json!({"success":true,"file_change":{"version":1,"verified":true,"path":path,"added":1,"removed":0,"rows":[{"kind":"added","new_line":1,"text":"const answer = 42;"}]}}).to_string();
        let full = change(&output, false).unwrap();
        let preview = change_in_workspace(&output, false, Some(&workspace)).unwrap();
        assert!(plain(&full).contains(&path.to_string_lossy().to_string()));
        assert!(plain(&preview).starts_with("● Update(中文 file.ts)\n"));
        for (original, shown) in full.iter().skip(1).zip(preview.iter().skip(1)) {
            assert_eq!(render(original, 100, true), render(shown, 100, true));
        }
    }
    #[test]
    fn typescript_tokens_match_reference_foregrounds() {
        let line = Line(painted(
            "export function answer(): number { return 42; }",
            Style::Added,
            Style::AddedWord,
            None,
            true,
        ));
        let rendered = render(&line, 120, true);
        for code in [
            "38;2;249;38;114",
            "38;2;102;217;239",
            "38;2;166;226;46",
            "38;2;190;132;255",
        ] {
            assert!(rendered.contains(code), "missing {code}");
        }
    }
    #[test]
    fn diff_rows_fill_finite_width_with_base_background_only() {
        for (gutter, word, base_code) in [
            (Style::AddedGutter, Style::AddedWord, "48;2;2;40;0"),
            (Style::RemovedGutter, Style::RemovedWord, "48;2;61;1;0"),
        ] {
            let line = Line(vec![span("+", gutter), span("x", word)]);
            let rendered = render(&line, 8, true);
            assert!(rendered.ends_with("      \x1b[0m"));
            let padding = rendered.rsplit_once("\x1b[0m\x1b[").unwrap().1;
            assert!(
                padding.contains(base_code),
                "padding must not use the changed-word background"
            );
            assert_eq!(render(&line, 8, false), "+x");
            assert_eq!(render(&line, usize::MAX, false), "+x");
            assert!(render(&line, usize::MAX, true).len() < 200);
            assert_eq!(render(&line, 0, true), "");
        }
        let value = serde_json::json!({"success":true,"file_change":{"version":1,"verified":true,"path":"x","added":2,"removed":0,"rows":[]}});
        assert!(plain(&change(&value.to_string(), false).unwrap())
            .contains("Added 2 lines, removed 0 lines"));
    }
    #[test]
    fn failed_output_has_no_success_diff() {
        assert!(change(r#"{"success":false}"#, false).is_none());
        assert!(change("{}", true).is_none());
    }
}
