//! Read-only, trusted workspace file access shared by Desktop adapters.
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const FILE_LIMIT: u64 = 1_000_000;
const SCAN_LIMIT: u64 = 32_000_000;
const ENTRY_LIMIT: usize = 10_000;
const RESULT_LIMIT: usize = 500;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scope {
    pub conversation_id: Option<String>,
    pub project_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileRef {
    pub root_id: String,
    pub relative_path: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub root_id: String,
    pub name: String,
    pub path: PathBuf,
}

pub fn roots(conn: &Connection, private_root: &Path, scope: &Scope) -> Result<Vec<Root>> {
    let (primary, project) = if let Some(id) = &scope.conversation_id {
        let conversation = crate::db::conversation_repo::get_conversation(conn, id)?
            .context("Conversation not found")?;
        if let Some(project_id) = &scope.project_id {
            ensure!(
                conversation.project_id.as_ref() == Some(project_id),
                "Conversation does not belong to this project"
            );
        }
        let (path, project) =
            crate::native_engine::workspace::resolve_workspace(conn, private_root, id, None)?;
        (Some(path), project)
    } else if let Some(id) = &scope.project_id {
        let project = crate::db::project_repo::require_trusted(conn, id)?;
        ensure!(!project.is_archived, "Project is archived");
        (None, Some(project))
    } else {
        return Ok(Vec::new());
    };
    let paths = if let Some(project) = project {
        project
            .folders
            .into_iter()
            .enumerate()
            .map(|(index, folder)| {
                if index == 0 {
                    primary
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(&folder.path))
                } else {
                    PathBuf::from(folder.path)
                }
            })
            .collect::<Vec<_>>()
    } else {
        primary.into_iter().collect()
    };
    paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            ensure!(
                path.is_absolute() && path.is_dir(),
                "Workspace folder is unavailable"
            );
            let path = path.canonicalize()?;
            Ok(Root {
                root_id: format!("root-{index}"),
                name: path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                path,
            })
        })
        .collect()
}

pub fn resolve(roots: &[Root], file: &FileRef, allow_missing: bool) -> Result<PathBuf> {
    let root = roots
        .iter()
        .find(|root| root.root_id == file.root_id)
        .context("Unknown workspace root")?;
    let relative = &file.relative_path;
    ensure!(
        !relative.contains(['\\', ':', '\0']) && !relative.starts_with('/'),
        "Invalid relative file path"
    );
    ensure!(
        relative.is_empty()
            || relative
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Invalid relative file path"
    );
    ensure!(
        Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Invalid relative file path"
    );
    #[cfg(windows)]
    for part in relative.split('/').filter(|part| !part.is_empty()) {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        ensure!(
            !part.ends_with([' ', '.'])
                && !matches!(
                    stem.as_str(),
                    "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                )
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit()),
            "Device paths are not supported"
        );
    }
    let path = root.path.join(relative);
    let mut existing = path.as_path();
    while allow_missing && !existing.try_exists()? {
        existing = existing.parent().context("Workspace file is unavailable")?;
    }
    let canonical = existing.canonicalize()?;
    ensure!(
        canonical.starts_with(&root.path),
        "File is outside the workspace root"
    );
    if existing == path {
        Ok(canonical)
    } else {
        Ok(path)
    }
}

fn bytes(path: &Path) -> Result<(Vec<u8>, u64)> {
    ensure!(fs::metadata(path)?.is_file(), "Expected a regular file");
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Expected a regular file");
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    Ok((bytes, metadata.len()))
}

pub fn read(roots: &[Root], file: &FileRef) -> Result<Value> {
    let path = resolve(roots, file, false)?;
    let (bytes, size) = bytes(&path)?;
    let truncated = bytes.len() as u64 > FILE_LIMIT;
    let binary = bytes.contains(&0) || (!truncated && std::str::from_utf8(&bytes).is_err());
    let notice = if truncated {
        Some("File exceeds the 1 MB inline preview limit")
    } else if binary {
        Some("Binary or non-UTF-8 file; inline preview unavailable")
    } else {
        None
    };
    let content = if truncated || binary {
        None
    } else {
        Some(String::from_utf8(bytes)?)
    };
    Ok(
        json!({"rootId":file.root_id,"relativePath":file.relative_path,"path":path,"content":content,"size":size,"binary":binary,"truncated":truncated,"notice":notice}),
    )
}

pub fn tree(roots: &[Root], file: &FileRef) -> Result<Value> {
    let path = resolve(roots, file, false)?;
    ensure!(path.is_dir(), "Expected a directory");
    let mut entries = Vec::new();
    let mut truncated = false;
    let start = Instant::now();
    for entry in ignore::WalkBuilder::new(&path)
        .hidden(false)
        .max_depth(Some(1))
        .follow_links(false)
        .build()
        .skip(1)
    {
        if entries.len() >= 2000 || start.elapsed() > Duration::from_secs(5) {
            truncated = true;
            break;
        }
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let relative_path = if file.relative_path.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", file.relative_path)
        };
        let child = FileRef {
            root_id: file.root_id.clone(),
            relative_path: relative_path.clone(),
        };
        let Ok(full) = resolve(roots, &child, false) else {
            continue;
        };
        let metadata = fs::metadata(full)?;
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        entries.push(json!({"rootId":file.root_id,"relativePath":relative_path,"name":name,"kind":if metadata.is_dir(){"directory"}else{"file"},"size":if metadata.is_file(){Some(metadata.len())}else{None}}));
    }
    entries.sort_by(|a, b| {
        a["kind"]
            .as_str()
            .cmp(&b["kind"].as_str())
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(json!({"entries":entries,"truncated":truncated}))
}

pub fn search(roots: &[Root], query: &str, cancelled: &AtomicBool) -> Result<Value> {
    ensure!(
        !query.is_empty() && query.len() <= 1024 && !query.contains(['\r', '\n', '\0']),
        "Search requires a single-line literal of 1–1024 bytes"
    );
    let start = Instant::now();
    let mut matches = Vec::new();
    let (mut scanned, mut skipped, mut total, mut visited) = (0, 0, 0u64, 0);
    let mut truncated = false;
    'roots: for root in roots {
        for entry in ignore::WalkBuilder::new(&root.path)
            .hidden(false)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build()
        {
            ensure!(!cancelled.load(Ordering::Relaxed), "Search cancelled");
            visited += 1;
            if visited > ENTRY_LIMIT
                || total >= SCAN_LIMIT
                || start.elapsed() > Duration::from_secs(5)
            {
                truncated = true;
                break 'roots;
            }
            let Ok(entry) = entry else {
                skipped += 1;
                continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root.path)?
                .to_string_lossy()
                .replace('\\', "/");
            let file = FileRef {
                root_id: root.root_id.clone(),
                relative_path: relative,
            };
            let Ok(path) = resolve(roots, &file, false) else {
                skipped += 1;
                continue;
            };
            if fs::metadata(&path).map_or(true, |metadata| metadata.len() > FILE_LIMIT) {
                skipped += 1;
                continue;
            }
            let Ok((bytes, _)) = bytes(&path) else {
                skipped += 1;
                continue;
            };
            total += bytes.len() as u64;
            if bytes.len() as u64 > FILE_LIMIT || bytes.contains(&0) {
                skipped += 1;
                continue;
            }
            let Ok(content) = std::str::from_utf8(&bytes) else {
                skipped += 1;
                continue;
            };
            scanned += 1;
            for (line_number, line) in content.lines().enumerate() {
                for (offset, _) in line.match_indices(query) {
                    if matches.len() == RESULT_LIMIT {
                        truncated = true;
                        break 'roots;
                    }
                    matches.push(json!({"rootId":root.root_id,"relativePath":file.relative_path,"line":line_number+1,"startColumn":line[..offset].encode_utf16().count()+1,"endColumn":line[..offset+query.len()].encode_utf16().count()+1,"preview":line.chars().take(500).collect::<String>()}));
                }
            }
        }
    }
    Ok(
        json!({"matches":matches,"truncated":truncated,"skippedFiles":skipped,"scannedFiles":scanned,"notice":"Literal, case-sensitive UTF-8 search; respects repository ignore rules, binary/non-UTF-8 and files over 1 MB. Limits: 500 matches, 10,000 entries, 32 MB, 5 seconds. Columns are 1-based UTF-16 with an exclusive end."}),
    )
}

pub fn diff(roots: &[Root], file: &FileRef) -> Result<crate::worktree::changes::ScopedFileDiff> {
    let path = resolve(roots, file, true)?;
    let root = roots
        .iter()
        .find(|root| root.root_id == file.root_id)
        .context("Unknown workspace root")?;
    crate::worktree::changes::scoped_diff(&root.path, &path)
}

#[derive(Debug, PartialEq, Eq)]
pub struct TargetVersion {
    length: u64,
    modified: std::time::SystemTime,
    digest: u64,
}

pub fn target_version(path: &Path) -> Result<Option<TargetVersion>> {
    use std::hash::Hasher;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Save target must be a regular file, not a link"
    );
    let mut file = fs::File::open(path)?;
    let mut digest = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.write(&buffer[..size]);
    }
    Ok(Some(TargetVersion {
        length: metadata.len(),
        modified: metadata.modified()?,
        digest: digest.finish(),
    }))
}

/// The native caller owns the target and obtains overwrite consent before calling.
pub fn save_copy(
    source: &mut fs::File,
    target: &Path,
    approved: &Option<TargetVersion>,
) -> Result<()> {
    let parent = target.parent().context("Save target has no parent")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(source, &mut staged)?;
    staged.as_file().sync_all()?;
    ensure!(
        target_version(target)? == *approved,
        "Save target changed after confirmation; choose Save as again"
    );
    if approved.is_some() {
        staged.persist(target).map_err(|error| error.error)?;
    } else {
        staged
            .persist_noclobber(target)
            .map_err(|error| error.error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Vec<Root>) {
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![Root {
            root_id: "root-0".into(),
            name: "test".into(),
            path: dir.path().canonicalize().unwrap(),
        }];
        (dir, roots)
    }
    #[test]
    fn trusted_multi_root_and_private_session_resolution() {
        use crate::db::{conversation_repo, project_repo};
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        conversation_repo::migrate_workspace_mode(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = ["first", "extra"].map(|name| {
            let path = dir.path().join(name);
            fs::create_dir(&path).unwrap();
            path.to_string_lossy().into_owned()
        });
        project_repo::insert_project(
            &conn, "project", "Project", None, None, None, false, "now", "now",
        )
        .unwrap();
        project_repo::set_folders(&conn, "project", &paths).unwrap();
        let scope = Scope {
            project_id: Some("project".into()),
            conversation_id: None,
        };
        assert!(roots(&conn, dir.path(), &scope).is_err());
        project_repo::trust_folders(&conn, "project", &paths).unwrap();
        let resolved = roots(&conn, dir.path(), &scope).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved[1].path,
            Path::new(&paths[1]).canonicalize().unwrap()
        );
        conversation_repo::insert_conversation(
            &conn,
            "session",
            None,
            None,
            None,
            Some(&paths[0]),
            None,
            false,
            false,
            false,
            "now",
            "now",
            0,
        )
        .unwrap();
        let scope = Scope {
            conversation_id: Some("session".into()),
            project_id: None,
        };
        assert_eq!(
            roots(&conn, dir.path(), &scope).unwrap()[0].path,
            dir.path().canonicalize().unwrap().join("session")
        );
        assert!(roots(
            &conn,
            dir.path(),
            &Scope {
                project_id: Some("project".into()),
                ..scope
            }
        )
        .is_err());
    }
    #[test]
    fn session_worktree_replaces_only_the_primary_root() {
        use crate::db::{conversation_repo, project_repo};
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        conversation_repo::migrate_workspace_mode(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let extra = dir.path().join("extra");
        let worktree = dir.path().join("worktree");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&extra).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&source)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        git(&[
            "worktree",
            "add",
            "-b",
            "fixture-worktree",
            worktree.to_str().unwrap(),
        ]);
        let folders = vec![
            source.to_string_lossy().into_owned(),
            extra.to_string_lossy().into_owned(),
        ];
        project_repo::insert_project(
            &conn, "project", "Project", None, None, None, false, "now", "now",
        )
        .unwrap();
        project_repo::set_folders(&conn, "project", &folders).unwrap();
        project_repo::trust_folders(&conn, "project", &folders).unwrap();
        conversation_repo::insert_conversation(
            &conn,
            "session",
            None,
            None,
            None,
            source.to_str(),
            Some("project"),
            false,
            false,
            false,
            "now",
            "now",
            0,
        )
        .unwrap();
        conn.execute("INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch) VALUES('session','project',?1,?1,?2,'fixture-worktree')", rusqlite::params![source.to_string_lossy(),worktree.to_string_lossy()]).unwrap();
        let roots = roots(
            &conn,
            dir.path(),
            &Scope {
                conversation_id: Some("session".into()),
                project_id: None,
            },
        )
        .unwrap();
        assert_eq!(roots[0].path, worktree.canonicalize().unwrap());
        assert_eq!(roots[1].path, extra.canonicalize().unwrap());
    }
    #[test]
    fn raw_save_preserves_bytes_and_rejects_changed_targets() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, [0, 255, 13, 10, 1]).unwrap();
        save_copy(&mut fs::File::open(&source).unwrap(), &target, &None).unwrap();
        assert_eq!(fs::read(&target).unwrap(), fs::read(&source).unwrap());
        let approved = target_version(&target).unwrap();
        fs::write(&target, "changed").unwrap();
        assert!(save_copy(&mut fs::File::open(&source).unwrap(), &target, &approved).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"changed");
        let approved = target_version(&target).unwrap();
        save_copy(&mut fs::File::open(&source).unwrap(), &target, &approved).unwrap();
        assert_eq!(fs::read(&target).unwrap(), fs::read(&source).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
    #[test]
    fn search_caps_matches_and_obeys_ignore_files() {
        let (dir, roots) = fixture();
        fs::write(dir.path().join("many"), "x\n".repeat(600)).unwrap();
        let result = search(&roots, "x", &AtomicBool::new(false)).unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), RESULT_LIMIT);
        assert_eq!(result["truncated"], true);
        fs::write(dir.path().join(".ignore"), "many\n").unwrap();
        assert!(
            search(&roots, "x", &AtomicBool::new(false)).unwrap()["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn rejects_traversal_and_unknown_roots() {
        let (_dir, roots) = fixture();
        for relative in [
            "../secret",
            "a/../secret",
            "C:/secret",
            "a\\secret",
            "/secret",
            "//device",
            "a//b",
        ] {
            assert!(resolve(
                &roots,
                &FileRef {
                    root_id: "root-0".into(),
                    relative_path: relative.into()
                },
                true
            )
            .is_err());
        }
        assert!(resolve(
            &roots,
            &FileRef {
                root_id: "other".into(),
                relative_path: "".into()
            },
            false
        )
        .is_err());
    }
    #[test]
    fn literal_search_reports_utf16_crlf_and_binary_skips() {
        let (dir, roots) = fixture();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/a.txt"), "😀中文 [x]\r\n中文\r\n").unwrap();
        fs::write(dir.path().join("binary"), [0, 1, 2]).unwrap();
        let result = search(&roots, "中文", &AtomicBool::new(false)).unwrap();
        assert_eq!(result["matches"][0]["startColumn"], 3);
        assert_eq!(result["matches"][0]["endColumn"], 5);
        assert_eq!(result["matches"][1]["line"], 2);
        assert_eq!(result["skippedFiles"], 1);
        assert_eq!(
            search(&roots, "[x]", &AtomicBool::new(false)).unwrap()["matches"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(search(&roots, "x", &AtomicBool::new(true)).is_err());
    }
    #[test]
    fn read_is_bounded_and_binary_is_not_decoded() {
        let (dir, roots) = fixture();
        let file = FileRef {
            root_id: "root-0".into(),
            relative_path: "data".into(),
        };
        fs::write(dir.path().join("data"), [0, 255]).unwrap();
        assert_eq!(read(&roots, &file).unwrap()["binary"], true);
        fs::write(dir.path().join("data"), vec![b'a'; FILE_LIMIT as usize + 1]).unwrap();
        assert_eq!(read(&roots, &file).unwrap()["truncated"], true);
    }
    #[cfg(unix)]
    #[test]
    fn symlink_outside_root_is_rejected() {
        let (dir, roots) = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        assert!(resolve(
            &roots,
            &FileRef {
                root_id: "root-0".into(),
                relative_path: "escape".into()
            },
            false
        )
        .is_err());
    }
}
