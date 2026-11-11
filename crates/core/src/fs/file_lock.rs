use std::fs::{File, TryLockError};
use std::io;

/// Takes a nonblocking exclusive file lock, released when the file is closed.
/// Android's standard-library file locks are unsupported; use the same kernel
/// flock ownership semantics through rustix on that platform.
pub fn try_lock_file(file: &File) -> Result<(), TryLockError> {
    #[cfg(target_os = "android")]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                let error = io::Error::from(error);
                if error.kind() == io::ErrorKind::WouldBlock {
                    TryLockError::WouldBlock
                } else {
                    TryLockError::Error(error)
                }
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    file.try_lock()
}

/// Waits for exclusive file ownership without changing the file's contents.
pub fn lock_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "android")]
    loop {
        match rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive) {
            Err(rustix::io::Errno::INTR) => continue,
            result => return result.map_err(io::Error::from),
        }
    }
    #[cfg(not(target_os = "android"))]
    file.lock()
}

/// Releases ownership now instead of leaving it to the descriptor close.
///
/// Close is not sufficient. `flock` ownership belongs to the open file
/// description, and `fork` gives the child that same description through its
/// inherited descriptor. A child that outlives the owner keeps the lock held
/// after the owner drops its `File`, so a freshly opened contender keeps
/// blocking until the child exits. An explicit unlock clears the lock on the
/// shared description, so the next owner gets the session at once.
pub fn unlock_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "android")]
    loop {
        match rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock) {
            Err(rustix::io::Errno::INTR) => continue,
            result => return result.map_err(io::Error::from),
        }
    }
    #[cfg(not(target_os = "android"))]
    file.unlock()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_file_ownership_survives_probes_and_releases_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "deepseek-file-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let open = || {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let owner = open();
        lock_file(&owner).unwrap();
        let contender = open();
        assert!(matches!(
            try_lock_file(&contender),
            Err(TryLockError::WouldBlock)
        ));
        drop(contender);
        let contender = open();
        assert!(matches!(
            try_lock_file(&contender),
            Err(TryLockError::WouldBlock)
        ));
        // Release through the production path. `flock` ownership belongs to the
        // open file description, so another thread of this test process can
        // `fork` while this descriptor is open and its child briefly holds a
        // copy until exec; close-only release is therefore not immediate in a
        // process that spawns processes. The deterministic comparison between
        // close and explicit unlock, with a child that really keeps a copy, is
        // `an_inherited_descriptor_keeps_a_close_only_lock_but_explicit_unlock_frees_it`.
        unlock_file(&owner).unwrap();
        drop(owner);
        try_lock_file(&contender).unwrap();
        assert!(matches!(
            try_lock_file(&open()),
            Err(TryLockError::WouldBlock)
        ));
        drop(contender);
        lock_file(&open()).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    /// A `fork`ed child shares the parent's open file description, so its
    /// inherited descriptor keeps the lock alive after the owner drops its
    /// `File`. Closing the owner cannot release it; an explicit unlock can.
    /// The child is a disposable process, so no unsafe `fork` happens in the
    /// multithreaded test process.
    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn an_inherited_descriptor_keeps_a_close_only_lock_but_explicit_unlock_frees_it() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        for explicit_unlock in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "deepseek-inherited-lock-{}-{}",
                std::process::id(),
                if explicit_unlock { "unlock" } else { "close" }
            ));
            let _ = std::fs::remove_file(&path);
            let open = || {
                File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)
                    .unwrap()
            };
            let owner = open();
            lock_file(&owner).unwrap();
            // The owner keeps its descriptor when it closes; clearing
            // close-on-exec lets the child inherit the same description.
            rustix::io::fcntl_setfd(&owner, rustix::io::FdFlags::empty()).unwrap();
            let mut child = Command::new("sh")
                .arg("-c")
                .arg("echo ready; exec sleep 30")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(
                ready.trim(),
                "ready",
                "the child must have started after inheriting the descriptor"
            );

            if explicit_unlock {
                unlock_file(&owner).unwrap();
            }
            drop(owner);

            let contender = open();
            let blocked = matches!(
                try_lock_file(&contender),
                Err(TryLockError::WouldBlock)
            );
            assert_eq!(
                blocked, !explicit_unlock,
                "explicit_unlock={explicit_unlock}: release must not wait for the child to exit"
            );
            if !blocked {
                unlock_file(&contender).unwrap();
            }
            drop(contender);
            // The child is still alive here: with close-only release the owner
            // is gone but the session stays blocked, which is the defect.
            let after = open();
            let available = try_lock_file(&after).is_ok();
            assert_eq!(
                available, explicit_unlock,
                "explicit_unlock={explicit_unlock}: the session must be free once the owner released it"
            );
            if available {
                unlock_file(&after).unwrap();
            }
            drop(after);

            child.kill().unwrap();
            child.wait().unwrap();
            std::fs::remove_file(&path).unwrap();
        }
    }
}
