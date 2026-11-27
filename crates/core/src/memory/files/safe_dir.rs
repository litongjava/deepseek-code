//! Directory-relative I/O. No mutation ever truncates an existing target inode.
use super::{invalid, safe_component, MemoryFileError, Result};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) struct SafeDir {
    file: File,
    // Windows has no openat: pin the selected root's ancestors without delete
    // sharing too, not only its final component. Unix uses no-follow traversal.
    _ancestors: Vec<File>,
    path: PathBuf,
    parent: Option<(Arc<SafeDir>, String)>,
}

impl SafeDir {
    /// The data root must already exist and be selected by the host. We never
    /// create an arbitrary caller-supplied chain with create_dir_all.
    pub fn root(path: &Path) -> Result<Arc<Self>> {
        if !path.is_absolute() {
            return Err(invalid("data root must be absolute"));
        }
        let path = path.canonicalize()?;
        super::scope::reject_network(&path)?;
        let ancestors = platform::pin_ancestors(&path)?;
        let file = platform::open_root(&path)?;
        platform::validate_dir(&file)?;
        Ok(Arc::new(Self {
            file,
            _ancestors: ancestors,
            path,
            parent: None,
        }))
    }

    pub fn child(self: &Arc<Self>, name: &str) -> Result<Arc<Self>> {
        self.child_mode(name, false)
    }

    pub fn child_mode(self: &Arc<Self>, name: &str, read_only: bool) -> Result<Arc<Self>> {
        safe_component(name)?;
        self.verify()?;
        let file = if read_only {
            platform::existing_child(self, name)?
        } else {
            platform::open_child(self, name)?
        };
        platform::validate_dir(&file)?;
        let result = Arc::new(Self {
            file,
            _ancestors: Vec::new(),
            path: self.path.join(name),
            parent: Some((self.clone(), name.into())),
        });
        result.verify()?;
        Ok(result)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn verify(&self) -> Result<()> {
        let current = if let Some((parent, name)) = &self.parent {
            parent.verify()?;
            platform::existing_child(parent, name)?
        } else {
            platform::open_root(&self.path)?
        };
        platform::validate_dir(&current)?;
        if platform::identity(&current)? != platform::identity(&self.file)? {
            return Err(MemoryFileError::UnsafePath(
                "directory identity changed".into(),
            ));
        }
        Ok(())
    }

    pub fn read(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        safe_component(name)?;
        self.verify()?;
        let mut file = match platform::open_file(self, name, false, false) {
            Err(MemoryFileError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None)
            }
            result => result?,
        };
        platform::validate_file(&file)?;
        let before = file.metadata()?;
        if before.len() > limit as u64 {
            return Err(invalid(format!("{name} exceeds byte limit")));
        }
        let mut data = Vec::new();
        (&mut file).take(limit as u64 + 1).read_to_end(&mut data)?;
        file.rewind()?;
        let mut confirmed = Vec::new();
        (&mut file)
            .take(limit as u64 + 1)
            .read_to_end(&mut confirmed)?;
        let after = file.metadata()?;
        self.verify_file(name, &file)?;
        if data.len() > limit {
            return Err(invalid(format!("{name} exceeds byte limit")));
        }
        if data != confirmed
            || before.len() != after.len()
            || before.modified()? != after.modified()?
            || after.len() != data.len() as u64
        {
            return Err(MemoryFileError::Conflict(name.into()));
        }
        Ok(Some(data))
    }

    /// Hash the complete index while retaining only a bounded display prefix.
    pub fn index_prefix(&self, limit: usize) -> Result<Option<(Vec<u8>, String)>> {
        use sha2::{Digest, Sha256};
        let name = "MEMORY.md";
        self.verify()?;
        let mut file = match platform::open_file(self, name, false, false) {
            Err(MemoryFileError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None)
            }
            result => result?,
        };
        platform::validate_file(&file)?;
        let before = file.metadata()?;
        let mut prefix = Vec::new();
        let mut hash = Sha256::new();
        let mut buffer = [0; 16384];
        let mut size = 0u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hash.update(&buffer[..n]);
            prefix.extend_from_slice(&buffer[..n.min(limit.saturating_sub(prefix.len()))]);
        }
        let after = file.metadata()?;
        self.verify_file(name, &file)?;
        if size != before.len() || size != after.len() || before.modified()? != after.modified()? {
            return Err(MemoryFileError::Conflict(name.into()));
        }
        Ok(Some((prefix, format!("{:x}", hash.finalize()))))
    }

    /// Copy into an unpublished shard. Do not follow links or write through
    /// existing files, even if an entry changes after the directory listing.
    pub fn copy_contents(self: &Arc<Self>, dest: &Arc<Self>) -> Result<()> {
        for name in self.list()? {
            safe_component(&name)?;
            if std::fs::symlink_metadata(self.path.join(&name))?.is_dir() {
                self.child_mode(&name, true)?
                    .copy_contents(&dest.child(&name)?)?;
            } else {
                let mut source = platform::open_file(self, &name, false, false)?;
                self.verify_file(&name, &source)?;
                let mut target = platform::open_file(dest, &name, true, true)?;
                platform::validate_file(&target)?;
                std::io::copy(&mut source, &mut target)?;
                self.verify_file(&name, &source)?;
                target.sync_all()?;
                dest.verify_file(&name, &target)?;
            }
        }
        dest.sync()
    }

    pub fn create(&self, name: &str, bytes: &[u8]) -> Result<()> {
        safe_component(name)?;
        self.verify()?;
        let mut file = platform::open_file(self, name, true, true)?;
        platform::validate_file(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self.verify()?;
        self.sync()
    }

    /// Used for control files. Data transactions stage files before publishing
    /// the journal and then use replace(), so recovery has a durable payload.
    pub fn atomic_write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        safe_component(name)?;
        let temp = format!("stage-{}", uuid::Uuid::new_v4());
        self.create(&temp, bytes)?;
        let result = self.replace(&temp, name);
        if result.is_err() {
            let _ = self.remove(&temp);
        }
        result
    }

    pub fn replace(&self, temp: &str, name: &str) -> Result<()> {
        safe_component(temp)?;
        safe_component(name)?;
        self.verify()?;
        // Both are inspected without following links. The final rename replaces
        // a directory entry, never writes through a raced final symlink/hardlink.
        platform::validate_file(&platform::open_file(self, temp, false, false)?)?;
        self.check_target(name)?;
        platform::rename(self, temp, name)?;
        self.verify()?;
        self.sync()
    }

    fn check_target(&self, name: &str) -> Result<()> {
        match platform::open_file(self, name, false, false) {
            Ok(file) => platform::validate_file(&file),
            Err(MemoryFileError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        safe_component(name)?;
        self.verify()?;
        self.check_target(name)?;
        match platform::unlink(self, name) {
            Err(MemoryFileError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(())
            }
            result => result?,
        }
        self.verify()?;
        self.sync()
    }

    pub fn list(&self) -> Result<Vec<String>> {
        self.verify()?;
        let mut names = platform::list(self)?;
        names.sort();
        Ok(names)
    }

    pub fn lock_file(&self, read_only: bool) -> Result<File> {
        self.verify()?;
        let file = platform::open_file(self, "lock", !read_only, false)?;
        platform::validate_file(&file)?;
        Ok(file)
    }

    pub fn verify_file(&self, name: &str, held: &File) -> Result<()> {
        self.verify()?;
        platform::validate_file(held)?;
        let current = platform::open_file(self, name, false, false)?;
        platform::validate_file(&current)?;
        if platform::identity(held)? != platform::identity(&current)? {
            return Err(MemoryFileError::UnsafePath("file identity changed".into()));
        }
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        platform::sync(self)
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use rustix::fs::{self, AtFlags, Mode, OFlags};
    use std::os::unix::fs::MetadataExt;

    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    pub fn open_root(path: &Path) -> Result<File> {
        let mut current = fs::open("/", DIR_FLAGS, Mode::empty()).map_err(std::io::Error::from)?;
        for component in path.components() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    current = fs::openat(&current, name, DIR_FLAGS, Mode::empty())
                        .map_err(std::io::Error::from)?;
                }
                _ => {
                    return Err(MemoryFileError::UnsafePath(
                        "non-canonical data root".into(),
                    ))
                }
            }
        }
        Ok(current.into())
    }
    pub fn pin_ancestors(_path: &Path) -> Result<Vec<File>> {
        Ok(Vec::new())
    }
    pub fn existing_child(parent: &SafeDir, name: &str) -> Result<File> {
        Ok(fs::openat(&parent.file, name, DIR_FLAGS, Mode::empty())
            .map_err(std::io::Error::from)?
            .into())
    }
    pub fn open_child(parent: &SafeDir, name: &str) -> Result<File> {
        match fs::mkdirat(&parent.file, name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                parent.sync()?;
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
        existing_child(parent, name)
    }
    pub fn identity(file: &File) -> Result<(u64, u64)> {
        let m = file.metadata()?;
        Ok((m.dev(), m.ino()))
    }
    pub fn validate_dir(file: &File) -> Result<()> {
        if !file.metadata()?.is_dir() {
            return Err(MemoryFileError::UnsafePath("not a directory".into()));
        }
        Ok(())
    }
    pub fn validate_file(file: &File) -> Result<()> {
        let m = file.metadata()?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(MemoryFileError::UnsafePath(
                "not a single-link regular file".into(),
            ));
        }
        Ok(())
    }
    pub fn open_file(dir: &SafeDir, name: &str, write: bool, exclusive: bool) -> Result<File> {
        let mut flags = OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        flags |= if write {
            OFlags::RDWR | OFlags::CREATE
        } else {
            OFlags::RDONLY
        };
        if exclusive {
            flags |= OFlags::EXCL;
        }
        Ok(
            fs::openat(&dir.file, name, flags, Mode::from_raw_mode(0o600))
                .map_err(std::io::Error::from)?
                .into(),
        )
    }
    pub fn rename(dir: &SafeDir, from: &str, to: &str) -> Result<()> {
        fs::renameat(&dir.file, from, &dir.file, to).map_err(std::io::Error::from)?;
        Ok(())
    }
    pub fn unlink(dir: &SafeDir, name: &str) -> Result<()> {
        fs::unlinkat(&dir.file, name, AtFlags::empty()).map_err(std::io::Error::from)?;
        Ok(())
    }
    pub fn list(dir: &SafeDir) -> Result<Vec<String>> {
        let mut reader = fs::Dir::read_from(&dir.file).map_err(std::io::Error::from)?;
        let mut names = Vec::new();
        for entry in &mut reader {
            let entry = entry.map_err(std::io::Error::from)?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name =
                std::str::from_utf8(bytes).map_err(|_| invalid("non-UTF-8 memory filename"))?;
            names.push(name.to_owned());
            if names.len() > 20_000 {
                return Err(invalid("memory directory entry limit exceeded"));
            }
        }
        Ok(names)
    }
    pub fn sync(dir: &SafeDir) -> Result<()> {
        dir.file.sync_all()?;
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                GetFileInformationByHandle, MoveFileExW, BY_HANDLE_FILE_INFORMATION,
                FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
                MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            },
        },
    };

    fn info(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION> {
        let mut result = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut result) }
            .map_err(std::io::Error::other)?;
        if result.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(MemoryFileError::UnsafePath("reparse point".into()));
        }
        Ok(result)
    }
    pub fn identity(file: &File) -> Result<(u64, u64)> {
        let i = info(file)?;
        Ok((
            i.dwVolumeSerialNumber as u64,
            ((i.nFileIndexHigh as u64) << 32) | i.nFileIndexLow as u64,
        ))
    }
    pub fn pin_ancestors(path: &Path) -> Result<Vec<File>> {
        let mut ancestors: Vec<_> = path.ancestors().skip(1).collect();
        ancestors.reverse();
        let mut handles = Vec::new();
        for ancestor in ancestors {
            let file = open_root(ancestor)?;
            validate_dir(&file)?;
            handles.push(file);
        }
        Ok(handles)
    }
    pub fn open_root(path: &Path) -> Result<File> {
        // No FILE_SHARE_DELETE: held ancestor handles pin every managed path
        // component, preventing replacement/rename during path-based Win32 I/O.
        Ok(std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?)
    }
    pub fn existing_child(parent: &SafeDir, name: &str) -> Result<File> {
        open_root(&parent.path.join(name))
    }
    pub fn open_child(parent: &SafeDir, name: &str) -> Result<File> {
        match std::fs::create_dir(parent.path.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        existing_child(parent, name)
    }
    pub fn validate_dir(file: &File) -> Result<()> {
        if info(file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            return Err(MemoryFileError::UnsafePath("not a directory".into()));
        }
        Ok(())
    }
    pub fn validate_file(file: &File) -> Result<()> {
        let i = info(file)?;
        if i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 || i.nNumberOfLinks != 1 {
            return Err(MemoryFileError::UnsafePath(
                "not a single-link regular file".into(),
            ));
        }
        Ok(())
    }
    pub fn open_file(dir: &SafeDir, name: &str, write: bool, exclusive: bool) -> Result<File> {
        Ok(std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .create(write && !exclusive)
            .create_new(exclusive)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(dir.path.join(name))?)
    }
    pub fn rename(dir: &SafeDir, from: &str, to: &str) -> Result<()> {
        let wide = |s: &str| {
            dir.path
                .join(s)
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        let (from, to) = (wide(from), wide(to));
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(std::io::Error::other)?;
        Ok(())
    }
    pub fn unlink(dir: &SafeDir, name: &str) -> Result<()> {
        std::fs::remove_file(dir.path.join(name))?;
        Ok(())
    }
    pub fn list(dir: &SafeDir) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir.path)? {
            names.push(
                entry?
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode memory filename"))?,
            );
            if names.len() > 20_000 {
                return Err(invalid("memory directory entry limit exceeded"));
            }
        }
        Ok(names)
    }
    // Regular files are FlushFileBuffers'd and publication uses WRITE_THROUGH.
    // Windows does not support POSIX fsync on a read-only directory handle.
    pub fn sync(_dir: &SafeDir) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("File memory requires Unix or Windows filesystem primitives");
