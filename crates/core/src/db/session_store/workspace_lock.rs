//! Workspace admission shares filesystem locks with Super Agent cleanup across processes.
use super::{SessionDb, Stores};
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

fn path_key(canonical: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(canonical.as_os_str().as_encoded_bytes())
    )
}

fn shared_lock(file: &File) -> Result<()> {
    #[cfg(target_os = "android")]
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockShared)?;
    #[cfg(not(target_os = "android"))]
    file.try_lock_shared()?;
    Ok(())
}

impl Stores {
    fn workspace_lock_file(&self, key: &str) -> Result<File> {
        let locks = self.layout().lock_path("workspace");
        fs::create_dir_all(&locks)?;
        Ok(File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(locks.join(key))?)
    }

    // The locked file is immutable once published. Reading its revision and
    // probing its lock always refers to the same execution, even across a rename.
    fn lease_slot(&self, incarnation: &str) -> Result<std::path::PathBuf> {
        ensure!(
            super::layout::canonical_uuid(incarnation),
            "Invalid workspace lease identity"
        );
        Ok(self
            .layout()
            .lock_path("workspace")
            .join(format!("lease-{incarnation}")))
    }

    pub(crate) fn hold_worktree_removal(&self, path: &Path) -> Result<File> {
        let parent = path
            .parent()
            .context("Worktree has no parent")?
            .canonicalize()?;
        let key = path_key(&parent.join(path.file_name().context("Worktree has no name")?));
        let fence = self.workspace_lock_file(&key)?;
        crate::fs::try_lock_file(&fence).context("Worktree has active workspace users")?;
        // The exclusive path fence now excludes new attachments through commit.
        // A stale membership is harmless after owner exit, even after a crash.
        self.global().read(|c| {
            let mut q = c.prepare(
                "SELECT incarnation, revision FROM execution_workspace_paths WHERE path_key=?1",
            )?;
            for row in q.query_map([key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (incarnation, added) = row?;
                let mut slot = match File::options()
                    .read(true)
                    .write(true)
                    .open(self.lease_slot(&incarnation)?)
                {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                match crate::fs::try_lock_file(&slot) {
                    Ok(()) => continue,
                    Err(fs::TryLockError::WouldBlock) => {}
                    Err(fs::TryLockError::Error(e)) => return Err(e.into()),
                }
                let mut bytes = [0; 8];
                slot.read_exact(&mut bytes)?;
                ensure!(
                    added <= i64::from_le_bytes(bytes),
                    "Worktree has active workspace users"
                );
            }
            Ok(())
        })?;
        Ok(fence)
    }

    pub(super) fn admit_workspace_path(&self, path: &Path) -> Result<Vec<File>> {
        let mut locks = BTreeMap::new();
        self.include_workspace_path(path, &mut locks, |_| Ok(()))?;
        Ok(locks.into_values().collect())
    }

    pub(super) fn include_workspace_path(
        &self,
        path: &Path,
        guards: &mut BTreeMap<String, File>,
        mut attach: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let canonical = path.canonicalize()?;
        for ancestor in canonical.ancestors() {
            let key = path_key(ancestor);
            if !guards.contains_key(&key) {
                let file = self.workspace_lock_file(&key)?;
                shared_lock(&file).context("Worktree removal in progress")?;
                guards.insert(key.clone(), file);
            }
            attach(&key)?;
        }
        ensure!(
            path.canonicalize()? == canonical && canonical.is_dir(),
            "Workspace changed during admission"
        );
        Ok(())
    }

    pub(super) fn include_session_workspace(
        &self,
        c: &Connection,
        id: &str,
        guards: &mut BTreeMap<String, File>,
        change: Option<(&str, i64)>,
    ) -> Result<()> {
        let (project, recorded): (Option<String>, Option<String>) = c.query_row(
            "SELECT coalesce(w.path,p.workspace_path),c.workspace_path FROM conversations c
             LEFT JOIN projects p ON p.id=c.project_id
             LEFT JOIN conversation_worktrees w ON w.conversation_id=c.id AND w.project_id=p.id
             WHERE c.id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut include = |path: &Path| {
            self.include_workspace_path(path, guards, |key| {
                if let Some((incarnation, revision)) = change {
                    c.execute(
                        "INSERT INTO execution_workspace_paths(incarnation,path_key,revision) VALUES(?1,?2,?3)
                         ON CONFLICT(incarnation,path_key) DO UPDATE SET revision=excluded.revision",
                        rusqlite::params![incarnation, key, revision],
                    )?;
                }
                Ok(())
            })
        };
        if let Some(path) = &project {
            include(Path::new(path))?;
        }
        let mut extra = c.prepare(
            "SELECT f.path FROM conversations c JOIN project_folders f
            ON f.project_id=c.project_id WHERE c.id=?1 AND f.sort_order>0",
        )?;
        for path in extra.query_map([id], |r| r.get::<_, String>(0))? {
            include(Path::new(&path?))?;
        }
        // A synced header can carry a path from another machine.
        if let Some(path) = recorded.filter(|p| Some(p) != project.as_ref()) {
            let path = Path::new(&path);
            if path.is_absolute() && path.try_exists()? {
                include(path)?;
            }
        }
        Ok(())
    }

    pub(super) fn admit_session_workspace(&self, c: &Connection, id: &str) -> Result<Vec<File>> {
        let mut guards = BTreeMap::new();
        self.include_session_workspace(c, id, &mut guards, None)?;
        Ok(guards.into_values().collect())
    }

    // Record the final paths even if the editor began before execution admission.
    // The G commit orders each attachment against the lease's initial snapshot.
    pub(super) fn admit_workspace_change(
        &self,
        c: &Connection,
        id: &str,
        incarnation: &str,
        guards: &mut BTreeMap<String, File>,
    ) -> Result<()> {
        let previous = workspace_revision(c, incarnation)?;
        self.include_session_workspace(
            c,
            id,
            guards,
            Some((
                incarnation,
                previous
                    .checked_add(1)
                    .context("Workspace revision exhausted")?,
            )),
        )
    }
}

fn workspace_revision(c: &Connection, incarnation: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT coalesce(max(revision),0) FROM execution_workspace_paths WHERE incarnation=?1",
        [incarnation],
        |r| r.get(0),
    )?)
}

impl SessionDb {
    pub(super) fn execution_workspace_locks(&self) -> Result<Vec<File>> {
        let slot = self.stores.lease_slot(self.incarnation())?;
        let directory = slot.parent().context("Missing workspace lock directory")?;
        fs::create_dir_all(directory)?;
        // Capture the cutoff BEFORE publishing the slot, and read the binding
        // AFTER it. An intervening move is covered by its attachment or by the
        // direct path guards in the second snapshot. Neither read needs a G writer.
        let revision = self
            .stores
            .global()
            .read(|c| workspace_revision(c, self.incarnation()))?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&revision.to_le_bytes())?;
        shared_lock(file.as_file())?;
        let lease = file.persist(slot)?;
        let mut guards = self.stores.global().read(|c| {
            let snapshot = c.unchecked_transaction()?;
            self.stores.admit_session_workspace(&snapshot, self.id())
        })?;
        guards.push(lease);
        Ok(guards)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_removal_excludes_moves_and_execution_at_the_canonical_path() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stores = Stores::open(&root.path().join("data"))?;
        let source = root.path().join("source");
        let worktree = root.path().join("worktree");
        fs::create_dir(&source)?;
        fs::create_dir(&worktree)?;
        let subdirectory = worktree.join("nested");
        fs::create_dir(&subdirectory)?;
        #[cfg(unix)]
        let target = {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&subdirectory, &alias)?;
            alias
        };
        #[cfg(not(unix))]
        let target = subdirectory;
        let create = |path: &Path| {
            stores.create_code_session(path, "model", "provider", &uuid::Uuid::new_v4().to_string())
        };
        let resident = create(&target)?;
        let moving = create(&source)?;
        let change = || {
            crate::services::session_workspace::change(
                &stores,
                moving.id(),
                &source,
                target.to_str().unwrap(),
                false,
            )
        };
        let removal = stores.hold_worktree_removal(&worktree)?;
        assert!(resident.acquire_execution().is_err());
        assert!(change().is_err());
        assert_eq!(moving.workspace()?.canonicalize()?, source.canonicalize()?);
        let elsewhere = moving.acquire_execution()?;
        drop((elsewhere, removal));
        assert_eq!(change()?.canonicalize()?, target.canonicalize()?);
        let lease = moving.acquire_execution()?;
        assert!(stores.hold_worktree_removal(&worktree).is_err());
        drop(lease);
        stores.hold_worktree_removal(&worktree)?;
        Ok(())
    }

    #[test]
    fn project_folder_edits_attach_to_leases_in_another_process() -> Result<()> {
        use crate::db::session_store::{ProjectEdit, Projects};
        const EDIT_ROOT: &str = "DCL_TEST_LEASE_PROJECT_EDIT_ROOT";
        if let Some(root) = std::env::var_os(EDIT_ROOT) {
            let root = std::path::PathBuf::from(root);
            let stores = Stores::open(&root.join("data"))?;
            let project = stores.projects()?.remove(0);
            let source = project.workspace_path.unwrap();
            let secondary = root.join("secondary").to_string_lossy().into_owned();
            for folders in [vec![source.clone(), secondary.clone()], vec![source]] {
                let added = folders.len() == 2;
                stores.edit_project(
                    &project.id,
                    &ProjectEdit {
                        folders: Some(folders),
                        ..Default::default()
                    },
                )?;
                let occupants = stores
                    .workspace_sessions()?
                    .into_iter()
                    .filter(|(_, _, path)| path == &secondary)
                    .count();
                assert_eq!(occupants, if added { 2 } else { 0 });
            }
            return Ok(());
        }
        let root = tempfile::tempdir()?;
        let source = root.path().join("primary");
        let secondary = root.path().join("secondary");
        fs::create_dir(&source)?;
        fs::create_dir(&secondary)?;
        let stores = Stores::open(&root.path().join("data"))?;
        let create =
            || stores.create_code_session(&source, "m", "p", &uuid::Uuid::new_v4().to_string());
        let first = create()?;
        let second = create()?;
        let a = first.acquire_execution()?;
        let b = second.acquire_execution()?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact","db::session_store::workspace_lock::tests::project_folder_edits_attach_to_leases_in_another_process"])
            .env(EDIT_ROOT,root.path()).status()?;
        assert!(status.success());
        assert!(stores.hold_worktree_removal(&secondary).is_err());
        drop(a);
        assert!(stores.hold_worktree_removal(&secondary).is_err());
        drop(b);
        let _new_turn = second.acquire_execution()?;
        stores.hold_worktree_removal(&secondary)?;
        Ok(())
    }

    #[test]
    fn import_replay_keeps_all_rows_hidden_until_path_admission() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stores = Stores::open(&root.path().join("data"))?;
        let workspace = root.path().join("workspace");
        let archive = root.path().join("archive");
        fs::create_dir(&workspace)?;
        fs::create_dir(&archive)?;
        for id in ["first", "second"] {
            fs::write(
                archive.join(format!("{id}.json")),
                serde_json::to_vec(
                    &serde_json::json!({"id":id,"name":"Import","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","messages":[]}),
                )?,
            )?;
        }
        let fence = stores.hold_worktree_removal(&workspace)?;
        let error = stores.import_json(&archive, &workspace, false).unwrap_err();
        assert!(format!("{error:#}").contains("Worktree removal in progress"));
        assert!(stores.open_session("first").is_err());
        assert!(stores
            .global()
            .list_sessions(Default::default())?
            .is_empty());
        drop(fence);
        stores.open_session("second")?;
        assert_eq!(stores.global().list_sessions(Default::default())?.len(), 2);
        Ok(())
    }
}
