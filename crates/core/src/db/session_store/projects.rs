use super::{DirectoryFilter, Stores};
use crate::db::{conversation_repo::ConversationRow, project_repo as project};
use anyhow::{Context, Result};
use project::{ProjectFileRow, ProjectRow};
use rusqlite::Connection;

#[derive(Default)]
pub struct ProjectEdit {
    pub name: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub folders: Option<Vec<String>>,
    pub archived: Option<bool>,
}

pub trait Projects {
    fn projects(&self) -> Result<Vec<ProjectRow>>;
    fn trusted_project(&self, id: &str) -> Result<ProjectRow>;
    fn create_project(&self, id: &str, edit: &ProjectEdit) -> Result<ProjectRow>;
    fn edit_project(&self, id: &str, edit: &ProjectEdit) -> Result<ProjectRow>;
    fn trust_project(&self, id: &str, folders: &[String]) -> Result<ProjectRow>;
    fn delete_project(&self, id: &str) -> Result<()>;
    fn project_conversations(&self, id: &str) -> Result<Vec<ConversationRow>>;
    fn project_detail(
        &self,
        id: &str,
    ) -> Result<(ProjectRow, Vec<ProjectFileRow>, Vec<ConversationRow>)>;
}

fn save(c: &Connection, id: &str, edit: &ProjectEdit, create: bool) -> Result<ProjectRow> {
    if create {
        let name = edit
            .name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .context("Project name is required")?;
        let folders = project::normalize_folders(edit.folders.as_deref().unwrap_or_default())?;
        let now = chrono::Utc::now().to_rfc3339();
        project::insert_project(
            c,
            id,
            name,
            edit.description.as_deref(),
            edit.instructions.as_deref(),
            Some(&folders[0]),
            false,
            &now,
            &now,
        )?;
        project::set_folders(c, id, &folders)?;
    } else {
        project::update_project(
            c,
            id,
            edit.name.as_deref(),
            edit.description.as_deref(),
            edit.instructions.as_deref(),
            None,
            edit.archived,
        )?;
        if let Some(folders) = &edit.folders {
            c.execute(
                "UPDATE conversations SET metadata_revision=metadata_revision+1 WHERE project_id=?1
                 AND id NOT IN (SELECT conversation_id FROM conversation_worktrees)",
                [id],
            )?;
            project::set_folders(c, id, folders)?;
        }
    }
    project::get_project(c, id)?.context("Project not found")
}
fn trust(c: &Connection, id: &str, folders: &[String]) -> Result<ProjectRow> {
    project::trust_folders(c, id, folders)?;
    project::get_project(c, id)?.context("Project not found")
}
impl Projects for Stores {
    fn projects(&self) -> Result<Vec<ProjectRow>> {
        self.global().read(project::list_projects)
    }
    fn trusted_project(&self, id: &str) -> Result<ProjectRow> {
        self.global().read(|c| project::require_trusted(c, id))
    }
    fn create_project(&self, id: &str, edit: &ProjectEdit) -> Result<ProjectRow> {
        self.global()
            .write("create project", id, false, |c| save(c, id, edit, true))
    }
    fn edit_project(&self, id: &str, edit: &ProjectEdit) -> Result<ProjectRow> {
        self.global()
            .write("update project", id, false, |c| {
                let project = save(c, id, edit, false)?;
                let mut admission = Default::default();
                if edit.folders.is_some() {
                    for folder in &project.folders {
                        self.include_workspace_path(
                            std::path::Path::new(&folder.path),
                            &mut admission,
                            |_| Ok(()),
                        )?;
                    }
                    let mut q =
                        c.prepare("SELECT id,incarnation FROM conversations WHERE project_id=?1")?;
                    for row in q.query_map([id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })? {
                        let (session, incarnation) = row?;
                        self.admit_workspace_change(c, &session, &incarnation, &mut admission)?;
                    }
                }
                Ok((project, admission))
            })
            .map(|(project, _admission)| project)
    }
    fn trust_project(&self, id: &str, folders: &[String]) -> Result<ProjectRow> {
        self.global()
            .write("trust project", id, false, |c| trust(c, id, folders))
    }
    fn delete_project(&self, id: &str) -> Result<()> {
        self.global().write("delete project", id, false, |c| {
            c.execute(
                "UPDATE conversations SET metadata_revision=metadata_revision+1 WHERE project_id=?1",
                [id],
            )?;
            project::delete_project(c, id)
        })
    }
    fn project_conversations(&self, id: &str) -> Result<Vec<ConversationRow>> {
        Ok(self
            .global()
            .list_sessions(DirectoryFilter {
                project: Some(id),
                ..Default::default()
            })?
            .into_iter()
            .map(|e| e.conversation)
            .collect())
    }
    fn project_detail(
        &self,
        id: &str,
    ) -> Result<(ProjectRow, Vec<ProjectFileRow>, Vec<ConversationRow>)> {
        self.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            let row = project::get_project(&tx, id)?.context("Project not found")?;
            let files = project::list_project_files(&tx, id)?;
            let chats = super::directory::list(
                &tx,
                DirectoryFilter {
                    project: Some(id),
                    ..Default::default()
                },
            )?
            .into_iter()
            .map(|e| e.conversation)
            .collect();
            Ok((row, files, chats))
        })
    }
}
fn chats(c: &Connection, id: &str) -> Result<Vec<ConversationRow>> {
    Ok(crate::db::conversation_repo::list_conversations(c)?
        .into_iter()
        .filter(|r| r.project_id.as_deref() == Some(id))
        .collect())
}
impl Projects for crate::db::DbManager {
    fn projects(&self) -> Result<Vec<ProjectRow>> {
        self.with_conn(project::list_projects)?
    }
    fn trusted_project(&self, id: &str) -> Result<ProjectRow> {
        self.with_conn(|c| project::require_trusted(c, id))?
    }
    fn create_project(&self, id: &str, edit: &ProjectEdit) -> Result<ProjectRow> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "create project", super::WRITE_DEADLINE, |tx| {
                save(tx, id, edit, true)
            })
        })?
    }
    fn edit_project(&self, id: &str, edit: &ProjectEdit) -> Result<ProjectRow> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "edit project", super::WRITE_DEADLINE, |tx| {
                save(tx, id, edit, false)
            })
        })?
    }
    fn trust_project(&self, id: &str, folders: &[String]) -> Result<ProjectRow> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "trust project", super::WRITE_DEADLINE, |tx| {
                trust(tx, id, folders)
            })
        })?
    }
    fn delete_project(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "delete project", super::WRITE_DEADLINE, |tx| {
                project::delete_project(tx, id)
            })
        })?
    }
    fn project_conversations(&self, id: &str) -> Result<Vec<ConversationRow>> {
        self.with_conn(|c| chats(c, id))?
    }
    fn project_detail(
        &self,
        id: &str,
    ) -> Result<(ProjectRow, Vec<ProjectFileRow>, Vec<ConversationRow>)> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            Ok((
                project::get_project(&tx, id)?.context("Project not found")?,
                project::list_project_files(&tx, id)?,
                chats(&tx, id)?,
            ))
        })?
    }
}

impl crate::db::DbManager {
    pub fn create_project_chat(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
        mode: crate::db::conversation_repo::WorkspaceMode,
        branch: Option<&str>,
        use_worktree: bool,
    ) -> Result<ConversationRow> {
        use crate::db::conversation_repo as chats;
        let project = self.trusted_project(id)?;
        anyhow::ensure!(!project.is_archived, "Select an active project");
        let source = project
            .workspace_path
            .as_deref()
            .context("Project has no folder")?;
        let binding = super::CreateOptions::from_desktop_model(model)?;
        let prepared =
            crate::worktree::session::prepare(std::path::Path::new(source), branch, use_worktree)?;
        let chat = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let saved = self
            .with_conn(|c| {
                crate::db::write_tx(
                    c,
                    "create project conversation",
                    super::WRITE_DEADLINE,
                    |tx| {
                        let current = project::require_trusted(tx, id)?;
                        anyhow::ensure!(
                            !current.is_archived
                                && current.workspace_path == project.workspace_path,
                            "Project selection changed"
                        );
                        chats::insert_conversation(
                            tx,
                            &chat,
                            title,
                            binding.model.as_deref(),
                            binding.provider.as_deref(),
                            Some(&prepared.path.to_string_lossy()),
                            Some(id),
                            false,
                            false,
                            false,
                            &now,
                            &now,
                            0,
                        )?;
                        prepared.save(tx, &chat, id, source)?;
                        chats::set_workspace_mode(tx, &chat, mode.as_str())?;
                        chats::get_conversation(tx, &chat)?
                            .context("Project conversation unavailable")
                    },
                )
            })
            .and_then(|r| r);
        if saved.is_err() {
            prepared.cleanup();
        }
        saved
    }
}
