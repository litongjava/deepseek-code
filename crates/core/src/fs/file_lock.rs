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
}
