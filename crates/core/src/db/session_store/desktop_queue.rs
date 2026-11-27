//! The queue retains a compact reference to the last admitted Desktop context.
use super::{layout, Stores};
use crate::{db::DbManager, native_engine::engine_core::ChatRequest};
use anyhow::{ensure, Context, Result};

impl Stores {
    pub fn remember_desktop_context(&self, req: &ChatRequest) -> Result<()> {
        let mut context = req.clone();
        context.messages.clear();
        let value = serde_json::to_value(context)?;
        let key = layout::storage_key(&req.conversation_id)?;
        let _locks = self
            .resolved_key(&key)?
            .context("Queue session unavailable")?;
        let digest = self.put_payload(&value)?;
        self.global().write(
            "remember queued turn context",
            &req.conversation_id,
            false,
            |c| {
                c.execute(
                    "INSERT INTO desktop_queue_context(session_id,request_json) VALUES(?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET request_json=excluded.request_json
             WHERE request_json<>excluded.request_json",
                    [&req.conversation_id, &digest],
                )?;
                Ok(())
            },
        )
    }
    pub fn desktop_context(&self, id: &str) -> Result<ChatRequest> {
        let digest: String = self.global().read(|c| {
            Ok(c.query_row(
                "SELECT request_json FROM desktop_queue_context WHERE session_id=?1",
                [id],
                |r| r.get(0),
            )?)
        })?;
        let request: ChatRequest = serde_json::from_value(self.payload(&digest)?)?;
        ensure!(
            request.conversation_id == id,
            "Queue context belongs to another session"
        );
        Ok(request)
    }
}
impl DbManager {
    pub fn remember_desktop_context(&self, req: &ChatRequest) -> Result<()> {
        let mut context = req.clone();
        context.messages.clear();
        let raw = serde_json::to_string(&context)?;
        self.with_conn(|c| -> Result<()> {
            crate::db::write_tx(c,"remember queued turn context",super::WRITE_DEADLINE,|tx|{
                tx.execute_batch("CREATE TABLE IF NOT EXISTS desktop_queue_context(session_id TEXT PRIMARY KEY,request_json TEXT NOT NULL)")?;
                tx.execute("INSERT INTO desktop_queue_context VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET request_json=excluded.request_json",
                    [&req.conversation_id,&raw])?;
                Ok(())
            })
        })?
    }
    pub fn desktop_context(&self, id: &str) -> Result<ChatRequest> {
        let raw: String = self.with_conn(|c| {
            c.query_row(
                "SELECT request_json FROM desktop_queue_context WHERE session_id=?1",
                [id],
                |r| r.get(0),
            )
        })??;
        Ok(serde_json::from_str(&raw)?)
    }
}

use crate::db::turn_repo::{self, TurnRecord, TurnStatus, TurnTimer};
use crate::services::{
    input_queue::{self as queue, QueueItem, QueueState},
    session_messages::ExecutionLease,
};
use std::sync::Arc;

impl super::SessionRead<'_> {
    pub(crate) fn dispatched_turn(&self, id: &str) -> Result<TurnRecord> {
        let record = turn_repo::get_turn(self.0, id)?.context("Queued turn missing")?;
        ensure!(
            record.status == TurnStatus::Running,
            "Queued turn is already terminal"
        );
        Ok(record)
    }
}

impl Stores {
    pub fn chat_execution_lease(&self, id: &str, model: &str) -> Result<ExecutionLease> {
        Ok(self
            .admit_chat_execution(id, model, None, "continuation")?
            .0)
    }

    /// Returns whether creation already persisted the first user message.
    pub fn admit_chat_execution(
        &self,
        id: &str,
        model: &str,
        first: Option<&serde_json::Value>,
        origin: &str,
    ) -> Result<(ExecutionLease, bool)> {
        let row = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?;
        let Some(row) = row else {
            let options = super::conversations::first_chat_options(model, first, origin)?;
            return self.create_chat_with_lease(id, options);
        };
        let session = self.open_listed(&row)?;
        let lease = session.acquire_execution()?;
        session.settle_abandoned(&lease)?;
        let options = super::conversations::first_chat_options(model, first, origin)?;
        let sealed_first = session.consume_sealed_first(options.first_message.as_ref())?;
        Ok((lease, sealed_first))
    }
    pub fn existing_chat_lease(&self, id: &str) -> Result<ExecutionLease> {
        self.open_session(id)?.acquire_execution()
    }
    pub fn next_desktop_input(&self, id: &str) -> Result<Option<QueueItem>> {
        self.open_session(id)?.desktop_dispatch_candidate()
    }
    pub fn start_desktop_input(
        &self,
        id: &str,
        lease: Arc<ExecutionLease>,
        item: &QueueItem,
    ) -> Result<TurnTimer> {
        let session = self.open_session(id)?;
        session.delivery_under_lease(lease)?.start_queued_turn(item)
    }
}

impl DbManager {
    pub fn chat_execution_lease(&self, id: &str, _model: &str) -> Result<ExecutionLease> {
        self.existing_chat_lease(id)
    }
    pub fn existing_chat_lease(&self, id: &str) -> Result<ExecutionLease> {
        ExecutionLease::acquire(self.data_dir(), id)?
            .context("Session is executing in another process")
    }
    pub fn next_desktop_input(&self, id: &str) -> Result<Option<QueueItem>> {
        Ok(queue::list(self, id)?
            .into_iter()
            .find(|item| matches!(item.state, QueueState::Queued | QueueState::Claimed)))
    }
    pub fn start_desktop_input(
        self: &Arc<Self>,
        id: &str,
        _lease: Arc<ExecutionLease>,
        item: &QueueItem,
    ) -> Result<TurnTimer> {
        let claimed = if item.state == QueueState::Queued {
            queue::claim(self, id, item.id, item.revision)?
        } else {
            item.clone()
        };
        queue::handoff(
            self,
            id,
            claimed.id,
            claimed.revision,
            |tx, item, message| {
                let paused:bool=tx.query_row("SELECT coalesce((SELECT paused FROM session_input_queue_control WHERE session_id=?1),0)",[id],|r|r.get(0))?;
                ensure!(!paused, queue::QueueError::Paused);
                let order = tx.query_row(
                    "SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                crate::db::message_repo::save_user_message(
                    tx,
                    message,
                    id,
                    &item.content,
                    &chrono::Utc::now().to_rfc3339(),
                    order,
                )?;
                super::state::set_local(
                    tx,
                    &format!("message-origin:{message}"),
                    &serde_json::json!({"promptOrigin":item.prompt_origin}).to_string(),
                )?;
                tx.execute("UPDATE conversations SET message_count=(SELECT COUNT(*) FROM messages WHERE conversation_id=?1) WHERE id=?1",[id])?;
                Ok(())
            },
        )?;
        self.start_turn_timer(id)
    }
}
