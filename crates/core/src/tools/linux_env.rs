//! Linux environment for platforms without a usable shell toolchain (Android).
//!
//! Shell commands run inside an Ubuntu root filesystem through PRoot, which
//! needs no root rights: the model gets `bash`, `apt`, `git`, `python` and the
//! rest of a normal distribution. The host app ships PRoot and points the
//! engine at it with environment variables; the root filesystem is downloaded
//! and unpacked into the app's data directory on first use.

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// Path of the PRoot executable (packaged by the app).
pub const PROOT_ENV: &str = "DEEPSEEK_CODE_PROOT";
/// Path of PRoot's loader executable, when shipped separately.
pub const LOADER_ENV: &str = "DEEPSEEK_CODE_PROOT_LOADER";
/// Directory holding the root filesystem.
pub const ROOTFS_ENV: &str = "DEEPSEEK_CODE_ROOTFS";
/// Written after a complete unpack; a partial download is never used.
const READY_MARKER: &str = ".deepseek-ready";

/// Ubuntu base images: small (~30 MB) and installable with `apt`. Hashes are
/// from the release's signed SHA256SUMS.
pub fn default_rootfs() -> Option<(&'static str, &'static str)> {
    match std::env::consts::ARCH {
        "x86_64" => Some((
            "https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.5-base-amd64.tar.gz",
            UBUNTU_AMD64_SHA256,
        )),
        "aarch64" => Some((
            "https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.5-base-arm64.tar.gz",
            UBUNTU_ARM64_SHA256,
        )),
        _ => None,
    }
}
const UBUNTU_AMD64_SHA256: &str =
    "e77b6f10c2590cef872b33ee9f635a0e3fd1f57fb074c0e52b5c7f56147a0c86";
const UBUNTU_ARM64_SHA256: &str =
    "a91d5a93010193712d346d761372b7c9db6dfcf093893161c64ca107f05914f2";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxEnvironment {
    pub proot: PathBuf,
    pub loader: Option<PathBuf>,
    pub rootfs: PathBuf,
}

impl LinuxEnvironment {
    /// The environment configured by the host app, whether or not it is installed.
    pub fn from_env() -> Option<Self> {
        let proot = PathBuf::from(std::env::var_os(PROOT_ENV)?);
        let rootfs = PathBuf::from(std::env::var_os(ROOTFS_ENV)?);
        let loader = std::env::var_os(LOADER_ENV).map(PathBuf::from);
        Some(Self {
            proot,
            loader,
            rootfs,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.proot.is_file()
            && self.rootfs.join(READY_MARKER).is_file()
            && self.rootfs.join("bin/bash").exists()
    }

    /// A command that runs `/bin/bash` inside the root filesystem. The caller
    /// appends the shell arguments (`-c <script>`) and sets the working
    /// directory, which must be `cwd` or below it.
    ///
    /// Host directories the engine works in (the data directory, the
    /// workspace, shared storage) are bound at the same paths, so file tools
    /// and shell commands see the same files.
    pub fn bash_command(&self, cwd: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.proot);
        command.args(self.proot_args(cwd));
        command.arg("/bin/bash");
        if let Some(loader) = &self.loader {
            command.env("PROOT_LOADER", loader);
        }
        // PRoot links against libraries shipped next to it.
        if let Some(dir) = self.proot.parent() {
            command.env("LD_LIBRARY_PATH", dir);
        }
        let tmp = self.rootfs.join("tmp");
        command
            .env("PROOT_TMP_DIR", &tmp)
            .env("HOME", "/root")
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LANG", "C.UTF-8")
            .env("TERM", "xterm-256color")
            .env_remove("LD_PRELOAD");
        command
    }

    /// Uses the same environment and bind mounts for an interactive terminal.
    pub fn pty_command(&self, cwd: &str) -> portable_pty::CommandBuilder {
        let bash = self.bash_command(cwd);
        let source = bash.as_std();
        let mut command = portable_pty::CommandBuilder::new(source.get_program());
        command.args(source.get_args());
        command.arg("--login");
        for (name, value) in source.get_envs() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            }
        }
        command.cwd(cwd);
        command
    }

    fn proot_args(&self, cwd: &str) -> Vec<String> {
        let mut args = vec![
            // Fake root for apt; hard links become symlinks (Android forbids link()).
            "-0".to_string(),
            "--link2symlink".to_string(),
            "--kill-on-exit".to_string(),
            "-r".to_string(),
            self.rootfs.to_string_lossy().into_owned(),
        ];
        let mut binds: Vec<String> = ["/dev", "/proc", "/sys", "/storage", "/sdcard"]
            .into_iter()
            .filter(|path| Path::new(path).exists())
            .map(String::from)
            .collect();
        for var in ["DEEPSEEK_CODE_DATA_DIR", "HOME"] {
            if let Some(dir) = std::env::var_os(var).map(PathBuf::from) {
                if dir.is_dir() && !dir.starts_with(&self.rootfs) {
                    binds.push(dir.to_string_lossy().into_owned());
                }
            }
        }
        if Path::new(cwd).is_dir() && !Path::new(cwd).starts_with(&self.rootfs) {
            binds.push(cwd.to_string());
        }
        binds.dedup();
        for bind in binds {
            args.push("-b".into());
            args.push(bind);
        }
        // No -w: PRoot starts in the host's current directory, which is bound
        // at the same path, so the Bash tool's saved directory keeps working.
        args
    }
}

/// The installed environment, if the host app configured one.
pub fn active() -> Option<LinuxEnvironment> {
    LinuxEnvironment::from_env().filter(LinuxEnvironment::is_ready)
}

/// Downloads, verifies and unpacks the root filesystem into `rootfs`.
pub async fn install(rootfs: &Path, url: &str, sha256: &str) -> Result<()> {
    if rootfs.join(READY_MARKER).is_file() {
        return Ok(());
    }
    let parent = rootfs
        .parent()
        .context("Root filesystem path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let archive = parent.join("rootfs.tar.gz.part");
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    let digest = hex_digest(&bytes);
    ensure!(
        digest.eq_ignore_ascii_case(sha256),
        "Root filesystem checksum mismatch: {digest}"
    );
    std::fs::write(&archive, &bytes)?;
    let target = rootfs.to_path_buf();
    let source = archive.clone();
    tokio::task::spawn_blocking(move || unpack(&source, &target)).await??;
    std::fs::remove_file(&archive).ok();
    configure(rootfs)?;
    std::fs::write(rootfs.join(READY_MARKER), sha256)?;
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Unpacks a gzip tarball. Hard links are copied and device nodes skipped,
/// because unprivileged Android apps can create neither.
pub fn unpack(archive: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        std::fs::remove_dir_all(target)?;
    }
    std::fs::create_dir_all(target)?;
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut hard_links = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = safe_relative(&entry.path()?)?;
        let dest = target.join(&path);
        let kind = entry.header().entry_type();
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if kind.is_hard_link() {
            let link = entry.link_name()?.context("Hard link without a target")?;
            hard_links.push((dest, target.join(safe_relative(&link)?)));
            continue;
        }
        if kind.is_block_special() || kind.is_character_special() || kind.is_fifo() {
            continue;
        }
        if kind.is_symlink() {
            let link = entry.link_name()?.context("Symlink without a target")?;
            let _ = std::fs::remove_file(&dest);
            #[cfg(unix)]
            std::os::unix::fs::symlink(link.as_ref(), &dest)?;
            continue;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("Cannot unpack {}", path.display()))?;
    }
    for (dest, source) in hard_links {
        std::fs::copy(&source, &dest)
            .with_context(|| format!("Cannot copy {}", source.display()))?;
    }
    Ok(())
}

/// Rejects absolute paths and `..` so an archive cannot write outside the target.
fn safe_relative(path: &Path) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                bail!("Unsafe path in archive: {}", path.display())
            }
        }
    }
    Ok(clean)
}

/// Settings a chroot-style Ubuntu needs to work without root privileges.
fn configure(rootfs: &Path) -> Result<()> {
    std::fs::create_dir_all(rootfs.join("tmp"))?;
    std::fs::create_dir_all(rootfs.join("root"))?;
    std::fs::create_dir_all(rootfs.join("etc"))?;
    // Ubuntu's resolv.conf may be a dangling symlink to systemd-resolved.
    let _ = std::fs::remove_file(rootfs.join("etc/resolv.conf"));
    std::fs::write(
        rootfs.join("etc/resolv.conf"),
        "nameserver 8.8.8.8\nnameserver 1.1.1.1\n",
    )?;
    std::fs::write(
        rootfs.join("etc/hosts"),
        "127.0.0.1 localhost\n::1 localhost\n",
    )?;
    let apt = rootfs.join("etc/apt/apt.conf.d");
    std::fs::create_dir_all(&apt)?;
    // apt drops privileges to _apt, which PRoot's fake root cannot do.
    std::fs::write(apt.join("99deepseek"), "APT::Sandbox::User \"root\";\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(dir: &Path) -> PathBuf {
        let path = dir.join("rootfs.tar.gz");
        let file = std::fs::File::create(&path).unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::fast(),
        ));
        let mut add = |name: &str, kind: tar::EntryType, data: &[u8], link: Option<&str>| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            builder.append_data(&mut header, name, data).unwrap();
        };
        add("bin/bash", tar::EntryType::Regular, b"#!fake", None);
        add("usr/bin/perl", tar::EntryType::Regular, b"perl", None);
        add(
            "usr/bin/perl5",
            tar::EntryType::Link,
            b"",
            Some("usr/bin/perl"),
        );
        add("bin/sh", tar::EntryType::Symlink, b"", Some("bash"));
        add("dev/null", tar::EntryType::Char, b"", None);
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn unpack_copies_hard_links_keeps_symlinks_and_skips_devices() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        unpack(&archive(dir.path()), &rootfs).unwrap();
        assert_eq!(
            std::fs::read(rootfs.join("usr/bin/perl5")).unwrap(),
            b"perl"
        );
        assert_eq!(
            std::fs::read_link(rootfs.join("bin/sh")).unwrap(),
            Path::new("bash")
        );
        assert!(!rootfs.join("dev/null").exists());
        configure(&rootfs).unwrap();
        assert!(
            std::fs::read_to_string(rootfs.join("etc/apt/apt.conf.d/99deepseek"))
                .unwrap()
                .contains("Sandbox::User")
        );
    }

    #[test]
    fn archive_paths_cannot_escape_the_target() {
        assert!(safe_relative(Path::new("../etc/passwd")).is_err());
        assert_eq!(
            safe_relative(Path::new("/usr/./bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
    }

    #[test]
    fn environment_is_used_only_after_a_complete_install() {
        let dir = tempfile::tempdir().unwrap();
        let proot = dir.path().join("libproot.so");
        std::fs::write(&proot, b"").unwrap();
        let env = LinuxEnvironment {
            proot,
            loader: None,
            rootfs: dir.path().join("rootfs"),
        };
        assert!(!env.is_ready());
        std::fs::create_dir_all(env.rootfs.join("bin")).unwrap();
        std::fs::write(env.rootfs.join("bin/bash"), b"").unwrap();
        assert!(!env.is_ready(), "a partial unpack has no marker");
        std::fs::write(env.rootfs.join(READY_MARKER), b"sha").unwrap();
        assert!(env.is_ready());
        let args = env.proot_args(dir.path().to_str().unwrap());
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-r" && w[1] == env.rootfs.to_string_lossy()));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-b" && w[1] == dir.path().to_string_lossy()));
        assert!(args.contains(&"--link2symlink".to_string()));
    }

    #[test]
    fn interactive_shell_keeps_ubuntu_environment_and_workspace_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let env = LinuxEnvironment {
            proot: dir.path().join("libproot.so"),
            loader: Some(dir.path().join("libproot-loader.so")),
            rootfs: dir.path().join("rootfs"),
        };
        let cwd = dir.path().to_str().unwrap();
        let command = env.pty_command(cwd);
        let args = command.get_argv();
        assert_eq!(args[0], env.proot.as_os_str());
        assert!(args.windows(2).any(|w| w[0] == "-b" && w[1] == cwd));
        assert_eq!(args[args.len() - 2], "/bin/bash");
        assert_eq!(args[args.len() - 1], "--login");
        assert_eq!(command.get_env("HOME"), Some(std::ffi::OsStr::new("/root")));
        assert_eq!(
            command.get_env("PROOT_LOADER"),
            env.loader.as_deref().map(Path::as_os_str)
        );
        assert_eq!(
            command.get_cwd().map(|p| p.as_os_str()),
            Some(std::ffi::OsStr::new(cwd))
        );
    }
}
