//! Match LF/CRLF equivalently while keeping untouched bytes and local line endings.
use anyhow::{ensure, Result};

pub(super) fn replace(content: &str, old: &str, new: &str, all: bool) -> Result<(String, usize)> {
    ensure!(!old.is_empty(), "old_string must not be empty");
    let bytes = content.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut offsets = vec![0];
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\r\n") {
            index += 1;
        }
        normalized.push(bytes[index]);
        index += 1;
        offsets.push(index);
    }
    // Removing CR from CRLF cannot invalidate UTF-8.
    let normalized = String::from_utf8(normalized).expect("UTF-8 input");
    let old = old.replace("\r\n", "\n");
    let matches: Vec<_> = normalized
        .match_indices(&old)
        .map(|(at, text)| (offsets[at], offsets[at + text.len()]))
        .collect();
    ensure!(!matches.is_empty(), "old_string not found in file");
    ensure!(
        all || matches.len() == 1,
        "old_string found {} times, use replace_all=true",
        matches.len()
    );
    let new = new.replace("\r\n", "\n");
    let mut result = String::with_capacity(content.len());
    let mut previous = 0;
    for &(start, end) in &matches {
        result.push_str(&content[previous..start]);
        // Prefer a newline within the match, then the current line, then file style.
        let style = newline(&content[start..end])
            .or_else(|| newline(&content[start..]))
            .or_else(|| newline(content))
            .unwrap_or("\n");
        result.push_str(&if style == "\r\n" {
            new.replace('\n', "\r\n")
        } else {
            new.clone()
        });
        previous = end;
    }
    result.push_str(&content[previous..]);
    Ok((result, matches.len()))
}

fn newline(text: &str) -> Option<&'static str> {
    text.find('\n').map(|index| {
        if index > 0 && text.as_bytes()[index - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_and_multiedit_share_preview_validation_and_atomic_failure() {
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.ps1");
        let before = "中文\r\nfirst\r\nsecond\r\nuntouched\nlast";
        std::fs::write(&path, before).unwrap();
        let edit = json!({"file_path":"fixture.ps1","old_string":"first\nsecond\n","new_string":"changed\n"});
        assert!(super::super::file_change::preview("Edit", &edit, root.path()).is_some());
        assert_eq!(
            super::super::tool_edit(edit, root.path().to_str().unwrap()).unwrap()["success"],
            true
        );
        let expected = "中文\r\nchanged\r\nuntouched\nlast";
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
        let failed = json!({"file_path":"fixture.ps1","edits":[{"old_string":"changed","new_string":"partial"},{"old_string":"absent","new_string":"bad"}]});
        assert!(super::super::file_change::preview("MultiEdit", &failed, root.path()).is_none());
        assert_eq!(
            super::super::tool_multi_edit(failed, root.path().to_str().unwrap()).unwrap()
                ["success"],
            false
        );
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
        let duplicate = json!({"file_path":"fixture.ps1","edits":[{"old_string":"changed","new_string":"same same"},{"old_string":"same","new_string":"unique"}]});
        assert!(super::super::file_change::preview("MultiEdit", &duplicate, root.path()).is_none());
        assert_eq!(
            super::super::tool_multi_edit(duplicate, root.path().to_str().unwrap()).unwrap()
                ["success"],
            false
        );
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
        let success = json!({"file_path":"fixture.ps1","edits":[{"old_string":"changed\n","new_string":"next\n"},{"old_string":"last","new_string":"end"}]});
        assert!(super::super::file_change::preview("MultiEdit", &success, root.path()).is_some());
        assert_eq!(
            super::super::tool_multi_edit(success, root.path().to_str().unwrap()).unwrap()
                ["success"],
            true
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "中文\r\nnext\r\nuntouched\nend".as_bytes()
        );
    }
    #[test]
    fn normalized_matches_preserve_unicode_bom_mixed_endings_and_final_newline() {
        let before = "\u{feff}头\r\none\r\ntwo\r\nkeep\n末尾";
        assert_eq!(
            replace(before, "one\ntwo\n", "替换\n两行\n", false)
                .unwrap()
                .0,
            "\u{feff}头\r\n替换\r\n两行\r\nkeep\n末尾"
        );
        assert_eq!(
            replace("a\nb\nend", "a\r\nb", "x\r\ny", false).unwrap().0,
            "x\ny\nend"
        );
        assert_eq!(
            replace("first\r\nlast", "last", "new\nline", false)
                .unwrap()
                .0,
            "first\r\nnew\r\nline"
        );
    }
    #[test]
    fn uniqueness_counts_both_newline_styles_and_all_keeps_each_style() {
        let before = "a\r\nb\r\na\nb\n";
        assert!(replace(before, "a\nb\n", "x\n", false).is_err());
        assert_eq!(
            replace(before, "a\nb\n", "x\n", true).unwrap(),
            ("x\r\nx\n".into(), 2)
        );
        assert!(replace(before, "", "x", true).is_err());
        assert!(replace(before, "absent", "x", true).is_err());
        assert_eq!(
            replace("one\rtwo", "one\rtwo", "ok", false).unwrap().0,
            "ok"
        );
    }
}
