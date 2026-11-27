mod copy;
pub(super) mod sync;
pub(super) use copy::workspace;
pub use sync::SyncHeader;
mod import;
use super::layout::{self, canonical_uuid, durable_move, storage_key, sync_dir, Permits, SESSION};
use super::{verify_manifest, SessionDb, Stores};
use crate::db::project_repo;
use anyhow::{bail, ensure, Context, Result};
pub use copy::Branch;
pub use import::ImportResult;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    BeforeCreateIntentCommit,
    CreateIntent,
    StageDirectory,
    BeforeSeal,
    Sealed,
    Checkpointed,
    Renamed,
    Installed,
    BeforePublishCommit,
    Published,
    BeforeDeleteIntentCommit,
    DeleteIntent,
    TrashRenamed,
    Trashed,
    BeforeTrashRemoval,
    Removed,
    Retired,
    ForkSnapshot,
    SyncContent,
    EditMetadata,
    DeliveryClaimed,
    DeliveryCommitted,
    DeliveryAcknowledged,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FirstMessage {
    pub origin: String,
    pub text: String,
    pub typed: Option<serde_json::Value>,
}
#[derive(Clone, Debug)]
pub enum ProjectSelection {
    Existing(String),
    Workspace(PathBuf),
}
#[derive(Clone, Debug, Default)]
pub struct CreateOptions {
    pub title: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace_mode: Option<String>,
    pub research: bool,
    pub project: Option<ProjectSelection>,
    pub first_message: Option<FirstMessage>,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    // Prior v1 intents recorded only the manifest count.
    #[serde(default)]
    initial_shape: Option<(bool, bool)>,
    #[serde(default)]
    copied: bool,
    #[serde(default)]
    source: Option<copy::Source>,
    title: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    mode: String,
    research: bool,
    created: String,
    project: Option<Project>,
    #[serde(default)]
    sync: Option<sync::SyncPublication>,
    #[serde(default)]
    feishu_chat: Option<String>,
    #[serde(default)]
    prepared: Option<crate::worktree::session::PreparedWorkspace>,
}
#[derive(Serialize, Deserialize, PartialEq)]
enum Project {
    Selected {
        id: String,
        folders: Vec<String>,
        #[serde(default)]
        workspace: Option<String>,
    },
    NewWorkspace(String),
}

impl Project {
    fn select(conn: &Connection, selection: &ProjectSelection) -> Result<Self> {
        let id = match selection {
            ProjectSelection::Existing(id) => id.clone(),
            ProjectSelection::Workspace(path) => {
                let folder =
                    project_repo::normalize_folders(&[path.to_string_lossy().into_owned()])?
                        .remove(0);
                match Self::matching(conn, &folder)? {
                    Some(id) => id,
                    None => return Ok(Self::NewWorkspace(folder)),
                }
            }
        };
        let p = project_repo::require_trusted(conn, &id)?;
        ensure!(!p.is_archived, "Project is archived");
        Ok(Self::Selected {
            workspace: p.workspace_path,
            id,
            folders: p.folders.into_iter().map(|f| f.path).collect(),
        })
    }

    fn matching(c: &Connection, path: &str) -> Result<Option<String>> {
        Ok(c.query_row(
            "SELECT p.id FROM projects p JOIN project_folders f ON f.project_id=p.id
            WHERE p.is_archived=0 AND p.workspace_path=?1 AND f.path=?1 AND f.trusted=1
            AND (SELECT count(*) FROM project_folders WHERE project_id=p.id)=1
            ORDER BY p.updated_at DESC,p.id LIMIT 1",
            [path],
            |r| r.get(0),
        )
        .optional()?)
    }

    fn publish(&self, tx: &Connection, now: &str) -> Result<(String, Option<String>)> {
        match self {
            Self::Selected {
                id,
                folders,
                workspace,
            } => {
                let p = project_repo::require_trusted(tx, id)?;
                ensure!(
                    !p.is_archived
                        && workspace
                            .as_ref()
                            .is_none_or(|w| p.workspace_path.as_ref() == Some(w))
                        && p.folders.iter().map(|f| &f.path).eq(folders),
                    "Project selection changed"
                );
                Ok((id.clone(), p.workspace_path))
            }
            Self::NewWorkspace(path) => {
                ensure!(
                    project_repo::normalize_folders(std::slice::from_ref(path))?[0] == *path,
                    "Workspace path changed"
                );
                if let Some(id) = Self::matching(tx, path)? {
                    return Ok((id, Some(path.clone())));
                }
                let conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE workspace_path=?1 AND is_archived=0)",
                    [path], |r| r.get(0))?;
                ensure!(!conflict, "Workspace project selection changed");
                let id = Uuid::new_v4().to_string();
                let name = Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                project_repo::insert_project(
                    tx,
                    &id,
                    &name,
                    None,
                    None,
                    Some(path),
                    false,
                    now,
                    now,
                )?;
                tx.execute("INSERT INTO project_folders VALUES(?1,?2,1,0)", [&id, path])?;
                Ok((id, Some(path.clone())))
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
enum QuarantineSource {
    Stage,
    Live,
    Trash,
}
#[derive(Serialize, Deserialize)]
struct Quarantine {
    source: QuarantineSource,
    reason: String,
}

pub(super) struct Intent {
    operation: String,
    store: String,
    id: String,
    key: String,
    incarnation: String,
    kind: String,
    metadata: Option<String>,
}
impl Intent {
    fn new(stores: &Stores, id: &str, key: &str, incarnation: String, kind: &str) -> Self {
        Self {
            operation: Uuid::new_v4().to_string(),
            store: stores.global().uuid.clone(),
            id: id.into(),
            key: key.into(),
            incarnation,
            kind: kind.into(),
            metadata: None,
        }
    }
    fn insert(&self, tx: &Connection, metadata: Option<&str>) -> Result<()> {
        tx.execute(
            "INSERT INTO lifecycle(operation,store_uuid,id,storage_key,incarnation,kind,metadata)
            VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                self.operation,
                self.store,
                self.id,
                self.key,
                self.incarnation,
                self.kind,
                metadata
            ],
        )?;
        Ok(())
    }
    fn stage(&self, stores: &Stores) -> PathBuf {
        stores.sessions().join(".staging").join(&self.operation)
    }
    fn live(&self, stores: &Stores) -> PathBuf {
        stores.sessions().join(&self.key)
    }
    fn finish(&self, tx: &Connection) -> Result<()> {
        ensure!(tx.execute("DELETE FROM lifecycle WHERE operation=?1 AND store_uuid=?2 AND incarnation=?3 AND quarantine_reason IS NULL",
            [&self.operation,&self.store,&self.incarnation])? == 1, "Lifecycle intent changed");
        Ok(())
    }
    fn validate(&self, stores: &Stores) -> Result<()> {
        ensure!(
            self.store == stores.global().uuid
                && self.key == storage_key(&self.id)?
                && canonical_uuid(&self.incarnation)
                && canonical_uuid(&self.operation),
            "Invalid lifecycle identity"
        );
        Ok(())
    }
}

enum Seed<'a> {
    New,
    Chat(&'a str),
    Feishu(&'a str),
    Fork(&'a SessionDb, Branch<'a>, Option<&'a crate::services::lifecycle_events::NewSessionConfiguration>),
    Import(&'a import::LegacySession),
    Project(&'a Project, &'a crate::worktree::session::PreparedWorkspace),
    Sync(sync::SyncSeed<'a>),
}

impl FirstMessage {
    fn digest(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        // Typed first submissions retain their existing recovery identity.
        let content = if self.origin == "typed" {
            serde_json::to_vec(&(&self.text, &self.typed))?
        } else {
            serde_json::to_vec(&(&self.origin, &self.text, &self.typed))?
        };
        Ok(format!("{:x}", Sha256::digest(content)))
    }
}

impl SessionDb {
    /// A sealed chat can be published by any recovery reader before its submitter retries.
    /// Consume the receipt under the execution lease. Ordinary identical later prompts append.
    pub(super) fn consume_sealed_first(&self, first: Option<&FirstMessage>) -> Result<bool> {
        let Some(first) = first else { return Ok(false); };
        let digest = first.digest()?;
        let pending = self.read(|r| Ok(r.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM first_chat_receipts WHERE digest=?1)",
            [&digest], |row| row.get::<_, bool>(0),
        )?))?;
        if !pending {
            return Ok(false);
        }
        let typed = first.typed.as_ref().map(serde_json::to_string).transpose()?;
        self.write_named("consume sealed first chat", |w| {
            Ok(w.0.execute(
                "DELETE FROM first_chat_receipts WHERE digest=?1 AND message_id IN (
                    SELECT m.id FROM messages m LEFT JOIN message_user_content t ON t.message_id=m.id
                    WHERE m.id=first_chat_receipts.message_id AND m.role='user'
                      AND m.content=?2 AND t.content_json IS ?3)",
                params![digest, first.text, typed],
            )? == 1)
        })
    }
}

impl Stores {
    pub fn create(&self, id: &str, options: CreateOptions) -> Result<SessionDb> {
        let key = storage_key(id)?;
        let _locks = self.group_access(vec![(&key, true, true)])?;
        self.create_locked(id, options, Seed::New, None)
    }

    pub(super) fn create_chat_with_lease(
        &self,
        id: &str,
        options: CreateOptions,
    ) -> Result<(crate::services::session_messages::ExecutionLease, bool)> {
        use crate::services::session_messages::ExecutionLease;
        let key = storage_key(id)?;
        let locks = self.group_access(vec![(&key, true, true)])?;
        let incarnation = Uuid::new_v4().to_string();
        let mut lease = ExecutionLease::acquire(self.data_dir(), &incarnation)?
            .context("Invalid execution incarnation")?;
        let first = options.first_message.clone();
        let shard = self.create_locked(id, options, Seed::Chat(&incarnation), None)?;
        lease.address = Some(shard.address());
        drop(locks);
        lease.lifecycle = self.key_lock(&key, false, false)?;
        shard.check_directory()?;
        lease.lifecycle.extend(shard.execution_workspace_locks()?);
        let sealed = shard.consume_sealed_first(first.as_ref())?;
        Ok((lease, sealed))
    }

    pub fn feishu_conversation(&self,chat:&str,title:Option<String>)->Result<super::im::FeishuChatMapping> {
        if let Some(row)=self.feishu_mapping(chat)? {return Ok(row);}
        let id=uuid::Uuid::new_v4().to_string();
        let key=storage_key(&id)?;
        let _guard=self.group_access(vec![(&key,true,true)])?;
        let options=CreateOptions {title:Some(title.unwrap_or_else(||format!("飞书-{}",chat.chars().take(8).collect::<String>()))),..Default::default()};
        let result=self.create_locked(&id,options,Seed::Feishu(chat),None);
        if let Some(row)=self.feishu_mapping(chat)? {return Ok(row);}
        result?;
        anyhow::bail!("Feishu mapping disappeared during creation")
    }

    pub fn create_project_chat(
        &self,
        project_id: &str,
        title: Option<&str>,
        model: Option<&str>,
        mode: crate::db::conversation_repo::WorkspaceMode,
        branch: Option<&str>,
        use_worktree: bool,
    ) -> Result<crate::db::conversation_repo::ConversationRow> {
        let id = Uuid::new_v4().to_string();
        let key = storage_key(&id)?;
        let guards = self.group_access(vec![(&key, true, true)])?;
        let selection = ProjectSelection::Existing(project_id.to_owned());
        let plan = self.global().read(|c| Project::select(c, &selection))?;
        let Project::Selected {
            workspace: Some(source),
            ..
        } = &plan
        else {
            bail!("Project has no folder");
        };
        let prepared = crate::worktree::session::prepare(Path::new(source), branch, use_worktree)?;
        let result = self.create_locked(
            &id,
            CreateOptions {
                title: title.map(str::to_owned),
                model: model.map(str::to_owned),
                workspace_mode: Some(mode.as_str().into()),
                project: Some(selection),
                ..Default::default()
            },
            Seed::Project(&plan, &prepared),
            None,
        );
        if let Err(error) = result {
            // Recovery may still publish a sealed intent referencing this worktree.
            if self.pending(&key)?.is_none() && self.global().incarnation(&id, &key)?.is_none() {
                prepared.cleanup();
            }
            return Err(error);
        }
        drop(guards);
        self.global()
            .conversation(&id)?
            .context("Published project conversation unavailable")
    }

    fn prepare(
        &self,
        tx: &Connection,
        id: &str,
        options: &CreateOptions,
        seed: &Seed<'_>,
        source: Option<copy::Source>,
    ) -> Result<(Intent, Metadata)> {
        let key = storage_key(id)?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 OR storage_key=?2 UNION ALL SELECT 1 FROM lifecycle WHERE storage_key=?2 AND (quarantine_reason IS NULL OR kind='delete'))",[id,&key],|r|r.get(0))?;
        ensure!(!exists, "Session already exists or is fenced");
        let incarnation = match seed {
            Seed::Chat(reserved) => (*reserved).to_owned(),
            _ => Uuid::new_v4().to_string(),
        };
        let op = Intent::new(self, id, &key, incarnation, "create");
        let mode = options
            .workspace_mode
            .clone()
            .unwrap_or_else(|| "chat".into());
        ensure!(
            matches!(mode.as_str(), "chat" | "work" | "study" | "code"),
            "Invalid workspace mode"
        );
        let project = options
            .project
            .as_ref()
            .map(|p| Project::select(tx, p))
            .transpose()?;
        let prepared = if let Seed::Project(expected, prepared) = seed {
            ensure!(
                project.as_ref() == Some(*expected),
                "Project changed while preparing the workspace"
            );
            Some((*prepared).clone())
        } else {
            None
        };
        let copied = !matches!(seed, Seed::New | Seed::Chat(_) | Seed::Feishu(_) | Seed::Project(..));
        let meta = Metadata {
            feishu_chat: if let Seed::Feishu(chat)=seed {Some(chat.to_string())} else {None},
            initial_shape: (!copied).then(|| {
                (
                    options.first_message.is_some(),
                    options
                        .first_message
                        .as_ref()
                        .is_some_and(|m| m.typed.is_some()),
                )
            }),
            copied,
            source,
            title: options.title.clone(),
            model: options.model.clone(),
            provider: options.provider.clone(),
            mode,
            research: options.research,
            created: match seed {
                Seed::Import(s) => s.created_at.clone(),
                Seed::Sync(s) => s.header.created_at.clone(),
                _ => chrono::Utc::now().to_rfc3339(),
            },
            sync: match seed {
                Seed::Sync(s) => Some(sync::SyncPublication {
                    digest: s.digest.into(),
                    workspace_path: s.header.workspace_path.clone(),
                    pinned: s.header.pinned,
                    archived: s.header.archived,
                }),
                _ => None,
            },
            project,
            prepared,
        };
        Ok((op, meta))
    }

    // The lifecycle lock fences normal creators. Check the filesystem outside G.
    fn check_destination(&self, id: &str) -> Result<()> {
        ensure!(
            !self.sessions().join(storage_key(id)?).try_exists()?,
            "Session directory already exists"
        );
        Ok(())
    }

    fn create_locked(
        &self,
        id: &str,
        options: CreateOptions,
        seed: Seed<'_>,
        source: Option<copy::Source>,
    ) -> Result<SessionDb> {
        self.check_destination(id)?;
        let (op, meta) = self.global().write("create intent", id, true, |tx| {
            let (op, meta) = self.prepare(tx, id, &options, &seed, source.clone())?;
            op.insert(tx, Some(&serde_json::to_string(&meta)?))?;
            self.step(Phase::BeforeCreateIntentCommit)?;
            Ok((op, meta))
        })?;
        self.step(Phase::CreateIntent)?;
        self.build(&op, &meta, options.first_message.as_ref(), seed)?;
        self.publish(&op, &meta, &op.stage(self))?;
        Ok(SessionDb {
            stores: self.clone(),
            id: id.into(),
            key: op.key,
            incarnation: op.incarnation,
        })
    }

    fn build(
        &self,
        op: &Intent,
        meta: &Metadata,
        first: Option<&FirstMessage>,
        seed: Seed<'_>,
    ) -> Result<()> {
        let id = &op.id;
        let created = &meta.created;
        use crate::services::memory_files::prompt::Admission;
        let roots: Vec<_> = match &meta.project {
            Some(Project::Selected { folders, .. }) => folders.iter().map(PathBuf::from).collect(),
            Some(Project::NewWorkspace(folder)) => vec![PathBuf::from(folder)],
            None => Vec::new(),
        };
        let stage = op.stage(self);
        fs::create_dir(&stage)?;
        let session_dir = self.sessions().join(&op.key).join("memory");
        let agent = match &seed {
            Seed::Fork(source, kind, _) if !matches!(kind, Branch::Child | Branch::Clear) => {
                crate::memory::files::copy_session_memory(
                    self.data_dir(),
                    &source.memory_directory(),
                    &stage.join("memory"),
                )?;
                source.read(|r| r.setting(super::settings::MEMORY_AGENT))?
            }
            _ => None,
        };
        let memory = Admission::starting(
            self,
            &roots,
            &op.incarnation,
            session_dir.clone(),
            agent.clone(),
        )?
        .staged_session(stage.join("memory"));
        sync_dir(stage.parent().unwrap())?;
        self.step(Phase::StageDirectory)?;
        {
            let _permit = Permits::take(if matches!(seed, Seed::Fork(..)) { 2 } else { 1 });
            let sealed = layout::initialize(&stage.join("session.db"), SESSION, |tx| {
                let n = i64::from(first.is_some());
                tx.execute(
                    "INSERT INTO manifest VALUES(1,?1,?2,?3,?4,?5,?5,?6,?6,NULL)",
                    params![op.store, id, op.incarnation, op.operation, created, n],
                )?;
                if let Some(agent) = &agent {
                    super::settings::MEMORY_AGENT.write(tx, agent)?;
                }
                if let Some(first) = first {
                    let content = first
                        .typed
                        .clone()
                        .unwrap_or_else(|| serde_json::Value::String(first.text.clone()));
                    if first.origin == "typed" {
                        crate::services::permission_authority::record_conn(
                            tx,
                            id,
                            crate::services::permission_authority::Source::User,
                            &content,
                        )?;
                    }
                    let message = crate::db::transcript::MessageIds::new(&op.incarnation).issue();
                    tx.execute("INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order)
                        VALUES(?1,?2,'user',?3,?4,0)",params![message,id,first.text,created])?;
                    tx.execute(
                        "INSERT INTO message_metadata VALUES(?1,?2)",
                        params![
                            message,
                            serde_json::json!({"promptOrigin": first.origin}).to_string()
                        ],
                    )?;
                    memory.freeze(tx, None)?;
                    if first.origin == "typed" {
                        super::history::mark_input_pending(tx, &message)?;
                    }
                    if let Some(typed) = &first.typed {
                        tx.execute(
                            "INSERT INTO message_user_content VALUES(?1,?2)",
                            params![message, serde_json::to_string(typed)?],
                        )?;
                    }
                    if matches!(seed, Seed::Chat(_)) {
                        tx.execute(
                            "INSERT INTO first_chat_receipts(message_id,digest) VALUES(?1,?2)",
                            params![message, first.digest()?],
                        )?;
                    }
                }
                match seed {
                    Seed::New | Seed::Chat(_) | Seed::Feishu(_) | Seed::Project(..) => {}
                    Seed::Fork(source, boundary, configuration) => copy::copy_snapshot(
                        source,
                        tx,
                        &op.id,
                        boundary,
                        configuration,
                        &memory,
                        meta.source
                            .as_ref()
                            .is_some_and(|s| s.provider_selection.is_some()),
                    )?,
                    Seed::Import(session) => session.populate(tx)?,
                    Seed::Sync(s) => s.populate(tx, &memory)?,
                }
                self.step(Phase::BeforeSeal)
            })?;
            self.step(Phase::Sealed)?;
            sealed.close().map_err(|(_, e)| e)?;
        }
        Ok(())
    }

    pub(super) fn pending(&self, key: &str) -> Result<Option<Intent>> {
        self.global().read(|c| {
            Ok(c.query_row(
                "SELECT operation,store_uuid,id,storage_key,incarnation,kind,metadata
            FROM lifecycle WHERE storage_key=?1 AND quarantine_reason IS NULL",
                [key],
                |r| {
                    Ok(Intent {
                        operation: r.get(0)?,
                        store: r.get(1)?,
                        id: r.get(2)?,
                        key: r.get(3)?,
                        incarnation: r.get(4)?,
                        kind: r.get(5)?,
                        metadata: r.get(6)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub(super) fn recover_key(&self, key: &str) -> Result<()> {
        let Some(op) = self.pending(key)? else {
            return Ok(());
        };
        op.validate(self)?;
        let stage = op.stage(self);
        let live = op.live(self);
        let dir = if stage.try_exists()? { &stage } else { &live };
        if let Some(meta) = op.metadata.as_deref() {
            let value: serde_json::Value = serde_json::from_str(meta)?;
            if let Some(disposition) = value.get("quarantine") {
                let disposition: Quarantine = serde_json::from_value(disposition.clone())?;
                return self.finish_quarantine(&op, &disposition);
            }
        }
        let recovered_quarantine = self.sessions().join(".quarantine").join(&op.operation);
        if recovered_quarantine.try_exists()? {
            // A pre-fix interrupted move has no saved source. Cover each
            // possible source directory before acknowledging the visible name.
            sync_dir(recovered_quarantine.parent().unwrap())?;
            sync_dir(&self.sessions())?;
            sync_dir(&self.sessions().join(".staging"))?;
            sync_dir(&self.sessions().join(".trash"))?;
            return self.record_quarantine(&op, "interrupted quarantine");
        }
        if op.kind == "delete" {
            return self.retire(&op);
        }
        ensure!(op.kind == "create", "Unknown intent");
        let visible = self.global().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE storage_key=?1 OR id=?2)",
                [&op.key, &op.id],
                |r| r.get::<_, bool>(0),
            )?)
        })?;
        if visible {
            if stage.try_exists()? {
                self.quarantine(&op, &stage, "occupied directory row")?;
            } else {
                self.record_quarantine(&op, "occupied directory row")?;
            }
            return Ok(());
        }
        let db = dir.join("session.db");
        let unsealed = match self.unsealed(&db) {
            Ok(value) => value,
            Err(error) => {
                return self.quarantine(&op, dir, &format!("invalid staged database: {error:#}"));
            }
        };
        if unsealed {
            if dir == &live && live.try_exists()? {
                return self.quarantine(&op, &live, "Missing or unsealed installed database");
            }
            // Only discard our private, unsealed stage. Never remove a final
            // directory on the strength of a create intent.
            if stage.try_exists()? {
                fs::remove_dir_all(&stage)?;
            }
            sync_dir(stage.parent().unwrap())?;
            return self
                .global()
                .write("discard unsealed create", &op.id, true, |tx| op.finish(tx));
        }
        let meta =
            serde_json::from_str(op.metadata.as_deref().context("Missing create metadata")?)?;
        self.publish(&op, &meta, dir)
    }

    fn unsealed(&self, db: &Path) -> Result<bool> {
        // A crash before initialization can leave a private stage without a file.
        match fs::symlink_metadata(db) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", db.display()))
            }
            Ok(_) => {}
        }
        let _permit = Permits::take(1);
        let c = layout::open(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // Schema, version and seal are installed in a single transaction.
        // A rolled back first transaction leaves a zero-schema SQLite file.
        Ok(c.query_row("SELECT application_id=0 AND NOT EXISTS(SELECT 1 FROM sqlite_schema) FROM pragma_application_id",[], |r|r.get(0))?)
    }

    fn quarantine(&self, op: &Intent, dir: &Path, reason: &str) -> Result<()> {
        let source = if dir == op.stage(self) {
            QuarantineSource::Stage
        } else if dir == op.live(self) {
            QuarantineSource::Live
        } else {
            QuarantineSource::Trash
        };
        let disposition = Quarantine {
            source,
            reason: reason.into(),
        };
        self.global().write("journal quarantine", &op.id, true, |tx| {
            tx.execute("UPDATE lifecycle SET metadata=json_set(COALESCE(metadata,'{}'),'$.quarantine',json(?1)) WHERE operation=?2",
                params![serde_json::to_string(&disposition)?,op.operation])?;
            Ok(())
        })?;
        self.finish_quarantine(op, &disposition)
    }

    fn finish_quarantine(&self, op: &Intent, disposition: &Quarantine) -> Result<()> {
        let source = match disposition.source {
            QuarantineSource::Stage => op.stage(self),
            QuarantineSource::Live => op.live(self),
            QuarantineSource::Trash => self.sessions().join(".trash").join(&op.incarnation),
        };
        let dest = self.sessions().join(".quarantine").join(&op.operation);
        if !dest.try_exists()? {
            layout::rename(&source, &dest).context(QuarantineMove)?;
        }
        sync_dir(dest.parent().unwrap())?;
        sync_dir(source.parent().unwrap())?;
        self.record_quarantine(op, &disposition.reason)
    }

    fn record_quarantine(&self, op: &Intent, reason: &str) -> Result<()> {
        self.global()
            .write("record quarantine", &op.id, true, |tx| {
                tx.execute(
                    "UPDATE lifecycle SET quarantine_reason=?1 WHERE operation=?2",
                    [reason, &op.operation],
                )?;
                Ok(())
            })
    }

    fn upgrade_pending(&self, op: &Intent, path: &Path) -> Result<()> {
        if layout::needs_upgrade(path, SESSION)? {
            let c = layout::open(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            verify_manifest(&c, &op.store, &op.id, &op.incarnation)?;
            if op.kind == "create" {
                ensure!(
                    c.query_row("SELECT sealed_operation FROM manifest", [], |r| r
                        .get::<_, String>(0))?
                        == op.operation,
                    "Seal mismatch"
                );
            }
            drop(c);
            layout::upgrade(path, SESSION)?;
        }
        Ok(())
    }
    fn install(&self, op: &Intent, meta: &Metadata, dir: &Path) -> Result<super::Projection> {
        let live = op.live(self);
        let db = dir.join("session.db");
        let inspect = || -> Result<_> {
            let _permit = Permits::take(1);
            self.upgrade_pending(op, &db)?;
            drop(super::admission::open(&db)?);
            let c = layout::open(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            layout::verify(&c, SESSION)?;
            layout::integrity(&c)?;
            let matches: bool = c.query_row(
                "SELECT store_uuid=?1 AND id=?2 AND incarnation=?3 AND sealed_operation=?4
                FROM manifest WHERE singleton=1",
                [&op.store, &op.id, &op.incarnation, &op.operation],
                |r| r.get(0),
            )?;
            if !matches {
                return Err(layout::InvalidFormat("Sealed identity mismatch".into()).into());
            }
            if meta.copied {
                let mut messages=c.prepare("SELECT id,conversation_id,role,content,thinking,created_at,is_compact_boundary,sort_order FROM messages")?;
                for row in messages.query_map([], crate::db::message_repo::row_to_message)? {
                    row?;
                }

                ensure!(
                    c.query_row("SELECT message_count=(SELECT count(*) FROM messages) FROM manifest WHERE singleton=1", [], |r| r.get::<_, bool>(0))?,
                    "Invalid copied message count"
                );
                let broken: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
                    [],
                    |r| r.get(0),
                )?;
                ensure!(!broken, "Invalid copied reference");
                let mut q = c.prepare("SELECT content_json FROM message_user_content")?;
                for value in q.query_map([], |r| r.get::<_, String>(0))? {
                    serde_json::from_str::<serde_json::Value>(&value?)?;
                }
            } else {
                let history_ok: bool = c.query_row(
                    "SELECT
                m.message_count IN (0,1) AND m.content_revision=m.message_count
                AND m.message_count=(SELECT count(*) FROM messages)
                AND NOT EXISTS(SELECT 1 FROM messages WHERE conversation_id IS NOT m.id
                    OR role IS NOT 'user' OR sort_order IS NOT 0 OR typeof(content)<>'text'
                    OR created_at IS NOT m.created_at)
                AND NOT EXISTS(SELECT 1 FROM pragma_foreign_key_check)
                AND (?1 IS NULL OR m.message_count=?1)
                AND (?2 IS NULL OR EXISTS(SELECT 1 FROM messages JOIN message_user_content
                    ON message_id=messages.id WHERE sort_order=0)=?2)
                FROM manifest m WHERE singleton=1",
                    params![
                        meta.initial_shape.map(|v| v.0),
                        meta.initial_shape.map(|v| v.1)
                    ],
                    |r| r.get(0),
                )?;
                if !history_ok {
                    return Err(layout::InvalidFormat(
                        "Initial history differs from sealed projection".into(),
                    )
                    .into());
                }
            }
            let identity = super::VerifiedShard {
                file: layout::file_identity(&db)?,
                schema: c.pragma_query_value(None, "schema_version", |r| r.get(0))?,
                external_edit: c.query_row(
                    "SELECT history_edit FROM projection_state WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?,
                incarnation: op.incarnation.clone(),
            };
            // A failure to read the secondary summary leaves the sealed intent retryable.
            let summary = super::history::projection(&c);
            drop(c);
            layout::checkpoint(&db, SESSION)?;
            *self.verification_slot(&op.key).lock().unwrap() = Some(identity);
            Ok(summary)
        };
        let projection = match inspect() {
            Ok(summary) => summary?,
            Err(e) => {
                self.quarantine(op, dir, &format!("invalid sealed database: {e:#}"))?;
                return Err(e);
            }
        };

        self.step(Phase::Checkpointed)?;
        if dir != live {
            if live.try_exists()? {
                self.quarantine(op, dir, "occupied destination")?;
                bail!("Session destination occupied");
            }
            layout::rename(dir, &live)?;
            self.step(Phase::Renamed)?;
        }
        // Also repeat both barriers on replay after the rename.
        sync_dir(&self.sessions())?;
        sync_dir(&self.sessions().join(".staging"))?;
        self.step(Phase::Installed)?;
        Ok(projection)
    }

    fn publish_row(
        &self,
        tx: &Connection,
        op: &Intent,
        meta: &Metadata,
        p: &super::Projection,
    ) -> Result<()> {
        let (project, workspace) = match &meta.project {
            None => (None, None),
            Some(selection) => {
                let (id, path) = selection.publish(tx, &meta.created)?;
                (Some(id), path)
            }
        };
        tx.execute("INSERT INTO conversations(id,storage_key,incarnation,title,model,provider,workspace_path,project_id,
            workspace_mode,research_mode,created_at,updated_at,message_count,content_revision,first_prompt,latest_prompt,transcript_bytes,last_turn_status,git_branch,request_count,input_tokens,output_tokens,total_tokens)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",params![op.id,op.key,op.incarnation,meta.title,meta.model,meta.provider,workspace,project,meta.mode,meta.research,meta.created,p.activity,p.count,p.revision,p.first_prompt,p.latest_prompt,p.transcript_bytes,p.status,p.git_branch,p.request_count,p.input_tokens,p.output_tokens,p.total_tokens])?;
        if let Some(prepared) = &meta.prepared {
            let project = project.as_deref().context("Missing prepared project")?;
            let source = workspace
                .as_deref()
                .context("Missing prepared project folder")?;
            prepared.save(tx, &op.id, project, source)?;
            tx.execute(
                "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
                params![prepared.path.to_string_lossy(), op.id],
            )?;
        }
        if let Some(chat) = &meta.feishu_chat {
            tx.execute("INSERT INTO feishu_chat_mappings(chat_id,conversation_id,title,created_at,last_active_at) VALUES(?1,?2,?3,?4,?4)", params![chat,op.id,meta.title,meta.created])?;
        }
        if let Some(sync) = &meta.sync {
            let synced_workspace=sync::workspace(tx,&op.id,project.as_deref(),sync.workspace_path.as_deref())?;
            tx.execute("UPDATE conversations SET pinned=?1,archived=?2,workspace_path=coalesce(?3,workspace_path) WHERE id=?4",
                params![sync.pinned,sync.archived,synced_workspace,op.id])?;
            tx.execute(
                "INSERT INTO sync_publications VALUES(?1,?2,?3)",
                [&op.id, &sync.digest, &op.incarnation],
            )?;
        }
        if let Some(saved) = &meta.source {
            let source = &saved.id;
            let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND incarnation=?2 AND metadata_revision=?3)",params![source,saved.incarnation,saved.revision],|r|r.get(0))?;
            ensure!(
                valid && copy::binding(tx, source)? == saved.binding,
                "Source binding changed"
            );
            if let Some(binding) = &saved.provider_selection {
                binding.publish(tx, source, &op.id)?;
            }
            for (table, cols) in [
                (
                    "conversation_worktrees",
                    "project_id,source_path,repo_path,path,branch",
                ),
                ("conversation_worktree_backends", "backend,metadata"),
            ] {
                tx.execute(&format!("INSERT INTO {table}(conversation_id,{cols}) SELECT ?1,{cols} FROM {table} WHERE conversation_id=?2"),[&op.id,source])?;
            }
            tx.execute(
                "UPDATE conversations SET workspace_path=w.path FROM conversation_worktrees w
                WHERE id=?1 AND w.conversation_id=id AND w.project_id=conversations.project_id",
                [&op.id],
            )?;
            if saved.child && project.is_none() {
                tx.execute("INSERT INTO conversation_private_workspace_bindings
                    SELECT ?1,coalesce(b.parent_id,c.id),coalesce(b.parent_incarnation,c.incarnation)
                    FROM conversations c LEFT JOIN conversation_private_workspace_bindings b ON b.child_id=c.id WHERE c.id=?2",[&op.id,source])?;
            }
            if saved.child {
                tx.execute(
                    "INSERT INTO session_execution_parents(child_id,parent_id,parent_incarnation)
                            SELECT ?1,id,incarnation FROM conversations WHERE id=?2",
                    [&op.id, source],
                )?;
                let root = self.root.layout().workspaces();
                let parent_path = copy::workspace(tx, &root, source)?;
                let child_path = copy::workspace(tx, &root, &op.id)?;
                let actual = parent_path.canonicalize()?;
                ensure!(
                    child_path.canonicalize()? == actual
                        && saved.expected_cwd.as_ref().is_none_or(|p| *p == actual),
                    "Child workspace binding changed"
                );
            }
        }
        op.finish(tx)?;
        Ok(())
    }

    fn publish(&self, op: &Intent, meta: &Metadata, dir: &Path) -> Result<()> {
        let projection = self.install(op, meta, dir)?;
        let live = op.live(self);
        let mut project_conflict = false;
        let result = self.global().write("publish session", &op.id, true, |tx| {
            self.publish_row(tx, op, meta, &projection)
                .inspect_err(|e| {
                    project_conflict = e.downcast_ref::<rusqlite::Error>().is_none()
                })?;
            let admission = self.admit_session_workspace(tx, &op.id)?;
            self.step(Phase::BeforePublishCommit)?;
            Ok(admission)
        });
        if let Err(e) = result {
            // Validation and constraint conflicts are permanent. IO, busy, and
            // other SQLite errors retain the sealed operation for retry.
            let sql = e.downcast_ref::<rusqlite::Error>();
            if project_conflict
                || sql.and_then(|e| e.sqlite_error_code())
                    == Some(rusqlite::ErrorCode::ConstraintViolation)
            {
                self.quarantine(op, &live, "publication validation failed")?;
            }
            return Err(e);
        }
        self.step(Phase::Published)
    }

    fn retire(&self, op: &Intent) -> Result<()> {
        self.forget_verification(&op.key);
        let live = op.live(self);
        let trash = self.sessions().join(".trash").join(&op.incarnation);
        let dir = if live.try_exists()? { &live } else { &trash };
        // Once the verified trash is released for unlink, a crash can leave
        // only some of its files. That authorization never applies to live.
        let cleanup_authorized = op
            .metadata
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()?
            .and_then(|value| value["cleanup_authorized"].as_bool())
            == Some(true);
        if (dir == &live || !cleanup_authorized) && dir.try_exists()? {
            let verify = || -> Result<()> {
                let path = dir.join("session.db");
                let _permit = Permits::take(1);
                self.upgrade_pending(op, &path)?;
                let conn = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                layout::verify(&conn, SESSION)?;
                layout::integrity(&conn)?;
                verify_manifest(&conn, &op.store, &op.id, &op.incarnation)
            };
            if let Err(error) = verify() {
                return self.quarantine(op, dir, &format!("invalid retiring database: {error:#}"));
            }
        }
        if live.try_exists()? {
            ensure!(!trash.try_exists()?, "Retired destination occupied");
            layout::rename(&live, &trash)?;
            self.step(Phase::TrashRenamed)?;
        }
        sync_dir(&self.sessions())?;
        sync_dir(trash.parent().unwrap())?;
        self.step(Phase::Trashed)?;
        if trash.try_exists()? {
            if !cleanup_authorized {
                self.global().write("authorize trash cleanup", &op.id, true, |tx| {
                    tx.execute("UPDATE lifecycle SET metadata=json_set(COALESCE(metadata,'{}'),'$.cleanup_authorized',json('true')) WHERE operation=?1",
                        [&op.operation])?;
                    Ok(())
                })?;
            }
            self.step(Phase::BeforeTrashRemoval)?;
            fs::remove_dir_all(&trash)?;
        }
        sync_dir(trash.parent().unwrap())?;
        self.step(Phase::Removed)?;
        self.global()
            .write("finish delete", &op.id, true, |tx| op.finish(tx))?;
        self.step(Phase::Retired)
    }

    pub(super) fn reconcile(&self) -> Result<()> {
        self.reconcile_batches()?;
        let keys = self.global().read(|c| {
            let mut q = c.prepare("SELECT storage_key FROM lifecycle WHERE quarantine_reason IS NULL AND storage_key NOT IN (SELECT storage_key FROM import_members) ORDER BY storage_key")?;
            let rows = q.query_map([],|r|r.get::<_,String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for key in keys {
            ensure!(layout::is_storage_key(&key), "Invalid journal storage key");
            if let Err(error) = self.group_access(vec![(&key, true, false)]) {
                // Quarantined operations are terminal. Windows sharing errors
                // stay journaled and can be retried independently of other IDs.
                if self.pending(&key)?.is_some() {
                    if !error.is::<QuarantineMove>() && !sharing_violation(&error) {
                        return Err(error);
                    }
                    tracing::warn!(%key, cause = %error, "Session remains hidden pending filesystem recovery");
                }
            }
        }
        self.quarantine_orphans()
    }

    fn owns_live_directory(&self, key: &str) -> Result<bool> {
        self.global().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE storage_key=?1
             UNION ALL SELECT 1 FROM lifecycle WHERE storage_key=?1 AND quarantine_reason IS NULL)",
                [key],
                |r| r.get(0),
            )?)
        })
    }

    fn move_proven_orphan(&self, dir: &Path, name: &str, staged: bool) -> Result<()> {
        let existing_lock = if staged {
            None
        } else {
            self.try_key_lock(name, true, false)?
        };
        if !staged && self.owns_live_directory(name)? {
            return Ok(());
        }
        let path = dir.join("session.db");
        let inspect = || -> Result<()> {
            let _permit = Permits::take(1);
            ensure!(
                fs::symlink_metadata(&path)?.is_file(),
                "not a regular session file"
            );
            let identity = |c: &Connection| -> Result<()> {
                let (store,id,incarnation,seal): (String,String,String,String) = c.query_row(
                    "SELECT store_uuid,id,incarnation,sealed_operation FROM manifest WHERE singleton=1",
                    [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                ensure!(
                    store == self.global().uuid
                        && canonical_uuid(&incarnation)
                        && canonical_uuid(&seal)
                        && if staged {
                            seal == name
                        } else {
                            storage_key(&id)? == name
                        },
                    "orphan identity does not match this store and directory"
                );
                Ok(())
            };
            // A read-only SQLite WAL open can create or modify a shared-memory
            // sidecar. Inspect a nonempty WAL with a private WAL index, even
            // when the only copy of the sealed manifest is in that WAL.
            let wal = dir.join("session.db-wal");
            if wal.try_exists()? {
                let meta = fs::symlink_metadata(&wal)?;
                ensure!(meta.is_file(), "not a regular WAL");
                if meta.len() > 0 {
                    let scratch = tempfile::Builder::new()
                        .prefix(INSPECTION_PREFIX)
                        .tempdir_in(self.sessions().join(".quarantine"))?;
                    let owner = scratch.path().join("store-uuid");
                    fs::write(&owner, &self.global().uuid)?;
                    fs::File::options().write(true).open(owner)?.sync_all()?;
                    sync_dir(scratch.path())?;
                    let copy = scratch.path().join("session.db");
                    fs::copy(&path, &copy)?;
                    fs::copy(wal, scratch.path().join("session.db-wal"))?;
                    let conn = layout::open(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    layout::verify_supported_session(&conn)?;
                    identity(&conn)?;
                    drop(conn);
                    scratch.close()?;
                    return Ok(());
                }
            }
            // An immutable probe never creates sidecars in a foreign directory.
            let mut url = reqwest::Url::from_file_path(&path)
                .map_err(|_| anyhow::anyhow!("Invalid database path"))?;
            url.set_query(Some("immutable=1"));
            let probe = Connection::open_with_flags(
                url.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let app: i32 = probe.pragma_query_value(None, "application_id", |r| r.get(0))?;
            ensure!(app == SESSION, "unrecognized application ID");
            identity(&probe)?;
            drop(probe);
            let conn = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            layout::verify_supported_session(&conn)?;
            identity(&conn)
        };
        if let Err(error) = inspect() {
            tracing::warn!(path = %dir.display(), reason = %error, "Leaving unverified session directory in place");
            return Ok(());
        }
        // Only our sweep can move an unowned live directory. A concurrent
        // creator must reject the occupied destination. Register a missing
        // lock only after proving ownership, then recheck G under that key.
        let _new_lock;
        if !staged && existing_lock.is_none() {
            _new_lock = self.key_lock(name, true, true)?;
            if self.owns_live_directory(name)? {
                return Ok(());
            }
        }
        durable_move(
            dir,
            &self
                .sessions()
                .join(".quarantine")
                .join(Uuid::new_v4().to_string()),
        )
    }

    fn quarantine_orphans(&self) -> Result<()> {
        let _sweep = layout::lock_existing(
            &self
                .root
                .layout()
                .lock_path("session")
                .join(".orphan-sweep"),
            true,
        )?;
        let quarantine = self.sessions().join(".quarantine");
        // The sweep lock also serializes cleanup after a killed WAL inspection.
        let mut has_quarantine = false;
        for entry in fs::read_dir(&quarantine)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(INSPECTION_PREFIX)
            {
                if entry.file_type()?.is_dir()
                    && owned_inspection(&entry.path(), &self.global().uuid)?
                {
                    fs::remove_dir_all(entry.path())?;
                    continue;
                }
                tracing::warn!(path = %entry.path().display(), "Leaving unverified inspection directory in place");
            }
            has_quarantine = true;
        }
        // Unjournaled orphans have no G intent to drive replay after rename.
        // A surviving destination can come from either the live or staging parent.
        if has_quarantine {
            sync_dir(&quarantine)?;
            sync_dir(&self.sessions())?;
            sync_dir(&self.sessions().join(".staging"))?;
        }
        for entry in fs::read_dir(self.sessions().join(".staging"))? {
            let entry = entry?;
            let operation = entry.file_name();
            let Some(operation) = operation.to_str().filter(|s| canonical_uuid(s)) else {
                tracing::warn!(path = %entry.path().display(), "Leaving unrecognized staging entry in place");
                continue;
            };
            // An operation's UUID is never reused. Its intent must already be durable
            // before its stage exists, so no live builder can own an unjournaled stage.
            let known = self.global().read(|c| Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM lifecycle WHERE operation=?1 AND quarantine_reason IS NULL)",
                [operation], |r| r.get::<_, bool>(0))?))?;
            if !known && entry.file_type()?.is_dir() {
                self.move_proven_orphan(&entry.path(), operation, true)?;
            } else if !known {
                tracing::warn!(path = %entry.path().display(), "Leaving unrelated staging entry in place");
            }
        }
        for entry in fs::read_dir(self.sessions())? {
            let entry = entry?;
            let key = entry.file_name();
            if matches!(key.to_str(), Some(".staging" | ".trash" | ".quarantine")) {
                continue;
            }
            let Some(key) = key.to_str().filter(|k| layout::is_storage_key(k)) else {
                tracing::warn!(path = %entry.path().display(), "Leaving unrecognized sessions entry in place");
                continue;
            };
            if !entry.file_type()?.is_dir() {
                tracing::warn!(path = %entry.path().display(), "Leaving unrelated sessions entry in place");
                continue;
            }
            if !self.owns_live_directory(key)? {
                self.move_proven_orphan(&entry.path(), key, false)?;
            }
        }
        Ok(())
    }
}

const INSPECTION_PREFIX: &str = ".orphan-inspection-";

fn owned_inspection(dir: &Path, store: &str) -> Result<bool> {
    let marker = dir.join("store-uuid");
    if !fs::symlink_metadata(&marker).is_ok_and(|m| m.is_file() && m.len() == store.len() as u64)
        || !fs::read(marker).is_ok_and(|contents| contents == store.as_bytes())
    {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || !matches!(
                entry.file_name().to_str(),
                Some("store-uuid" | "session.db" | "session.db-wal" | "session.db-shm")
            )
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Debug, thiserror::Error)]
#[error("Moving journaled shard into quarantine")]
struct QuarantineMove;

fn sharing_violation(e: &anyhow::Error) -> bool {
    cfg!(windows)
        && e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| matches!(e.raw_os_error(), Some(32 | 33)))
}

impl SessionDb {
    pub fn delete(&self) -> Result<()> {
        let s = &self.stores;
        let _locks = s.key_lock(&self.key, true, false)?;
        self.check_directory()?;
        self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |_| Ok(()))?;
        let op = Intent::new(s, &self.id, &self.key, self.incarnation.clone(), "delete");
        s.global().write("retire session", &self.id, true, |tx| {
            ensure!(
                tx.execute(
                    "DELETE FROM conversations WHERE id=?1 AND incarnation=?2",
                    [&self.id, &self.incarnation]
                )? == 1,
                "Stale session handle"
            );
            op.insert(tx, None)?;
            s.step(Phase::BeforeDeleteIntentCommit)
        })?;
        s.step(Phase::DeleteIntent)?;
        s.retire(&op)
    }
}
