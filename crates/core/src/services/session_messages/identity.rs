//! Resolve Git outside delivery transactions. The writer rechecks the live DB
//! binding and the cheap filesystem identity before accepting the snapshot.
use crate::{
    db::{conversation_repo, project_repo},
    native_engine::workspace,
    worktree::session,
};
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use std::{
    collections::VecDeque,
    fs,
    io::{Read, Seek},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

const CACHE_CAPACITY: usize = 64;
const GIT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const REDIRECTS: [&str; 3] = ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE"];

pub(crate) const GIT_OVERRIDE_REASON: &str =
    "Git directory override in effect: peer identity cannot be verified";

#[derive(Debug, thiserror::Error)]
#[error("{GIT_OVERRIDE_REASON}")]
pub(crate) struct GitDirectoryOverride;

fn reject_git_redirects() -> Result<()> {
    if REDIRECTS.iter().any(|key| std::env::var_os(key).is_some()) {
        return Err(GitDirectoryOverride.into());
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("Git worktree identity lookup timed out")]
pub(crate) struct GitLookupTimeout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitDirs {
    git: PathBuf,
    common: PathBuf,
}

impl GitDirs {
    pub(crate) fn lookup(cwd: &Path, deadline: Instant) -> Result<Self> {
        let mut command = session::git_command(
            cwd,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
                "--absolute-git-dir",
            ],
        );
        // A concurrent environment change cannot redirect the spawned command.
        for key in REDIRECTS {
            command.env_remove(key);
        }
        let output = git_output(&mut command, deadline)?;
        let mut lines = std::str::from_utf8(&output)?.lines();
        let common = Path::new(lines.next().context("Git returned no common directory")?);
        let git = Path::new(lines.next().context("Git returned no Git directory")?);
        ensure!(
            lines.next().is_none() && common.is_absolute() && git.is_absolute(),
            "Invalid Git directory response"
        );
        Ok(Self {
            git: git.canonicalize()?,
            common: common.canonicalize()?,
        })
    }
}

fn git_output(command: &mut Command, until: Instant) -> Result<Vec<u8>> {
    // A regular output file lets us wait on just the owned process. A wrapper
    // that leaves a descendant holding stdout cannot stall a pipe reader.
    let mut output = tempfile::tempfile()?;
    if Instant::now() >= until {
        return Err(GitLookupTimeout.into());
    }
    let mut child = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(output.try_clone()?)
        .spawn()?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                ensure!(
                    status.success(),
                    "Git could not resolve the worktree directories"
                );
                break;
            }
            Ok(None) if Instant::now() < until => std::thread::sleep(
                until
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(5)),
            ),
            result => {
                let _ = child.kill();
                child.wait()?;
                result?;
                return Err(GitLookupTimeout.into());
            }
        }
    }
    output.rewind()?;
    let mut bytes = Vec::new();
    output.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "Oversized Git directory response");
    Ok(bytes)
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct GitKey {
    marker: PathBuf,
    cwd_id: crate::fs::DirectoryId,
    root_id: crate::fs::DirectoryId,
    dirs: GitDirs,
    git_id: crate::fs::DirectoryId,
    common_id: crate::fs::DirectoryId,
    head: SystemTime,
    modified: SystemTime,
    config: Option<SystemTime>,
    worktree_config: Option<SystemTime>,
    standard_layout: bool,
    discovery_env: Vec<Option<std::ffi::OsString>>,
}

impl GitKey {
    fn read(cwd: &Path) -> Result<Self> {
        reject_git_redirects()?;
        let cwd = cwd.canonicalize()?;
        let root = cwd
            .ancestors()
            // A generated tree can itself live below the source's .git.
            // Losing its marker must not bind it to that outer repository.
            .take_while(|dir| dir.file_name() != Some(std::ffi::OsStr::new(".git")))
            .find(|dir| dir.join(".git").exists())
            .context("Missing worktree Git marker")?;
        let marker = root.join(".git");
        let git = if marker.is_dir() {
            marker.canonicalize()?
        } else {
            ensure!(marker.is_file(), "Invalid worktree Git marker");
            let link = fs::read_to_string(&marker)?;
            root.join(
                link.trim_end_matches(['\r', '\n'])
                    .strip_prefix("gitdir: ")
                    .context("Invalid gitdir link")?,
            )
            .canonicalize()?
        };
        let common = match fs::read_to_string(git.join("commondir")) {
            Ok(link) => git
                .join(link.trim_end_matches(['\r', '\n']))
                .canonicalize()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => git.clone(),
            Err(e) => return Err(e.into()),
        };
        let standard_layout = common.join("objects").is_dir() && common.join("refs").is_dir();
        fn modified(path: &Path) -> Result<Option<SystemTime>> {
            match fs::metadata(path) {
                Ok(meta) => Ok(Some(meta.modified()?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        Ok(Self {
            marker,
            cwd_id: crate::fs::directory_id(&cwd)?,
            root_id: crate::fs::directory_id(root)?,
            git_id: crate::fs::directory_id(&git)?,
            common_id: crate::fs::directory_id(&common)?,
            head: fs::metadata(git.join("HEAD"))?.modified()?,
            modified: fs::metadata(&common)?.modified()?,
            config: modified(&common.join("config"))?,
            worktree_config: modified(&git.join("config.worktree"))?,
            dirs: GitDirs { git, common },
            standard_layout,
            discovery_env: [
                "GIT_CEILING_DIRECTORIES",
                "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            ]
            .iter()
            .map(std::env::var_os)
            .collect(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ProjectBinding {
    id: String,
    path: PathBuf,
    folders: Vec<project_repo::ProjectFolder>,
    worktree: Option<session::SavedWorktree>,
}

impl ProjectBinding {
    // Only directory records are read here, including when the writer compares
    // them to the snapshot whose filesystem and Git identities were validated.
    fn read(conn: &Connection, id: &str) -> Result<Self> {
        super::valid_id(id)?;
        let row = conversation_repo::conversation_metadata(conn, id)?
            .context("Message session not found")?;
        ensure!(!row.archived, "Message session is archived");
        ensure!(
            row.workspace_mode == "code",
            "Messages require a Code session"
        );
        let project_id = row
            .project_id
            .context("Message session has no source project")?;
        workspace::private_workspace_owner(conn, id)?;
        let project =
            project_repo::get_project(conn, &project_id)?.context("Message project not found")?;
        ensure!(!project.is_archived, "Message project is archived");
        Ok(Self {
            path: project
                .workspace_path
                .context("Message project has no source directory")?
                .into(),
            folders: project.folders,
            worktree: session::SavedWorktree::read(conn, id, &project_id)?,
            id: project_id,
        })
    }

    fn validate_source(&self, lookup: impl FnMut(&Path) -> Result<PathBuf>) -> Result<String> {
        project_repo::validate_trusted_folders(&self.folders)?;
        workspace::validate_source(&self.path)?;
        if let Some(worktree) = &self.worktree {
            worktree.validate(&self.path, lookup)?;
        }
        let source = self.path.canonicalize()?.to_string_lossy().into_owned();
        Ok(if cfg!(windows) {
            source.to_lowercase()
        } else {
            source
        })
    }
}

pub(crate) struct PeerProject {
    pub(super) source: String,
    binding: ProjectBinding,
    // Linkage checked by Git during preparation. A retarget while waiting for
    // the writer invalidates the evidence without launching another process.
    git_keys: Vec<(PathBuf, GitKey)>,
}

impl PeerProject {
    pub(crate) fn confirm(&self, conn: &Connection, id: &str) -> Result<&str> {
        ensure!(
            self.binding == ProjectBinding::read(conn, id)?,
            "Message project changed during authorization"
        );
        let source = self.binding.validate_source(|cwd| {
            let (_, key) = self
                .git_keys
                .iter()
                .find(|(path, _)| path == cwd)
                .context("Missing verified Git identity")?;
            ensure!(
                GitKey::read(cwd)? == *key,
                "Git worktree identity changed during authorization"
            );
            Ok(key.dirs.common.clone())
        })?;
        ensure!(
            source == self.source,
            "Message project source changed during authorization"
        );
        Ok(&self.source)
    }
}

pub(crate) struct Resolver {
    lookup: Box<dyn Fn(&Path, Instant) -> Result<GitDirs> + Send + Sync>,
    verified: VecDeque<(PathBuf, GitKey)>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(GitDirs::lookup)
    }
}

impl Resolver {
    pub(crate) fn new(
        lookup: impl Fn(&Path, Instant) -> Result<GitDirs> + Send + Sync + 'static,
    ) -> Self {
        Self {
            lookup: Box::new(lookup),
            verified: VecDeque::new(),
        }
    }

    fn resolve_git(&mut self, path: &Path, prepare_until: Option<Instant>) -> Result<GitKey> {
        let before = GitKey::read(path);
        if let Some(index) = self.verified.iter().position(|(cwd, _)| cwd == path) {
            let (cwd, cached) = self.verified.remove(index).expect("existing cache entry");
            if before.as_ref().is_ok_and(|key| key == &cached) {
                self.verified.push_back((cwd, cached.clone()));
                return Ok(cached);
            }
        }
        if before
            .as_ref()
            .is_err_and(|e| e.is::<GitDirectoryOverride>())
        {
            return before;
        }
        let now = Instant::now();
        let call_deadline = now + GIT_LOOKUP_TIMEOUT;
        let deadline = prepare_until.map_or(call_deadline, |end| end.min(call_deadline));
        if now >= deadline {
            return Err(GitLookupTimeout.into());
        }
        let actual = (self.lookup)(path, deadline)?;
        let key = GitKey::read(path)?;
        ensure!(
            before? == key && actual == key.dirs,
            "Git worktree identity changed during lookup"
        );
        if key.standard_layout && key.discovery_env.iter().all(Option::is_none) {
            if self.verified.len() == CACHE_CAPACITY {
                self.verified.pop_front();
            }
            self.verified.push_back((path.into(), key.clone()));
        }
        Ok(key)
    }

    pub(crate) fn project(
        &mut self,
        conn: &Connection,
        id: &str,
        prepare_until: Option<Instant>,
    ) -> Result<PeerProject> {
        let binding = ProjectBinding::read(conn, id)?;
        let mut git_keys = Vec::new();
        let source = binding.validate_source(|cwd| {
            let key = self.resolve_git(cwd, prepare_until)?;
            let common = key.dirs.common.clone();
            git_keys.push((cwd.to_owned(), key));
            Ok(common)
        })?;
        Ok(PeerProject {
            source,
            binding,
            git_keys,
        })
    }

    pub(crate) fn pair(
        &mut self,
        conn: &Connection,
        from: &str,
        to: &str,
    ) -> Result<(PeerProject, PeerProject)> {
        Ok((
            self.project(conn, from, None)?,
            self.project(conn, to, None)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn repo(path: &Path) -> Result<GitDirs> {
        ensure!(Command::new("git")
            .args(["init", "-q"])
            .arg(path)
            .status()?
            .success());
        GitDirs::lookup(path, Instant::now() + GIT_LOOKUP_TIMEOUT)
    }

    #[test]
    fn successful_git_without_matching_filesystem_evidence_cannot_authorize() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let actual = repo(&temp.path().join("actual"))?;
        let other = temp.path().join("other");
        let no_marker = temp.path().join("no-marker");
        fs::create_dir(&no_marker)?;
        repo(&other)?;
        let calls = Arc::new(AtomicUsize::new(0));
        let lookups = calls.clone();
        let mut resolver = Resolver::new(move |_, _| {
            lookups.fetch_add(1, Ordering::Relaxed);
            Ok(actual.clone())
        });
        assert!(resolver.resolve_git(&no_marker, None).is_err());
        assert!(resolver.resolve_git(&other, None).is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(resolver.verified.is_empty());
        Ok(())
    }

    #[test]
    fn recreating_a_worktree_at_the_same_path_misses_the_cache() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dirs = repo(&temp.path().join("repo"))?;
        let tree = temp.path().join("tree");
        fs::create_dir(&tree)?;
        let link = format!("gitdir: {}\n", dirs.git.display());
        fs::write(tree.join(".git"), &link)?;
        let lookups = Arc::new(AtomicUsize::new(0));
        let calls = lookups.clone();
        let mut resolver = Resolver::new(move |cwd, deadline| {
            calls.fetch_add(1, Ordering::Relaxed);
            GitDirs::lookup(cwd, deadline)
        });
        let before = resolver.resolve_git(&tree, None)?;
        assert_eq!(before, resolver.resolve_git(&tree, None)?);
        fs::remove_file(tree.join(".git"))?;
        fs::remove_dir(&tree)?;
        fs::create_dir(&tree)?;
        fs::write(tree.join(".git"), &link)?;
        let after = resolver.resolve_git(&tree, None)?;
        assert_eq!(before.dirs, after.dirs);
        assert_ne!(before.root_id, after.root_id);
        assert_eq!(lookups.load(Ordering::Relaxed), 2);
        Ok(())
    }

    #[test]
    fn the_owner_cache_evicts_the_least_recently_used_path() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dirs = repo(temp.path())?;
        let mut paths = Vec::new();
        for i in 0..=CACHE_CAPACITY {
            let path = temp.path().join(format!("dir-{i}"));
            fs::create_dir(&path)?;
            paths.push(path);
        }
        let lookups = Arc::new(AtomicUsize::new(0));
        let calls = lookups.clone();
        let mut resolver = Resolver::new(move |_, _| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(dirs.clone())
        });
        for path in &paths[..CACHE_CAPACITY] {
            resolver.resolve_git(path, None)?;
        }
        resolver.resolve_git(&paths[0], None)?;
        resolver.resolve_git(&paths[CACHE_CAPACITY], None)?;
        resolver.resolve_git(&paths[0], None)?;
        assert_eq!(lookups.load(Ordering::Relaxed), CACHE_CAPACITY + 1);
        resolver.resolve_git(&paths[1], None)?;
        assert_eq!(lookups.load(Ordering::Relaxed), CACHE_CAPACITY + 2);
        assert_eq!(resolver.verified.len(), CACHE_CAPACITY);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_git_subprocess_returns_the_timeout_error() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let err = git_output(&mut cmd, started + Duration::from_millis(75)).unwrap_err();
        assert!(err.is::<GitLookupTimeout>(), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
