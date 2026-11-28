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

#[derive(Debug, thiserror::Error)]
#[error("Git worktree identity lookup timed out")]
pub(crate) struct GitLookupTimeout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitDirs {
    git: PathBuf,
    common: PathBuf,
}

impl GitDirs {
    pub(crate) fn lookup(cwd: &Path) -> Result<Self> {
        let mut command = session::git_command(
            cwd,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
                "--absolute-git-dir",
            ],
        );
        let output = git_output(&mut command, Duration::from_secs(2))?;
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

fn git_output(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    // A regular output file lets us wait on just the owned process. A wrapper
    // that leaves a descendant holding stdout cannot stall a pipe reader.
    let mut output = tempfile::tempfile()?;
    let until = Instant::now() + timeout;
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
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
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
    cwd_id: (u64, u64),
    root_id: (u64, u64),
    dirs: GitDirs,
    git_id: (u64, u64),
    common_id: (u64, u64),
    head: SystemTime,
    modified: SystemTime,
    config: Option<SystemTime>,
    worktree_config: Option<SystemTime>,
    standard_layout: bool,
    discovery_env: Vec<Option<std::ffi::OsString>>,
}

impl GitKey {
    fn read(cwd: &Path) -> Result<Self> {
        let cwd = cwd.canonicalize()?;
        let root = cwd
            .ancestors()
            .take_while(|dir| {
                !(dir.join("HEAD").is_file()
                    && (dir.join("objects").is_dir() || dir.join("commondir").is_file()))
            })
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
                "GIT_DIR",
                "GIT_COMMON_DIR",
                "GIT_WORK_TREE",
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
        ensure!(
            self.path.is_absolute() && self.path.is_dir(),
            "Project folder is unavailable"
        );
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
    lookup: Box<dyn Fn(&Path) -> Result<GitDirs> + Send + Sync>,
    verified: VecDeque<(PathBuf, GitKey)>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(GitDirs::lookup)
    }
}

impl Resolver {
    pub(crate) fn new(lookup: impl Fn(&Path) -> Result<GitDirs> + Send + Sync + 'static) -> Self {
        Self {
            lookup: Box::new(lookup),
            verified: VecDeque::new(),
        }
    }

    fn resolve_git(&mut self, path: &Path) -> Result<GitKey> {
        let before = GitKey::read(path);
        if let Some(index) = self.verified.iter().position(|(cwd, _)| cwd == path) {
            let (cwd, cached) = self.verified.remove(index).expect("existing cache entry");
            if before.as_ref().is_ok_and(|key| key == &cached) {
                self.verified.push_back((cwd, cached.clone()));
                return Ok(cached);
            }
        }
        let actual = (self.lookup)(path)?;
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

    pub(crate) fn project(&mut self, conn: &Connection, id: &str) -> Result<PeerProject> {
        let binding = ProjectBinding::read(conn, id)?;
        let mut git_keys = Vec::new();
        let source = binding.validate_source(|cwd| {
            let key = self.resolve_git(cwd)?;
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
        Ok((self.project(conn, from)?, self.project(conn, to)?))
    }
}
