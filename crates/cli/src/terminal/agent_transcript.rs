//! Bounded agent history rendered with the main transcript's blocks and Markdown.
use super::{markdown, scrollback::Scrollback, tool_view};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct Transcript {
    pub history: Scrollback,
    number: usize,
    calls: HashMap<String, (usize, String, Value, Option<String>)>,
    completed: HashSet<String>,
    text: String,
}

impl Transcript {
    fn separate(&mut self) {
        if self.history.needs_separator() {
            self.history.push(String::new(), None);
        }
    }

    pub fn note(&mut self, text: &str) {
        self.separate();
        self.history
            .push(tool_view::plain_terminal_text(text), None);
    }

    pub fn user(&mut self, text: &str) {
        self.separate();
        self.history.record_prompt(text);
    }

    pub fn assistant(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.separate();
        let width =
            crossterm::terminal::size().map_or(80, |(cols, _)| cols.saturating_sub(2) as usize);
        let mut renderer = markdown::MarkdownStream::new(std::env::var_os("NO_COLOR").is_none());
        renderer.set_width(width.saturating_sub(2));
        let mut rows = text
            .lines()
            .flat_map(|line| renderer.line(line))
            .collect::<Vec<_>>();
        rows.extend(renderer.finish());
        for (index, row) in rows.iter().enumerate() {
            let indent = " ".repeat(2 + markdown::hanging_indent(row));
            self.history
                .push_flow(row, if index == 0 { "● " } else { "  " }, &indent, width);
        }
    }

    pub fn tool(&mut self, name: &str, input: &Value, output: &str, failed: bool) {
        self.number += 1;
        self.finished(self.number, name, input, output, failed, None);
    }

    fn finished(&mut self, number: usize, name: &str, input: &Value, output: &str, failed: bool, title: Option<&str>) {
        self.separate();
        let short = tool_view::numbered(
            tool_view::titled(tool_view::finished(name, input, output, failed, None, None), name, title, failed),
            Some(number),
        );
        let full = tool_view::numbered(
            tool_view::titled(tool_view::expanded_block(name, input, output, failed, None, None), name, title, failed),
            Some(number),
        );
        let (id, rows) = self.history.block(short, full);
        self.history.bind_copy(
            id,
            tool_view::copy_command(name, input),
            tool_view::named_result_text(name, output),
        );
        for row in rows {
            self.history.push(row, Some(id));
        }
    }

    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        self.assistant(&text);
    }

    fn start(&mut self, id: &str, name: &str, input: &Value, title: Option<&str>) {
        self.flush_text();
        if self.calls.contains_key(id) || self.completed.contains(id) {
            return;
        }
        self.number += 1;
        self.calls
            .insert(id.into(), (self.number, name.into(), input.clone(), title.map(str::to_owned)));
    }

    fn result(&mut self, id: &str, output: &str, failed: bool) {
        if !self.completed.insert(id.into()) {
            return;
        }
        self.flush_text();
        if let Some((number, name, input, title)) = self.calls.remove(id) {
            self.finished(number, &name, &input, output, failed, title.as_deref());
        } else {
            self.note(&format!("  ⎿  {}", tool_view::result_text(output)));
        }
    }

    pub fn events(&mut self, output: &str, session: &str) {
        let events = output
            .lines()
            .rev()
            .take(120)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(|value| value["session_id"].as_str().is_none_or(|id| id == session))
            .collect::<Vec<_>>();
        // Stream-json includes native events and SDK envelopes for the same call.
        let native = events
            .iter()
            .any(|value| matches!(value["type"].as_str(), Some("tool_start" | "text")));
        for value in events {
            match value["type"].as_str() {
                Some("text") if native => self
                    .text
                    .push_str(value["delta"].as_str().unwrap_or_default()),
                Some("tool_start") if native => self.start(
                    value["id"].as_str().unwrap_or_default(),
                    value["name"].as_str().unwrap_or("Tool"),
                    &value["input"],
                    value["display_title"].as_str(),
                ),
                Some("tool_result") => self.result(
                    value["id"].as_str().unwrap_or_default(),
                    value["output"].as_str().unwrap_or_default(),
                    value["is_error"] == true,
                ),
                Some("assistant") if !native => {
                    for block in value["message"]["content"].as_array().into_iter().flatten() {
                        match block["type"].as_str() {
                            Some("text") => {
                                self.assistant(block["text"].as_str().unwrap_or_default())
                            }
                            Some("tool_use") => self.start(
                                block["id"].as_str().unwrap_or_default(),
                                block["name"].as_str().unwrap_or("Tool"),
                                &block["input"],
                                block["display_title"].as_str(),
                            ),
                            _ => {}
                        }
                    }
                }
                Some("user") if !native => {
                    for block in value["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_result" {
                            let output = block["content"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| block["content"].to_string());
                            self.result(
                                block["tool_use_id"].as_str().unwrap_or_default(),
                                &output,
                                block["is_error"] == true,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        self.flush_text();
        let mut pending = self.calls.drain().map(|(_, call)| call).collect::<Vec<_>>();
        pending.sort_by_key(|call| call.0);
        for (number, name, input, title) in pending {
            self.note(&format!(
                "● #{number} {} · running",
                tool_view::display_label(&name, &input, None, title.as_deref())
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_title_survives_worker_replay_and_error_results() {
        for completed in [false, true] {
            let mut events = json!({"type":"tool_start","id":"t1","name":"TaskOutput","input":{"task_id":"123"},"display_title":"Run tests"}).to_string();
            if completed { events.push_str(&format!("\n{}", json!({"type":"tool_result","id":"t1","output":"Task failed","is_error":true}))); }
            let mut transcript = Transcript::default();
            transcript.events(&events, "session");
            let body = markdown::plain_rendered(&transcript.history.all().join("\n"));
            assert!(body.contains("#1 TaskOutput(Run tests)"), "{body}");
            assert!(!body.contains("123"), "{body}");
        }
    }

    #[test]
    fn worker_events_share_tool_blocks_merge_text_and_ignore_sdk_duplicates() {
        let events = [
            json!({"type":"text","delta":"Hello "}),
            json!({"type":"text","delta":"world"}),
            json!({"type":"assistant","session_id":"child","message":{"content":[{"type":"text","text":"Hello world"}]}}),
            json!({"type":"tool_start","id":"r1","name":"Read","input":{"file_path":"fixture.md"}}),
            json!({"type":"tool_result","id":"r1","output":"READ_RESULT","is_error":true}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"r1","content":"READ_RESULT"}]}}),
            json!({"type":"text","session_id":"other","delta":"OTHER_SESSION"}),
            json!({"type":"tool_start","id":"r2","name":"Read","input":{"file_path":"pending.md"}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let mut transcript = Transcript::default();
        transcript.events(&events, "child");
        let body = markdown::plain_rendered(&transcript.history.all().join("\n"));
        assert_eq!(body.matches("Hello world").count(), 1);
        assert_eq!(body.matches("READ_RESULT").count(), 1);
        assert!(body.contains("#1 Read(fixture.md)"), "{body}");
        assert!(body.contains("#2 Read(pending.md) · running"), "{body}");
        assert!(!body.contains("OTHER_SESSION"));
        assert!(body.contains("world\n\n"));
    }
}
