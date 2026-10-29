//! Files the model has read in a session and the state it saw, as in the
//! reference CLI: Edit and overwriting Write need a read, and fail when the
//! file has changed on disk since then. Kept per session under the runtime
//! directory, so a change made between turns is caught too.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Modification time (ns since the epoch) and length.
type Stamp = (u128, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = path.metadata().ok()?;
    let modified = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some((modified, metadata.len()))
}

#[derive(Default)]
pub(super) struct FileState {
    seen: HashMap<String, Stamp>,
    loaded: bool,
}

pub(super) fn store(root: &Path, session: &str) -> PathBuf {
    root.join("file-state").join(format!("{session}.json"))
}

impl FileState {
    fn load(&mut self, store: Option<&Path>) {
        if self.loaded { return; }
        self.loaded = true;
        if let Some(saved) = store.and_then(|path| std::fs::read(path).ok()).and_then(|bytes| serde_json::from_slice::<HashMap<String, Stamp>>(&bytes).ok()) {
            for (path, stamp) in saved { self.seen.entry(path).or_insert(stamp); }
        }
    }

    fn save(&self, store: Option<&Path>) {
        let Some(store) = store else { return };
        if let Some(parent) = store.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(bytes) = serde_json::to_vec(&self.seen) { let _ = std::fs::write(store, bytes); }
    }

    /// The model has now seen this file as it is on disk.
    pub fn record(&mut self, path: &Path, store: Option<&Path>) {
        self.load(store);
        if let Some(stamp) = stamp(path) {
            self.seen.insert(path.to_string_lossy().into_owned(), stamp);
            self.save(store);
        }
    }

    /// Why an Edit or overwriting Write of `path` must not run, if it must not.
    /// `history` lists files read in transcripts saved before stamps existed.
    pub fn check(&mut self, tool_name: &str, path: &Path, store: Option<&Path>, history: impl FnOnce() -> Vec<PathBuf>) -> Option<String> {
        if !path.is_file() { return None; }
        self.load(store);
        let key = path.to_string_lossy().into_owned();
        match self.seen.get(&key) {
            Some(seen) if stamp(path).as_ref() == Some(seen) => None,
            Some(_) => Some("File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.".to_owned()),
            None if history().iter().any(|seen| seen == path) => { self.record(path, store); None }
            None => Some(if tool_name == "Write" {
                "File has not been read yet. Read it first before writing to it.".to_owned()
            } else {
                "File has not been read yet. Read it first before editing it.".to_owned()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_need_a_read_and_a_matching_disk_state_across_executors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let saved = store(dir.path(), "s1");
        let mut state = FileState::default();
        assert!(state.check("Edit", &file, Some(&saved), Vec::new).unwrap().contains("not been read"));
        state.record(&file, Some(&saved));
        assert!(state.check("Edit", &file, Some(&saved), Vec::new).is_none());

        // A later turn's executor loads the saved state and sees the outside change.
        std::fs::write(&file, "changed by the user").unwrap();
        let mut next = FileState::default();
        assert!(next.check("Write", &file, Some(&saved), Vec::new).unwrap().contains("modified since read"));
        next.record(&file, Some(&saved));
        assert!(next.check("Write", &file, Some(&saved), Vec::new).is_none());

        // New files and reads from older transcripts are allowed.
        assert!(next.check("Write", &dir.path().join("new.txt"), Some(&saved), Vec::new).is_none());
        let other = dir.path().join("b.txt");
        std::fs::write(&other, "x").unwrap();
        assert!(FileState::default().check("Edit", &other, None, || vec![other.clone()]).is_none());
    }
}
