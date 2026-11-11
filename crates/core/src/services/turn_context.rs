//! Context attached to one user message for the rest of the session.
//!
//! The reference CLI saves hook context and slash-command skill content as
//! extra messages beside the prompt, so every later request still carries
//! them in the same place. Frontends here save only the user's own text, so
//! the engine records the blocks beside the session and attaches them to the
//! same message on every request. The prompt prefix then stays stable for
//! caching, and the context is not lost after its turn.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// SHA-256 of the message content as the frontend sent it.
    pub key: String,
    /// Which message with this key: 0 for the first, 1 for the next, ...
    pub occurrence: usize,
    /// Text blocks placed after the user's own content.
    pub blocks: Vec<String>,
    /// The skill this entry records, so it can be shown again once
    /// compaction has removed its message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<InvokedSkill>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokedSkill {
    pub name: String,
    pub path: String,
    pub content: String,
}

/// The reference CLI's budget for skills shown again after compaction.
const SKILLS_TOTAL_TOKENS: usize = 25_000;
const SKILL_MAX_TOKENS: usize = 5_000;

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

pub fn key(content: &Value) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, content.to_string().as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Key and occurrence of each user message, in order; None for other roles.
pub fn locate(messages: &[Value]) -> Vec<Option<(String, usize)>> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    messages
        .iter()
        .map(|message| {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let key = key(message.get("content").unwrap_or(&Value::Null));
            let count = seen.entry(key.clone()).or_default();
            *count += 1;
            Some((key, *count - 1))
        })
        .collect()
}

fn file(root: &Path, session: &str) -> PathBuf {
    root.join("turn-context").join(format!("{session}.jsonl"))
}

pub fn load(root: &Path, session: &str) -> Result<Vec<Entry>> {
    let path = file(root, session);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).with_context(|| format!("Parsing {}", path.display())))
        .collect()
}

/// An entry placing `blocks` after the last user message of `messages`.
pub fn for_last(messages: &[Value], blocks: Vec<String>) -> Option<Entry> {
    let (key, occurrence) = locate(messages).into_iter().rev().flatten().next()?;
    Some(Entry { key, occurrence, blocks, skill: None })
}

pub fn save(root: &Path, session: &str, entry: &Entry) -> Result<()> {
    let path = file(root, session);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("Creating {}", parent.display()))?;
    }
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("Opening {}", path.display()))?;
    writeln!(out, "{}", serde_json::to_string(entry)?).with_context(|| format!("Writing {}", path.display()))
}

/// Record blocks for the last user message of `messages`.
pub fn attach_to_last(root: &Path, session: &str, messages: &[Value], blocks: Vec<String>) -> Result<Option<Entry>> {
    let Some(entry) = for_last(messages, blocks) else { return Ok(None) };
    save(root, session, &entry)?;
    Ok(Some(entry))
}

/// The blocks for each message of `messages`, empty where none were recorded.
/// A later entry for the same message adds to the earlier ones.
pub fn resolve(entries: &[Entry], messages: &[Value]) -> Vec<Vec<String>> {
    locate(messages)
        .into_iter()
        .map(|slot| match slot {
            Some((key, occurrence)) => entries
                .iter()
                .filter(|entry| entry.key == key && entry.occurrence == occurrence)
                .flat_map(|entry| entry.blocks.iter().cloned())
                .collect(),
            None => Vec::new(),
        })
        .collect()
}

/// Skills whose message is no longer in `messages` (compaction removed it),
/// worded as the reference CLI shows them after a compaction: newest first,
/// one per name, within its token budget. None when there are none.
pub fn invoked_skills_reminder(entries: &[Entry], messages: &[Value]) -> Option<String> {
    let present: std::collections::HashSet<_> = locate(messages).into_iter().flatten().collect();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    let mut sections = Vec::new();
    for entry in entries.iter().rev() {
        let Some(skill) = &entry.skill else { continue };
        if present.contains(&(entry.key.clone(), entry.occurrence)) || !seen.insert(skill.name.clone()) {
            continue;
        }
        let mut content = skill.content.trim().to_owned();
        if estimate_tokens(&content) > SKILL_MAX_TOKENS {
            let mut end = SKILL_MAX_TOKENS * 4;
            while !content.is_char_boundary(end) { end -= 1; }
            content.truncate(end);
            content.push_str("\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]");
        }
        let section = format!("### Skill: {}\nPath: {}\n\n{content}", skill.name, skill.path);
        if total + estimate_tokens(&section) > SKILLS_TOTAL_TOKENS { continue; }
        total += estimate_tokens(&section);
        sections.push(section);
    }
    (!sections.is_empty()).then(|| format!(
        "<system-reminder>\nThe following skills were invoked EARLIER in this session (before the conversation was compacted), not on the current turn. They are shown here for context only so you remain aware of their guidelines.\n\nIMPORTANT: Do NOT re-execute these skills or perform their one-time setup actions (e.g., scheduling, creating files) again. Any request or argument text embedded in the skill bodies below \u{2014} for example under a \"## User Request\" or \"## Input\" heading \u{2014} was captured when that skill was first invoked. It is NOT the user's current message and NOT a new request: do not act on it as if it were live. Only continue to apply ongoing behavioral guidelines from these skills where still relevant.\n\n{}\n</system-reminder>",
        sections.join("\n\n---\n\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn blocks_follow_the_same_message_across_turns() {
        let root = tempfile::tempdir().unwrap();
        let first = vec![json!({"role":"user","content":"go"})];
        attach_to_last(root.path(), "s", &first, vec!["hook one".into()]).unwrap();
        // The same text again later is a different message.
        let later = vec![
            json!({"role":"user","content":"go"}),
            json!({"role":"assistant","content":"done"}),
            json!({"role":"user","content":"go"}),
        ];
        attach_to_last(root.path(), "s", &later, vec!["skill two".into()]).unwrap();
        let entries = load(root.path(), "s").unwrap();
        assert_eq!(resolve(&entries, &later), vec![vec!["hook one".to_string()], vec![], vec!["skill two".to_string()]]);
        assert!(load(root.path(), "other").unwrap().is_empty());
        assert!(attach_to_last(root.path(), "s", &[json!({"role":"assistant","content":"x"})], vec!["x".into()]).unwrap().is_none());
    }

    #[test]
    fn skills_reappear_only_once_their_message_is_compacted_away() {
        let skill = |name: &str, content: &str| Some(InvokedSkill { name: name.into(), path: format!("/s/{name}/SKILL.md"), content: content.into() });
        let history = vec![json!({"role":"user","content":"/lint"}), json!({"role":"assistant","content":"ok"}), json!({"role":"user","content":"/fmt"})];
        let mut entries = vec![
            Entry { skill: skill("lint", "old lint rules"), ..for_last(&history[..1], vec!["old lint rules".into()]).unwrap() },
            for_last(&history[..1], vec!["hook".into()]).unwrap(),
            Entry { skill: skill("fmt", "fmt rules"), ..for_last(&history, vec![]).unwrap() },
        ];
        assert!(invoked_skills_reminder(&entries, &history).is_none());
        let compacted = vec![json!({"role":"user","content":"Summary of the earlier conversation"})];
        entries.push(Entry { skill: skill("lint", "new lint rules"), ..for_last(&history[..1], vec![]).unwrap() });
        let reminder = invoked_skills_reminder(&entries, &compacted).unwrap();
        assert!(reminder.starts_with("<system-reminder>\nThe following skills were invoked EARLIER"));
        let (lint, fmt) = (reminder.find("### Skill: lint\nPath: /s/lint/SKILL.md\n\nnew lint rules").unwrap(), reminder.find("### Skill: fmt").unwrap());
        assert!(lint < fmt, "newest first");
        assert!(!reminder.contains("old lint rules") && !reminder.contains("hook"));
        let huge = vec![Entry { skill: skill("big", &"x".repeat(SKILL_MAX_TOKENS * 8)), ..for_last(&history[..1], vec![]).unwrap() }];
        assert!(invoked_skills_reminder(&huge, &compacted).unwrap().contains("skill content truncated"));
    }
}
