//! Resolve the same project and history for interactive hosts, SDKs and workers.
use super::{CreateOptions, ProjectSelection, SessionDb, SessionWrite, Stores};
use crate::db::{conversation_repo::ConversationRow, project_repo};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

impl Stores {
    pub fn create_code_session(
        &self,
        workspace: &Path,
        model: &str,
        provider: &str,
        id: &str,
    ) -> Result<SessionDb> {
        let id = uuid::Uuid::parse_str(id)
            .context("Session ID must be a UUID")?
            .to_string();
        let path = workspace
            .canonicalize()
            .context("Project directory is unavailable")?;
        ensure!(path.is_dir(), "Project path must be a directory");
        let _admission = self.admit_workspace_path(&path)?;
        self.create(
            &id,
            CreateOptions {
                project: Some(ProjectSelection::Workspace(path)),
                workspace_mode: Some("code".into()),
                model: Some(model.into()),
                provider: Some(provider.into()),
                title: Some("CLI session".into()),
                ..Default::default()
            },
        )
    }

    pub fn resume_code(&self, id: &str) -> Result<(SessionDb, ConversationRow)> {
        let shard = self.open_session(id)?;
        let (metadata, _) = shard.code_selection()?;
        Ok((shard, metadata))
    }

    /// Resume for a read-only history viewer.
    ///
    /// Unlike [`Self::resume_code`], a deleted project folder or managed
    /// worktree is reported as [`ResumedWorkspace::Missing`] instead of an
    /// error. Every other eligibility and trust rule stays identical.
    pub fn resume_code_for_inspection(
        &self,
        id: &str,
    ) -> Result<(SessionDb, ConversationRow, ResumedWorkspace)> {
        let shard = self.open_session(id)?;
        let (metadata, workspace) = shard.code_selection_for_inspection()?;
        Ok((shard, metadata, workspace))
    }

    pub fn latest_code(&self, workspace: &Path) -> Result<Option<ConversationRow>> {
        let workspace = workspace
            .canonicalize()
            .context("Project directory is unavailable")?;
        ensure!(workspace.is_dir(), "Project path must be a directory");
        let rows = self.global().continuation_candidates()?;
        for row in rows {
            // Worktrees and private children can have a different directory from
            // the project's main path. Only an eligible selection opens history.
            let current = self.global().read_directory_row(&row.conversation.id, |c| {
                let snapshot = c.unchecked_transaction()?;
                let matches: bool = snapshot.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND incarnation=?2 AND archived=0 AND workspace_mode='code')",
                    [&row.conversation.id,&row.incarnation], |r|r.get(0))?;
                if !matches {return Ok(None);}
                let actual = super::lifecycle::workspace(&snapshot,&self.layout().workspaces(),&row.conversation.id);
                Ok(actual.ok().and_then(|p|p.canonicalize().ok()).filter(|p|p==&workspace))
            })?;
            if current.is_some() && self.open_listed(&row).is_ok() {
                return Ok(Some(row.conversation));
            }
        }
        Ok(None)
    }
}

impl SessionDb {
    pub fn code_selection(&self) -> Result<(ConversationRow, std::path::PathBuf)> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            let mut row = super::directory::one(&tx, self.id())?
                .context("Session not found")?
                .conversation;
            ensure!(
                row.workspace_mode == "code",
                "Only Code sessions can be resumed in CLI"
            );
            ensure!(!row.archived, "Session is archived");
            crate::native_engine::workspace::conversation_project(&tx, self.id())?;
            let path =
                super::lifecycle::workspace(&tx, &self.stores.layout().workspaces(), self.id())?;
            if row.project_id.is_none() {
                row.workspace_path = Some(path.to_string_lossy().into_owned());
            }
            Ok((row, path))
        })
    }

    /// The read-only counterpart of [`Self::code_selection`].
    ///
    /// A missing workspace is a display condition for inspection. Trust,
    /// archived state and canonical folder identity are still enforced exactly
    /// as they are before an executing resume.
    pub fn code_selection_for_inspection(
        &self,
    ) -> Result<(ConversationRow, ResumedWorkspace)> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            let mut row = super::directory::one(&tx, self.id())?
                .context("Session not found")?
                .conversation;
            ensure!(
                row.workspace_mode == "code",
                "Only Code sessions can be resumed in CLI"
            );
            ensure!(!row.archived, "Session is archived");
            let workspace = inspection_workspace(
                &tx,
                self.id(),
                &row,
                &self.stores.layout().workspaces(),
            )?;
            if row.project_id.is_none() {
                if let ResumedWorkspace::Available(path) = &workspace {
                    row.workspace_path = Some(path.to_string_lossy().into_owned());
                }
            }
            Ok((row, workspace))
        })
    }
}

fn inspection_workspace(
    conn: &rusqlite::Connection,
    id: &str,
    row: &ConversationRow,
    workspaces_root: &Path,
) -> Result<ResumedWorkspace> {
    let Some(project_id) = row.project_id.as_deref().filter(|id| !id.is_empty()) else {
        let path = super::lifecycle::workspace(conn, workspaces_root, id)?;
        return Ok(ResumedWorkspace::Available(path));
    };
    let project = project_repo::get_project(conn, project_id)?.context("Project not found")?;
    ensure!(
        !project.is_archived,
        "Select an active project before sending messages"
    );
    ensure!(
        !project.folders.is_empty(),
        "Add at least one folder to this project"
    );
    ensure!(
        project.folders.iter().all(|folder| folder.trusted),
        "Trust all project folders before starting a chat"
    );
    let mut existing = Vec::new();
    let mut missing = Vec::new();
    for folder in &project.folders {
        let path = Path::new(&folder.path);
        ensure!(
            path.is_absolute(),
            "Folder must be an existing absolute directory: {}",
            path.display()
        );
        if path.is_dir() {
            existing.push(folder.path.clone());
        } else {
            missing.push(folder.path.clone());
        }
    }
    if missing.is_empty() {
        // Existing folders must still match their canonical form. A relocated
        // or tampered folder never becomes readable merely because it exists.
        let project = project_repo::require_trusted(conn, project_id)?;
        let source = project
            .workspace_path
            .as_deref()
            .context("Project has no working directory")?;
        let source_path = Path::new(source);
        ensure!(
            source_path.is_absolute() && source_path.is_dir(),
            "Project folder is unavailable"
        );
        let worktree: Option<(String, String)> = conn
            .query_row(
                "SELECT source_path,path FROM conversation_worktrees WHERE conversation_id=?1 AND project_id=?2",
                params![id, project_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((original, path)) = &worktree {
            let path = PathBuf::from(path);
            if path.is_absolute() && !path.is_dir() {
                let same_source = Path::new(original).canonicalize()?
                    == source_path.canonicalize()?;
                ensure!(
                    same_source,
                    "Project source folder changed; start a new conversation for this folder"
                );
                return Ok(ResumedWorkspace::Missing {
                    path: path.clone(),
                    missing_path: path,
                    reason: "Conversation worktree directory no longer exists".into(),
                });
            }
        }
        if let Some(path) =
            crate::worktree::session::resolve(conn, id, project_id, source_path)?
        {
            return Ok(ResumedWorkspace::Available(path));
        }
        return Ok(ResumedWorkspace::Available(source_path.to_path_buf()));
    }
    // Other folders still on disk must be the exact canonical paths recorded
    // with their trust decision.
    if !existing.is_empty() {
        let normalized = project_repo::normalize_folders(&existing)?;
        ensure!(
            normalized == existing,
            "Project folders have changed; select and trust them again"
        );
    }
    let worktree: Option<String> = conn
        .query_row(
            "SELECT path FROM conversation_worktrees WHERE conversation_id=?1 AND project_id=?2",
            params![id, project_id],
            |row| row.get(0),
        )
        .optional()?;
    let recorded = worktree
        .filter(|path| Path::new(path).is_absolute())
        .or_else(|| project.workspace_path.clone())
        .or_else(|| row.workspace_path.clone());
    let missing = missing.remove(0);
    let path = recorded
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&missing));
    let missing_path = PathBuf::from(&missing);
    // A single missing project keeps the plain directory wording. When another
    // folder is still recorded as the display workspace, name the missing
    // folder explicitly so the notice never blames the surviving folder.
    let reason = if path == missing_path {
        format!("Directory no longer exists: {}", missing_path.display())
    } else {
        format!("Project folder no longer exists: {}", missing_path.display())
    };
    Ok(ResumedWorkspace::Missing {
        path,
        missing_path,
        reason,
    })
}

/// What a resume can do with the session's saved workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumedWorkspace {
    Available(PathBuf),
    Missing {
        /// The recorded display workspace, which may still exist when another
        /// folder of a multi-folder project is gone.
        path: PathBuf,
        /// The folder or worktree path that actually disappeared.
        missing_path: PathBuf,
        reason: String,
    },
}

impl SessionWrite<'_> {
    pub(super) fn append_user_content(
        &self,
        display: &str,
        content: &Value,
        typed: bool,
    ) -> Result<String> {
        let content = crate::runtime::normalize_user_content(content)?;
        let message = self.append("user", display)?;
        self.0.execute(
            "INSERT INTO message_user_content(message_id,content_json) VALUES(?1,?2)",
            params![message, serde_json::to_string(&content)?],
        )?;
        if typed {
            crate::services::permission_authority::record_conn(
                self.0,
                self.1,
                crate::services::permission_authority::Source::User,
                &content,
            )?;
        }
        Ok(message)
    }

    pub fn append_hook_user(&self, text: &str) -> Result<String> {
        let message = self.append("user", text)?;
        self.set_message_metadata(
            &message,
            &json!({"type":"hook","hookEvent":"SessionStart","promptOrigin":"hook"}),
        )?;
        Ok(message)
    }
}

#[cfg(test)]
mod inspection_tests {
    use super::*;

    struct Fixture {
        root: tempfile::TempDir,
        stores: Stores,
        project: PathBuf,
        id: String,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let stores = Stores::open(&root.path().join("data")).unwrap();
            let id = stores
                .create_code_session(&project, "model", "provider", &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .id()
                .to_owned();
            Self {
                root,
                stores,
                project,
                id,
            }
        }

        fn delete_project(&self) {
            std::fs::remove_dir_all(&self.project).unwrap();
        }

        fn missing(&self) -> ResumedWorkspace {
            self.stores
                .resume_code_for_inspection(&self.id)
                .unwrap()
                .2
        }

        fn strict_error(&self) -> String {
            format!("{:#}", self.stores.resume_code(&self.id).err().expect("strict resume"))
        }
    }

    #[test]
    fn missing_project_folder_is_inspectable_but_not_resumable() {
        let fixture = Fixture::new();
        fixture.delete_project();
        let workspace = fixture.missing();
        let ResumedWorkspace::Missing {
            path,
            missing_path,
            reason,
        } = workspace
        else {
            panic!("expected a missing workspace: {workspace:?}");
        };
        assert_eq!(path, fixture.project);
        assert_eq!(missing_path, fixture.project);
        assert!(reason.contains("Directory no longer exists"), "{reason}");
        let error = fixture.strict_error();
        assert!(
            error.contains("Folder must be an existing absolute directory")
                || error.contains("Project folder is unavailable"),
            "{error}"
        );
    }

    #[test]
    fn missing_bound_worktree_is_inspectable_but_not_resumable() {
        let fixture = Fixture::new();
        let path = fixture.root.path().join("gone-worktree");
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch)
                     SELECT id,project_id,workspace_path,workspace_path,?2,'agent/missing' FROM conversations WHERE id=?1",
                    params![fixture.id, path.to_string_lossy()],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let ResumedWorkspace::Missing {
            path: shown,
            missing_path,
            reason,
        } = fixture.missing()
        else {
            panic!("expected a missing worktree");
        };
        assert_eq!(shown, path);
        assert_eq!(missing_path, path);
        assert!(reason.contains("worktree"), "{reason}");
        assert!(fixture.strict_error().contains("worktree"), "{}", fixture.strict_error());
    }

    #[test]
    fn untrusted_and_archived_scope_never_becomes_read_only() {
        let fixture = Fixture::new();
        fixture.delete_project();
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE project_folders SET trusted=0 WHERE project_id=(SELECT project_id FROM conversations WHERE id=?1)",
                    params![fixture.id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = format!(
            "{:#}",
            fixture
                .stores
                .resume_code_for_inspection(&fixture.id)
                .err()
                .expect("untrusted")
        );
        assert!(error.contains("Trust all project folders"), "{error}");

        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE project_folders SET trusted=1 WHERE project_id=(SELECT project_id FROM conversations WHERE id=?1)",
                    params![fixture.id],
                )?;
                conn.execute(
                    "UPDATE conversations SET archived=1 WHERE id=?1",
                    params![fixture.id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = format!(
            "{:#}",
            fixture
                .stores
                .resume_code_for_inspection(&fixture.id)
                .err()
                .expect("archived session")
        );
        assert!(error.contains("Session is archived"), "{error}");
    }

    #[test]
    fn archived_project_and_non_code_sessions_remain_errors() {
        let fixture = Fixture::new();
        fixture.delete_project();
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE projects SET is_archived=1 WHERE id=(SELECT project_id FROM conversations WHERE id=?1)",
                    params![fixture.id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = format!(
            "{:#}",
            fixture
                .stores
                .resume_code_for_inspection(&fixture.id)
                .err()
                .expect("archived project")
        );
        assert!(
            error.contains("Select an active project before sending messages"),
            "{error}"
        );

        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE projects SET is_archived=0 WHERE id=(SELECT project_id FROM conversations WHERE id=?1)",
                    params![fixture.id],
                )?;
                conn.execute(
                    "UPDATE conversations SET workspace_mode='chat' WHERE id=?1",
                    params![fixture.id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = format!(
            "{:#}",
            fixture
                .stores
                .resume_code_for_inspection(&fixture.id)
                .err()
                .expect("non-code session")
        );
        assert!(
            error.contains("Only Code sessions can be resumed in CLI"),
            "{error}"
        );
    }

    #[test]
    fn an_existing_folder_with_a_changed_canonical_path_stays_an_error() {
        let fixture = Fixture::new();
        let altered = format!("{}/.", fixture.project.display());
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE project_folders SET path=?2 WHERE project_id=(SELECT project_id FROM conversations WHERE id=?1)",
                    params![fixture.id, altered],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = format!(
            "{:#}",
            fixture
                .stores
                .resume_code_for_inspection(&fixture.id)
                .err()
                .expect("changed canonical path")
        );
        assert!(error.contains("Project folders have changed"), "{error}");
    }

    #[test]
    fn multi_folder_project_reports_the_deleted_folder_and_rejects_a_changed_primary() {
        let fixture = Fixture::new();
        let primary = fixture.project.clone();
        let secondary = fixture.root.path().join("secondary");
        std::fs::create_dir(&secondary).unwrap();
        let secondary = secondary.canonicalize().unwrap();
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "INSERT INTO project_folders(project_id,path,trusted,sort_order)
                     SELECT project_id,?2,1,1 FROM conversations WHERE id=?1",
                    params![fixture.id, secondary.to_string_lossy()],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(&secondary).unwrap();

        let ResumedWorkspace::Missing {
            path,
            missing_path,
            reason,
        } = fixture.missing()
        else {
            panic!("expected the deleted secondary folder");
        };
        assert_eq!(path, primary);
        assert_eq!(missing_path, secondary);
        assert!(
            reason.contains(&secondary.display().to_string()),
            "{reason}"
        );
        assert!(
            !reason.contains(&primary.display().to_string()),
            "{reason}"
        );

        // A surviving folder still has to keep its recorded canonical path.
        let altered = format!("{}/.", primary.display());
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE project_folders SET path=?2 WHERE project_id=
                     (SELECT project_id FROM conversations WHERE id=?1) AND sort_order=0",
                    params![fixture.id, altered],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = format!(
            "{:#}",
            fixture
                .stores
                .resume_code_for_inspection(&fixture.id)
                .err()
                .expect("changed primary folder")
        );
        assert!(error.contains("Project folders have changed"), "{error}");
    }

    #[test]
    fn projectless_sessions_resolve_their_private_workspace() {
        let fixture = Fixture::new();
        fixture
            .stores
            .fixture_directory(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE conversations SET project_id=NULL,workspace_path=NULL WHERE id=?1",
                    params![fixture.id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let (row, workspace) = fixture
            .stores
            .open_session(&fixture.id)
            .unwrap()
            .code_selection_for_inspection()
            .unwrap();
        let ResumedWorkspace::Available(path) = workspace else {
            panic!("expected the private workspace");
        };
        assert!(path.is_dir());
        assert_eq!(row.workspace_path.as_deref(), path.to_str());
    }
}
