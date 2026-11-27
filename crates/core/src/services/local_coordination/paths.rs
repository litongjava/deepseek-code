//! Private coordination domain paths.
//!
//! Resolution is independent of the conversation data directory: only an
//! explicit argument, [`COORDINATION_DIR_ENV`], or the home directory
//! participate. Reading or resolving a path never creates anything.
//!
//! On Unix, preparation and state files are walked, created, opened and
//! permission-changed through pinned directory descriptors with no-follow
//! steps, so a pathname substituted after resolution or during creation cannot
//! redirect creation or modify an unrelated object. A resolved
//! [`CoordinationPaths::root`] is therefore a locator, not a persistent
//! filesystem identity: callers must prepare and use state through these
//! helpers.

use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::OsStr,
    fs::File,
    path::{Component, Path, PathBuf},
};

/// Explicit coordination root override.
pub const COORDINATION_DIR_ENV: &str = "DEEPSEEK_CODE_COORDINATION_DIR";

const DATABASE_FILE: &str = "coordination.sqlite3";
const DEFAULT_HOME_DIR: &str = ".deepseek-code/coordination/v1";

/// Resolved coordination domain root, shared by hosts running as this user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinationPaths {
    pub root: PathBuf,
}

impl CoordinationPaths {
    /// Resolve the coordination root without touching the filesystem.
    ///
    /// Precedence is `explicit` > `env` > `home/.deepseek-code/coordination/v1`.
    /// Relative paths resolve against `launch_cwd` (the pre-`-C` invocation
    /// directory). Existing ancestors are canonicalized; missing components are
    /// appended lexically. Empty overrides and traversal above the filesystem
    /// root are rejected.
    pub fn resolve(
        explicit: Option<&Path>,
        env: Option<&OsStr>,
        home: Option<&Path>,
        launch_cwd: &Path,
    ) -> Result<Self> {
        let chosen = match (explicit, env) {
            (Some(path), _) => path.to_path_buf(),
            (None, Some(value)) => PathBuf::from(value),
            (None, None) => home
                .context("Cannot locate the home directory; set DEEPSEEK_CODE_COORDINATION_DIR")?
                .join(DEFAULT_HOME_DIR),
        };
        ensure!(
            !chosen.as_os_str().is_empty(),
            "Coordination directory override is empty"
        );
        let absolute = if chosen.is_absolute() {
            chosen
        } else {
            ensure!(
                launch_cwd.is_absolute(),
                "Launch directory must be absolute for a relative coordination directory"
            );
            launch_cwd.join(chosen)
        };
        Ok(Self {
            root: resolve_ancestors(&normalize(&absolute)?)?,
        })
    }

    /// Registry database inside the coordination root.
    pub fn database(&self) -> PathBuf {
        self.root.join(DATABASE_FILE)
    }

    /// Create missing coordination directories and validate the root.
    ///
    /// Missing components are created 0700 through pinned descriptors and the
    /// final root must be a real directory owned by this user with exactly
    /// 0700 permissions. Existing directories are never chmod'd; a symlink, an
    /// unsafe owner/mode or a non-directory is rejected instead. A directory
    /// created concurrently by a peer under the same root is opened and
    /// validated, not treated as an error.
    pub fn prepare_private(&self) -> Result<()> {
        #[cfg(unix)]
        {
            // `open_dirs` validates the final root; the descriptor is only held
            // for the duration of this call.
            drop(unix::open_dirs(&self.root, true)?);
            Ok(())
        }
        #[cfg(not(unix))]
        {
            prepare_private_portable(&self.root)
        }
    }
}

/// Lexically normalize an absolute path, rejecting traversal above the root.
fn normalize(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => {
                normalized.push(part);
                depth += 1;
            }
            Component::ParentDir => {
                if depth == 0 {
                    bail!(
                        "Coordination directory escapes the filesystem root: {}",
                        path.display()
                    );
                }
                normalized.pop();
                depth -= 1;
            }
        }
    }
    ensure!(
        normalized.is_absolute(),
        "Coordination directory must be absolute: {}",
        path.display()
    );
    ensure!(
        normalized.file_name().is_some(),
        "Coordination directory must not be the filesystem root"
    );
    Ok(normalized)
}

/// Canonicalize the deepest existing ancestor of the root's parent, then append
/// the missing components unchanged. The root leaf itself is preserved so
/// preparation can reject a substituted symlink leaf.
fn resolve_ancestors(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("Coordination directory must have a parent")?;
    let leaf = path
        .file_name()
        .context("Coordination directory must have a final component")?;
    let mut existing = parent;
    loop {
        match std::fs::metadata(existing) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir(),
                    "Coordination directory ancestor is not a directory: {}",
                    existing.display()
                );
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing
                    .parent()
                    .context("Coordination directory has no existing ancestor")?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot inspect {}", existing.display()))
            }
        }
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("Cannot resolve {}", existing.display()))?;
    for component in parent
        .strip_prefix(existing)
        .context("Coordination directory ancestor is not a prefix")?
        .components()
    {
        match component {
            Component::Normal(part) => resolved.push(part),
            _ => bail!(
                "Unexpected component in coordination directory: {}",
                path.display()
            ),
        }
    }
    resolved.push(leaf);
    Ok(resolved)
}

/// Split a state-file path into its private parent directory and single leaf.
/// The parent walk rejects `..` and the filesystem root.
#[cfg(unix)]
fn split_state_path(path: &Path) -> Result<(&Path, &OsStr)> {
    ensure!(
        path.is_absolute(),
        "Coordination file path must be absolute: {}",
        path.display()
    );
    let name = path
        .file_name()
        .context("Coordination file path must end in a file name")?;
    let parent = path
        .parent()
        .context("Coordination file path must have a parent")?;
    Ok((parent, name))
}

/// Open an existing private coordination directory; never creates one.
///
/// The returned error keeps the original errno in its chain; a genuine missing
/// component surfaces as `rustix::io::Errno::NOENT` for the caller to match.
#[cfg(unix)]
pub(super) fn open_private_dir(path: &Path) -> Result<File> {
    match unix::open_dirs(path, false) {
        Ok(fd) => Ok(File::from(fd)),
        Err(error) => {
            let missing = error.chain().any(|cause| {
                cause.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT)
            });
            if missing {
                Err(anyhow::anyhow!(rustix::io::Errno::NOENT))
            } else {
                Err(error)
            }
        }
    }
}

/// Create or reopen a private state file relative to a validated directory.
#[cfg(unix)]
pub(super) fn create_private_at(parent: &File, name: &OsStr, exclusive: bool) -> Result<File> {
    use std::os::fd::AsFd;
    unix::private_file_at(parent.as_fd(), name, exclusive)
}

/// Open an existing private state file; never creates, truncates or chmods.
#[cfg(unix)]
pub(super) fn open_existing_private_at(
    parent: &File,
    name: &OsStr,
    writable: bool,
) -> std::io::Result<File> {
    use std::os::fd::AsFd;
    unix::open_existing_private_at(parent.as_fd(), name, writable)
}

/// Open a coordination state file with private permissions.
///
/// `create_new` refuses an existing path; otherwise an existing regular file is
/// opened without truncation. On Unix the parent is obtained through pinned
/// descriptors, the open uses no-follow/CLOEXEC/nonblocking flags, and type,
/// owner and an exact 0600 mode are validated on the opened descriptor before
/// any permission change. An existing unsafe object is rejected unchanged.
#[allow(dead_code)] // Used by coordination state files from the store round onwards.
pub(crate) fn private_file(path: &Path, create_new: bool) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let (parent, leaf) = split_state_path(path)?;
        let directory = unix::open_dirs(parent, false)?;
        unix::private_file_at(directory.as_fd(), leaf, create_new)
    }
    #[cfg(not(unix))]
    {
        private_file_portable(path, create_new)
    }
}

/// Non-Unix fallback: this module's native identity and permission guarantees
/// are Unix-only. The later owner adapter rejects participation on platforms
/// without a native adapter.
#[cfg(not(unix))]
fn prepare_private_portable(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("Cannot create coordination directory {}", root.display()))?;
    let metadata = std::fs::symlink_metadata(root)
        .with_context(|| format!("Cannot inspect {}", root.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "Coordination directory is not a real directory: {}",
        root.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn private_file_portable(path: &Path, create_new: bool) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    let file = options
        .open(path)
        .with_context(|| format!("Cannot open coordination file {}", path.display()))?;
    ensure!(
        file.metadata()
            .with_context(|| format!("Cannot inspect {}", path.display()))?
            .is_file(),
        "Coordination file is not a regular file: {}",
        path.display()
    );
    Ok(file)
}

/// Descriptor-relative Unix implementation.
#[cfg(unix)]
mod unix {
    use super::Result;
    use anyhow::{bail, ensure, Context};
    use rustix::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        fs::{fchmod, fstat, mkdirat, open, openat, statat, AtFlags, FileType, Mode, OFlags, Stat},
        io::Errno,
    };
    use std::{
        ffi::{OsStr, OsString},
        fs::File,
        path::{Component, Path},
        time::Duration,
    };

    const OWNER_RW: Mode = Mode::RUSR.union(Mode::WUSR);
    const PRIVATE_MODE: u32 = 0o700;
    const FILE_MODE: u32 = 0o600;
    /// Group/other permission bits and all special bits.
    const STRICT_BITS: u32 = 0o7077;
    /// Bounded reopen attempts while a peer finishes a restrictive-umask create.
    const BOUNDED_RETRIES: u32 = 20;
    const RETRY_DELAY: Duration = Duration::from_millis(5);

    const DIR_IO_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    // Traversal-only ancestors may be execute-only, so Linux uses an O_PATH
    // descriptor there.
    #[cfg(target_os = "linux")]
    const ANCESTOR_FLAGS: OFlags = OFlags::PATH
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    #[cfg(not(target_os = "linux"))]
    const ANCESTOR_FLAGS: OFlags = DIR_IO_FLAGS;

    struct Step {
        fd: OwnedFd,
        was_missing: bool,
    }

    /// Permission and special bits, in one width on every Unix target.
    ///
    /// Apple targets use `libc::stat`, whose `st_mode` is a 16-bit `mode_t`;
    /// Linux uses `u32`. Normalizing here keeps every mask and comparison
    /// type-correct without changing what is inspected.
    fn mode_bits(st: &Stat) -> u32 {
        #[allow(clippy::unnecessary_cast)]
        {
            (st.st_mode as u32) & 0o7777
        }
    }

    pub(super) fn check_uid(actual: u32, expected: u32) -> Result<()> {
        ensure!(
            actual == expected,
            "Coordination path is owned by uid {actual}, expected {expected}"
        );
        Ok(())
    }

    /// Validate an opened directory. Private roots and state-file parents must
    /// be owned by this user with exactly 0700; ordinary ancestors only have to
    /// be directories.
    pub(super) fn check_directory(st: &Stat, private: bool) -> Result<()> {
        ensure!(
            FileType::from_raw_mode(st.st_mode) == FileType::Directory,
            "Coordination entry is not a directory (mode {:o})",
            mode_bits(st)
        );
        if private {
            check_uid(st.st_uid, rustix::process::geteuid().as_raw())
                .context("Coordination directory has an unexpected owner")?;
            ensure!(
                mode_bits(st) == PRIVATE_MODE,
                "Coordination directory must be mode 0700, found {:04o}",
                mode_bits(st)
            );
        }
        Ok(())
    }

    /// Validate a directory this process just created, before changing its
    /// mode. Only owner bits may be masked by the process umask.
    pub(super) fn check_created_directory(st: &Stat) -> Result<()> {
        ensure!(
            FileType::from_raw_mode(st.st_mode) == FileType::Directory,
            "New coordination entry is not a directory"
        );
        check_uid(st.st_uid, rustix::process::geteuid().as_raw())
            .context("New coordination directory has an unexpected owner")?;
        ensure!(
            mode_bits(st) & STRICT_BITS == 0,
            "New coordination directory has unexpected group/other or special bits: {:04o}",
            mode_bits(st)
        );
        Ok(())
    }

    /// Validate an opened state-file descriptor: regular file owned by this user.
    pub(super) fn check_regular_owner(st: &Stat) -> Result<()> {
        ensure!(
            FileType::from_raw_mode(st.st_mode) == FileType::RegularFile,
            "Coordination file is not a regular file (mode {:o})",
            mode_bits(st)
        );
        check_uid(st.st_uid, rustix::process::geteuid().as_raw())
            .context("Coordination file has an unexpected owner")
    }

    /// Open (and optionally create) the directory described by `path`.
    ///
    /// The walk starts at a descriptor for `/`, opens each native component
    /// individually with no-follow flags, and uses the opened ancestor as the
    /// anchor for the next step. Missing components are created only when
    /// `create` is set; from the first missing component onward every entry must
    /// validate as private, including one a peer just created.
    pub(super) fn open_dirs(path: &Path, create: bool) -> Result<OwnedFd> {
        let names = walk_names(path)?;
        let mut directory =
            open("/", DIR_IO_FLAGS, Mode::empty()).context("Cannot open the filesystem root")?;
        let last = names.len() - 1;
        let mut require_private = false;
        for (index, name) in names.iter().enumerate() {
            let leaf = index == last;
            let step = if leaf {
                ensure_component(directory.as_fd(), name, true, create)
            } else {
                step_component(directory.as_fd(), name, require_private, create, false)
            }
            .with_context(|| format!("Cannot prepare {}", path.display()))?;
            require_private |= leaf || step.was_missing;
            directory = step.fd;
        }
        let st = fstat(&directory).with_context(|| format!("Cannot inspect {}", path.display()))?;
        check_directory(&st, true)
            .with_context(|| format!("Unsafe coordination directory {}", path.display()))?;
        Ok(directory)
    }

    /// Validate the final component and return its ordinary directory FD.
    fn ensure_component(
        parent: BorrowedFd<'_>,
        name: &OsStr,
        require_private: bool,
        create: bool,
    ) -> Result<Step> {
        step_component(parent, name, require_private, create, true)
    }

    /// One walk step. `final_step` selects ordinary directory-I/O flags for the
    /// object being validated instead of traversal-only flags.
    fn step_component(
        parent: BorrowedFd<'_>,
        name: &OsStr,
        require_private: bool,
        create: bool,
        final_step: bool,
    ) -> Result<Step> {
        let flags = if final_step {
            DIR_IO_FLAGS
        } else {
            ANCESTOR_FLAGS
        };
        match open_retrying(parent, name, flags) {
            Ok(fd) => {
                let st = fstat(&fd)
                    .with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
                let checked = if require_private {
                    check_directory(&st, true)
                } else {
                    check_directory(&st, false)
                };
                checked.with_context(|| {
                    format!("Unsafe coordination directory {}", name.to_string_lossy())
                })?;
                verify_entry(parent, name, &st)?;
                Ok(Step {
                    fd,
                    was_missing: false,
                })
            }
            Err(Errno::NOENT) if create => {
                check_creation_parent(parent, name)?;
                #[cfg(test)]
                hooks::run(hooks::PreparePoint::BeforeMkdir, name)?;
                match mkdirat(parent, name, Mode::RWXU) {
                    Ok(()) => {
                        #[cfg(test)]
                        hooks::run(hooks::PreparePoint::AfterMkdir, name)?;
                        let fd = finish_created_directory(parent, name)?;
                        Ok(Step {
                            fd,
                            was_missing: true,
                        })
                    }
                    Err(Errno::EXIST) => Ok(Step {
                        // The entry was absent a moment ago (ENOENT), so it is
                        // part of the private suffix: a peer may only have won
                        // the create race with the same private directory.
                        fd: reopen_competing(parent, name, true, flags)?,
                        was_missing: true,
                    }),
                    Err(error) => Err(error).with_context(|| {
                        format!(
                            "Cannot create coordination directory {}",
                            name.to_string_lossy()
                        )
                    }),
                }
            }
            Err(Errno::NOENT) => Err::<Step, _>(Errno::NOENT).with_context(|| {
                format!(
                    "Coordination directory is missing: {}",
                    name.to_string_lossy()
                )
            }),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "Cannot open coordination directory {}",
                    name.to_string_lossy()
                )
            }),
        }
    }

    /// Open an existing entry, retrying briefly while a peer finishes a create
    /// under a restrictive umask (owner bits are still missing).
    fn open_retrying(
        parent: BorrowedFd<'_>,
        name: &OsStr,
        flags: OFlags,
    ) -> std::result::Result<OwnedFd, Errno> {
        let mut attempt = 0;
        loop {
            match openat(parent, name, flags, Mode::empty()) {
                Err(Errno::ACCESS)
                    if attempt < BOUNDED_RETRIES && pending_owned_directory(parent, name) =>
                {
                    attempt += 1;
                    std::thread::sleep(RETRY_DELAY);
                }
                result => return result,
            }
        }
    }

    fn pending_owned_directory(parent: BorrowedFd<'_>, name: &OsStr) -> bool {
        statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|st| {
            FileType::from_raw_mode(st.st_mode) == FileType::Directory
                && st.st_uid == rustix::process::geteuid().as_raw()
        })
    }

    /// Handle a peer winning the `mkdirat` race: open and validate the entry
    /// that now exists, without chmodding it.
    fn reopen_competing(
        parent: BorrowedFd<'_>,
        name: &OsStr,
        require_private: bool,
        flags: OFlags,
    ) -> Result<OwnedFd> {
        let mut attempt = 0;
        loop {
            match openat(parent, name, flags, Mode::empty()) {
                Ok(fd) => {
                    let st = fstat(&fd)
                        .with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
                    let checked = if require_private {
                        check_directory(&st, true)
                    } else {
                        check_directory(&st, false)
                    };
                    checked.with_context(|| {
                        format!(
                            "Competing coordination entry {} is unsafe",
                            name.to_string_lossy()
                        )
                    })?;
                    verify_entry(parent, name, &st)?;
                    return Ok(fd);
                }
                Err(Errno::ACCESS)
                    if attempt < BOUNDED_RETRIES && pending_owned_directory(parent, name) =>
                {
                    attempt += 1;
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "Cannot open the competing coordination directory {}",
                            name.to_string_lossy()
                        )
                    })
                }
            }
        }
    }

    /// Refuse to create inside a directory another user can replace the new
    /// entry in. Never chmod an ancestor to qualify it.
    fn check_creation_parent(parent: BorrowedFd<'_>, name: &OsStr) -> Result<()> {
        let st = fstat(parent).context("Cannot inspect the coordination directory parent")?;
        ensure!(
            FileType::from_raw_mode(st.st_mode) == FileType::Directory,
            "Coordination parent is not a directory"
        );
        let owner_trusted = st.st_uid == rustix::process::geteuid().as_raw() || st.st_uid == 0;
        let protected = mode_bits(&st) & 0o022 == 0 || mode_bits(&st) & 0o1000 != 0;
        ensure!(
            owner_trusted && protected,
            "Refusing to create {} inside a directory other users can modify",
            name.to_string_lossy()
        );
        Ok(())
    }

    /// Finish a directory this call just created: reopen with no-follow
    /// directory flags, validate it, then set 0700 through the validated
    /// descriptor.
    fn finish_created_directory(parent: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd> {
        match openat(parent, name, DIR_IO_FLAGS, Mode::empty()) {
            Ok(fd) => finalize_created(parent, name, fd),
            #[cfg(target_os = "linux")]
            Err(Errno::ACCESS) => finalize_created_path_fd(parent, name),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "Cannot open the new coordination directory {}",
                    name.to_string_lossy()
                )
            }),
        }
    }

    fn finalize_created(parent: BorrowedFd<'_>, name: &OsStr, fd: OwnedFd) -> Result<OwnedFd> {
        let before =
            fstat(&fd).with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
        check_created_directory(&before)?;
        fchmod(&fd, Mode::RWXU)
            .with_context(|| format!("Cannot set the mode of {}", name.to_string_lossy()))?;
        let after =
            fstat(&fd).with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
        check_directory(&after, true).with_context(|| {
            format!(
                "Unsafe new coordination directory {}",
                name.to_string_lossy()
            )
        })?;
        verify_entry(parent, name, &after)?;
        Ok(fd)
    }

    /// Restrictive-umask fallback. An unprivileged creator with umask 0777
    /// creates mode 0000, which the ordinary RDONLY open cannot open. Hold an
    /// O_PATH descriptor, change the mode through the kernel's descriptor
    /// reference in `/proc/self/fd`, then reopen and compare identity.
    #[cfg(target_os = "linux")]
    fn finalize_created_path_fd(parent: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd> {
        let held = openat(parent, name, ANCESTOR_FLAGS, Mode::empty()).with_context(|| {
            format!(
                "Cannot open the new coordination directory {}",
                name.to_string_lossy()
            )
        })?;
        let before =
            fstat(&held).with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
        check_created_directory(&before)?;
        chmod_via_procfs(held.as_fd())?;
        let reopened = openat(parent, name, DIR_IO_FLAGS, Mode::empty()).with_context(|| {
            format!(
                "Cannot reopen the new coordination directory {}",
                name.to_string_lossy()
            )
        })?;
        let after = fstat(&reopened)
            .with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
        ensure!(
            after.st_dev == before.st_dev && after.st_ino == before.st_ino,
            "New coordination directory {} was replaced during preparation",
            name.to_string_lossy()
        );
        check_directory(&after, true).with_context(|| {
            format!(
                "Unsafe new coordination directory {}",
                name.to_string_lossy()
            )
        })?;
        verify_entry(parent, name, &after)?;
        drop(held);
        Ok(reopened)
    }

    #[cfg(target_os = "linux")]
    fn chmod_via_procfs(fd: BorrowedFd<'_>) -> Result<()> {
        let proc_dir = open("/proc/self/fd", DIR_IO_FLAGS, Mode::empty()).context(
            "Linux procfs is required to secure a directory created under a restrictive umask",
        )?;
        let filesystem = rustix::fs::fstatfs(&proc_dir).context("Cannot identify /proc/self/fd")?;
        ensure!(
            filesystem.f_type as i128 == libc::PROC_SUPER_MAGIC as i128,
            "Linux procfs is required to secure a directory created under a restrictive umask"
        );
        let number = fd.as_raw_fd().to_string();
        rustix::fs::chmodat(&proc_dir, number.as_str(), Mode::RWXU, AtFlags::empty()).context(
            "Cannot set the coordination directory mode through its descriptor reference",
        )?;
        Ok(())
    }

    /// Confirm the directory entry still names the object the descriptor
    /// refers to, so a detached/replaced leaf is reported before success.
    fn verify_entry(parent: BorrowedFd<'_>, name: &OsStr, opened: &Stat) -> Result<()> {
        let linked = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
            .with_context(|| format!("Cannot re-inspect {}", name.to_string_lossy()))?;
        ensure!(
            linked.st_dev == opened.st_dev && linked.st_ino == opened.st_ino,
            "Coordination entry {} changed during preparation",
            name.to_string_lossy()
        );
        Ok(())
    }

    /// Components of an absolute normalized path; rejects `..` and root-only.
    fn walk_names(path: &Path) -> Result<Vec<OsString>> {
        ensure!(
            path.is_absolute(),
            "Coordination path must be absolute: {}",
            path.display()
        );
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => names.push(name.to_os_string()),
                Component::ParentDir => bail!(
                    "Coordination path must not contain '..': {}",
                    path.display()
                ),
                Component::Prefix(_) => {
                    bail!("Coordination path must be a Unix path: {}", path.display())
                }
            }
        }
        ensure!(
            !names.is_empty(),
            "Coordination path must not be the filesystem root"
        );
        Ok(names)
    }

    /// Open a private state file relative to a validated parent descriptor.
    pub(super) fn private_file_at(
        parent: BorrowedFd<'_>,
        name: &OsStr,
        create_new: bool,
    ) -> Result<File> {
        let common = OFlags::RDWR
            .union(OFlags::NOFOLLOW)
            .union(OFlags::CLOEXEC)
            .union(OFlags::NONBLOCK)
            .union(OFlags::NOCTTY);
        let create_flags = common.union(OFlags::CREATE).union(OFlags::EXCL);
        match openat(parent, name, create_flags, OWNER_RW) {
            Ok(fd) => {
                let created = fstat(&fd)
                    .with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
                check_regular_owner(&created).context("The new coordination file is unsafe")?;
                ensure!(
                    mode_bits(&created) & STRICT_BITS == 0,
                    "New coordination file has unexpected group/other or special bits: {:04o}",
                    mode_bits(&created)
                );
                fchmod(&fd, OWNER_RW).with_context(|| {
                    format!("Cannot set the mode of {}", name.to_string_lossy())
                })?;
                let after = fstat(&fd)
                    .with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
                ensure!(
                    mode_bits(&after) == FILE_MODE,
                    "Coordination file must be mode 0600, found {:04o}",
                    mode_bits(&after)
                );
                Ok(File::from(fd))
            }
            Err(Errno::EXIST) if create_new => Err(Errno::EXIST).with_context(|| {
                format!(
                    "Coordination file already exists: {}",
                    name.to_string_lossy()
                )
            }),
            Err(Errno::EXIST) => {
                if let Ok(st) = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                    check_regular_owner(&st)
                        .context("The existing coordination entry is unsafe")?;
                }
                let fd = openat(parent, name, common, Mode::empty()).with_context(|| {
                    format!("Cannot open coordination file {}", name.to_string_lossy())
                })?;
                let st = fstat(&fd)
                    .with_context(|| format!("Cannot inspect {}", name.to_string_lossy()))?;
                check_regular_owner(&st).context("The existing coordination file is unsafe")?;
                ensure!(
                    mode_bits(&st) == FILE_MODE,
                    "Existing coordination file must be mode 0600, found {:04o}",
                    mode_bits(&st)
                );
                Ok(File::from(fd))
            }
            Err(error) => Err(error).with_context(|| {
                format!("Cannot create coordination file {}", name.to_string_lossy())
            }),
        }
    }

    /// Open an existing private state file: never creates, truncates or chmods.
    ///
    /// `NotFound` means the entry is genuinely absent. `InvalidData` means the
    /// entry exists but is a symlink, FIFO, wrong owner or not mode 0600.
    pub(super) fn open_existing_private_at(
        parent: BorrowedFd<'_>,
        name: &OsStr,
        writable: bool,
    ) -> std::io::Result<File> {
        let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidData);
        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile => {}
            Ok(_) => return Err(invalid()),
            Err(error) => return Err(std::io::Error::from(error)),
        }
        let base = OFlags::NOFOLLOW
            .union(OFlags::CLOEXEC)
            .union(OFlags::NONBLOCK)
            .union(OFlags::NOCTTY);
        let flags = if writable {
            base.union(OFlags::RDWR)
        } else {
            base.union(OFlags::RDONLY)
        };
        let fd = openat(parent, name, flags, Mode::empty()).map_err(std::io::Error::from)?;
        let st = fstat(&fd).map_err(std::io::Error::from)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
            || st.st_uid != rustix::process::geteuid().as_raw()
            || mode_bits(&st) != FILE_MODE
        {
            return Err(invalid());
        }
        Ok(File::from(fd))
    }

    /// Test-only fault injection at the mkdir boundary. The callback runs on the
    /// calling thread for the installed point and leaf only.
    #[cfg(test)]
    pub(super) mod hooks {
        use super::Result;
        use std::{
            cell::RefCell,
            ffi::{OsStr, OsString},
        };

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum PreparePoint {
            BeforeMkdir,
            AfterMkdir,
        }

        pub(crate) type Callback = Box<dyn FnOnce(PreparePoint, &OsStr) -> Result<()>>;
        type Entry = (PreparePoint, OsString, Callback);

        thread_local! {
            static HOOK: RefCell<Option<Entry>> = RefCell::new(None);
        }

        /// Clears the thread-local hook when dropped, including on unwind.
        pub(crate) struct Guard;

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = HOOK.try_with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        }

        pub(crate) fn install(point: PreparePoint, leaf: &OsStr, callback: Callback) -> Guard {
            HOOK.with(|slot| {
                *slot.borrow_mut() = Some((point, leaf.to_os_string(), callback));
            });
            Guard
        }

        pub(super) fn run(point: PreparePoint, leaf: &OsStr) -> Result<()> {
            let callback = HOOK.with(|slot| {
                let mut slot = slot.borrow_mut();
                let matches = slot.as_ref().is_some_and(|(installed, installed_leaf, _)| {
                    *installed == point && installed_leaf == leaf
                });
                if matches {
                    slot.take().map(|(_, _, callback)| callback)
                } else {
                    None
                }
            });
            if let Some(callback) = callback {
                callback(point, leaf)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::local_coordination::load_settings;
    use anyhow::{anyhow, bail, Context, Result};
    use std::{
        cell::Cell,
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
        process::{Child, Command, Output, Stdio},
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    fn canonical(path: &Path) -> Result<PathBuf> {
        Ok(path.canonicalize()?)
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> Result<u32> {
        Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o7777)
    }

    #[cfg(unix)]
    fn inode_of(path: &Path) -> Result<(u64, u64)> {
        let metadata = fs::symlink_metadata(path)?;
        Ok((metadata.dev(), metadata.ino()))
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) -> Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    fn fresh_nonce(label: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}x{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn wait_for<T>(
        what: &str,
        deadline: Instant,
        mut poll: impl FnMut() -> Result<Option<T>>,
    ) -> Result<T> {
        loop {
            if let Some(value) = poll()? {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {what}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // ---- Process-helper plumbing (child processes only) --------------------

    /// Test-process umask guard. Only used inside the dedicated helper child;
    /// the parallel parent harness must never change the process-global umask.
    #[cfg(unix)]
    struct UmaskGuard(rustix::fs::Mode);

    #[cfg(unix)]
    impl UmaskGuard {
        fn set(mask: rustix::fs::RawMode) -> Self {
            Self(rustix::process::umask(rustix::fs::Mode::from_raw_mode(
                mask,
            )))
        }
    }

    #[cfg(unix)]
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            rustix::process::umask(self.0);
        }
    }

    #[cfg(unix)]
    fn spawn_helper(mode: &str, dir: &Path, nonce: &str, extra: &[(&str, &str)]) -> Result<Child> {
        let executable = std::env::current_exe().context("helper executable")?;
        let mut command = Command::new(executable);
        command
            .arg("--exact")
            .arg("services::local_coordination::paths::tests::lc_process_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("DCL_LC_HELPER_MODE", mode)
            .env("DCL_LC_HELPER_DIR", dir)
            .env("DCL_LC_HELPER_NONCE", nonce)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra {
            command.env(key, value);
        }
        command
            .spawn()
            .with_context(|| format!("cannot start the {mode} helper"))
    }

    #[cfg(unix)]
    fn finish_helper(mut child: Child, deadline: Instant) -> Result<Output> {
        loop {
            if child.try_wait()?.is_some() {
                return Ok(child.wait_with_output()?);
            }
            if Instant::now() >= deadline {
                let pid = child.id();
                let _ = child.kill();
                let output = child.wait_with_output()?;
                bail!(
                    "helper process {pid} did not finish: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn require_helper_success(output: &Output) -> Result<()> {
        ensure!(
            output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        ensure!(
            stdout.contains("1 passed; 0 failed"),
            "the helper did not run exactly one test: {stdout}"
        );
        Ok(())
    }

    #[cfg(unix)]
    fn read_markers(dir: &Path, prefix: &str, nonce: &str) -> Result<Vec<(PathBuf, String)>> {
        let mut found = Vec::new();
        if !dir.exists() {
            return Ok(found);
        }
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            ensure!(
                content.starts_with(nonce),
                "marker {} does not carry the expected nonce",
                path.display()
            );
            found.push((path, content));
        }
        found.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(found)
    }

    #[cfg(unix)]
    fn observation(content: &str, nonce: &str) -> Result<(u64, u64, u32)> {
        let mut lines = content.lines();
        ensure!(lines.next() == Some(nonce), "unexpected nonce line");
        let line = lines.next().context("missing observation line")?;
        let mut parts = line.split_whitespace();
        let device = parts.next().context("missing device")?.parse()?;
        let inode = parts.next().context("missing inode")?.parse()?;
        let mode = u32::from_str_radix(parts.next().context("missing mode")?, 8)?;
        Ok((device, inode, mode))
    }

    #[cfg(unix)]
    fn helper_concurrent(dir: &Path, nonce: &str) -> Result<()> {
        let root = dir.join("root");
        let leaf = root.file_name().context("root leaf")?.to_os_string();
        let ready = dir.join(format!("ready-{}", std::process::id()));
        let directory = dir.to_path_buf();
        let hook_nonce = nonce.to_string();
        let fired = Rc::new(Cell::new(0u32));
        let hook_fired = Rc::clone(&fired);
        let umask = UmaskGuard::set(0o077);
        let hook = unix::hooks::install(
            unix::hooks::PreparePoint::BeforeMkdir,
            &leaf,
            Box::new(move |_, _| {
                hook_fired.set(hook_fired.get() + 1);
                fs::write(&ready, format!("{hook_nonce}\n{}", std::process::id()))?;
                wait_for(
                    "the release marker",
                    Instant::now() + Duration::from_secs(60),
                    || Ok(directory.join("release").exists().then_some(())),
                )?;
                Ok(())
            }),
        );
        let result = CoordinationPaths { root: root.clone() }.prepare_private();
        drop(hook);
        drop(umask);
        result.context("concurrent helper prepare failed")?;
        ensure!(
            fired.get() == 1,
            "the before-mkdir hook did not fire exactly once"
        );
        let metadata = fs::symlink_metadata(&root)?;
        fs::write(
            dir.join(format!("result-{}", std::process::id())),
            format!(
                "{nonce}\n{} {} {:o}\n",
                metadata.dev(),
                metadata.ino(),
                metadata.permissions().mode() & 0o7777
            ),
        )?;
        Ok(())
    }

    #[cfg(unix)]
    fn helper_restrictive_umask(dir: &Path, nonce: &str) -> Result<()> {
        let root = dir.join("a").join("b");
        let umask = UmaskGuard::set(0o777);
        CoordinationPaths { root: root.clone() }.prepare_private()?;
        let marker = root.join("marker");
        drop(private_file(&marker, true)?);
        drop(umask);
        let first = mode_of(root.parent().context("first created level")?)?;
        let second = mode_of(&root)?;
        let file_mode = mode_of(&marker)?;
        let uid = fs::symlink_metadata(&root)?.uid();
        fs::write(
            dir.join(format!("result-{}", std::process::id())),
            format!("{nonce}\n{first:o} {second:o} {file_mode:o} {uid}\n"),
        )?;
        Ok(())
    }

    #[cfg(unix)]
    fn helper_data_dir(dir: &Path, nonce: &str) -> Result<()> {
        let home = PathBuf::from(std::env::var_os("DCL_LC_HELPER_HOME").context("helper home")?);
        let launch =
            PathBuf::from(std::env::var_os("DCL_LC_HELPER_LAUNCH").context("helper launch")?);
        let data =
            PathBuf::from(std::env::var_os("DEEPSEEK_CODE_DATA_DIR").context("data directory")?);
        ensure!(data.exists(), "data directory fixture is missing");
        let paths = CoordinationPaths::resolve(None, None, Some(&home), &launch)?;
        ensure!(
            !paths.root.starts_with(&data),
            "the coordination root was derived from the data directory"
        );
        ensure!(
            !paths.root.exists(),
            "resolve created the coordination root: {}",
            paths.root.display()
        );
        fs::write(
            dir.join(format!("result-{}", std::process::id())),
            format!("{nonce}\n{}\n{}\n", paths.root.display(), data.display()),
        )?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "process helper: started only by the coordination process tests"]
    fn lc_process_helper() {
        let mode = std::env::var("DCL_LC_HELPER_MODE").expect("helper mode");
        let dir = PathBuf::from(std::env::var_os("DCL_LC_HELPER_DIR").expect("helper directory"));
        let nonce = std::env::var("DCL_LC_HELPER_NONCE").expect("helper nonce");
        let outcome = match mode.as_str() {
            "concurrent" => helper_concurrent(&dir, &nonce),
            "restrictive-umask" => helper_restrictive_umask(&dir, &nonce),
            "data-dir" => helper_data_dir(&dir, &nonce),
            other => Err(anyhow!("unknown helper mode {other}")),
        };
        if let Err(error) = outcome {
            let _ = fs::write(
                dir.join(format!("failed-{}-{nonce}", std::process::id())),
                format!("{error:#}\n"),
            );
            panic!("helper {mode} failed: {error:#}");
        }
    }

    // ---- Round-01 acceptance tests (preserved) -----------------------------

    #[test]
    fn coordination_paths_ignore_data_dir() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let cwd_a = temp.path().join("nested/cwd-a");
        let cwd_b = temp.path().join("cwd-b");
        for dir in [&home, &cwd_a, &cwd_b] {
            fs::create_dir_all(dir)?;
        }
        // Two unrelated data directories with different settings must not feed
        // coordination resolution: a data-dir override is not an input at all.
        let data_a = temp.path().join("data-a");
        let data_b = temp.path().join("data-b");
        for (dir, enabled) in [(&data_a, "false"), (&data_b, "true")] {
            crate::data_layout::write_config_file(
                crate::data_layout::DataLayout::from_root(dir).config_file("settings.json"),
                format!(r#"{{"localCoordination":{{"enabled":{enabled}}}}}"#),
            )?;
        }
        assert_ne!(load_settings(&data_a)?, load_settings(&data_b)?);

        let explicit = temp.path().join("explicit-root");
        let from_a = CoordinationPaths::resolve(
            Some(&explicit),
            Some(OsStr::new("ignored-env")),
            Some(&home),
            &cwd_a,
        )?;
        let from_b = CoordinationPaths::resolve(
            Some(&explicit),
            Some(OsStr::new("ignored-env")),
            Some(&home),
            &cwd_b,
        )?;
        assert_eq!(from_a, from_b);
        assert_eq!(from_a.root, canonical(temp.path())?.join("explicit-root"));
        assert_eq!(
            from_a.database(),
            canonical(temp.path())?.join("explicit-root/coordination.sqlite3")
        );
        assert!(!from_a.root.starts_with(&data_a));
        assert!(!from_a.root.starts_with(&data_b));

        // explicit > env > home.
        let env_root = temp.path().join("env-root");
        let by_env =
            CoordinationPaths::resolve(None, Some(env_root.as_os_str()), Some(&home), &cwd_a)?;
        assert_eq!(by_env.root, canonical(temp.path())?.join("env-root"));
        let by_explicit = CoordinationPaths::resolve(
            Some(&explicit),
            Some(env_root.as_os_str()),
            Some(&home),
            &cwd_a,
        )?;
        assert_eq!(
            by_explicit.root,
            canonical(temp.path())?.join("explicit-root")
        );
        let by_home = CoordinationPaths::resolve(None, None, Some(&home), &cwd_a)?;
        assert_eq!(
            by_home.root,
            canonical(&home)?.join(".deepseek-code/coordination/v1")
        );

        // A relative override follows the supplied launch cwd (pre-`-C`), and
        // normalization applies before any filesystem lookup.
        let relative = Path::new("relative/coord");
        let in_a = CoordinationPaths::resolve(
            None,
            Some(OsStr::new("relative/coord")),
            Some(&home),
            &cwd_a,
        )?;
        let in_b = CoordinationPaths::resolve(
            None,
            Some(OsStr::new("relative/coord")),
            Some(&home),
            &cwd_b,
        )?;
        assert_eq!(in_a.root, canonical(&cwd_a)?.join(relative));
        assert_eq!(in_b.root, canonical(&cwd_b)?.join(relative));
        assert_ne!(in_a, in_b);
        let normalized = CoordinationPaths::resolve(
            None,
            Some(OsStr::new("nested/../coord")),
            Some(&home),
            &cwd_a,
        )?;
        assert_eq!(normalized.root, canonical(&cwd_a)?.join("coord"));

        // Empty overrides, a missing home and traversal above the root fail.
        assert!(CoordinationPaths::resolve(
            Some(Path::new("")),
            Some(env_root.as_os_str()),
            Some(&home),
            &cwd_a
        )
        .is_err());
        assert!(
            CoordinationPaths::resolve(None, Some(OsStr::new("")), Some(&home), &cwd_a).is_err()
        );
        assert!(CoordinationPaths::resolve(None, None, None, &cwd_a).is_err());
        let mut above_root = PathBuf::new();
        for _ in 0..=cwd_a
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
            .count()
        {
            above_root.push("..");
        }
        assert!(
            CoordinationPaths::resolve(None, Some(above_root.as_os_str()), None, &cwd_a).is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_paths_are_private() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("coord space Ω");
        let paths = CoordinationPaths::resolve(Some(&root), None, None, temp.path())?;
        assert!(!paths.root.exists(), "resolve must not create anything");
        paths.prepare_private()?;
        assert_eq!(mode_of(&paths.root)?, 0o700);
        assert_eq!(fs::symlink_metadata(&paths.root)?.uid(), unsafe {
            libc::geteuid()
        });

        // Private files are 0600, owned by this user, and roundtrip content.
        let marker = paths.root.join("marker file ü.txt");
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut file = private_file(&marker, true)?;
            file.write_all(b"coordination")?;
            file.seek(SeekFrom::Start(0))?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            assert_eq!(text, "coordination");
        }
        assert_eq!(mode_of(&marker)?, 0o600);
        assert_eq!(fs::symlink_metadata(&marker)?.uid(), unsafe {
            libc::geteuid()
        });
        assert!(private_file(&marker, true).is_err(), "create_new refuses");

        // A directory is not a valid private file, and no-follow rejects symlinks.
        assert!(private_file(&paths.root, false).is_err());
        let target = temp.path().join("symlink-target");
        let link = paths.root.join("symlink");
        std::os::unix::fs::symlink(&target, &link)?;
        assert!(private_file(&link, false).is_err());

        // A symlink leaf is rejected instead of being followed or chmod'd.
        let link_root = temp.path().join("linked-root");
        std::os::unix::fs::symlink(&paths.root, &link_root)?;
        assert!(CoordinationPaths { root: link_root }
            .prepare_private()
            .is_err());
        assert_eq!(mode_of(&paths.root)?, 0o700);

        // File leaves and too-open existing roots are rejected, not modified.
        let file_root = temp.path().join("file-root");
        fs::write(&file_root, b"not a directory".to_vec())?;
        assert!(CoordinationPaths { root: file_root }
            .prepare_private()
            .is_err());

        let open_root = temp.path().join("open-root");
        fs::create_dir(&open_root)?;
        set_mode(&open_root, 0o755)?;
        assert!(CoordinationPaths {
            root: open_root.clone()
        }
        .prepare_private()
        .is_err());
        assert_eq!(mode_of(&open_root)?, 0o755);

        // Missing intermediate directories are created private as well.
        let nested = temp.path().join("missing-a/missing-b/coord");
        CoordinationPaths {
            root: nested.clone(),
        }
        .prepare_private()?;
        assert_eq!(mode_of(&nested)?, 0o700);
        assert_eq!(mode_of(nested.parent().context("created parent")?)?, 0o700);
        Ok(())
    }

    // ---- Round-01b repair tests -------------------------------------------

    #[cfg(unix)]
    #[test]
    fn coordination_created_leaf_swap_never_chmods_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("unrelated-target");
        fs::create_dir(&target)?;
        set_mode(&target, 0o755)?;
        fs::write(target.join("sentinel"), b"untouched".to_vec())?;
        let target_inode = inode_of(&target)?;

        let root = temp.path().join("coord");
        let leaf = root.file_name().context("leaf")?.to_os_string();
        let moved = temp.path().join("coord-moved");
        let fired = Rc::new(Cell::new(0u32));
        let hook_fired = Rc::clone(&fired);
        let hook_root = root.clone();
        let hook_target = target.clone();
        let _hook = unix::hooks::install(
            unix::hooks::PreparePoint::AfterMkdir,
            &leaf,
            Box::new(move |_, _| {
                hook_fired.set(hook_fired.get() + 1);
                fs::rename(&hook_root, &moved)?;
                std::os::unix::fs::symlink(&hook_target, &hook_root)?;
                Ok(())
            }),
        );
        assert!(
            CoordinationPaths { root: root.clone() }
                .prepare_private()
                .is_err(),
            "a leaf replaced by a symlink must be rejected"
        );
        assert_eq!(fired.get(), 1, "the after-mkdir hook fired exactly once");
        assert_eq!(inode_of(&target)?, target_inode);
        assert_eq!(mode_of(&target)?, 0o755, "the target must not be chmod'd");
        assert_eq!(fs::read(target.join("sentinel"))?, b"untouched".to_vec());
        assert_eq!(
            fs::read_dir(&target)?.count(),
            1,
            "no files may be added under the target"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_parent_replacement_cannot_redirect_creation() -> Result<()> {
        // Case A: the parent pathname is replaced after resolution.
        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("fixture-parent");
        fs::create_dir(&parent)?;
        let replacement = temp.path().join("replacement");
        fs::create_dir(&replacement)?;
        let paths =
            CoordinationPaths::resolve(Some(&parent.join("coord")), None, None, temp.path())?;
        fs::rename(&parent, temp.path().join("fixture-parent-moved"))?;
        std::os::unix::fs::symlink(&replacement, &parent)?;
        assert!(
            paths.prepare_private().is_err(),
            "a substituted parent pathname must be rejected"
        );
        assert!(!replacement.join("coord").exists());
        assert_eq!(fs::read_dir(&replacement)?.count(), 0);

        // Case B: the parent is replaced after its descriptor is pinned.
        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("held-parent");
        fs::create_dir(&parent)?;
        let replacement = temp.path().join("held-replacement");
        fs::create_dir(&replacement)?;
        let root = parent.join("coord");
        let leaf = root.file_name().context("leaf")?.to_os_string();
        let moved = temp.path().join("held-parent-moved");
        let hook_moved = moved.clone();
        let hook_parent = parent.clone();
        let hook_replacement = replacement.clone();
        let _hook = unix::hooks::install(
            unix::hooks::PreparePoint::BeforeMkdir,
            &leaf,
            Box::new(move |_, _| {
                fs::rename(&hook_parent, &hook_moved)?;
                std::os::unix::fs::symlink(&hook_replacement, &hook_parent)?;
                Ok(())
            }),
        );
        let outcome = CoordinationPaths { root: root.clone() }.prepare_private();
        drop(_hook);
        assert_eq!(
            fs::read_dir(&replacement)?.count(),
            0,
            "the replacement directory stayed empty"
        );
        assert!(!replacement.join("coord").exists());
        if outcome.is_ok() {
            assert_eq!(mode_of(&moved.join("coord"))?, 0o700);
        }

        // A symlink in a state-file parent path cannot receive the file.
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("state-root");
        CoordinationPaths { root: root.clone() }.prepare_private()?;
        let target = temp.path().join("state-target");
        fs::create_dir(&target)?;
        let link = root.join("link-parent");
        std::os::unix::fs::symlink(&target, &link)?;
        assert!(private_file(&link.join("state"), true).is_err());
        assert_eq!(
            fs::read_dir(&target)?.count(),
            0,
            "no state file may be created under the symlink target"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordination_concurrent_prepare_accepts_valid_peer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("shared");
        fs::create_dir(&dir)?;
        let nonce = fresh_nonce("concurrent");
        let first = spawn_helper("concurrent", &dir, &nonce, &[])?;
        let second = spawn_helper("concurrent", &dir, &nonce, &[])?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let ready_pids = wait_for("two distinct ready helper PIDs", deadline, || {
            let markers = read_markers(&dir, "ready-", &nonce)?;
            let pids: BTreeSet<u32> = markers
                .iter()
                .filter_map(|(_, content)| {
                    content
                        .lines()
                        .nth(1)
                        .and_then(|line| line.parse::<u32>().ok())
                })
                .collect();
            Ok((pids.len() >= 2).then_some(pids))
        })?;
        assert_eq!(ready_pids.len(), 2, "two distinct helper PIDs were ready");
        fs::write(dir.join("release"), nonce.as_str())?;
        let first_output = finish_helper(first, deadline)?;
        let second_output = finish_helper(second, deadline)?;
        require_helper_success(&first_output)?;
        require_helper_success(&second_output)?;

        let results = read_markers(&dir, "result-", &nonce)?;
        assert_eq!(results.len(), 2, "both helpers reported an observation");
        let left = observation(&results[0].1, &nonce)?;
        let right = observation(&results[1].1, &nonce)?;
        assert_eq!(left, right, "both helpers observed the same directory");
        assert_eq!(left.2, 0o700, "the shared root is private");
        eprintln!(
            "coordination concurrent helpers ready pids {:?}; observation dev={} ino={} mode={:o}",
            ready_pids, left.0, left.1, left.2
        );
        let root = dir.join("root");
        assert!(fs::symlink_metadata(&root)?.is_dir());
        assert_eq!(fs::read_dir(&root)?.count(), 0);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_competing_unsafe_leaf_is_preserved() -> Result<()> {
        // A competing 0755 directory is rejected and preserved.
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("coord");
        let leaf = root.file_name().context("leaf")?.to_os_string();
        let hook_root = root.clone();
        let _hook = unix::hooks::install(
            unix::hooks::PreparePoint::BeforeMkdir,
            &leaf,
            Box::new(move |_, _| {
                fs::create_dir(&hook_root)?;
                set_mode(&hook_root, 0o755)?;
                Ok(())
            }),
        );
        assert!(CoordinationPaths { root: root.clone() }
            .prepare_private()
            .is_err());
        drop(_hook);
        let inode = inode_of(&root)?;
        assert_eq!(mode_of(&root)?, 0o755);
        assert_eq!(inode_of(&root)?, inode);

        // A competing symlink is rejected and its target is unchanged.
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("competing-target");
        fs::create_dir(&target)?;
        set_mode(&target, 0o755)?;
        fs::write(target.join("sentinel"), b"target".to_vec())?;
        let target_inode = inode_of(&target)?;
        let root = temp.path().join("coord");
        let leaf = root.file_name().context("leaf")?.to_os_string();
        let hook_root = root.clone();
        let hook_target = target.clone();
        let _hook = unix::hooks::install(
            unix::hooks::PreparePoint::BeforeMkdir,
            &leaf,
            Box::new(move |_, _| {
                std::os::unix::fs::symlink(&hook_target, &hook_root)?;
                Ok(())
            }),
        );
        assert!(CoordinationPaths { root: root.clone() }
            .prepare_private()
            .is_err());
        drop(_hook);
        assert!(fs::symlink_metadata(&root)?.file_type().is_symlink());
        assert_eq!(inode_of(&target)?, target_inode);
        assert_eq!(mode_of(&target)?, 0o755);
        assert_eq!(fs::read(target.join("sentinel"))?, b"target".to_vec());
        assert_eq!(fs::read_dir(&target)?.count(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_competing_unsafe_intermediate_is_rejected() -> Result<()> {
        // The first missing component is an intermediate, not the leaf. A peer
        // winning that mkdir with a non-private directory must not be accepted
        // as the parent of a new root.
        let temp = tempfile::tempdir()?;
        let a = temp.path().join("a");
        fs::create_dir(&a)?;
        set_mode(&a, 0o700)?;
        let competing = a.join("b");
        let root = competing.join("coord");
        let created_inode = Rc::new(Cell::new((0u64, 0u64)));
        let hook_inode = Rc::clone(&created_inode);
        let hook_competing = competing.clone();
        let _hook = unix::hooks::install(
            unix::hooks::PreparePoint::BeforeMkdir,
            OsStr::new("b"),
            Box::new(move |_, _| {
                fs::create_dir(&hook_competing)?;
                set_mode(&hook_competing, 0o755)?;
                hook_inode.set(inode_of(&hook_competing)?);
                Ok(())
            }),
        );
        assert!(
            CoordinationPaths { root: root.clone() }
                .prepare_private()
                .is_err(),
            "a competing unsafe intermediate must be rejected"
        );
        drop(_hook);
        assert_eq!(
            inode_of(&competing)?,
            created_inode.get(),
            "the competing directory must be the same inode"
        );
        assert_eq!(mode_of(&competing)?, 0o755, "0755 must be preserved");
        assert!(!competing.join("coord").exists());
        assert!(!root.exists());
        assert_eq!(
            fs::read_dir(&competing)?.count(),
            0,
            "nothing may be created under the rejected intermediate"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_existing_root_requires_exact_0700() -> Result<()> {
        let temp = tempfile::tempdir()?;
        for mode in [0o400, 0o500, 0o755, 0o1700] {
            let root = temp.path().join(format!("root-{mode:o}"));
            fs::create_dir(&root)?;
            set_mode(&root, mode)?;
            let inode = inode_of(&root)?;
            assert!(
                CoordinationPaths { root: root.clone() }
                    .prepare_private()
                    .is_err(),
                "existing mode {mode:o} must be rejected"
            );
            assert_eq!(mode_of(&root)?, mode, "mode {mode:o} must be preserved");
            assert_eq!(inode_of(&root)?, inode);
            set_mode(&root, 0o700)?;
        }
        let root = temp.path().join("root-ok");
        fs::create_dir(&root)?;
        set_mode(&root, 0o700)?;
        let inode = inode_of(&root)?;
        CoordinationPaths { root: root.clone() }.prepare_private()?;
        assert_eq!(mode_of(&root)?, 0o700);
        assert_eq!(inode_of(&root)?, inode);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordination_fifo_rejection_preserves_metadata() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("coord");
        CoordinationPaths { root: root.clone() }.prepare_private()?;
        let fifo = root.join("state.fifo");
        let directory = rustix::fs::open(
            &root,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )?;
        rustix::fs::mkfifoat(
            &directory,
            "state.fifo",
            rustix::fs::Mode::from_raw_mode(0o644),
        )?;
        drop(directory);
        set_mode(&fifo, 0o644)?;
        let before = fs::symlink_metadata(&fifo)?;
        assert!(
            private_file(&fifo, false).is_err(),
            "a FIFO must be rejected"
        );
        let after = fs::symlink_metadata(&fifo)?;
        assert!(after.file_type().is_fifo());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.permissions().mode() & 0o7777, 0o644);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_file_reopen_preserves_mode_and_bytes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("coord");
        CoordinationPaths { root: root.clone() }.prepare_private()?;

        // An existing 0644 file is rejected without modification.
        let unsafe_file = root.join("unsafe");
        fs::write(&unsafe_file, b"sentinel-unsafe".to_vec())?;
        set_mode(&unsafe_file, 0o644)?;
        let inode = inode_of(&unsafe_file)?;
        assert!(private_file(&unsafe_file, false).is_err());
        assert_eq!(fs::read(&unsafe_file)?, b"sentinel-unsafe".to_vec());
        assert_eq!(mode_of(&unsafe_file)?, 0o644);
        assert_eq!(inode_of(&unsafe_file)?, inode);

        // An existing 0600 file opens without truncation or chmod.
        let existing = root.join("existing");
        fs::write(&existing, b"sentinel-existing".to_vec())?;
        set_mode(&existing, 0o600)?;
        {
            use std::io::Read;
            let mut file = private_file(&existing, false)?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            assert_eq!(text, "sentinel-existing");
        }
        assert_eq!(fs::read(&existing)?, b"sentinel-existing".to_vec());
        assert_eq!(mode_of(&existing)?, 0o600);
        assert!(
            private_file(&existing, true).is_err(),
            "create_new must never replace an existing file"
        );
        assert_eq!(fs::read(&existing)?, b"sentinel-existing".to_vec());

        // An absent leaf in open-or-create mode becomes a 0600 regular file.
        let created = root.join("created");
        drop(private_file(&created, true)?);
        assert!(fs::symlink_metadata(&created)?.file_type().is_file());
        assert_eq!(mode_of(&created)?, 0o600);

        // A symlink leaf is rejected and its target is unchanged.
        let target = temp.path().join("symlink-target");
        fs::write(&target, b"target-bytes".to_vec())?;
        set_mode(&target, 0o600)?;
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link)?;
        assert!(private_file(&link, false).is_err());
        assert!(private_file(&link, true).is_err());
        assert_eq!(fs::read(&target)?, b"target-bytes".to_vec());
        assert_eq!(mode_of(&target)?, 0o600);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_uid_mismatch_is_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("coord");
        CoordinationPaths { root: root.clone() }.prepare_private()?;
        let file_path = root.join("owner");
        drop(private_file(&file_path, true)?);

        let directory = rustix::fs::open(
            &root,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )?;
        let directory_stat = rustix::fs::fstat(&directory)?;
        let opened = fs::File::open(&file_path)?;
        let file_stat = rustix::fs::fstat(&opened)?;

        let actual = directory_stat.st_uid;
        unix::check_uid(actual, actual)?;
        assert!(unix::check_uid(actual, actual ^ 1).is_err());

        // A copied stat with only the UID changed is rejected by both the
        // pre-chmod directory validator and the descriptor-based file check.
        let mut foreign_directory = directory_stat;
        foreign_directory.st_uid = actual ^ 1;
        assert!(unix::check_created_directory(&foreign_directory).is_err());
        assert!(unix::check_directory(&foreign_directory, true).is_err());
        let mut foreign_file = file_stat;
        foreign_file.st_uid = actual ^ 1;
        assert!(unix::check_regular_owner(&foreign_file).is_err());

        // The regular-file check is descriptor based, not pathname based: a
        // directory descriptor with a matching owner is still rejected.
        assert!(unix::check_regular_owner(&directory_stat).is_err());

        // The unmodified fixtures still satisfy the production predicates.
        unix::check_created_directory(&directory_stat)?;
        unix::check_directory(&directory_stat, true)?;
        unix::check_regular_owner(&file_stat)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_private_file_has_cloexec() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("coord");
        CoordinationPaths { root: root.clone() }.prepare_private()?;

        let created = private_file(&root.join("created"), true)?;
        assert!(rustix::io::fcntl_getfd(&created)?.contains(rustix::io::FdFlags::CLOEXEC));
        drop(created);

        let existing_path = root.join("existing");
        drop(private_file(&existing_path, true)?);
        let existing = private_file(&existing_path, false)?;
        assert!(rustix::io::fcntl_getfd(&existing)?.contains(rustix::io::FdFlags::CLOEXEC));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordination_restrictive_umask_still_creates_private_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("fixture");
        fs::create_dir(&dir)?;
        let nonce = fresh_nonce("umask");
        let child = spawn_helper("restrictive-umask", &dir, &nonce, &[])?;
        let output = finish_helper(child, Instant::now() + Duration::from_secs(120))?;
        require_helper_success(&output)?;

        let results = read_markers(&dir, "result-", &nonce)?;
        assert_eq!(results.len(), 1);
        let mut lines = results[0].1.lines();
        assert_eq!(lines.next(), Some(nonce.as_str()));
        let line = lines.next().context("missing mode observation")?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "unexpected observation: {line}");
        assert_eq!(u32::from_str_radix(parts[0], 8)?, 0o700);
        assert_eq!(u32::from_str_radix(parts[1], 8)?, 0o700);
        assert_eq!(u32::from_str_radix(parts[2], 8)?, 0o600);
        assert_eq!(parts[3].parse::<u32>()?, unsafe { libc::geteuid() });
        let failed: Vec<_> = fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("failed-"))
            .collect();
        assert!(failed.is_empty(), "the helper reported a failure marker");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn coordination_data_dir_env_does_not_split_default_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("fixture");
        fs::create_dir(&dir)?;
        let home = dir.join("home");
        fs::create_dir(&home)?;
        let launch = dir.join("launch");
        fs::create_dir(&launch)?;
        let data_a = dir.join("data-a");
        let data_b = dir.join("data-b");
        fs::create_dir(&data_a)?;
        fs::create_dir(&data_b)?;
        let entries_before = (
            fs::read_dir(&data_a)?.count(),
            fs::read_dir(&data_b)?.count(),
        );

        let home_text = home.to_str().context("home path")?.to_string();
        let launch_text = launch.to_str().context("launch path")?.to_string();
        let data_a_text = data_a.to_str().context("data-a path")?.to_string();
        let data_b_text = data_b.to_str().context("data-b path")?.to_string();
        let nonce = fresh_nonce("data-dir");
        let deadline = Instant::now() + Duration::from_secs(120);

        let first = spawn_helper(
            "data-dir",
            &dir,
            &nonce,
            &[
                ("DEEPSEEK_CODE_DATA_DIR", data_a_text.as_str()),
                ("DCL_LC_HELPER_HOME", home_text.as_str()),
                ("DCL_LC_HELPER_LAUNCH", launch_text.as_str()),
            ],
        )?;
        let second = spawn_helper(
            "data-dir",
            &dir,
            &nonce,
            &[
                ("DEEPSEEK_CODE_DATA_DIR", data_b_text.as_str()),
                ("DCL_LC_HELPER_HOME", home_text.as_str()),
                ("DCL_LC_HELPER_LAUNCH", launch_text.as_str()),
            ],
        )?;
        let first_output = finish_helper(first, deadline)?;
        let second_output = finish_helper(second, deadline)?;
        require_helper_success(&first_output)?;
        require_helper_success(&second_output)?;

        let expected = canonical(&home)?.join(".deepseek-code/coordination/v1");
        let results = read_markers(&dir, "result-", &nonce)?;
        assert_eq!(results.len(), 2);
        let mut data_dirs = Vec::new();
        for (path, content) in &results {
            let mut lines = content.lines();
            assert_eq!(lines.next(), Some(nonce.as_str()));
            let root = lines.next().context("missing resolved root")?;
            let data = lines.next().context("missing data directory")?;
            assert_eq!(
                PathBuf::from(root),
                expected,
                "helper {} resolved a different root",
                path.display()
            );
            data_dirs.push(PathBuf::from(data));
        }
        data_dirs.sort();
        assert_eq!(data_dirs, vec![data_a.clone(), data_b.clone()]);
        for data in &data_dirs {
            assert!(!expected.starts_with(data));
        }
        assert!(!home.join(".deepseek-code").exists());
        assert_eq!(
            (
                fs::read_dir(&data_a)?.count(),
                fs::read_dir(&data_b)?.count()
            ),
            entries_before
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_non_directory_ancestor_is_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("ordinary-file");
        fs::write(&file, b"not a directory".to_vec())?;
        set_mode(&file, 0o644)?;
        let before = fs::read(&file)?;

        let target = file.join("missing").join("coord");
        assert!(CoordinationPaths::resolve(Some(&target), None, None, temp.path()).is_err());
        assert!(!file.join("missing").exists());

        assert!(CoordinationPaths {
            root: target.clone()
        }
        .prepare_private()
        .is_err());
        assert!(private_file(&file.join("missing").join("state"), true).is_err());

        assert_eq!(fs::read(&file)?, before);
        assert_eq!(mode_of(&file)?, 0o644);
        assert!(!file.join("missing").exists());
        Ok(())
    }
}
