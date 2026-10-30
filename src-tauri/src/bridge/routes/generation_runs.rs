//! Bridge-side bookkeeping for chat turns streamed by `/api/chat`.
//!
//! A run is registered while its stream collector owns the engine receiver and
//! is removed only after the partial or final trajectory has been persisted.
//! Stopping a run makes that collector drop the receiver, which is the same
//! cancellation path as a disconnected response transport.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Default)]
pub struct GenerationRuns {
    next_id: AtomicU64,
    runs: Mutex<HashMap<String, Vec<Arc<Run>>>>,
}

pub struct Run {
    id: u64,
    started_at: String,
    stop: watch::Sender<bool>,
    done: watch::Sender<bool>,
    event_count: AtomicU64,
    progress: Mutex<Progress>,
}

#[derive(Default)]
struct Progress {
    text: String,
    thinking: String,
}

/// Deregisters its run when the collector finishes or is dropped.
pub struct RunGuard {
    runs: Arc<GenerationRuns>,
    conversation_id: String,
    run: Arc<Run>,
}

impl std::ops::Deref for RunGuard {
    type Target = Run;
    fn deref(&self) -> &Run {
        &self.run
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut runs = self.runs.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = runs.get_mut(&self.conversation_id) {
            list.retain(|run| run.id != self.run.id);
            if list.is_empty() {
                runs.remove(&self.conversation_id);
            }
        }
        drop(runs);
        self.run.done.send_replace(true);
    }
}

impl Run {
    /// Resolves once a stop was requested for this run.
    pub async fn stop_requested(&self) {
        let mut stop = self.stop.subscribe();
        let _ = stop.wait_for(|requested| *requested).await;
    }

    pub fn record_event(&self) {
        self.event_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn push_text(&self, text: &str) {
        self.progress().text.push_str(text);
    }

    pub fn push_thinking(&self, thinking: &str) {
        self.progress().thinking.push_str(thinking);
    }

    /// A context reset starts a new assistant segment.
    pub fn reset_progress(&self) {
        *self.progress() = Progress::default();
    }

    fn progress(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    /// Runs that were active when the stop was requested.
    pub requested: usize,
    /// Whether all of them finished persisting before the wait elapsed.
    pub settled: bool,
}

impl GenerationRuns {
    pub fn start(self: &Arc<Self>, conversation_id: &str) -> RunGuard {
        let run = Arc::new(Run {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            started_at: chrono::Utc::now().to_rfc3339(),
            stop: watch::channel(false).0,
            done: watch::channel(false).0,
            event_count: AtomicU64::new(0),
            progress: Mutex::new(Progress::default()),
        });
        self.lock()
            .entry(conversation_id.to_string())
            .or_default()
            .push(run.clone());
        RunGuard {
            runs: self.clone(),
            conversation_id: conversation_id.to_string(),
            run,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Arc<Run>>>> {
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn latest(&self, conversation_id: &str) -> Option<Arc<Run>> {
        self.lock()
            .get(conversation_id)
            .and_then(|list| list.last().cloned())
    }

    pub fn is_active(&self, conversation_id: &str) -> bool {
        self.latest(conversation_id).is_some()
    }

    /// Response body for `GET /api/conversations/{id}/generation-status`.
    pub fn generation_status(&self, conversation_id: &str) -> Value {
        let Some(run) = self.latest(conversation_id) else {
            return json!({ "active": false, "generating": false, "status": "idle" });
        };
        let progress = run.progress();
        json!({
            "active": true,
            "generating": true,
            "status": "generating",
            "stopRequested": *run.stop.borrow(),
            "startedAt": run.started_at,
            "eventCount": run.event_count.load(Ordering::Relaxed),
            "text": progress.text,
            "thinking": progress.thinking,
            // Progress is served from this process, not re-read from SQLite.
            "crossProcess": false,
        })
    }

    /// Response body for `GET /api/conversations/{id}/stream-status`.
    ///
    /// `active` means "a stream can be re-attached". The Bridge has no
    /// reattach route, so a running turn is reported through `generating`,
    /// which leads the UI to follow it with generation-status polling.
    pub fn stream_status(&self, conversation_id: &str) -> Value {
        let run = self.latest(conversation_id);
        json!({
            "active": false,
            "reconnectable": false,
            "generating": run.is_some(),
            "eventCount": run.as_ref().map_or(0, |run| run.event_count.load(Ordering::Relaxed)),
        })
    }

    /// Requests every active run of the conversation to stop and waits up to
    /// `wait` for them to finish persisting. Idempotent when nothing runs.
    pub async fn stop(&self, conversation_id: &str, wait: std::time::Duration) -> StopOutcome {
        let runs = self
            .lock()
            .get(conversation_id)
            .cloned()
            .unwrap_or_default();
        let mut finished = Vec::with_capacity(runs.len());
        for run in &runs {
            run.stop.send_replace(true);
            finished.push(run.done.subscribe());
        }
        let settled = tokio::time::timeout(wait, async {
            for mut done in finished {
                let _ = done.wait_for(|done| *done).await;
            }
        })
        .await
        .is_ok();
        StopOutcome {
            requested: runs.len(),
            settled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn idle_stop_is_a_no_op_and_status_reports_idle() {
        let runs = Arc::new(GenerationRuns::default());
        let outcome = runs.stop("idle", Duration::from_millis(10)).await;
        assert_eq!(
            outcome,
            StopOutcome {
                requested: 0,
                settled: true
            }
        );
        assert_eq!(runs.generation_status("idle")["active"], false);
        assert_eq!(runs.generation_status("idle")["status"], "idle");
        assert_eq!(runs.stream_status("idle")["generating"], false);
    }

    #[tokio::test]
    async fn stop_signals_the_run_and_waits_for_its_guard() {
        let runs = Arc::new(GenerationRuns::default());
        let guard = runs.start("conv");
        guard.push_text("partial");
        guard.record_event();
        let status = runs.generation_status("conv");
        assert_eq!(
            (status["active"].clone(), status["text"].clone()),
            (json!(true), json!("partial"))
        );
        assert_eq!(runs.stream_status("conv")["active"], false);
        assert_eq!(runs.stream_status("conv")["eventCount"], 1);
        let collector = tokio::spawn(async move {
            guard.stop_requested().await;
            drop(guard);
        });
        let outcome = runs.stop("conv", Duration::from_secs(5)).await;
        assert_eq!(
            outcome,
            StopOutcome {
                requested: 1,
                settled: true
            }
        );
        collector.await.unwrap();
        assert!(!runs.is_active("conv"));
        assert_eq!(
            runs.stop("conv", Duration::from_millis(10)).await.requested,
            0
        );
    }

    #[tokio::test]
    async fn an_older_run_finishing_keeps_the_newer_run_registered() {
        let runs = Arc::new(GenerationRuns::default());
        let older = runs.start("conv");
        let newer = runs.start("conv");
        newer.push_text("new");
        drop(older);
        assert_eq!(runs.generation_status("conv")["text"], "new");
        let outcome = runs.stop("conv", Duration::from_millis(20)).await;
        assert_eq!(
            outcome,
            StopOutcome {
                requested: 1,
                settled: false
            }
        );
        drop(newer);
        assert!(!runs.is_active("conv"));
    }
}
