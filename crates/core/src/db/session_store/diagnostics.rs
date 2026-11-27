//! Read-only observations for doctor. Opening the resolver would bootstrap or repair the root.
use super::{layout, Connection, OpenFlags, OptionalExtension};
use anyhow::{ensure, Result};
use std::{io::ErrorKind, path::Path};

pub struct DirectoryHealth {
    pub stored_agents: Option<String>,
}

pub fn inspect_directory(root: &Path) -> Result<Option<DirectoryHealth>> {
    // Stores canonicalize their root, so a symlinked parent (for example
    // /var on macOS) would otherwise fail SQLite's NOFOLLOW open below.
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let path = root.join("global.db");
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        result => ensure!(result?.is_file(), "Expected a regular directory database"),
    }
    let mut c: Connection = layout::open_timed(&path, OpenFlags::SQLITE_OPEN_READ_ONLY, 250)?;
    let snapshot = c.transaction()?;
    layout::verify(&snapshot, layout::GLOBAL)?;
    let result: String = snapshot.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    ensure!(result == "ok", "Directory integrity check failed");
    let stored_agents = snapshot
        .query_row(
            "SELECT CASE WHEN length(CAST(value AS BLOB))<=1048576 THEN value ELSE '' END
         FROM global_settings WHERE domain='agent' AND name='agent-definitions'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    snapshot.commit()?;
    Ok(Some(DirectoryHealth { stored_agents }))
}
