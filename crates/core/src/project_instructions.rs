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
/// Files in the user's data directory that apply to every project.
const USER_FILE_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_FILES: usize = 128;

/// The reference CLI's lead-in for the instructions reminder.
pub const PREAMBLE: &str = "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::User => "user's private global instructions for all projects",
            Scope::Project => "project instructions, checked into the codebase",
            Scope::Local => "user's private project instructions, not checked in",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Instruction {
    pub path: PathBuf,
    pub scope: Scope,
    pub content: String,
    pub sha256: String,
}

/// Every instruction as the reference CLI lists it, or "" when none exist.
pub fn render(instructions: &[Instruction]) -> String {
    instructions
        .iter()
        .map(|item| format!("Contents of {} ({}):\n\n{}", item.path.display(), item.scope.label(), item.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The `<system-reminder>` the reference CLI puts at the start of the first
/// user message, or None when there are no instructions.
pub fn reminder(instructions: &[Instruction]) -> Option<String> {
    (!instructions.is_empty()).then(|| format!("<system-reminder>\n{PREAMBLE}\n\n{}\n</system-reminder>", render(instructions)))
}

/// Project instructions only, rendered; see [`load_all`] for user instructions too.
pub fn load(workspace: &Path) -> Result<String> {
    Ok(render(&load_all(None, workspace)?))
}

/// User instructions from `user_dir` first, then the workspace's ancestor chain
/// from the filesystem root down, preserving file order within each directory.
/// Local instructions follow checked-in instructions. Missing files are normal;
/// unreadable, oversized or invalid UTF-8 files fail explicitly rather than
/// silently dropping user instructions.
pub fn load_all(user_dir: Option<&Path>, workspace: &Path) -> Result<Vec<Instruction>> {
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
    let mut reader = Reader::default();
    if let Some(user_dir) = user_dir {
        for name in USER_FILE_NAMES {
            reader.read(&user_dir.join(name), Scope::User)?;
        }
    }
    reader.read_directories(&directories)?;
    Ok(reader.files)
}

#[cfg(test)]
fn load_directories(directories: &[PathBuf]) -> Result<String> {
    let mut reader = Reader::default();
    reader.read_directories(directories)?;
    Ok(render(&reader.files))
}

#[derive(Default)]
struct Reader {
    files: Vec<Instruction>,
    seen: HashSet<PathBuf>,
    total_bytes: usize,
}

impl Reader {
    fn read_directories(&mut self, directories: &[PathBuf]) -> Result<()> {
        for directory in directories {
            for name in FILE_NAMES {
                let scope = if name == "CLAUDE.local.md" { Scope::Local } else { Scope::Project };
                self.read(&directory.join(name), scope)?;
            }
        }
        Ok(())
    }

    fn read(&mut self, path: &Path, scope: Scope) -> Result<()> {
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Resolving project instructions {}", path.display())
                })
            }
        };
        if !self.seen.insert(canonical.clone()) {
            return Ok(());
        }
        anyhow::ensure!(
            self.seen.len() <= MAX_FILES,
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
        self.total_bytes += bytes.len();
        anyhow::ensure!(
            self.total_bytes <= MAX_TOTAL_BYTES,
            "Project instructions exceed {MAX_TOTAL_BYTES} total bytes at {}",
            path.display()
        );
        let content = String::from_utf8(bytes).with_context(|| {
            format!("Project instructions must be UTF-8: {}", path.display())
        })?;
        let content = content.trim_start_matches('\u{feff}');
        if !content.trim().is_empty() {
            let digest = ring::digest::digest(&ring::digest::SHA256, content.as_bytes());
            self.files.push(Instruction {
                path: path.to_path_buf(),
                scope,
                content: content.to_owned(),
                sha256: digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect(),
            });
        }
        Ok(())
    }
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

    #[test]
    fn user_instructions_come_first_with_reference_labels() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(user.path().join("CLAUDE.md"), "user marker").unwrap();
        fs::write(project.path().join("AGENTS.md"), "project marker\n").unwrap();
        fs::write(project.path().join("CLAUDE.local.md"), "local marker").unwrap();
        let all = load_all(Some(user.path()), project.path()).unwrap();
        let scopes: Vec<_> = all.iter().map(|item| item.scope).collect();
        assert_eq!(scopes, [Scope::User, Scope::Project, Scope::Local]);
        let reminder = reminder(&all).unwrap();
        assert!(reminder.starts_with(&format!("<system-reminder>\n{PREAMBLE}\n\nContents of ")));
        assert!(reminder.contains("CLAUDE.md (user's private global instructions for all projects):\n\nuser marker\n\nContents of "));
        assert!(reminder.contains("AGENTS.md (project instructions, checked into the codebase):\n\nproject marker\n\n"));
        assert!(reminder.ends_with("CLAUDE.local.md (user's private project instructions, not checked in):\n\nlocal marker\n</system-reminder>"));
        assert!(super::reminder(&[]).is_none());
    }
}
