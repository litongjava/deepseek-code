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
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, PartialEq, Eq, Clone)]
struct GitKey {
    marker: PathBuf,
    git_dir: PathBuf,
    head: SystemTime,
    common: PathBuf,
    modified: SystemTime,
}

impl GitKey {
    // Unsupported layouts still go through Git, but never populate the cache.
    fn read(cwd: &Path) -> Option<Self> {
        if [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        ]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
        {
            return None;
        }
        let cwd = cwd.canonicalize().ok()?;
        let marker = cwd
            .ancestors()
            .map(|dir| dir.join(".git"))
            .find(|path| path.exists())?;
        let git_dir = if marker.is_dir() {
            marker.canonicalize().ok()?
        } else {
            let link = fs::read_to_string(&marker).ok()?;
            marker
                .parent()?
                .join(link.trim_end().strip_prefix("gitdir: ")?)
                .canonicalize()
                .ok()?
        };
        let common = match fs::read_to_string(git_dir.join("commondir")) {
            Ok(link) => git_dir.join(link.trim_end()).canonicalize().ok()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
            Err(_) => return None,
        };
        if !common.join("objects").is_dir() || !common.join("refs").is_dir() {
            return None;
        }
        Some(Self {
            marker,
            head: fs::metadata(git_dir.join("HEAD")).ok()?.modified().ok()?,
            git_dir,
            modified: fs::metadata(&common).ok()?.modified().ok()?,
            common,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct GitPath {
    path: PathBuf,
    key: Option<GitKey>,
}

impl GitPath {
    fn new(path: &Path) -> Self {
        Self {
            path: path.into(),
            key: GitKey::read(path),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PeerProject {
    pub(super) source: String,
    project_id: String,
    git: Option<(GitPath, GitPath)>,
}

impl PeerProject {
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
        let project = project_repo::require_trusted(conn, &project_id)?;
        ensure!(!project.is_archived, "Message project is archived");
        let path = Path::new(
            project
                .workspace_path
                .as_deref()
                .context("Message project has no source directory")?,
        );
        ensure!(
            path.is_absolute() && path.is_dir(),
            "Project folder is unavailable"
        );
        let mut git = None;
        session::resolve_with(conn, id, &project_id, path, |worktree, repo| {
            git = Some((GitPath::new(worktree), GitPath::new(repo)));
            Ok(())
        })?;
        let source = path.canonicalize()?.to_string_lossy().into_owned();
        Ok(Self {
            source: if cfg!(windows) {
                source.to_lowercase()
            } else {
                source
            },
            project_id,
            git,
        })
    }

    pub(crate) fn confirm(&self, conn: &Connection, id: &str) -> Result<&str> {
        ensure!(
            *self == Self::read(conn, id)?,
            "Message project changed during authorization"
        );
        Ok(&self.source)
    }
}

pub(crate) struct Resolver {
    lookup: Box<dyn Fn(&Path) -> Result<PathBuf> + Send + Sync>,
    verified: HashMap<PathBuf, GitKey>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(session::common_dir)
    }
}

impl Resolver {
    pub(crate) fn new(lookup: impl Fn(&Path) -> Result<PathBuf> + Send + Sync + 'static) -> Self {
        Self {
            lookup: Box::new(lookup),
            verified: HashMap::new(),
        }
    }

    fn common_dir(&mut self, path: &GitPath) -> Result<PathBuf> {
        if let Some(key) = &path.key {
            if self.verified.get(&path.path) == Some(key) {
                return Ok(key.common.clone());
            }
        }
        self.verified.remove(&path.path);
        let actual = (self.lookup)(&path.path)?;
        if let Some(key) = &path.key {
            if key.common == actual && GitKey::read(&path.path).as_ref() == Some(key) {
                self.verified.insert(path.path.clone(), key.clone());
            }
        }
        Ok(actual)
    }

    pub(crate) fn project(&mut self, conn: &Connection, id: &str) -> Result<PeerProject> {
        let project = PeerProject::read(conn, id)?;
        if let Some((worktree, repo)) = &project.git {
            ensure!(
                self.common_dir(worktree)? == self.common_dir(repo)?,
                "Conversation worktree no longer belongs to this repository"
            );
        }
        Ok(project)
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
