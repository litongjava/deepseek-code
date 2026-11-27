//! Every new context window is sealed in the transaction that inserts its first row.
use super::*;
use crate::services::memory_files::prompt::{Admission, IndexState};

pub(crate) enum Kind {
    Compaction,
    ApprovedPlan,
    Synced,
}

#[derive(Clone, Copy)]
pub(crate) enum Storage<'a> {
    Shard(&'a Admission),
    Frozen(&'a IndexState),
    Shared,
}

pub(crate) fn create(
    tx: &Connection,
    session: &str,
    order: i64,
    content: &str,
    kind: Kind,
    storage: Storage<'_>,
) -> Result<String> {
    let id = match storage {
        Storage::Shard(_) | Storage::Frozen(_) => {
            crate::db::transcript::MessageIds::for_shard(tx)?.issue()
        }
        Storage::Shared => uuid::Uuid::new_v4().to_string(),
    };
    crate::db::message_repo::insert_message(
        tx,
        &id,
        session,
        "user",
        content,
        None,
        &chrono::Utc::now().to_rfc3339(),
        false,
        order,
    )?;
    seal(tx, session, &id, kind, storage)?;
    Ok(id)
}

/// Sync preserves the remote row ID and can promote an existing user row.
pub(crate) fn seal(
    tx: &Connection,
    session: &str,
    id: &str,
    kind: Kind,
    storage: Storage<'_>,
) -> Result<()> {
    ensure!(tx.execute(
        "UPDATE messages SET is_compact_boundary=1 WHERE id=?1 AND conversation_id=?2 AND role='user'",
        [id, session],
    )? == 1, "Invalid boundary owner or role");
    let origin = match kind {
        Kind::Compaction => "compaction",
        Kind::ApprovedPlan => "approved_plan",
        Kind::Synced => "sync",
    };
    let metadata = serde_json::json!({"promptOrigin":origin}).to_string();
    match storage {
        Storage::Shard(_) | Storage::Frozen(_) => {
            if matches!(kind, Kind::Synced) {
                tx.execute("UPDATE message_metadata SET value=json_set(value,'$.beforeSyncBoundary',json_extract(value,'$.promptOrigin')) WHERE message_id=?1",[id])?;
            }
            tx.execute(
                "INSERT INTO message_metadata VALUES(?1,?2) ON CONFLICT(message_id) DO UPDATE SET value=json_patch(message_metadata.value,excluded.value)",
                [id, &metadata],
            )?;
            match storage {
                Storage::Shard(memory) => memory.freeze(tx, Some(id))?,
                Storage::Frozen(index) => index.seal(tx, Some(id))?,
                Storage::Shared => unreachable!(),
            }
        }
        Storage::Shared => {
            // The compatibility database has no file-memory scope or metadata table.
            super::state::set_local(tx, &format!("message-origin:{id}"), &metadata)?;
            crate::services::memory_files::prompt::freeze_empty_boundary(tx, id)?;
        }
    }
    Ok(())
}

/// A partial summary of the suffix keeps its earlier rows. The branch has a
/// new memory epoch even though its transcript still starts at the old cut.
pub(super) fn freeze_retained_prefix(tx: &Connection, memory: &Admission) -> Result<()> {
    let cut: Option<String> = tx.query_row("SELECT id FROM messages WHERE is_compact_boundary ORDER BY sort_order DESC,rowid DESC LIMIT 1",[],|r|r.get(0)).optional()?;
    memory.frozen_index()?.seal(tx, cut.as_deref())
}

pub(super) fn demote(tx: &Connection, id: &str, shared: bool) -> Result<()> {
    tx.execute(
        "UPDATE messages SET is_compact_boundary=0 WHERE id=?1",
        [id],
    )?;
    if shared {
        tx.execute(
            "DELETE FROM peripheral_settings WHERE key='compaction.native.'||?1",
            [id],
        )?;
    } else {
        tx.execute("DELETE FROM native_boundaries WHERE message_id=?1", [id])?;
        tx.execute("UPDATE message_metadata SET value=json_remove(json_set(value,'$.promptOrigin',
            coalesce(json_extract(value,'$.beforeSyncBoundary'),'sync')),'$.beforeSyncBoundary') WHERE message_id=?1", [id])?;
    }
    Ok(())
}
