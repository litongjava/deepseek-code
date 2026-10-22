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

/// The tool approval prompt: what will run, then numbered choices.
pub fn approval_question(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> String {
    let mut question = match (name, input["command"].as_str()) {
        ("Bash" | "PowerShell", Some(command)) => {
            let mut text = format!("{name} command\n");
            for line in command.lines().take(12) {
                text.push_str(&format!("  {}\n", printable(line)));
            }
            if let Some(description) = input["description"].as_str() {
                text.push_str(&format!("  {}\n", printable(description)));
            }
            text
        }
        _ => {
            let mut text = format!("{}\n", label(name, input, workspace));
            let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
            let lines: Vec<&str> = pretty.lines().collect();
            for line in lines.iter().take(12) {
                text.push_str(&format!("  {}\n", printable(line)));
            }
            if lines.len() > 12 {
                text.push_str(&format!("  \u{2026} {} more lines\n", lines.len() - 12));
            }
            text
        }
    };
    question.push_str("Allow this call?  1. Yes   2. No  (1/y or 2/n)");
    question
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
        let question = approval_question("Bash", &json!({"command":"ls -la","description":"List files"}), None);
        assert_eq!(question, "Bash command\n  ls -la\n  List files\nAllow this call?  1. Yes   2. No  (1/y or 2/n)");
        let question = approval_question("Write", &json!({"file_path":"a.txt","content":"x"}), None);
        assert!(question.starts_with("Write(a.txt)\n  {"), "{question}");
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
