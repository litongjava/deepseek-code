//! Model-only compatibility for reports written before notifications carried their result.
use crate::services::background_sessions::notification_text;
use serde_json::Value;

#[cfg(test)]
#[path = "runtime_background_replay_tests.rs"]
mod tests;
use std::collections::{HashMap, HashSet};

const RESULT_TAIL: &str =
    "<result>See the background task report for this task above.</result>\n</task-notification>";

#[derive(Clone, Copy)]
pub(super) struct Report<'a> {
    raw: &'a str,
    task: &'a str,
    status: &'a str,
}

impl<'a> Report<'a> {
    pub(super) fn parse(raw: &'a str) -> Option<Self> {
        let mut lines = raw.strip_suffix("\n[End background task report]")?.lines();
        if lines.next()? != "[Background task report]" {
            return None;
        }
        let task = lines.next()?.strip_prefix("Task: ")?;
        lines.next()?.strip_prefix("Source session: ")?;
        let status = lines.next()?.strip_prefix("Status: ")?;
        lines.next()?.strip_prefix("Title: ")?;
        (!task.is_empty()).then_some(Self { raw, task, status })
    }

    pub(super) fn notification(self) -> String {
        format!("<task-notification>\n<task-id>{}</task-id>\n<status>{}</status>\n<result>{}</result>\n</task-notification>", notification_text(self.task), notification_text(self.status), notification_text(self.raw))
    }
}

// The old producer emitted a single text value; keep the persisted block shape.
fn single_text(content: &mut Value) -> Option<&mut Value> {
    if content.is_string() {
        return Some(content);
    }
    let [block] = content.as_array_mut()?.as_mut_slice() else {
        return None;
    };
    if !matches!(block["type"].as_str(), Some("text" | "input_text")) {
        return None;
    }
    block.get_mut("text")
}

pub(super) fn merge(history: &mut Vec<Value>, reports: Vec<(usize, Report<'_>)>) {
    if reports.is_empty() {
        return;
    }
    let mut pending: HashMap<_, _> = reports
        .into_iter()
        .map(|(at, r)| (r.task, (at, r)))
        .collect();
    let mut consumed = HashSet::new();
    for row in history.iter_mut() {
        if row["role"] != "user" || row["origin"] != "task_notification" {
            continue;
        }
        let Some(value) = single_text(&mut row["content"]) else {
            continue;
        };
        let Some(prefix) = value.as_str().and_then(|s| s.strip_suffix(RESULT_TAIL)) else {
            continue;
        };
        let Some((task, _)) = prefix
            .strip_prefix("<task-notification>\n<task-id>")
            .and_then(|s| s.split_once("</task-id>\n"))
        else {
            continue;
        };
        if let Some((at, report)) = pending.remove(task) {
            *value = Value::String(format!(
                "{prefix}<result>{}</result>\n</task-notification>",
                notification_text(report.raw)
            ));
            consumed.insert(at);
        }
    }
    let mut indices = 0..;
    history.retain(|_| !consumed.contains(&indices.next().unwrap()));
}

// Responses items have no durable user origin. Never match a user message there.
pub(super) fn native_fallbacks(window: &mut Value) {
    if window["compaction"] != true {
        return;
    }
    for item in window
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if item["role"] != "system" {
            continue;
        }
        let Some(value) = item.get_mut("content").and_then(single_text) else {
            continue;
        };
        if let Some(report) = value.as_str().and_then(Report::parse) {
            *value = Value::String(report.notification());
            item["role"] = "user".into();
        }
    }
}
