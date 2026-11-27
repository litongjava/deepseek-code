use super::{hash, invalid, Result};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// Construct only from host-authorized roots, never a model-supplied path/key.
/// Identity is immutable; a host workspace move must resolve and rebind again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryScope {
    key: String,
    identity: Vec<u8>,
}

impl MemoryScope {
    /// Resolve host workspace inputs. A default Home/empty workspace is private
    /// even when the shell happens to run inside a repository above Home.
    /// `home` and `private_owner` must come from the host, not model arguments.
    pub fn for_workspace(
        roots: &[PathBuf],
        trusted: bool,
        home: &Path,
        private_owner: &str,
    ) -> Result<Self> {
        if roots.is_empty()
            || (roots.len() == 1 && roots[0].canonicalize()? == home.canonicalize()?)
        {
            Self::private(private_owner)
        } else {
            Self::project(roots, trusted)
        }
    }

    /// `trusted` is the host's persisted project trust decision. Additional tool
    /// directories are deliberately not accepted implicitly as project roots.
    pub fn project(roots: &[PathBuf], trusted: bool) -> Result<Self> {
        if !trusted || roots.is_empty() {
            return Err(invalid("a nonempty trusted project is required"));
        }
        let mut identities = roots
            .iter()
            .map(|root| root_identity(root))
            .collect::<Result<Vec<_>>>()?;
        identities.sort();
        identities.dedup();
        let identity = if identities.len() == 1 {
            identities.remove(0)
        } else {
            let mut bytes = b"roots-v1\0".to_vec();
            for item in identities {
                append(&mut bytes, &item);
            }
            bytes
        };
        Ok(Self::from_identity(identity))
    }

    /// Pass the persistent private workspace owner (parent for owned children).
    pub fn private(owner: &str) -> Result<Self> {
        if owner.is_empty()
            || owner.len() > 128
            || !owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(invalid("invalid private workspace owner"));
        }
        let mut identity = b"private-v1\0".to_vec();
        append(&mut identity, owner.as_bytes());
        Ok(Self::from_identity(identity))
    }

    pub fn is_private(&self) -> bool {
        self.identity.starts_with(b"private-v1\0")
    }

    fn from_identity(identity: Vec<u8>) -> Self {
        Self {
            key: format!("v1-{}", hash(&identity)),
            identity,
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

fn append(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

fn root_identity(root: &Path) -> Result<Vec<u8>> {
    if !root.is_absolute() {
        return Err(invalid("project root must be absolute"));
    }
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(invalid("project root must be a directory"));
    }
    reject_network(&root)?;
    // An explicit .git boundary is also evidence that a failed Git query must
    // not silently turn a repository/worktree into an unrelated directory key.
    let boundary = root
        .ancestors()
        .any(|p| p.join(".git").symlink_metadata().is_ok())
        || (root.join("HEAD").is_file() && root.join("objects").is_dir());
    let path = if boundary {
        let mut command = Command::new("git");
        command
            .args(["--no-optional-locks", "-C"])
            .arg(&root)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            );
        // A parent CLI's Git overrides must not redirect identity to its repo.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            );
        let output = command.output()?;
        if !output.status.success() {
            return Err(invalid("Git common directory could not be resolved"));
        }
        let mut bytes = output.stdout;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let common = output_path(bytes)?;
        if !common.is_absolute() || !common.is_dir() {
            return Err(invalid("invalid Git common directory"));
        }
        common.canonicalize()?
    } else {
        root
    };
    reject_network(&path)?;
    let mut identity = if boundary {
        b"git-v1\0".to_vec()
    } else {
        b"dir-v1\0".to_vec()
    };
    append(&mut identity, &native_path(&path));
    Ok(identity)
}

pub(super) fn reject_network(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if text.starts_with(r"\\?\UNC\") || (text.starts_with(r"\\") && !text.starts_with(r"\\?\"))
        {
            return Err(invalid("network memory/project roots are unsupported"));
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn native_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(windows)]
fn native_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
#[cfg(not(any(unix, windows)))]
fn native_path(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn output_path(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_vec(bytes).into())
}
#[cfg(not(unix))]
fn output_path(bytes: Vec<u8>) -> Result<PathBuf> {
    Ok(String::from_utf8(bytes)
        .map_err(|_| invalid("Git returned a non-UTF-8 path"))?
        .into())
}
