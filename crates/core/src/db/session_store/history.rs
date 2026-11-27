//! Session-bound repository adapters. A callback cannot carry its connection
//! into another store or outlive the lifecycle lock.
use super::{GlobalDb, SessionDb, SessionRead, SessionWrite};
use crate::db::{diff_repo, message_repo as messages, permission_repo, turn_repo};
use crate::native_engine::mcp_output::McpOutput;
use crate::services::{input_queue, permission_authority};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub revision: i64,
    pub count: i64,
    pub activity: String,
    pub status: Option<String>,
    pub first_prompt: Option<String>,
    pub latest_prompt: Option<String>,
    pub transcript_bytes: i64,
    pub git_branch: Option<String>,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

pub(super) fn projection(c: &Connection) -> Result<Projection> {
    Ok(c.query_row("SELECT content_revision,message_count,updated_at,last_turn_status,
        first_prompt,latest_prompt,transcript_bytes,(SELECT value FROM peripheral_settings WHERE key='git.branch'),
        request_count,CAST(input_tokens AS INTEGER),CAST(output_tokens AS INTEGER),CAST(total_tokens AS INTEGER)
        FROM manifest JOIN projection_state USING(singleton) WHERE singleton=1", [], Projection::from_row)?)
}

// SQLite clamps decimal TEXT to the signed column range on the final CAST.
// Keep the exact totals locally so deleting a large request can undo a clamp.
pub(super) fn flush_summary(c: &Connection) -> Result<()> {
    let (dirty, history_dirty): (bool, bool) = c.query_row(
        "SELECT prompts_dirty,history_edit IS NOT NULL FROM projection_state WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if dirty {
        let first: Option<String> = c.query_row("SELECT content FROM messages WHERE role='user' AND content<>'' ORDER BY sort_order,rowid LIMIT 1", [], |r|r.get(0)).optional()?;
        let latest: Option<String> = c
            .query_row(
                "SELECT content FROM messages WHERE role='user' ORDER BY created_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        c.execute(
            "UPDATE projection_state SET first_prompt=?1,latest_prompt=?2,
            prompts_dirty=0,history_edit=NULL WHERE singleton=1",
            params![
                first
                    .as_deref()
                    .and_then(crate::db::conversation_repo::prompt_title),
                latest
                    .as_deref()
                    .map(|text| text.lines().next().unwrap_or_default())
            ],
        )?;
    } else if history_dirty {
        c.execute(
            "UPDATE projection_state SET history_edit=NULL WHERE singleton=1",
            [],
        )?;
    }
    let mut stmt = c.prepare(
        "SELECT direction,coalesce(input,0),coalesce(output,0),total FROM request_metric_deltas",
    )?;
    let mut rows = stmt.query([])?;
    let mut delta = [0i128; 4];
    let mut changed = false;
    while let Some(r) = rows.next()? {
        let sign = i128::from(r.get::<_, i64>(0)?);
        let input = i128::from(r.get::<_, i64>(1)?);
        let output = i128::from(r.get::<_, i64>(2)?);
        let total = r
            .get::<_, Option<i64>>(3)?
            .map(i128::from)
            .unwrap_or(input + output);
        for (sum, value) in delta.iter_mut().zip([1, input, output, total]) {
            *sum += sign * value;
        }
        changed = true;
    }
    if changed {
        let stored: [String; 3] = c.query_row("SELECT input_tokens,output_tokens,total_tokens FROM projection_state WHERE singleton=1", [], |r|Ok([r.get(0)?,r.get(1)?,r.get(2)?]))?;
        for (sum, prior) in delta[1..].iter_mut().zip(stored) {
            *sum += prior.parse::<i128>()?;
        }
        c.execute("UPDATE projection_state SET request_count=request_count+?1,input_tokens=?2,output_tokens=?3,total_tokens=?4 WHERE singleton=1",
            params![i64::try_from(delta[0])?,delta[1].to_string(),delta[2].to_string(),delta[3].to_string()])?;
        c.execute("DELETE FROM request_metric_deltas", [])?;
    }
    Ok(())
}

impl Projection {
    pub(super) fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Projection {
            revision: r.get(0)?,
            count: r.get(1)?,
            activity: r.get(2)?,
            status: r.get(3)?,
            first_prompt: r
                .get::<_, Option<String>>(4)?
                .as_deref()
                .and_then(crate::db::conversation_repo::prompt_title),
            latest_prompt: r.get(5)?,
            transcript_bytes: r.get(6)?,
            git_branch: r.get(7)?,
            request_count: r.get(8)?,
            input_tokens: r.get(9)?,
            output_tokens: r.get(10)?,
            total_tokens: r.get(11)?,
        })
    }
}

// Compare instants including offsets and sub-millisecond precision. Keep the
// original representation of the winning timestamp (including imported text).
pub(super) fn activity_time(value: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(value).ok()
}

pub(super) fn latest_activity<'a>(existing: &'a str, incoming: &'a str) -> &'a str {
    if activity_time(existing) >= activity_time(incoming) {
        existing
    } else {
        incoming
    }
}

pub(super) fn advance(c: &Connection) -> Result<()> {
    flush_summary(c)?;
    let existing: String = c.query_row(
        "SELECT updated_at FROM manifest WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    c.execute(
        "UPDATE manifest SET updated_at=?1,content_revision=content_revision+1,
        message_count=(SELECT count(*) FROM messages),
        last_turn_status=(SELECT status FROM execution_turns ORDER BY started_at DESC,rowid DESC LIMIT 1)
        WHERE singleton=1",
        [latest_activity(&existing, &chrono::Utc::now().to_rfc3339())],
    )?;
    Ok(())
}

impl GlobalDb {
    // Projection is secondary. Never wait for the G writer after a local commit.
    pub(super) fn project(&self, id: &str, incarnation: &str, p: &Projection) -> Result<()> {
        self.checkout(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, true, |c| {
            c.pragma_update(None, "synchronous", "NORMAL")?;
            c.busy_timeout(std::time::Duration::ZERO)?;
            let result = (|| -> Result<()> {
                let tx = rusqlite::Transaction::new_unchecked(
                    c,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let current = tx
                    .query_row(
                        "SELECT updated_at,content_revision FROM conversations WHERE id=?1 AND incarnation=?2",
                        params![id, incarnation],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                let Some((existing, revision)) = current else {
                    return Ok(());
                };
                let activity = latest_activity(&existing, &p.activity);
                if p.revision <= revision {
                    if activity != existing {
                        tx.execute(
                            "UPDATE conversations SET updated_at=?1 WHERE id=?2 AND incarnation=?3",
                            params![activity, id, incarnation],
                        )?;
                    }
                    tx.commit()?;
                    return Ok(());
                }
                tx.execute(
                    "UPDATE conversations SET content_revision=?1,message_count=?2,updated_at=?3,
                last_turn_status=?4,first_prompt=?5,latest_prompt=?6,transcript_bytes=?7, git_branch=?10, request_count=?11,input_tokens=?12,output_tokens=?13,total_tokens=?14
                WHERE id=?8 AND incarnation=?9 AND content_revision<?1",
                    params![
                        p.revision,
                        p.count,
                        activity,
                        p.status,
                        p.first_prompt,
                        p.latest_prompt,
                        p.transcript_bytes,
                        id,
                        incarnation, p.git_branch,p.request_count,p.input_tokens,p.output_tokens,p.total_tokens
                    ],
                )?;
                tx.commit()?;
                Ok(())
            })();
            c.busy_timeout(std::time::Duration::from_secs(5))?;
            result?;
            Ok(())
        })
    }
}

fn stream_occurrence_id(message: &str, provider_id: &str) -> String {
    format!("{message}:{provider_id}")
}

impl SessionDb {
    pub fn context_search(&self, input: Value) -> Result<String> {
        crate::tools::context_search::search(&self.incarnation, input, |selection| {
            self.read(|r| crate::db::context_search_repo::read_snapshot(r.0, &self.id, selection))
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Save to the same occurrence that a later assistant stream flush will address.
    pub fn save_mcp(
        &self,
        message: &str,
        provider_id: &str,
        transport: &Value,
        failed: bool,
    ) -> Result<String> {
        let output = McpOutput::from_transport(transport)?;
        let display = output.display();
        self.save_mcp_output(message, provider_id, &output, &display, failed)?;
        Ok(display)
    }
    pub(super) fn save_mcp_output(
        &self,
        message: &str,
        provider_id: &str,
        output: &McpOutput,
        display: &str,
        failed: bool,
    ) -> Result<()> {
        let id = stream_occurrence_id(message, provider_id);
        self.write_named("save MCP output", |tx| {
            output.save(tx.0, &self.id, &id, display, failed)
        })
    }
}

impl SessionRead<'_> {
    pub fn enriched_messages(&self) -> Result<Vec<Value>> {
        let id: String =
            self.0
                .query_row("SELECT id FROM manifest WHERE singleton=1", [], |r| {
                    r.get(0)
                })?;
        crate::db::transcript::enriched_messages(self.0, &id)
    }
    pub fn last_assistant_report(&self) -> Result<Option<String>> {
        Ok(self.0.query_row("SELECT content FROM messages WHERE role='assistant' AND content<>'' ORDER BY sort_order DESC,rowid DESC LIMIT 1", [], |row| row.get(0)).optional()?)
    }

    pub fn messages(&self) -> Result<Vec<messages::MessageRow>> {
        let id: String = self
            .0
            .query_row("SELECT id FROM manifest", [], |r| r.get(0))?;
        messages::get_messages_by_conversation(self.0, &id)
    }
    pub fn projection(&self) -> Result<Projection> {
        projection(self.0)
    }
    pub fn turn(&self, id: &str) -> Result<Option<turn_repo::TurnRecord>> {
        Ok(turn_repo::get_turn(self.0, id)?)
    }
    pub fn message_turn(&self, message: &str) -> Result<Option<turn_repo::TurnRecord>> {
        Ok(turn_repo::get_message_turn(self.0, message)?)
    }

    pub fn recent_failed_tools(&self) -> Result<Vec<(String, Option<String>, Option<String>)>> {
        let mut query = self.0.prepare("SELECT t.name,t.input,t.output FROM tool_calls t JOIN messages m ON t.message_id=m.id WHERE t.is_error=1 ORDER BY m.sort_order DESC,m.rowid DESC,t.sort_order DESC,t.rowid DESC LIMIT 200")?;
        let rows = query.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub fn segments(&self, id: &str) -> Result<Vec<String>> {
        Ok(turn_repo::turn_messages(self.0, id)?)
    }
    pub fn requests(&self, id: &str) -> Result<Value> {
        messages::get_request_snapshots(self.0, id)
    }
    pub fn tools(&self, id: &str) -> Result<Vec<messages::ToolCallRow>> {
        messages::get_tool_calls_for_message(self.0, id)
    }
    pub fn origins(&self, id: &str) -> Result<Vec<(String, String, i64)>> {
        messages::get_tool_origins_for_message(self.0, id)
    }
    pub fn delivered_inputs(&self, id: &str) -> Result<Vec<input_queue::DeliveredInput>> {
        input_queue::load_delivered(self.0, id)
    }
    pub fn mcp(&self, id: &str, display: &str) -> Result<Option<(Value, Value, bool)>> {
        let row: Option<(String, String, bool)> = self
            .0
            .query_row("SELECT content_json,metadata_json,is_error FROM mcp_tool_outputs WHERE tool_use_id=?1 AND display_text=?2", [id, display], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        row.map(|(c, m, e)| Ok((serde_json::from_str(&c)?, serde_json::from_str(&m)?, e)))
            .transpose()
    }
    pub fn approval(&self, id: &str) -> Result<Option<permission_repo::PermissionApprovalRow>> {
        permission_repo::get_approval_by_id(self.0, id)
    }
    pub fn diffs(&self, message: &str) -> Result<Vec<diff_repo::CodeDiffRow>> {
        diff_repo::get_diffs_by_message(self.0, message)
    }
    pub fn user_content(&self, message: &str) -> Result<Value> {
        let m = messages::get_message(self.0, message)?.context("Message unavailable")?;
        messages::user_content_for_display(self.0, &m)
    }
}

pub(super) fn mark_input_pending(tx: &Connection, id: &str) -> Result<()> {
    super::state::set_local(tx, &format!("prompt.pending.{id}"), "true")
}

impl super::SessionDb {
    pub(crate) fn acknowledge_model_inputs(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.write(|w| {
            let mut update = w.0.prepare_cached(
                "UPDATE peripheral_settings SET value=json_remove(value,'$.pending')
                WHERE key='memory.prompt.message.'||?1 AND json_extract(value,'$.pending')=1",
            )?;
            for id in ids {
                update.execute([id])?;
                w.0.execute(
                    "DELETE FROM peripheral_settings WHERE key='prompt.pending.'||?1",
                    [id],
                )?;
            }
            Ok(())
        })
    }

    pub fn admit_user(&self, id: &str, content: &Value, origin: &str) -> Result<()> {
        let _guard = self.hold_incarnation()?;
        let memory = crate::services::memory_files::prompt::Admission::session(self)?;
        self.write(|w| w.append_user(id, content, origin, Some(&memory)))
    }

    /// Freeze any resident-index change in the same transaction as its prompt.
    pub fn admit_prompt(
        &self,
        display: &str,
        content: Option<&Value>,
        origin: &str,
    ) -> Result<String> {
        let _guard = self.hold_incarnation()?;
        let memory = crate::services::memory_files::prompt::Admission::session(self)?;
        self.write(|w| {
            if origin == "hook" {
                return w.append_hook_user(display);
            }
            let id = match content {
                Some(content) => w.append_user_content(display, content, origin == "typed")?,
                None => w.append("user", display)?,
            };
            w.set_prompt_origin(&id, origin)?;
            memory.admit(w.0, &id)?;
            if origin == "typed" {
                mark_input_pending(w.0, &id)?;
            }
            Ok(id)
        })
    }
}

impl SessionWrite<'_> {
    pub(crate) fn refresh_resident_indexes(
        &self,
        memory: &crate::services::memory_files::prompt::Admission,
    ) -> Result<bool> {
        memory.refresh_request(self.0, || {
            let id = self.append("user", "")?;
            self.set_prompt_origin(&id, "memory_change")?;
            Ok(id)
        })
    }
    pub(super) fn next_order(&self) -> Result<i64> {
        Ok(self.0.query_row(
            "SELECT coalesce(max(sort_order),-1)+1 FROM messages",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn replace_messages(&self, messages: &[Value]) -> Result<()> {
        crate::db::transcript::replace_messages(self.0, self.1, messages)?;
        self.2.set(true);
        Ok(())
    }
    pub fn append(&self, role: &str, content: &str) -> Result<String> {
        let id = crate::db::transcript::MessageIds::for_shard(self.0)?.issue();
        messages::insert_message(
            self.0,
            &id,
            self.1,
            role,
            content,
            None,
            &chrono::Utc::now().to_rfc3339(),
            false,
            self.next_order()?,
        )?;
        if role == "user" {
            self.set_prompt_origin(&id, "generated")?;
        }
        self.2.set(true);
        Ok(id)
    }
    pub(super) fn set_prompt_origin(&self, id: &str, origin: &str) -> Result<()> {
        self.set_message_metadata(id, &serde_json::json!({"promptOrigin": origin}))
    }
    pub(super) fn append_user(
        &self,
        id: &str,
        content: &Value,
        origin: &str,
        memory: Option<&crate::services::memory_files::prompt::Admission>,
    ) -> Result<()> {
        ensure!(
            !self
                .0
                .prepare("SELECT 1 FROM messages WHERE id=?1")?
                .exists([id])?,
            "User message ID already admitted"
        );
        messages::save_user_message(
            self.0,
            id,
            self.1,
            content,
            &chrono::Utc::now().to_rfc3339(),
            self.next_order()?,
        )?;
        self.set_prompt_origin(id, origin)?;
        if let Some(memory) = memory {
            memory.admit(self.0, id)?;
        }
        if origin == "typed" {
            mark_input_pending(self.0, id)?;
            permission_authority::record_conn(
                self.0,
                self.1,
                permission_authority::Source::User,
                content,
            )?;
        }
        self.2.set(true);
        Ok(())
    }
    pub fn start_turn(&self) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.start_turn_at(&id, &chrono::Utc::now().to_rfc3339())?;
        Ok(id)
    }
    pub(crate) fn start_turn_at(&self, id: &str, at: &str) -> Result<()> {
        ensure!(!self.3.pending, "Provider switch pending; worker is retiring");
        self.0.execute("INSERT INTO execution_turns(id,conversation_id,started_at,status) VALUES(?1,?2,?3,'running')",params![id,self.1,at])?;
        self.2.set(true);
        Ok(())
    }
    pub fn segment(&self, turn: &str) -> Result<String> {
        let id = self.append("assistant", "")?;
        turn_repo::link_message(self.0, turn, &id)?;
        Ok(id)
    }
    pub fn finish_turn(
        &self,
        turn: &str,
        status: turn_repo::TurnStatus,
        elapsed_ms: u64,
    ) -> Result<()> {
        self.finish_turn_at(turn, status, elapsed_ms, &chrono::Utc::now().to_rfc3339())
    }
    pub(crate) fn finish_turn_at(
        &self,
        turn: &str,
        status: turn_repo::TurnStatus,
        elapsed_ms: u64,
        ended_at: &str,
    ) -> Result<()> {
        ensure!(
            status != turn_repo::TurnStatus::Running,
            "Expected terminal status"
        );
        ensure!(
            self.0.execute(
                "UPDATE execution_turns SET status=?1,ended_at=?2,elapsed_ms=?3 WHERE id=?4 AND status='running'",
                params![
                    turn_repo::status_name(status),
                    ended_at,
                    elapsed_ms,
                    turn
                ]
            )? == 1,
            "Turn is no longer running"
        );
        self.0.execute("UPDATE manifest SET updated_at=?1 WHERE singleton=1 AND julianday(updated_at)<julianday(?1)",[ended_at])?;
        self.2.set(true);
        Ok(())
    }
    pub fn settle_interrupted(&self) -> Result<Vec<turn_repo::TurnRecord>> {
        self.2.set(true);
        let mut settled = Vec::new();
        while let Some(turn) = turn_repo::settle_next(self.0, self.1)? {
            settled.push(turn);
        }
        Ok(settled)
    }
    pub fn message_content(&self, id: &str, content: &str) -> Result<()> {
        messages::update_message_content(self.0, id, content)
    }
    pub fn snapshots(&self, message: &str, requests: &Value) -> Result<()> {
        let previous = messages::get_request_snapshots(self.0, message)?;
        if previous != *requests {
            if let Some(old) = previous.as_array() {
                let retained: std::collections::HashSet<&str> = requests
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r["id"].as_str())
                    .collect();
                for id in old.iter().filter_map(|r| r["id"].as_str()) {
                    if !retained.contains(id) {
                        self.0.execute(
                            "DELETE FROM model_requests WHERE message_id=?1 AND id=?2",
                            [message, id],
                        )?;
                    }
                }
                for entry in old.iter().filter(|r| r["id"].as_str().is_none()) {
                    crate::db::request_repo::remove_anonymous_snapshot(self.0, message, entry)?;
                }
            }
            messages::save_request_snapshots(self.0, message, requests)?;
        }
        Ok(())
    }
    pub fn normalized_request(&self, message: &str, request: &Value) -> Result<()> {
        crate::db::request_repo::save_request(self.0, message, request)
    }
    pub fn stream_tools(
        &self,
        message: &str,
        content: &str,
        tools: &[messages::ToolCallRow],
        origins: &crate::services::tool_history::ToolOrigins,
    ) -> Result<()> {
        if !tools.is_empty() {
            messages::upsert_stream_message(self.0, self.1, message, content)?;
        }
        for tool in tools {
            let row = messages::ToolCallRow {
                id: stream_occurrence_id(message, &tool.id),
                message_id: message.into(),
                ..tool.clone()
            };
            if !messages::pending_after_completion(self.0, &row.id, row.output.as_deref())? {
                self.tool(&row, None)?;
            }
            if let Some((parent, sequence)) = origins.get(&tool.id) {
                messages::save_tool_origin(
                    self.0,
                    &row.id,
                    &stream_occurrence_id(message, parent),
                    i64::try_from(*sequence)?,
                )?;
            }
        }
        Ok(())
    }
    pub fn tool(&self, row: &messages::ToolCallRow, origin: Option<(&str, i64)>) -> Result<()> {
        messages::upsert_tool_call(
            self.0,
            &row.id,
            &row.message_id,
            &row.name,
            row.input.as_deref(),
            row.output.as_deref(),
            row.is_error,
            row.sort_order,
        )?;
        if let Some((parent, seq)) = origin {
            messages::save_tool_origin(self.0, &row.id, parent, seq)?;
        }
        Ok(())
    }
    pub fn tool_note(&self, id: &str, note: &str) -> Result<()> {
        messages::set_tool_call_note(self.0, id, note)
    }
    pub fn tool_duration(&self, id: &str, millis: u64) -> Result<()> {
        messages::set_tool_duration(self.0, id, millis)
    }
    pub fn attachment(
        &self,
        id: &str,
        message: &str,
        name: &str,
        kind: &str,
        mime: &str,
        size: i64,
        source: &str,
    ) -> Result<()> {
        self.0.execute(
            "INSERT INTO attachments VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, message, name, kind, mime, size, source],
        )?;
        Ok(())
    }
    pub fn approval(&self, row: &permission_repo::PermissionApprovalRow) -> Result<()> {
        permission_repo::insert_permission_approval(
            self.0,
            &row.id,
            self.1,
            &row.message_id,
            &row.tool_name,
            &row.action,
            &row.risk_level,
            &row.status,
            &row.created_at,
        )
    }
    pub fn decide(
        &self,
        id: &str,
        status: &str,
        decision: Option<&str>,
        reason: Option<&str>,
        at: &str,
    ) -> Result<()> {
        permission_repo::update_approval_status(self.0, id, status, decision, reason, at)
    }
    pub fn diff(&self, row: &diff_repo::CodeDiffRow) -> Result<()> {
        diff_repo::insert_code_diff(
            self.0,
            &row.id,
            self.1,
            &row.message_id,
            &row.file_path,
            row.original_content.as_deref(),
            row.modified_content.as_deref(),
            row.diff_text.as_deref(),
            &row.status,
            &row.created_at,
        )
    }
    pub fn diff_status(&self, id: &str, status: &str, at: Option<&str>) -> Result<()> {
        diff_repo::update_diff_status(self.0, id, status, at)
    }
}

/// Durable execution addresses carry the saved incarnation across worker restarts.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryAddress {
    pub store: String,
    pub session: String,
    pub incarnation: String,
}
impl SessionDb {
    pub fn address(&self) -> HistoryAddress {
        HistoryAddress {
            store: self.stores.global().uuid.clone(),
            session: self.id.clone(),
            incarnation: self.incarnation.clone(),
        }
    }
}
impl super::Stores {
    pub fn resume_history(&self, address: &HistoryAddress) -> Result<SessionDb> {
        ensure!(
            self.global().uuid == address.store,
            "Execution store changed"
        );
        self.open_recorded(&address.session, &address.incarnation)
    }
}
#[derive(Debug, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub mime: Option<String>,
    pub size: Option<i64>,
    pub source: Option<String>,
}
impl SessionRead<'_> {
    pub fn attachments(&self, message: &str) -> Result<Vec<Attachment>> {
        let mut q = self.0.prepare("SELECT id,file_name,file_type,mime_type,file_size,source FROM attachments WHERE message_id=?1 ORDER BY rowid")?;
        let rows = q
            .query_map([message], |r| {
                Ok(Attachment {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    mime: r.get(3)?,
                    size: r.get(4)?,
                    source: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn tool_note(&self, id: &str) -> Result<Option<String>> {
        messages::tool_call_note(self.0, id)
    }
    pub fn tool_duration(&self, id: &str) -> Result<Option<u64>> {
        messages::tool_duration(self.0, id)
    }
    pub fn child_invocations(
        &self,
        message: &str,
    ) -> Result<Vec<(String, crate::services::subagents::Invocation)>> {
        let mut q = self.0.prepare("SELECT child_incarnation,invocation FROM child_invocations WHERE parent_message=?1 ORDER BY rowid")?;
        let mut cursor = q.query([message])?;
        let mut out = Vec::new();
        while let Some(r) = cursor.next()? {
            out.push((r.get(0)?, serde_json::from_str(&r.get::<_, String>(1)?)?));
        }
        Ok(out)
    }
    pub fn receipt_count(&self) -> Result<i64> {
        Ok(self
            .0
            .query_row("SELECT count(*) FROM delivery_receipts", [], |r| r.get(0))?)
    }
    pub fn authority(&self) -> Result<Vec<Value>> {
        let mut q = self
            .0
            .prepare("SELECT content_json FROM session_permission_authority ORDER BY id")?;
        let mut rows = q.query([])?;
        let mut v = Vec::new();
        while let Some(r) = rows.next()? {
            v.push(serde_json::from_str(&r.get::<_, String>(0)?)?);
        }
        Ok(v)
    }
    pub fn binding(&self) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT definition FROM session_agents", [], |r| r.get(0))
            .optional()?)
    }
}
impl SessionWrite<'_> {
    pub fn child_invocation(
        &self,
        address: &HistoryAddress,
        row: &crate::services::subagents::Invocation,
    ) -> Result<()> {
        let store: String = self
            .0
            .query_row("SELECT store_uuid FROM manifest", [], |r| r.get(0))?;
        ensure!(
            address.store == store
                && address.session != self.1
                && row.parent_session_id == self.1
                && row.child_session_id == address.session
                && !row.attempt_id.is_empty()
                && !row.tool_use_id.is_empty(),
            "Invalid child invocation ownership"
        );
        let updated = self.0.execute(
            "INSERT INTO child_invocations VALUES(?1,?2,?3,?4) ON CONFLICT(attempt_key) DO UPDATE SET invocation=excluded.invocation WHERE parent_message=excluded.parent_message AND child_incarnation=excluded.child_incarnation",
            params![
                row.key(),
                row.parent_message_id,
                address.incarnation,
                serde_json::to_string(row)?
            ],
        )?;
        ensure!(updated == 1, "Child invocation identity changed");
        Ok(())
    }
    pub fn bind_agent(&self, definition: &str) -> Result<()> {
        self.0.execute("INSERT INTO session_agents VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET definition=excluded.definition", params![self.1, definition])?;
        Ok(())
    }
    pub fn approved_plan(&self, content: &Value) -> Result<()> {
        permission_authority::record_conn(
            self.0,
            self.1,
            permission_authority::Source::ApprovedPlan,
            content,
        )
    }
    pub fn rewind_from(&self, message: &str) -> Result<()> {
        let order: i64 = self.0.query_row(
            "SELECT sort_order FROM messages WHERE id=?1",
            [message],
            |r| r.get(0),
        )?;
        self.0.execute("DELETE FROM execution_turns WHERE id IN (SELECT t.turn_id FROM message_turns t JOIN messages m ON m.id=t.message_id WHERE m.sort_order>=?1) AND id NOT IN (SELECT t.turn_id FROM message_turns t JOIN messages m ON m.id=t.message_id WHERE m.sort_order<?1)", [order])?;
        self.0
            .execute("DELETE FROM messages WHERE sort_order>=?1", [order])?;
        self.2.set(true);
        Ok(())
    }

    /// Only assistant segments belong to an attempt. A segment carrying delivered
    /// input must stay replayable with its tool results and receipt.
    pub fn discard_failed_segments(&self, turn: &str) -> Result<()> {
        ensure!(self.0.prepare("SELECT 1 FROM execution_turns WHERE id=?1 AND conversation_id=?2 AND status='running'")?.exists([turn,self.1])?,
            "Compaction retry lost the active turn");
        self.0.execute(
            "DELETE FROM messages WHERE role='assistant'
             AND id IN (SELECT message_id FROM message_turns WHERE turn_id=?1)
             AND NOT EXISTS (SELECT 1 FROM tool_calls t JOIN tool_queued_inputs q
                 ON q.tool_call_id=t.id WHERE t.message_id=messages.id)",
            [turn],
        )?;
        self.2.set(true);
        Ok(())
    }
}

impl SessionRead<'_> {
    pub fn compaction_count(&self) -> Result<i64> {
        Ok(self.0.query_row(
            "SELECT local_count FROM compaction_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)
    }
    pub(crate) fn boundary_id(&self) -> Result<Option<String>> {
        Ok(self.0.query_row("SELECT id FROM messages WHERE is_compact_boundary=1 ORDER BY sort_order DESC,rowid DESC LIMIT 1",
            [], |r| r.get(0)).optional()?)
    }

    /// Identify the active exchange even when a new process opens it after
    /// multiple steers. Next-turn deliveries have their own generation.
    pub(crate) fn protected_inputs(
        &self,
        messages: &[Value],
    ) -> Result<(Option<usize>, Vec<String>)> {
        let mut steer = self.0.prepare_cached(
            "SELECT 1 FROM delivery_receipts d WHERE d.delivery_id=?1 AND NOT EXISTS(
            SELECT 1 FROM delivery_generations g WHERE g.delivery_id=d.delivery_id)",
        )?;
        let mut pending = self.0.prepare_cached(
            "SELECT 1 FROM peripheral_settings WHERE key='memory.prompt.message.'||?1
            AND json_extract(value,'$.pending')=1 OR key='prompt.pending.'||?1",
        )?;
        let mut unsent = Vec::new();
        let mut first = None;
        let mut active = true;
        for (i, row) in messages.iter().enumerate().rev() {
            let Some(id) = row["id"].as_str().filter(|_| row["role"] == "user") else {
                continue;
            };
            if row["origin"] == "typed" {
                let unseen = pending.exists([id])?;
                if unseen {
                    unsent.push(id.to_owned());
                }
                if active || unseen {
                    first = Some(i);
                }
            }
            if active && !steer.exists([id])? {
                active = false;
            }
        }
        Ok((first, unsent))
    }

    pub fn model_history(
        &self,
        format: &crate::native_engine::provider_manager::ApiFormat,
    ) -> Result<(Vec<Value>, Option<String>)> {
        self.model_history_before(format, None)
    }

    pub(crate) fn model_history_before(
        &self,
        format: &crate::native_engine::provider_manager::ApiFormat,
        before: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let id: String = self
            .0
            .query_row("SELECT id FROM manifest", [], |r| r.get(0))?;
        crate::runtime::replay_history_with_ids(
            self.0,
            &id,
            format,
            |message| {
                let raw: Option<String> = self
                    .0
                    .query_row(
                        "SELECT value FROM native_boundaries WHERE message_id=?1",
                        [message],
                        |r| r.get(0),
                    )
                    .optional()?;
                raw.map(|v| serde_json::from_str(&v).map_err(Into::into))
                    .transpose()
            },
            true,
            before,
        )
    }
}
impl SessionWrite<'_> {
    pub fn native_boundary(&self, message: &str, value: &Value) -> Result<()> {
        let mut window = value.clone();
        crate::native_engine::tool_loop::seal_native_window(
            &mut window,
            crate::db::model_results::data_root(self.0)?.as_deref(),
            self.1,
        );
        self.0.execute("UPDATE compaction_state SET local_count=local_count+1 WHERE singleton=1 AND NOT EXISTS(SELECT 1 FROM native_boundaries WHERE message_id=?1)", [message])?;
        self.0
            .execute("INSERT INTO native_boundaries VALUES(?1,?2) ON CONFLICT(message_id) DO UPDATE SET value=excluded.value", params![message, serde_json::to_string(&window)?])?;
        self.0.execute(
            "UPDATE messages SET is_compact_boundary=1 WHERE id=?1",
            [message],
        )?;
        self.2.set(true);
        Ok(())
    }
}

/// Timings and token counts reported by a single model request.
pub struct RequestMetrics {
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub content_tokens: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub generation_ms: Option<i64>,
    pub status: String,
    pub response_blocks: Value,
}

impl SessionRead<'_> {
    pub fn request_metrics(&self, id: &str) -> Result<Option<RequestMetrics>> {
        let row=self.0.query_row("SELECT output_tokens,reasoning_tokens,content_tokens,ttft_ms,generation_ms,status,response_blocks_json FROM model_requests WHERE id=?1",[id],|r|Ok((
            RequestMetrics{output_tokens:r.get(0)?,reasoning_tokens:r.get(1)?,content_tokens:r.get(2)?,ttft_ms:r.get(3)?,generation_ms:r.get(4)?,status:r.get(5)?,response_blocks:Value::Null},
            r.get::<_,Option<String>>(6)?
        ))).optional()?;
        row.map(|(mut metrics, raw)| {
            if let Some(raw) = raw {
                metrics.response_blocks = serde_json::from_str(&raw)?;
            }
            Ok(metrics)
        })
        .transpose()
    }
}

pub struct TranscriptExcerpt {
    pub role: String,
    pub text: String,
    pub tools: Vec<(String, Option<String>, Option<String>, bool)>,
}

impl SessionRead<'_> {
    pub fn occurrence_content(&self, message: &str, provider_id: &str, display: &str) -> Result<Option<Value>> {
        let id = stream_occurrence_id(message, provider_id);
        Ok(self.mcp(&id, display)?.map(|(content, _, _)| content))
    }

    pub fn changed_lines(&self) -> Result<(i64,i64)> {
        Ok(self.0.query_row("SELECT COALESCE(SUM(json_extract(output,'$.file_change.added')),0), COALESCE(SUM(json_extract(output,'$.file_change.removed')),0) FROM tool_calls WHERE json_valid(output) AND is_error=0", [], |r| Ok((r.get(0)?,r.get(1)?)))?)
    }

    pub fn output_tokens(&self) -> Result<i64> {
        let mut query=self.0.prepare("SELECT id,output_tokens FROM model_requests WHERE output_tokens IS NOT NULL")?;
        let mut counts=query.query_map([],|r| Ok((r.get::<_, String>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<std::collections::HashMap<_,_>>>()?;
        for live in self.setting(super::settings::LIVE_REQUESTS)?.unwrap_or_default() {
            if let (Some(id),Some(tokens))=(live.id, live.output_tokens) { counts.insert(id,i64::try_from(tokens).unwrap_or(i64::MAX)); }
        }
        Ok(counts.into_values().fold(0i64,i64::saturating_add))
    }

    pub fn transcript_tail(&self) -> Result<Vec<TranscriptExcerpt>> {
        let mut messages=self.0.prepare("SELECT id,role,substr(content,-65536) FROM (SELECT id,role,content,sort_order,rowid AS sequence FROM messages ORDER BY sort_order DESC,rowid DESC LIMIT 20) ORDER BY sort_order,sequence")?;
        let mut calls=self.0.prepare("SELECT name,substr(input,1,8192),substr(output,-16384),is_error FROM (SELECT name,input,output,is_error,sort_order,rowid AS sequence FROM tool_calls WHERE message_id=?1 ORDER BY sort_order DESC,rowid DESC LIMIT 30) ORDER BY sort_order,sequence")?;
        let mut rows=messages.query([])?;
        let mut tail=Vec::new();
        while let Some(row)=rows.next()? {
            let role:String=row.get(1)?;
            let tools=if role=="assistant" {
                calls.query_map([row.get::<_,String>(0)?], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
            } else { Vec::new() };
            tail.push(TranscriptExcerpt { role, text:row.get(2)?, tools });
        }
        Ok(tail)
    }
}
