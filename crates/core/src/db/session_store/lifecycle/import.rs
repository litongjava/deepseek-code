use super::*;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
pub(super) struct LegacySession {
    id: String,
    name: String,
    pub(super) created_at: String,
    updated_at: String,
    messages: Vec<LegacyMessage>,
}
#[derive(Deserialize)]
struct LegacyMessage {
    role: String,
    content: String,
    timestamp: String,
}
impl LegacySession {
    pub(super) fn populate(&self, c: &Connection) -> Result<()> {
        let ids = crate::db::transcript::MessageIds::for_shard(c)?;
        for (i, m) in self.messages.iter().enumerate() {
            let message = ids.issue();
            crate::db::message_repo::insert_message(
                c,
                &message,
                &self.id,
                &m.role,
                &m.content,
                None,
                &m.timestamp,
                false,
                i as i64,
            )?;
            if m.role == "user" {
                c.execute(
                    "INSERT INTO message_metadata VALUES(?1,?2)",
                    [&message, r#"{"promptOrigin":"import"}"#],
                )?;
            }
        }
        c.execute(
            "UPDATE manifest SET updated_at=?1,message_count=?2,content_revision=?2",
            params![self.updated_at, self.messages.len()],
        )?;
        Ok(())
    }
}
#[derive(Debug)]
pub struct ImportResult {
    pub project_id: String,
    pub imported: Vec<String>,
    pub skipped: Vec<String>,
    pub backup: Option<PathBuf>,
}
fn read_archive(source: &Path) -> Result<Vec<(LegacySession, String)>> {
    let mut files = if source.is_dir() {
        fs::read_dir(source)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?
    } else {
        vec![source.into()]
    };
    if source.is_dir() {
        files.retain(|p| p.extension().is_some_and(|e| e == "json"))
    }
    files.sort();
    ensure!(
        !files.is_empty() && files.len() <= 1000,
        "Choose 1..1000 session JSON files"
    );
    let mut result = Vec::new();
    let mut bytes = 0;
    let mut ids = std::collections::HashSet::new();
    for file in files {
        bytes += fs::metadata(&file)?.len();
        ensure!(bytes <= 25 * 1024 * 1024, "Session archive exceeds 25 MiB");
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(file)?)?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&raw)?));
        let s: LegacySession = serde_json::from_value(raw)?;
        storage_key(&s.id)?;
        ensure!(ids.insert(s.id.clone()), "Duplicate import ID");
        chrono::DateTime::parse_from_rfc3339(&s.created_at)?;
        chrono::DateTime::parse_from_rfc3339(&s.updated_at)?;
        ensure!(s.messages.len() <= 10000, "Too many messages");
        for m in &s.messages {
            ensure!(
                matches!(m.role.as_str(), "user" | "assistant" | "system"),
                "Unsupported import role"
            );
            chrono::DateTime::parse_from_rfc3339(&m.timestamp)?;
        }
        result.push((s, hash))
    }
    Ok(result)
}
impl Stores {
    // The caller supplies storage-key order. An unsuccessful attempt drops its
    // entire prefix before sleeping, including when replaying a pending batch.
    fn lock_batch_keys(&self, keys: &[(&str, bool, bool)]) -> Result<Vec<fs::File>> {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let attempt = layout::without_lifecycle_wait(|| {
                let mut guards = Vec::new();
                for &(key, exclusive, allocate) in keys {
                    guards.extend(self.key_lock(key, exclusive, allocate)?);
                }
                Ok(guards)
            });
            match attempt {
                Err(e) if e.is::<layout::LifecycleBusy>() && layout::lifecycle_wait_allowed() => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(e).context("Session batch is busy; retry the operation");
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                result => return result,
            }
        }
    }

    pub(in crate::db::session_store) fn group_access(
        &self,
        keys: Vec<(&str, bool, bool)>,
    ) -> Result<Vec<fs::File>> {
        let bounded = keys.len() > 1;
        self.group_access_with(keys, bounded)
    }

    fn group_access_with(
        &self,
        mut keys: Vec<(&str, bool, bool)>,
        bounded: bool,
    ) -> Result<Vec<fs::File>> {
        keys.sort_by_key(|k| k.0);
        loop {
            let guards = match keys.as_slice() {
                &[(key, exclusive, allocate)] if !bounded => {
                    self.key_lock(key, exclusive, allocate)?
                }
                keys => self.lock_batch_keys(keys)?,
            };
            let mut batch = None;
            for &(key, _, _) in &keys {
                batch = self.batch_for(key)?;
                if batch.is_some() {
                    break;
                }
            }
            if let Some(batch) = batch {
                drop(guards);
                self.recover_batch(&batch)?;
                continue;
            }
            for &(key, exclusive, _) in &keys {
                if exclusive {
                    self.recover_key(key)?;
                }
            }
            return Ok(guards);
        }
    }
    pub(super) fn reconcile_batches(&self) -> Result<()> {
        let batches = self.global().read(|c| {
            let mut q = c.prepare("SELECT DISTINCT batch FROM import_members")?;
            let values = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(values)
        })?;
        for batch in batches {
            if let Err(e) = self.recover_batch(&batch) {
                let pending = self.global().read(|c| {
                    Ok(c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM import_members WHERE batch=?1)",
                        [&batch],
                        |r| r.get::<_, bool>(0),
                    )?)
                })?;
                if pending && e.downcast_ref::<QuarantineMove>().is_none() {
                    return Err(e);
                }
                tracing::warn!(%batch, error=%e, "Recovering import batch");
            }
        }
        Ok(())
    }
    fn batch_for(&self, key: &str) -> Result<Option<String>> {
        self.global().read(|c| {
            Ok(c.query_row(
                "SELECT batch FROM import_members WHERE storage_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?)
        })
    }
    fn recover_batch(&self, batch: &str) -> Result<()> {
        let keys = self.global().read(|c| {
            let mut q = c.prepare(
                "SELECT storage_key FROM import_members WHERE batch=?1 ORDER BY storage_key",
            )?;
            let k = q
                .query_map([batch], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(k)
        })?;
        for key in &keys {
            ensure!(layout::is_storage_key(key), "Invalid import key");
        }
        let keys: Vec<_> = keys.iter().map(|key| (key.as_str(), true, false)).collect();
        let _guards = self.lock_batch_keys(&keys)?;
        self.finish_batch(batch)
    }
    fn finish_batch(&self, batch: &str) -> Result<()> {
        let members = self.global().read(|c| {
            let mut q = c.prepare("SELECT l.operation,l.store_uuid,l.id,l.storage_key,l.incarnation,l.kind,l.metadata,m.digest,m.abort_reason
                FROM import_members m JOIN lifecycle l ON l.operation=m.operation WHERE m.batch=?1 ORDER BY m.storage_key")?;
            let rows = q
                .query_map([batch], |r| {
                    Ok((
                        Intent {
                            operation: r.get(0)?,
                            store: r.get(1)?,
                            id: r.get(2)?,
                            key: r.get(3)?,
                            incarnation: r.get(4)?,
                            kind: r.get(5)?,
                            metadata: r.get(6)?,
                        },
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        if members.is_empty() {
            return Ok(());
        }
        let mut ready = Vec::new();
        let mut reason = None;
        for (op, _, aborted) in &members {
            op.validate(self)?;
            if let Some(r) = aborted {
                reason = Some(r.clone());
                break;
            }
            let stage = op.stage(self);
            let live = op.live(self);
            let dir = if stage.try_exists()? { stage } else { live };
            match self.unsealed(&dir.join("session.db")) {
                Ok(false) => {}
                Ok(true) => {
                    reason = Some("Incomplete import stage".into());
                    break;
                }
                Err(e) => {
                    reason = Some(format!("Unreadable import stage: {e:#}"));
                    break;
                }
            }
            let meta: Metadata =
                serde_json::from_str(op.metadata.as_deref().context("Missing import metadata")?)?;
            match self.install(op, &meta, &dir) {
                Ok(p) => ready.push((meta, p)),
                Err(e) if self.pending(&op.key)?.is_none() => {
                    reason = Some(format!("Damaged import: {e:#}"));
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(reason) = reason {
            self.abort_batch(batch, &members, &reason)?;
            bail!("Import aborted: {reason}")
        }
        let mut rejected = false;
        self.global()
            .write("publish import batch", batch, true, |tx| {
                if let Some(Project::NewWorkspace(path)) =
                    ready.first().and_then(|(meta, _)| meta.project.as_ref())
                {
                    if Project::matching(tx, path)?.is_some() {
                        rejected = true;
                        bail!("Import project selection changed before publication");
                    }
                }
                let mut admission = Default::default();
                for ((op, digest, _), (meta, p)) in members.iter().zip(&ready) {
                    self.publish_row(tx, op, meta, p).inspect_err(|e| {
                        rejected = e.downcast_ref::<rusqlite::Error>().is_none()
                    })?;
                    self.include_session_workspace(tx, &op.id, &mut admission, None)?;
                    tx.execute("INSERT INTO legacy_session_imports SELECT id,?2,project_id,incarnation FROM conversations WHERE id=?1", [&op.id, digest])?;
                }
                tx.execute("DELETE FROM import_members WHERE batch=?1", [batch])?;
                self.step(Phase::BeforePublishCommit)?;
                Ok(admission)
            })
            .or_else(|e| {
                if rejected {
                    self.abort_batch(
                        batch,
                        &members,
                        &format!("Import publication failed: {e:#}"),
                    )?;
                }
                Err(e)
            })?;
        self.step(Phase::Published)
    }
    fn abort_batch(
        &self,
        batch: &str,
        members: &[(Intent, String, Option<String>)],
        reason: &str,
    ) -> Result<()> {
        self.global()
            .write("abort import batch", batch, true, |c| {
                c.execute(
                    "UPDATE import_members SET abort_reason=?2 WHERE batch=?1",
                    [batch, reason],
                )?;
                Ok(())
            })?;
        for (op, _, _) in members {
            op.validate(self)?;
            // Resume a journaled quarantine before marking the batch terminal.
            if let Some(meta) = op.metadata.as_ref() {
                let value: serde_json::Value = serde_json::from_str(meta)?;
                if let Some(q) = value.get("quarantine") {
                    self.finish_quarantine(op, &serde_json::from_value(q.clone())?)?;
                    continue;
                }
            }
            let stage = op.stage(self);
            let live = op.live(self);
            if stage.try_exists()? {
                self.quarantine(op, &stage, reason)?
            } else if live.try_exists()? {
                let own = (|| -> Result<bool> {
                    let _permit = Permits::take(1);
                    let db =
                        layout::open(&live.join("session.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    Ok(
                        db.query_row("SELECT store_uuid=?1 AND incarnation=?2 AND sealed_operation=?3 FROM manifest", [&op.store, &op.incarnation, &op.operation], |r| {
                            r.get(0)
                        })?,
                    )
                })();
                if own.unwrap_or(false) {
                    self.quarantine(op, &live, reason)?
                } else {
                    self.record_quarantine(op, reason)?
                }
            } else {
                self.record_quarantine(op, reason)?
            }
        }
        self.global()
            .write("finish aborted import", batch, true, |c| {
                c.execute("DELETE FROM import_members WHERE batch=?1", [batch])?;
                Ok(())
            })
    }
    pub fn import_json(
        &self,
        source: &Path,
        workspace: &Path,
        preview: bool,
    ) -> Result<ImportResult> {
        self.import_json_into_project(source, workspace, preview, None)
    }

    pub fn import_json_into_project(
        &self,
        source: &Path,
        workspace: &Path,
        preview: bool,
        target_project: Option<&str>,
    ) -> Result<ImportResult> {
        let archive = read_archive(source)?;
        let workspace = workspace.canonicalize()?;
        ensure!(workspace.is_dir(), "Project directory required");
        let keys = archive
            .iter()
            .map(|(s, _)| storage_key(&s.id))
            .collect::<Result<Vec<_>>>()?;
        let _guards = if preview {
            Vec::new()
        } else {
            self.group_access_with(
                keys.iter().map(|k| (k.as_str(), true, true)).collect(),
                true,
            )?
        };
        let mut result = ImportResult {
            project_id: String::new(),
            imported: vec![],
            skipped: vec![],
            backup: None,
        };
        let mut selected = Vec::new();
        let plan = self.global().read(|conn| {
            let snapshot = conn.unchecked_transaction()?;
            let c = &snapshot;
            let selection = if let Some(id) = target_project {
                let p = project_repo::require_trusted(c, id)?;
                ensure!(
                    !p.is_archived
                        && p.workspace_path.as_deref().is_some_and(|stored| {
                            let stored = Path::new(stored);
                            stored == workspace
                                || stored
                                    .canonicalize()
                                    .ok()
                                    .zip(workspace.canonicalize().ok())
                                    .is_some_and(|(left, right)| left == right)
                        }),
                    "Selected project is unavailable"
                );
                ProjectSelection::Existing(id.into())
            } else {
                ProjectSelection::Workspace(workspace.clone())
            };
            let plan = Project::select(c, &selection)?;
            if let Project::NewWorkspace(path) = &plan {
                ensure!(
                    !c.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE workspace_path=?1 AND is_archived=0)", [path], |r| r.get::<_, bool>(0))?,
                    "Workspace project selection changed"
                );
            }
            for (session, digest) in &archive {
                let previous = c
                    .query_row("SELECT digest,project_id,EXISTS(SELECT 1 FROM conversations c WHERE c.id=i.id AND c.incarnation=i.incarnation AND c.project_id=i.project_id)
                    FROM legacy_session_imports i WHERE i.id=?1", [&session.id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, bool>(2)?,
                        ))
                    })
                    .optional()?;
                if let Some((prior, project, exists)) = previous {
                    ensure!(
                        prior == *digest
                            && matches!(&plan,Project::Selected{id,..} if *id==project)
                            && exists,
                        "Import conflicts with previous import"
                    );
                    result.skipped.push(session.id.clone());
                    continue;
                }
                let options = CreateOptions {
                    title: Some(session.name.clone()),
                    workspace_mode: Some("code".into()),
                    project: Some(selection.clone()),
                    ..Default::default()
                };
                self.prepare(c, &session.id, &options, &Seed::Import(session), None)?;
                result.imported.push(session.id.clone());
                selected.push((session, digest, options))
            }
            Ok(plan)
        })?;
        for (session, _, _) in &selected {
            self.check_destination(&session.id)?;
        }
        result.project_id = match &plan {
            Project::Selected { id, .. } => id.clone(),
            // Preview historically returns a proposed ID with no visible project.
            Project::NewWorkspace(_) if preview => Uuid::new_v4().to_string(),
            Project::NewWorkspace(_) => String::new(),
        };
        if preview || selected.is_empty() {
            return Ok(result);
        }
        let batch = Uuid::new_v4().to_string();
        let backup = self.root.layout().backups().join(&batch);
        fs::create_dir_all(&backup)?;
        self.global().read(|c| {
            c.execute(
                "VACUUM INTO ?1",
                [backup.join("global.db").to_string_lossy().as_ref()],
            )?;
            Ok(())
        })?;
        let manifest = backup.join("manifest.json");
        fs::write(
            &manifest,
            serde_json::to_vec(
                &serde_json::json!({"store":self.global().uuid,"batch":batch,"new_sessions":result.imported,"existing_shards_modified":[]}),
            )?,
        )?;
        for name in ["global.db", "manifest.json"] {
            fs::File::options()
                .write(true)
                .open(backup.join(name))?
                .sync_all()?;
        }
        sync_dir(&backup)?;
        sync_dir(backup.parent().unwrap())?;
        sync_dir(&self.root.path)?;
        result.backup = Some(manifest);
        let members = self
            .global()
            .write("prepare import batch", &batch, true, |tx| {
                let mut members = Vec::new();
                for (s, digest, options) in &selected {
                    let (op, meta) = self.prepare(tx, &s.id, options, &Seed::Import(s), None)?;
                    ensure!(meta.project.as_ref() == Some(&plan), "Import project selection changed during backup");
                    op.insert(tx, Some(&serde_json::to_string(&meta)?))?;
                    tx.execute("INSERT INTO import_members(storage_key,batch,operation,digest) VALUES(?1,?2,?3,?4)", params![op.key, batch, op.operation, digest])?;
                    members.push((op, meta));
                }
                Ok(members)
            })?;
        self.step(Phase::CreateIntent)?;
        for ((op, meta), (s, _, _)) in members.iter().zip(selected) {
            self.build(op, meta, None, Seed::Import(s))?;
        }
        self.finish_batch(&batch)?;
        result.project_id = self.global().read(|c| {
            Ok(c.query_row(
                "SELECT project_id FROM legacy_session_imports WHERE id=?1",
                [&result.imported[0]],
                |r| r.get(0),
            )?)
        })?;
        Ok(result)
    }
}
