//! Directory rows contain the display fields needed before a session is opened.
use super::{GlobalDb, Result, SessionDb, Stores};
use crate::db::conversation_repo::{row_to_conversation, ConversationRow};
use rusqlite::{params_from_iter, types::Value, OptionalExtension};

#[derive(Default)]
pub struct DirectoryFilter<'a> {
    pub project: Option<&'a str>,
    pub workspace: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub archived: Option<bool>,
    pub pinned: Option<bool>,
    pub nonempty: bool,
    pub exclude: Option<&'a str>,
}

pub struct DirectoryEntry {
    pub conversation: ConversationRow,
    pub incarnation: String,
    pub workspace_incarnation: String,
    pub first_prompt: Option<String>,
    pub latest_prompt: Option<String>,
    pub git_branch: Option<String>,
    pub transcript_bytes: i64,
    pub last_turn_status: Option<String>,
    pub content_revision: i64,
    pub(super) metadata_revision: i64,
    pub(super) stored_title: Option<String>,
}
const FIELDS: &str = "SELECT id, title, model, provider, workspace_path, project_id,
    research_mode, pinned, archived, created_at, updated_at, message_count, workspace_mode,
    incarnation, first_prompt, latest_prompt, git_branch, transcript_bytes, last_turn_status,
    content_revision, coalesce((SELECT b.parent_incarnation FROM conversation_private_workspace_bindings b
        WHERE b.child_id=conversations.id AND conversations.project_id IS NULL),incarnation),metadata_revision
    FROM conversations";
pub(super) fn display_title(stored: Option<String>, first_prompt: Option<&str>) -> Option<String> {
    if stored.as_deref().is_none_or(|title| title.trim().is_empty() || title.eq_ignore_ascii_case("CLI session")) {
        first_prompt.map(str::to_owned).or(stored)
    } else {
        stored
    }
}

fn entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<DirectoryEntry> {
    let mut conversation = row_to_conversation(row)?;
    let first_prompt: Option<String> = row.get(14)?;
    let stored_title = conversation.title.take();
    conversation.title = display_title(stored_title.clone(), first_prompt.as_deref());
    Ok(DirectoryEntry {
        stored_title,
        conversation,
        incarnation: row.get(13)?,
        first_prompt,
        latest_prompt: row.get(15)?,
        git_branch: row.get(16)?,
        transcript_bytes: row.get(17)?,
        last_turn_status: row.get(18)?,
        content_revision: row.get(19)?,
        workspace_incarnation: row.get(20)?,
        metadata_revision: row.get(21)?,
    })
}

pub(super) fn one(c: &rusqlite::Connection, id: &str) -> Result<Option<DirectoryEntry>> {
    Ok(c.query_row(&format!("{FIELDS} WHERE id=?1"), [id], entry)
        .optional()?)
}

pub(super) fn list(
    conn: &rusqlite::Connection,
    filter: DirectoryFilter<'_>,
) -> Result<Vec<DirectoryEntry>> {
    list_ordered(conn, filter, false)
}

fn list_ordered(
    conn: &rusqlite::Connection,
    filter: DirectoryFilter<'_>,
    descending_id: bool,
) -> Result<Vec<DirectoryEntry>> {
    let mut sql = String::from(FIELDS);
    let mut conditions = Vec::new();
    let mut args = Vec::<Value>::new();
    for (predicate, value) in [
        ("project_id=?", filter.project),
        ("workspace_mode=?", filter.mode),
        ("id<>?", filter.exclude),
    ] {
        if let Some(value) = value {
            conditions.push(predicate);
            args.push(value.to_owned().into());
        }
    }
    if let Some(workspace) = filter.workspace {
        // A Git worktree at the repository root can carry an empty final
        // component. The picker canonicalizes its input and drops that separator.
        conditions.push("(workspace_path=? OR workspace_path=?)");
        args.push(workspace.to_owned().into());
        args.push(format!("{workspace}{}", std::path::MAIN_SEPARATOR).into());
    }
    for (predicate, value) in [("archived=?", filter.archived), ("pinned=?", filter.pinned)] {
        if let Some(value) = value {
            conditions.push(predicate);
            args.push(i64::from(value).into());
        }
    }
    if filter.nonempty {
        conditions.push("message_count>0");
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY updated_at DESC,id");
    if descending_id {
        sql.push_str(" DESC");
    }

    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), entry)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
impl GlobalDb {
    pub fn list_sessions(&self, filter: DirectoryFilter<'_>) -> Result<Vec<DirectoryEntry>> {
        self.read(|c| list(c, filter))
    }
    pub(super) fn continuation_candidates(&self) -> Result<Vec<DirectoryEntry>> {
        self.read(|c| {
            list_ordered(
                c,
                DirectoryFilter {
                    mode: Some("code"),
                    archived: Some(false),
                    nonempty: true,
                    ..Default::default()
                },
                true,
            )
        })
    }
    pub fn conversation(&self, id: &str) -> Result<Option<ConversationRow>> {
        self.read_directory_row(id, |c| Ok(one(c, id)?.map(|r| r.conversation)))
    }
}
impl Stores {
    pub fn conversations(&self) -> Result<Vec<ConversationRow>> {
        Ok(self
            .global()
            .list_sessions(DirectoryFilter::default())?
            .into_iter()
            .map(|row| row.conversation)
            .collect())
    }

    /// An enumerated row can be deleted and its logical ID reused before selection.
    pub fn open_listed(&self, row: &DirectoryEntry) -> Result<SessionDb> {
        self.open_recorded(&row.conversation.id, &row.incarnation)
    }
}

pub struct ResumeEntry {
    pub conversation: ConversationRow,
    pub latest: Option<String>,
    pub branch: Option<String>,
    pub bytes: i64,
}

impl Stores {
    pub fn resume_entries(
        &self,
        current: &str,
        workspace: Option<&std::path::Path>,
    ) -> Result<Vec<ResumeEntry>> {
        let original = workspace.map(|p| p.to_string_lossy().into_owned());
        let canonical = workspace
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.to_string_lossy().into_owned());
        let query = |workspace: Option<&str>| {
            self.global().list_sessions(DirectoryFilter {
                archived: Some(false),
                nonempty: true,
                exclude: Some(current),
                workspace,
                ..Default::default()
            })
        };
        let mut rows = query(original.as_deref())?;
        if rows.is_empty() {
            if let Some(canonical) = canonical
                .as_deref()
                .filter(|canonical| Some(*canonical) != original.as_deref())
            {
                rows = query(Some(canonical))?;
            }
        }
        Ok(rows
            .into_iter()
            .map(|row| ResumeEntry {
                conversation: row.conversation,
                latest: row.latest_prompt,
                branch: row.git_branch,
                bytes: row.transcript_bytes,
            })
            .collect())
    }

    pub fn record_git_branch(&self, id: &str, branch: &str) -> Result<()> {
        self.open_session(id)?
            .write(|w| w.set_git_branch(Some(branch)))
    }

    pub fn rename_session(&self, id: &str, title: &str) -> Result<()> {
        let key = super::layout::storage_key(id)?;
        let _guards = self
            .resolved_key(&key)?
            .ok_or_else(|| anyhow::anyhow!("Session unavailable"))?;
        self.global().write_with_deadline("session name update",id,false,crate::db::STARTUP_WRITE_DEADLINE,|c| {
            anyhow::ensure!(c.execute("UPDATE conversations SET title=?1, metadata_revision=metadata_revision+1,
                updated_at=CASE WHEN julianday(updated_at)>=julianday(?2) THEN updated_at ELSE ?2 END WHERE id=?3",
                rusqlite::params![title,chrono::Utc::now().to_rfc3339(),id])?==1,"Session unavailable");
            Ok(())
        })
    }
}

impl crate::db::DbManager {
    pub fn resume_entries(
        &self,
        current: &str,
        workspace: Option<&std::path::Path>,
    ) -> Result<Vec<ResumeEntry>> {
        let here = workspace.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_owned()));
        self.with_conn(|c| {
            let mut result=Vec::new();
            for mut row in crate::db::conversation_repo::list_conversations(c)? {
                if row.id==current || row.archived || row.message_count==0 {continue;}
                if let Some(root)=&here {
                    if row.workspace_path.as_deref().map(std::path::Path::new).map(|p|p.canonicalize().unwrap_or_else(|_|p.to_owned())).as_ref()!=Some(root) { continue; }
                }
                if row.title.as_deref().is_none_or(|t|t.trim().is_empty()||t=="CLI session") {
                    let first:Option<String>=c.query_row("SELECT content FROM messages WHERE conversation_id=?1 AND role='user' AND instr(content,?2)<>1 ORDER BY sort_order,rowid LIMIT 1",
                        [&row.id,crate::tools::background_shell::NOTIFICATION_START],|r|r.get(0)).optional()?;
                    row.title=first.as_deref().and_then(crate::db::conversation_repo::prompt_title);
                }
                let latest=c.query_row("SELECT content FROM messages WHERE conversation_id=?1 AND role='user' ORDER BY created_at DESC LIMIT 1",[&row.id],|r|r.get(0)).optional()?;
                let branch=c.query_row("SELECT value FROM peripheral_settings WHERE key=?1",[format!("cli.git-branch.{}",row.id)],|r|r.get(0)).optional()?;
                let bytes=c.query_row("SELECT (SELECT coalesce(sum(length(CAST(content AS BLOB))+coalesce(length(CAST(thinking AS BLOB)),0)),0) FROM messages WHERE conversation_id=?1)
                     +(SELECT coalesce(sum(coalesce(length(CAST(t.input AS BLOB)),0)+coalesce(length(CAST(t.output AS BLOB)),0)),0) FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1)
                     +(SELECT coalesce(sum(length(CAST(t.requests_json AS BLOB))),0) FROM message_trajectory t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1)",[&row.id],|r|r.get(0))?;
                result.push(ResumeEntry{conversation:row,latest,branch,bytes});
            }
            Ok(result)
        })?
    }

    pub fn record_git_branch(&self, id: &str, branch: &str) -> Result<()> {
        self.with_conn(|c| c.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![format!("cli.git-branch.{id}"),branch]))??;
        Ok(())
    }
    pub fn rename_session(&self, id: &str, title: &str) -> Result<()> {
        self.with_conn(|c| crate::db::conversation_repo::update_conversation_title(c, id, title))?
    }
}
