//! Bounded presentation metadata captured from actual file bytes, never model input.
use serde_json::{json, Value};
use std::{fs, io::Read, path::Path};
const MAX_BYTES: usize = 256 * 1024;
const MAX_LINES: usize = 4000;
const MAX_ROWS: usize = 120;
pub(super) fn snapshot(text: &str) -> Option<String> {
    (text.len() <= MAX_BYTES && !text.contains('\0')).then(|| text.to_owned())
}
pub(super) fn capture(path: &Path) -> Option<String> {
    match fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take((MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > MAX_BYTES || bytes.contains(&0) {
                return None;
            }
            String::from_utf8(bytes).ok()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    }
}
pub(super) fn completed(path: &Path, before: Option<String>) -> Value {
    let after = if path.is_file() { capture(path) } else { None };
    completed_text(path, before, after)
}

/// Callers with protected storage must supply snapshots read through that storage.
pub(crate) fn completed_text(path: &Path, before: Option<String>, after: Option<String>) -> Value {
    let base = json!({"version":1,"path":path.to_string_lossy(),"verified":true});
    let mut result = base;
    let Some((before, after)) = before.zip(after) else {
        result["notice"] = json!("Diff omitted: binary, unavailable, or larger than 256 KiB");
        return result;
    };
    if before.len() > MAX_BYTES
        || after.len() > MAX_BYTES
        || before.lines().count() > MAX_LINES
        || after.lines().count() > MAX_LINES
    {
        result["notice"] = json!("Diff omitted: more than 4000 lines");
        return result;
    }
    let mut format_notes = Vec::new();
    if before.starts_with('\u{feff}') != after.starts_with('\u{feff}') {
        format_notes.push(if after.starts_with('\u{feff}') {
            "UTF-8 BOM added"
        } else {
            "UTF-8 BOM removed"
        });
    }
    if before.contains("\r\n") != after.contains("\r\n") {
        format_notes.push(if after.contains("\r\n") {
            "CRLF line endings introduced"
        } else {
            "CRLF line endings removed"
        });
    }
    if before.ends_with('\n') != after.ends_with('\n') {
        format_notes.push(if after.ends_with('\n') {
            "Final newline added"
        } else {
            "Final newline removed"
        });
    }
    result["format_notes"] = json!(format_notes);
    let patch = diffy::create_patch(&before, &after);
    let mut rows = Vec::new();
    let mut added = 0;
    let mut removed = 0;
    let mut retained_bytes = 0;
    let mut truncated = false;
    for hunk in patch.hunks() {
        let mut old = hunk.old_range().start();
        let mut new = hunk.new_range().start();
        for line in hunk.lines() {
            let (kind, text, old_line, new_line) = match line {
                diffy::Line::Context(text) => {
                    let entry = ("context", *text, Some(old), Some(new));
                    old += 1;
                    new += 1;
                    entry
                }
                diffy::Line::Delete(text) => {
                    removed += 1;
                    let entry = ("removed", *text, Some(old), None);
                    old += 1;
                    entry
                }
                diffy::Line::Insert(text) => {
                    added += 1;
                    let entry = ("added", *text, None, Some(new));
                    new += 1;
                    entry
                }
            };
            if rows.len() < MAX_ROWS && retained_bytes + text.len() <= 32768 {
                retained_bytes += text.len();
                rows.push(json!({"kind":kind,"text":text.trim_end_matches(['\r','\n']),"old_line":old_line,"new_line":new_line}));
            } else {
                truncated = true;
            }
        }
    }
    result["added"] = json!(added);
    result["removed"] = json!(removed);
    result["rows"] = json!(rows);
    result["truncated"] = json!(truncated);
    result
}

/// The change a pending Write, Edit or MultiEdit call would make, computed in
/// memory for approval prompts. None when the call would fail or the file
/// cannot be previewed; the tool itself still decides on execution.
pub fn preview(name: &str, input: &Value, cwd: &Path) -> Option<Value> {
    let file_path = input["file_path"].as_str()?;
    let path = if Path::new(file_path).is_absolute() {
        Path::new(file_path).to_path_buf()
    } else {
        cwd.join(file_path)
    };
    let before = capture(&path)?;
    let replace = |content: &str, old: &str, new: &str, all: bool| -> Option<String> {
        let count = content.matches(old).count();
        if old.is_empty() || count == 0 || (!all && count > 1) {
            return None;
        }
        Some(if all { content.replace(old, new) } else { content.replacen(old, new, 1) })
    };
    let after = match name {
        "Write" => input["content"].as_str()?.to_owned(),
        "Edit" => replace(
            &before,
            input["old_string"].as_str()?,
            input["new_string"].as_str()?,
            input["replace_all"].as_bool().unwrap_or(false),
        )?,
        "MultiEdit" => {
            let mut content = before.clone();
            for edit in input["edits"].as_array()? {
                content = replace(&content, edit["old_string"].as_str()?, edit["new_string"].as_str()?, true)?;
            }
            content
        }
        _ => return None,
    };
    let mut change = completed_text(&path, Some(before), Some(after));
    change["verified"] = json!(false);
    change["created"] = json!(!path.exists());
    Some(change)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_tools_publish_diff_only_after_successful_disk_write() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_str().unwrap();
        let written =
            super::super::tool_write(json!({"file_path":"a.txt","content":"old\n"}), cwd).unwrap();
        assert_eq!(written["file_change"]["added"], 1);
        let failed = super::super::tool_edit(
            json!({"file_path":"a.txt","old_string":"missing","new_string":"bad"}),
            cwd,
        )
        .unwrap();
        assert_eq!(failed["success"], false);
        assert!(failed.get("file_change").is_none());
        let edited = super::super::tool_edit(
            json!({"file_path":"a.txt","old_string":"old","new_string":"new"}),
            cwd,
        )
        .unwrap();
        assert_eq!(edited["file_change"]["rows"][1]["text"], "new");
        let failed=super::super::tool_multi_edit(json!({"file_path":"a.txt","edits":[{"old_string":"new","new_string":"partial"},{"old_string":"missing","new_string":"oops"}]}),cwd).unwrap();
        assert_eq!(failed["success"], false);
        assert!(failed.get("file_change").is_none());
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }
    #[test]
    fn format_changes_are_explicit_even_when_display_lines_look_equal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("format.txt");
        fs::write(&path, "\u{feff}line\n").unwrap();
        let result = completed(&path, Some("line\r\n".into()));
        assert!(result["format_notes"]
            .to_string()
            .contains("UTF-8 BOM added"));
        assert!(result["format_notes"]
            .to_string()
            .contains("CRLF line endings removed"));
    }
    #[test]
    fn many_changes_truncate_rows_without_losing_counts() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("many.txt");
        fs::write(&file, "old\n".repeat(200)).unwrap();
        let before = capture(&file);
        fs::write(&file, "new\n".repeat(200)).unwrap();
        let change = completed(&file, before);
        assert_eq!(change["added"], 200);
        assert_eq!(change["removed"], 200);
        assert_eq!(change["rows"].as_array().unwrap().len(), MAX_ROWS);
        assert_eq!(change["truncated"], true);
    }
    #[test]
    fn disk_diff_is_bounded_and_contains_actual_changed_lines() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, "old\r\n").unwrap();
        let before = capture(&file);
        fs::write(&file, "new\r\n").unwrap();
        let change = completed(&file, before);
        assert_eq!(change["added"], 1);
        assert_eq!(change["removed"], 1);
        assert_eq!(change["rows"][0]["text"], "old");
        assert_eq!(change["rows"][1]["text"], "new");
        fs::write(&file, vec![b'x'; MAX_BYTES + 1]).unwrap();
        assert!(completed(&file, Some("before".into()))["notice"].is_string());
    }

    #[test]
    fn previews_pending_edits_without_touching_the_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.py"), "def f():\n    return 1\n").unwrap();
        let edit = json!({"file_path":"a.py","old_string":"return 1","new_string":"return 2"});
        let change = preview("Edit", &edit, temp.path()).unwrap();
        assert_eq!((change["added"].clone(), change["removed"].clone(), change["created"].clone()), (json!(1), json!(1), json!(false)));
        assert_eq!(fs::read_to_string(temp.path().join("a.py")).unwrap(), "def f():\n    return 1\n");
        assert!(preview("Edit", &json!({"file_path":"a.py","old_string":"missing","new_string":"x"}), temp.path()).is_none());
        let created = preview("Write", &json!({"file_path":"new.txt","content":"a\nb\n"}), temp.path()).unwrap();
        assert_eq!((created["added"].clone(), created["created"].clone()), (json!(2), json!(true)));
        assert!(preview("Bash", &json!({"command":"ls"}), temp.path()).is_none());
    }
}
