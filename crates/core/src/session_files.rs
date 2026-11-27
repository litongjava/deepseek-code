//! Paths shared by per-session writers, deletion and time-based pruning.
//!
//! Every writer resolves its path through this module, and cleanup and pruning
//! use the same lists, so a newly written file cannot be left out of either.

use std::path::{Path, PathBuf};

/// Directory of the frozen first-message reminders.
pub(crate) const ENVIRONMENT_DIR: &str = "cache/environment";
/// Directory of the frozen MCP tool definitions.
pub(crate) const MCP_SNAPSHOT_DIR: &str = "cache/mcp-tool-snapshots";

/// Kinds tracked by `prompt::set_changes`; each writes `<session>.<kind>.names.json`.
pub(crate) const SET_CHANGE_SKILLS: &str = "skills";
pub(crate) const SET_CHANGE_LOADABLE_TOOLS: &str = "loadable-tools";
pub(crate) const SET_CHANGE_EXPERIENCE: &str = "experience";
const SET_CHANGE_KINDS: &[&str] = &[
    SET_CHANGE_SKILLS,
    SET_CHANGE_LOADABLE_TOOLS,
    SET_CHANGE_EXPERIENCE,
];

/// Suffixes written directly under `cache/environment/`.
const ENVIRONMENT_SUFFIXES: &[&str] = &[
    ".txt",
    ".date.txt",
    ".announced-date",
    ".loadable-tools.json",
    ".skills.json",
    ".inline-skills.json",
    ".experience.json",
    SUGGESTION_SUFFIX,
];

/// The MCP tool snapshot for a session.
const MCP_SNAPSHOT_SUFFIX: &str = ".json";

/// A session id is used as one file name component.
pub(crate) fn is_safe_session(session: &str) -> bool {
    !session.is_empty()
        && !session.contains('/')
        && !session.contains('\\')
        && !session.contains("..")
}

pub(crate) fn environment_file(root: &Path, session: &str, suffix: &str) -> PathBuf {
    root.join(ENVIRONMENT_DIR).join(format!("{session}{suffix}"))
}

pub(crate) fn set_changes_file(root: &Path, session: &str, kind: &str) -> PathBuf {
    environment_file(root, session, &format!(".{kind}.names.json"))
}

pub(crate) fn mcp_snapshot_file(root: &Path, session: &str) -> PathBuf {
    root.join(MCP_SNAPSHOT_DIR)
        .join(format!("{session}{MCP_SNAPSHOT_SUFFIX}"))
}

/// Every tracked file for one session, in a stable order.
pub(crate) fn files_for_session(root: &Path, session: &str) -> Vec<PathBuf> {
    ENVIRONMENT_SUFFIXES
        .iter()
        .map(|suffix| environment_file(root, session, suffix))
        .chain(
            SET_CHANGE_KINDS
                .iter()
                .map(|kind| set_changes_file(root, session, kind)),
        )
        .chain(std::iter::once(mcp_snapshot_file(root, session)))
        .collect()
}

/// Suffixes the prune scan recognizes under `environment/`.
pub(crate) fn environment_suffixes() -> Vec<String> {
    ENVIRONMENT_SUFFIXES
        .iter()
        .map(|suffix| (*suffix).to_owned())
        .chain(
            SET_CHANGE_KINDS
                .iter()
                .map(|kind| format!(".{kind}.names.json")),
        )
        .collect()
}

pub(crate) const SUGGESTION_SUFFIX: &str = ".suggestion.json";

pub mod suggestion;
