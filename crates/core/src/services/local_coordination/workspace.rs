//! Workspace identity: lossless paths plus read-only Git metadata.
//!
//! Two paths that resolve to the same directory share an identity because the
//! canonical path is hashed together with the directory's device/inode, which
//! also stops a recycled path from impersonating the old one. Git is only ever
//! read: a failing or timed-out probe leaves the metadata absent rather than
//! inventing a repository.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::types::{
    valid_branch_ref, validate_workspace_fields, CoordResult, CoordinationError, NativePath,
};

/// Where a participant works, snapshotted at registration or on refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub workspace: NativePath,
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub branch_ref: Option<Vec<u8>>,
    pub observed_ms: i64,
}

impl WorkspaceIdentity {
    /// The same rules a stored row must satisfy.
    ///
    /// A refresh validates before it writes, so a value the writer accepts can
    /// never make a later snapshot decode fail for the whole registry.
    pub(super) fn validate(&self) -> CoordResult<()> {
        validate_workspace_fields(
            &self.workspace,
            self.repo_id.as_deref(),
            self.worktree_id.as_deref(),
            self.branch_ref.as_deref(),
            Some(self.observed_ms),
        )
    }
}

/// Version tags keep the two hashes in separate domains.
const DATA_TAG: &[u8] = b"dcl-data-v1";
const REPO_TAG: &[u8] = b"dcl-repo-v1";
const WORKTREE_TAG: &[u8] = b"dcl-worktree-v1";
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_OUTPUT_LIMIT: usize = 64 * 1024;

fn hash_fields(tag: &[u8], fields: &[Option<&[u8]>]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((tag.len() as u32).to_le_bytes());
    hasher.update(tag);
    for field in fields {
        match field {
            Some(bytes) => {
                hasher.update(1u8.to_le_bytes());
                hasher.update((bytes.len() as u32).to_le_bytes());
                hasher.update(bytes);
            }
            None => hasher.update(0u8.to_le_bytes()),
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Linux identity of a directory, so a recycled path is a different place.
fn native_identity(path: &Path) -> CoordResult<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)
            .map_err(|_| CoordinationError::WorkspaceUnavailable)?;
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&metadata.dev().to_le_bytes());
        bytes.extend_from_slice(&metadata.ino().to_le_bytes());
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CoordinationError::UnsupportedPlatform)
    }
}

fn canonical_bytes(path: &Path) -> CoordResult<Vec<u8>> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| CoordinationError::WorkspaceUnavailable)?;
    NativePath::from_path(&canonical).validate_native()?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(canonical.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(canonical.to_string_lossy().into_owned().into_bytes())
    }
}

/// The domain identity of one data directory.
///
/// Registration needs an existing directory; a read-only status view with no
/// directory simply does not compute a namespace. Fields are length-prefixed so
/// distinct inputs cannot collide by concatenation.
pub fn data_namespace(data_dir: &Path) -> CoordResult<String> {
    if !data_dir.is_dir() {
        return Err(CoordinationError::DataDirectoryUnavailable);
    }
    let path = canonical_bytes(data_dir).map_err(|_| CoordinationError::DataDirectoryUnavailable)?;
    let identity =
        native_identity(data_dir).map_err(|_| CoordinationError::DataDirectoryUnavailable)?;
    Ok(hash_fields(DATA_TAG, &[Some(&path), Some(&identity)]))
}

/// Render path bytes for humans: printable ASCII is kept, every other byte is
/// escaped, so control characters and invalid UTF-8 stay visible and lossless.
pub fn escaped_native(value: &NativePath) -> String {
    let mut rendered = String::with_capacity(value.bytes.len());
    for byte in &value.bytes {
        match byte {
            b'\\' => rendered.push_str("\\\\"),
            0x20..=0x7e => rendered.push(*byte as char),
            other => rendered.push_str(&format!("\\x{other:02x}")),
        }
    }
    rendered
}

/// Resolve a workspace path and its Git metadata off the async runtime.
pub async fn observe_workspace(cwd: &Path, now_ms: i64) -> CoordResult<WorkspaceIdentity> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || observe_workspace_blocking(&cwd, now_ms))
        .await
        .map_err(|_| CoordinationError::WorkspaceUnavailable)?
}

/// One bounded, read-only Git invocation. `None` covers every way the metadata
/// can be unavailable: a non-repository error, a timeout, or oversized output.
#[cfg(unix)]
fn run_git(cwd: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    run_git_command(&mut command, deadline)
}

/// Non-Unix builds have no verified process adapter for this probe.
#[cfg(not(unix))]
fn run_git(_cwd: &Path, _args: &[&str], _deadline: Instant) -> Option<Vec<u8>> {
    None
}

/// Start one prepared command and drain its stdout within the deadline.
///
/// The deadline is checked before spawning, so an expired probe never starts
/// another process.
#[cfg(unix)]
fn run_git_command(command: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take();
    // A missing stdout still goes through the single cleanup exit, so no path
    // after a successful spawn can leave the child running or unreaped.
    finish_git_child(&mut child, stdout, deadline)
}

/// The only production exit after a successful spawn: collect, then reap.
///
/// EOF on stdout does not mean the child has exited, and a child that has
/// exited may leave a writer behind. Both orderings are handled inside, and
/// every failure path reaps the child this probe owns.
#[cfg(unix)]
fn finish_git_child<R: std::io::Read + std::os::fd::AsFd>(
    child: &mut std::process::Child,
    stdout: Option<R>,
    deadline: Instant,
) -> Option<Vec<u8>> {
    // The closure drops the pipe before the child is reaped, so a reader that
    // is still open cannot delay cleanup.
    let collected = stdout.and_then(|mut pipe| collect_git_output(child, &mut pipe, deadline));
    if collected.is_none() {
        reap_owned(child);
    }
    collected
}

/// Drain piped output incrementally while enforcing the deadline and the cap.
///
/// EOF and exit are tracked independently: output is complete only once both
/// have happened, so a grandchild holding the pipe open is bounded by the
/// deadline rather than silently truncating a still-running child.
#[cfg(unix)]
fn collect_git_output(
    child: &mut std::process::Child,
    output: &mut (impl std::io::Read + std::os::fd::AsFd),
    deadline: Instant,
) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    // Nonblocking reads let the loop interleave the pipe with child reaping.
    let flags = rustix::fs::fcntl_getfl(&*output).ok()?;
    rustix::fs::fcntl_setfl(&*output, flags | rustix::fs::OFlags::NONBLOCK).ok()?;
    let mut eof = false;
    let mut exited_successfully = false;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if !exited_successfully {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return None;
                    }
                    exited_successfully = true;
                }
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        // Both observations are in: the probe is complete, provided the
        // deadline has not passed while they were being made.
        if eof && exited_successfully {
            return (Instant::now() < deadline).then_some(bytes);
        }
        if eof {
            // Only the exit status is missing; do not read stdout again.
            sleep_until_next_poll(deadline)?;
            continue;
        }
        let remaining = GIT_OUTPUT_LIMIT
            .saturating_sub(bytes.len())
            .saturating_add(1)
            .min(chunk.len());
        match output.read(&mut chunk[..remaining]) {
            Ok(0) => eof = true,
            Ok(read) => {
                if bytes.len() + read > GIT_OUTPUT_LIMIT {
                    return None;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // Nothing buffered yet: wait briefly, then recheck child and clock.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                sleep_until_next_poll(deadline)?;
            }
            Err(_) => return None,
        }
    }
}

/// Wait for the next poll, never past the deadline. `None` means no time left.
#[cfg(unix)]
fn sleep_until_next_poll(deadline: Instant) -> Option<()> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return None;
    }
    std::thread::sleep(left.min(Duration::from_millis(5)));
    Some(())
}

/// Kill and reap only the child this probe owns, never a numeric PID.
///
/// A status already collected by `try_wait` needs no signal: the child is
/// already gone and only has to be waited for.
#[cfg(unix)]
fn reap_owned(child: &mut std::process::Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// One output path per invocation. Exactly one command delimiter is removed;
/// interior and trailing native bytes are preserved, and the result is handed
/// to Git's caller as raw `OsStr` bytes on Unix.
fn git_path(cwd: &Path, args: &[&str], deadline: Instant) -> Option<PathBuf> {
    let mut bytes = run_git(cwd, args, deadline)?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

fn path_identity(path: &Path) -> Option<(Vec<u8>, Vec<u8>)> {
    let bytes = canonical_bytes(path).ok()?;
    let identity = native_identity(path).ok()?;
    Some((bytes, identity))
}

/// Workspace metadata without touching the async runtime; the registry's
/// blocking worker and `observe_workspace` both use it.
pub(super) fn observe_workspace_blocking(
    cwd: &Path,
    now_ms: i64,
) -> CoordResult<WorkspaceIdentity> {
    let canonical = std::fs::canonicalize(cwd)
        .map_err(|_| CoordinationError::WorkspaceUnavailable)?;
    let workspace = NativePath::from_path(&canonical);
    workspace.validate_native()?;
    let deadline = Instant::now() + GIT_TIMEOUT;

    let toplevel = git_path(
        cwd,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        deadline,
    );
    let common = git_path(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        deadline,
    );
    let admin = git_path(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
        deadline,
    );

    // A repository claim needs all three resolved paths; anything less is
    // "metadata unavailable", never a new repository identity.
    let (repo_id, worktree_id) = match (toplevel.as_deref(), common.as_deref(), admin.as_deref()) {
        (Some(toplevel), Some(common), Some(admin)) => {
            let repo = path_identity(common).map(|(path, identity)| {
                hash_fields(REPO_TAG, &[Some(&path), Some(&identity)])
            });
            let worktree = match (path_identity(toplevel), path_identity(admin)) {
                (Some((root, root_id)), Some((admin_path, admin_id))) => Some(hash_fields(
                    WORKTREE_TAG,
                    &[Some(&root), Some(&root_id), Some(&admin_path), Some(&admin_id)],
                )),
                _ => None,
            };
            (repo, worktree)
        }
        _ => (None, None),
    };

    let branch_ref = if repo_id.is_some() {
        run_git(cwd, &["symbolic-ref", "-q", "HEAD"], deadline).and_then(|mut bytes| {
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            // A detached HEAD prints nothing and exits non-zero, so no branch.
            // A reference the store could not persist is dropped, not stored.
            valid_branch_ref(&bytes).then_some(bytes)
        })
    } else {
        None
    };

    Ok(WorkspaceIdentity {
        workspace,
        repo_id,
        worktree_id,
        branch_ref,
        observed_ms: now_ms,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::process::{Command, Stdio};

    /// Git with native (possibly non-UTF-8) arguments.
    fn git_native(args: &[&std::ffi::OsStr]) -> bool {
        Command::new("git")
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn git(args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// Repository, linked worktree, clone, symlink alias and non-Git paths: the
    /// hashes must follow the directory identity, not the spelling of the path.
    #[tokio::test]
    async fn local_coordination_workspace_paths() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let repo = root.join("repo");
        assert!(git(&[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().unwrap()
        ]));
        assert!(git(&[
            "-C",
            repo.to_str().unwrap(),
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]));

        let main = observe_workspace(&repo, 1_000).await?;
        assert!(main.repo_id.is_some(), "a repository has a repo identity");
        assert!(main.worktree_id.is_some());
        assert_eq!(main.branch_ref.as_deref(), Some(b"refs/heads/main".as_slice()));

        let linked = root.join("linked");
        assert!(git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            linked.to_str().unwrap(),
        ]));
        let linked_identity = observe_workspace(&linked, 1_000).await?;
        assert_eq!(linked_identity.repo_id, main.repo_id);
        assert_ne!(linked_identity.worktree_id, main.worktree_id);
        assert_eq!(
            linked_identity.branch_ref.as_deref(),
            Some(b"refs/heads/wt".as_slice())
        );

        let clone = root.join("clone");
        assert!(git(&[
            "clone",
            "-q",
            repo.to_str().unwrap(),
            clone.to_str().unwrap()
        ]));
        let cloned = observe_workspace(&clone, 1_000).await?;
        assert_ne!(cloned.repo_id, main.repo_id, "a clone is another repository");

        let alias = root.join("alias");
        std::os::unix::fs::symlink(&repo, &alias).unwrap();
        let aliased = observe_workspace(&alias, 1_000).await?;
        assert_eq!(aliased.workspace, main.workspace, "a symlink shares the identity");
        assert_eq!(aliased.repo_id, main.repo_id);

        let plain = root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        let plain_identity = observe_workspace(&plain, 1_000).await?;
        assert!(plain_identity.repo_id.is_none());
        assert!(plain_identity.worktree_id.is_none());
        assert!(plain_identity.branch_ref.is_none());
        assert_eq!(
            plain_identity.workspace.to_path()?,
            std::fs::canonicalize(&plain).unwrap()
        );

        assert!(git(&[
            "-C",
            clone.to_str().unwrap(),
            "checkout",
            "-q",
            "--detach"
        ]));
        let detached = observe_workspace(&clone, 1_000).await?;
        assert!(detached.repo_id.is_some());
        assert!(detached.branch_ref.is_none(), "a detached HEAD has no branch");
        Ok(())
    }

    /// Non-UTF-8 and control bytes stay lossless for storage and readable for
    /// humans.
    #[test]
    fn local_coordination_workspace_escapes_native_bytes() -> CoordResult<()> {
        let raw = b"/tmp/a\x1bb\nc\x07\xff";
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(raw));
        let native = NativePath::from_path(&path);
        assert_eq!(native.to_path()?, path);
        assert_eq!(escaped_native(&native), "/tmp/a\\x1bb\\x0ac\\x07\\xff");
        assert!(!escaped_native(&native).contains('\n'));
        assert!(!escaped_native(&native).contains('\x1b'));
        Ok(())
    }

    /// The payload `eof:<code>` writes before closing its own stdout.
    const EOF_PAYLOAD: &[u8] = b"coord-eof-payload";

    /// A direct fixture child that is reaped on every path, including unwinding.
    ///
    /// The fallback is deliberately independent of the collector: it exists so
    /// a failed assertion cannot leave a running fixture behind.
    struct GitFixtureChild(std::process::Child);

    impl GitFixtureChild {
        fn new(child: std::process::Child) -> Self {
            Self(child)
        }

        fn child(&mut self) -> &mut std::process::Child {
            &mut self.0
        }
    }

    impl Drop for GitFixtureChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    /// The ignored fixture process for the pipe tests: `bytes:<n>` writes `n`
    /// known bytes and exits, `keeper` holds the inherited stdout until its
    /// stdin gate closes. It is infrastructure, never an acceptance test.
    #[test]
    #[ignore = "process helper: started only by the Git pipe fixtures"]
    fn lc_git_fixture_helper() {
        let mode = std::env::var("DCL_LC_GIT_MODE").expect("fixture mode");
        let fixture = std::path::PathBuf::from(
            std::env::var_os("DCL_LC_GIT_DIR").expect("fixture directory"),
        );
        let nonce = std::env::var("DCL_LC_GIT_NONCE").expect("fixture nonce");
        assert_eq!(
            std::fs::read_to_string(fixture.join("nonce")).expect("fixture nonce"),
            nonce
        );
        use std::io::Write;
        if let Some(count) = mode.strip_prefix("bytes:") {
            let count: usize = count.parse().expect("byte count");
            let mut stdout = std::io::stdout();
            stdout.write_all(&vec![b'Z'; count]).expect("stdout writes");
            stdout.flush().expect("stdout flushes");
            // Exit directly so libtest appends no trailer after the payload.
            std::process::exit(0);
        }
        if let Some(code) = mode.strip_prefix("eof:") {
            let code: i32 = code.parse().expect("exit code");
            // A fixed payload, then EOF from this process while it stays alive:
            // the ordering that used to discard valid output.
            let mut stdout = std::io::stdout();
            stdout.write_all(EOF_PAYLOAD).expect("stdout writes");
            stdout.flush().expect("stdout flushes");
            drop(stdout);
            // Close the descriptor itself, not just the Rust handle.
            unsafe {
                libc::close(libc::STDOUT_FILENO);
            }
            let mut gate = std::io::stdin();
            let mut byte = [0u8; 1];
            let _ = std::io::Read::read(&mut gate, &mut byte);
            std::process::exit(code);
        }
        if mode == "keeper" {
            let mut gate = std::io::stdin();
            let mut byte = [0u8; 1];
            let _ = std::io::Read::read(&mut gate, &mut byte);
            std::process::exit(0);
        }
        panic!("unknown fixture mode {mode}");
    }

    /// Spawn the ignored fixture with an isolated nonce directory.
    fn spawn_git_fixture(
        mode: &str,
        stdout: Stdio,
        stdin: Stdio,
    ) -> (GitFixtureChild, tempfile::TempDir) {
        let fixture = tempfile::tempdir().unwrap();
        let nonce = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
        std::fs::write(fixture.path().join("nonce"), &nonce).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("services::local_coordination::workspace::tests::lc_git_fixture_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("DCL_LC_GIT_MODE", mode)
            .env("DCL_LC_GIT_DIR", fixture.path())
            .env("DCL_LC_GIT_NONCE", nonce)
            .stdout(stdout)
            .stderr(Stdio::null())
            .stdin(stdin)
            .spawn()
            .expect("the fixture starts");
        (GitFixtureChild::new(child), fixture)
    }

    /// A fixture command with piped stdout, for the collector tests.
    fn git_fixture_command(mode: &str) -> (Command, tempfile::TempDir) {
        let fixture = tempfile::tempdir().unwrap();
        let nonce = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
        std::fs::write(fixture.path().join("nonce"), &nonce).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("services::local_coordination::workspace::tests::lc_git_fixture_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("DCL_LC_GIT_MODE", mode)
            .env("DCL_LC_GIT_DIR", fixture.path())
            .env("DCL_LC_GIT_NONCE", nonce)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        (command, fixture)
    }

    /// A child that fills a small pipe must be drained while it runs, not after
    /// it exits: waiting for the exit first deadlocks below the cap.
    #[test]
    #[cfg(target_os = "linux")]
    fn local_coordination_git_drains_pipe_backpressure() {
        use std::os::fd::AsRawFd;
        let (reader, writer) = std::io::pipe().unwrap();
        let capacity = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
        assert!(capacity >= 4096, "the pipe capacity is {capacity}");
        assert!(capacity < 32 * 1024, "the pipe must be smaller than the payload");

        let (mut child, _fixture) =
            spawn_git_fixture("bytes:32768", Stdio::from(writer), Stdio::null());
        let mut reader = reader;
        let collected = finish_git_child(
            child.child(),
            Some(&mut reader),
            Instant::now() + Duration::from_secs(20),
        )
        .expect("the payload must be captured");
        // libtest writes its own header first; the payload is the tail.
        assert!(collected.len() >= 32 * 1024, "the payload is complete");
        assert!(collected[collected.len() - 32 * 1024..]
            .iter()
            .all(|byte| *byte == b'Z'));
        assert_eq!(
            child.child().try_wait().expect("the probe was reaped").map(|s| s.success()),
            Some(true)
        );
    }

    /// The byte cap applies while reading: a control below it is captured and
    /// one byte over it fails.
    #[test]
    fn local_coordination_git_rejects_output_over_cap() {
        let (mut control, _fixture) = git_fixture_command("bytes:61440");
        let control = run_git_command(&mut control, Instant::now() + Duration::from_secs(20))
            .expect("the control payload is below the cap");
        assert!(control.len() >= 61_440);
        assert!(control[control.len() - 61_440..].iter().all(|byte| *byte == b'Z'));

        let (mut oversize, _fixture) = git_fixture_command("bytes:65537");
        assert!(
            run_git_command(&mut oversize, Instant::now() + Duration::from_secs(20)).is_none(),
            "one byte over the cap must fail"
        );
    }

    /// A descendant that keeps stdout open cannot extend the probe: the reader
    /// returns at the deadline while that child is still alive.
    #[test]
    fn local_coordination_git_deadline_covers_inherited_stdout() {
        let (reader, writer) = std::io::pipe().unwrap();
        // The keeper inherits a writer for the same pipe and waits on stdin.
        let (mut keeper, _keeper_fixture) = spawn_git_fixture(
            "keeper",
            Stdio::from(writer.try_clone().unwrap()),
            Stdio::piped(),
        );
        let (mut probe, _probe_fixture) =
            spawn_git_fixture("bytes:16", Stdio::from(writer), Stdio::null());
        // The probe is reaped before the collector runs, forcing the
        // exit-before-EOF ordering the review reproduced.
        let status = probe.child().wait().unwrap();
        assert!(status.success());

        let (sender, receiver) = std::sync::mpsc::channel();
        let collector = std::thread::spawn(move || {
            let mut reader = reader;
            let deadline = Instant::now() + Duration::from_millis(220);
            let _ = sender.send(collect_git_output(&mut probe.0, &mut reader, deadline));
        });
        let collected = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the collector must not wait for the keeper");
        // Record the observations before touching the keeper, then release the
        // gate so no path can leave it blocked.
        let keeper_alive = keeper.child().try_wait().unwrap().is_none();
        drop(keeper.child().stdin.take());
        collector.join().unwrap();
        keeper.child().wait().unwrap();
        assert!(collected.is_none(), "the open pipe must not extend the probe");
        assert!(
            keeper_alive,
            "the collector must never signal the keeper"
        );
    }

    /// Git's own output for a path may itself contain LF (interior or final) or
    /// invalid UTF-8. Only the one command delimiter is removed, so the native
    /// identity survives.
    #[tokio::test]
    async fn local_coordination_git_paths_preserve_lf_and_non_utf8() -> CoordResult<()> {
        use std::ffi::OsStr;
        let temp = tempfile::tempdir().unwrap();
        let names: [&[u8]; 3] = [b"repo\ninterior", b"repo-trail\n", b"repo-inv\xff"];
        let mut observed_paths = Vec::new();
        for name in names {
            let path = temp.path().join(OsStr::from_bytes(name));
            std::fs::create_dir(&path).unwrap();
            assert!(
                git_native(&[
                    OsStr::new("-C"),
                    path.as_os_str(),
                    OsStr::new("init"),
                    OsStr::new("-q"),
                    OsStr::new("-b"),
                    OsStr::new("main"),
                ]),
                "the repository fixture is created"
            );
            let observed = observe_workspace(&path, 1_000).await?;
            let canonical = std::fs::canonicalize(&path).unwrap();
            assert_eq!(observed.workspace.to_path()?, canonical);
            assert!(observed.repo_id.is_some(), "a repository has a repo id");
            assert!(observed.worktree_id.is_some());
            assert_eq!(
                observed.branch_ref.as_deref(),
                Some(b"refs/heads/main".as_slice())
            );
            observed_paths.push((path, observed));
        }

        // A symlink alias resolves to the same identity, never a new one.
        let (target, expected) = &observed_paths[1];
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(target, &alias).unwrap();
        let aliased = observe_workspace(&alias, 1_000).await?;
        assert_eq!(aliased.workspace, expected.workspace);
        assert_eq!(aliased.repo_id, expected.repo_id);
        assert_eq!(aliased.worktree_id, expected.worktree_id);
        Ok(())
    }

    /// A taken piped stdout that reports the first real EOF to the test.
    ///
    /// It forwards reads unchanged, so the production collector sees exactly
    /// what it would see without a hook.
    struct EofNotice<R> {
        inner: R,
        notice: Option<std::sync::mpsc::Sender<()>>,
    }

    impl<R: std::io::Read> std::io::Read for EofNotice<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let result = self.inner.read(buffer);
            if matches!(result, Ok(0)) {
                if let Some(notice) = self.notice.take() {
                    let _ = notice.send(());
                }
            }
            result
        }
    }

    impl<R: std::os::fd::AsFd> std::os::fd::AsFd for EofNotice<R> {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.inner.as_fd()
        }
    }

    /// `Ok` only when the direct child has already been reaped.
    ///
    /// `WNOWAIT` keeps any unexpected child waitable, so a leak is reported
    /// rather than silently consumed.
    #[cfg(target_os = "linux")]
    fn assert_reaped_without_waiting(child: &std::process::Child) -> std::io::Result<()> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        let error = std::io::Error::last_os_error();
        if rc == -1 && error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(());
        }
        Err(std::io::Error::other(format!(
            "the direct child was not reaped (rc {rc}, {error})"
        )))
    }

    /// What the collector thread observed, reported before any assertion.
    struct EofOutcome {
        collected: Option<Vec<u8>>,
        reaped: bool,
        status: Option<std::process::ExitStatus>,
    }

    /// One EOF-first fixture run; the parent owns the stdin gate.
    struct EofRun {
        gate: Option<std::process::ChildStdin>,
        eof: std::sync::mpsc::Receiver<()>,
        outcome: std::sync::mpsc::Receiver<EofOutcome>,
        handle: std::thread::JoinHandle<()>,
        _fixture: tempfile::TempDir,
    }

    /// Spawn `eof:<code>`, then collect on a worker thread with `deadline`.
    #[cfg(target_os = "linux")]
    fn run_eof_fixture(code: i32, deadline: Duration) -> EofRun {
        let (mut child, fixture) =
            spawn_git_fixture(&format!("eof:{code}"), Stdio::piped(), Stdio::piped());
        let gate = child.child().stdin.take();
        let reader = child.child().stdout.take().expect("piped stdout");
        let (notice, eof) = std::sync::mpsc::channel();
        let reader = EofNotice {
            inner: reader,
            notice: Some(notice),
        };
        let (sender, outcome) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut child = child;
            let collected = finish_git_child(
                child.child(),
                Some(reader),
                Instant::now() + deadline,
            );
            // Prove reaping before `wait`, which would otherwise hide the leak.
            // A child that still needs cleanup is left to the fixture guard:
            // an unconditional wait here would block on a held gate.
            let reaped = assert_reaped_without_waiting(child.child()).is_ok();
            let status = if reaped {
                child.child().wait().ok()
            } else {
                None
            };
            let _ = sender.send(EofOutcome {
                collected,
                reaped,
                status,
            });
        });
        EofRun {
            gate,
            eof,
            outcome,
            handle,
            _fixture: fixture,
        }
    }

    /// EOF from the probe's stdout is not its exit: the payload must survive
    /// and the owned direct child must be reaped before the probe completes.
    #[test]
    #[cfg(target_os = "linux")]
    fn local_coordination_git_eof_before_exit_returns_payload_after_reap() {
        let mut run = run_eof_fixture(0, Duration::from_secs(60));
        run.eof
            .recv_timeout(Duration::from_secs(30))
            .expect("the fixture must reach EOF");
        // While the gate is held the child is alive, so the probe is pending.
        let pending = run.outcome.recv_timeout(Duration::from_millis(300)).is_err();
        run.gate = None;
        let outcome = run
            .outcome
            .recv_timeout(Duration::from_secs(30))
            .expect("the probe finishes once the child exits");
        run.handle.join().unwrap();

        assert!(pending, "EOF alone must not complete the probe");
        let collected = outcome
            .collected
            .expect("a successful probe keeps the payload seen before EOF");
        assert!(
            collected.ends_with(EOF_PAYLOAD),
            "the fixture payload must be the captured tail"
        );
        assert!(outcome.reaped, "the direct child must already be reaped");
        assert_eq!(
            outcome.status.and_then(|status| status.code()),
            Some(0),
            "the cached status is the child's own exit"
        );
    }

    /// A nonzero exit after EOF is a failed probe, but the payload is still
    /// collected until then: killing on EOF would report SIGKILL instead of 7.
    #[test]
    #[cfg(target_os = "linux")]
    fn local_coordination_git_eof_before_nonzero_exit_returns_none_and_reaps() {
        let mut run = run_eof_fixture(7, Duration::from_secs(60));
        run.eof
            .recv_timeout(Duration::from_secs(30))
            .expect("the fixture must reach EOF");
        let pending = run.outcome.recv_timeout(Duration::from_millis(300)).is_err();
        run.gate = None;
        let outcome = run
            .outcome
            .recv_timeout(Duration::from_secs(30))
            .expect("the probe finishes once the child exits");
        run.handle.join().unwrap();

        assert!(pending, "EOF alone must not complete the probe");
        assert!(
            outcome.collected.is_none(),
            "an unsuccessful exit makes the metadata unavailable"
        );
        assert!(outcome.reaped, "the direct child must already be reaped");
        assert_eq!(
            outcome.status.and_then(|status| status.code()),
            Some(7),
            "cleanup must not kill a child that exited on its own"
        );
    }

    /// If EOF arrives but the child never exits, the deadline ends the probe and
    /// the owned child is killed and reaped before the caller sees the result.
    #[test]
    #[cfg(target_os = "linux")]
    fn local_coordination_git_eof_before_deadline_kills_and_reaps_owned_child() {
        use std::os::unix::process::ExitStatusExt;
        let mut run = run_eof_fixture(0, Duration::from_millis(250));
        run.eof
            .recv_timeout(Duration::from_secs(30))
            .expect("the fixture must reach EOF");
        // The gate stays held: only the deadline can end this probe.
        let outcome = run
            .outcome
            .recv_timeout(Duration::from_secs(30))
            .expect("the deadline must end the probe without the gate");
        run.gate = None;
        run.handle.join().unwrap();

        assert!(outcome.collected.is_none(), "an expired probe returns nothing");
        assert!(
            outcome.reaped,
            "cleanup must complete before the result is published"
        );
        assert_eq!(
            outcome.status.and_then(|status| status.signal()),
            Some(libc::SIGKILL),
            "the still-running owned child is killed"
        );
    }
}
