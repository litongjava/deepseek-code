//! Durable same-project messages. Acknowledgement means committed context, not model processing.
use super::{EnvironmentCredentials, ServiceContext};
use crate::{
    db::conversation_repo,
    runtime::RuntimePaths,
};
#[cfg(test)]
use crate::db::BusyTimeoutGuard;
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};
const MAX_PENDING: i64 = 256;
const MAX_PENDING_BYTES: i64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub created_at: String,
    pub state: String,
    pub read: bool,
    pub blocked_reason: Option<String>,
}
pub(crate) fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS session_inbox(id TEXT PRIMARY KEY,sender TEXT NOT NULL,recipient TEXT NOT NULL,project_source TEXT NOT NULL,content TEXT NOT NULL,created_at TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'queued',read INTEGER NOT NULL DEFAULT 0,notified INTEGER NOT NULL DEFAULT 0,blocked_reason TEXT,consumer_generation TEXT,committed_at TEXT); CREATE INDEX IF NOT EXISTS session_inbox_recipient ON session_inbox(recipient,state); CREATE TABLE IF NOT EXISTS session_inbox_consumers(session_id TEXT PRIMARY KEY,generation TEXT NOT NULL,pid INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS session_inbox_operations(operation_id TEXT PRIMARY KEY,message_id TEXT NOT NULL,sender TEXT NOT NULL,recipient TEXT NOT NULL,content_hash TEXT NOT NULL,created_at TEXT NOT NULL);")?;
    Ok(())
}
pub fn context(root: &Path) -> Result<ServiceContext> {
    ServiceContext::open(
        RuntimePaths::new(root.to_path_buf())?,
        Arc::new(EnvironmentCredentials),
    )
}
fn valid_id(id: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "Message endpoint must be a session UUID"
    );
    Ok(())
}
pub(crate) fn project(conn: &Connection, root: &Path, id: &str) -> Result<String> {
    valid_id(id)?;
    let row =
        conversation_repo::conversation_metadata(conn, id)?.context("Message session not found")?;
    ensure!(!row.archived, "Message session is archived");
    ensure!(
        row.workspace_mode == "code",
        "Messages require a Code session"
    );
    let project_id = row
        .project_id
        .as_deref()
        .context("Message session has no source project")?;
    // Resolve validates trust and the managed-worktree mapping, but returns an
    // adapted worktree project. Identity must come from the original project.
    crate::native_engine::workspace::resolve_workspace(
        conn,
        &root.join("data/workspaces"),
        id,
        None,
    )?;
    let project = crate::db::project_repo::get_project(conn, project_id)?
        .context("Message project not found")?;
    ensure!(!project.is_archived, "Message project is archived");
    let source = Path::new(
        project
            .workspace_path
            .as_deref()
            .context("Message project has no source directory")?,
    )
    .canonicalize()?;
    let source = source.to_string_lossy().into_owned();
    Ok(if cfg!(windows) {
        source.to_lowercase()
    } else {
        source
    })
}
pub(crate) fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        sender: row.get(1)?,
        recipient: row.get(2)?,
        content: row.get(3)?,
        created_at: row.get(4)?,
        state: row.get(5)?,
        read: row.get(6)?,
        blocked_reason: row.get(7)?,
    })
}
const SELECT: &str =
    "SELECT id,sender,recipient,content,created_at,state,read,blocked_reason FROM session_inbox";
fn super_agent_pair(conn: &Connection, first: &str, second: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sa_tasks WHERE (router_session_id=?1 AND (coder_session_id=?2 OR reviewer_session_id=?2)) OR (router_session_id=?2 AND (coder_session_id=?1 OR reviewer_session_id=?1)) OR (coder_session_id=?1 AND reviewer_session_id=?2) OR (coder_session_id=?2 AND reviewer_session_id=?1))",
        params![first, second],
        |row| row.get(0),
    )
}

fn content_digest(content: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn message_by_id(conn: &Connection, id: &str) -> Result<Message> {
    conn.query_row(&format!("{SELECT} WHERE id=?1"), [id], row)
        .optional()?
        .context("Delivered message is missing")
}

pub fn send(
    context: &impl PeerMailbox,
    sender: &str,
    recipient: &str,
    content: &str,
) -> Result<Message> {
    context.deliver(None, sender, recipient, content)
}

/// Delivers a message at most once for a durable operation ID. A retry after a
/// crash between delivery and the caller's event append returns the original
/// message instead of queueing a duplicate.
pub fn send_once(
    context: &impl PeerMailbox,
    operation_id: &str,
    sender: &str,
    recipient: &str,
    content: &str,
) -> Result<Message> {
    ensure!(
        !operation_id.trim().is_empty(),
        "Message operation ID is required"
    );
    context.deliver(Some(operation_id), sender, recipient, content)
}

pub trait PeerMailbox {
    fn deliver(
        &self,
        operation: Option<&str>,
        sender: &str,
        recipient: &str,
        content: &str,
    ) -> Result<Message>;
    fn inbox(&self, recipient: &str, unread_only: bool) -> Result<Vec<Message>>;
    fn pending_count(&self, recipient: &str) -> Result<i64>;
    fn sent(&self, sender: &str) -> Result<Vec<Message>>;
    fn mark_read(&self, recipient: &str, id: &str) -> Result<()>;
    fn notices(&self, recipient: &str) -> Result<Vec<String>>;
}
pub fn inbox(
    context: &impl PeerMailbox,
    recipient: &str,
    unread_only: bool,
) -> Result<Vec<Message>> {
    context.inbox(recipient, unread_only)
}
pub fn pending_count(context: &impl PeerMailbox, recipient: &str) -> Result<i64> {
    context.pending_count(recipient)
}
pub fn sent(context: &impl PeerMailbox, sender: &str) -> Result<Vec<Message>> {
    context.sent(sender)
}
pub fn mark_read(context: &impl PeerMailbox, recipient: &str, id: &str) -> Result<()> {
    context.mark_read(recipient, id)
}
pub fn notices(context: &impl PeerMailbox, recipient: &str) -> Result<Vec<String>> {
    context.notices(recipient)
}
impl PeerMailbox for ServiceContext<crate::db::session_store::Stores> {
    fn deliver(
        &self,
        operation: Option<&str>,
        sender: &str,
        recipient: &str,
        content: &str,
    ) -> Result<Message> {
        let operation = operation
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.db.open_session(sender)?.send_peer_message(
            &self.db.open_session(recipient)?,
            &operation,
            content,
        )
    }
    fn inbox(&self, recipient: &str, unread_only: bool) -> Result<Vec<Message>> {
        self.db.open_session(recipient)?.inbox(unread_only)
    }
    fn pending_count(&self, recipient: &str) -> Result<i64> {
        self.db.open_session(recipient)?.pending_peer_count()
    }
    fn sent(&self, sender: &str) -> Result<Vec<Message>> {
        self.db.open_session(sender)?.sent()
    }
    fn mark_read(&self, recipient: &str, id: &str) -> Result<()> {
        self.db.open_session(recipient)?.mark_peer_read(id)
    }
    fn notices(&self, recipient: &str) -> Result<Vec<String>> {
        self.db.open_session(recipient)?.peer_notices()
    }
}
impl PeerMailbox for ServiceContext<crate::db::DbManager> {
    fn deliver(
        &self,
        operation_id: Option<&str>,
        sender: &str,
        recipient: &str,
        content: &str,
    ) -> Result<Message> {
        ensure!(
            !content.trim().is_empty() && content.len() <= 64 * 1024,
            "Message must contain 1..65536 bytes of nonempty text"
        );
        ensure!(
            sender != recipient,
            "SendMessage requires a different recipient session"
        );
        let digest = content_digest(content);
        self.db.with_conn(|conn|->Result<_>{
        schema(conn)?;let tx=rusqlite::Transaction::new_unchecked(conn,rusqlite::TransactionBehavior::Immediate)?;
        if let Some(operation_id) = operation_id {
            if let Some((message_id, stored_sender, stored_recipient, stored_digest)) = tx
                .query_row(
                    "SELECT message_id,sender,recipient,content_hash FROM session_inbox_operations WHERE operation_id=?1",
                    [operation_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
                )
                .optional()?
            {
                ensure!(
                    stored_sender == sender
                        && stored_recipient == recipient
                        && stored_digest == digest,
                    "Message operation {operation_id} was already used for different content"
                );
                let message = message_by_id(&tx, &message_id)?;
                tx.commit()?;
                return Ok(message);
            }
        }
        let source=project(&tx,&self.paths.data_dir,sender)?;
        let recipient_source=project(&tx,&self.paths.data_dir,recipient)?;
        // A Super Agent task deliberately pairs a Router with sessions in
        // isolated workspaces. The durable task record is the authorization for
        // exactly those pairs; arbitrary cross-project messages stay rejected.
        ensure!(source==recipient_source||super_agent_pair(&tx,sender,recipient)?,"Recipient belongs to another project");
        let (count,bytes):(i64,i64)=tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(content AS BLOB))),0) FROM session_inbox WHERE recipient=?1 AND state='queued'",[recipient],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(count<MAX_PENDING && bytes+content.len() as i64<=MAX_PENDING_BYTES,"Recipient inbox is full; inspect pending messages before sending more");
        let message=Message{id:uuid::Uuid::new_v4().to_string(),sender:sender.into(),recipient:recipient.into(),content:content.into(),created_at:chrono::Utc::now().to_rfc3339(),state:"queued".into(),read:false,blocked_reason:None};
        tx.execute("INSERT INTO session_inbox(id,sender,recipient,project_source,content,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![message.id,sender,recipient,source,content,message.created_at])?;
        if let Some(operation_id) = operation_id {
            tx.execute(
                "INSERT INTO session_inbox_operations(operation_id,message_id,sender,recipient,content_hash,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![operation_id, message.id, sender, recipient, digest, message.created_at],
            )?;
        }
        tx.commit()?;Ok(message)
    })?
    }
    fn inbox(&self, recipient: &str, unread_only: bool) -> Result<Vec<Message>> {
        valid_id(recipient)?;
        self.db.with_conn(|conn| -> Result<_> {
        schema(conn)?;
        ensure!(
            conversation_repo::get_conversation(conn, recipient)?.is_some(),
            "Recipient session not found"
        );
        let mut q = conn.prepare(&format!(
            "{SELECT} WHERE recipient=?1 AND (?2=0 OR read=0) ORDER BY (state='queued') DESC, rowid DESC LIMIT 1000"
        ))?;
        let result = q
            .query_map(params![recipient, unread_only], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    })?
    }
    fn pending_count(&self, recipient: &str) -> Result<i64> {
        valid_id(recipient)?;
        self.db.with_conn(|conn| -> Result<_> {
            schema(conn)?;
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM session_inbox WHERE recipient=?1 AND state='queued'",
                [recipient],
                |row| row.get(0),
            )?)
        })?
    }
    fn sent(&self, sender: &str) -> Result<Vec<Message>> {
        valid_id(sender)?;
        self.db.with_conn(|conn| -> Result<_> {
            schema(conn)?;
            let mut query = conn.prepare(&format!(
                "{SELECT} WHERE sender=?1 ORDER BY rowid DESC LIMIT 1000"
            ))?;
            let result = query
                .query_map([sender], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(result)
        })?
    }
    fn mark_read(&self, recipient: &str, id: &str) -> Result<()> {
        valid_id(recipient)?;
        valid_id(id)?;
        self.db.with_conn(|conn| -> Result<_> {
            schema(conn)?;
            ensure!(
                conn.execute(
                    "UPDATE session_inbox SET read=1 WHERE id=?1 AND recipient=?2",
                    params![id, recipient]
                )? == 1,
                "Message not found in recipient inbox"
            );
            Ok(())
        })?
    }
    /// At-most-once UI notices; the durable inbox remains readable after a UI crash.
    fn notices(&self, recipient: &str) -> Result<Vec<String>> {
        valid_id(recipient)?;
        self.db.with_conn(|conn| -> Result<_> {
            schema(conn)?;
            // Empty polls stay read-only and never compete for the writer lock.
            let pending: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_inbox WHERE recipient=?1 AND notified=0)",
                [recipient],
                |row| row.get(0),
            )?;
            if !pending {
                return Ok(Vec::new());
            }
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let rows = {
                let mut query = tx.prepare(&format!(
                    "{SELECT} WHERE recipient=?1 AND notified=0 ORDER BY rowid LIMIT 16"
                ))?;
                let rows = query
                    .query_map([recipient], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let mut result = Vec::new();
            for message in rows {
                tx.execute(
                    "UPDATE session_inbox SET notified=1 WHERE id=?1",
                    [&message.id],
                )?;
                let sender =
                    super::background_sessions::list(&self.paths.data_dir, Some(recipient))?
                        .into_iter()
                        .find(|job| job.session_id == message.sender)
                        .map(|job| job.title)
                        .or(conversation_repo::get_conversation(&tx, &message.sender)?
                            .and_then(|row| row.title)
                            .filter(|title| {
                                !title.trim().is_empty()
                                    && !title.eq_ignore_ascii_case("CLI session")
                            }))
                        .unwrap_or_else(|| "Agent".into());
                result.push(format!(
                    "Agent message from {sender} · {}\n{}",
                    message.state, message.content
                ));
            }
            tx.commit()?;
            Ok(result)
        })?
    }
}
fn lock_path(root: &Path, session: &str) -> Result<PathBuf> {
    valid_id(session)?;
    Ok(root
        .join("state/locks/session-message")
        .join(format!("{session}.lock")))
}
pub fn receiver_active(root:&Path,session:&str)->Result<bool>{
    context(root)?.db.message_receiver_active(session)
}
impl crate::db::DbManager {
    pub(crate) fn message_receiver_active(&self,session:&str)->Result<bool>{receiver_active_by_key(self.data_dir(),session)}
}
pub(crate) fn receiver_active_by_key(root: &Path, session: &str) -> Result<bool> {
    let path = lock_path(root, session)?;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    match crate::fs::try_lock_file(&file) {
        Ok(()) => {
            #[cfg(test)]
            lock_observer::inspect(&file);
            // Same obligation as the presence probe.
            if let Err(error) = crate::fs::unlock_file(&file) {
                tracing::warn!("Could not release message receiver probe lock: {error}");
            }
            Ok(false)
        }
        // A held lock belongs to the real consumer; the probe must not unlock it.
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}
/// Releases a lock this process owns, now rather than at descriptor close.
///
/// `flock` ownership belongs to the open file description, so a duplicated or
/// inherited descriptor keeps the lock alive after the owner drops its `File`.
/// Only the process that took the lock may release it: a forked child inherits
/// the owner PID value alongside the descriptor and must not unlock a parent
/// that is still alive. Descriptor close remains the fallback for abnormal exit.
fn release_owned(file: &File, owner: u32) {
    if std::process::id() != owner {
        return;
    }
    if let Err(error) = crate::fs::unlock_file(file) {
        // Reported without panicking; this also runs from `Drop`.
        tracing::warn!("Could not release session lock: {error}");
    }
}

/// Releases an acquisition path's lock if it returns an error before the owning
/// RAII type is built, so a failed attach cannot leave the lock held through a
/// duplicated descriptor.
struct OwnedLockGuard<'a> {
    file: &'a File,
    owner: u32,
    armed: bool,
}

impl Drop for OwnedLockGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            release_owned(self.file, self.owner);
        }
    }
}

/// Held by an interactive frontend for as long as a session is open, so other
/// sessions can tell an open but idle recipient from a closed one.
pub struct PresenceLease {
    file: File,
    owner: u32,
}
impl PresenceLease {
    pub fn acquire(root: &Path, session: &str) -> Result<Self> {
        valid_id(session)?;
        let directory = root.join("state/presence");
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("{session}.lock")))?;
        crate::fs::try_lock_briefly(&file)
            .context("Session is already open in another frontend")?;
        // Nothing fallible remains, so ownership can be handed over directly.
        Ok(Self {
            file,
            owner: std::process::id(),
        })
    }
}

impl Drop for PresenceLease {
    fn drop(&mut self) {
        // A close-only release would keep the session apparently open while any
        // duplicated or forked descriptor for the same description survives.
        release_owned(&self.file, self.owner);
    }
}

/// Whether a frontend currently holds the session open.
pub fn session_open(root: &Path, session: &str) -> Result<bool> {
    valid_id(session)?;
    let path = root.join("state/presence").join(format!("{session}.lock"));
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    match crate::fs::try_lock_file(&file) {
        Ok(()) => {
            #[cfg(test)]
            lock_observer::inspect(&file);
            // Release the probe's own lock before returning: leaving it to the
            // descriptor close would report a session with no owner as open for
            // as long as an inherited or duplicated descriptor survives.
            if let Err(error) = crate::fs::unlock_file(&file) {
                tracing::warn!("Could not release session presence probe lock: {error}");
            }
            Ok(false)
        }
        // A held lock belongs to the real owner; the probe must not unlock it.
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Observe execution ownership without creating a lease or trusting persisted state.
pub fn execution_active(root: &Path, session: &str) -> Result<bool> {
    if uuid::Uuid::parse_str(session).is_err() {
        return Ok(false);
    }
    let path = root
        .join("state/locks/session-execution")
        .join(format!("{session}.lock"));
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match crate::fs::try_lock_file(&file) {
        Ok(()) => {
            // Release the probe's own lock before returning. Leaving it to the
            // descriptor close would keep the session apparently owned for as
            // long as any descriptor forked during the probe survives.
            if let Err(error) = crate::fs::unlock_file(&file) {
                tracing::warn!("Could not release execution probe lock: {error}");
            }
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Cross-engine/process execution ownership, including Desktop private sessions.
pub struct ExecutionLease {
    file: File,
    pub(crate) address: Option<crate::db::session_store::HistoryAddress>,
    pub(crate) lifecycle: Vec<File>,
    /// The process that took the lease. A forked child inherits both the
    /// descriptor and this value, so its copy must not unlock an owner that is
    /// still alive in the parent.
    owner: u32,
}
impl ExecutionLease {
    pub(crate) fn acquire_inbox(root: &Path, incarnation: &str) -> Result<Self> {
        let path = lock_path(root, incarnation)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        crate::fs::try_lock_briefly(&file).context("Recipient already has a consumer")?;
        Ok(Self {
            file,
            address: None,
            lifecycle: Vec::new(),
            owner: std::process::id(),
        })
    }

    /// Exclusive admission for one session. Every caller - a frontend turn, an
    /// engine without a host and a Desktop private session - competes for the
    /// same lock, so a second top-level turn is rejected before it can start a
    /// turn record or a model request.
    pub fn acquire(root: &Path, session: &str) -> Result<Option<Self>> {
        Self::acquire_with(root, session, crate::fs::try_lock_briefly)
    }

    pub(crate) fn try_acquire(root: &Path, session: &str) -> Result<Option<Self>> {
        Self::acquire_with(root, session, crate::fs::try_lock_file)
    }

    fn acquire_with(
        root: &Path,
        session: &str,
        lock: fn(&File) -> std::result::Result<(), std::fs::TryLockError>,
    ) -> Result<Option<Self>> {
        if uuid::Uuid::parse_str(session).is_err() {
            return Ok(None);
        }
        // Lock ownership does not require opening/migrating a second database pool.
        // UUID validation bounds the filename; workspace/trust validation stays with the executor.
        let directory = root.join("state/locks/session-execution");
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("{session}.lock")))?;
        // A real owner holds the lock for a whole turn, so the short retry only absorbs momentary holds such as an execution_active probe or a forked descriptor; two overlapping turns still cannot both run.
        lock(&file).context("Session is already executing in another owner")?;
        #[cfg(test)]
        lock_observer::inspect(&file);
        Ok(Some(Self {
            file,
            address: None,
            lifecycle: Vec::new(),
            owner: std::process::id(),
        }))
    }
}

#[cfg(test)]
impl ExecutionLease {
    pub(crate) fn duplicate_descriptor(&self) -> std::io::Result<File> {
        self.file.try_clone()
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        // A child inherits the descriptor and the lock's shared open file
        // description; its copy must not release the parent's live ownership.
        if std::process::id() != self.owner {
            return;
        }
        if let Err(error) = crate::fs::unlock_file(&self.file) {
            // Descriptor close still follows; report without panicking in Drop.
            tracing::warn!("Could not release session execution lock: {error}");
        }
        for lock in &self.lifecycle {
            if let Err(error) = crate::fs::unlock_file(lock) {
                tracing::warn!("Could not release session lifecycle lock: {error}");
            }
        }

    }
}

/// The target expires as soon as the process that acquired its execution lease drops the consumer.
pub struct Consumer {
    pub target:Target,
    owner:u32,
    delivery:Arc<std::sync::Mutex<Option<crate::db::session_store::DeliveryOwner>>>,
}
#[derive(Clone)]
pub struct Target {
    generation:u32,
    live:std::sync::Weak<std::sync::Mutex<Option<crate::db::session_store::DeliveryOwner>>>,
}
impl Target {
    pub fn drain(&self,limit:usize)->Result<Vec<String>>{
        ensure!(self.generation==std::process::id(),"Message consumer ownership changed");
        let owner=self.live.upgrade().context("Message consumer ownership changed")?;
        let mut guard = owner.lock().unwrap_or_else(|e| e.into_inner());
        let delivery = guard.as_mut().context("Message consumer ownership changed")?;
        let inputs = delivery.poll_peer_context(limit)?;
        Ok(inputs.into_iter().map(|i|i.text).collect())
    }
}
impl Consumer {
    pub fn attach(root:&Path,id:&str)->Result<Option<Self>>{
        if uuid::Uuid::parse_str(id).is_err(){return Ok(None);}
        let ctx=context(root)?;
        let Some(row)=ctx.db.conversation_metadata(id)? else {return Ok(None);};
        if row.workspace_mode!="code" || row.project_id.is_none(){return Ok(None);}
        let session=ctx.db.open_session(id)?;
        session.validate_peer_endpoint()?;
        let delivery=std::sync::Arc::new(std::sync::Mutex::new(Some(session.delivery_owner()?)));
        Ok(Some(Self{owner:std::process::id(), target:Target{generation:std::process::id(),live:std::sync::Arc::downgrade(&delivery)},delivery}))
    }
}

impl Drop for Consumer {
    fn drop(&mut self){
        if self.owner==std::process::id(){self.delivery.lock().unwrap_or_else(|e|e.into_inner()).take();}
    }
}

/// Source-project matching includes managed worktrees. Runtime probing happens
/// separately: saved records and receiver ownership are not an HTTP health test.
pub fn peers(context: &ServiceContext, session: &str) -> Result<Vec<serde_json::Value>> {
    context.db.peers(session,false)
}

impl crate::db::DbManager {
    fn peers(&self,session:&str,family:bool)->Result<Vec<serde_json::Value>>{
        if family {return self.family_peers(session);}
        let root=self.data_dir();
        self.with_conn(|conn|->Result<_>{
        let source=project(conn,root,session)?;let rows=conversation_repo::list_conversations(conn)?;let mut result=Vec::new();
        for row in rows {if project(conn,root,&row.id).is_ok_and(|p|p==source) {result.push(serde_json::json!({"id":row.id,"title":row.title,"current":row.id==session,"receiver_active":receiver_active_by_key(root,&row.id)?,"saved":true}));}}
        Ok(result)
    })?
}

fn family_peers(&self,session:&str)->Result<Vec<serde_json::Value>>{
    let root=self.data_dir();
    self.with_conn(|conn| -> Result<_> {
        let source = project(conn, root, session)?;
        let mut links = Vec::new();
        let mut query = conn.prepare("SELECT key,value FROM peripheral_settings WHERE key LIKE 'delegation.%' OR key LIKE 'background-agent.%'")?;
        for row in query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
            let (key, value) = row?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&value) else { continue; };
            let child = value["session_id"].as_str().or_else(|| key.strip_prefix("delegation."));
            let parent = value["parent_session_id"].as_str().or_else(|| value["parent"].as_str());
            if let (Some(parent), Some(child)) = (parent, child) { links.push((parent.to_owned(), child.to_owned())); }
        }
        let mut members = std::collections::HashSet::from([session.to_owned()]);
        loop {
            let before = members.len();
            for (parent, child) in &links {
                if members.contains(parent) || members.contains(child) { members.insert(parent.clone()); members.insert(child.clone()); }
            }
            if members.len() == before { break; }
        }
        let mut result = Vec::new();
        for id in members {
            if project(conn, root, &id).is_ok_and(|other| other == source) {
                if let Some(row) = conversation_repo::get_conversation(conn, &id)? {
                    result.push(serde_json::json!({"id":id,"title":row.title,"current":id==session,"receiver_active":receiver_active_by_key(root,&id)?,"saved":true}));
                }
            }
        }
        result.sort_by_key(|row| (row["current"] != true, row["id"].as_str().unwrap_or_default().to_owned()));
        Ok(result)
    })?
}

}

pub fn session_peers(context:&ServiceContext, session:&str)->Result<Vec<serde_json::Value>>{
    context.db.peers(session,true)
}

/// Test-only observer for a lock the code has just acquired.
///
/// The descriptor is locked but not yet released at this point, which is exactly
/// where a `fork` in another thread would inherit it. A test installs a hook to
/// duplicate the real descriptor, so release on the success and error paths can
/// be checked without forking a threaded process.
#[cfg(test)]
mod lock_observer {
    use std::cell::RefCell;
    use std::fs::File;

    type Hook = Box<dyn FnMut(&File) + Send>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(super) fn install(hook: Hook) {
        HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    /// Called with the caller's locked descriptor before it is released.
    pub(super) fn inspect(file: &File) {
        HOOK.with(|slot| {
            if let Some(mut hook) = slot.borrow_mut().take() {
                hook(file);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn family_excludes_unrelated_saved_sessions_and_notices_include_body_once() {
        let (_temp, context, store, parent, other) = fixture();
        let root = context.paths.data_dir.join("data/workspaces");
        let cwd = store.workspace(&parent, &root).unwrap();
        let child = store.derive_owned_child(&parent, &root, &cwd).unwrap();
        let grandchild = store.derive_owned_child(&child, &root, &cwd).unwrap();
        for current in [&parent, &child, &grandchild] {
            let peers = session_peers(&context, current).unwrap();
            assert_eq!(peers.len(), 3);
            assert!(!peers.iter().any(|row| row["id"] == other));
            assert_eq!(peers.iter().filter(|row| row["current"] == true).count(), 1);
        }
        let message = send(&context, &child, &parent, "中文 body\nsecond line").unwrap();
        let notices = notices(&context, &parent).unwrap();
        assert!(notices[0].contains("中文 body\nsecond line"));
        assert!(notices[0].contains("Agent message from") && notices[0].contains("queued"));
        assert!(!notices[0].contains(&child));
        assert!(super::notices(&context, &parent).unwrap().is_empty());
        let inbox = inbox(&context, &parent, false).unwrap();
        assert_eq!(inbox[0].id, message.id);
        assert!(!inbox[0].read);
        assert_eq!(inbox[0].state, "queued");
    }

    #[test]
    fn presence_marks_an_open_session_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(!session_open(dir.path(), &id).unwrap());
        let lease = PresenceLease::acquire(dir.path(), &id).unwrap();
        assert!(session_open(dir.path(), &id).unwrap());
        drop(lease);
        assert!(!session_open(dir.path(), &id).unwrap());
    }

    /// `dup` gives a second descriptor for the same open file description, so
    /// the presence lock survives the owner's `File` being dropped. Only an
    /// explicit release lets a new frontend open the session at once, while the
    /// old duplicate is still alive.
    #[test]
    fn presence_release_is_immediate_even_while_a_duplicated_descriptor_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let lease = PresenceLease::acquire(dir.path(), &id).unwrap();
        assert!(session_open(dir.path(), &id).unwrap());
        let duplicate = lease.file.try_clone().unwrap();
        drop(lease);
        assert!(
            !session_open(dir.path(), &id).unwrap(),
            "the presence lock outlived its owner through the duplicated descriptor"
        );
        let next = PresenceLease::acquire(dir.path(), &id)
            .expect("a new frontend may open the session while the old duplicate is alive");
        drop(next);
        drop(duplicate);
        assert!(!session_open(dir.path(), &id).unwrap());
    }

    /// A live presence owner stays exclusive: a second acquire must still fail.
    #[test]
    fn a_live_presence_owner_stays_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let lease = PresenceLease::acquire(dir.path(), &id).unwrap();
        assert!(PresenceLease::acquire(dir.path(), &id).is_err());
        drop(lease);
        drop(PresenceLease::acquire(dir.path(), &id).expect("released owner is free"));
        assert!(!session_open(dir.path(), &id).unwrap());
    }

    /// The same duplicated-descriptor exposure applies to a message consumer.
    /// Release must be explicit and ordered so a newer consumer's generation row
    /// survives the older generation's cleanup.
    #[test]
    fn consumer_release_is_immediate_and_preserves_a_newer_generation() {
        let (_temp, context, store, _sender, recipient) = fixture();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        assert!(receiver_active(&context.paths.data_dir, &recipient).unwrap());
        let duplicate = owner.delivery.lock().unwrap().as_ref().unwrap().duplicate_inbox_lock().unwrap();
        let stale = owner.target.clone();
        drop(owner);
        assert!(
            !receiver_active(&context.paths.data_dir, &recipient).unwrap(),
            "the consumer lock outlived its owner through the duplicated descriptor"
        );
        let resumed = Consumer::attach(&context.paths.data_dir, &recipient)
            .expect("a new consumer is admitted while the old duplicate is alive")
            .expect("a real session id");
        // The old generation is no longer the consumer, even though a stale
        // `Target` handle still exists.
        assert!(stale.drain(8).is_err());
        assert!(receiver_active(&context.paths.data_dir, &recipient).unwrap());
        assert!(resumed.target.drain(8).unwrap().is_empty());
        drop(duplicate);
        drop(resumed);
        let _ = store;
        assert!(!receiver_active(&context.paths.data_dir, &recipient).unwrap());
    }

    #[test]
    fn a_copied_consumer_does_not_clean_up_the_live_owner() {
        let (_temp, ctx, _, _, recipient) = fixture();
        let owner = Consumer::attach(&ctx.paths.data_dir, &recipient).unwrap().unwrap();
        // A fork copies the parent’s target and lifetime. Simulate dropping that
        // foreign copy while the acquiring parent still owns the lease.
        let copied = Consumer {
            target: owner.target.clone(), delivery: owner.delivery.clone(),
            owner: std::process::id().wrapping_add(1),
        };
        drop(copied);
        assert!(owner.target.drain(8).unwrap().is_empty());
        assert!(Consumer::attach(&ctx.paths.data_dir,&recipient).is_err());
        let stale = owner.target.clone();
        drop(owner);
        let next = Consumer::attach(&ctx.paths.data_dir, &recipient).unwrap().unwrap();
        assert!(next.target.drain(8).unwrap().is_empty());
        assert!(stale.drain(8).is_err());
    }

    #[test]
    fn a_stale_target_cannot_commit_after_release_while_the_directory_writer_is_busy() {
        let (_temp, ctx, store, sender, recipient) = fixture();
        send(&ctx,&sender,&recipient,"stays queued").unwrap();
        let owner = Consumer::attach(&ctx.paths.data_dir,&recipient).unwrap().unwrap();
        let stale = owner.target.clone();
        let (release, holder) = hold_writer(&ctx);
        drop(owner);
        assert!(stale.drain(8).is_err());
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(!receiver_active(&ctx.paths.data_dir,&recipient).unwrap());
        assert_eq!(inbox(&ctx,&recipient,false).unwrap()[0].state,"queued");
        assert!(store.history(&recipient).unwrap().is_empty());
    }

    /// Release waits for a drain that already started, and once it returns no
    /// stale handle may commit.
    #[test]
    fn release_waits_for_an_in_flight_drain_and_then_rejects_it() {
        let (_temp, context, _store, _sender, recipient) = fixture();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let stale = owner.target.clone();
        // Hold the same lock a running drain holds. The bounded wait below only
        // checks that release did not overtake it; the correctness assertion is
        // the rejected drain after the release completed.
        let live = stale.live.upgrade().unwrap();
        let in_flight = live
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let dropper = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            drop(owner);
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "release must wait for an in-flight drain"
        );
        drop(in_flight);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("release completes once the drain let go");
        dropper.join().unwrap();
        assert!(
            stale.drain(8).is_err(),
            "a stale target committed after the release completed"
        );
    }

    /// A failed attach after a successful lock acquisition must release the lock
    /// explicitly. The observer duplicates the real descriptor at the moment a
    /// `fork` could have, and the injected trigger makes the DB step fail.
    #[test]
    fn a_failed_consumer_attach_releases_the_lock_before_descriptor_close() {
        let (_temp, context, _store, _sender, recipient) = fixture();
        let shard = context.db.open_session(&recipient).unwrap();
        let competing_inbox = ExecutionLease::acquire_inbox(&context.paths.data_dir, shard.incarnation()).unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        lock_observer::install(Box::new(move |file| {
            send.send(file.try_clone().unwrap()).unwrap();
        }));
        assert!(Consumer::attach(&context.paths.data_dir,&recipient).is_err());
        let duplicate = receive.try_recv().expect("attach acquired the execution lock");
        assert!(!execution_active(&context.paths.data_dir,shard.incarnation()).unwrap());
        drop(competing_inbox);
        // A real consumer is admitted at once, while the duplicate is alive.
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .expect("the failed attach released the lock")
            .expect("a real session id");
        drop(owner);
        drop(duplicate);
        assert!(!receiver_active(&context.paths.data_dir, &recipient).unwrap());
    }

    /// A probe that successfully takes the lock must release it before returning
    /// false: otherwise an inherited/duplicated descriptor keeps the session
    /// looking open with no owner.
    #[test]
    fn a_completed_presence_probe_leaves_no_lock_for_a_duplicated_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        // The probe only opens an existing file; it never creates one.
        drop(PresenceLease::acquire(dir.path(), &id).unwrap());
        let (send, receive) = std::sync::mpsc::channel();
        lock_observer::install(Box::new(move |file| {
            send.send(file.try_clone().unwrap()).unwrap();
        }));
        assert!(!session_open(dir.path(), &id).unwrap());
        let duplicate = receive.try_recv().expect("the probe never took the lock");
        assert!(
            !session_open(dir.path(), &id).unwrap(),
            "the probe's lock outlived it through a duplicated descriptor"
        );
        let owner = PresenceLease::acquire(dir.path(), &id)
            .expect("a real frontend may open the session after the probe released");
        drop(owner);
        drop(duplicate);
    }

    /// The message-receiver probe has the same obligation.
    #[test]
    fn a_completed_receiver_probe_leaves_no_lock_for_a_duplicated_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = lock_path(dir.path(), &id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        lock_observer::install(Box::new(move |file| {
            send.send(file.try_clone().unwrap()).unwrap();
        }));
        assert!(!receiver_active_by_key(dir.path(), &id).unwrap());
        let duplicate = receive.try_recv().expect("the probe never took the lock");
        assert!(
            !receiver_active_by_key(dir.path(), &id).unwrap(),
            "the probe's lock outlived it through a duplicated descriptor"
        );
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        crate::fs::try_lock_file(&owner)
            .expect("a real consumer may attach after the probe released");
        crate::fs::unlock_file(&owner).unwrap();
        drop(owner);
        drop(duplicate);
    }

    #[test]
    fn admission_is_exclusive_in_one_process_and_available_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let first = super::ExecutionLease::acquire(dir.path(), &id)
            .unwrap()
            .unwrap();
        assert!(super::execution_active(dir.path(), &id).unwrap());
        // A second independent top-level turn cannot share the admitted one,
        // even inside the same process, and the refusal is immediate.
        assert!(super::ExecutionLease::acquire(dir.path(), &id).is_err());
        // Probing must not disturb a genuine owner: the probe takes the lock,
        // reports the session active and releases only its own probe lock.
        assert!(super::execution_active(dir.path(), &id).unwrap());
        assert!(super::ExecutionLease::acquire(dir.path(), &id).is_err());
        drop(first);
        // Release is explicit, so the freed session is observable at once.
        assert!(!super::execution_active(dir.path(), &id).unwrap());
        // Sequential turns still succeed once the owner released the session.
        let next = super::ExecutionLease::acquire(dir.path(), &id)
            .unwrap()
            .unwrap();
        drop(next);
        assert!(!super::execution_active(dir.path(), &id).unwrap());
    }

    /// The activity probe takes the lock only for an instant. An admission
    /// that starts during that instant must wait for the probe instead of
    /// being refused as if another turn were executing.
    #[test]
    fn probe_does_not_refuse_a_concurrent_admission() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir
            .path()
            .join("state/locks/session-execution")
            .join(format!("{id}.lock"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Open and lock exactly as `execution_active` does during its probe.
        let probe = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        crate::fs::try_lock_file(&probe).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            crate::fs::unlock_file(&probe).unwrap();
        });
        let lease = super::ExecutionLease::acquire(dir.path(), &id)
            .expect("a momentary probe must not refuse a real admission")
            .expect("a real session id");
        release.join().unwrap();
        assert!(super::execution_active(dir.path(), &id).unwrap());
        drop(lease);
        assert!(!super::execution_active(dir.path(), &id).unwrap());
    }

    /// Retrying absorbs only momentary holders: a real turn still refuses a
    /// second overlapping admission, and does so without a long stall.
    #[test]
    fn held_lease_is_still_refused_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let first = super::ExecutionLease::acquire(dir.path(), &id)
            .unwrap()
            .unwrap();
        let started = std::time::Instant::now();
        assert!(super::ExecutionLease::acquire(dir.path(), &id).is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a held session must be refused quickly, took {:?}",
            started.elapsed()
        );
        drop(first);
    }

    /// A forked child shares the owner's open file description, so a
    /// close-only release would keep the session locked until that child
    /// exited. The disposable child is a separate process; no unsafe `fork`
    /// runs in this threaded test process.
    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn a_lease_releases_immediately_even_while_a_child_holds_the_inherited_descriptor() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let lease = super::ExecutionLease::acquire(dir.path(), &id)
            .unwrap()
            .unwrap();
        // Clear close-on-exec so the child keeps the locked descriptor past exec,
        // as a fork between acquire and release would.
        rustix::io::fcntl_setfd(&lease.file, rustix::io::FdFlags::empty()).unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("echo ready; exec sleep 30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready", "the child inherited the descriptor");
        assert!(super::execution_active(dir.path(), &id).unwrap());

        drop(lease);
        // Immediate, with the child still alive: this is the behaviour that a
        // close-only release cannot provide.
        assert!(
            !super::execution_active(dir.path(), &id).unwrap(),
            "the session stayed locked by the child's inherited descriptor"
        );
        let next = super::ExecutionLease::acquire(dir.path(), &id)
            .expect("the released session is free while the child is still alive")
            .expect("a real session id");
        drop(next);

        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The activity probe takes the lock to test it; that temporary ownership
    /// must not survive the probe through an inherited descriptor either.
    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn a_completed_activity_probe_leaves_no_lock_for_an_inherited_descriptor() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir
            .path()
            .join("state/locks/session-execution")
            .join(format!("{id}.lock"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Emulate the probe's own acquisition, then inherit it into a child.
        let probe = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        crate::fs::try_lock_file(&probe).unwrap();
        rustix::io::fcntl_setfd(&probe, rustix::io::FdFlags::empty()).unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("echo ready; exec sleep 30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        // This is what the probe does before returning.
        crate::fs::unlock_file(&probe).unwrap();
        drop(probe);

        assert!(
            !super::execution_active(dir.path(), &id).unwrap(),
            "the probe's lock outlived it through the child"
        );

        child.kill().unwrap();
        child.wait().unwrap();
    }

    use super::*;
    use crate::runtime::SessionStore;
    fn fixture() -> (
        tempfile::TempDir,
        ServiceContext,
        SessionStore,
        String,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let context = context(&temp.path().join("data")).unwrap();
        let store = SessionStore::new(context.db.clone());
        let sender = store.create(&workspace, "fixture", "fixture").unwrap();
        let recipient = store.fork(&sender).unwrap();
        (temp, context, store, sender, recipient)
    }
    #[test]
    fn operation_scoped_delivery_is_exactly_once() {
        let (_temp, context, _store, sender, recipient) = fixture();
        let first = send_once(&context, "crp-send:tool-1", &sender, &recipient, "hello").unwrap();
        let replay = send_once(&context, "crp-send:tool-1", &sender, &recipient, "hello").unwrap();
        assert_eq!(first.id, replay.id);
        assert_eq!(inbox(&context, &recipient, false).unwrap().len(), 1);
        assert!(send_once(
            &context,
            "crp-send:tool-1",
            &sender,
            &recipient,
            "different"
        )
        .is_err());
        let other = send_once(&context, "crp-send:tool-2", &sender, &recipient, "hello").unwrap();
        assert_ne!(first.id, other.id);
        assert_eq!(inbox(&context, &recipient, false).unwrap().len(), 2);
    }

    #[test]
    fn transaction_ack_is_atomic_and_replays_once_under_consumer_ownership() {
        let (_temp, context, store, sender, recipient) = fixture();
        let message = send(&context, &sender, &recipient, "check cancellation").unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        assert!(receiver_active(&context.paths.data_dir, &recipient).unwrap());
        assert!(Consumer::attach(&context.paths.data_dir, &recipient).is_err());
        context.db.fixture_shard(&recipient, |conn|conn.execute_batch("CREATE TRIGGER reject_inbox_context BEFORE INSERT ON messages WHEN NEW.role='user' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;")).unwrap().unwrap();
        assert!(owner.target.drain(8).is_err());
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "claimed"
        );
        assert!(store.history(&recipient).unwrap().is_empty());
        context
            .db
            .fixture_shard(&recipient, |conn| conn.execute_batch("DROP TRIGGER reject_inbox_context"))
            .unwrap()
            .unwrap();
        mark_read(&context, &recipient, &message.id).unwrap();
        let context_rows = owner.target.drain(8).unwrap();
        assert_eq!(context_rows.len(), 1);
        assert!(context_rows[0].contains("check cancellation"));
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "context_committed"
        );
        assert!(owner.target.drain(8).unwrap().is_empty());
        let stale = owner.target.clone();
        drop(owner);
        assert!(!receiver_active(&context.paths.data_dir, &recipient).unwrap());
        assert!(stale.drain(8).is_err());
        let resumed = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        assert!(resumed.target.drain(8).unwrap().is_empty());
        assert_eq!(store.history(&recipient).unwrap().len(), 1);
    }

    /// Holds the writer on a second connection, as the host does while it
    /// persists a long transcript at the same tool boundary the engine polls.
    fn hold_writer(
        context: &ServiceContext,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let path = context.paths.data_dir.join("global.db");
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
            conn.execute_batch("ROLLBACK").unwrap();
        });
        locked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        (release_tx, holder)
    }

    /// An empty boundary poll is read-only: the host's long write must not turn
    /// it into a failed turn after busy_timeout.
    #[test]
    fn an_empty_drain_does_not_take_the_writer_lock() {
        let (_temp, context, _store, _sender, recipient) = fixture();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let (release, holder) = hold_writer(&context);
        let started = std::time::Instant::now();
        let drained = owner.target.drain(8).unwrap();
        assert!(drained.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "an empty drain waited for the writer for {:?}",
            started.elapsed()
        );
        release.send(()).unwrap();
        holder.join().unwrap();
        drop(owner);
    }

    /// A pending message still needs the writer, but the poll waits only a
    /// bounded time. Contention rolls the whole unit back, so no acknowledgement
    /// or history row can survive it, and the same message is delivered exactly
    /// once after the host releases the writer.
    #[test]
    fn a_pending_drain_defers_under_a_held_writer_and_delivers_once_after_release() {
        let (_temp, context, store, sender, recipient) = fixture();
        send(&context, &sender, &recipient, "queued while host writes").unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let (release, holder) = hold_writer(&context);
        let started = std::time::Instant::now();
        let drained = owner.target.drain(8).unwrap();
        let elapsed = started.elapsed();
        assert!(drained.is_empty());
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "a contended poll waited {elapsed:?}"
        );
        let (state, consumer_generation, committed_at): (String, Option<i64>, Option<String>) =
            context
                .db
                .fixture_directory(|conn| {
                    Ok::<_, anyhow::Error>(conn.query_row(
                        "SELECT state,generation,committed_at FROM session_inbox WHERE recipient=?1",
                        [&recipient],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?)
                })
                .unwrap()
                .unwrap();
        assert_eq!(state, "queued");
        assert!(consumer_generation.is_none() && committed_at.is_none());
        assert!(store.history(&recipient).unwrap().is_empty());
        release.send(()).unwrap();
        holder.join().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let delivered = loop {
            let delivered = owner.target.drain(8).unwrap();
            if !delivered.is_empty() || std::time::Instant::now() >= deadline {
                break delivered;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].contains("queued while host writes"));
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "context_committed"
        );
        let history = store.history(&recipient).unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0]["content"]
            .as_str()
            .unwrap()
            .contains("queued while host writes"));
        assert!(owner.target.drain(8).unwrap().is_empty());
        drop(owner);
    }

    /// A non-contention failure still reaches the caller, now naming the
    /// operation and the extended SQLite code. The message stays queued and a
    /// later poll delivers it once.
    #[test]
    fn a_nonbusy_drain_failure_names_the_operation_and_keeps_the_queue() {
        let (_temp, context, store, sender, recipient) = fixture();
        send(&context, &sender, &recipient, "private queued body").unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        context.db.fixture_shard(&recipient, |conn|conn.execute_batch("CREATE TRIGGER reject_inbox_context BEFORE INSERT ON messages WHEN NEW.role='user' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;")).unwrap().unwrap();
        let error = owner.target.drain(8).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("deliver input failed after"),
            "{text}"
        );
        // SQLITE_CONSTRAINT_TRIGGER from the fixture's RAISE(ABORT).
        assert!(text.contains("sqlite extended code 1811"), "{text}");
        assert_eq!(crate::db::sqlite_extended_code(&error), Some(1811));
        // The host-facing diagnostic keeps the static operation and code but
        // never the SQL text or the queued body.
        let diagnostic = crate::performance::request_diagnostic(&error);
        assert!(
            diagnostic.contains("deliver input failed after"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("sqlite extended code 1811"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("private queued body"), "{diagnostic}");
        assert!(!diagnostic.contains("INSERT INTO"), "{diagnostic}");
        assert!(store.history(&recipient).unwrap().is_empty());
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "claimed"
        );
        context
            .db
            .fixture_shard(&recipient, |conn| conn.execute_batch("DROP TRIGGER reject_inbox_context"))
            .unwrap()
            .unwrap();
        assert_eq!(owner.target.drain(8).unwrap().len(), 1);
        drop(owner);
    }

    /// Recovery acknowledges the existing receipt without replaying the local effect.
    #[test]
    fn a_late_acknowledgement_failure_recovers_one_durable_receipt() {
        let (_temp, context, store, sender, recipient) = fixture();
        send(&context, &sender, &recipient, "rolled back body").unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let count = |context: &ServiceContext| -> i64 {
            context
                .db
                .fixture_directory(|conn| {
                    Ok::<_, anyhow::Error>(conn.query_row(
                        "SELECT message_count FROM conversations WHERE id=?1",
                        [&recipient],
                        |row| row.get(0),
                    )?)
                })
                .unwrap()
                .unwrap()
        };
        let before_count = count(&context);
        // The RAISE fires on the acknowledgement UPDATE, i.e. after
        // insert_message and increment_message_count already ran.
        context
            .db
            .fixture_directory(|conn| {
                conn.execute_batch(
                    "CREATE TRIGGER reject_inbox_ack BEFORE UPDATE OF state ON session_inbox \
                     WHEN NEW.state='context_committed' \
                     BEGIN SELECT RAISE(ABORT,'fixture ack failure'); END;",
                )
            })
            .unwrap()
            .unwrap();
        let error = owner.target.drain(8).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("acknowledge delivery failed after"),
            "{text}"
        );
        assert!(text.contains("sqlite extended code 1811"), "{text}");
        assert!(
            !format!("{error:#}").contains("rolled back body"),
            "the diagnostic exposed queued content: {text}"
        );
        assert_eq!(store.history(&recipient).unwrap().len(),1);
        let (state, generation, committed_at): (String, Option<i64>, Option<String>) = context
            .db
            .fixture_directory(|conn| {
                Ok::<_, anyhow::Error>(conn.query_row(
                    "SELECT state,generation,committed_at FROM session_inbox WHERE recipient=?1",
                    [&recipient],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap()
            .unwrap();
        assert_eq!(state, "claimed");
        assert!(generation.is_some() && committed_at.is_none());
        context
            .db
            .fixture_directory(|conn| conn.execute_batch("DROP TRIGGER reject_inbox_ack"))
            .unwrap()
            .unwrap();
        let delivered = owner.target.drain(8).unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].contains("rolled back body"));
        assert_eq!(store.history(&recipient).unwrap().len(), 1);
        assert_eq!(count(&context), before_count + 1);
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "context_committed"
        );
        assert!(owner.target.drain(8).unwrap().is_empty());
        drop(owner);
    }

    /// An empty inbox is still subject to the same authorization checks the
    /// write path enforced: the consumer generation alone is not enough.
    #[test]
    fn an_empty_inbox_still_enforces_project_authorization() {
        let (_temp, context, _store, _sender, recipient) = fixture();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let current = owner.target.clone();
        let project_id: String = context
            .db
            .fixture_directory(|conn| {
                Ok::<_, anyhow::Error>(conn.query_row(
                    "SELECT project_id FROM conversations WHERE id=?1",
                    [&recipient],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
            .unwrap();
        let update = |sql: &str, value: &str, id: &str| {
            context
                .db
                .fixture_directory(|conn| {
                    Ok::<_, anyhow::Error>(conn.execute(sql, rusqlite::params![value, id])?)
                })
                .unwrap()
                .unwrap();
        };

        update(
            "UPDATE conversations SET archived=?1 WHERE id=?2",
            "1",
            &recipient,
        );
        let error = owner.target.drain(8).unwrap_err();
        assert!(format!("{error:#}").contains("archived"), "{error:#}");
        update(
            "UPDATE conversations SET archived=?1 WHERE id=?2",
            "0",
            &recipient,
        );

        update(
            "UPDATE conversations SET workspace_mode=?1 WHERE id=?2",
            "chat",
            &recipient,
        );
        let error = owner.target.drain(8).unwrap_err();
        assert!(format!("{error:#}").contains("Code session"), "{error:#}");
        update(
            "UPDATE conversations SET workspace_mode=?1 WHERE id=?2",
            "code",
            &recipient,
        );

        update(
            "UPDATE project_folders SET trusted=?1 WHERE project_id=?2",
            "0",
            &project_id,
        );
        let error = owner.target.drain(8).unwrap_err();
        assert!(
            format!("{error:#}").contains("Trust all project folders"),
            "{error:#}"
        );
        update(
            "UPDATE project_folders SET trusted=?1 WHERE project_id=?2",
            "1",
            &project_id,
        );

        // None of these refusals changed the consumer or consumed context.
        assert!(current.drain(8).unwrap().is_empty());
        assert!(owner.target.drain(8).unwrap().is_empty());
        drop(owner);
    }

    /// Revoking the consumer still refuses the poll and never drops the queue.
    #[test]
    fn a_revoked_consumer_is_refused_and_the_queue_survives() {
        let (_temp, context, _store, sender, recipient) = fixture();
        send(&context, &sender, &recipient, "survives revocation").unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let stale = owner.target.clone();
        drop(owner);
        let error = stale.drain(8).unwrap_err();
        assert!(
            format!("{error:#}").contains("Message consumer ownership changed"),
            "{error:#}"
        );
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "queued"
        );
        let resumed = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        let delivered = resumed.target.drain(8).unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].contains("survives revocation"));
        assert!(resumed.target.drain(8).unwrap().is_empty());
        drop(resumed);
    }

    /// The bounded poll must put the borrowed connection's own timeout back,
    /// including when the poll unwinds.
    #[test]
    fn the_poll_restores_the_borrowed_connection_timeout_on_success_and_unwind() {
        fn timeout_ms(conn: &Connection) -> u64 {
            conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .unwrap()
        }
        let (_temp, context, _store, _sender, _recipient) = fixture();
        let path = context.paths.data_dir.join("global.db");
        let conn = Connection::open(&path).unwrap();
        conn.busy_timeout(std::time::Duration::from_millis(7_000))
            .unwrap();
        {
            let _poll = BusyTimeoutGuard::within(&conn, crate::db::session_store::TURN_POLL_WAIT).unwrap();
            assert_eq!(timeout_ms(&conn), 250);
        }
        assert_eq!(timeout_ms(&conn), 7_000);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _poll = BusyTimeoutGuard::within(&conn, crate::db::session_store::TURN_POLL_WAIT).unwrap();
            panic!("unwind while the poll timeout is lowered");
        }));
        assert!(unwound.is_err());
        assert_eq!(timeout_ms(&conn), 7_000);
    }
    #[test]
    fn trusted_identity_limits_and_recipient_ack_cannot_be_spoofed() {
        let (temp, context, store, sender, recipient) = fixture();
        let other = temp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let outside = store.create(&other, "fixture", "fixture").unwrap();
        assert!(send(&context, &sender, &outside, "wrong project").is_err());
        assert!(send(&context, "spoof", &recipient, "wrong sender").is_err());
        assert!(send(&context, &sender, "../escape", "wrong path").is_err());
        assert!(send(&context, &sender, &recipient, &"x".repeat(65537)).is_err());
        let result=crate::tools::agent_messages::execute_live(&context,&sender,serde_json::json!({"operation":"send","from":outside,"to":recipient,"content":"from actual session"})).unwrap();
        assert_eq!(result["message"]["sender"], sender);
        assert_eq!(result["delivery"], "next_resume");
        assert!(mark_read(&context, &sender, result["message"]["id"].as_str().unwrap()).is_err());
        assert_eq!(notices(&context, &recipient).unwrap().len(), 1);
        assert!(notices(&context, &recipient).unwrap().is_empty());
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].state,
            "queued"
        );
        for _ in 0..15 {
            send(&context, &sender, &recipient, &"x".repeat(65536)).unwrap();
        }
        assert!(send(&context, &sender, &recipient, &"x".repeat(65536)).is_err());
    }
    #[test]
    fn pending_listing_survives_large_history_and_chat_sessions_are_skipped() {
        let (_temp, context, _store, sender, recipient) = fixture();
        let old = send(&context, &sender, &recipient, "old").unwrap();
        context.db.fixture_directory(|conn| -> Result<()> {
            conn.execute("UPDATE session_inbox SET state='context_committed' WHERE id=?1", [&old.id])?;
            for index in 0..1001 {
                conn.execute("INSERT INTO session_inbox(id,sender,sender_incarnation,recipient,project_source,payload,size,created_at,state) SELECT ?1,sender,sender_incarnation,recipient,project_source,payload,size,created_at,state FROM session_inbox WHERE id=?2", params![format!("fixture-{index}"),old.id])?;
            }
            Ok(())
        }).unwrap().unwrap();
        let pending = send(&context, &sender, &recipient, "new").unwrap();
        assert_eq!(pending_count(&context, &recipient).unwrap(), 1);
        assert_eq!(
            inbox(&context, &recipient, false).unwrap()[0].id,
            pending.id
        );
        context
            .db
            .fixture_directory(|conn| {
                conn.execute(
                    "UPDATE conversations SET workspace_mode='chat' WHERE id=?1",
                    [&recipient],
                )
            })
            .unwrap()
            .unwrap();
        assert!(Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .is_none());
    }
    #[test]
    fn revoked_sender_trust_blocks_without_dropping_pending_messages() {
        let (_temp, context, store, sender, recipient) = fixture();
        send(&context, &sender, &recipient, "pending").unwrap();
        // Rebind sender to an independent project before revoking its trust.
        let original = super::super::session_trust::inspect(&context.db, &sender).unwrap();
        super::super::session_trust::repair(
            &context.db,
            &context.paths.data_dir,
            &sender,
            &[PathBuf::from(original.workspace_path.unwrap())],
        )
        .unwrap();
        let sender_project = super::super::session_trust::inspect(&context.db, &sender).unwrap();
        context
            .db
            .fixture_directory(|conn| {
                conn.execute(
                    "UPDATE project_folders SET trusted=0 WHERE project_id=?1",
                    [sender_project.id],
                )
            })
            .unwrap()
            .unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        assert!(owner.target.drain(8).unwrap().is_empty());
        let queued = inbox(&context, &recipient, false).unwrap();
        assert_eq!(queued[0].state, "blocked");
        assert!(queued[0].blocked_reason.is_some());
        assert!(store.history(&recipient).unwrap().is_empty());
    }
    #[test]
    fn managed_worktree_peers_share_source_identity_and_inbox() {
        let (_temp, context, store, sender, recipient) = fixture();
        let source = store
            .workspace(&sender, &context.paths.data_dir.join("data/workspaces"))
            .unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Fixture"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["commit", "--allow-empty", "-qm", "initial"],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&source)
                .output()
                .unwrap()
                .status
                .success());
        }
        let worktree = crate::worktree::session::prepare(&source, None, true).unwrap();
        let project = super::super::session_trust::inspect(&context.db, &recipient).unwrap();
        context
            .db
            .fixture_directory(|conn| {
                worktree.save(conn, &recipient, &project.id, source.to_str().unwrap())
            })
            .unwrap()
            .unwrap();
        assert_ne!(
            store
                .workspace(&recipient, &context.paths.data_dir.join("data/workspaces"))
                .unwrap(),
            source
        );
        assert_eq!(peers(&context, &sender).unwrap().len(), 2);
        send(&context, &sender, &recipient, "worktree message").unwrap();
        let owner = Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap();
        assert_eq!(owner.target.drain(8).unwrap().len(), 1);
    }
}
