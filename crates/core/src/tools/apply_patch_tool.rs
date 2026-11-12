//! The model-visible `ApplyPatch` builtin.
//!
//! This module is the only product entry point into the internal parser,
//! preflight and apply layers. It never bypasses the engine: the call arrives
//! through the normal tool dispatch, after PreToolUse hooks and permissions,
//! with the effective (possibly hook-rewritten) input. Every affected path is
//! resolved and authorized before a single byte changes:
//!
//! 1. Parse the full patch text; a syntax error changes nothing.
//! 2. Resolve every source and destination against the workspace plus the
//!    session's additional trusted roots, read the current bytes and compute
//!    the complete plan. A denied or unresolvable path changes nothing.
//! 3. Reject sensitive paths (`.git`, shell profiles, MCP/IDE settings, ...)
//!    that would otherwise need a separate edit approval prompt. This tool has
//!    no per-path prompt of its own, so it refuses instead of silently editing
//!    them; `Edit`/`Write` remain the path for those.
//! 4. Apply with staging, verification and rollback, and report the structured
//!    outcome.
//!
//! Path grammar, newline/BOM behavior and failure semantics are documented in
//! [`super::apply_patch`], [`super::apply_patch_fs`] and
//! [`super::apply_patch_apply`]. Paths are data: no shell or `git apply`.
//!
//! # Result
//!
//! ```json
//! {
//!   "success": true,
//!   "content": "Applied 4 changes ...",
//!   "applied": [{"path","raw_path","action","destination"}],
//!   "unchanged": ["/abs/path"],
//!   "rolled_back": ["/abs/path"],
//!   "unrestored": [{"path","raw_path","message","backup"}],
//!   "created_dirs": ["/abs/path"],
//!   "removed_dirs": ["/abs/path"],
//!   "warnings": ["..."],
//!   "error": {"kind","message"} | null,
//!   "rollback_error": "..." | null,
//!   "changes": [{"path","raw_path","action","file_change","moved_from"}],
//!   "changes_truncated": false,
//!   "file_change": {...}          // only for exactly one changed file
//! }
//! ```
//!
//! `changes[*].file_change` is the same bounded diff metadata the edit tools
//! return; it is derived from the actual bytes after a successful apply. When a
//! path is listed as unrestored the diff is computed from the planned bytes and
//! marked `verified: false`, so a clean rollback is never implied.
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::{
    apply_patch,
    apply_patch_apply::{self, AppliedAction, AppliedChange, ApplyError, ApplyHooks, ApplyReport},
    apply_patch_fs::{self, DiskChange, DiskPlan, TrustedRoots},
    file_change, path_scope,
};

const MAX_REPORTED_DIFFS: usize = 40;

/// Permission outcome for every canonical path one patch would touch. The
/// engine uses it before dispatch to ask for confirmation; the tool uses it
/// again before any mutation so a direct or rewritten invocation cannot turn an
/// explicit ask into an automatic write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathAuthorization {
    Allowed,
    ConfirmationRequired { paths: Vec<String> },
    Denied { path: String, reason: String },
}

pub(crate) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "ApplyPatch".into(),
        description: "Apply one multi-file text patch in the workspace.\n\n\
The patch is line-oriented and must start with `*** Begin Patch` and end with `*** End Patch`:\n\
- `*** Add File: path` with one `+` line per new line (the file ends with a newline; no `+` lines creates an empty file).\n\
- `*** Update File: path` followed by hunks: context lines start with a space, removals with `-`, insertions with `+`. A hunk may start with `@@` or `@@ <context>`; `*** End of File` anchors it to the end. Context and removal lines must match the current file exactly and uniquely.\n\
- `*** Delete File: path`.\n\
- `*** Move to: path` immediately after an Update header to rename the file (optionally with hunks).\n\n\
Every path is validated before anything changes; traversal, symbolic-link targets, files outside the workspace/additional directories, and sensitive configuration paths are refused. The whole patch is applied or refused, and the result lists applied, unchanged, rolled-back and unrestored paths. This is not crash-atomic across files.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "The complete patch text, including the `*** Begin Patch` and `*** End Patch` markers"
                }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn execute(input: Value, cwd: &str) -> Result<Value> {
    let patch_text = input["patch"]
        .as_str()
        .ok_or_else(|| anyhow!("patch required"))?;

    let parsed = match apply_patch::parse_patch(patch_text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(failure(
                "parse",
                error.to_string(),
                None,
                Vec::new(),
                Vec::new(),
            ))
        }
    };

    let roots = match trusted_roots(cwd) {
        Ok(roots) => roots,
        Err(message) => {
            return Ok(failure("path", message, None, Vec::new(), Vec::new()));
        }
    };

    let plan = match apply_patch_fs::preflight_patch(&roots, &parsed) {
        Ok(plan) => plan,
        Err(error) => {
            return Ok(failure(
                "path",
                error.to_string(),
                None,
                Vec::new(),
                Vec::new(),
            ))
        }
    };

    // Sensitive configuration paths need an explicit edit approval prompt. This
    // tool has none, so refuse them instead of editing them without asking.
    let protected: Vec<String> = plan
        .changes
        .iter()
        .flat_map(change_paths)
        .filter_map(|path| {
            let text = path.to_string_lossy();
            crate::permissions::file_requires_explicit_approval(&text, Path::new(cwd))
                .then(|| text.into_owned())
        })
        .collect();
    if !protected.is_empty() {
        return Ok(failure(
            "permission",
            format!(
                "these paths require an explicit Edit/Write approval prompt and ApplyPatch cannot show one: {}",
                protected.join(", ")
            ),
            None,
            Vec::new(),
            Vec::new(),
        ));
    }

    // One denied target refuses the whole patch, and an explicit ask is never
    // silently downgraded to the mode's automatic edit grant. The engine asks
    // the user before dispatch; if this call did not receive that approval
    // (direct invocation, unsupported host, or a hook rewrite), nothing is
    // written.
    match authorize_plan(&plan, cwd, None) {
        PathAuthorization::Denied { path, reason } => {
            return Ok(failure(
                "permission",
                format!("'{path}' cannot be changed, so the whole patch is refused: {reason}"),
                None,
                Vec::new(),
                Vec::new(),
            ));
        }
        PathAuthorization::ConfirmationRequired { paths } if !path_scope::patch_approval() => {
            return Ok(failure(
                "permission",
                format!(
                    "these paths require explicit confirmation and this ApplyPatch call was not approved for them: {}; nothing was changed",
                    paths.join(", ")
                ),
                None,
                Vec::new(),
                Vec::new(),
            ));
        }
        _ => {}
    }

    let report = apply_patch_apply::apply_plan(&roots, &plan, ApplyHooks::default());
    Ok(result(&plan, &report))
}

/// Resolve the effective patch's canonical paths and aggregate the explicit
/// file-operation rules that cover them. `None` means the patch cannot be
/// parsed or resolved, so it cannot change anything; the tool itself will
/// report the concrete error.
pub(crate) fn path_authorization(
    cwd: &str,
    input: &Value,
    session: Option<&str>,
) -> Option<PathAuthorization> {
    let patch_text = input["patch"].as_str()?;
    let parsed = apply_patch::parse_patch(patch_text).ok()?;
    let roots = trusted_roots(cwd).ok()?;
    let plan = apply_patch_fs::preflight_patch(&roots, &parsed).ok()?;
    Some(authorize_plan(&plan, cwd, session))
}

fn authorize_plan(plan: &DiskPlan, cwd: &str, session: Option<&str>) -> PathAuthorization {
    let mut confirmation = Vec::new();
    for change in &plan.changes {
        for path in change_paths(change) {
            let path = path.to_string_lossy();
            let rule = match session {
                Some(session) => {
                    path_scope::explicit_path_rule_for_session(&path, Path::new(cwd), session)
                }
                None => path_scope::explicit_path_rule(&path, Path::new(cwd)),
            };
            match rule {
                Some(path_scope::ExplicitPathRule::Denied(reason)) => {
                    return PathAuthorization::Denied {
                        path: path.into_owned(),
                        reason,
                    };
                }
                Some(path_scope::ExplicitPathRule::ConfirmationRequired(_)) => {
                    confirmation.push(path.into_owned());
                }
                None => {}
            }
        }
    }
    confirmation.dedup();
    if confirmation.is_empty() {
        PathAuthorization::Allowed
    } else {
        PathAuthorization::ConfirmationRequired {
            paths: confirmation,
        }
    }
}

fn trusted_roots(cwd: &str) -> std::result::Result<TrustedRoots, String> {
    let additional: Vec<PathBuf> = path_scope::allowed_roots()
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    TrustedRoots::new(cwd, &additional).map_err(|error| error.to_string())
}

fn change_paths(change: &DiskChange) -> Vec<&Path> {
    match change {
        DiskChange::Add { path, .. }
        | DiskChange::Update { path, .. }
        | DiskChange::Delete { path, .. } => vec![path],
        DiskChange::Move { from, to, .. } => vec![from, to],
    }
}

fn action_name(change: &DiskChange) -> &'static str {
    match change {
        DiskChange::Add { .. } => "added",
        DiskChange::Update { .. } => "updated",
        DiskChange::Delete { .. } => "deleted",
        DiskChange::Move { .. } => "moved",
    }
}

fn applied_action_name(action: AppliedAction) -> &'static str {
    match action {
        AppliedAction::Added => "added",
        AppliedAction::Updated => "updated",
        AppliedAction::Deleted => "deleted",
        AppliedAction::Moved => "moved",
    }
}

fn apply_error_kind(error: &ApplyError) -> &'static str {
    match error {
        ApplyError::Verify { .. } => "verify",
        ApplyError::Stage { .. } => "stage",
        ApplyError::Commit { .. } => "commit",
        ApplyError::Directory { .. } => "directory",
        ApplyError::Hook(_) => "hook",
    }
}

/// Result of a failure that happened before applying. No filesystem change was
/// made, so the report lists empty state and the structured error.
fn failure(
    kind: &str,
    message: String,
    rollback_error: Option<String>,
    warnings: Vec<String>,
    created_dirs: Vec<PathBuf>,
) -> Value {
    json!({
        "success": false,
        "content": format!("ApplyPatch failed: {message}"),
        "error": {"kind": kind, "message": message},
        "applied": [],
        "unchanged": [],
        "rolled_back": [],
        "unrestored": [],
        "created_dirs": paths(created_dirs),
        "removed_dirs": [],
        "warnings": warnings,
        "rollback_error": rollback_error,
        "changes": [],
        "changes_truncated": false,
    })
}

fn paths(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn result(plan: &DiskPlan, report: &ApplyReport) -> Value {
    let unrestored_paths: Vec<&PathBuf> = report
        .unrestored
        .iter()
        .map(|change| &change.path)
        .collect();
    let applied: Vec<&AppliedChange> = report.applied.iter().collect();
    let applied_by_path = |change: &DiskChange| -> Option<&AppliedChange> {
        let path = match change {
            DiskChange::Move { from, .. } => from,
            DiskChange::Add { path, .. }
            | DiskChange::Update { path, .. }
            | DiskChange::Delete { path, .. } => path,
        };
        applied.iter().copied().find(|entry| &entry.path == path)
    };

    let mut changes = Vec::new();
    for change in &plan.changes {
        // An unchanged update is reported in `unchanged`, never as a change.
        if matches!(change, DiskChange::Update { old_contents, new_contents, .. } if old_contents == new_contents)
        {
            continue;
        }
        let Some(applied) = applied_by_path(change) else {
            continue;
        };
        if changes.len() >= MAX_REPORTED_DIFFS {
            continue;
        }
        changes.push(change_json(change, applied, &unrestored_paths));
    }
    let changed_total = plan
        .changes
        .iter()
        .filter(|change| applied_by_path(change).is_some())
        .count();
    let changes_truncated = changed_total > changes.len();

    let mut counts = [0usize; 4];
    for change in &report.applied {
        match change.action {
            AppliedAction::Added => counts[0] += 1,
            AppliedAction::Updated => counts[1] += 1,
            AppliedAction::Deleted => counts[2] += 1,
            AppliedAction::Moved => counts[3] += 1,
        }
    }
    let mut content = format!(
        "Applied {} change(s): {} added, {} updated, {} deleted, {} moved",
        report.applied.len(),
        counts[0],
        counts[1],
        counts[2],
        counts[3]
    );
    if !report.unchanged.is_empty() {
        content.push_str(&format!("; {} unchanged", report.unchanged.len()));
    }
    if !report.rolled_back.is_empty() {
        content.push_str(&format!("; {} rolled back", report.rolled_back.len()));
    }
    if !report.unrestored.is_empty() {
        content.push_str(&format!(
            "; {} could not be restored (see unrestored)",
            report.unrestored.len()
        ));
    }

    let single = if changed_total == 1 && report.succeeded() {
        changes.first().cloned()
    } else {
        None
    };

    json!({
        "success": report.succeeded(),
        "content": content,
        "error": report.error.as_ref().map(|error| json!({
            "kind": apply_error_kind(error),
            "message": error.to_string(),
        })),
        "applied": report.applied.iter().map(|change| json!({
            "path": change.path.to_string_lossy(),
            "raw_path": change.raw_path,
            "action": applied_action_name(change.action),
            "destination": change.destination.as_ref().map(|path| path.to_string_lossy()),
            "destination_raw": change.destination_raw,
        })).collect::<Vec<_>>(),
        "unchanged": paths(report.unchanged.clone()),
        "rolled_back": paths(report.rolled_back.clone()),
        "unrestored": report.unrestored.iter().map(|change| json!({
            "path": change.path.to_string_lossy(),
            "raw_path": change.raw_path,
            "message": change.message,
            "backup": change.backup.as_ref().map(|path| path.to_string_lossy()),
        })).collect::<Vec<_>>(),
        "created_dirs": paths(report.created_dirs.clone()),
        "removed_dirs": paths(report.removed_dirs.clone()),
        "warnings": report.warnings,
        "rollback_error": report.rollback_error,
        "changes": changes,
        "changes_truncated": changes_truncated,
        "file_change": single.as_ref().and_then(|change| change.get("file_change").cloned()),
    })
}

fn change_json(change: &DiskChange, applied: &AppliedChange, unrestored: &[&PathBuf]) -> Value {
    let verified = !unrestored.contains(&&applied.path);
    let (path, before, after, created, moved_from) = match change {
        DiskChange::Add { path, contents, .. } => (
            path.clone(),
            Some(String::new()),
            String::from_utf8(contents.clone()).ok(),
            true,
            Value::Null,
        ),
        DiskChange::Update {
            path,
            old_contents,
            new_contents,
            ..
        } => (
            path.clone(),
            String::from_utf8(old_contents.clone()).ok(),
            String::from_utf8(new_contents.clone()).ok(),
            false,
            Value::Null,
        ),
        DiskChange::Delete {
            path, old_contents, ..
        } => (
            path.clone(),
            String::from_utf8(old_contents.clone()).ok(),
            None,
            false,
            Value::Null,
        ),
        DiskChange::Move {
            from,
            to,
            new_contents,
            ..
        } => (
            to.clone(),
            None,
            String::from_utf8(new_contents.clone()).ok(),
            true,
            json!(from.to_string_lossy()),
        ),
    };

    // On a verified change the diff comes from the bytes now on disk. An
    // unrestored path may hold an external edit, so it keeps the planned bytes
    // and is marked unverified instead of presenting a clean result.
    let after_on_disk = if verified && after.is_some() {
        file_change::capture(&path)
    } else {
        after
    };
    let mut diff = file_change::completed_text(&path, before, after_on_disk);
    diff["verified"] = json!(verified);
    diff["created"] = json!(created);
    if !verified {
        diff["notice"] = json!(
            "Not verified: this path could not be restored after a failure; see unrestored and the retained backup"
        );
    }

    json!({
        "path": path.to_string_lossy(),
        "raw_path": applied.raw_path,
        "action": action_name(change),
        "moved_from": moved_from,
        "file_change": diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{execute_tool, get_tool_definitions};

    fn tool(patch: &str, cwd: &str) -> Value {
        execute_tool("ApplyPatch", json!({ "patch": patch }), cwd).unwrap()
    }

    fn texts(value: &Value, key: &str) -> Vec<String> {
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn definition_is_registered_once_with_a_patch_schema() {
        let definitions = get_tool_definitions();
        let matches: Vec<_> = definitions
            .iter()
            .filter(|definition| definition.name == "ApplyPatch")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].input_schema["required"], json!(["patch"]));
        assert_eq!(
            matches[0].input_schema["additionalProperties"],
            json!(false)
        );
        assert!(matches[0].description.contains("*** Begin Patch"));
        let loading = crate::services::customization::default_loading("ApplyPatch");
        assert!(!loading.enabled);
        assert!(loading.on_demand);
    }

    #[test]
    fn applies_a_mixed_patch_and_reports_real_bytes_and_diffs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("edit.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.path().join("del.txt"), "gone\n").unwrap();
        std::fs::write(root.path().join("move.txt"), "old\n").unwrap();
        let cwd = root.path().to_str().unwrap();
        let result = tool(
            "\
*** Begin Patch
*** Add File: sub/added.txt
+new
*** Update File: edit.txt
@@
 one
-two
+TWO
*** Delete File: del.txt
*** Update File: move.txt
*** Move to: moved.txt
@@
-old
+NEW
*** End Patch",
            cwd,
        );
        assert_eq!(result["success"], true, "{result}");
        assert!(result["error"].is_null());
        assert_eq!(
            std::fs::read_to_string(root.path().join("sub/added.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("edit.txt")).unwrap(),
            "one\nTWO\n"
        );
        assert!(!root.path().join("del.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("moved.txt")).unwrap(),
            "NEW\n"
        );
        assert!(!root.path().join("move.txt").exists());

        let applied = result["applied"].as_array().unwrap();
        assert_eq!(applied.len(), 4);
        let actions: Vec<_> = applied
            .iter()
            .map(|entry| entry["action"].as_str().unwrap())
            .collect();
        assert_eq!(actions, ["added", "updated", "deleted", "moved"]);
        assert_eq!(applied[3]["destination_raw"], "moved.txt");

        let changes = result["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 4);
        let edited = changes
            .iter()
            .find(|change| change["raw_path"] == "edit.txt")
            .unwrap();
        assert_eq!(edited["file_change"]["verified"], true);
        assert_eq!(edited["file_change"]["added"], 1);
        assert_eq!(edited["file_change"]["removed"], 1);
        let moved = changes
            .iter()
            .find(|change| change["raw_path"] == "move.txt")
            .unwrap();
        assert_eq!(
            moved["moved_from"].as_str().unwrap(),
            result["applied"][3]["path"].as_str().unwrap()
        );
        // Multi-file results have no single top-level diff.
        assert!(result["file_change"].is_null());
        assert_eq!(result["changes_truncated"], false);
    }

    #[test]
    fn a_mismatch_in_a_later_file_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(root.path().join("b.txt"), "two\n").unwrap();
        let cwd = root.path().to_str().unwrap();
        let result = tool(
            "\
*** Begin Patch
*** Update File: a.txt
@@
-one
+ONE
*** Update File: b.txt
@@
-absent
+X
*** End Patch",
            cwd,
        );
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(result["error"]["kind"], "path");
        assert!(result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("b.txt"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
            "two\n"
        );
        assert!(texts(&result, "applied").is_empty());
    }

    #[test]
    fn traversal_and_absolute_outside_paths_are_refused_before_any_change() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let cwd = workspace.to_str().unwrap();

        for path in [
            "../outside/secret.txt",
            outside.join("secret.txt").to_str().unwrap(),
        ] {
            let result = tool(
                &format!("*** Begin Patch\n*** Delete File: {path}\n*** End Patch"),
                cwd,
            );
            assert_eq!(result["success"], false, "{result}");
            assert_eq!(result["error"]["kind"], "path");
            assert_eq!(
                std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
                "secret\n"
            );
        }

        let result = tool(
            "*** Begin Patch\n*** Add File: ../outside/new.txt\n+x\n*** End Patch",
            cwd,
        );
        assert_eq!(result["success"], false);
        assert!(!outside.join("new.txt").exists());
    }

    #[test]
    fn a_second_file_outside_the_allowed_roots_is_refused_whole() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("inside.txt"), "one\n").unwrap();
        std::fs::write(outside.join("outside.txt"), "two\n").unwrap();
        let cwd = workspace.to_str().unwrap();
        let outside_path = outside.join("outside.txt");
        let result = tool(
            &format!(
                "*** Begin Patch\n*** Update File: inside.txt\n@@\n-one\n+ONE\n*** Update File: {}\n@@\n-two\n+TWO\n*** End Patch",
                outside_path.to_str().unwrap()
            ),
            cwd,
        );
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("inside.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            outside_path.to_str().unwrap(),
            outside_path.to_str().unwrap()
        );
        assert_eq!(std::fs::read_to_string(&outside_path).unwrap(), "two\n");
    }

    #[test]
    fn an_unchanged_update_reports_no_change() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("same.txt"), "same\n").unwrap();
        let cwd = root.path().to_str().unwrap();
        let result = tool(
            "*** Begin Patch\n*** Update File: same.txt\n@@\n-same\n+same\n*** End Patch",
            cwd,
        );
        assert_eq!(result["success"], true, "{result}");
        assert!(result["applied"].as_array().unwrap().is_empty());
        assert_eq!(result["unchanged"].as_array().unwrap().len(), 1);
        assert!(result["changes"].as_array().unwrap().is_empty());
        assert!(result["file_change"].is_null());
        assert_eq!(
            std::fs::read_to_string(root.path().join("same.txt")).unwrap(),
            "same\n"
        );
    }

    #[test]
    fn sensitive_paths_are_refused_rather_than_edited_without_a_prompt() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/config"), "old\n").unwrap();
        let cwd = root.path().to_str().unwrap();
        let result = tool(
            "*** Begin Patch\n*** Update File: .git/config\n@@\n-old\n+new\n*** End Patch",
            cwd,
        );
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(result["error"]["kind"], "permission");
        assert_eq!(
            std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn a_single_file_success_exposes_the_conventional_file_change() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "old\n").unwrap();
        let cwd = root.path().to_str().unwrap();
        let result = tool(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
            cwd,
        );
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["file_change"]["verified"], true);
        assert_eq!(result["file_change"]["added"], 1);
        assert_eq!(result["file_change"]["removed"], 1);
        assert_eq!(result["changes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_catalog_lists_it_once_and_tool_search_may_load_it() {
        let catalog = crate::native_engine::tool_loop::builtin_tool_catalog();
        let matches: Vec<_> = catalog
            .iter()
            .filter(|tool| tool.name == "ApplyPatch")
            .collect();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].description.contains("*** Begin Patch"));
        // ToolSearch's eligibility filter is (on_demand && !enabled && !exposed)
        // plus the agent allowlist; the default loading is what makes it
        // discoverable without becoming a resident tool.
        let customization = crate::services::customization::Customization::default();
        let loading = customization.loading("ApplyPatch");
        assert!(loading.on_demand);
        assert!(!loading.enabled);
    }

    fn permission_context(
        mode: crate::permissions::PermissionMode,
    ) -> (
        std::sync::Arc<crate::permissions::PermissionManager>,
        crate::permissions::PermissionContext,
    ) {
        let manager = std::sync::Arc::new(crate::permissions::PermissionManager::new(
            std::sync::Arc::new(crate::permissions::AuditLogger::new(16)),
        ));
        manager.set_mode(mode);
        let context = crate::permissions::PermissionContext {
            mcp_read_only: false,
            tool_name: "ApplyPatch".into(),
            tool_input: json!({"patch": "*** Begin Patch\n*** End Patch"}),
            conversation_id: "session".into(),
            user_id: None,
            workspace_path: Some("/workspace".into()),
        };
        (manager, context)
    }

    #[test]
    fn it_is_a_write_tool_for_permission_classification() {
        use crate::permissions::PermissionManager;
        assert!(PermissionManager::is_file_edit_tool("ApplyPatch"));
        let (_, context) = permission_context(crate::permissions::PermissionMode::PlanMode);
        let manager = crate::permissions::PermissionManager::new(std::sync::Arc::new(
            crate::permissions::AuditLogger::new(16),
        ));
        manager.set_mode(crate::permissions::PermissionMode::PlanMode);
        assert!(matches!(
            manager.check_permission(&context),
            crate::permissions::PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn accept_edits_grants_it_without_a_prompt() {
        let (manager, context) =
            permission_context(crate::permissions::PermissionMode::AcceptEdits);
        assert_eq!(
            manager.check_permission(&context),
            crate::permissions::PermissionResult::Granted
        );
    }

    #[test]
    fn ask_mode_still_prompts_for_a_patch() {
        let (manager, context) =
            permission_context(crate::permissions::PermissionMode::AskPermissions);
        assert!(matches!(
            manager.check_permission(&context),
            crate::permissions::PermissionResult::RequiresConfirmation(_)
        ));
    }

    async fn apply_with_session_rules(
        root: &Path,
        patch: String,
        mode: crate::permissions::PermissionMode,
        rules: std::collections::HashMap<String, crate::permissions::rules::PermissionAction>,
        approved: bool,
    ) -> Value {
        use crate::permissions::{AuditLogger, PermissionManager};
        let manager = std::sync::Arc::new(PermissionManager::new(std::sync::Arc::new(
            AuditLogger::new(16),
        )));
        manager.set_mode(mode);
        manager.set_edit_session_allowed("session", true);
        manager.set_session_rules("session", rules);
        let cwd = root.to_str().unwrap().to_owned();
        let input = json!({ "patch": patch });
        let call_input = input.clone();
        let cwd_for_call = cwd.clone();
        crate::tools::path_scope::with_permissions(
            Some(manager),
            crate::tools::path_scope::with_edit_session(Some("session".into()), async move {
                crate::tools::path_scope::approved_auto_call(
                    "ApplyPatch",
                    &input,
                    &cwd,
                    approved,
                    async move {
                        crate::tools::execute_tool("ApplyPatch", call_input, &cwd_for_call)
                    },
                )
                .await
            }),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_explicit_confirmation_rule_is_not_silently_treated_as_allow() {
        use crate::permissions::{rules::PermissionAction, PermissionMode};
        use std::collections::HashMap;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(root.path().join("secret.txt"), "secret\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: secret.txt\n@@\n-secret\n+leak\n*** End Patch".to_string();
        let result = apply_with_session_rules(
            root.path(),
            patch,
            PermissionMode::AcceptEdits,
            HashMap::from([("Edit(secret.txt)".to_string(), PermissionAction::Confirm)]),
            false,
        )
        .await;
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(result["error"]["kind"], "permission");
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
            "secret\n"
        );
    }

    #[tokio::test]
    async fn a_user_approved_call_may_apply_the_confirmed_patch() {
        use crate::permissions::{rules::PermissionAction, PermissionMode};
        use std::collections::HashMap;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("secret.txt"), "secret\n").unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: secret.txt\n@@\n-secret\n+NEW\n*** End Patch"
                .to_string();
        let result = apply_with_session_rules(
            root.path(),
            patch,
            PermissionMode::AcceptEdits,
            HashMap::from([("Edit(secret.txt)".to_string(), PermissionAction::Confirm)]),
            true,
        )
        .await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
            "NEW\n"
        );
    }

    #[tokio::test]
    async fn write_and_multi_edit_rules_also_cover_the_patch() {
        use crate::permissions::{rules::PermissionAction, PermissionMode};
        use std::collections::HashMap;
        for rule in ["Write(secret.txt)", "MultiEdit(secret.txt)"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("secret.txt"), "secret\n").unwrap();
            let patch =
                "*** Begin Patch\n*** Update File: secret.txt\n@@\n-secret\n+NEW\n*** End Patch"
                    .to_string();
            let result = apply_with_session_rules(
                root.path(),
                patch,
                PermissionMode::AcceptEdits,
                HashMap::from([(rule.to_string(), PermissionAction::Confirm)]),
                false,
            )
            .await;
            assert_eq!(result["success"], false, "{rule}: {result}");
            assert_eq!(result["error"]["kind"], "permission", "{rule}");
            assert_eq!(
                std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
                "secret\n",
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn a_confirmed_move_destination_refuses_the_whole_move() {
        use crate::permissions::{rules::PermissionAction, PermissionMode};
        use std::collections::HashMap;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("move.txt"), "old\n").unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: move.txt\n*** Move to: moved.txt\n*** End Patch"
                .to_string();
        let result = apply_with_session_rules(
            root.path(),
            patch,
            PermissionMode::AcceptEdits,
            HashMap::from([("Edit(moved.txt)".to_string(), PermissionAction::Confirm)]),
            false,
        )
        .await;
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(result["error"]["kind"], "permission");
        assert_eq!(
            std::fs::read_to_string(root.path().join("move.txt")).unwrap(),
            "old\n"
        );
        assert!(!root.path().join("moved.txt").exists());
    }

    #[tokio::test]
    async fn one_explicitly_denied_path_refuses_the_whole_patch() {
        use crate::permissions::{AuditLogger, PermissionManager, PermissionMode};
        use std::collections::HashMap;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(root.path().join("secret.txt"), "secret\n").unwrap();
        let manager = std::sync::Arc::new(PermissionManager::new(std::sync::Arc::new(
            AuditLogger::new(16),
        )));
        manager.set_mode(PermissionMode::AcceptEdits);
        manager.set_edit_session_allowed("session", true);
        manager.set_session_rules(
            "session",
            HashMap::from([(
                "Edit(secret.txt)".to_string(),
                crate::permissions::rules::PermissionAction::Deny,
            )]),
        );
        let cwd = root.path().to_str().unwrap().to_owned();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: secret.txt\n@@\n-secret\n+leak\n*** End Patch".to_string();
        let result = crate::tools::path_scope::with_permissions(
            Some(manager),
            crate::tools::path_scope::with_edit_session(Some("session".into()), async move {
                crate::tools::execute_tool("ApplyPatch", json!({ "patch": patch }), &cwd)
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(result["error"]["kind"], "permission");
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
            "secret\n"
        );
    }

    #[tokio::test]
    async fn additional_trusted_roots_authorize_their_own_files() {
        let workspace = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        std::fs::write(extra.path().join("outside-root.txt"), "old\n").unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
            extra.path().join("outside-root.txt").display()
        );
        let cwd = workspace.path().to_str().unwrap().to_owned();
        let allowed = vec![extra.path().to_path_buf()];
        let result = crate::tools::path_scope::scope(allowed, async move {
            execute_tool("ApplyPatch", json!({"patch": patch}), &cwd)
        })
        .await
        .unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(
            std::fs::read_to_string(extra.path().join("outside-root.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("outside-root.txt")).is_err(),
            true
        );
    }

    #[test]
    fn malformed_and_empty_patches_fail_without_touching_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "old\n").unwrap();
        let cwd = root.path().to_str().unwrap();
        for patch in [
            "",
            "*** Begin Patch\n*** End Patch",
            "*** Begin Patch\n*** Update File: a.txt\n*** End Patch",
            "*** Begin Patch\n*** Update File: a.txt\n?bad\n*** End Patch",
        ] {
            let result = tool(patch, cwd);
            assert_eq!(result["success"], false, "{patch:?} -> {result}");
            assert_eq!(result["error"]["kind"], "parse");
            assert_eq!(
                std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
                "old\n"
            );
        }
    }
}
