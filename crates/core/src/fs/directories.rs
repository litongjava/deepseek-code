//! One level of a directory tree, for choosing project folders on a server.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Entries returned for one directory; the rest are counted in `truncated`.
pub const MAX_DIRECTORY_ENTRIES: usize = 1000;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct DirectoryListing {
    /// The listed directory, canonical and absolute.
    pub path: String,
    /// Its parent, absent at a filesystem root.
    pub parent: Option<String>,
    /// The user's home directory, the default starting point.
    pub home: Option<String>,
    /// Filesystem roots: drive letters on Windows, `/` elsewhere.
    pub roots: Vec<String>,
    /// Subdirectories sorted by name, case-insensitively.
    pub directories: Vec<DirectoryEntry>,
    /// Subdirectories left out beyond `MAX_DIRECTORY_ENTRIES`.
    pub truncated: usize,
}

fn display(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    // Windows canonical paths carry a verbatim prefix that users never type.
    text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text)
}

fn roots() -> Vec<String> {
    if cfg!(windows) {
        ('A'..='Z')
            .map(|letter| format!("{letter}:\\"))
            .filter(|root| Path::new(root).is_dir())
            .collect()
    } else {
        vec!["/".into()]
    }
}

/// Lists the subdirectories of `path` (the home directory when `None`).
/// Hidden directories (leading dot) are skipped unless `show_hidden`; entries
/// that cannot be read, such as broken links, are skipped silently.
pub fn list_directories(path: Option<&str>, show_hidden: bool) -> Result<DirectoryListing> {
    let home = dirs::home_dir();
    let requested = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => home.clone().context("No home directory; enter a folder path")?,
    };
    anyhow::ensure!(requested.is_absolute(), "Folder path must be absolute: {}", requested.display());
    let dir = std::fs::canonicalize(&requested)
        .with_context(|| format!("Folder not found: {}", requested.display()))?;
    anyhow::ensure!(dir.is_dir(), "Not a folder: {}", dir.display());
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("Cannot open {}", dir.display()))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        // Follows links, so a linked folder is offered like a real one.
        if !entry.path().is_dir() {
            continue;
        }
        directories.push(DirectoryEntry { path: display(&entry.path()), name });
    }
    directories.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.name.cmp(&b.name)));
    let truncated = directories.len().saturating_sub(MAX_DIRECTORY_ENTRIES);
    directories.truncate(MAX_DIRECTORY_ENTRIES);
    Ok(DirectoryListing {
        parent: dir.parent().map(display),
        path: display(&dir),
        home: home.as_deref().map(display),
        roots: roots(),
        directories,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_visible_subdirectories_sorted() {
        let root = tempfile::tempdir().unwrap();
        for name in ["beta", "Alpha", ".hidden", "gamma"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        std::fs::write(root.path().join("file.txt"), "x").unwrap();
        let listing = list_directories(root.path().to_str(), false).unwrap();
        let names: Vec<_> = listing.directories.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"]);
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(listing.path, display(&canonical));
        assert_eq!(listing.parent.as_deref(), canonical.parent().map(display).as_deref());
        assert_eq!(listing.directories[0].path, display(&canonical.join("Alpha")));
        assert_eq!(listing.truncated, 0);
        let all = list_directories(root.path().to_str(), true).unwrap();
        assert!(all.directories.iter().any(|d| d.name == ".hidden"));
    }

    #[test]
    fn rejects_relative_missing_and_file_paths() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(list_directories(Some("relative/path"), false).is_err());
        assert!(list_directories(root.path().join("missing").to_str(), false).is_err());
        assert!(list_directories(file.to_str(), false).is_err());
    }

    #[test]
    fn caps_large_directories() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..MAX_DIRECTORY_ENTRIES + 3 {
            std::fs::create_dir(root.path().join(format!("d{index:05}"))).unwrap();
        }
        let listing = list_directories(root.path().to_str(), false).unwrap();
        assert_eq!(listing.directories.len(), MAX_DIRECTORY_ENTRIES);
        assert_eq!(listing.truncated, 3);
    }
}
