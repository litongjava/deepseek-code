//! Ephemeral activity output. Only the event consumer writes to the terminal;
//! the timer never races the editor or response stream.
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

const TIPS: &[&str] = &[
    "Tip: /help lists commands.",
    "Tip: /rename names this session.",
    "Tip: /context shows active history size.",
    "Tip: /compact frees context.",
];

pub struct TurnProgress {
    workspace: Option<std::path::PathBuf>,
    enabled: bool,
    hook_only: bool,
    color: bool,
    visible: bool,
    visible_rows: usize,
    rendered_lines: Vec<String>,
    rendered_size: (u16, u16),
    permission_mode: String,
    activity: String,
    hook_activity: Option<String>,
    phase: &'static str,
    started: Instant,
    activity_started: Instant,
    line_open: bool,
    usage: ReportedUsage,
    tools: HashMap<String, Instant>,
    finished_tools: HashMap<String, (String, Value, String, bool)>,
    agents: HashMap<String, Instant>,
}

impl TurnProgress {
    pub fn new(interactive_text: bool) -> Self {
        Self::with_activity(interactive_text, "Waiting for response", false)
    }

    pub fn for_hook(interactive_text: bool, activity: &str) -> Self {
        Self::with_activity(interactive_text, activity, true)
    }

    fn with_activity(interactive_text: bool, activity: &str, hook_only: bool) -> Self {
        let enabled = interactive_text
            && !crate::terminal::screen_reader()
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && io::stderr().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb");
        let mut progress = Self {
            workspace: None,
            enabled,
            hook_only,
            color: colors_enabled(enabled, std::env::var_os("NO_COLOR").is_some()),
            visible: false,
            visible_rows: 0,
            rendered_lines: Vec::new(),
            rendered_size: (0, 0),
            permission_mode: "ask".into(),
            activity: String::new(),
            hook_activity: None,
            phase: "",
            started: Instant::now(),
            activity_started: Instant::now(),
            line_open: false,
            usage: ReportedUsage::default(),
            tools: HashMap::new(),
            finished_tools: HashMap::new(),
            agents: HashMap::new(),
        };
        progress.set_activity(activity);
        progress
    }

    pub fn set_activity(&mut self, activity: &str) {
        self.set_status(activity, "");
    }

    pub fn hook_activity(&mut self, message: Option<String>) {
        self.hook_activity = message;
        self.tick();
    }

    pub fn set_workspace(&mut self, workspace: &std::path::Path) {
        self.workspace = Some(workspace.to_owned());
    }

    pub fn set_permission_mode(&mut self, mode: &str) {
        self.permission_mode = mode.into();
        self.tick();
    }

    pub fn thinking(&mut self) {
        self.set_status("Sketching", " thinking");
    }

    pub fn reported_usage(&mut self, value: &Value) {
        self.usage.update(value);
        if self.visible {
            self.tick();
        }
    }

    pub fn tool_started(&mut self, id: &str, name: &str, input: &Value) {
        if self.tools.contains_key(id) || self.finished_tools.contains_key(id) {
            return;
        }
        self.tools.insert(id.into(), Instant::now());
        self.notice(&format!("• {}", tool_preview_in_workspace(name, input, self.workspace.as_deref())));
        let label = if name == "Agent" {
            format!("Running subagent: {name}")
        } else {
            format!("Working: {name}")
        };
        self.set_activity(&label);
    }

    pub fn tool_finished(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        output: &str,
        failed: bool,
    ) {
        // The pipeline can mirror ToolUseDone with PipelineToolResult. Each
        // unique tool call gets one completion line and retains its timing.
        if self.finished_tools.get(id).is_some_and(|previous| {
            previous.0 == name
                && previous.1 == *input
                && previous.2 == output
                && previous.3 == failed
        }) {
            return;
        }
        self.finished_tools.insert(
            id.into(),
            (name.into(), input.clone(), output.into(), failed),
        );
        let elapsed = self.tools.remove(id).map(|start| start.elapsed());
        self.notice(&tool_result_summary(name, output, failed, elapsed));
        self.set_activity("Working");
    }
    pub fn agent_progress(&mut self, id: &str, agent_type: &str, status: &str, tool: Option<&str>) {
        if status == "started" {
            self.agents.insert(id.into(), Instant::now());
        }
        let mut label = format!(
            "Agent {} [{}] · {}",
            single_line(agent_type),
            clipped(&single_line(id), 12),
            single_line(status)
        );
        if let Some(tool) = tool {
            label.push_str(&format!(" · {}", single_line(tool)));
        }
        if matches!(status, "completed" | "failed" | "cancelled") {
            if let Some(start) = self.agents.remove(id) {
                label.push_str(&format!(" · {:.1}s", start.elapsed().as_secs_f64()));
            }
        }
        self.notice(&format!("  ↳ {label}"));
        if matches!(status, "completed" | "failed" | "cancelled") {
            self.set_activity("Working");
        } else {
            self.set_activity(&label);
        }
    }

    fn notice(&mut self, message: &str) {
        self.clear();
        if !self.enabled {
            return;
        }
        if self.line_open {
            eprintln!();
            self.line_open = false;
        }
        let (width, _) = crossterm::terminal::size().unwrap_or((80, 24));
        let width = width as usize;
        eprintln!(
            "{}",
            clipped(&single_line(message), width.saturating_sub(1))
        );
    }

    fn set_status(&mut self, activity: &str, phase: &'static str) {
        if self.visible && self.activity == activity && self.phase == phase {
            return;
        }
        if self.activity != activity || self.phase != phase {
            self.activity_started = Instant::now();
        }
        self.activity = activity.chars().filter(|c| !c.is_control()).collect();
        self.phase = phase;
        if self.enabled {
            if self.line_open {
                eprintln!();
                self.line_open = false;
            }
            self.draw();
        }
    }

    pub fn tick(&mut self) {
        if self.visible {
            self.draw();
        }
    }

    fn draw(&mut self) {
        if !self.enabled {
            return;
        }
        let seconds = self.started.elapsed().as_secs();
        let tip = TIPS[(seconds / 12) as usize % TIPS.len()];
        let seconds = if self.phase.is_empty() {
            seconds
        } else {
            self.activity_started.elapsed().as_secs()
        };
        // Keep the cursor on the last frame row. Timer updates repaint only
        // changed rows, without line feeds that would scroll the viewport.
        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
        let size = (width, height);
        let width = width as usize;
        let mut status = format!("* {} ({seconds}s{})", self.hook_activity.as_deref().unwrap_or(&self.activity), if self.hook_activity.is_some() { "" } else { self.phase });
        if let Some(usage) = self.usage.summary() {
            status.push_str(&format!(" · {usage}"));
        }
        let status = styled(
            &clipped(&status, width.saturating_sub(1)),
            self.color,
            crate::terminal::style::activity(),
        );
        let tip = styled(
            &clipped(&format!("  └ {tip}"), width.saturating_sub(1)),
            self.color,
            crate::terminal::style::muted_ansi(),
        );
        let mut lines = if self.hook_only { vec![status] } else { activity_frame(
            status,
            tip,
            &self.permission_mode,
            width.saturating_sub(1),
            height as usize,
            self.color,
        ) };
        let output = if self.visible {
            // Resize invalidates the cached row contents. Reuse only the rows
            // already reserved and still in the viewport; never add new rows
            // during a refresh. The next fresh frame can use the larger size.
            let rows = self.visible_rows.min(usize::from(height).max(1));
            lines.truncate(rows);
            lines.resize(rows, String::new());
            refresh_frame(&self.rendered_lines, &lines, self.rendered_size != size)
        } else {
            format!("\x1b[?25l\r{}\x1b[?25h", lines.join("\r\n"))
        };
        write_frame(&output);
        self.visible_rows = lines.len();
        self.rendered_lines = lines;
        self.rendered_size = size;
        self.visible = true;
    }

    pub fn clear(&mut self) {
        if self.visible {
            let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
            let rows = self.visible_rows.min(usize::from(height).max(1));
            let mut output = String::from("\x1b[?25l\r\x1b[2K");
            for _ in 1..rows {
                output.push_str("\x1b[1A\r\x1b[2K");
            }
            output.push_str("\x1b[?25h");
            write_frame(&output);
            self.visible = false;
            self.visible_rows = 0;
            self.rendered_lines.clear();
        }
    }

    pub fn before_text(&mut self, text: &str) {
        self.clear();
        if !text.is_empty() {
            self.line_open = !text.ends_with('\n');
        }
    }

    pub fn finish(&mut self, error: Option<&str>) {
        self.clear();
        if self.enabled {
            let time = chrono::Local::now().format("%-I:%M %p").to_string();
            eprintln!(
                "{}",
                styled(
                    &footer(self.started.elapsed(), error, &time),
                    self.color,
                    "90"
                )
            );
            if let Some(usage) = self.usage.summary() {
                eprintln!(
                    "{}",
                    styled(&format!("{usage} (not a turn total)"), self.color, "90")
                );
            }
        }
    }
}

// Build one update before acquiring the output lock. In particular, do not
// flush an empty/cleared frame before the replacement contents are ready.
fn write_frame(output: &str) {
    if output.is_empty() {
        return;
    }
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(output.as_bytes());
    let _ = stderr.flush();
}

fn refresh_frame(previous: &[String], next: &[String], force: bool) -> String {
    let mut output = String::new();
    for (row, line) in next.iter().enumerate() {
        if !force && previous.get(row) == Some(line) {
            continue;
        }
        if output.is_empty() {
            output.push_str("\x1b[?25l");
        }
        let distance = next.len() - row - 1;
        output.push('\r');
        if distance > 0 {
            output.push_str(&format!("\x1b[{distance}A"));
        }
        output.push_str(line);
        // Erase only the old tail, after writing the replacement text.
        output.push_str("\x1b[K");
        if distance > 0 {
            output.push_str(&format!("\x1b[{distance}B"));
        }
    }
    if !output.is_empty() {
        output.push_str("\r\x1b[?25h");
    }
    output
}

fn activity_frame(
    status: String,
    tip: String,
    mode: &str,
    width: usize,
    height: usize,
    color: bool,
) -> Vec<String> {
    // A compact fallback avoids scrolling a whole screen on very short terminals.
    if height < 10 {
        let mut lines = vec![status, tip];
        lines.truncate(height.max(1));
        return lines;
    }
    let rule = styled(&"─".repeat(width), color, "90");
    let footer = clipped(
        &format!("  {mode} mode · Ctrl+C interrupt · input after response"),
        width,
    );
    vec![
        String::new(),
        status,
        tip,
        String::new(),
        rule.clone(),
        clipped("› ", width),
        rule,
        styled(&footer, color, "90"),
    ]
}

#[derive(Default)]
pub(crate) struct ReportedUsage {
    input: Option<u64>,
    output: Option<u64>,
}

impl ReportedUsage {
    pub(crate) fn update(&mut self, value: &Value) {
        // Anthropic sends partial usage updates; OpenAI sends prompt/completion.
        // Child sessions also forward usage without a request ID, so summing
        // these events would double-count or conflate independent requests.
        if let Some(input) = value["input_tokens"]
            .as_u64()
            .or_else(|| value["prompt_tokens"].as_u64())
        {
            self.input = Some(input);
        }
        if let Some(output) = value["output_tokens"]
            .as_u64()
            .or_else(|| value["completion_tokens"].as_u64())
        {
            self.output = Some(output);
        }
    }

    pub(crate) fn output(&self) -> Option<u64> {
        self.output
    }

    pub(crate) fn summary(&self) -> Option<String> {
        if self.input.is_none() && self.output.is_none() {
            return None;
        }
        Some(format!(
            "Latest reported tokens: {} in / {} out",
            self.input
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".into()),
            self.output
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".into())
        ))
    }
}

pub(super) fn single_line(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
pub(crate) fn tool_preview(name: &str, input: &Value) -> String {
    tool_preview_in_workspace(name, input, None)
}

pub(crate) fn tool_preview_in_workspace(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> String {
    let name = single_line(name);
    let preview = if name == "Bash" {
        input["command"].as_str().map(|command| {
            let lower = command.to_ascii_lowercase();
            if [
                "password",
                "token",
                "secret",
                "api_key",
                "api-key",
                "authorization",
                "bearer",
            ]
            .iter()
            .any(|key| lower.contains(key))
            {
                "[credential-like command; preview hidden]".to_owned()
            } else {
                single_line(command)
            }
        })
    } else {
        ["file_path", "path", "pattern", "agent_type", "operation", "prompt"]
            .iter()
            .find_map(|key| input[*key].as_str().map(|value| {
                if matches!(*key, "file_path" | "path") {
                    super::display_path::label(value, workspace)
                } else {
                    value.to_owned()
                }
            }))
            .map(|value| single_line(&value))
    };
    match preview {
        Some(preview) if !preview.is_empty() => format!("{name}: {}", clipped(&preview, 100)),
        _ => name,
    }
}

pub(crate) fn tool_result_summary(
    name: &str,
    output: &str,
    failed: bool,
    elapsed: Option<Duration>,
) -> String {
    let status = if failed { "error" } else { "done" };
    let elapsed = elapsed
        .map(|value| format!(" · {:.1}s", value.as_secs_f64()))
        .unwrap_or_default();
    let mut result = format!("  {} · {status}{elapsed}", single_line(name));
    // Review findings are reported only through this tool, so show them in full.
    if name == "ReportFindings" && !failed {
        if let Some(content) = serde_json::from_str::<Value>(output).ok().and_then(|value| value["content"].as_str().map(str::to_owned)) {
            for line in content.lines() { result.push_str(&format!("\n    {line}")); }
        }
    }
    if name == "Agent" {
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            if let Some(id) = value["session_id"].as_str() {
                result.push_str(&format!(" · session {}", clipped(&single_line(id), 50)));
            }
        }
    }
    result
}

impl Drop for TurnProgress {
    fn drop(&mut self) {
        self.clear();
    }
}

fn footer(elapsed: Duration, error: Option<&str>, time: &str) -> String {
    let status = match error {
        None => "done",
        Some(message)
            if {
                let lower = message.to_ascii_lowercase();
                lower.contains("interrupted") || lower.contains("cancelled")
            } =>
        {
            "interrupted"
        }
        Some(_) => "failed",
    };
    format!("✻ Worked for {} · {status} {time}", human_duration(elapsed))
}

fn human_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn colors_enabled(interactive_terminal: bool, no_color_present: bool) -> bool {
    interactive_terminal && !no_color_present
}

fn styled(text: &str, color: bool, code: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.into()
    }
}

fn clipped(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_preview_changes_labels_only() {
        let workspace = std::env::temp_dir().join("preview workspace");
        let path = workspace.join("src").join("中文 file.rs");
        let input = serde_json::json!({"file_path":path});
        let original = input.clone();
        assert_eq!(super::tool_preview_in_workspace("Read", &input, Some(&workspace)), "Read: src/中文 file.rs");
        assert_eq!(input, original);
        let command = serde_json::json!({"command":format!("cat {}", path.display())});
        assert_eq!(super::tool_preview_in_workspace("Bash", &command, Some(&workspace)), super::tool_preview("Bash", &command));
        let external = serde_json::json!({"path":workspace.with_file_name("other").join("x.rs")});
        assert_eq!(super::tool_preview_in_workspace("Read", &external, Some(&workspace)), super::tool_preview("Read", &external));
    }
    use super::*;

    #[test]
    fn timer_refresh_only_repaints_status_without_scrolling_or_erasing_borders() {
        let previous = activity_frame("Sketching (1s)".into(), "Tip".into(), "ask", 60, 24, true);
        let mut next = previous.clone();
        next[1] = "Sketching (2s)".into();
        assert_eq!(
            refresh_frame(&previous, &next, false),
            "\x1b[?25l\r\x1b[6ASketching (2s)\x1b[K\x1b[6B\r\x1b[?25h"
        );
        let output = refresh_frame(&previous, &next, false);
        assert!(!output.contains('\n'));
        assert!(!output.contains("─"));
        assert!(!output.contains("\x1b[2K"));
        assert_eq!(refresh_frame(&next, &next, false), "");
    }

    #[test]
    fn shortened_status_erases_old_tail_after_replacement_and_restores_cursor() {
        let previous = vec!["Working on a long command".into(), "Tip".into()];
        let next = vec!["Done".into(), "Tip".into()];
        assert_eq!(
            refresh_frame(&previous, &next, false),
            "\x1b[?25l\r\x1b[1ADone\x1b[K\x1b[1B\r\x1b[?25h"
        );
    }

    #[test]
    fn resize_refresh_uses_only_reserved_visible_rows_and_no_line_feeds() {
        let previous = activity_frame("Working".into(), "Tip".into(), "ask", 60, 24, false);
        let next = activity_frame("Working".into(), "Tip".into(), "ask", 20, 2, false);
        let update = refresh_frame(&previous, &next, true);
        assert_eq!(update.matches("\x1b[K").count(), 2);
        assert!(!update.contains('\n'));
        assert!(!update.contains("\x1b[7A"));
        let tiny = activity_frame("W".into(), "T".into(), "ask", 1, 1, false);
        assert_eq!(tiny.len(), 1);
        assert!(!refresh_frame(&next, &tiny, true).contains("\x1b[1A"));
    }

    #[test]
    fn activity_layout_separates_prompt_status_tip_and_input_area() {
        let lines = activity_frame(
            "Sketching".into(),
            "  └ Tip: /help".into(),
            "plan",
            60,
            24,
            false,
        );
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "Sketching");
        assert!(lines[2].starts_with("  └ Tip:"));
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], lines[6]);
        assert_eq!(lines[5], "› ");
        assert!(lines[7].contains("plan mode"));
        assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
        assert_eq!(
            activity_frame("Working".into(), "Tip".into(), "ask", 20, 5, false).len(),
            2
        );
    }

    #[test]
    fn footer_never_reports_failures_as_success() {
        let duration = Duration::from_secs(2);
        assert_eq!(
            footer(duration, None, "5:51 PM"),
            "✻ Worked for 2s · done 5:51 PM"
        );
        assert!(footer(duration, Some("Interrupted"), "5:51 PM").contains("· interrupted"));
        assert!(
            footer(duration, Some("Turn stopped: cancelled"), "5:51 PM").contains("· interrupted")
        );
        assert!(footer(duration, Some("network failed"), "5:51 PM").contains("· failed"));
    }

    #[test]
    fn disabled_progress_stays_silent_through_all_states() {
        let mut progress = TurnProgress::new(false);
        progress.set_activity("Thinking");
        progress.tick();
        assert!(!progress.visible);
        progress.before_text("response");
        progress.set_activity("Working: Read");
        progress.finish(None);
        assert!(!progress.visible);
    }

    #[test]
    fn duration_formats_real_seconds_minutes_and_hours() {
        for (seconds, expected) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m 0s"),
            (368, "6m 8s"),
            (3601, "1h 0m 1s"),
        ] {
            assert_eq!(human_duration(Duration::from_secs(seconds)), expected);
        }
        assert_eq!(
            footer(Duration::from_secs(368), None, "6:06 PM"),
            "✻ Worked for 6m 8s · done 6:06 PM"
        );
    }

    #[test]
    fn no_color_and_redirected_output_disable_sgr_styling() {
        for (terminal, no_color) in [(false, false), (false, true), (true, true)] {
            let rendered = styled("footer", colors_enabled(terminal, no_color), "90");
            assert_eq!(rendered, "footer");
            assert!(!rendered.contains('\u{1b}'));
        }
        assert_eq!(
            styled("footer", colors_enabled(true, false), "90"),
            "\x1b[90mfooter\x1b[0m"
        );
    }

    #[test]
    fn status_never_wraps_on_narrow_or_unicode_terminals() {
        assert_eq!(clipped("Working: 搜索", 11), "Working: 搜");
        assert_eq!(clipped("Tip", 0), "");
    }

    #[test]
    fn streamed_unicode_and_newlines_track_output_boundaries() {
        let mut progress = TurnProgress::new(false);
        progress.before_text("你好");
        assert!(progress.line_open);
        progress.before_text("");
        assert!(progress.line_open);
        progress.before_text("世界\n");
        assert!(!progress.line_open);
        progress.before_text("🙂");
        assert!(progress.line_open);
    }

    #[test]
    fn thinking_phase_is_cleared_when_tool_work_begins() {
        let mut progress = TurnProgress::new(false);
        progress.thinking();
        assert_eq!(progress.activity, "Sketching");
        assert_eq!(progress.phase, " thinking");
        progress.set_activity("Working: Read");
        assert_eq!(progress.phase, "");
    }

    #[test]
    fn usage_accepts_both_provider_formats_without_summing_partial_updates() {
        let mut usage = ReportedUsage::default();
        assert!(usage.summary().is_none());
        usage.update(&serde_json::json!({"input_tokens": 42, "output_tokens": 0}));
        usage.update(&serde_json::json!({"output_tokens": 12}));
        usage.update(&serde_json::json!({"output_tokens": 12}));
        assert_eq!(
            usage.summary().unwrap(),
            "Latest reported tokens: 42 in / 12 out"
        );
        usage.update(&serde_json::json!({"prompt_tokens": 81, "completion_tokens": 17}));
        assert_eq!(
            usage.summary().unwrap(),
            "Latest reported tokens: 81 in / 17 out"
        );
        usage.update(&serde_json::json!({"input_tokens": -4, "output_tokens": "unknown"}));
        assert_eq!(usage.input, Some(81));
        assert_eq!(usage.output, Some(17));
    }

    #[test]
    fn tool_previews_are_bounded_single_lines_and_hide_credentials() {
        assert_eq!(
            tool_preview(
                "Bash",
                &serde_json::json!({"command": "cargo test\n --locked"})
            ),
            "Bash: cargo test --locked"
        );
        assert!(!tool_preview(
            "Bash",
            &serde_json::json!({"command": "curl -H 'Authorization: Bearer abc'"})
        )
        .contains("abc"));
        let preview = tool_preview(
            "Read",
            &serde_json::json!({"file_path": "你好\u{1b}[2J\nfile"}),
        );
        assert!(!preview.contains('\u{1b}'));
        assert!(!preview.contains('\n'));
        assert_eq!(
            tool_preview(
                "Agent",
                &serde_json::json!({"prompt": "private task", "agent_type": "general"})
            ),
            "Agent: general"
        );
    }

    #[test]
    fn tool_results_report_real_outcome_and_known_child_id_without_dumping_body() {
        let output = r#"{"session_id":"child-42","status":"completed","result":"large body"}"#;
        assert_eq!(
            tool_result_summary("Agent", output, false, Some(Duration::from_millis(1500))),
            "  Agent · done · 1.5s · session child-42"
        );
        assert_eq!(
            tool_result_summary("Bash", "secret command output", true, None),
            "  Bash · error"
        );
    }

    #[test]
    fn tool_lifecycle_is_silent_when_noninteractive_and_releases_timing_state() {
        let mut progress = TurnProgress::new(false);
        progress.tool_started("one", "Bash", &serde_json::json!({"command": "echo hi"}));
        assert_eq!(progress.tools.len(), 1);
        assert!(!progress.visible);
        progress.tool_finished("one", "Bash", &serde_json::json!({"command":"echo hi"}), "hi", false);
        assert!(progress.tools.is_empty());
        assert!(!progress.visible);
    }

    #[test]
    fn nested_agents_have_separate_lifetimes() {
        let mut progress = TurnProgress::new(false);
        progress.agent_progress("parent", "general", "started", None);
        progress.agent_progress("child", "reviewer", "started", None);
        progress.agent_progress("child", "reviewer", "running", Some("Read"));
        assert_eq!(progress.agents.len(), 2);
        progress.agent_progress("child", "reviewer", "completed", None);
        assert!(progress.agents.contains_key("parent"));
        assert!(!progress.agents.contains_key("child"));
        progress.agent_progress("parent", "general", "failed", None);
        assert!(progress.agents.is_empty());
        assert!(!progress.visible);
    }

    #[test]
    fn mirrored_tool_events_do_not_repeat_completion_or_reset_current_activity() {
        let mut progress = TurnProgress::new(false);
        let input = serde_json::json!({"file_path": "README.md"});
        progress.tool_started("read-1", "Read", &input);
        let started = progress.tools["read-1"];
        progress.tool_started("read-1", "Read", &input);
        assert_eq!(progress.tools["read-1"], started);
        progress.tool_finished("read-1", "Read", &input, "content", false);
        progress.tool_started("read-2", "Read", &input);
        progress.tool_finished("read-1", "Read", &input, "content", false);
        assert_eq!(progress.activity, "Working: Read");
        assert_eq!(progress.finished_tools.len(), 1);
        assert!(progress.tools.contains_key("read-2"));
        progress.tool_started("read-1", "Read", &input);
        assert!(!progress.tools.contains_key("read-1"));
    }
    #[test]
    fn changed_completion_preserves_latest_input_and_one_tool_count_in_fallback() {
        let mut progress = TurnProgress::new(false);
        let old = serde_json::json!({"file_path":"a.txt"});
        let revised = serde_json::json!({"file_path":"b.txt"});
        progress.tool_started("one", "Read", &old);
        progress.tool_finished("one", "Read", &old, "old", false);
        progress.tool_finished("one", "Other", &revised, "changed", true);
        assert_eq!(progress.finished_tools.len(), 1);
        assert_eq!(progress.finished_tools["one"], ("Other".into(), revised.clone(), "changed".into(), true));
        assert!(progress.tools.is_empty());
        progress.set_activity("Another task");
        progress.tool_finished("one", "Other", &revised, "changed", true);
        assert_eq!(progress.activity, "Another task");
    }

}
