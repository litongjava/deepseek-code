//! Version 1 logical backup. Only the tables listed here belong to the wire format.
//! A transport result can commit before its running turn persists the tool call.
//! Such an mcp_output retains its scoped tool_use_id and carries occurrence_pending=true.
//! Once its tool_call is present in the snapshot the marker is omitted.
mod references;
mod source;

use super::{history::activity_time, layout, Stores};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{
    blob::Blob, types::ValueRef, Connection, DatabaseName, OpenFlags, OptionalExtension,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Write, path::Path};

const CHUNK: usize = 32 * 1024;

// (wire kind, source table, persisted key order, v1 fields). Messages carry their optional
// turn link and typed content inline. JSON-valued SQLite text retains its exact bytes.
const RECORDS: &[(&str, &str, &str, &str)] = &[
    ("turn", "execution_turns", "started_at,id", "id conversation_id source_turn_id started_at ended_at elapsed_ms status"),
    ("message", "messages", "sort_order,rowid", "id conversation_id role content thinking created_at is_compact_boundary sort_order"),
    ("tool_call", "tool_calls", "message_id,sort_order,rowid", "id message_id name input output is_error sort_order"),
    ("tool_origin", "tool_call_origins", "parent_tool_call_id,sequence,tool_call_id", "tool_call_id parent_tool_call_id sequence origin"),
    ("delivered_inputs", "tool_queued_inputs", "tool_call_id", "tool_call_id inputs_json"),
    ("tool_note", "tool_call_notes", "tool_call_id", "tool_call_id note"),
    ("tool_duration", "tool_call_timings", "tool_call_id", "tool_call_id duration_ms"),
    ("mcp_output", "mcp_tool_outputs", "tool_use_id", "conversation_id tool_use_id content_json metadata_json display_text is_error"),
    ("attachment", "attachments", "id", "id message_id file_name file_type mime_type file_size source"),
    ("diff", "code_diffs", "id", "id conversation_id message_id file_path original_content modified_content diff_text status applied_at created_at"),
    ("approval", "permission_approvals", "id", "id conversation_id message_id tool_name action risk_level status user_decision decision_reason created_at decided_at"),
    ("request_snapshot", "message_trajectory", "message_id", "message_id requests_json"),
    ("model_request", "model_requests", "id", "id message_id conversation_id request_number provider_id model response_model api_format status started_at_ms first_token_at_ms ended_at_ms total_duration_ms ttft_ms generation_ms throughput_tokens_per_second input_tokens output_tokens reasoning_tokens content_tokens total_tokens cache_read_tokens cache_write_tokens max_output_tokens effort finish_reason response_id timing_source error response_blocks_json usage_json"),
    ("request_blob", "request_blobs", "hash", "hash body"),
    ("agent", "session_agents", "conversation_id", "conversation_id definition"),
    ("approved_plan_boundary", "approved_plan_boundaries", "message_id", "message_id"),
    ("child_invocation", "child_invocations", "attempt_key", "attempt_key parent_message child_incarnation invocation"),
    ("compaction_cancellation", "compaction_cancellations", "id", "id reason"),
    ("compaction_checkpoint", "compaction_checkpoint", "fingerprint,request_key", "fingerprint request_key summary"),
    ("compaction_request", "compaction_request", "singleton", "singleton id keep target_message"),
    ("compaction_state", "compaction_state", "singleton", "singleton local_count"),
    ("delivery_generation", "delivery_generations", "delivery_id", "delivery_id turn_id"),
    ("delivery_receipt", "delivery_receipts", "delivery_id", "delivery_id digest effect_id kind"),
    ("file_read_stamp", "file_read_stamps", "path", "path modified_ns size digest"),
    ("first_chat_receipt", "first_chat_receipts", "message_id", "message_id digest"),
    ("local_setting", "peripheral_settings", "key", "key value"),
    ("message_metadata", "message_metadata", "message_id", "message_id value"),
    ("native_boundary", "native_boundaries", "message_id", "message_id value"),
    ("permission_authority", "session_permission_authority", "id", "id conversation_id source origin_id content_json"),
    ("removed_message_owner", "removed_message_owners", "id,role", "id role"),
    ("request_metadata", "request_metadata", "request_id", "request_id value"),
    ("sync_receipt", "sync_receipts", "digest", "digest content_revision metadata_revision"),

];

impl Stores {
    pub fn export_session(path: &Path, id: &str, output: impl Write) -> Result<()> {
        Self::export_session_interruptible(path, id, output, &|| Ok(()))
    }

    pub fn export_session_interruptible(
        path: &Path,
        id: &str,
        output: impl Write,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        layout::allow_entry()?;
        check()?;
        let key = layout::storage_key(id)?;
        let data_layout = crate::data_layout::DataLayout::from_root(&path);
        let locks = data_layout.lock_path("session");
        let _namespace = shared_lock(&locks.join(".directory"), check)?;
        let _key = shared_lock(&locks.join(&key), check)?;
        let shard = data_layout.sessions().join(&key);
        ensure!(
            shard.join("session.db").is_file()
                && !shard.join(super::conversation_edit::PENDING).try_exists()?,
            "Session unavailable"
        );
        let uuid = std::fs::read_to_string(locks.join(".initialized"))?;
        let global = data_layout.global_db();
        let _permit = layout::Permits::take(1);
        let mut stream = Stream {
            output,
            check,
            counts: BTreeMap::new(),
            digest: Sha256::new(),
        };
        const ATTEMPTS: usize = 8;
        for _ in 0..ATTEMPTS {
            let mut directory = source::Source::open(&global, check)?;
            // Pin the directory row before acquiring the shard snapshot. Large header
            // fields can then stream from that same row after the revision fence.
            let header = directory.db.transaction()?;
            layout::verify_export(&header, layout::GLOBAL)?;
            let store: String = header.query_row(
                "SELECT store_uuid FROM store_identity WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                store == uuid && layout::canonical_uuid(&store),
                "Lifecycle namespace identity changed"
            );
            let (rowid, revision, projected, updated, incarnation) = header.query_row(
                "SELECT rowid,metadata_revision,content_revision,updated_at,incarnation FROM conversations
                 WHERE id=?1 AND storage_key=?2 AND NOT EXISTS
                 (SELECT 1 FROM lifecycle WHERE storage_key=?2 AND quarantine_reason IS NULL)",
                [id, &key], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?,
                    activity_time(r.get_ref(3)?.as_str()?), r.get::<_,String>(4)?)))
                .optional()?.context("Session unavailable")?;
            let mut source = source::Source::open(&shard.join("session.db"), check)?;
            let tx = source.db.transaction()?;
            super::verify_manifest(&tx, &store, id, &incarnation)?;
            layout::verify_export(&tx, layout::SESSION)?;
            layout::integrity(&tx)?;
            let (content, count, activity) = tx.query_row(
                "SELECT content_revision,message_count,updated_at,last_turn_status FROM manifest",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        activity_time(r.get_ref(2)?.as_str()?),
                    ))
                },
            )?;
            let current: i64 = source::Source::open(&global, check)?.db.query_row(
                "SELECT metadata_revision FROM conversations WHERE id=?1 AND incarnation=?2",
                [id, &incarnation],
                |r| r.get(0),
            )?;
            if current != revision || projected > content {
                continue;
            }
            layout::callback(|| {
                let mut record = json!({"format":"deepseek-code.session","version":1,"session_id":id,
                        "content_revision":content,"metadata_revision":revision,"message_count":count});
                let columns = "incarnation title project_id workspace_path workspace_mode provider model archived pinned research_mode created_at";
                let (time_db, time_table, time_row) = if updated >= activity {
                    (&*header, "conversations", rowid)
                } else {
                    (&*tx, "manifest", 1)
                };
                stream.record(
                    &header,
                    "header",
                    "conversations",
                    columns,
                    rowid,
                    &mut record,
                    &[
                        (time_db, time_table, time_row, "updated_at", "updated_at"),
                        (&*tx, "manifest", 1, "last_turn_status", "status"),
                    ],
                )
            })?;
            drop(header);
            drop(directory);
            return layout::callback(|| {
                for &(kind, table, order, columns) in RECORDS {
                    stream.table(&tx, kind, table, order, columns)?;
                }
                stream.finish()
            });
        }
        anyhow::bail!(
            "Could not acquire a consistent session export snapshot after {ATTEMPTS} attempts"
        )
    }
}

fn shared_lock(path: &Path, check: &dyn Fn() -> Result<()>) -> Result<std::fs::File> {
    let file = std::fs::File::open(path).context("Session unavailable")?;
    loop {
        check()?;
        match file.try_lock_shared() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
        }
    }
}

struct Stream<'a, W> {
    check: &'a dyn Fn() -> Result<()>,
    output: W,
    counts: BTreeMap<&'static str, u64>,
    digest: Sha256,
}

impl<W: Write> Stream<'_, W> {
    fn line(&mut self, kind: &'static str, record: &mut Value) -> Result<()> {
        (self.check)()?;
        record["type"] = json!(kind);
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.output.write_all(&line)?;
        self.digest.update(&line);
        *self.counts.entry(kind).or_default() += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        (self.check)()?;
        let hash = format!("{:x}", std::mem::take(&mut self.digest).finalize());
        serde_json::to_writer(
            &mut self.output,
            &json!({"type":"end","counts":self.counts,"sha256":hash}),
        )?;
        self.output.write_all(b"\n")?;
        self.output.flush()?;
        Ok(())
    }

    fn table(
        &mut self,
        db: &Connection,
        kind: &'static str,
        table: &str,
        order: &str,
        columns: &str,
    ) -> Result<()> {
        // Rewind can leave unreferenced deduplicated headers behind.
        let filter = if kind == "request_blob" {
            references::index(db, self.check)?;
            "WHERE hash IN (SELECT hash FROM temp.export_refs)"
        } else {
            ""
        };
        let mut ids = db.prepare(&format!(
            "SELECT rowid FROM {table} {filter} ORDER BY {order}"
        ))?;
        for row in ids.query_map([], |r| r.get(0))? {
            let rowid = row?;
            let mut record = json!({});
            self.record(db, kind, table, columns, rowid, &mut record, &[])?;
        }
        Ok(())
    }

    fn record(
        &mut self,
        db: &Connection,
        kind: &'static str,
        table: &str,
        columns: &str,
        rowid: i64,
        record: &mut Value,
        extra: &[(&Connection, &str, i64, &str, &str)],
    ) -> Result<()> {
        let owner = format!("{kind}/{rowid}");
        let mut pending = Vec::new();
        let fields = columns
            .split_whitespace()
            .map(|field| (db, table, rowid, field, field));
        for (db, table, rowid, column, field) in fields.chain(extra.iter().copied()) {
            read_field(
                db,
                table,
                rowid,
                column,
                field,
                &owner,
                record,
                &mut pending,
                self.check,
            )?;
        }
        if kind == "message" {
            let (turn,typed): (Option<i64>,Option<i64>) = db.query_row(
                "SELECT mt.rowid,u.rowid FROM messages m LEFT JOIN message_turns mt ON mt.message_id=m.id
                 LEFT JOIN message_user_content u ON u.message_id=m.id WHERE m.rowid=?1",
                [rowid], |r| Ok((r.get(0)?,r.get(1)?)))?;
            for (source, row, column, field) in [
                ("message_turns", turn, "turn_id", "turn_id"),
                (
                    "message_user_content",
                    typed,
                    "content_json",
                    "user_content_json",
                ),
            ] {
                record[field] = Value::Null;
                if let Some(row) = row {
                    read_field(
                        db,
                        source,
                        row,
                        column,
                        field,
                        &owner,
                        record,
                        &mut pending,
                        self.check,
                    )?;
                }
            }
        }
        if kind == "mcp_output" && db.prepare_cached(
            "SELECT 1 FROM mcp_tool_outputs o WHERE o.rowid=?1
             AND NOT EXISTS (SELECT 1 FROM tool_calls c WHERE c.id=o.tool_use_id)
             AND EXISTS (SELECT 1 FROM execution_turns t JOIN message_turns m ON m.turn_id=t.id
                         WHERE t.status='running' AND substr(o.tool_use_id,1,length(m.message_id)+1)=m.message_id||':')"
        )?.exists([rowid])? {
            record["occurrence_pending"] = json!(true);
        }
        if kind == "request_blob" {
            let body = &record["body"];
            let digest = match body.as_str() {
                Some(text) => format!("{:x}", Sha256::digest(text.as_bytes())),
                None => body["sha256"]
                    .as_str()
                    .context("Invalid request blob body")?
                    .to_owned(),
            };
            ensure!(
                record["hash"] == digest,
                "Request blob digest mismatch for {}",
                record["hash"]
            );
        }
        self.line(kind, record)?;
        for (field, digest, blob) in pending {
            visit_chunks(&blob, |index, bytes| {
                self.line("chunk", &mut json!({
                "owner":owner,"field":field,"index":index,"sha256":digest,"data":STANDARD.encode(bytes)
            }))
            })?;
        }
        Ok(())
    }
}

fn visit_chunks(blob: &Blob<'_>, mut visit: impl FnMut(usize, &[u8]) -> Result<()>) -> Result<()> {
    let mut buf = vec![0; CHUNK];
    for (index, offset) in (0..blob.len()).step_by(CHUNK).enumerate() {
        let n = CHUNK.min(blob.len() - offset);
        blob.read_at_exact(&mut buf[..n], offset)?;
        visit(index, &buf[..n])?;
    }
    Ok(())
}

fn read_field<'a>(
    db: &'a Connection,
    table: &str,
    rowid: i64,
    column: &str,
    field: &str,
    owner: &str,
    record: &mut Value,
    pending: &mut Vec<(String, String, Blob<'a>)>,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    // Read the storage class first. SQLite can answer typeof from the record header.
    let sql = format!("SELECT typeof({column}),CASE WHEN typeof({column}) NOT IN ('text','blob') THEN {column} END FROM {table} WHERE rowid=?1");
    let mut stmt = db.prepare_cached(&sql)?;
    let mut rows = stmt.query([rowid])?;
    let row = rows.next()?.context("Missing export row")?;
    record[field] = match row.get_ref(1)? {
        ValueRef::Integer(n)
            if matches!(
                field,
                "is_compact_boundary" | "archived" | "pinned" | "research_mode" | "is_error"
            ) =>
        {
            json!(n != 0)
        }
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(n) => {
            ensure!(n.is_finite(), "Non-finite SQLite number");
            json!(n)
        }
        ValueRef::Text(s) => json!(std::str::from_utf8(s)?),
        ValueRef::Blob(b) => json!({"encoding":"base64","data":STANDARD.encode(b)}),
        ValueRef::Null => match row.get_ref(0)?.as_str()? {
            "null" => Value::Null,
            ty => {
                let blob = db.blob_open(DatabaseName::Main, table, column, rowid, true)?;
                if blob.len() <= CHUNK {
                    let mut bytes = vec![0; blob.len()];
                    blob.read_at_exact(&mut bytes, 0)?;
                    record[field] = if ty == "text" {
                        json!(String::from_utf8(bytes)?)
                    } else {
                        json!({"encoding":"base64", "data":STANDARD.encode(bytes)})
                    };
                    return Ok(());
                }
                let mut hash = Sha256::new();
                visit_chunks(&blob, |_, bytes| {
                    check()?;
                    hash.update(bytes);
                    Ok(())
                })?;
                let hash = format!("{:x}", hash.finalize());
                let descriptor = json!({"owner":owner,"encoding":if ty=="text" {"utf8"} else {"binary"},
                    "chunks":blob.len().div_ceil(CHUNK),"bytes":blob.len(),"sha256":hash});
                pending.push((field.to_owned(), hash, blob));
                descriptor
            }
        },
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn chunks_binary_fields_and_releases_snapshot_on_sink_error() -> Result<()> {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = tempfile::tempdir()?;
        let stores = Stores::open(root.path())?;
        let session = stores.create("export", Default::default())?;
        let bytes: Vec<_> = (0..CHUNK + 1).map(|i| i as u8).collect();
        session.write(|w| {
            w.0.execute(
                "INSERT INTO file_read_stamps VALUES(?1,'0','0',x'00ff')",
                [&bytes],
            )?;
            Ok(())
        })?;
        let mut out = Vec::new();
        Stores::export_session(root.path(), "export", &mut out)?;
        let rows: Vec<Value> = serde_json::Deserializer::from_slice(&out)
            .into_iter()
            .collect::<Result<_, _>>()?;
        let file = rows
            .iter()
            .find(|r| r["type"] == "file_read_stamp")
            .unwrap();
        assert_eq!(
            file["path"]["sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
        assert_eq!(file["digest"], json!({"encoding":"base64","data":"AP8="}));
        assert_eq!(rows.last().unwrap()["counts"]["chunk"], 2);
        let err = Stores::export_session(root.path(), "export", Broken).unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        session.delete()?;
        Ok(())
    }
}
