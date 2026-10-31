//! Transcript rendering for finished tool calls: one `● Tool(args)` header with
//! a short `⎿` result, and a full form for the Ctrl+O details view.
use super::change_view::{self, Line};
use serde_json::Value;
use std::time::Duration;

/// Lines of a successful result shown inline before "+N lines".
const SUMMARY_LINES: usize = 4;
/// Lines of one result printed by the details view before truncation.
const EXPANDED_LINES: usize = 400;

/// `Bash(ls -la)` for previewable inputs, otherwise the tool name.
pub fn label(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> String {
    if matches!(name, "Grep" | "Glob") {
        let pattern = printable(input["pattern"].as_str().unwrap_or_default());
        let path = input["path"].as_str().map(|path| super::display_path::label(path, workspace));
        return match path {
            Some(path) => format!("{name}(pattern: {pattern}, path: {})", printable(&path)),
            None => format!("{name}({pattern})"),
        };
    }
    // Preserve the file name even when its directory is longer than the preview limit.
    if let Some(path) = input["file_path"].as_str().or_else(|| input["path"].as_str()) {
        return format!("{}({})", printable(name), printable(&super::display_path::label(path, workspace)));
    }
    // Agents read as their short description, as in the reference CLI.
    if let Some(description) = input["description"].as_str().filter(|text| name == "Agent" && !text.trim().is_empty()) {
        return format!("{}({})", super::progress::single_line(name), printable(description));
    }
    let preview = super::progress::tool_preview_in_workspace(name, input, workspace);
    let name = super::progress::single_line(super::progress::display_name(name));
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
            if !deepseek_code_core::permissions::shell_read_only::is_read_only(command) {
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
    if !failed && serde_json::from_str::<Value>(output).is_ok_and(|value| value["backgrounded"] == true) {
        if name == "Agent" {
            return (vec!["Backgrounded agent (\u{2193} to manage \u{b7} ctrl+o to expand)".into()], 0);
        }
        return (vec!["Running in the background (\u{2193} to manage)".into()], 0);
    }
    // The model reads the full rejection text; the transcript shows the answer.
    if let Some(rest) = output.split_once(REJECTED).map(|(_, rest)| rest) {
        let note = rest.split_once("the user said:\n").map(|(_, note)| printable(note.trim()));
        return (vec![note.map_or_else(|| "Rejected".to_owned(), |note| format!("Rejected: {note}"))], 0);
    }
    let text = result_text(output);
    let lines: Vec<&str> = text.lines().collect();
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
    if name == "TodoWrite" {
        if let Some(lines) = todo_lines(input, failed) {
            return lines;
        }
    }
    let _ = elapsed;
    let (shown, hidden) = summary(name, output, failed);
    let mut lines = vec![change_view::tool_call(&label(name, input, workspace), "", failed)];
    for (index, line) in shown.iter().enumerate() {
        lines.push(change_view::tool_output(line, index == 0, failed));
    }
    if hidden > 0 {
        lines.push(change_view::muted(&format!("     +{hidden} lines (click to expand)")));
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

/// Expanded inline block, with timing and a clickable collapse footer.
pub fn expanded_block(
    name: &str, input: &Value, output: &str, failed: bool,
    elapsed: Option<Duration>, workspace: Option<&std::path::Path>,
) -> Vec<Line> {
    let mut lines = expanded(name, input, output, failed, workspace);
    if let Some(elapsed) = elapsed {
        lines.push(change_view::muted(&format!("  • {:.2}s", elapsed.as_secs_f64())));
    }
    lines.push(change_view::muted("     − Collapse"));
    lines
}

/// What the second approval choice grants for the rest of the session.
#[derive(Clone, Debug, PartialEq)]
pub enum Remember {
    /// Switch the session to accept-edits mode.
    AcceptEdits,
    /// Add a session allow rule such as `Bash(npm test)`.
    Rule(String),
    /// Switch the session to auto mode.
    Auto,
}

/// The reference CLI's text for a rejected call; a bare "No" also ends the turn.
pub const REJECTED: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file).";

/// The answer to an approval prompt.
#[derive(Debug, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    pub grant: Option<Remember>,
    /// Text added with Tab: "Yes, use -5 instead" or "No, keep the readme".
    pub note: Option<String>,
}
impl Decision {
    /// What the model reads for a rejection, and whether the turn stops.
    pub fn rejection(&self) -> (String, bool) {
        match &self.note {
            Some(note) => (format!("{REJECTED} To tell you how to proceed, the user said:\n{note}"), false),
            None => (format!("{REJECTED} STOP what you are doing and wait for the user to tell you how to proceed."), true),
        }
    }
}

/// A tool approval prompt: what will run, the question and numbered choices.
pub struct Approval {
    /// Heading and preview, shown above the question.
    pub lines: Vec<Line>,
    pub question: String,
    pub options: Vec<String>,
    pub remember: Option<Remember>,
    /// Whether a "switch to auto mode" choice follows the remember choice.
    pub auto: bool,
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
        let mut hints = vec!["1/y yes".to_owned()];
        if self.remember.is_some() {
            hints.push("2 yes for this session".into());
        }
        if self.auto {
            hints.push(format!("{} switch to auto mode", self.options.len() - 1));
        }
        hints.push(format!("{}/n no", self.options.len()));
        text.push_str(&format!("\n({})", hints.join(", ")));
        text
    }

    /// The choice an answer makes. The live menu answers with the option
    /// number, followed on the next line by any text added with Tab.
    pub fn decide(&self, answer: &str) -> Decision {
        let (choice, note) = answer.split_once('\n').unwrap_or((answer, ""));
        let note = Some(note.trim().to_owned()).filter(|note| !note.is_empty());
        let last = self.options.len();
        let index = match choice.trim().to_lowercase().as_str() {
            "y" | "yes" | "\u{662f}" | "\u{5141}\u{8bb8}" => 1,
            number => number.parse().unwrap_or(last),
        };
        let grant = match index {
            2 if self.remember.is_some() => self.remember.clone(),
            index if self.auto && index == last - 1 => Some(Remember::Auto),
            _ => None,
        };
        Decision { allowed: index == 1 || grant.is_some(), grant, note }
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |name| name.to_string_lossy().into_owned())
}

/// Programs that delete, overwrite, run code or reach the network are only
/// ever remembered as the exact command.
const EXACT_ONLY: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "dd", "ln", "chmod", "chown", "chgrp", "truncate", "shred", "tee",
    "sudo", "su", "doas", "kill", "pkill", "killall", "curl", "wget", "ssh", "scp", "rsync", "nc",
    "sh", "bash", "zsh", "fish", "env", "eval", "exec", "xargs", "find", "python", "python3",
    "node", "deno", "perl", "ruby", "php", "osascript", "powershell", "pwsh",
];

/// The prefix the reference CLI offers to remember: the program and its
/// subcommand (`npm run` for `npm run build --silent`), or the program alone
/// when options come first (`git` for `git -C src log`). A program called
/// only with arguments, such as `touch notes.txt`, keeps the whole command.
fn command_prefix(command: &str) -> Option<String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let plain = |word: &str| {
        word.starts_with(|c: char| c.is_ascii_lowercase())
            && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
    };
    if words.len() < 2 || !plain(words[0]) || EXACT_ONLY.contains(&words[0]) {
        return None;
    }
    if plain(words[1]) {
        Some(words[..2].join(" "))
    } else if words[1].starts_with('-') {
        Some(words[0].to_owned())
    } else {
        None
    }
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
        // As in the reference CLI: don't ask again for this host.
        ("WebFetch", _) => format!("WebFetch(domain:{})", fetch_host(input)?),
        ("SandboxNetwork", _) => format!("SandboxNetwork(domain:{})", fetch_host(input)?),
        _ => name.to_owned(),
    };
    deepseek_code_core::permissions::patterns::validate(&rule).ok().map(|_| rule)
}

/// The host a WebFetch call would contact.
fn fetch_host(input: &Value) -> Option<String> {
    deepseek_code_core::permissions::web_fetch::host(input["url"].as_str()?).filter(|host| deepseek_code_core::permissions::patterns::validate(&format!("WebFetch(domain:{host})")).is_ok())
}

/// `Yes, and don’t ask again for: npm run *`, as in the reference CLI.
fn remember_option(rule: &str) -> String {
    if let Some(host) = rule.strip_prefix("WebFetch(domain:").or_else(|| rule.strip_prefix("SandboxNetwork(domain:")).and_then(|rest| rest.strip_suffix(')')) {
        return format!("Yes, and don\u{2019}t ask again for {host}");
    }
    let subject = rule
        .strip_prefix("Bash(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(rule);
    format!("Yes, and don\u{2019}t ask again for: {subject}")
}

/// The approval prompt for one tool call. File edits preview the diff they
/// would make; other calls show the command or their input.
pub fn approval(name: &str, input: &Value, workspace: Option<&std::path::Path>, auto: bool) -> Approval {
    let edits = matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit");
    let preview = workspace
        .and_then(|workspace| deepseek_code_core::tools::file_change::preview(name, input, workspace));
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
                // A sandboxed command reaching a host outside allowedDomains.
                ("SandboxNetwork", _) => {
                    lines.push(change_view::heading("Network request outside of sandbox"));
                    lines.push(change_view::body(&format!("  Host: {}", printable(input["host"].as_str().unwrap_or_default()))));
                }
                // The reference CLI's fetch approval: the URL and its host.
                ("WebFetch", _) if fetch_host(input).is_some() => {
                    let host = fetch_host(input).unwrap_or_default();
                    lines.push(change_view::heading("Fetch"));
                    lines.push(change_view::body(&format!("  {}", printable(input["url"].as_str().unwrap_or_default()))));
                    lines.push(change_view::muted(&format!("  DeepSeek Code wants to fetch content from {host}")));
                }
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
                    for line in command.lines() {
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
            if name == "WebFetch" && fetch_host(input).is_some() {
                "Do you want to allow DeepSeek Code to fetch this content?".to_owned()
            } else if name == "SandboxNetwork" {
                "Do you want to allow this connection?".to_owned()
            } else {
                "Do you want to proceed?".to_owned()
            }
        }
    };
    let remember = if edits { Some(Remember::AcceptEdits) } else { session_rule(name, input).map(Remember::Rule) };
    let mut options = vec!["Yes".to_owned()];
    match &remember {
        Some(Remember::AcceptEdits) => options.push("Yes, allow all edits during this session (shift+tab)".into()),
        Some(Remember::Rule(rule)) => options.push(remember_option(rule)),
        _ => {}
    }
    if auto {
        options.push("Yes, and switch to auto mode \u{b7} auto mode handles these prompts for you".into());
    }
    options.push("No".into());
    Approval { lines, question, options, remember, auto }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| change_view::render(line, usize::MAX, false)).collect()
    }

    #[test]
    fn search_blocks_show_tool_pattern_path_and_result() {
        for name in ["Grep", "Glob"] {
            let input = serde_json::json!({"pattern": "TODO", "path": "src"});
            let text = change_view::plain(&finished(name, &input, "src/main.rs:12: TODO", false, None, None));
            assert!(text.contains(name));
            assert!(text.contains("TODO"));
            assert!(text.contains("src"));
            assert!(!text.contains("Searched for"));
        }
    }

    #[test]
    fn web_tools_use_the_reference_names() {
        assert_eq!(label("WebFetch", &json!({"url":"https://example.com/","prompt":"title?"}), None), "Fetch(https://example.com/)");
        assert_eq!(label("WebSearch", &json!({"query":"rust 1.85"}), None), "Web Search(\"rust 1.85\")");
    }

    #[test]
    fn web_fetch_approval_names_the_host_and_remembers_it() {
        let approval = approval("WebFetch", &json!({"url":"https://Example.com/a?b=1","prompt":"p"}), None, false);
        assert_eq!(approval.question, "Do you want to allow DeepSeek Code to fetch this content?");
        assert!(text(&approval.lines).iter().any(|line| line.contains("wants to fetch content from example.com")));
        assert_eq!(approval.options, ["Yes", "Yes, and don\u{2019}t ask again for example.com", "No"]);
        assert!(matches!(&approval.remember, Some(Remember::Rule(rule)) if rule == "WebFetch(domain:example.com)"));
    }

    #[test]
    fn sandbox_host_approval_matches_the_reference() {
        let approval = approval("SandboxNetwork", &json!({"host":"pypi.org","url":"https://pypi.org/"}), None, false);
        assert_eq!(approval.question, "Do you want to allow this connection?");
        assert_eq!(text(&approval.lines), ["Network request outside of sandbox", "  Host: pypi.org"]);
        assert_eq!(approval.options, ["Yes", "Yes, and don\u{2019}t ask again for pypi.org", "No"]);
        assert!(matches!(&approval.remember, Some(Remember::Rule(rule)) if rule == "SandboxNetwork(domain:pypi.org)"));
    }

    #[test]
    fn finished_calls_show_one_header_and_a_short_result() {
        let bash = json!({"exit_code":0,"stdout":"a\nb\nc\nd\ne\n","stderr":""}).to_string();
        let lines = text(&finished("Bash", &json!({"command":"ls -la"}), &bash, false, Some(Duration::from_millis(120)), None));
        assert_eq!(lines[0], "\u{25cf} Bash(ls -la)");
        assert_eq!(lines[1], "  \u{23bf}  a");
        assert_eq!(lines[3], "     c");
        assert_eq!(lines[4], "     d");
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[5], "     +1 lines (click to expand)");
        let full = text(&expanded_block("Bash", &json!({"command":"ls"}), &bash, false, Some(Duration::from_millis(2050)), None));
        assert_eq!(&full[full.len()-2..], &["  • 2.05s", "     − Collapse"]);

        let read = json!({"content":"1\tx","lines":2,"truncated":false}).to_string();
        let lines = text(&finished("Read", &json!({"file_path":"README.md"}), &read, false, None, None));
        assert_eq!(lines, vec!["\u{25cf} Read(README.md)", "  \u{23bf}  1 x"]);

        let lines = text(&finished("Bash", &json!({"command":"cargo x"}), "Bash is not available in plan mode", true, None, None));
        assert_eq!(lines[1], "  \u{23bf}  Bash is not available in plan mode");
    }

    #[test]
    fn rejected_calls_show_the_answer_not_the_model_text() {
        let denied = Decision { allowed: false, grant: None, note: Some("keep it".into()) }.rejection().0;
        let lines = text(&finished("Bash", &json!({"command":"rm a"}), &format!("Tool approval failed: {denied}"), true, None, None));
        assert_eq!(lines[1], "  \u{23bf}  Rejected: keep it");
        let stopped = Decision { allowed: false, grant: None, note: None }.rejection().0;
        assert_eq!(summary("Bash", &stopped, true).0, ["Rejected"]);
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
        let bash = approval("Bash", &json!({"command":"ls -la","description":"List files"}), None, false);
        let text = bash.text();
        assert!(text.starts_with("Bash command\nList files\n\u{254c}"), "{text}");
        assert!(text.contains("\nls -la\n\u{254c}"), "{text}");
        assert!(text.ends_with("This command requires approval\nDo you want to proceed?\n  1. Yes\n  2. Yes, and don\u{2019}t ask again for: ls *\n  3. No\n(1/y yes, 2 yes for this session, 3/n no)"), "{text}");
        let rule = Some(Remember::Rule("Bash(ls *)".into()));
        assert_eq!(bash.decide("2"), Decision { allowed: true, grant: rule, note: None });
        assert_eq!(bash.decide("y"), Decision { allowed: true, grant: None, note: None });
        assert_eq!(bash.decide("3"), Decision { allowed: false, grant: None, note: None });
        // Tab adds a note to Yes or No.
        assert_eq!(bash.decide("1\nuse -5 instead"), Decision { allowed: true, grant: None, note: Some("use -5 instead".into()) });
        let no = bash.decide("3\nkeep the readme");
        assert!(!no.allowed);
        assert_eq!(no.rejection(), (format!("{REJECTED} To tell you how to proceed, the user said:\nkeep the readme"), false));
        assert!(bash.decide("n").rejection().1, "a bare No ends the turn");
        // With auto mode configured, a third choice switches to it.
        let auto = approval("Bash", &json!({"command":"touch notes.txt"}), None, true);
        assert_eq!(auto.options[2], "Yes, and switch to auto mode \u{b7} auto mode handles these prompts for you");
        assert_eq!(auto.decide("3").grant, Some(Remember::Auto));
        assert!(!auto.decide("4").allowed);
        assert!(auto.text().ends_with("(1/y yes, 2 yes for this session, 3 switch to auto mode, 4/n no)"));
        let prefix = approval("Bash", &json!({"command":"npm run build --silent"}), None, false);
        assert_eq!(prefix.remember, Some(Remember::Rule("Bash(npm run *)".into())));
        assert_eq!(prefix.options[1], "Yes, and don\u{2019}t ask again for: npm run *");
        assert_eq!(session_rule("Bash", &json!({"command":"git status --short"})).as_deref(), Some("Bash(git status *)"));
        // Options first: the program alone, as the reference CLI offers `git *`.
        assert_eq!(session_rule("Bash", &json!({"command":"git -C src log --oneline -3"})).as_deref(), Some("Bash(git *)"));
        // A program with only arguments, or one that can destroy data or run
        // code, is remembered exactly.
        assert_eq!(session_rule("Bash", &json!({"command":"touch notes.txt"})).as_deref(), Some("Bash(touch notes.txt)"));
        assert_eq!(session_rule("Bash", &json!({"command":"rm -rf build"})).as_deref(), Some("Bash(rm -rf build)"));
        assert_eq!(session_rule("Bash", &json!({"command":"python3 -m pytest"})).as_deref(), Some("Bash(python3 -m pytest)"));
        // Allow rules never match chained commands, so none is offered.
        assert_eq!(approval("Bash", &json!({"command":"npm test && npm run lint"}), None, false).options, ["Yes", "No"]);
        let wildcard = approval("Bash", &json!({"command":"rm -rf *"}), None, false);
        assert_eq!(wildcard.options, ["Yes", "No"]);
        assert!(!wildcard.decide("2").allowed);
        let write = approval("Write", &json!({"file_path":"a.txt","content":"x"}), None, false);
        assert!(write.text().starts_with("Write(a.txt)\n  {"), "{}", write.text());
        assert_eq!(write.remember, Some(Remember::AcceptEdits));
    }

    #[test]
    fn long_read_paths_keep_the_file_name_in_both_views() {
        let path = format!("{}Config.java", "directory/".repeat(20));
        let input = json!({"file_path":path});
        assert!(label("Read", &input, None).contains(&path));
        let full = change_view::plain(&expanded("Read", &input, "content", false, None));
        assert!(full.contains(&path));
        let short = change_view::plain(&finished("Read", &input, "content", false, None, None));
        assert!(short.contains(&path));
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
        let edit = approval("Edit", &input, Some(temp.path()), false);
        let text = edit.text();
        assert!(text.starts_with("Edit file\ncalc.py\n"), "{text}");
        assert!(text.contains("    2 +    if b == 0:"), "{text}");
        assert!(text.contains("Do you want to make this edit to calc.py?\n  1. Yes\n  2. Yes, allow all edits during this session (shift+tab)\n  3. No"), "{text}");
        let created = approval("Write", &json!({"file_path":"new.txt","content":"hi\n"}), Some(temp.path()), false);
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

/// One disclosure for a completed reasoning segment. Missing historical timing stays unknown.
pub fn thought(text: &str, seconds: Option<u64>) -> (Vec<Line>, Vec<Line>) {
    let text = text.trim_end();
    let count = text.lines().count();
    let title = match seconds {
        Some(seconds) => format!("Thought for {seconds}s {count} lines"),
        None => format!("Thought {count} lines"),
    };
    let short = vec![change_view::muted(&format!("{title} (click to expand)"))];
    let mut full = vec![change_view::muted(&title)];
    full.extend(text.lines().map(|row| change_view::body(&printable(row))));
    full.push(change_view::muted("(click to collapse)"));
    (short, full)
}
