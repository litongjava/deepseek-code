use super::*;
use crate::services::{compaction, compaction_requests};
use rusqlite::params;
use serde_json::Value;

/// Only digests of provider configuration and fragment inputs reach SQLite.
pub struct CheckpointKey {
    fingerprint: String,
    request: String,
}
impl CheckpointKey {
    pub fn new(
        provider: &crate::native_engine::provider_manager::ResolvedProvider,
        configuration_system: &str,
        request_system: &str,
        fragment: &str,
        output: u32,
    ) -> Self {
        Self {
            fingerprint: compaction::checkpoint::config_fingerprint(provider, configuration_system),
            request: compaction::checkpoint::Checkpoint::request_key(
                request_system,
                fragment,
                output,
            ),
        }
    }
}

/// A next-turn delivery already owns its transcript row when its turn starts.
pub(super) fn queued_turn_input(c: &Connection) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT m.id FROM delivery_generations d JOIN execution_turns t ON t.id=d.turn_id
         JOIN messages m ON m.id=d.delivery_id WHERE t.status='running'
         ORDER BY m.sort_order,m.rowid LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()?)
}

impl SessionRead<'_> {
    pub fn queued_turn_input(&self) -> Result<Option<String>> {
        queued_turn_input(self.0)
    }
}

pub struct CompactionClaim {
    pub(crate) id: String,
    incarnation: String,
    fingerprint: String,
    before: Option<String>,
    pub keep: String,
    pub transcript: String,
}

enum PendingClaim {
    Empty,
    Ready(CompactionClaim),
    Obsolete {
        id: String,
        revision: i64,
        reason: &'static str,
    },
}

impl CompactionClaim {
    pub fn request_id(&self) -> &str {
        &self.id
    }
}

impl SessionWrite<'_> {
    pub(crate) fn start_compaction_metrics(
        &self,
        snapshot: &Value,
        metadata: &Value,
    ) -> Result<Option<String>> {
        let message: Option<String> = self.0.query_row(
            "SELECT id FROM messages WHERE conversation_id=?1 ORDER BY sort_order DESC,rowid DESC LIMIT 1",
            [self.1], |row| row.get(0),
        ).optional()?;
        if let Some(message) = &message {
            self.finish_compaction_metrics(message, snapshot)?;
            self.set_request_metadata(
                snapshot["id"].as_str().context("Missing request ID")?,
                metadata,
            )?;
        }
        Ok(message)
    }

    pub(crate) fn finish_compaction_metrics(&self, message: &str, snapshot: &Value) -> Result<()> {
        crate::db::request_repo::save_request(self.0, message, snapshot)
    }

    pub fn request_compaction(&self, keep: &str, used_tokens: u64, window: u64) -> Result<()> {
        compaction_requests::validate(self.0, self.1, keep, used_tokens, window)?;
        self.0.execute(
            "INSERT INTO compaction_request(singleton,id,keep,target_message) VALUES(1,?1,?2,(SELECT id FROM messages ORDER BY sort_order DESC,rowid DESC LIMIT 1))
            ON CONFLICT(singleton) DO UPDATE SET id=excluded.id,keep=excluded.keep,target_message=excluded.target_message",
            params![uuid::Uuid::new_v4().to_string(), keep],
        )?;
        Ok(())
    }

    pub fn save_compaction_checkpoint(&self, key: &CheckpointKey, summary: &str) -> Result<()> {
        ensure!(
            !summary.trim().is_empty() && summary.chars().count() <= 32_000,
            "Empty or oversized compaction summary"
        );
        self.0.execute("INSERT INTO compaction_checkpoint VALUES(?1,?2,?3)
            ON CONFLICT(fingerprint,request_key) DO UPDATE SET summary=excluded.summary WHERE summary<>excluded.summary",
            [key.fingerprint.as_str(),key.request.as_str(),summary])?;
        Ok(())
    }
}

impl SessionRead<'_> {
    pub fn compaction_cancellation(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT reason FROM compaction_cancellations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn compaction_checkpoint(&self, key: &CheckpointKey) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT summary FROM compaction_checkpoint WHERE fingerprint=?1 AND request_key=?2",
                [&key.fingerprint, &key.request],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn native_compaction_boundary(&self, message: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM native_boundaries WHERE message_id=?1",
                [message],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
}

/// The physical transcript cut and its optimistic write fence are selected in
/// the same snapshot. A tool-delivered input belongs to the whole assistant row.
pub(crate) struct CompactionWindow {
    pub prefix_len: usize,
    before: Option<String>,
    pub(crate) fingerprint: String,
}

impl SessionRead<'_> {
    pub(crate) fn compaction_window(
        &self,
        messages: &[serde_json::Value],
        protected: Option<usize>,
        segment: Option<&str>,
        format: &crate::native_engine::provider_manager::ApiFormat,
    ) -> Result<CompactionWindow> {
        let protected_row: Option<String> = if let Some(at) = protected {
            let input = messages[at]["id"]
                .as_str()
                .context("Protected input has no identity")?;
            Some(self.0.query_row(
                "SELECT id FROM messages WHERE id=?1 UNION ALL
                 SELECT t.message_id FROM tool_calls t JOIN tool_queued_inputs q ON t.id=q.tool_call_id
                 JOIN json_each(q.inputs_json) j WHERE json_extract(j.value,'$.message_id')=?1 LIMIT 1",
                [input], |r| r.get(0),
            )?)
        } else {
            None
        };
        // An active assistant segment can precede a later steer. Keep it on
        // the visible side so results written after compaction remain replayable.
        let before: Option<String> = self.0.query_row(
            "WITH cut AS (SELECT sort_order,rowid AS seq FROM messages WHERE is_compact_boundary
                 ORDER BY sort_order DESC,rowid DESC LIMIT 1)
             SELECT id FROM messages WHERE id IN (?1,?2) AND
                 (NOT EXISTS(SELECT 1 FROM cut) OR (sort_order,rowid)>(SELECT sort_order,seq FROM cut))
             ORDER BY sort_order,rowid LIMIT 1", params![protected_row, segment], |r| r.get(0),
        ).optional()?;
        let (prefix, boundary) = self.model_history_before(format, before.as_deref())?;
        let prefix_len = if boundary.is_some()
            && prefix.len() == 1
            && (prefix[0]["provider_content"]["compaction"] == true
                || prefix[0]["origin"] == "compaction")
        {
            0 // Only the previously sealed opaque window precedes the protected exchange.
        } else {
            prefix.len()
        };
        let id: String = self
            .0
            .query_row("SELECT id FROM manifest", [], |r| r.get(0))?;
        Ok(CompactionWindow {
            before,
            prefix_len,
            fingerprint: compaction::fingerprint(self.0, &id)?.0,
        })
    }
    pub(crate) fn mid_turn_compaction_fence(
        &self,
    ) -> Result<(Option<String>, Option<String>, usize)> {
        let last_message: Option<String> = self
            .0
            .query_row(
                "SELECT id FROM messages ORDER BY sort_order DESC, rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let boundary = self.boundary_id()?;
        let count: i64 = self.0.query_row(
            "WITH cut AS (SELECT sort_order, rowid AS seq FROM messages WHERE is_compact_boundary=1 ORDER BY sort_order DESC, rowid DESC LIMIT 1)
             SELECT COUNT(*) FROM messages WHERE NOT EXISTS(SELECT 1 FROM cut) OR (sort_order, rowid) >= (SELECT sort_order, seq FROM cut)",
            [],
            |r| r.get(0),
        )?;
        Ok((last_message, boundary, count.max(0) as usize))
    }
}

impl SessionDb {
    pub(crate) fn commit_compaction_window(
        &self,
        cut: &CompactionWindow,
        summary: &compaction::GeneratedSummary,
    ) -> Result<String> {
        let _guard = self.hold_incarnation()?;
        let memory = crate::services::memory_files::prompt::Admission::session(self)?;
        self.write_locked("seal compressed window", |w| {
            ensure!(
                compaction::fingerprint(w.0, w.1)?.0 == cut.fingerprint,
                "Session changed during compaction"
            );
            w.commit_summary(summary, &memory, cut.before.as_deref())
        })
    }

    pub(crate) fn commit_mid_turn_compaction(
        &self,
        expected_last_message: Option<&str>,
        expected_boundary: Option<&str>,
        summary: &compaction::GeneratedSummary,
    ) -> Result<String> {
        ensure!(
            !summary.text.trim().is_empty() && summary.text.chars().count() <= 32_000,
            "Empty or oversized compaction summary"
        );
        let _guard = self.hold_incarnation()?;
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        let memory = crate::services::memory_files::prompt::Admission::session(self)?;
        self.require_active_compaction()?;
        self.write_locked("commit mid-turn compaction", |w| {
            let (current_last, current_boundary, _) = SessionRead(w.0).mid_turn_compaction_fence()?;
            ensure!(
                current_last.as_deref() == expected_last_message
                    && current_boundary.as_deref() == expected_boundary,
                "Session changed during compaction"
            );
            w.0.execute("DELETE FROM compaction_request", [])?;
            w.commit_summary(summary, &memory, None)
        })
    }
}
impl SessionDb {
    fn require_active_compaction(&self) -> Result<()> {
        let s = self;
        let archived: bool = s.stores.global().read(|c| {
            Ok(c.query_row(
                "SELECT archived FROM conversations WHERE id=?1 AND incarnation=?2",
                [&s.id, &s.incarnation],
                |r| r.get(0),
            )?)
        })?;
        ensure!(!archived, "Session is archived");
        Ok(())
    }
}
impl DeliveryOwner {
    /// The execution lease is the claim. Leave the request durable until the
    /// boundary commits so a replacement worker can resume the same work.
    pub fn claim_compaction(&self) -> Result<Option<CompactionClaim>> {
        layout::allow_entry()?;
        let s = &self.session;
        s.require_active_compaction()?;
        let candidate = s.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
            let tx = c.transaction()?;
            let request = tx
                .query_row("SELECT q.id,q.keep,EXISTS(SELECT 1 FROM messages WHERE id=q.target_message),m.content_revision
                    FROM compaction_request q JOIN manifest m USING(singleton)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, bool>(2)?, r.get::<_, i64>(3)?))
                })
                .optional()?;
            let Some((id, keep, has_target, revision)) = request else {
                return Ok(PendingClaim::Empty);
            };
            if !has_target {
                return Ok(PendingClaim::Obsolete {id, revision, reason:"missing compaction target"});
            }
            let before = queued_turn_input(&tx)?;
            let (fingerprint, count) = compaction::fingerprint_before(&tx, &s.id, before.as_deref())?;
            if count < 2 {
                return Ok(PendingClaim::Obsolete {id, revision, reason:"not enough messages to compact"});
            }
            Ok(PendingClaim::Ready(CompactionClaim {
                id,
                incarnation: s.incarnation.clone(),
                fingerprint,
                transcript: compaction::recorded_history(&tx, &s.id, true, before.as_deref())?,
                before,
                keep,
            }))
        })?;
        match candidate {
            PendingClaim::Empty => Ok(None),
            PendingClaim::Ready(claim) => Ok(Some(claim)),
            PendingClaim::Obsolete {
                id,
                revision,
                reason,
            } => {
                // The read snapshot has closed. Fence both a replacement request
                // and history that became eligible before acquiring the writer.
                s.write_locked("cancel obsolete compaction", |w| {
                    if w.0.execute("DELETE FROM compaction_request WHERE id=?1 AND (SELECT content_revision FROM manifest)=?2", params![id,revision])? == 1 {
                        w.0.execute("INSERT INTO compaction_cancellations(id,reason) VALUES(?1,?2)",[id.as_str(), reason])?;
                    }
                    Ok(())
                })?;
                Ok(None)
            }
        }
    }

    pub fn finish_compaction(
        &self,
        claim: &CompactionClaim,
        summary: &compaction::GeneratedSummary,
    ) -> Result<String> {
        layout::allow_entry()?;
        let s = &self.session;
        let _guard = s.hold_incarnation()?;
        s.require_active_compaction()?;
        ensure!(s.incarnation == claim.incarnation, "Wrong compaction owner");
        ensure!(
            !summary.text.trim().is_empty() && summary.text.chars().count() <= 32_000,
            "Empty or oversized compaction summary"
        );
        let memory = crate::services::memory_files::prompt::Admission::session(s)?;
        s.write_locked("commit session compaction", |w| {
            ensure!(
                w.0.execute("DELETE FROM compaction_request WHERE id=?1", [&claim.id])? == 1,
                "Compaction request changed"
            );
            ensure!(
                compaction::fingerprint(w.0, w.1)?.0 == claim.fingerprint,
                "Session changed during compaction"
            );
            w.commit_summary(summary, &memory, claim.before.as_deref())
        })
    }
}

impl SessionRepository<'_> {
    pub(crate) fn request_compaction(&self, keep:&str, used:u64, window:u64) -> Result<()> {
        match &self.0 {
            owned::Backend::Admitted(s) => s.write_named("queue compaction", |w| w.request_compaction(keep, used, window)),
            owned::Backend::Shared(_,_) => self.write("queue compaction",|c,id|{
                compaction_requests::validate(c,id,keep,used,window)?;
                compaction_requests::save(c,id,keep)
            }),
        }
    }

    pub(crate) fn compaction_total(&self) -> Result<compaction::CompactionCount> {
        match &self.0 {
            owned::Backend::Admitted(shard) => shard.read(|r| {
                Ok(compaction::CompactionCount::Exact(
                    r.compaction_count()?.try_into()?,
                ))
            }),
            owned::Backend::Shared(_, _) => self.read(compaction::legacy_count),
        }
    }

    pub(crate) fn prepare_compaction(
        &self,
        before_input: Option<&str>,
    ) -> Result<compaction::PreparedCompaction> {
        let incarnation = match &self.0 {
            owned::Backend::Admitted(shard) => Some(shard.incarnation().to_owned()),
            owned::Backend::Shared(_, _) => None,
        };
        if let owned::Backend::Admitted(shard) = &self.0 {
            shard.require_active_compaction()?;
        }
        self.read(|c, id| {
            if incarnation.is_none() {
                let row = crate::db::conversation_repo::conversation_metadata(c, id)?
                    .context("Session not found")?;
                ensure!(!row.archived, "Session is archived");
            }
            let before = if let Some(id) = before_input {
                ensure!(incarnation.is_some(), "Expected admitted session input");
                Some(id.to_owned())
            } else if incarnation.is_some() {
                queued_turn_input(c)?
            } else {
                None
            };
            let (fingerprint, count) = compaction::fingerprint_before(c, id, before.as_deref())?;
            ensure!(
                count >= 2 || (before.is_some() && count == 1),
                "Not enough messages to compact"
            );
            let pending_request = if incarnation.is_some() {
                c.query_row("SELECT id FROM compaction_request WHERE singleton=1",[],|r|r.get(0)).optional()?
            } else {None};
            Ok(compaction::PreparedCompaction {
                pending_request,
                conversation_id: id.into(),
                fingerprint,
                transcript: compaction::recorded_history(
                    c,
                    id,
                    incarnation.is_some(),
                    before.as_deref(),
                )?,
                count,
                incarnation,
                before,
            })
        })
    }

    pub(crate) fn commit_compaction(
        &self,
        prepared: &compaction::PreparedCompaction,
        generated: &compaction::GeneratedSummary,
        context: Option<&str>,
    ) -> Result<compaction::CompactionResult> {
        match &self.0 {
            owned::Backend::Shared(_, _) => {
                ensure!(prepared.incarnation.is_none(), "Compaction store changed");
                self.write("commit compaction", |c, _| {
                    compaction::commit_locked(c, prepared, generated, context)
                })
            }
            owned::Backend::Admitted(s) => {
                ensure!(
                    prepared.incarnation.as_deref() == Some(s.incarnation()),
                    "Compaction session was replaced"
                );
                let _locks = s.stores.key_lock(&s.key, false, false)?;
                let memory = crate::services::memory_files::prompt::Admission::session(s)?;
                s.require_active_compaction()?;
                s.write_locked("commit compaction", |w| {
                    ensure!(
                        compaction::fingerprint(w.0, w.1)?.0 == prepared.fingerprint,
                        "Session changed during compaction; retry after the active turn finishes"
                    );
                    if let Some(request)=&prepared.pending_request {
                        ensure!(w.0.execute("DELETE FROM compaction_request WHERE id=?1",[request])?==1,"Compaction request changed");
                    }
                    let message_id =
                        w.commit_summary(generated, &memory, prepared.before.as_deref())?;
                    if let Some(context) = context {
                        state::set_local(
                            w.0,
                            "session-start-context",
                            &serde_json::to_string(context)?,
                        )?;
                    }
                    Ok(compaction::CompactionResult {
                        message_id,
                        summarized_messages: prepared.count,
                        warnings: Vec::new(),
                        summary_path: None,
                        export_error: None,
                        duration_ms: 0,
                    })
                })
            }
        }
    }
}

impl SessionWrite<'_> {
    fn commit_summary(
        &self,
        summary: &compaction::GeneratedSummary,
        memory: &crate::services::memory_files::prompt::Admission,
        before: Option<&str>,
    ) -> Result<String> {
        let content = format!(
            "[Conversation summary — earlier transcript retained in session history]\n{}",
            summary.text.trim()
        );
        let order = if let Some(id) = before {
            let order =
                self.0
                    .query_row("SELECT sort_order FROM messages WHERE id=?1", [id], |r| {
                        r.get::<_, i64>(0)
                    })?;
            self.0.execute(
                "UPDATE messages SET sort_order=sort_order+1 WHERE (sort_order,rowid)>=(SELECT sort_order,rowid FROM messages WHERE id=?1)",
                [id],
            )?;
            order
        } else {
            self.next_order()?
        };
        let message = boundary::create(
            self.0,
            self.1,
            order,
            &content,
            boundary::Kind::Compaction,
            boundary::Storage::Shard(memory),
        )?;
        self.2.set(true);
        if let Some(native) = &summary.native {
            self.0.execute(
                "INSERT INTO native_boundaries VALUES(?1,?2)",
                params![message, serde_json::to_string(native)?],
            )?;
        }
        self.0
            .execute("UPDATE compaction_state SET local_count=local_count+1", [])?;
        self.0.execute("DELETE FROM compaction_checkpoint", [])?;
        Ok(message)
    }
}

impl SessionRepository<'_> {
    pub(crate) fn checkpoint_fragments(
        &self,
        fingerprint: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        self.read(|c, id| match &self.0 {
            owned::Backend::Admitted(_) => {
                let mut q = c.prepare_cached(
                    "SELECT request_key,summary FROM compaction_checkpoint WHERE fingerprint=?1",
                )?;
                let rows = q.query_map([fingerprint], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            }
            owned::Backend::Shared(_, _) => Ok(legacy_fragments(c, id, fingerprint)?),
        })
    }
    pub(crate) fn save_checkpoint_fragment(
        &self,
        fingerprint: &str,
        request: &str,
        summary: &str,
    ) -> Result<()> {
        self.write("save completed summary", |c,id| match &self.0 {
            owned::Backend::Admitted(_) => {
                c.execute("INSERT INTO compaction_checkpoint VALUES(?1,?2,?3)
                  ON CONFLICT(fingerprint,request_key) DO UPDATE SET summary=excluded.summary WHERE summary<>excluded.summary",
                  [fingerprint,request,summary])?;
                Ok(())
            }
            owned::Backend::Shared(_,_) => {
                let mut completed=legacy_fragments(c,id,fingerprint)?;
                completed.insert(request.into(),summary.into());
                let raw=serde_json::to_string(&compaction::checkpoint::Saved{ fingerprint:fingerprint.into(), completed })?;
                state::set_local(c,&format!("compaction.checkpoint.{id}"), &raw)
            }
        })
    }
}

fn legacy_fragments(
    c: &Connection,
    id: &str,
    fingerprint: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    Ok(state::local(c, &format!("compaction.checkpoint.{id}"))?
        .and_then(|raw| serde_json::from_str::<compaction::checkpoint::Saved>(&raw).ok())
        .filter(|s| s.fingerprint == fingerprint)
        .map(|s| s.completed)
        .unwrap_or_default())
}

impl SessionRepository<'_> {
    pub(crate) fn latest_compact_content(
        &self,
    ) -> Result<Option<(String, String, Option<serde_json::Value>)>> {
        self.read(|c,id| {
            let join=match self.0 {
                owned::Backend::Admitted(_) => "LEFT JOIN native_boundaries b ON b.message_id=m.id",
                owned::Backend::Shared(_,_) => "LEFT JOIN peripheral_settings b ON b.key='compaction.native.'||m.id",
            };
            let mut q=c.prepare_cached(&format!("SELECT m.id,m.content,b.value FROM messages m {join}
                WHERE m.conversation_id=?1 AND m.is_compact_boundary=1 ORDER BY m.sort_order DESC,m.rowid DESC LIMIT 1"))?;
            let row=q.query_row([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?))).optional()?;
            row.map(|(message_id,text,native)|Ok((message_id,text,native.map(|s|serde_json::from_str(&s)).transpose()?))).transpose()
        })
    }
}

impl SessionRepository<'_> {
    pub(crate) fn latest_compact_message(&self) -> Result<Option<String>> {
        self.read(|c,id|Ok(c.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1 ORDER BY sort_order DESC,rowid DESC LIMIT 1",[id],|r|r.get(0)).optional()?))
    }
}
