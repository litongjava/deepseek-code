//! Ephemeral hook activity. No status is restored from a prior process/session.
use std::{collections::BTreeMap, sync::OnceLock};
use tokio::sync::watch;

type State = BTreeMap<(String, String), String>;
fn channel() -> &'static watch::Sender<State> {
    static CHANNEL: OnceLock<watch::Sender<State>> = OnceLock::new();
    CHANNEL.get_or_init(|| watch::channel(State::new()).0)
}

pub struct Subscription {
    session: String,
    receiver: watch::Receiver<State>,
    previous: Vec<(String, String)>,
}

pub fn subscribe(session: &str) -> Subscription {
    let mut subscription = Subscription {
        session: session.into(),
        receiver: channel().subscribe(),
        previous: Vec::new(),
    };
    subscription.previous = subscription.snapshot();
    subscription
}

impl Subscription {
    pub fn snapshot(&self) -> Vec<(String, String)> {
        self.receiver
            .borrow()
            .iter()
            .filter(|((session, _), _)| session == &self.session)
            .map(|((_, id), message)| (id.clone(), message.clone()))
            .collect()
    }
    pub async fn changed(&mut self) -> Vec<(String, String)> {
        loop {
            let _ = self.receiver.changed().await;
            let snapshot = self.snapshot();
            if snapshot != self.previous {
                self.previous = snapshot.clone();
                return snapshot;
            }
        }
    }
}

pub(crate) struct Running {
    session: String,
    id: String,
}

impl Running {
    pub(crate) fn start(session: &str, message: Option<&str>) -> Option<Self> {
        let message = message?;
        let message: String = message
            .chars()
            .filter(|c| !c.is_control())
            .take(4096)
            .collect();
        if message.trim().is_empty() {
            return None;
        }
        let guard = Self {
            session: session.into(),
            id: uuid::Uuid::new_v4().to_string(),
        };
        channel().send_modify(|state| {
            state.insert((guard.session.clone(), guard.id.clone()), message.clone());
        });
        guard.emit(Some(&message));
        Some(guard)
    }

    fn emit(&self, message: Option<&str>) {
        if let Err(error) = super::session_worker::emit_event(&serde_json::json!({
            "type":"hook_progress", "session_id":self.session,
            "hook_id":self.id, "status_message":message
        })) {
            tracing::warn!("Hook progress delivery failed: {error}");
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        channel().send_modify(|state| {
            state.remove(&(self.session.clone(), self.id.clone()));
        });
        self.emit(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn overlapping_activity_clears_independently_and_never_crosses_sessions() {
        let session = uuid::Uuid::new_v4().to_string();
        let mut receiver = subscribe(&session);
        let other = Running::start("other-session", Some("secret"));
        assert!(receiver.snapshot().is_empty());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), receiver.changed())
                .await
                .is_err()
        );
        let first = Running::start(&session, Some("first\n"));
        assert_eq!(receiver.changed().await[0].1, "first");
        let second = Running::start(&session, Some("second"));
        assert_eq!(receiver.changed().await.len(), 2);
        drop(first);
        assert_eq!(receiver.changed().await[0].1, "second");
        drop(second);
        assert!(receiver.changed().await.is_empty());
        drop(other);
    }

    #[tokio::test]
    async fn real_hook_success_failure_timeout_and_cancel_clear_status() {
        use super::super::tool_hooks::{HookEvent, HookRunner};
        for mode in ["success", "failure", "timeout", "cancel"] {
            let root = tempfile::tempdir().unwrap();
            let session = uuid::Uuid::new_v4().to_string();
            let mut subscription = subscribe(&session);
            let wait = if cfg!(windows) {
                "while (!(Test-Path -LiteralPath 'release')) { Start-Sleep -Milliseconds 20 }"
            } else {
                "while [ ! -f release ]; do sleep 0.02; done"
            };
            let command = format!("{wait}; exit {}", if mode == "failure" { 2 } else { 0 });
            crate::data_layout::write_config_file(
                crate::data_layout::DataLayout::from_root(&root.path())
                    .config_file("settings.json"),
                serde_json::json!({
                    "hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":command,
                        "shell":if cfg!(windows) {"powershell"} else {"sh"}, "once":true,
                        "statusMessage":"Checking fixture", "timeout":2}]}]}
                })
                .to_string(),
            )
            .unwrap();
            let runner = HookRunner::load(root.path(), root.path()).unwrap();
            let worker_session = session.clone();
            let task = tokio::spawn(async move {
                runner
                    .run(
                        HookEvent::PreToolUse,
                        &worker_session,
                        serde_json::json!({"tool_name":"Read"}),
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while subscription.snapshot().is_empty() {
                    subscription.changed().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(subscription.snapshot()[0].1, "Checking fixture");
            match mode {
                "cancel" => {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                }
                "timeout" => {
                    assert!(task.await.unwrap().is_err());
                }
                _ => {
                    std::fs::write(root.path().join("release"), "").unwrap();
                    assert_eq!(task.await.unwrap().unwrap().blocked, mode == "failure");
                }
            }
            assert!(subscription.snapshot().is_empty());
        }
    }
}
