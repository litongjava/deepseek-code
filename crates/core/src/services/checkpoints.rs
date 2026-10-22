//! Conflict-checked turn checkpoints. Ignored files and repository metadata are never backed up.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 5000;

pub struct Capture {
    data_root: PathBuf,
    session_id: String,
    message_id: String,
    workspace: PathBuf,
    before: BTreeMap<String, Vec<u8>>,
    tracked: BTreeSet<String>,
}
impl Capture {
    /// Associate only explicit file-edit tool targets with this turn. Shell commands,
    /// unrelated agent activity and arbitrary workspace changes are not rewindable.
    pub fn record_tool(&mut self, name: &str, input: &serde_json::Value) -> Result<()> {
        if !matches!(
            name,
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "FileWrite" | "file_write"
        ) {
            return Ok(());
        }
        let raw = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .or_else(|| input.get("notebook_path"))
            .and_then(|value| value.as_str())
            .context("File edit tool has no checkpointable path")?;
        let path = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.workspace.join(raw)
        };
        no_links(&path)?;
        // Canonicalize the nearest existing ancestor, supporting newly-created
        // nested files and Windows extended-length path prefixes consistently.
        let mut ancestor = path.as_path();
        let mut suffix = Vec::new();
        while !ancestor.exists() {
            suffix.push(
                ancestor
                    .file_name()
                    .context("Invalid checkpoint target")?
                    .to_os_string(),
            );
            ancestor = ancestor
                .parent()
                .context("No existing checkpoint ancestor")?;
        }
        let mut normalized = ancestor.canonicalize()?;
        for part in suffix.into_iter().rev() {
            normalized.push(part);
        }
        let relative = normalized
            .strip_prefix(&self.workspace)
            .context("File edit is outside checkpoint workspace")?;
        let relative = relative
            .to_str()
            .context("Checkpoint path is not UTF-8")?
            .replace('\\', "/");
        scoped(&self.workspace, &relative)?;
        self.tracked.insert(relative);
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: String,
    pub message_id: String,
    pub workspace: PathBuf,
    pub created_at: String,
    pub changes: Vec<Change>,
    #[serde(default)]
    pub restored: bool,
}
#[derive(Debug, Serialize)]
pub struct RestorePreview {
    pub id: String,
    pub paths: Vec<String>,
    pub conflicts: Vec<String>,
    pub restored: bool,
}
fn no_links(path: &Path) -> Result<()> {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        cursor.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) || !cursor.is_absolute() {
            continue;
        }
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                #[cfg_attr(not(any(windows, target_os = "macos")), allow(unused_mut))]
                let mut link = metadata.file_type().is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    link |= metadata.file_attributes() & 0x400 != 0;
                }
                #[cfg(target_os = "macos")]
                if link && matches!(cursor.to_str(), Some("/var" | "/tmp" | "/etc")) {
                    // Only the OS root aliases are allowed. Workspace symlinks
                    // remain rejected, including links within these directories.
                    let expected = Path::new("/private").join(cursor.strip_prefix("/")?);
                    if cursor.canonicalize().is_ok_and(|actual| actual == expected) {
                        link = false;
                    }
                }
                ensure!(
                    !link,
                    "Checkpoint refuses symlink/junction: {}",
                    cursor.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Inspecting checkpoint path {}", cursor.display()))
            }
        }
    }
    Ok(())
}
fn scoped(workspace: &Path, name: &str) -> Result<PathBuf> {
    let relative = Path::new(name);
    ensure!(
        !name.is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Invalid checkpoint relative path"
    );
    ensure!(
        !relative.components().any(|part| part
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(".git")),
        "Repository metadata is excluded"
    );
    let path = workspace.join(relative);
    no_links(&path)?;
    Ok(path)
}
fn git_paths(workspace: &Path, args: &[&str]) -> Result<BTreeSet<String>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .output()
        .context("Listing checkpoint files with git")?;
    ensure!(
        output.status.success(),
        "Checkpoint requires a Git worktree: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.stdout.len() <= 4 * 1024 * 1024,
        "Checkpoint file listing exceeds 4 MiB"
    );
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            String::from_utf8(name.to_vec()).context("Checkpoint paths must be valid UTF-8")
        })
        .collect()
}
fn files(workspace: &Path, data_root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let paths = git_paths(
        workspace,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let ignored = git_paths(
        workspace,
        &[
            "ls-files",
            "--cached",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    ensure!(
        paths.len() <= MAX_FILES,
        "Checkpoint exceeds {MAX_FILES} files; no checkpoint created"
    );
    let mut result = BTreeMap::new();
    let mut total = 0;
    for name in paths.difference(&ignored) {
        let path = scoped(workspace, name)?;
        if path.starts_with(data_root) {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file(),
            "Checkpoint cannot capture directory/submodule {}",
            path.display()
        );
        ensure!(
            metadata.len() <= MAX_BYTES as u64,
            "Checkpoint file exceeds 64 MiB: {}",
            path.display()
        );
        let bytes = read_bounded(&path)?;
        total += bytes.len();
        ensure!(
            total <= MAX_BYTES,
            "Checkpoint exceeds 64 MiB; no checkpoint created"
        );
        result.insert(name.clone(), bytes);
    }
    Ok(result)
}
pub fn capture(
    data_root: &Path,
    session_id: &str,
    message_id: &str,
    workspace: &Path,
) -> Result<Capture> {
    ensure!(
        workspace.is_absolute(),
        "Checkpoint workspace must be absolute"
    );
    no_links(workspace)?;
    let workspace = workspace.canonicalize()?;
    let data_root = if data_root.exists() {
        data_root.canonicalize()?
    } else {
        data_root.to_owned()
    };
    let before = files(&workspace, &data_root)?;
    Ok(Capture {
        data_root,
        session_id: session_id.into(),
        message_id: message_id.into(),
        workspace,
        before,
        tracked: BTreeSet::new(),
    })
}
fn dir(root: &Path, session: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(session).is_ok(),
        "Invalid checkpoint session ID"
    );
    Ok(root.join("checkpoints").join(session))
}
fn save(root: &Path, checkpoint: &Checkpoint) -> Result<()> {
    let dir = dir(root, &checkpoint.session_id)?;
    fs::create_dir_all(&dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut temporary, checkpoint)?;
    temporary.flush()?;
    temporary
        .persist(dir.join(format!("{}.json", checkpoint.id)))
        .map_err(|error| error.error)?;
    Ok(())
}
pub fn finish(capture: Capture) -> Result<Checkpoint> {
    let after = files(&capture.workspace, &capture.data_root)?;
    let names: BTreeSet<_> = capture.before.keys().chain(after.keys()).collect();
    let changes = names
        .into_iter()
        .filter(|name| {
            capture.tracked.contains(name.as_str()) && capture.before.get(*name) != after.get(*name)
        })
        .map(|name| Change {
            path: name.clone(),
            before: capture.before.get(name).map(|value| STANDARD.encode(value)),
            after: after.get(name).map(|value| STANDARD.encode(value)),
        })
        .collect();
    let checkpoint = Checkpoint {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: capture.session_id,
        message_id: capture.message_id,
        workspace: capture.workspace,
        created_at: chrono::Utc::now().to_rfc3339(),
        changes,
        restored: false,
    };
    save(&capture.data_root, &checkpoint)?;
    Ok(checkpoint)
}
pub fn list(root: &Path, session: &str) -> Result<Vec<Checkpoint>> {
    let dir = dir(root, session)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            result.push(serde_json::from_slice::<Checkpoint>(&fs::read(path)?)?);
        }
    }
    result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(result)
}
pub fn get(root: &Path, session: &str, id: &str) -> Result<Checkpoint> {
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "Invalid checkpoint ID");
    let checkpoint: Checkpoint =
        serde_json::from_slice(&fs::read(dir(root, session)?.join(format!("{id}.json")))?)?;
    ensure!(
        checkpoint.session_id == session && checkpoint.id == id,
        "Checkpoint identity mismatch"
    );
    Ok(checkpoint)
}
fn bytes(value: &Option<String>) -> Result<Option<Vec<u8>>> {
    value
        .as_ref()
        .map(|value| STANDARD.decode(value).map_err(Into::into))
        .transpose()
}
fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "File exceeds checkpoint 64 MiB limit: {}",
        path.display()
    );
    Ok(bytes)
}
fn current(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path.exists() {
        return Ok(None);
    }
    match read_bounded(path) {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .map(|error| error.kind())
                == Some(std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
pub fn preview(root: &Path, session: &str, id: &str) -> Result<RestorePreview> {
    let checkpoint = get(root, session, id)?;
    no_links(&checkpoint.workspace)?;
    let mut conflicts = Vec::new();
    for change in &checkpoint.changes {
        let path = scoped(&checkpoint.workspace, &change.path)?;
        if current(&path)?
            != bytes(if checkpoint.restored {
                &change.before
            } else {
                &change.after
            })?
        {
            conflicts.push(change.path.clone());
        }
    }
    Ok(RestorePreview {
        id: checkpoint.id,
        paths: checkpoint
            .changes
            .iter()
            .map(|change| change.path.clone())
            .collect(),
        conflicts,
        restored: checkpoint.restored,
    })
}
fn put(path: &Path, value: &Option<String>) -> Result<()> {
    match bytes(value)? {
        Some(bytes) => {
            fs::create_dir_all(path.parent().context("Missing parent")?)?;
            let parent = path.parent().context("Missing parent")?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            if let Ok(metadata) = fs::metadata(path) {
                temporary
                    .as_file()
                    .set_permissions(metadata.permissions())?;
            }
            temporary.write_all(&bytes)?;
            temporary.flush()?;
            temporary.persist(path).map_err(|error| error.error)?;
        }
        None => {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}
pub fn restore(root: &Path, session: &str, id: &str) -> Result<RestorePreview> {
    let report = preview(root, session, id)?;
    ensure!(!report.restored, "Checkpoint was already restored");
    ensure!(
        report.conflicts.is_empty(),
        "Files changed after this checkpoint; refusing overwrite: {}",
        report.conflicts.join(", ")
    );
    let mut checkpoint = get(root, session, id)?;
    let mut applied: Vec<&Change> = Vec::new();
    for change in &checkpoint.changes {
        // Recheck immediately before each write; protect edits since preview.
        let outcome = (|| -> Result<()> {
            let path = scoped(&checkpoint.workspace, &change.path)?;
            ensure!(
                current(&path)? == bytes(&change.after)?,
                "File changed during restore: {}",
                change.path
            );
            put(&path, &change.before)
        })();
        if let Err(error) = outcome {
            let rollback = rollback(&checkpoint.workspace, &applied);
            return match rollback {
                Ok(()) => {
                    Err(error).context("Restore failed; already-restored files were rolled back")
                }
                Err(rollback) => Err(anyhow::anyhow!(
                    "Restore failed: {error:#}; rollback incomplete: {rollback:#}"
                )),
            };
        }
        applied.push(change);
    }
    checkpoint.restored = true;
    if let Err(error) = save(root, &checkpoint) {
        let changes: Vec<&Change> = checkpoint.changes.iter().collect();
        rollback(&checkpoint.workspace, &changes)
            .context("Checkpoint state could not be saved and rollback failed")?;
        return Err(error)
            .context("Checkpoint state could not be saved; file restoration rolled back");
    }
    preview(root, session, id)
}

fn rollback(workspace: &Path, applied: &[&Change]) -> Result<()> {
    let mut failures = Vec::new();
    for previous in applied.iter().rev() {
        let result = (|| -> Result<()> {
            let path = scoped(workspace, &previous.path)?;
            ensure!(
                current(&path)? == bytes(&previous.before)?,
                "Concurrent edit prevents rollback of {}",
                previous.path
            );
            put(&path, &previous.after)
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error:#}", previous.path));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, String) {
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "--quiet"])
            .arg(work.path())
            .status()
            .unwrap()
            .success());
        (work, data, uuid::Uuid::new_v4().to_string())
    }
    #[test]
    fn restores_changes_new_files_deletions_and_preserves_ignored() {
        let (work, data, session) = fixture();
        fs::write(work.path().join("中文.txt"), "before").unwrap();
        fs::write(work.path().join("deleted.txt"), "restore").unwrap();
        fs::write(work.path().join(".gitignore"), "secret.env\n").unwrap();
        fs::write(work.path().join("secret.env"), "private").unwrap();
        // Track the deletion so git still lists it after deletion.
        assert!(Command::new("git")
            .arg("-C")
            .arg(work.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        let mut before = capture(data.path(), &session, "message", work.path()).unwrap();
        for path in ["\u{4e2d}\u{6587}.txt", "deleted.txt", "new.txt"] {
            before
                .record_tool("Write", &serde_json::json!({"file_path":path}))
                .unwrap();
        }
        fs::write(work.path().join("unrelated.txt"), "other agent").unwrap();
        fs::write(work.path().join("中文.txt"), "after").unwrap();
        fs::remove_file(work.path().join("deleted.txt")).unwrap();
        fs::write(work.path().join("new.txt"), "new").unwrap();
        fs::write(work.path().join("secret.env"), "changed private").unwrap();
        let checkpoint = finish(before).unwrap();
        assert_eq!(checkpoint.changes.len(), 3);
        restore(data.path(), &session, &checkpoint.id).unwrap();
        assert_eq!(
            fs::read_to_string(work.path().join("中文.txt")).unwrap(),
            "before"
        );
        assert!(work.path().join("deleted.txt").exists());
        assert!(!work.path().join("new.txt").exists());
        assert_eq!(
            fs::read_to_string(work.path().join("unrelated.txt")).unwrap(),
            "other agent"
        );
        assert_eq!(
            fs::read_to_string(work.path().join("secret.env")).unwrap(),
            "changed private"
        );
    }
    #[test]
    fn refuses_external_edits_and_path_escape() {
        let (work, data, session) = fixture();
        fs::write(work.path().join("a"), "before").unwrap();
        let mut before = capture(data.path(), &session, "message", work.path()).unwrap();
        before
            .record_tool("Edit", &serde_json::json!({"file_path":"a"}))
            .unwrap();
        assert!(before
            .record_tool("Write", &serde_json::json!({"file_path":"../outside"}))
            .is_err());
        fs::write(work.path().join("a"), "after").unwrap();
        let checkpoint = finish(before).unwrap();
        fs::write(work.path().join("a"), "user changed").unwrap();
        assert!(restore(data.path(), &session, &checkpoint.id).is_err());
        assert_eq!(
            fs::read_to_string(work.path().join("a")).unwrap(),
            "user changed"
        );
        assert!(scoped(work.path(), "../outside").is_err());
        assert!(scoped(work.path(), ".git/config").is_err());
        assert!(get(data.path(), &session, "../escape").is_err());
    }
    #[cfg(windows)]
    #[test]
    fn refuses_windows_junction_targets() {
        use std::os::windows::process::CommandExt;
        let (work, data, session) = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "unchanged").unwrap();
        let junction = work.path().join("linked");
        let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
        let script = format!("$ErrorActionPreference='Stop'; $null=New-Item -ItemType Junction -Path '{}' -Target '{}'", quote(&junction), quote(outside.path()));
        assert!(Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(0x08000000)
            .status()
            .unwrap()
            .success());
        assert!(no_links(&junction.join("secret")).is_err());
        assert!(capture(data.path(), &session, "message", &junction).is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "unchanged"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_alias_tests {
    use super::*;

    #[test]
    fn system_temp_alias_is_allowed_but_workspace_links_are_rejected() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        no_links(dir.path()).unwrap();
        let link = dir.path().join("linked");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(no_links(&link.join("file")).is_err());
    }
}
