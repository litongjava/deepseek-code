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
    Some(Entry { key, occurrence, blocks })
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
}
