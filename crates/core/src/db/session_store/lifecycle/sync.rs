//! Sync one session bucket at a time. A newer header and its shard content
//! share the recoverable conversation-edit protocol.
use super::*;
use crate::db::session_store::boundary;
use crate::services::memory_files::prompt::Admission;
use crate::db::message_repo::MessageRow;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHeader {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    #[serde(alias = "workspace_path")]
    pub workspace_path: Option<String>,
    #[serde(alias = "project_id")]
    pub project_id: Option<String>,
    #[serde(default, alias = "workspace_mode")]
    pub workspace_mode: Option<crate::db::conversation_repo::WorkspaceMode>,
    #[serde(default, alias = "research_mode")]
    pub research_mode: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(alias = "created_at")]
    pub created_at: String,
    #[serde(alias = "updated_at")]
    pub updated_at: String,
}

fn rows(
    conn: &Connection,
    id: &str,
    messages: &[MessageRow],
    storage: Option<boundary::Storage<'_>>,
) -> Result<()> {
    let shared = matches!(storage, Some(boundary::Storage::Shared));
    let mut upsert=conn.prepare_cached("INSERT INTO messages(id,conversation_id,role,content,thinking,created_at,sort_order)
        VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET role=excluded.role,content=excluded.content,
        thinking=excluded.thinking,created_at=excluded.created_at,sort_order=excluded.sort_order
        WHERE messages.conversation_id=excluded.conversation_id AND messages.role=excluded.role")?;
    for m in messages {
        ensure!(
            m.conversation_id == id,
            "Message belongs to another sync bucket"
        );
        let was_boundary = conn
            .prepare_cached("SELECT 1 FROM messages WHERE id=?1 AND is_compact_boundary=1")?
            .exists([&m.id])?;
        let changed_user = m.role == "user"
            && !conn
                .prepare_cached("SELECT 1 FROM messages WHERE id=?1 AND content=?2")?
                .exists([&m.id, &m.content])?;
        // The typed payload is a rendering of the old user text. A replay or
        // a timestamp/order change preserves it and the other subordinate rows.
        conn.execute("DELETE FROM message_user_content WHERE message_id=?1 AND EXISTS(
            SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2 AND role='user' AND content<>?3)",
            params![m.id, id, m.content])?;
        ensure!(
            upsert.execute(params![
                m.id,
                id,
                m.role,
                m.content,
                m.thinking,
                m.created_at,
                m.sort_order
            ])? == 1,
            "Sync message owner or role changed"
        );
        if m.is_compact_boundary && !was_boundary {
            boundary::seal(
                conn,
                id,
                &m.id,
                boundary::Kind::Synced,
                storage.context("Missing memory scope for sync boundary")?,
            )?;
        } else if was_boundary && !m.is_compact_boundary {
            boundary::demote(conn, &m.id, shared)?;
        }
        if changed_user && m.is_compact_boundary && !shared {
            // A changed remote payload no longer carries the old human provenance.
            conn.execute("UPDATE message_metadata SET value=json_remove(value,'$.beforeSyncBoundary') WHERE message_id=?1", [&m.id])?;
        }
        if changed_user && !m.is_compact_boundary {
            if matches!(storage, Some(boundary::Storage::Shared)) {
                super::super::state::set_local(
                    conn,
                    &format!("message-origin:{}", m.id),
                    r#"{"promptOrigin":"sync"}"#,
                )?;
                continue;
            }
            conn.execute(
                "INSERT INTO message_metadata VALUES(?1,?2) ON CONFLICT(message_id)
                 DO UPDATE SET value=json_patch(message_metadata.value,excluded.value)",
                [&m.id, r#"{"promptOrigin":"sync"}"#],
            )?;
        }
    }
    Ok(())
}

// The redo file holds exactly the rows and scoped index validated before
// directory publication. Recovery replays them even if MEMORY.md has changed.
#[derive(Serialize, Deserialize)]
pub(in crate::db::session_store) struct SyncEdit<'a> {
    messages: std::borrow::Cow<'a, [MessageRow]>,
    digest: String,
    metadata_revision: i64,
    index: Option<crate::services::memory_files::prompt::IndexState>,
}

impl SyncEdit<'_> {
    pub(in crate::db::session_store) fn apply(
        &self,
        w: &super::super::SessionWrite<'_>,
    ) -> Result<()> {
        if !w
            .0
            .prepare_cached("SELECT 1 FROM sync_receipts WHERE digest=?1")?
            .exists([&self.digest])?
        {
            rows(
                w.0,
                w.1,
                &self.messages,
                self.index.as_ref().map(boundary::Storage::Frozen),
            )?;
            w.0.execute("INSERT INTO sync_receipts(digest,content_revision,metadata_revision) SELECT ?1,content_revision+1,?2 FROM manifest",params![self.digest,self.metadata_revision])?;
        }
        Ok(())
    }
}

pub(super) struct SyncSeed<'a> {
    pub header: &'a SyncHeader,
    pub messages: &'a [MessageRow],
    pub digest: &'a str,
}
impl SyncSeed<'_> {
    pub(super) fn populate(&self, c: &Connection, memory: &Admission) -> Result<()> {
        rows(
            c,
            &self.header.id,
            self.messages,
            Some(boundary::Storage::Shard(memory)),
        )?;
        c.execute("UPDATE manifest SET updated_at=?1,message_count=(SELECT count(*) FROM messages),content_revision=1",
            [&self.header.updated_at])?;
        c.execute(
            "INSERT INTO sync_receipts(digest,content_revision) VALUES(?1,1)",
            [self.digest],
        )?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SyncPublication {
    pub digest: String,
    pub workspace_path: Option<String>,
    pub pinned: bool,
    pub archived: bool,
}

/// Sync headers describe paths on the sending machine. Resolve project authority locally.
pub(super) fn workspace(
    c: &Connection,
    id: &str,
    project: Option<&str>,
    remote: Option<&str>,
) -> Result<Option<String>> {
    let Some(project) = project else {
        // A private child derives its directory from the local owner incarnation.
        let private_child = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_private_workspace_bindings WHERE child_id=?1)",
            [id], |row| row.get::<_, bool>(0),
        )?;
        return Ok(if private_child { None } else { remote.map(str::to_owned) });
    };
    let local = project_repo::require_trusted(c, project)?;
    ensure!(!local.is_archived, "Project is archived");
    let bound: Option<String> = c
        .query_row(
            "SELECT path FROM conversation_worktrees WHERE conversation_id=?1 AND project_id=?2",
            [id, project],
            |r| r.get(0),
        )
        .optional()?;
    Ok(bound.or(local.workspace_path))
}
impl Stores {
    pub fn sync_conversation(
        &self,
        id: &str,
        header: Option<&SyncHeader>,
        messages: &[MessageRow],
    ) -> Result<()> {
        let key = storage_key(id)?;
        if let Some(h) = header {
            ensure!(h.id == id, "Wrong sync header");
            chrono::DateTime::parse_from_rfc3339(&h.created_at)?;
            chrono::DateTime::parse_from_rfc3339(&h.updated_at)?;
        }
        let mut hasher = Sha256::new();
        serde_json::to_writer(&mut hasher, &(id, header, messages))?;
        let digest = format!("{:x}", hasher.finalize());
        let _locks = self.group_access(vec![(&key, true, header.is_some())])?;
        let known = self.global().read(|c| {
            Ok(c.query_row(
                "SELECT incarnation FROM sync_publications WHERE id=?1 AND digest=?2",
                [id, &digest],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })?;
        let ready = self.global().incarnation(id, &key)?;
        if let Some(previous) = known {
            ensure!(
                ready.as_ref() == Some(&previous),
                "Synced session was retired or replaced"
            );
            return Ok(());
        }
        let Some(incarnation) = ready else {
            let header = header.context("Missing conversation for sync messages")?;
            let options = CreateOptions {
                title: header.title.clone(),
                model: header.model.clone(),
                provider: header.provider.clone(),
                workspace_mode: header.workspace_mode.map(|mode| mode.as_str().to_owned()),
                research: header.research_mode,
                project: header.project_id.clone().map(ProjectSelection::Existing),
                ..Default::default()
            };
            self.create_locked(
                id,
                options,
                Seed::Sync(SyncSeed {
                    header,
                    messages,
                    digest: &digest,
                }),
                None,
            )?;
            return Ok(());
        };
        let shard = SessionDb {
            stores: self.clone(),
            key,
            id: id.into(),
            incarnation,
        };
        shard.upgrade_locked()?;
        let metadata_revision=self.global().read(|c| {
            if let Some(h)=header {
                let newer:Option<i64>=c.query_row("SELECT metadata_revision FROM conversations WHERE id=?1 AND julianday(updated_at)<julianday(?2)",[id,&h.updated_at],|r|r.get(0)).optional()?;
                if newer.is_some() {
                    if let Some(project)=&h.project_id { Project::select(c,&ProjectSelection::Existing(project.clone()))?; }
                }
                return Ok(newer);
            }
            Ok(None)
        })?;
        let memory = if messages.iter().any(|m| m.is_compact_boundary) {
            let (project, roots, default_owner) = self.global().read(|c| {
                let entry = super::super::directory::one(c, id)?.context("Missing sync session")?;
                let project = match (metadata_revision, header) {
                    (Some(_), Some(h)) => h.project_id.clone(),
                    _ => entry.conversation.project_id,
                };
                let roots = match &project {
                    Some(id) => project_repo::require_trusted(c, id)?
                        .folders
                        .into_iter()
                        .map(|f| PathBuf::from(f.path))
                        .collect::<Vec<_>>(),
                    None => Vec::new(),
                };
                let owner = if roots.is_empty() {
                    entry.workspace_incarnation
                } else {
                    entry.incarnation
                };
                Ok((project, roots, owner))
            })?;
            let saved = shard
                .connection_under_lock(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
                    super::super::settings::MEMORY_OWNER.read(c)
                })?;
            let owner = saved
                .filter(|(p, _)| p == &project)
                .map_or(default_owner, |(_, owner)| owner);
            let agent = shard
                .connection_under_lock(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
                    super::super::settings::MEMORY_AGENT.read(c)
                })?;
            Some(Admission::starting(
                self,
                &roots,
                &owner,
                shard.memory_directory(),
                agent,
            )?)
        } else {
            None
        };
        if let (Some(h), Some(expected)) = (header, metadata_revision) {
            use super::super::conversation_edit::EditContent;
            let sync = SyncEdit {
                messages: messages.into(),
                digest: digest.clone(),
                metadata_revision: expected,
                index: memory.as_ref().map(Admission::frozen_index).transpose()?,
            };
            return shard.paired_edit(EditContent::Sync(sync), Phase::SyncContent, |c| {
                if let Some(project)=&h.project_id { Project::select(c,&ProjectSelection::Existing(project.clone()))?; }
                let workspace=workspace(c,id,h.project_id.as_deref(),h.workspace_path.as_deref())?;
                c.execute("DELETE FROM conversation_worktrees WHERE conversation_id=?1 AND project_id IS NOT ?2",params![id,h.project_id])?;
                let updated = c.execute("UPDATE conversations SET title=?1,model=?2,provider=?3,workspace_mode=coalesce(?4,workspace_mode),research_mode=?5,
                    pinned=?6,archived=?7,project_id=?8,workspace_path=?9,created_at=?10,
                    updated_at=CASE WHEN julianday(updated_at)>julianday(?11) THEN updated_at ELSE ?11 END, metadata_revision=metadata_revision+1
                    WHERE id=?12 AND incarnation=?13 AND metadata_revision=?14",
                    params![h.title,h.model,h.provider,h.workspace_mode.map(|m|m.as_str()),h.research_mode,h.pinned,h.archived,h.project_id,workspace,h.created_at,h.updated_at,id,shard.incarnation,expected])?;
                ensure!(updated == 1, "Session metadata changed during sync");
                c.execute("INSERT INTO sync_publications VALUES(?1,?2,?3)",[id,&digest,&shard.incarnation])?;
                self.admit_session_workspace(c,id)
            });
        }
        let storage = memory.as_ref().map(boundary::Storage::Shard);
        shard.write_locked("apply sync messages", |w| {
            if !w.0.prepare_cached("SELECT 1 FROM sync_receipts WHERE digest=?1")?.exists([&digest])? {
                rows(w.0,id,messages,storage)?;
                w.0.execute("INSERT INTO sync_receipts(digest,content_revision) SELECT ?1,content_revision+1 FROM manifest",[&digest])?;
            }
            Ok(())
        })?;
        self.step(Phase::SyncContent)?;
        self.global()
            .write("acknowledge session sync", id, false, |c| {
                c.execute(
                    "INSERT INTO sync_publications VALUES(?1,?2,?3)",
                    [id, &digest, &shard.incarnation],
                )?;
                Ok(())
            })
    }
}

impl crate::db::DbManager {
    pub fn sync_conversation(
        &self,
        id: &str,
        header: Option<&SyncHeader>,
        messages: &[MessageRow],
    ) -> Result<()> {
        self.with_conn(|c| crate::db::write_tx(c, "sync conversation", super::super::WRITE_DEADLINE, |tx| {
            if let Some(h) = header {
                ensure!(h.id == id, "Wrong sync header");
                tx.execute("INSERT INTO conversations(id,title,model,provider,workspace_path,project_id,workspace_mode,research_mode,pinned,archived,created_at,updated_at,message_count)
                    VALUES(?1,?2,?3,?4,?5,?6,coalesce(?7,'chat'),?8,?9,?10,?11,?12,0)
                    ON CONFLICT(id) DO UPDATE SET title=excluded.title,model=excluded.model,provider=excluded.provider,workspace_path=excluded.workspace_path,
                    project_id=excluded.project_id,workspace_mode=coalesce(?7,conversations.workspace_mode),research_mode=excluded.research_mode,pinned=excluded.pinned,
                    archived=excluded.archived,created_at=excluded.created_at,updated_at=excluded.updated_at
                    WHERE julianday(excluded.updated_at)>julianday(conversations.updated_at)", params![id,h.title,h.model,h.provider,h.workspace_path,h.project_id,h.workspace_mode.map(|m|m.as_str()),h.research_mode,h.pinned,h.archived,h.created_at,h.updated_at])?;
            }
            rows(tx,id,messages,Some(boundary::Storage::Shared))?;
            tx.execute("UPDATE conversations SET message_count=(SELECT count(*) FROM messages WHERE conversation_id=?1) WHERE id=?1",[id])?;
            Ok(())
        }))?
    }
    pub fn sync_messages(&self, id: &str) -> Result<Vec<MessageRow>> {
        self.with_conn(|c| crate::db::message_repo::get_messages_by_conversation(c, id))?
    }
    pub fn sync_delete(&self, id: &str) -> Result<()> {
        self.with_conn(|c| crate::db::conversation_repo::delete_conversation(c, id))?
    }
}
impl Stores {
    pub fn sync_messages(&self, id: &str) -> Result<Vec<MessageRow>> {
        self.open_session(id)?.read(|r| r.messages())
    }
    pub fn sync_delete(&self, id: &str) -> Result<()> {
        if let Some(row) = self
            .global()
            .read_directory_row(id, |c| super::super::directory::one(c, id))?
        {
            self.open_listed(&row)?.delete()?;
        }
        Ok(())
    }
}

pub type SyncContents = (
    Vec<crate::db::conversation_repo::ConversationRow>,
    std::collections::HashMap<String, Vec<MessageRow>>,
);
impl Stores {
    pub fn sync_contents(&self) -> Result<SyncContents> {
        let mut conversations = Vec::new();
        let mut messages = std::collections::HashMap::new();
        for entry in self.global().list_sessions(Default::default())? {
            let rows = self.open_listed(&entry)?.read(|r| r.messages())?;
            if !rows.is_empty() {
                messages.insert(entry.conversation.id.clone(), rows);
            }
            conversations.push(entry.conversation);
        }
        Ok((conversations, messages))
    }
}
impl crate::db::DbManager {
    pub fn sync_contents(&self) -> Result<SyncContents> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let conversations = crate::db::conversation_repo::list_conversations(&tx)?;
            let mut messages = std::collections::HashMap::new();
            for entry in &conversations {
                let rows = crate::db::message_repo::get_messages_by_conversation(&tx, &entry.id)?;
                if !rows.is_empty() {
                    messages.insert(entry.id.clone(), rows);
                }
            }
            Ok((conversations, messages))
        })?
    }
}
