//! Transcript rendering for finished tool calls: one `● Tool(args)` header with
//! a short `⎿` result, and a full form for the Ctrl+O details view.
use super::change_view::{self, Line};
use serde_json::Value;
use std::time::Duration;

/// Lines of a successful result shown inline before "+N lines".
const SUMMARY_LINES: usize = 4;
/// Lines of one result printed by the details view before truncation.
const EXPANDED_LINES: usize = 400;

/// Describe task retrieval without exposing the internal tool name in progress.
pub fn task_output_activity(name: &str, input: &Value) -> Option<&'static str> {
    matches!(name, "TaskGet" | "TaskOutput").then(|| {
        if input["block"] == true && input["timeout"] != 0 {
            "Waiting for background task to finish"
        } else {
            "Reading background task output"
        }
    })
}

/// The numbered form of a call's lines: `● #3 Bash(ls -la)`.
pub fn numbered(mut lines: Vec<Line>, number: Option<usize>) -> Vec<Line> {
    if let (Some(number), Some(first)) = (number, lines.first_mut()) {
        change_view::number_header(first, number);
    }
    lines
}

/// A display-only MCP label. Routing and stored input keep their raw identity.
pub(crate) fn mcp_label(name: &str, input: &Value) -> Option<String> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    let clean = |text: &str| super::progress::single_line(&plain_terminal_text(text));
    let title = if server == "codegraph" && tool == "codegraph_explore" {
        "Exploration".to_owned()
    } else {
        format!(
            "{} / {}",
            clean(server),
            clean(&tool.replace(['_', '-'], " "))
        )
    };
    let argument = [
        "query",
        "url",
        "file_path",
        "path",
        "pattern",
        "operation",
        "prompt",
        "name",
        "id",
    ]
    .iter()
    .find_map(|key| {
        input[*key]
            .as_str()
            .filter(|value| !value.trim().is_empty())
    })
    .map(clean);
    Some(match argument {
        Some(argument) => {
            let preview: String = argument.chars().take(100).collect();
            format!(
                "{title}: {preview}{}",
                if argument.chars().count() > 100 {
                    "…"
                } else {
                    ""
                }
            )
        }
        None => title,
    })
}

/// Full MCP identity and input are visible without opening details.
pub(crate) fn mcp_call(name: &str, input: &Value) -> Option<String> {
    mcp_label(name, input)?;
    // JSON escapes control characters rather than allowing terminal execution.
    Some(format!(
        "{}({input})",
        printable(&plain_terminal_text(name))
    ))
}

pub(super) fn is_mcp_header(line: &Line) -> bool {
    line.0.first().is_some_and(|span| span.text == "● ")
        && line.0.iter().any(|span| span.text.starts_with("mcp__"))
}

pub(super) fn is_shell_header(line: &Line) -> bool {
    line.0.first().is_some_and(|span| span.text == "● ")
        && line
            .0
            .iter()
            .any(|span| span.text.starts_with("Bash(") || span.text.starts_with("PowerShell("))
}

/// Wrap literal calls without Markdown's whitespace trimming.
pub(super) fn wrap_call(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(2);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0;
    for ch in plain_terminal_text(text).chars() {
        let size = ch.width().unwrap_or(0);
        if cells + size > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            cells = 0;
        }
        row.push(ch);
        cells += size;
    }
    rows.push(row);
    rows
}

/// `Bash(ls -la)` for previewable inputs, otherwise the tool name.
pub fn label(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> String {
    if name == "Skill" {
        if let Some(skill) = input["skill"].as_str().or_else(|| input["name"].as_str()) {
            return format!("Skill({})", printable(skill));
        }
    }
    if let Some(label) = mcp_call(name, input) { return label; }
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

pub fn display_label(name: &str, input: &Value, workspace: Option<&std::path::Path>, title: Option<&str>) -> String {
    if let Some(title) = title.filter(|title| !title.trim().is_empty() && matches!(name, "TaskOutput" | "TaskGet" | "TaskStop")) {
        return format!("{name}({})", printable(&plain_terminal_text(title)));
    }
    label(name, input, workspace)
}

pub fn titled(mut lines: Vec<Line>, name: &str, title: Option<&str>, failed: bool) -> Vec<Line> {
    if title.is_some() && matches!(name, "TaskOutput" | "TaskGet" | "TaskStop") {
        if let Some(header) = lines.first_mut() {
            *header = change_view::tool_call(&display_label(name, &Value::Null, None, title), "", failed);
        }
    }
    lines
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
    plain_terminal_text(&raw_result_text(output))
}

/// Tool-specific presentation; stored history and model results stay structured.
pub fn named_result_text(name: &str, output: &str) -> String {
    if name == "git_add" {
        if let Ok(value) = serde_json::from_str::<Value>(output.strip_prefix("Error: ").unwrap_or(output)) {
            if value["success"] == true && value["is_error"] != true && value["error"].is_null() && !output.starts_with("Error: ") {
                if let Some(files) = value["files"].as_array().filter(|files| files.iter().all(Value::is_string)) {
                    let count = files.len();
                    let mut text = format!("Git add completed — {count} requested {}", if count == 1 { "path" } else { "paths" });
                    for file in files { text.push('\n'); text.push_str(file.as_str().unwrap()); }
                    if let Some(details) = value["output"].as_str().filter(|text| !text.trim().is_empty()) { text.push('\n'); text.push_str(details.trim_end()); }
                    return plain_terminal_text(&text);
                }
            }
            if let Some(code) = value["exit_code"].as_i64().filter(|code| *code != 0) {
                return format!("{}\nExit code {code}", result_text(output));
            }
        }
    }
    result_text(output)
}

pub(crate) fn copy_command(name: &str, input: &Value) -> String {
    input["command"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{name}({input})"))
}

/// Sanitize literal terminal text without interpreting it as a tool envelope.
pub fn plain_terminal_text(text: &str) -> String {
    // Strip whole sequences after decoding JSON and before splitting into rows.
    // Filtering only ESC would leave visible fragments such as `[32m` behind.
    struct PlainOutput(String);
    impl vte::Perform for PlainOutput {
        fn print(&mut self, ch: char) {
            self.0.push(ch);
        }

        fn execute(&mut self, byte: u8) {
            if matches!(byte, b'\n' | b'\r' | b'\t') {
                self.0.push(char::from(byte));
            }
        }
    }
    let mut plain = PlainOutput(String::with_capacity(text.len()));
    vte::Parser::new().advance(&mut plain, text.as_bytes());
    plain.0
}

pub(super) fn background_shell_id(name: &str, output: &str, failed: bool) -> Option<String> {
    if failed || !matches!(name, "Bash" | "PowerShell") { return None; }
    let value: Value = serde_json::from_str(output).ok()?;
    if value["status"] != "running" && value["backgrounded"] != true { return None; }
    let id = value["task_id"].as_str()?;
    uuid::Uuid::parse_str(id).ok().map(|id| id.to_string())
}

fn raw_result_text(output: &str) -> String {
    // Failed calls wrap the tool's own JSON result as `Error: {...}`.
    let body = output.strip_prefix("Error: ").unwrap_or(output);
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(text) = deepseek_code_core::mcp::presentation::body(&value) {
            return if output.starts_with("Error: ") {
                format!("Error: {text}")
            } else {
                text
            };
        }
    }
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return output.trim_end().to_owned();
    };
    if let Some(diff) = fields.get("diff").and_then(Value::as_str) {
        return if diff.is_empty() { "(No changes)".into() } else { diff.trim_end().into() };
    }
    if let Some(agents) = fields.get("agents").and_then(Value::as_array) {
        return agents.iter().map(|agent| format!("- {} · {} · {}", agent["name"].as_str().unwrap_or("Agent").lines().next().unwrap_or("Agent"), agent["kind"].as_str().unwrap_or("agent"), agent["status"].as_str().unwrap_or("unknown"))).collect::<Vec<_>>().join("\n");
    }
    if let Some(instructions) = fields.get("instructions").and_then(Value::as_str) {
        return instructions.trim_end().into();
    }
    if let Some(files) = fields.get("files").and_then(Value::as_array).filter(|files| files.iter().all(|file| file["status"].is_string() && file["file"].is_string())) {
        if files.is_empty() { return "(Working tree clean)".into(); }
        return files.iter().map(|file| {
            let status = match file["status"].as_str().unwrap().trim() {
                "??" => "Untracked", "M" => "Modified", "A" => "Added", "D" => "Deleted", "R" => "Renamed", "C" => "Copied", "UU" => "Conflict", other => other,
            };
            format!("- {status}: {}", file["file"].as_str().unwrap())
        }).collect::<Vec<_>>().join("\n");
    }
    if let Some(message) = fields.get("message").filter(|message| message.is_object()) {
        if let Some(content) = message["content"].as_str() {
            return format!("To: {}\nDelivery: {}\n{}{}", fields.get("recipient_name").or_else(|| fields.get("to")).and_then(Value::as_str).unwrap_or("agent"), fields.get("state").and_then(Value::as_str).unwrap_or("queued"), content, fields.get("note").and_then(Value::as_str).map(|note| format!("\n{note}")).unwrap_or_default());
        }
    }
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
    if let Some(entries) = fields
        .get("entries")
        .and_then(Value::as_array)
        .filter(|entries| {
            entries
                .iter()
                .all(|entry| entry["name"].is_string() && entry["is_dir"].is_boolean())
        })
    {
        if entries.is_empty() {
            return "(Empty directory)".into();
        }
        return entries
            .iter()
            .map(|entry| {
                format!(
                    "- {}{}",
                    entry["name"].as_str().unwrap(),
                    if entry["is_dir"] == true { "/" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if fields.get("approved") == Some(&Value::Bool(true)) && fields.contains_key("planFilePath") {
        if let Some(plan) = fields
            .get("plan")
            .and_then(Value::as_str)
            .filter(|plan| !plan.trim().is_empty())
        {
            return format!(
                "{}\n\n{plan}",
                fields
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Plan approved.")
            );
        }
    }
    if fields.get("mode").and_then(Value::as_str) == Some("plan")
        && fields.contains_key("planFilePath")
    {
        let plan = fields
            .get("plan")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty());
        return match plan {
            Some(plan) => format!("Planning enabled. Current plan:\n{plan}"),
            None => "Planning enabled. No plan has been written yet. The agent will investigate your request, write the plan, and show it for approval before implementation.".into(),
        };
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

fn result_label(name: &str, input: &Value, output: &str, workspace: Option<&std::path::Path>) -> String {
    if matches!(name, "TaskGet" | "TaskOutput" | "TaskStop") {
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            if let Some(subject) = value["subject"].as_str() {
                return format!("{name}({})", printable(subject));
            }
        }
    }
    label(name, input, workspace)
}

/// Inline result lines and the number of lines left for the details view.
pub fn summary(name: &str, output: &str, failed: bool) -> (Vec<String>, usize) {
    if name == "Bash" && !failed && serde_json::from_str::<Value>(output).is_ok_and(|value| {
        value["task_id"].is_string() && value["status"] == "running"
    }) {
        return (vec!["Running in background (click to show output · /tasks to view)".into()], 0);
    }
    if !failed && serde_json::from_str::<Value>(output).is_ok_and(|value| value["backgrounded"] == true) {
        if name == "Agent" {
            return (vec!["Backgrounded agent (\u{2193} to manage \u{b7} ctrl+o to expand)".into()], 0);
        }
        return (vec!["Running in the background (click to show output · \u{2193} to manage)".into()], 0);
    }
    // The model reads the full rejection text; the transcript shows the answer.
    if let Some(rest) = output.split_once(REJECTED).map(|(_, rest)| rest) {
        let note = rest.split_once("the user said:\n").map(|(_, note)| printable(note.trim()));
        return (vec![note.map_or_else(|| "Rejected".to_owned(), |note| format!("Rejected: {note}"))], 0);
    }
    let text = if name == "ExitPlanMode" && !failed {
        serde_json::from_str::<Value>(output)
            .ok()
            .and_then(|value| value["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| named_result_text(name, output))
    } else {
        named_result_text(name, output)
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return (vec![if failed { "Failed without output" } else { "(No output)" }.into()], 0);
    }
    let shown = if name == "ExitPlanMode" && !failed {
        lines.len()
    } else {
        lines.len().min(SUMMARY_LINES)
    };
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
    let mut lines = vec![change_view::tool_call(&result_label(name, input, output, workspace), "", failed)];
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
        Some(command) if matches!(name, "Bash" | "PowerShell") => {
            format!("{name}({})", super::progress::single_line(command))
        }
        _ => result_label(name, input, output, workspace),
    };
    let mut lines = vec![change_view::tool_call(&header, if failed { "error" } else { "" }, failed)];
    let diff = change_view::details(name, output, failed);
    let text = diff.unwrap_or_else(|| named_result_text(name, output));
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

/// The footer of a block that a click expanded.
pub const COLLAPSE_HINT: &str = "     (click to collapse)";

/// Expanded inline block, with timing and a clickable collapse footer.
pub fn expanded_block(
    name: &str, input: &Value, output: &str, failed: bool,
    elapsed: Option<Duration>, workspace: Option<&std::path::Path>,
) -> Vec<Line> {
    let mut lines = expanded(name, input, output, failed, workspace);
    if let Some(elapsed) = elapsed {
        lines.push(change_view::muted(&format!("  • {:.2}s", elapsed.as_secs_f64())));
    }
    lines.push(change_view::muted(COLLAPSE_HINT));
    lines
}

/// What the second approval choice grants for the rest of the session.
#[derive(Clone, Debug, PartialEq)]
pub enum Remember {
    /// Switch the session to accept-edits mode.
    AcceptEdits,
    BashSession,
    EditsSession,
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
            if self.remember == Some(Remember::AcceptEdits) {
                hints.push("2 switch to accept-edits, 3 allow edits for this session".into());
            } else {
                hints.push("2 yes for this session".into());
            }
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
            3 if self.remember == Some(Remember::AcceptEdits) => Some(Remember::EditsSession),
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

/// A rule that allows this call again, when one can be written safely.
fn session_rule(name: &str, input: &Value) -> Option<String> {
    let rule = match (name, input["command"].as_str()) {
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
pub fn approval(
    name: &str,
    input: &Value,
    workspace: Option<&std::path::Path>,
    auto: bool,
) -> Approval {
    let local_fetch = name == "WebFetch"
        && deepseek_code_core::permissions::web_fetch::requires_local_approval(
            input["url"].as_str().unwrap_or_default(),
        );
    let auto = auto && !local_fetch;
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
                    let width = crate::terminal::size().0.saturating_sub(1).max(20) as usize;
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
    if local_fetch {
        lines.push(change_view::muted(
            "  Local or private address: approval applies to this request only.",
        ));
    }
    let remember = if local_fetch {
        None
    } else if name == "Bash" {
        deepseek_code_core::native_engine::permission_response::can_allow_bash_session(name, input)
            .then_some(Remember::BashSession)
    } else if edits {
        Some(Remember::AcceptEdits)
    } else {
        session_rule(name, input).map(Remember::Rule)
    };
    let mut options = vec!["Yes".to_owned()];
    match &remember {
        Some(Remember::AcceptEdits) => options.push("Yes, allow all edits during this session (shift+tab)".into()),
        Some(Remember::Rule(rule)) => options.push(remember_option(rule)),
        Some(Remember::BashSession) => options.push("Allow for Session".into()),
        _ => {}
    }
    if edits && deepseek_code_core::permissions::PermissionManager::is_file_edit_tool(name) {
        options.push("Allow for this session".into());
    }
    if auto {
        options.push("Yes, do not ask again \u{b7} enable and switch to auto mode".into());
    }
    options.push("No".into());
    Approval { lines, question, options, remember, auto }
}

#[cfg(test)]
mod tests {
    #[test]
    fn git_add_text_collapses_by_actual_hidden_lines_and_keeps_failures() {
        let files: Vec<_> = (0..12).map(|index| format!("src/file-{index}.rs")).collect();
        let output = serde_json::json!({"success":true,"files":files,"output":""}).to_string();
        let (shown, hidden) = super::summary("git_add", &output, false);
        assert_eq!(shown, ["Git add completed — 12 requested paths", "src/file-0.rs", "src/file-1.rs", "src/file-2.rs"]);
        assert_eq!(hidden, 9);
        let expanded = super::named_result_text("git_add", &output);
        assert!(expanded.ends_with("src/file-11.rs"));
        assert!(!expanded.contains("\"files\""));
        assert!(super::named_result_text("other", &output).contains("\"files\""));
        let single = r#"{"success":true,"files":["."],"output":""}"#;
        assert_eq!(super::named_result_text("git_add", single), "Git add completed — 1 requested path\n.");
        let failed = r#"{"success":false,"error":"pathspec missing","exit_code":128}"#;
        assert_eq!(super::named_result_text("git_add", failed), "pathspec missing\nExit code 128");
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_result_shows_content_or_explicit_pending_state() {
        let pending = result_text(&serde_json::json!({"mode":"plan","planFilePath":"plan.md","plan":null,"message":"internal instructions"}).to_string());
        assert!(pending.contains("No plan has been written"));
        let ready = result_text(&serde_json::json!({"mode":"plan","planFilePath":"plan.md","plan":"# Visible plan\nInspect permissions"}).to_string());
        assert!(ready.contains("Inspect permissions"));
        assert!(!ready.contains("internal instructions"));
    }

    #[test]
    fn background_shell_binding_rejects_non_shell_and_invalid_ids() {
        let id = uuid::Uuid::new_v4().to_string();
        let output = json!({"task_id":id,"status":"running"}).to_string();
        assert_eq!(background_shell_id("Bash", &output, false), Some(id.clone()));
        assert_eq!(background_shell_id("Bash", &json!({"task_id":id,"backgrounded":true}).to_string(), false), Some(id));
        assert!(background_shell_id("Agent", &output, false).is_none());
        assert!(background_shell_id("Bash", &output, true).is_none());
        assert!(background_shell_id("Bash", r#"{"task_id":"../other","status":"running"}"#, false).is_none());
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| change_view::render(line, usize::MAX, false)).collect()
    }

    #[test]
    fn compound_command_can_enable_auto_without_a_broad_allow_rule() {
        let menu = approval(
            "Bash",
            &json!({"command": "cd repo && git branch backup; git status 2>&1"}),
            None,
            true,
        );
        assert_eq!(menu.options.len(), 4);
        assert_eq!(menu.options[1], "Allow for Session");
        assert_eq!(menu.remember, Some(Remember::BashSession));
        assert_eq!(menu.decide("3").grant, Some(Remember::Auto));
        assert!(!menu.decide("4").allowed);
        assert!(!menu.decide("n").allowed);
        assert!(menu.text().contains("3 switch to auto mode"));
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
    fn local_fetch_menu_offers_only_single_call_approval() {
        let menu = approval(
            "WebFetch",
            &json!({"url":"http://localhost:8080/page"}),
            None,
            true,
        );
        assert_eq!(menu.options, ["Yes", "No"]);
        assert!(menu
            .text()
            .contains("approval applies to this request only"));
        assert!(menu.decide("1").allowed);
        assert!(!menu.decide("2").allowed);
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
        assert_eq!(&full[full.len()-2..], &["  • 2.05s", COLLAPSE_HINT]);

        let read = json!({"content":"1\tx","lines":2,"truncated":false}).to_string();
        let lines = text(&finished("Read", &json!({"file_path":"README.md"}), &read, false, None, None));
        assert_eq!(lines, vec!["\u{25cf} Read(README.md)", "  \u{23bf}  1 x"]);

        let lines = text(&finished("Bash", &json!({"command":"cargo x"}), "Bash is not available in plan mode", true, None, None));
        assert_eq!(lines[1], "  \u{23bf}  Bash is not available in plan mode");
    }

    #[test]
    fn ansi_output_is_clean_in_both_views() {
        let output = json!({
            "exit_code": 2,
            "stdout": "\u{1b}[2m2026-10-31\u{1b}[0m app \u{1b}[32msin\u{1b}[0m [\u{1b}[34minfo\u{1b}[0m]\n\u{1b}]8;;https://example.com\u{1b}\\链接\u{1b}]8;;\u{1b}\\\t正常 [text]\n",
            "stderr": "\u{1b}[31mfailed\u{1b}[0m"
        }).to_string();
        let expected = "2026-10-31 app sin [info]\n链接\t正常 [text]\nfailed\nExit code 2";
        assert_eq!(result_text(&output), expected);
        assert_eq!(result_text(&format!("Error: {output}")), expected);
        let input = json!({"command": "fly logs"});
        let collapsed = text(&finished("Bash", &input, &output, true, None, None));
        let full = text(&expanded("Bash", &input, &output, true, None));
        assert_eq!(&collapsed[1..], &full[1..]);
        assert_eq!(collapsed[1], "  \u{23bf}  2026-10-31 app sin [info]");
        assert_eq!(collapsed[2], "     链接 正常 [text]");
        assert_eq!(collapsed[3], "     failed");
        assert_eq!(collapsed[4], "     Exit code 2");
    }

    #[test]
    fn ansi_sequences_are_removed_before_splitting_rows() {
        let output = "before\u{1b}]0;hidden\ntitle\u{7}after\n\u{1b}[2K\u{1b}[1G中文\u{1b}(B\n\u{1b}Psecret\u{1b}\\end\u{1b}[38;2;1;2;3m!\u{1b}[0m";
        assert_eq!(result_text(output), "beforeafter\n中文\nend!");
        assert_eq!(
            summary("Bash", output, false),
            (vec!["beforeafter".into(), "中文".into(), "end!".into()], 0)
        );
        assert_eq!(
            result_text("ordinary [32m text\n\t中文"),
            "ordinary [32m text\n\t中文"
        );
        assert_eq!(result_text("ok\u{1b}[38;2;"), "ok");
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
        assert!(text.ends_with("This command requires approval\nDo you want to proceed?\n  1. Yes\n  2. Allow for Session\n  3. No\n(1/y yes, 2 yes for this session, 3/n no)"), "{text}");
        let rule = Some(Remember::BashSession);
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
        assert_eq!(auto.options[2], "Yes, do not ask again \u{b7} enable and switch to auto mode");
        assert_eq!(auto.decide("3").grant, Some(Remember::Auto));
        assert!(!auto.decide("4").allowed);
        assert!(auto.text().ends_with("(1/y yes, 2 yes for this session, 3 switch to auto mode, 4/n no)"));
        for command in ["npm run build --silent", "npm test && npm run lint", "python3 -m pytest", "rm -rf build"] {
            let menu = approval("Bash", &json!({"command":command}), None, false);
            assert_eq!(menu.remember, Some(Remember::BashSession));
            assert_eq!(menu.options, ["Yes", "Allow for Session", "No"]);
        }
        let danger = approval("Bash", &json!({"command":"rm -rf /"}), None, false);
        assert_eq!(danger.options, ["Yes", "No"]);
        assert!(!danger.decide("2").allowed);
        let write = approval("Write", &json!({"file_path":"a.txt","content":"x"}), None, false);
        assert!(write.text().starts_with("Write(a.txt)\n  {"), "{}", write.text());
        assert_eq!(write.remember, Some(Remember::AcceptEdits));
        assert_eq!(write.options[2], "Allow for this session");
        assert_eq!(write.decide("3").grant, Some(Remember::EditsSession));
        assert!(!write.decide("4").allowed);
        let auto_write = approval("Edit", &json!({"file_path":"a.txt"}), None, true);
        assert_eq!(auto_write.decide("3").grant, Some(Remember::EditsSession));
        assert_eq!(auto_write.decide("4").grant, Some(Remember::Auto));
        assert!(!auto_write.decide("5").allowed);
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
        assert!(text.contains("Do you want to make this edit to calc.py?\n  1. Yes\n  2. Yes, allow all edits during this session (shift+tab)\n  3. Allow for this session\n  4. No"), "{text}");
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
    let unit = if count == 1 { "line" } else { "lines" };
    let title = match seconds {
        Some(seconds) => format!("Thought for {seconds}s {count} {unit}"),
        None => format!("Thought {count} {unit}"),
    };
    let readable = |text: &str| change_view::styled(text, change_view::Style::TerminalDefault);
    let short = vec![readable(&format!("{title} (click to expand)"))];
    let mut full = vec![readable(&title)];
    full.extend(text.lines().map(|row| readable(&printable(row))));
    full.push(readable("(click to collapse)"));
    (short, full)
}

#[cfg(test)]
mod mcp_display_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_labels_are_shared_and_details_preserve_input() {
        let name = "mcp__codegraph__codegraph_explore";
        let input = json!({"query":"中文符号", "projectPath":"../docs", "maxFiles":7});
        assert_eq!(label(name, &input, None), format!("{name}({input})"));
        assert_eq!(
            super::super::progress::tool_preview_in_workspace(name, &input, None),
            label(name, &input, None)
        );
        let finished = change_view::plain(&finished(name, &input, "result", false, None, None));
        assert!(!finished.contains("Exploration:"));
        let details = change_view::plain(&expanded(name, &input, "result", false, None));
        assert!(details.contains(name));
        assert!(details.contains("projectPath"));
        assert!(details.contains("../docs"));
        assert!(details.contains("maxFiles"));
    }

    #[test]
    fn mcp_labels_handle_missing_unicode_long_and_control_arguments() {
        assert_eq!(
            mcp_label("mcp__docs__search_pages", &json!({})).unwrap(),
            "docs / search pages"
        );
        assert_eq!(
            mcp_label(
                "mcp__docs__search_pages",
                &json!({"query":"","url":"https://example.com"})
            )
            .unwrap(),
            "docs / search pages: https://example.com"
        );
        let query = "中".repeat(150);
        let input = json!({"query":query});
        let short = mcp_label("mcp__codegraph__codegraph_explore", &input).unwrap();
        assert!(short.ends_with('…'));
        assert_eq!(short.matches('中').count(), 100);
        assert!(change_view::plain(&expanded(
            "mcp__codegraph__codegraph_explore",
            &input,
            "ok",
            false,
            None
        ))
        .contains(&query));
        let clean = mcp_label(
            "mcp__docs__search",
            &json!({"query":"\u{1b}[31mhello\u{1b}[0m\nworld"}),
        )
        .unwrap();
        assert_eq!(clean, "docs / search: hello world");
        assert!(mcp_label("Read", &json!({})).is_none());
        assert!(mcp_label("mcp__broken", &json!({})).is_none());
    }
}

#[cfg(test)]
mod mcp_full_input_tests {
    use super::*;

    #[test]
    fn full_mcp_input_is_inline_in_finished_and_history_views() {
        let name = "mcp__codegraph__codegraph_explore";
        let input = serde_json::json!({"query": "中文🦀".repeat(150), "maxFiles": 17,
            "projectPath": "../other project", "options": {"enabled": false}, "extra": null});
        let json = input.to_string();
        for lines in [
            finished(name, &input, "ok", false, None, None),
            expanded(name, &input, "ok", false, None),
        ] {
            let text = change_view::plain(&lines);
            assert!(text.contains(&format!("{name}({json})")));
            assert!(!text.contains("Exploration:"));
            assert_eq!(text.matches(name).count(), 1);
        }
    }

    #[test]
    fn mcp_json_escapes_terminal_controls_without_losing_arguments() {
        let input =
            serde_json::json!({"query": "\u{1b}[31mhello\nworld", "secondary": "a  b\t\r\n"});
        let text = mcp_call("mcp__docs__search", &input).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains(&input.to_string()));
        assert!(!text.contains(" · "));
    }
}

#[cfg(test)]
mod result_regressions {
    use super::*;
    #[test]
    fn directory_lists_decode_and_keep_unicode_and_empty_state() {
        assert_eq!(
            result_text(
                r#"{"entries":[{"name":"目录","is_dir":true,"size":0},{"name":"a.md","is_dir":false,"size":12}]}"#
            ),
            "- 目录/\n- a.md"
        );
        assert_eq!(result_text(r#"{"entries":[]}"#), "(Empty directory)");
        assert!(result_text(r#"{"entries":[{"bad":true}]}"#).contains("bad"));
    }

    #[test]
    fn task_get_progress_distinguishes_reading_from_waiting() {
        assert_eq!(
            task_output_activity("TaskGet", &serde_json::json!({})),
            Some("Reading background task output")
        );
        assert_eq!(
            task_output_activity("TaskGet", &serde_json::json!({"block":false})),
            Some("Reading background task output")
        );
        assert_eq!(
            task_output_activity("TaskGet", &serde_json::json!({"block":true,"timeout":0})),
            Some("Reading background task output")
        );
        assert_eq!(
            task_output_activity("TaskGet", &serde_json::json!({"block":true})),
            Some("Waiting for background task to finish")
        );
        // The retired name stays displayable for old transcripts.
        assert_eq!(
            task_output_activity("TaskOutput", &serde_json::json!({})),
            Some("Reading background task output")
        );
        assert_eq!(task_output_activity("Bash", &serde_json::json!({})), None);
    }

    #[test]
    fn structured_tools_are_readable_and_tasks_are_named() {
        assert_eq!(result_text(r#"{"files":[{"file":"new.txt","status":"??"},{"file":"old.txt","status":"M"}]}"#), "- Untracked: new.txt\n- Modified: old.txt");
        assert_eq!(result_text(r#"{"diff":"--- a\n+++ b\n+hello"}"#), "--- a\n+++ b\n+hello");
        assert_eq!(result_text(r##"{"execution":"inline","instructions":"# Skill\nBody"}"##), "# Skill\nBody");
        assert_eq!(label("Skill", &serde_json::json!({"skill":"browser"}), None), "Skill(browser)");
        for name in ["TaskGet", "TaskOutput", "TaskStop"] {
            let output = r#"{"subject":"Run checks","stdout":"done"}"#;
            assert_eq!(result_label(name, &serde_json::json!({}), output, None), format!("{name}(Run checks)"));
        }
    }
    #[test]
    fn approved_plan_remains_available_in_expanded_history() {
        let output = serde_json::json!({"approved":true,"planFilePath":"plan.md","plan":"# Plan\n中文内容","message":"Plan approved."}).to_string();
        assert!(result_text(&output).contains("# Plan\n中文内容"));
        assert!(change_view::plain(&expanded(
            "ExitPlanMode",
            &serde_json::json!({}),
            &output,
            false,
            None
        ))
        .contains("中文内容"));
    }
}
