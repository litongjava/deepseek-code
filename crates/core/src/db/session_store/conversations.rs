use super::{Branch, CreateOptions, Stores};
use crate::db::{conversation_repo as conversations, message_repo as messages, transcript};
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde_json::Value;

pub enum Rewind<'a> {
    From(&'a str),
    Tail(i64),
}

fn validate_chat(c: &rusqlite::Connection, id: &str) -> Result<()> {
    if let Some(row) = conversations::conversation_metadata(c, id)? {
        anyhow::ensure!(
            row.workspace_mode != "code"
                || row.project_id.as_deref().is_some_and(|s| !s.is_empty()),
            "Select a project before sending messages in Code mode"
        );
        crate::native_engine::workspace::conversation_project(c, id)?;
    }
    Ok(())
}

pub(super) fn first_chat_options(
    model: &str,
    content: Option<&Value>,
    origin: &str,
) -> Result<CreateOptions> {
    let first_message = content
        .filter(|v| v.as_str() != Some(""))
        .map(|v| -> Result<_> {
            let (text, normalized) = messages::prepare_user_message(v)?;
            Ok(super::FirstMessage {
                origin: origin.to_owned(),
                text,
                typed: normalized.is_array().then_some(normalized),
            })
        })
        .transpose()?;
    Ok(CreateOptions {
        model: Some(model.into()),
        first_message,
        ..Default::default()
    })
}

impl Stores {
    pub fn validate_chat_project(&self, id: &str) -> Result<()> {
        self.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            validate_chat(&tx, id)
        })
    }
    pub fn admit_chat_message(
        &self,
        id: &str,
        model: &str,
        content: Option<&Value>,
        origin: &str,
    ) -> Result<()> {
        let content = content.filter(|v| v.as_str() != Some(""));
        let row = self.global().read_directory_row(id, |c| {
            let tx = c.unchecked_transaction()?;
            validate_chat(&tx, id)?;
            super::directory::one(&tx, id)
        })?;
        match row {
            Some(entry) => {
                let shard = self.open_listed(&entry)?;
                if let Some(content) = content {
                    let message = shard.new_message_id();
                    shard.admit_user(&message, content, origin)?;
                }
            }
            None => {
                self.create(id, first_chat_options(model, content, origin)?)?;
            }
        }
        Ok(())
    }

    pub fn create_conversation(
        &self,
        id: &str,
        title: Option<String>,
        model: Option<String>,
        research: bool,
        mode: conversations::WorkspaceMode,
    ) -> Result<()> {
        self.create(
            id,
            CreateOptions {
                title,
                model,
                research,
                workspace_mode: Some(mode.as_str().into()),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    pub fn conversation_detail(
        &self,
        id: &str,
    ) -> Result<(Option<conversations::ConversationRow>, Vec<Value>)> {
        let Some(selected) = self.global().read_directory_row(id, |c| super::directory::one(c, id))? else {
            return Ok((None, Vec::new()));
        };
        let shard = self.open_listed(&selected)?;
        let _guard = shard.hold_incarnation()?;
        loop {
            // Combined edits take the exclusive lifecycle lock. Metadata-only
            // edits and projection updates can still run alongside this reader.
            let row = self.global().read(|c| super::directory::one(c, id))?
                .context("Conversation retired")?;
            let (messages, projection) = shard.read(|r| Ok((r.enriched_messages()?, r.projection()?)))?;
            let unchanged = self.global().read(|c| Ok(c.query_row(
                "SELECT metadata_revision=?2 AND content_revision=?3 FROM conversations WHERE id=?1",
                rusqlite::params![id, row.metadata_revision, row.content_revision], |r| r.get::<_, bool>(0)
            )?))?;
            if !unchanged { continue; }
            let mut conversation = row.conversation;
            if projection.revision > row.content_revision {
                conversation.message_count = projection.count;
                conversation.title = super::directory::display_title(row.stored_title, projection.first_prompt.as_deref());
                conversation.updated_at = super::history::latest_activity(&conversation.updated_at, &projection.activity).to_owned();
            }
            return Ok((Some(conversation), messages));
        }
    }

    pub fn conversation_metadata(
        &self,
        id: &str,
    ) -> Result<Option<conversations::ConversationRow>> {
        self.global().conversation(id)
    }

    pub fn rewind_messages(&self, id: &str, boundary: Rewind<'_>) -> Result<Vec<Value>> {
        let session = self.open_session(id)?;
        session.write(|w| {
            let first=match boundary {
                Rewind::From(m)=>w.0.query_row("SELECT id FROM messages WHERE id=?1",[m],|r|r.get::<_,String>(0)).optional()?,
                Rewind::Tail(n)=>w.0.query_row("SELECT id FROM (SELECT id,sort_order FROM messages ORDER BY sort_order DESC,rowid DESC LIMIT ?1) ORDER BY sort_order LIMIT 1",[n],|r|r.get::<_,String>(0)).optional()?,
            };
            if let Some(m)=first {w.rewind_from(&m)?;}
            transcript::enriched_messages(w.0,id)
        })
    }

    pub fn branch_conversation(
        &self,
        id: &str,
        message: Option<&str>,
        index: Option<usize>,
    ) -> Result<String> {
        let destination = uuid::Uuid::new_v4().to_string();
        let boundary = if message.is_none() && index.is_none() {
            Branch::Full
        } else {
            Branch::Through { message, index }
        };
        self.open_session(id)?.branch(&destination, boundary)?;
        Ok(destination)
    }
}
impl crate::db::DbManager {
    pub fn validate_chat_project(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            validate_chat(&tx, id)
        })?
    }
    pub fn admit_chat_message(
        &self,
        id: &str,
        model: &str,
        content: Option<&Value>,
        origin: &str,
    ) -> Result<()> {
        self.with_conn(|c|crate::db::write_tx(c,"admit chat user message",super::WRITE_DEADLINE,|tx|{
            validate_chat(tx,id)?;
            let now=chrono::Utc::now().to_rfc3339();
            if conversations::conversation_metadata(tx,id)?.is_none(){
                conversations::insert_conversation(tx,id,None,Some(model),None,None,None,false,false,false,&now,&now,0)?;
            }
            if let Some(content)=content.filter(|v|v.as_str()!=Some("")) {
                let order=tx.query_row("SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",[id],|r|r.get(0))?;
                let message = uuid::Uuid::new_v4().to_string();
                messages::save_user_message(tx,&message,id,content,&now,order)?;
                super::state::set_local(tx,&format!("message-origin:{message}"),&serde_json::json!({"promptOrigin":origin}).to_string())?;
            }
            tx.execute("UPDATE conversations SET message_count=(SELECT count(*) FROM messages WHERE conversation_id=?1),updated_at=?2 WHERE id=?1",[id,&now])?;
            Ok(())
        }))?
    }

    pub fn create_conversation(
        &self,
        id: &str,
        title: Option<String>,
        model: Option<String>,
        research: bool,
        mode: conversations::WorkspaceMode,
    ) -> Result<()> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "create conversation", super::WRITE_DEADLINE, |tx| {
                let now = chrono::Utc::now().to_rfc3339();
                conversations::insert_conversation(
                    tx,
                    id,
                    title.as_deref(),
                    model.as_deref(),
                    None,
                    None,
                    None,
                    research,
                    false,
                    false,
                    &now,
                    &now,
                    0,
                )?;
                conversations::set_workspace_mode(tx, id, mode.as_str())
            })
        })?
    }
    pub fn conversation_detail(
        &self,
        id: &str,
    ) -> Result<(Option<conversations::ConversationRow>, Vec<Value>)> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            Ok((
                conversations::get_conversation(&tx, id)?,
                transcript::enriched_messages(&tx, id)?,
            ))
        })?
    }
    pub fn conversation_metadata(
        &self,
        id: &str,
    ) -> Result<Option<conversations::ConversationRow>> {
        self.with_conn(|c| conversations::get_conversation(c, id))?
    }
    pub fn rewind_messages(&self, id: &str, boundary: Rewind<'_>) -> Result<Vec<Value>> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "rewind conversation", super::WRITE_DEADLINE, |tx| {
                match boundary {
                    Rewind::From(mid) => {
                        if let Some(m) =
                            messages::get_message(tx, mid)?.filter(|m| m.conversation_id == id)
                        {
                            messages::delete_messages_from(tx, id, m.sort_order)?;
                        }
                    }
                    Rewind::Tail(count) => messages::delete_messages_tail(tx, id, count)?,
                }
                transcript::enriched_messages(tx, id)
            })
        })?
    }
    pub fn branch_conversation(
        &self,
        id: &str,
        message: Option<&str>,
        index: Option<usize>,
    ) -> Result<String> {
        self.with_conn(|c| conversations::branch_conversation(c, id, message, index))?
    }
}

impl super::SessionDb {
    pub(super) fn admit_edited_workspace(
        &self,
        tx: &rusqlite::Connection,
        body: &Value,
    ) -> Result<Vec<std::fs::File>> {
        if body.get("project_id").is_some() || body.get("workspace_path").is_some() {
            return self.stores.admit_session_workspace(tx, self.id());
        }
        Ok(Vec::new())
    }

    /// Serialize a Desktop edit with retirement and provider publication.
    pub fn edit_conversation(&self, body: &Value) -> Result<()> {
        // Pin, archive and title edits only serialize on the G writer. Keep
        // the incarnation stable alongside a running execution's shared lock.
        let exclusive = [
            "messages",
            "workspace_mode",
            "project_id",
            "model",
            "workspace_path",
        ]
        .iter()
        .any(|key| body.get(key).is_some());
        let _lock = self.stores.key_lock(&self.key, exclusive, false)?;
        self.check_directory()?;
        let has_metadata=["workspace_mode","pinned","archived","project_id","title","model","workspace_path"].iter().any(|k|body.get(k).is_some());
        if let Some(mode)=body.get("workspace_mode") {
            serde_json::from_value::<conversations::WorkspaceMode>(mode.clone())?;
        }
        if let Some(project)=body.get("project_id").and_then(Value::as_str) {
            self.stores.global().read(|c| crate::db::project_repo::require_trusted(c,project))?;
        }
        let messages = body.get("messages").and_then(Value::as_array);
        match (messages, has_metadata) {
            (Some(messages), true) => self.edit_history_and_metadata(messages, body),
            (Some(messages), false) => self.write_locked("replace conversation history", |w| {
                w.replace_messages(messages)
            }),
            (None, true) => {
                self.stores
                    .global()
                    .write("edit conversation", self.id(), false, |tx| {
                        edit_metadata(tx, self.id(), body)?;
                        self.admit_edited_workspace(tx, body)
                    }).map(drop)
            }
            (None, false) => Ok(()),
        }
    }
}

pub(super) fn edit_metadata(conn: &rusqlite::Connection, id: &str, body: &Value) -> Result<()> {
    let prior: String = conn.query_row(
        "SELECT updated_at FROM conversations WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if let Some(mode) = body.get("workspace_mode") {
        let mode: crate::db::conversation_repo::WorkspaceMode =
            serde_json::from_value(mode.clone())?;
        crate::db::conversation_repo::set_workspace_mode(conn, id, mode.as_str())?;
        crate::db::conversation_repo::update_conversation_timestamp(
            conn,
            id,
            &chrono::Utc::now().to_rfc3339(),
        )?;
    }
    if let Some(pinned) = body.get("pinned").and_then(|v| v.as_bool()) {
        crate::db::conversation_repo::pin_conversation(conn, id, pinned)?;
    }
    if let Some(archived) = body.get("archived").and_then(|v| v.as_bool()) {
        crate::db::conversation_repo::archive_conversation(conn, id, archived)?;
    }
    if let Some(project_id) = body.get("project_id") {
        if let Some(project_id) = project_id.as_str() {
            let project = crate::db::project_repo::require_trusted(conn, project_id)?;
            conn.execute("DELETE FROM conversation_worktrees WHERE conversation_id = ?1 AND project_id != ?2", rusqlite::params![id, project_id])?;
            conn.execute("UPDATE conversations SET project_id = ?1, workspace_path = COALESCE((SELECT path FROM conversation_worktrees WHERE conversation_id = ?4), ?2), updated_at = ?3 WHERE id = ?4",
                        rusqlite::params![project_id, project.workspace_path, chrono::Utc::now().to_rfc3339(), id])?;
        } else if project_id.is_null() {
            conn.execute(
                "DELETE FROM conversation_worktrees WHERE conversation_id = ?1",
                rusqlite::params![id],
            )?;
            conn.execute(
                "UPDATE conversations SET project_id = NULL, workspace_path = NULL WHERE id = ?1",
                rusqlite::params![id],
            )?;
        }
    }
    if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
        conn.execute(
            "UPDATE conversations SET title = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![title, chrono::Utc::now().to_rfc3339(), id],
        )?;
    }
    if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
        conn.execute(
            "UPDATE conversations SET model = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![model, chrono::Utc::now().to_rfc3339(), id],
        )?;
    }
    if let Some(workspace) = body.get("workspace_path").and_then(|v| v.as_str()) {
        conn.execute(
            "UPDATE conversations SET workspace_path = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![workspace, chrono::Utc::now().to_rfc3339(), id],
        )?;
    }

    conn.execute("UPDATE conversations SET metadata_revision=metadata_revision+1,updated_at=CASE WHEN julianday(updated_at)>=julianday(?2) THEN updated_at ELSE ?2 END WHERE id=?1",rusqlite::params![id,prior])?;
    Ok(())
}
