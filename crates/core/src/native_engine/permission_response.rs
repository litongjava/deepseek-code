//! Host permission decisions never travel through ordinary user-answer strings.
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{oneshot, watch, Mutex};

pub type Commit = Box<dyn FnOnce() -> Result<()> + Send>;

pub enum PermissionResponse {
    /// Compatibility answers still participate in the same atomic request claim.
    Text(String),
    Allow {
        updated_input: Option<Value>,
        /// A prevalidated host policy patch, published only after effective input validation.
        commit: Option<Commit>,
        /// What the user added when approving ("Yes, use -5 instead"); sent
        /// to the model as a user message after the tool result.
        note: Option<String>,
    },
    Deny {
        message: String,
        interrupt: bool,
    },
}

pub struct Entry {
    sender: Option<oneshot::Sender<PermissionResponse>>,
    closed: watch::Sender<bool>,
}
impl Entry {
    pub fn is_closed(&self) -> bool {
        self.sender.as_ref().is_none_or(oneshot::Sender::is_closed)
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}
pub type Mailbox = Arc<Mutex<HashMap<String, Entry>>>;

pub async fn register(mailbox: &Mailbox, id: &str) -> oneshot::Receiver<PermissionResponse> {
    let (sender, receiver) = oneshot::channel();
    let (closed, _) = watch::channel(false);
    mailbox.lock().await.insert(
        id.into(),
        Entry {
            sender: Some(sender),
            closed,
        },
    );
    receiver
}

pub async fn completion(mailbox: &Mailbox, id: &str) -> watch::Receiver<bool> {
    mailbox
        .lock()
        .await
        .get(id)
        .map(|entry| entry.closed.subscribe())
        .unwrap_or_else(|| watch::channel(true).1)
}

pub async fn respond(
    mailbox: &Mailbox,
    request_id: &str,
    response: PermissionResponse,
) -> Result<()> {
    let mut entries = mailbox.lock().await;
    let entry = entries
        .get_mut(request_id)
        .ok_or_else(|| anyhow!("Unknown or completed tool permission request: {request_id}"))?;
    let sender = entry
        .sender
        .take()
        .ok_or_else(|| anyhow!("Tool permission request already claimed: {request_id}"))?;
    // Close before delivery: host waiters can dismiss their own request while validation runs.
    entry.closed.send_replace(true);
    sender
        .send(response)
        .map_err(|_| anyhow!("Tool permission request is no longer active"))
}

pub(crate) struct Pending {
    pub mailbox: Mailbox,
    pub request_id: String,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.mailbox.try_lock() {
            if let Some(entry) = entries.remove(&self.request_id) {
                entry.closed.send_replace(true);
            }
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let mailbox = self.mailbox.clone();
            let id = self.request_id.clone();
            runtime.spawn(async move {
                if let Some(entry) = mailbox.lock().await.remove(&id) {
                    entry.closed.send_replace(true);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn first_claim_closes_only_its_request_and_losing_updates_never_commit() {
        let mailbox = Mailbox::default();
        let first = register(&mailbox, "first").await;
        let _second = register(&mailbox, "second").await;
        let mut closed = completion(&mailbox, "first").await;
        let second_closed = completion(&mailbox, "second").await;
        respond(&mailbox, "first", PermissionResponse::Text("yes".into()))
            .await
            .unwrap();
        closed.changed().await.unwrap();
        assert!(*closed.borrow());
        assert!(!*second_closed.borrow());
        let committed = Arc::new(AtomicBool::new(false));
        let flag = committed.clone();
        assert!(respond(
            &mailbox,
            "first",
            PermissionResponse::Allow {
                updated_input: None,
                commit: Some(Box::new(move || {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                })),
                note: None,
            }
        )
        .await
        .is_err());
        assert!(!committed.load(Ordering::SeqCst));
        assert!(matches!(first.await.unwrap(), PermissionResponse::Text(_)));
        drop(Pending {
            mailbox: mailbox.clone(),
            request_id: "first".into(),
        });
        assert!(mailbox.lock().await.contains_key("second"));
        assert!(*completion(&mailbox, "first").await.borrow());
    }

    #[tokio::test]
    async fn cancellation_closes_host_watch_and_rejects_late_answer() {
        let mailbox = Mailbox::default();
        let receiver = register(&mailbox, "cancelled").await;
        let mut closed = completion(&mailbox, "cancelled").await;
        drop(Pending {
            mailbox: mailbox.clone(),
            request_id: "cancelled".into(),
        });
        let _ = closed.changed().await;
        assert!(*closed.borrow());
        assert!(receiver.await.is_err());
        assert!(respond(
            &mailbox,
            "cancelled",
            PermissionResponse::Text("yes".into())
        )
        .await
        .is_err());
    }
}
