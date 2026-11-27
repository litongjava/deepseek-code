//! Unix hook exports, shared by hosts and standalone engine workers.
use anyhow::Result;
use std::path::{Path, PathBuf};

use super::tool_hooks::HookEvent;

fn directory(root: &Path, session: &str) -> PathBuf {
    // Session IDs also come from SDK clients; never use them as path components.
    let digest = ring::digest::digest(&ring::digest::SHA256, session.as_bytes());
    let name: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    root.join("state/session-env").join(name)
}

fn kind(event: HookEvent) -> Option<&'static str> {
    match event {
        HookEvent::Setup => Some("setup"),
        HookEvent::SessionStart => Some("sessionstart"),
        HookEvent::CwdChanged => Some("cwdchanged"),
        HookEvent::FileChanged => Some("filechanged"),
        _ => None,
    }
}

pub fn hook_file(
    root: &Path,
    session: &str,
    event: HookEvent,
    index: usize,
) -> Result<Option<PathBuf>> {
    if cfg!(windows) {
        return Ok(None);
    }
    let Some(kind) = kind(event) else {
        return Ok(None);
    };
    let dir = directory(root, session);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        for path in [root.join("state/session-env"), dir.clone()] {
            match std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)
            {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
            let meta = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "Unsafe session environment directory"
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(format!("{kind}-hook-{index}.sh"));
        // create_new prevents following a planted symlink on initial creation.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(&path)?;
                anyhow::ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "Unsafe session environment file"
                );
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            Err(e) => return Err(e.into()),
        }
        return Ok(Some(path));
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, kind, index);
        Ok(None)
    }
}

fn files(root: &Path, session: &str) -> Result<Vec<(usize, usize, PathBuf)>> {
    let mut files = Vec::new();
    for path in [root.join("state/session-env"), directory(root, session)] {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "Unsafe session environment directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
            Err(e) => return Err(e.into()),
        }
    }
    let entries = match std::fs::read_dir(directory(root, session)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        anyhow::ensure!(
            !entry.file_type()?.is_symlink(),
            "Unsafe session environment file"
        );
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        for (priority, kind) in ["setup", "sessionstart", "cwdchanged", "filechanged"]
            .iter()
            .enumerate()
        {
            if let Some(index) = name
                .strip_prefix(&format!("{kind}-hook-"))
                .and_then(|s| s.strip_suffix(".sh"))
                .and_then(|s| s.parse::<usize>().ok())
            {
                files.push((priority, index, entry.path()));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Read on each Bash invocation: async hooks and other worker processes may write.
pub fn script(root: &Path, session: &str) -> Result<Option<String>> {
    if cfg!(windows) {
        return Ok(None);
    }
    let mut scripts = Vec::new();
    if let Some(parent) = std::env::var_os("CLAUDE_ENV_FILE") {
        match std::fs::read_to_string(parent) {
            Ok(s) => scripts.push(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    for (_, _, path) in files(root, session)? {
        scripts.push(std::fs::read_to_string(path)?);
    }
    let script = scripts
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((!script.is_empty()).then_some(script))
}

pub fn clear_cwd(root: &Path, session: &str) -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    for (priority, _, path) in files(root, session)? {
        if priority >= 2 {
            std::fs::write(path, "")?;
        }
    }
    Ok(())
}

/// Called after all hook writers have been cancelled at the host close boundary.
pub fn close(root: &Path, session: &str) -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    match std::fs::remove_dir_all(directory(root, session)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests;
