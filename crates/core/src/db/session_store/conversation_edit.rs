//! G decides a combined edit after S has validated the full replacement. The
//! redo record stays beside S until its transaction commits durably. Every
//! subsequent key checkout finishes an interrupted decision before admitting a reader.
use super::{layout, state, Phase, SessionDb, SessionWrite, Stores, WRITE_DEADLINE};
use super::lifecycle::sync::SyncEdit;
use std::borrow::Cow;
use anyhow::{ensure, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::Path};

pub(super) const PENDING: &str = "pending-conversation-edit.json";
const RECEIPT: &str = "conversation.edit-receipt";

#[derive(Serialize, Deserialize)]
struct PendingEdit<'a> {
    id: std::borrow::Cow<'a, str>,
    incarnation: &'a str,
    operation: &'a str,
    #[serde(flatten)]
    content: EditContent<'a>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EditContent<'a> {
    Messages(Cow<'a, [Value]>),
    Sync(SyncEdit<'a>),
}

impl EditContent<'_> {
    fn apply(&self, w: &SessionWrite<'_>) -> Result<()> {
        match self {
            Self::Messages(messages) => w.replace_messages(messages),
            Self::Sync(sync) => sync.apply(w),
        }
    }
}

impl SessionDb {
    pub(super) fn edit_history_and_metadata(&self, messages: &[Value], body: &Value) -> Result<()> {
        self.paired_edit(
            EditContent::Messages(messages.into()),
            Phase::EditMetadata,
            |g| {
                super::conversations::edit_metadata(g, self.id(), body)?;
                self.admit_edited_workspace(g, body)
            },
        )
    }

    pub(super) fn paired_edit(
        &self,
        content: EditContent<'_>,
        phase: Phase,
        mut publish: impl FnMut(&rusqlite::Connection) -> Result<Vec<fs::File>>,
    ) -> Result<()> {
        let operation = uuid::Uuid::new_v4().to_string();
        let dir = self.stores.sessions().join(&self.key);
        let path = dir.join(PENDING);
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        let edit = PendingEdit {
            id: self.id().into(),
            incarnation: self.incarnation(),
            operation: &operation,
            content,
        };
        serde_json::to_writer(&mut file, &edit)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&path)?;
        layout::sync_dir(&dir)?;
        self.write_locked_with_commit(
            "replace conversation history",
            WRITE_DEADLINE,
            true,
            |w| {
                edit.content.apply(w)?;
                state::set_local(w.0, RECEIPT, &operation)
            },
            |tx| {
                self.stores.global().write("edit conversation", self.id(), true, |g| {
                    let decided: Option<String> = g.query_row(
                        "SELECT operation FROM conversation_edit_decisions WHERE incarnation=?1",
                        [self.incarnation()], |r| r.get(0)
                    ).optional()?;
                    if decided.as_deref() != Some(&operation) {
                        let locks = publish(g)?;
                        g.execute(
                            "INSERT INTO conversation_edit_decisions(incarnation,operation) VALUES(?1,?2)
                             ON CONFLICT(incarnation) DO UPDATE SET operation=excluded.operation",
                            [self.incarnation(), &operation],
                        )?;
                        return Ok(locks);
                    }
                    Ok(Vec::new())
                })?;
                self.stores.step(phase)?;
                Ok(tx.commit()?)
            },
        )?;
        fs::remove_file(&path)?;
        layout::sync_dir(&dir)
    }
}

impl Stores {
    /// Caller holds the exclusive lifecycle lock. A staged file alone carries no authority.
    pub(super) fn recover_conversation_edit(&self, key: &str, path: &Path) -> Result<()> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let edit: PendingEdit = serde_json::from_slice(&bytes)?;
        ensure!(
            layout::storage_key(&edit.id)? == key,
            "Edit journal belongs to another session"
        );
        let committed: bool = self.global().read(|g| {
            Ok(g.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_edit_decisions e JOIN conversations c USING(incarnation)
                 WHERE c.id=?1 AND e.incarnation=?2 AND e.operation=?3)",
                [edit.id.as_ref(), edit.incarnation, edit.operation],
                |r| r.get(0),
            )?)
        })?;
        if committed {
            let shard = SessionDb {
                stores: self.clone(),
                id: edit.id.into(),
                incarnation: edit.incarnation.into(),
                key: key.into(),
            };
            shard.write_locked_with_commit(
                "recover conversation edit",
                WRITE_DEADLINE,
                true,
                |w| {
                    if state::local(w.0, RECEIPT)?.as_deref() != Some(edit.operation) {
                        edit.content.apply(w)?;
                        state::set_local(w.0, RECEIPT, edit.operation)?;
                    }
                    Ok(())
                },
                |tx| Ok(tx.commit()?),
            )?;
        }
        fs::remove_file(path)?;
        layout::sync_dir(path.parent().expect("session directory"))
    }
}
