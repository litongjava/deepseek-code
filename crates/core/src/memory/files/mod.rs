//! Explicitly scoped file memory. No default paths, engine hooks, or migration.
//!
//! Hosts must resolve trusted roots and enforce their permission/Plan/role policy
//! before opening a store. Possessing a scope key is not user authorization.
pub(crate) mod format;
mod index;
mod name_fold;
mod safe_dir;
mod scope;
mod store;
#[cfg(test)]
mod tests;

pub use format::{MemoryDocument, MemoryKind, MAX_FILE_BYTES, MAX_FRONTMATTER_BYTES};
pub use index::{IndexContext, IndexLimits};
pub use scope::MemoryScope;
pub use store::{Diagnostic, Entry, ExpectedVersion, FileMemoryStore, Snapshot, StoreOptions};

use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum MemoryFileError {
    #[error("Invalid memory: {0}")]
    Invalid(String),
    #[error("Unsafe memory path: {0}")]
    UnsafePath(String),
    #[error("Memory changed since read: {0}")]
    Conflict(String),
    #[error("Memory journal conflicts with disk: {0}")]
    RecoveryConflict(String),
    #[error("Memory store is busy")]
    Busy,
    #[error("Memory operation cancelled before commit")]
    Cancelled,
    #[error("Memory storage is unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MemoryFileError>;

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid(message: impl Into<String>) -> MemoryFileError {
    MemoryFileError::Invalid(message.into())
}

fn safe_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 180
        || name == "."
        || name.contains("..")
        || name.ends_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(MemoryFileError::UnsafePath(name.into()));
    }
    let stem = name.split('.').next().unwrap().to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return Err(MemoryFileError::UnsafePath(name.into()));
    }
    Ok(())
}

fn memory_filename(name: &str) -> Result<()> {
    safe_component(name)?;
    if name.eq_ignore_ascii_case("MEMORY.md")
        || !name.ends_with(".md")
        || name.starts_with('.')
        || name.bytes().any(|b| b.is_ascii_uppercase())
    {
        return Err(MemoryFileError::UnsafePath(name.into()));
    }
    Ok(())
}

/// Host adapters use the exact storage filename and CAS validation rules.
pub fn valid_filename(name: &str) -> bool {
    memory_filename(name).is_ok()
}
pub fn content_hash(bytes: &[u8]) -> String {
    hash(bytes)
}

pub fn normalized_name(name: &str) -> String {
    format::name_key(name)
}

/// Read only the resident index. A manually maintained scope needs no catalog.
pub(crate) fn resident_index(root: &Path, directory: &Path) -> Result<Option<(Vec<u8>, String)>> {
    if !directory.join("MEMORY.md").try_exists()? {
        return Ok(None);
    }
    scoped_directory(root, directory, true)?.index_prefix(IndexLimits::default().bytes + 1)
}

pub(crate) fn copy_session_memory(root: &Path, source: &Path, dest: &Path) -> Result<()> {
    let source = match scoped_directory(root, source, true) {
        Err(MemoryFileError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    source.copy_contents(&scoped_directory(root, dest, false)?)
}

fn scoped_directory(
    root: &Path,
    directory: &Path,
    read_only: bool,
) -> Result<std::sync::Arc<safe_dir::SafeDir>> {
    let mut dir = safe_dir::SafeDir::root(root)?;
    for component in directory
        .strip_prefix(root)
        .map_err(|_| invalid("index outside scope"))?
        .iter()
    {
        dir = dir.child_mode(
            component
                .to_str()
                .ok_or_else(|| invalid("invalid memory directory"))?,
            read_only,
        )?;
    }
    Ok(dir)
}
