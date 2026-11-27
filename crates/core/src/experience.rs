//! Project experience entries: Markdown records of verified fixes, indexed into
//! the first user message and read in full only when the model needs them.

use crate::db::session_store::GlobalSetting;
use crate::services::ServiceContext;
use serde_yaml::{Mapping, Value};
use std::path::{Path, PathBuf};

/// Directory, relative to the workspace root or a first-level child directory.
pub const DIRECTORY: &str = ".agents/experience";
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 8 * 1024;
pub const MAX_ENTRIES: usize = 120;
pub const MAX_INDEX_BYTES: usize = 8 * 1024;
/// The workspace root plus one entry per first-level child directory.
pub const MAX_SCAN_DIRS: usize = 100;

const SETTING: &str = "experience.enabled";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub description: String,
    /// Relative to the workspace with `/` separators, for example
    /// `.agents/experience/a.md` or `docs/.agents/experience/a.md`.
    pub relative_path: String,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub entries: Vec<Entry>,
    /// Skipped files, for debug logging and a future doctor; never prompted.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Scan the workspace root and each first-level child directory. Read-only:
/// missing directories are empty, symlinks are never followed, and nothing is
/// created.
pub fn scan(workspace: &Path) -> Scan {
    let mut scan = Scan::default();
    for root in scan_roots(workspace) {
        let Ok(metadata) = std::fs::symlink_metadata(&root) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // `file_type` does not follow links, so a symlink is skipped here.
            if !file_type.is_file() {
                if file_type.is_symlink() {
                    scan.skipped.push((path, "symlink".into()));
                }
                continue;
            }
            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !crate::memory::files::valid_filename(filename) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > MAX_FILE_BYTES as u64 {
                scan.skipped.push((path, "file exceeds 64 KiB".into()));
                continue;
            }
            match entry_from_file(&path, workspace) {
                Ok(entry) => scan.entries.push(entry),
                Err(reason) => {
                    tracing::debug!(module = "Experience", path = %path.display(), %reason, "Skipped experience entry");
                    scan.skipped.push((path, reason));
                }
            }
        }
    }
    // Sort before deduplicating so the root's `.agents/...` wins over a child's
    // `docs/.agents/...` for the same name: `.` sorts before any directory name.
    scan.entries.sort_by(|a, b| {
        a.name
            .as_bytes()
            .cmp(b.name.as_bytes())
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });
    scan.entries.dedup_by(|a, b| a.name == b.name);
    scan
}

fn scan_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![workspace.join(DIRECTORY)];
    let mut children: Vec<PathBuf> = std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let hidden = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'));
            !hidden && entry.file_type().is_ok_and(|file_type| file_type.is_dir())
        })
        .map(|entry| entry.path().join(DIRECTORY))
        .collect();
    children.sort();
    roots.extend(children);
    if roots.len() > MAX_SCAN_DIRS {
        tracing::debug!(
            module = "Experience",
            limit = MAX_SCAN_DIRS,
            found = roots.len(),
            "Experience scan root limit reached"
        );
        roots.truncate(MAX_SCAN_DIRS);
    }
    roots
}

fn entry_from_file(path: &Path, workspace: &Path) -> Result<Entry, String> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    // The closing delimiter needs a few bytes beyond the frontmatter limit.
    let mut probe = Vec::new();
    file.take((MAX_FRONTMATTER_BYTES + 8) as u64)
        .read_to_end(&mut probe)
        .map_err(|error| error.to_string())?;
    let text = match std::str::from_utf8(&probe) {
        Ok(text) => text,
        // A multi-byte character cut by the probe boundary is not a bad file.
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&probe[..error.valid_up_to()])
                .map_err(|_| "not UTF-8".to_owned())?
        }
        Err(_) => return Err("not UTF-8".into()),
    };
    let text = text.replace("\r\n", "\n");
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| "missing frontmatter".to_owned())?;
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| "unterminated frontmatter".to_owned())?;
    let yaml = &rest[..end];
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err("frontmatter exceeds 8 KiB".into());
    }
    let (name, description) = parse_frontmatter(yaml)?;
    let relative = path
        .strip_prefix(workspace)
        .map_err(|_| "entry outside workspace".to_owned())?
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Entry {
        name,
        description,
        relative_path: relative,
    })
}

fn parse_frontmatter(yaml: &str) -> Result<(String, String), String> {
    crate::memory::files::format::yaml_subset(yaml).map_err(|error| error.to_string())?;
    let value: Value =
        serde_yaml::from_str(yaml).map_err(|_| "invalid frontmatter YAML".to_owned())?;
    crate::memory::files::format::validate_value(&value, 0).map_err(|error| error.to_string())?;
    let map = value
        .as_mapping()
        .ok_or_else(|| "frontmatter must be a mapping".to_owned())?;
    let name = field(map, "name", 80, None)?;
    let description = field(map, "description", 240, Some(512))?;
    if crate::memory::ContextManager::contains_credentials(&name)
        || crate::memory::ContextManager::contains_credentials(&description)
    {
        return Err("credential-like content".into());
    }
    Ok((name.to_owned(), description.to_owned()))
}

fn field<'a>(
    map: &'a Mapping,
    key: &str,
    max_chars: usize,
    max_bytes: Option<usize>,
) -> Result<&'a str, String> {
    let value = map
        .get(Value::String(key.into()))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing/string field: {key}"))?;
    if value.trim().is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        return Err(format!("invalid {key}"));
    }
    if let Some(max) = max_bytes.filter(|max| value.len() > *max) {
        return Err(format!("{key} exceeds {max} bytes"));
    }
    Ok(value)
}

/// One index line: `- {relative_path} | {name}: {one_line_bounded(description, LISTING_DESCRIPTION_MAX_CHARS)}`.
pub fn index_line(entry: &Entry) -> String {
    format!(
        "- {} | {}: {}",
        entry.relative_path,
        entry.name,
        crate::prompt::one_line_bounded(
            &entry.description,
            crate::prompt::LISTING_DESCRIPTION_MAX_CHARS,
        )
    )
}

/// Lines trimmed to `MAX_ENTRIES` and the block's `MAX_INDEX_BYTES` budget. A
/// truncated list ends with a marker naming how many entries were omitted.
pub fn index_lines(entries: &[Entry]) -> Vec<String> {
    let lead = "# Experience\n".len() + crate::prompt::EXPERIENCE_LEAD_IN.len();
    let mut lines = Vec::new();
    let mut used = lead;
    for entry in entries.iter().take(MAX_ENTRIES) {
        let line = index_line(entry);
        if used + 1 + line.len() > MAX_INDEX_BYTES {
            break;
        }
        used += 1 + line.len();
        lines.push(line);
    }
    if lines.len() == entries.len() {
        return lines;
    }
    loop {
        let omitted = entries.len() - lines.len();
        let marker = format!(
            "[Experience index truncated: {omitted} entries omitted; list .agents/experience/ directly.]"
        );
        if used + 1 + marker.len() <= MAX_INDEX_BYTES {
            lines.push(marker);
            return lines;
        }
        match lines.pop() {
            Some(line) => used -= 1 + line.len(),
            None => {
                lines.push(marker);
                return lines;
            }
        }
    }
}

/// `experience.enabled`, on unless the user turned it off.
pub fn enabled(context: &ServiceContext) -> bool {
    context
        .global_setting::<bool>(GlobalSetting::MachinePreference(SETTING))
        .ok()
        .flatten()
        .unwrap_or(true)
}

pub fn set_enabled(context: &ServiceContext, enabled: bool) -> anyhow::Result<()> {
    context.save_global_setting(GlobalSetting::MachinePreference(SETTING), &enabled)
}

/// True for the app's own draft directories, which must not receive the
/// project experience block. Compares canonical paths so a data directory
/// behind a symlink (`/var` vs `/private/var`) still matches.
pub fn is_internal_workspace(workspace: &Path, workspaces_dir: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(workspace).starts_with(canonical(workspaces_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(root: &Path, relative: &str, name: &str, description: &str, body: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
        path
    }

    #[test]
    fn parses_name_and_description() {
        let ws = tempdir();
        write(
            ws.path(),
            ".agents/experience/a.md",
            "alpha",
            "first symptom",
            "body",
        );
        let scan = scan(ws.path());
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].name, "alpha");
        assert_eq!(scan.entries[0].description, "first symptom");
        assert_eq!(scan.entries[0].relative_path, ".agents/experience/a.md");
        assert!(scan.skipped.is_empty());
    }

    #[test]
    fn rejects_missing_description() {
        let ws = tempdir();
        let path = ws.path().join(".agents/experience/a.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nname: alpha\n---\nbody\n").unwrap();
        let scan = scan(ws.path());
        assert!(scan.entries.is_empty());
        assert_eq!(scan.skipped.len(), 1);
        assert!(scan.skipped[0].1.contains("description"));
    }

    #[test]
    fn rejects_oversized_frontmatter() {
        let ws = tempdir();
        let path = ws.path().join(".agents/experience/a.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let padding = format!("unknown: \"{}\"", "x".repeat(MAX_FRONTMATTER_BYTES));
        std::fs::write(
            &path,
            format!("---\nname: alpha\ndescription: symptom\n{padding}\n---\nbody\n"),
        )
        .unwrap();
        assert!(scan(ws.path()).entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_and_symlink() {
        use std::os::unix::fs::symlink;
        let ws = tempdir();
        let dir = ws.path().join(".agents/experience");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bad.md"),
            b"---\nname: \xff\ndescription: symptom\n---\nbody\n",
        )
        .unwrap();
        let target = write(
            ws.path(),
            ".agents/experience/real.md",
            "real",
            "symptom",
            "body",
        );
        symlink(&target, dir.join("link.md")).unwrap();
        let scan = scan(ws.path());
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].name, "real");
        assert_eq!(scan.skipped.len(), 2);
    }

    #[test]
    fn scans_root_and_one_child_level() {
        let ws = tempdir();
        write(
            ws.path(),
            ".agents/experience/a.md",
            "alpha",
            "root",
            "body",
        );
        write(
            ws.path(),
            "docs/.agents/experience/b.md",
            "beta",
            "child",
            "body",
        );
        let scan = scan(ws.path());
        let names: Vec<_> = scan
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(
            scan.entries[1].relative_path,
            "docs/.agents/experience/b.md"
        );
    }

    #[test]
    fn does_not_scan_third_level() {
        let ws = tempdir();
        write(
            ws.path(),
            "docs/sub/.agents/experience/c.md",
            "gamma",
            "too deep",
            "body",
        );
        assert!(scan(ws.path()).entries.is_empty());
    }

    #[test]
    fn dedupes_same_name_across_roots() {
        let ws = tempdir();
        write(ws.path(), ".agents/experience/a.md", "same", "root", "body");
        write(
            ws.path(),
            "docs/.agents/experience/b.md",
            "same",
            "child",
            "body",
        );
        let scan = scan(ws.path());
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].relative_path, ".agents/experience/a.md");
        assert_eq!(scan.entries[0].description, "root");
    }

    #[test]
    fn sorts_deterministically() {
        let ws = tempdir();
        write(ws.path(), ".agents/experience/z.md", "zeta", "z", "body");
        write(ws.path(), ".agents/experience/a.md", "alpha", "a", "body");
        let first: Vec<_> = scan(ws.path())
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        let second: Vec<_> = scan(ws.path())
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(first, ["alpha", "zeta"]);
        assert_eq!(first, second);
    }

    #[test]
    fn renders_path_first_line() {
        let entry = Entry {
            name: "alpha".into(),
            description: "symptom and keywords".into(),
            relative_path: "docs/.agents/experience/a.md".into(),
        };
        assert_eq!(
            index_line(&entry),
            "- docs/.agents/experience/a.md | alpha: symptom and keywords"
        );
    }

    #[test]
    fn preserves_medium_description_and_truncates_beyond_limit() {
        let medium_description = "Build fails with ERESOLVE due to missing development peer dependencies when running pnpm install inside a clean worktree. Re-run with --frozen-lockfile after syncing the workspace catalog so all packages resolve cleanly.";
        assert!(medium_description.chars().count() > 200);
        assert!(medium_description.chars().count() <= crate::prompt::LISTING_DESCRIPTION_MAX_CHARS);
        let entry = Entry {
            name: "pnpm-peer-deps".into(),
            description: medium_description.into(),
            relative_path: ".agents/experience/pnpm-peer-deps.md".into(),
        };
        let line = index_line(&entry);
        assert_eq!(
            line,
            format!("- .agents/experience/pnpm-peer-deps.md | pnpm-peer-deps: {medium_description}")
        );
        assert!(!line.contains('…'));
        let lines = index_lines(&[entry]);
        assert_eq!(lines, vec![line]);

        let oversized = "x".repeat(crate::prompt::LISTING_DESCRIPTION_MAX_CHARS + 50);
        let oversized_entry = Entry {
            name: "oversized".into(),
            description: oversized,
            relative_path: ".agents/experience/oversized.md".into(),
        };
        let cut = index_line(&oversized_entry);
        let desc = cut.split_once(": ").unwrap().1;
        assert!(desc.ends_with('…'), "{desc}");
        assert_eq!(desc.chars().count(), crate::prompt::LISTING_DESCRIPTION_MAX_CHARS);
    }

    #[test]
    fn renders_empty_as_no_entries() {
        let reminder = crate::prompt::experience_reminder(&[]);
        assert!(reminder.contains("# Experience"));
        assert!(reminder.contains("No entries yet."));
        assert!(!reminder.contains("- .agents/experience/"));
        assert_eq!(reminder.matches("<system-reminder>").count(), 1);
    }

    #[test]
    fn truncates_after_limit() {
        let ws = tempdir();
        for index in 0..(MAX_ENTRIES + 1) {
            write(
                ws.path(),
                &format!(".agents/experience/e{index:03}.md"),
                &format!("entry-{index:03}"),
                "symptom",
                "body",
            );
        }
        let scan = scan(ws.path());
        assert_eq!(scan.entries.len(), MAX_ENTRIES + 1);
        let lines = index_lines(&scan.entries);
        assert_eq!(lines.len(), MAX_ENTRIES + 1);
        assert!(lines
            .last()
            .unwrap()
            .starts_with("[Experience index truncated: 1 entries omitted"));
        let reminder = crate::prompt::experience_reminder(&lines);
        assert!(reminder.len() <= MAX_INDEX_BYTES + 64, "{}", reminder.len());
    }

    #[test]
    fn does_not_read_body_into_index() {
        let ws = tempdir();
        write(
            ws.path(),
            ".agents/experience/a.md",
            "alpha",
            "symptom",
            "unique-body-marker",
        );
        let scan = scan(ws.path());
        assert!(index_lines(&scan.entries)
            .iter()
            .all(|line| !line.contains("unique-body-marker")));
    }

    #[test]
    fn rejects_credential_like_description() {
        let ws = tempdir();
        write(
            ws.path(),
            ".agents/experience/a.md",
            "alpha",
            "api_key = sk-abcdefgh",
            "body",
        );
        assert!(scan(ws.path()).entries.is_empty());
    }

    #[test]
    fn enabled_defaults_to_true_and_persists() {
        let root = tempdir();
        let context = crate::services::session_messages::context(root.path()).unwrap();
        assert!(enabled(&context));
        set_enabled(&context, false).unwrap();
        assert!(!enabled(&context));
        set_enabled(&context, true).unwrap();
        assert!(enabled(&context));
    }

    #[test]
    fn detects_internal_workspaces_through_symlinks() {
        let root = tempdir();
        let workspaces = root.path().join("data/workspaces");
        let internal = workspaces.join("draft-id");
        let project = root.path().join("project");
        std::fs::create_dir_all(&internal).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        assert!(is_internal_workspace(&internal, &workspaces));
        assert!(!is_internal_workspace(&project, &workspaces));
    }
}
