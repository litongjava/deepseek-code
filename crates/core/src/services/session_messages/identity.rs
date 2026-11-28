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
            if !marker.is_file() {
                return None;
            }
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
}

pub(crate) struct PeerProject {
    pub(super) source: String,
    binding: ProjectBinding,
    // Linkage checked by Git during preparation. A retarget while waiting for
    // the writer invalidates the evidence without launching another process.
    git_keys: Vec<(PathBuf, Option<GitKey>)>,
}

impl PeerProject {
    pub(crate) fn confirm(&self, conn: &Connection, id: &str) -> Result<&str> {
        ensure!(
            self.binding == ProjectBinding::read(conn, id)?,
            "Message project changed during authorization"
        );
        ensure!(
            self.git_keys
                .iter()
                .all(|(path, key)| GitKey::read(path) == *key),
            "Git worktree identity changed during authorization"
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

    fn common_dir(&mut self, path: &Path, key: Option<&GitKey>) -> Result<PathBuf> {
        if let Some(key) = key {
            if self.verified.get(path) == Some(key) {
                return Ok(key.common.clone());
            }
        }
        self.verified.remove(path);
        let actual = (self.lookup)(path)?;
        if let Some(key) = key {
            ensure!(
                GitKey::read(path).as_ref() == Some(key),
                "Git worktree identity changed during lookup"
            );
            if key.common == actual {
                self.verified.insert(path.into(), key.clone());
            }
        }
        Ok(actual)
    }

    pub(crate) fn project(&mut self, conn: &Connection, id: &str) -> Result<PeerProject> {
        let binding = ProjectBinding::read(conn, id)?;
        project_repo::validate_trusted_folders(&binding.folders)?;
        ensure!(
            binding.path.is_absolute() && binding.path.is_dir(),
            "Project folder is unavailable"
        );
        let mut git_keys = Vec::new();
        if let Some(worktree) = &binding.worktree {
            worktree.validate(&binding.path, |cwd| {
                let key = GitKey::read(cwd);
                let common = self.common_dir(cwd, key.as_ref())?;
                git_keys.push((cwd.to_owned(), key));
                Ok(common)
            })?;
        }
        let source = binding.path.canonicalize()?.to_string_lossy().into_owned();
        Ok(PeerProject {
            source: if cfg!(windows) {
                source.to_lowercase()
            } else {
                source
            },
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
