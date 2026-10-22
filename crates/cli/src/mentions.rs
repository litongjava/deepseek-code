//! `@path` mentions in a prompt. As in the reference CLI, a mentioned text file
//! is attached with its contents and a mentioned directory with its entries,
//! so the model does not need a separate Read call.
use std::path::Path;

const MAX_FILES: usize = 10;
const MAX_FILE_BYTES: u64 = 200 * 1024;
const MAX_ENTRIES: usize = 100;

/// Mentioned paths in order of appearance, without duplicates.
fn mentioned(prompt: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for word in prompt.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else { continue };
        // Trailing punctuation belongs to the sentence, not the path.
        let path = path.trim_end_matches(|ch: char| matches!(ch, ',' | '.' | ';' | ':' | ')' | '?' | '!' | '"' | '\''));
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn attachment(workspace: &Path, mention: &str) -> Option<String> {
    let path = workspace.join(mention);
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.is_dir() {
        let mut entries: Vec<String> = std::fs::read_dir(&path)
            .ok()?
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) { format!("{name}/") } else { name }
            })
            .collect();
        entries.sort();
        let more = entries.len().saturating_sub(MAX_ENTRIES);
        entries.truncate(MAX_ENTRIES);
        if more > 0 {
            entries.push(format!("… {more} more"));
        }
        return Some(format!("<mentioned-directory path=\"{mention}\">\n{}\n</mentioned-directory>", entries.join("\n")));
    }
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let text = String::from_utf8(std::fs::read(&path).ok()?).ok()?;
    Some(format!("<mentioned-file path=\"{mention}\">\n{}\n</mentioned-file>", text.trim_end()))
}

/// The prompt with mentioned files and directories attached, or `None` when
/// nothing was mentioned.
pub fn expand(workspace: &Path, prompt: &str) -> Option<String> {
    let attachments: Vec<String> = mentioned(prompt)
        .into_iter()
        .filter_map(|mention| attachment(workspace, mention))
        .take(MAX_FILES)
        .collect();
    if attachments.is_empty() {
        return None;
    }
    Some(format!("{prompt}\n\n{}", attachments.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentioned_files_and_directories_are_attached() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("logo.png"), [0x89, 0x50, 0xff, 0xfe]).unwrap();
        let expanded = expand(dir.path(), "explain @src/main.rs, then list @src.").unwrap();
        assert!(expanded.starts_with("explain @src/main.rs, then list @src."));
        assert!(expanded.contains("<mentioned-file path=\"src/main.rs\">\nfn main() {}\n</mentioned-file>"));
        assert!(expanded.contains("<mentioned-directory path=\"src\">\nmain.rs\n</mentioned-directory>"));
        assert!(expand(dir.path(), "look at @logo.png and @missing.txt").is_none());
        assert!(expand(dir.path(), "email me at a@b.c").is_none());
    }
}
