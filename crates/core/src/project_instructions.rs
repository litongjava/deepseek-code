//! Project instructions shared by every frontend. Only the workspace's ancestor
//! chain is searched; instructions in unrelated descendants are never loaded.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

const FILE_NAMES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "CLAUDE.local.md",
];
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_FILES: usize = 128;

/// Load instructions from filesystem root to workspace, preserving file order
/// within each directory. Local instructions follow checked-in instructions.
/// Missing files are normal; unreadable, oversized or invalid UTF-8 files fail
/// explicitly rather than silently dropping user instructions.
pub fn load(workspace: &Path) -> Result<String> {
    Ok(load_with_files(workspace)?.0)
}

pub fn load_with_files(workspace: &Path) -> Result<(String, Vec<(PathBuf, String)>)> {
    let workspace = workspace.canonicalize().with_context(|| {
        format!(
            "Resolving project instructions workspace {}",
            workspace.display()
        )
    })?;
    anyhow::ensure!(
        workspace.is_dir(),
        "Instructions workspace is not a directory: {}",
        workspace.display()
    );
    let mut directories: Vec<_> = workspace.ancestors().map(Path::to_path_buf).collect();
    directories.reverse();
    load_directories_with_files(&directories)
}

#[cfg(test)]
fn load_directories(directories: &[PathBuf]) -> Result<String> {
    Ok(load_directories_with_files(directories)?.0)
}

fn load_directories_with_files(
    directories: &[PathBuf],
) -> Result<(String, Vec<(PathBuf, String)>)> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut total_bytes = 0;
    let mut prompt = String::new();
    for directory in directories {
        for name in FILE_NAMES {
            let path = directory.join(name);
            let canonical = match path.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Resolving project instructions {}", path.display())
                    })
                }
            };
            if !seen.insert(canonical.clone()) {
                continue;
            }
            anyhow::ensure!(
                seen.len() <= MAX_FILES,
                "Project instructions exceed {MAX_FILES} files at {}",
                path.display()
            );
            let metadata = canonical
                .metadata()
                .with_context(|| format!("Inspecting project instructions {}", path.display()))?;
            anyhow::ensure!(
                metadata.is_file(),
                "Project instructions must be a regular file: {}",
                path.display()
            );
            anyhow::ensure!(
                metadata.len() <= MAX_FILE_BYTES as u64,
                "Project instructions exceed {MAX_FILE_BYTES} bytes: {}",
                path.display()
            );
            // Bound the read too, in case the file grows after metadata inspection.
            let mut bytes = Vec::new();
            File::open(&canonical)
                .and_then(|file| {
                    file.take((MAX_FILE_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)
                })
                .with_context(|| format!("Reading project instructions {}", path.display()))?;
            anyhow::ensure!(
                bytes.len() <= MAX_FILE_BYTES,
                "Project instructions exceed {MAX_FILE_BYTES} bytes: {}",
                path.display()
            );
            total_bytes += bytes.len();
            anyhow::ensure!(
                total_bytes <= MAX_TOTAL_BYTES,
                "Project instructions exceed {MAX_TOTAL_BYTES} total bytes at {}",
                path.display()
            );
            let content = String::from_utf8(bytes).with_context(|| {
                format!("Project instructions must be UTF-8: {}", path.display())
            })?;
            let content = content.trim_start_matches('\u{feff}');
            if !content.trim().is_empty() {
                let digest = ring::digest::digest(&ring::digest::SHA256, content.as_bytes());
                files.push((
                    path.clone(),
                    digest
                        .as_ref()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                ));
                prompt.push_str(&format!(
                    "\nContents of {} (project instructions):\n{content}\n",
                    path.display()
                ));
            }
        }
    }
    Ok((prompt, files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn hierarchy_is_ordered_and_ignores_descendants() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("project");
        fs::create_dir_all(child.join(".claude")).unwrap();
        fs::create_dir_all(child.join("unrelated")).unwrap();
        fs::write(root.path().join("CLAUDE.md"), "ancestor marker").unwrap();
        for (name, content) in FILE_NAMES.into_iter().zip([
            "agents marker",
            "project marker",
            "hidden marker",
            "local marker",
        ]) {
            fs::write(child.join(name), content).unwrap();
        }
        fs::write(child.join("unrelated/CLAUDE.md"), "unrelated marker").unwrap();
        let result = load(&child).unwrap();
        let positions: Vec<_> = [
            "ancestor marker",
            "agents marker",
            "project marker",
            "hidden marker",
            "local marker",
        ]
        .iter()
        .map(|marker| result.find(marker).unwrap())
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!result.contains("unrelated marker"));
    }

    #[test]
    fn duplicate_paths_are_read_once() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("CLAUDE.md"), "unique marker").unwrap();
        let result = load_directories(&[root.path().into(), root.path().into()]).unwrap();
        assert_eq!(result.matches("unique marker").count(), 1);
    }

    #[test]
    fn invalid_utf8_reports_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("CLAUDE.md"), [0xff]).unwrap();
        let error = load_directories(&[root.path().into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("UTF-8") && error.contains("CLAUDE.md"));
    }

    #[test]
    fn per_file_and_total_limits_are_enforced() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("CLAUDE.md"),
            vec![b'a'; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        assert!(load_directories(&[root.path().into()])
            .unwrap_err()
            .to_string()
            .contains("exceed"));
        fs::remove_file(root.path().join("CLAUDE.md")).unwrap();
        let mut directories = Vec::new();
        for index in 0..5 {
            let directory = root.path().join(index.to_string());
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("CLAUDE.md"), vec![b'a'; MAX_FILE_BYTES]).unwrap();
            directories.push(directory);
        }
        assert!(load_directories(&directories)
            .unwrap_err()
            .to_string()
            .contains("total bytes"));
    }

    #[test]
    fn missing_files_and_bom_are_supported() {
        let root = tempfile::tempdir().unwrap();
        assert!(load_directories(&[root.path().into()]).unwrap().is_empty());
        fs::write(root.path().join("CLAUDE.md"), "\u{feff}hello").unwrap();
        let result = load_directories(&[root.path().into()]).unwrap();
        assert!(result.contains("hello") && !result.contains('\u{feff}'));
    }
}
