//! Transcript rendering for finished tool calls: one `● Tool(args)` header with
//! a short `⎿` result, and a full form for the Ctrl+O details view.
use super::change_view::{self, Line};
use serde_json::Value;
use std::time::Duration;

/// Lines of a successful result shown inline before "+N lines".
const SUMMARY_LINES: usize = 3;
/// Lines of one result printed by the details view before truncation.
const EXPANDED_LINES: usize = 400;

/// `Bash(ls -la)` for previewable inputs, otherwise the tool name.
pub fn label(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> String {
    let preview = super::progress::tool_preview_in_workspace(name, input, workspace);
    let name = super::progress::single_line(name);
    match preview.strip_prefix(&format!("{name}: ")) {
        Some(argument) => format!("{name}({argument})"),
        None => preview,
    }
}

/// Read-only calls the reference CLI folds into one summary line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gathering {
    Search,
    Read,
    List,
    Shell,
}

pub fn gathering(name: &str, input: &Value) -> Option<Gathering> {
    match name {
        "Grep" | "Glob" => Some(Gathering::Search),
        "Read" => Some(Gathering::Read),
        "ListDir" => Some(Gathering::List),
        "Bash" => {
            let command = input["command"].as_str()?;
            if !claude_core::permissions::shell_read_only::is_read_only(command) {
                return None;
            }
            // As in the reference CLI, `ls -la src` reads as a listed directory.
            let simple = !command.contains(['|', ';', '&', '\n']);
            Some(match command.split_whitespace().next() {
                Some("ls" | "tree") if simple => Gathering::List,
                Some("cat" | "head" | "tail") if simple => Gathering::Read,
                Some("grep" | "rg" | "find" | "fd") if simple => Gathering::Search,
                _ => Gathering::Shell,
            })
        }
        _ => None,
    }
}

/// `Searching for 2 patterns, reading 3 files…` while calls run and
/// `searched for 2 patterns, read 3 files` once they have finished.
pub fn gathering_summary(kinds: &[Gathering], running: bool) -> String {
    let forms = [
        (Gathering::Search, "searching for", "searched for", "pattern", "patterns"),
        (Gathering::Read, "reading", "read", "file", "files"),
        (Gathering::List, "listing", "listed", "directory", "directories"),
        (Gathering::Shell, "running", "ran", "shell command", "shell commands"),
    ];
    let parts: Vec<String> = forms
        .iter()
        .filter_map(|(kind, present, past, one, many)| {
            let count = kinds.iter().filter(|other| *other == kind).count();
            if count == 0 {
                return None;
            }
            let verb = if running { present } else { past };
            Some(format!("{verb} {count} {}", if count == 1 { one } else { many }))
        })
        .collect();
    let mut text = parts.join(", ");
    if running {
        text.push('\u{2026}');
    }
    text
}

/// Human-readable text of a tool output; JSON envelopes are unwrapped.
pub fn result_text(output: &str) -> String {
    // Failed calls wrap the tool's own JSON result as `Error: {...}`.
    let body = output.strip_prefix("Error: ").unwrap_or(output);
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return output.trim_end().to_owned();
    };
    if fields.contains_key("stdout") || fields.contains_key("stderr") {
        let mut text = fields
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_end()
            .to_owned();
        if let Some(stderr) = fields
            .get("stderr")
            .and_then(Value::as_str)
            .filter(|stderr| !stderr.trim().is_empty())
        {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(stderr.trim_end());
        }
        if let Some(code) = fields
            .get("exit_code")
            .and_then(Value::as_i64)
            .filter(|code| *code != 0)
        {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("Exit code {code}"));
        }
        return text;
    }
    for key in ["error", "message", "result", "content"] {
        if let Some(text) = fields.get(key).and_then(Value::as_str) {
            return text.trim_end().to_owned();
        }
    }
    serde_json::to_string_pretty(&Value::Object(fields)).unwrap_or_default()
}

fn printable(line: &str) -> String {
    line.chars()
        .map(|ch| if ch == '\t' { ' ' } else { ch })
        .filter(|ch| !ch.is_control())
        .collect()
}

/// Inline result lines and the number of lines left for the details view.
pub fn summary(name: &str, output: &str, failed: bool) -> (Vec<String>, usize) {
    if !failed && name == "Read" {
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            if let Some(lines) = value["lines"].as_u64() {
                return (vec![format!("Read {lines} lines")], 0);
            }
        }
    }
    let text = result_text(output);
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    if lines.is_empty() {
        return (vec![if failed { "Failed without output" } else { "(No output)" }.into()], 0);
    }
    let shown = lines.len().min(SUMMARY_LINES);
    (
        lines[..shown].iter().map(|line| printable(line)).collect(),
        lines.len() - shown,
    )
}

/// The todo list as the reference CLI shows it: `☒` done (struck through),
/// `☐` open, the item in progress in bold.
fn todo_lines(input: &Value, failed: bool) -> Option<Vec<Line>> {
    let todos = input["todos"].as_array().filter(|todos| !todos.is_empty())?;
    let mut lines = vec![change_view::tool_call("Update Todos", "", failed)];
    for (index, todo) in todos.iter().enumerate() {
        let text = printable(todo["content"].as_str().unwrap_or_default());
        let status = todo["status"].as_str().unwrap_or("pending");
        lines.push(change_view::todo_item(&text, status, index == 0));
    }
    Some(lines)
}

/// `● Tool(args) · 0.4s` followed by a short result.
pub fn finished(
    name: &str,
    input: &Value,
    output: &str,
    failed: bool,
    elapsed: Option<Duration>,
    workspace: Option<&std::path::Path>,
) -> Vec<Line> {
    if matches!(name, "TodoWrite" | "todo_write") {
        if let Some(lines) = todo_lines(input, failed) {
            return lines;
        }
    }
    let detail = elapsed
        .map(|elapsed| format!("{:.1}s", elapsed.as_secs_f64()))
        .unwrap_or_default();
    let mut lines = vec![change_view::tool_call(&label(name, input, workspace), &detail, failed)];
    let (shown, hidden) = summary(name, output, failed);
    for (index, line) in shown.iter().enumerate() {
        lines.push(change_view::tool_output(line, index == 0, failed));
    }
    if hidden > 0 {
        lines.push(change_view::tool_output(
            &format!("\u{2026} +{hidden} lines (Ctrl+O to expand)"),
            false,
            false,
        ));
    }
    lines
}

/// The full call for the details view: complete command or input and output.
pub fn expanded(
    name: &str,
    input: &Value,
    output: &str,
    failed: bool,
    workspace: Option<&std::path::Path>,
) -> Vec<Line> {
    let header = match input["command"].as_str() {
        Some(command) if name == "Bash" => format!("{name}({})", super::progress::single_line(command)),
        _ => label(name, input, workspace),
    };
    let mut lines = vec![change_view::tool_call(&header, if failed { "error" } else { "" }, failed)];
    let diff = change_view::details(name, output, failed);
    let text = diff.unwrap_or_else(|| result_text(output));
    let rows: Vec<&str> = text.lines().collect();
    if rows.is_empty() {
        lines.push(change_view::tool_output("(No output)", true, false));
    }
    for (index, row) in rows.iter().take(EXPANDED_LINES).enumerate() {
        lines.push(change_view::tool_output(&printable(row), index == 0, failed));
    }
    if rows.len() > EXPANDED_LINES {
        lines.push(change_view::tool_output(
            &format!("\u{2026} {} more lines; /history shows the full result", rows.len() - EXPANDED_LINES),
            false,
            false,
        ));
    }
    lines
}

/// What the second approval choice grants for the rest of the session.
#[derive(Clone, Debug, PartialEq)]
pub enum Remember {
    /// Switch the session to accept-edits mode.
    AcceptEdits,
    /// Add a session allow rule such as `Bash(npm test)`.
    Rule(String),
}

/// A tool approval prompt: what will run, the question and numbered choices.
pub struct Approval {
    /// Heading and preview, shown above the question.
    pub lines: Vec<Line>,
    pub question: String,
    pub options: Vec<String>,
    pub remember: Option<Remember>,
}

impl Approval {
    /// The prompt as plain text, for terminals without the live menu,
    /// background workers and remote clients.
    pub fn text(&self) -> String {
        let mut text = change_view::plain(&self.lines);
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&self.question);
        for (index, option) in self.options.iter().enumerate() {
            text.push_str(&format!("\n  {}. {option}", index + 1));
        }
        let last = self.options.len();
        text.push_str(&format!("\n(1/y yes{}, {last}/n no)", if self.remember.is_some() { ", 2 yes for this session" } else { "" }));
        text
    }

    /// Whether an answer allows the call, and whether it also remembers it.
    pub fn decide(&self, answer: &str) -> (bool, bool) {
        let answer = answer.trim().to_lowercase();
        if ["y", "yes", "1", "\u{662f}", "\u{5141}\u{8bb8}"].contains(&answer.as_str()) {
            (true, false)
        } else if answer == "2" && self.remember.is_some() {
            (true, true)
        } else {
            (false, false)
        }
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |name| name.to_string_lossy().into_owned())
}

/// The prefix the reference CLI offers to remember: the program and its
/// subcommand, `npm run` for `npm run build --silent`. A program
/// called only with arguments, such as `rm notes.txt`, keeps the whole command
/// so that one answer never allows every use of it.
fn command_prefix(command: &str) -> Option<String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let plain = |word: &str| {
        word.starts_with(|c: char| c.is_ascii_lowercase())
            && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
    };
    if words.len() < 2 || !plain(words[0]) || !plain(words[1]) {
        return None;
    }
    Some(words[..2].join(" "))
}

/// A rule that allows this call again, when one can be written safely.
fn session_rule(name: &str, input: &Value) -> Option<String> {
    let rule = match (name, input["command"].as_str()) {
        ("Bash", Some(command)) => {
            let command = command.trim();
            // Allow rules never match chained or substituted commands, so
            // remembering one would not stop the next prompt.
            if command.is_empty()
                || command.contains(['\n', '\r', '*', '(', ')', ';', '&', '|', '`', '$', '<', '>'])
            {
                return None;
            }
            match command_prefix(command) {
                Some(prefix) => format!("Bash({prefix} *)"),
                None => format!("Bash({command})"),
            }
        }
        ("Bash" | "PowerShell", _) => return None,
        _ => name.to_owned(),
    };
    claude_core::permissions::patterns::validate(&rule).ok().map(|_| rule)
}

/// `Yes, and don’t ask again for: npm run *`, as in the reference CLI.
fn remember_option(rule: &str) -> String {
    let subject = rule
        .strip_prefix("Bash(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(rule);
    format!("Yes, and don\u{2019}t ask again for: {subject}")
}

/// The approval prompt for one tool call. File edits preview the diff they
/// would make; other calls show the command or their input.
pub fn approval(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> Approval {
    let edits = matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit");
    let preview = workspace
        .and_then(|workspace| claude_core::tools::file_change::preview(name, input, workspace));
    let mut lines = Vec::new();
    let question = match (&preview, input["file_path"].as_str()) {
        (Some(change), Some(path)) => {
            let created = change["created"] == true;
            let shown = super::display_path::label(change["path"].as_str().unwrap_or(path), workspace);
            lines.push(change_view::heading(match (name, created) {
                ("Write", true) => "Create file",
                ("Write", false) => "Overwrite file",
                _ => "Edit file",
            }));
            lines.push(change_view::muted(&shown));
            lines.extend(change_view::diff_rows(change, path).unwrap_or_default());
            match (name, created) {
                ("Write", true) => format!("Do you want to create {}?", file_name(path)),
                ("Write", false) => format!("Do you want to overwrite {}?", file_name(path)),
                _ => format!("Do you want to make this edit to {}?", file_name(path)),
            }
        }
        _ => {
            match (name, input["command"].as_str()) {
                ("Bash" | "PowerShell", Some(command)) => {
                    // As in the reference CLI: the description, then the command
                    // between dashed rules.
                    let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols.saturating_sub(1).max(20) as usize);
                    let rule = change_view::muted(&"\u{254c}".repeat(width));
                    lines.push(change_view::heading(&format!("{name} command")));
                    if let Some(description) = input["description"].as_str() {
                        lines.push(change_view::muted(&printable(description)));
                    }
                    lines.push(rule.clone());
                    for line in command.lines().take(12) {
                        lines.push(change_view::body(&printable(line)));
                    }
                    lines.push(rule);
                    lines.push(change_view::body("This command requires approval"));
                }
                _ => {
                    lines.push(change_view::heading(&label(name, input, workspace)));
                    let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
                    let rows: Vec<&str> = pretty.lines().collect();
                    for row in rows.iter().take(12) {
                        lines.push(change_view::muted(&format!("  {}", printable(row))));
                    }
                    if rows.len() > 12 {
                        lines.push(change_view::muted(&format!("  \u{2026} {} more lines", rows.len() - 12)));
                    }
                }
            }
            "Do you want to proceed?".to_owned()
        }
    };
    let remember = if edits { Some(Remember::AcceptEdits) } else { session_rule(name, input).map(Remember::Rule) };
    let mut options = vec!["Yes".to_owned()];
    match &remember {
        Some(Remember::AcceptEdits) => options.push("Yes, allow all edits during this session (shift+tab)".into()),
        Some(Remember::Rule(rule)) => options.push(remember_option(rule)),
        None => {}
    }
    options.push("No".into());
    Approval { lines, question, options, remember }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| change_view::render(line, usize::MAX, false)).collect()
    }

    #[test]
    fn finished_calls_show_one_header_and_a_short_result() {
        let bash = json!({"exit_code":0,"stdout":"a\nb\nc\nd\ne\n","stderr":""}).to_string();
        let lines = text(&finished("Bash", &json!({"command":"ls -la"}), &bash, false, Some(Duration::from_millis(120)), None));
        assert_eq!(lines[0], "\u{25cf} Bash(ls -la) \u{b7} 0.1s");
        assert_eq!(lines[1], "  \u{23bf}  a");
        assert_eq!(lines[3], "     c");
        assert_eq!(lines[4], "     \u{2026} +2 lines (Ctrl+O to expand)");

        let read = json!({"content":"1\tx","lines":2,"truncated":false}).to_string();
        let lines = text(&finished("Read", &json!({"file_path":"README.md"}), &read, false, None, None));
        assert_eq!(lines, vec!["\u{25cf} Read(README.md)", "  \u{23bf}  Read 2 lines"]);

        let lines = text(&finished("Bash", &json!({"command":"cargo x"}), "Bash is not available in plan mode", true, None, None));
        assert_eq!(lines[1], "  \u{23bf}  Bash is not available in plan mode");
    }

    #[test]
    fn failing_commands_report_stderr_and_exit_code() {
        let output = json!({"exit_code":2,"stdout":"","stderr":"no such file\n"}).to_string();
        assert_eq!(result_text(&output), "no such file\nExit code 2");
        assert_eq!(result_text(&format!("Error: {output}")), "no such file\nExit code 2");
        assert_eq!(result_text("Error: permission denied"), "Error: permission denied");
        let lines = text(&expanded("Bash", &json!({"command":"cat missing"}), &output, true, None));
        assert_eq!(lines[0], "\u{25cf} Bash(cat missing) \u{b7} error");
        assert_eq!(lines[2], "     Exit code 2");
    }

    #[test]
    fn approval_questions_show_the_command_and_numbered_choices() {
        let bash = approval("Bash", &json!({"command":"ls -la","description":"List files"}), None);
        let text = bash.text();
        assert!(text.starts_with("Bash command\nList files\n\u{254c}"), "{text}");
        assert!(text.contains("\nls -la\n\u{254c}"), "{text}");
        assert!(text.ends_with("This command requires approval\nDo you want to proceed?\n  1. Yes\n  2. Yes, and don\u{2019}t ask again for: ls -la\n  3. No\n(1/y yes, 2 yes for this session, 3/n no)"), "{text}");
        assert_eq!(bash.decide("2"), (true, true));
        assert_eq!(bash.decide("y"), (true, false));
        assert_eq!(bash.decide("3"), (false, false));
        let prefix = approval("Bash", &json!({"command":"npm run build --silent"}), None);
        assert_eq!(prefix.remember, Some(Remember::Rule("Bash(npm run *)".into())));
        assert_eq!(prefix.options[1], "Yes, and don\u{2019}t ask again for: npm run *");
        assert_eq!(session_rule("Bash", &json!({"command":"git status --short"})).as_deref(), Some("Bash(git status *)"));
        // A program with only arguments is remembered exactly, never as `rm:*`.
        assert_eq!(session_rule("Bash", &json!({"command":"rm notes.txt"})).as_deref(), Some("Bash(rm notes.txt)"));
        // Allow rules never match chained commands, so none is offered.
        assert_eq!(approval("Bash", &json!({"command":"npm test && npm run lint"}), None).options, ["Yes", "No"]);
        let wildcard = approval("Bash", &json!({"command":"rm -rf *"}), None);
        assert_eq!(wildcard.options, ["Yes", "No"]);
        assert_eq!(wildcard.decide("2"), (false, false));
        let write = approval("Write", &json!({"file_path":"a.txt","content":"x"}), None);
        assert!(write.text().starts_with("Write(a.txt)\n  {"), "{}", write.text());
        assert_eq!(write.remember, Some(Remember::AcceptEdits));
    }

    #[test]
    fn read_only_calls_fold_into_one_summary() {
        let kinds: Vec<Gathering> = [
            ("Grep", json!({"pattern":"TODO"})),
            ("Glob", json!({"pattern":"**/*.py"})),
            ("Read", json!({"file_path":"a.py"})),
            ("Bash", json!({"command":"ls -la src"})),
        ]
        .iter()
        .filter_map(|(name, input)| gathering(name, input))
        .collect();
        assert_eq!(gathering_summary(&kinds, true), "searching for 2 patterns, reading 1 file, listing 1 directory\u{2026}");
        assert_eq!(gathering_summary(&kinds, false), "searched for 2 patterns, read 1 file, listed 1 directory");
        assert_eq!(gathering("Bash", &json!({"command":"git log --oneline | head"})), Some(Gathering::Shell));
        assert_eq!(gathering("Bash", &json!({"command":"touch x"})), None);
        assert_eq!(gathering("Edit", &json!({})), None);
    }

    #[test]
    fn edit_approvals_preview_the_diff_they_would_make() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("calc.py"), "def divide(a, b):\n    return a / b\n").unwrap();
        let input = json!({"file_path":"calc.py","old_string":"    return a / b","new_string":"    if b == 0:\n        raise ValueError\n    return a / b"});
        let edit = approval("Edit", &input, Some(temp.path()));
        let text = edit.text();
        assert!(text.starts_with("Edit file\ncalc.py\n"), "{text}");
        assert!(text.contains("    2 +    if b == 0:"), "{text}");
        assert!(text.contains("Do you want to make this edit to calc.py?\n  1. Yes\n  2. Yes, allow all edits during this session (shift+tab)\n  3. No"), "{text}");
        let created = approval("Write", &json!({"file_path":"new.txt","content":"hi\n"}), Some(temp.path()));
        assert_eq!(created.question, "Do you want to create new.txt?");
    }

    #[test]
    fn todo_lists_render_as_checkboxes() {
        let input = json!({"todos":[
            {"content":"Read the code","status":"completed"},
            {"content":"Fix the bug","status":"in_progress"},
            {"content":"Run tests","status":"pending"}
        ]});
        let lines = text(&finished("TodoWrite", &input, "{}", false, None, None));
        assert_eq!(lines, ["\u{25cf} Update Todos", "  \u{23bf}  \u{2612} Read the code", "     \u{2610} Fix the bug", "     \u{2610} Run tests"]);
    }
}
