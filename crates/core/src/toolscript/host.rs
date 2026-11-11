//! Host-call admission, concurrency limiting, logging, and metrics.
//!
//! This module is only compiled where the embedded QuickJS engine is available.
//! It contains no permission or executor logic: a call admitted here goes
//! straight to the caller-supplied [`ToolCallHandler`].

use super::{
    truncate_utf8, Cancellation, ToolCallError, ToolCallHandler, ToolCallRequest, ToolScriptLimits,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;

#[derive(Default)]
struct LogState {
    text: String,
    entries: usize,
}

/// Shared state for the duration of one script run.
pub(super) struct HostState {
    handler: Arc<dyn ToolCallHandler>,
    pub(super) limits: ToolScriptLimits,
    pub(super) cancellation: Cancellation,
    semaphore: Arc<Semaphore>,
    /// Calls admitted but not settled (queued, running, or unawaited).
    pub(super) pending: AtomicUsize,
    next_call_id: AtomicU64,
    /// Total host-call attempts, including ones rejected by an admission limit.
    attempts: AtomicU64,
    active: AtomicUsize,
    pub(super) peak: AtomicUsize,
    pub(super) calls_started: AtomicU64,
    pub(super) calls_succeeded: AtomicU64,
    pub(super) calls_failed: AtomicU64,
    pub(super) calls_cancelled: AtomicU64,
    pub(super) started_at: Instant,
    log: Mutex<LogState>,
}

impl HostState {
    pub(super) fn new(
        handler: Arc<dyn ToolCallHandler>,
        limits: ToolScriptLimits,
        cancellation: Cancellation,
        started_at: Instant,
    ) -> Self {
        let concurrency = limits.max_concurrent_calls;
        Self {
            handler,
            limits,
            cancellation,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            pending: AtomicUsize::new(0),
            next_call_id: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls_started: AtomicU64::new(0),
            calls_succeeded: AtomicU64::new(0),
            calls_failed: AtomicU64::new(0),
            calls_cancelled: AtomicU64::new(0),
            started_at,
            log: Mutex::new(LogState::default()),
        }
    }

    /// Entry point for the hidden JS host callback.
    ///
    /// Always returns a JSON envelope. Returning an envelope instead of a Rust
    /// error keeps normal tool failures visible to the script (`{ok:false}` is
    /// converted into a catchable `ToolScriptError`) rather than collapsing
    /// them into an engine failure.
    pub(super) async fn host_call(&self, name: String, args: Value) -> String {
        if self.cancellation.is_cancelled() {
            return self.error_reply(
                "cancelled",
                "tool call was cancelled before it started",
                &name,
                None,
                None,
            );
        }
        if name.is_empty() || name.len() > self.limits.max_call_name_bytes {
            return self.error_reply(
                "invalid_tool_name",
                &format!(
                    "tool name must be 1..={} bytes",
                    self.limits.max_call_name_bytes
                ),
                &name,
                None,
                None,
            );
        }

        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt > self.limits.max_tool_calls {
            return self.error_reply(
                "too_many_calls",
                &format!(
                    "tool call limit of {} was exceeded",
                    self.limits.max_tool_calls
                ),
                &name,
                None,
                None,
            );
        }

        let pending = self.pending.fetch_add(1, Ordering::SeqCst) + 1;
        if pending > self.limits.max_pending_calls {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            return self.error_reply(
                "too_many_pending_calls",
                &format!(
                    "pending tool call limit of {} was exceeded",
                    self.limits.max_pending_calls
                ),
                &name,
                None,
                None,
            );
        }
        let _pending = PendingGuard(self);

        let call_id = self.next_call_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.calls_started.fetch_add(1, Ordering::SeqCst);

        // A queued call can be cancelled before it ever reaches the handler.
        let _permit = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                self.calls_cancelled.fetch_add(1, Ordering::SeqCst);
                return self.error_reply(
                    "cancelled",
                    "tool call was cancelled while waiting for a concurrency slot",
                    &name,
                    Some(call_id),
                    None,
                );
            }
            acquired = self.semaphore.clone().acquire_owned() => match acquired {
                Ok(permit) => permit,
                Err(_) => {
                    self.calls_failed.fetch_add(1, Ordering::SeqCst);
                    return self.error_reply(
                        "engine_closed",
                        "the host concurrency limiter is closed",
                        &name,
                        Some(call_id),
                        None,
                    );
                }
            },
        };

        let running = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(running, Ordering::SeqCst);
        let _active = ActiveGuard(self);

        let request = ToolCallRequest {
            call_id,
            name: name.clone(),
            args,
        };

        // On cancellation or timeout this drops the handler future. The runner
        // never spawns host work, so dropping the future is the whole contract.
        let result = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(CallFailure::Cancelled),
            timed = tokio::time::timeout(
                self.limits.call_timeout,
                self.handler.call(request, self.cancellation.clone()),
            ) => match timed {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(CallFailure::Tool(error)),
                Err(_) => Err(CallFailure::Timeout),
            },
        };

        match result {
            Ok(value) => match serde_json::to_vec(&value) {
                Ok(bytes) if bytes.len() <= self.limits.max_call_result_bytes => {
                    self.calls_succeeded.fetch_add(1, Ordering::SeqCst);
                    json!({"ok": true, "value": value}).to_string()
                }
                Ok(bytes) => {
                    self.calls_failed.fetch_add(1, Ordering::SeqCst);
                    self.error_reply(
                        "result_too_large",
                        &format!(
                            "tool result is {} bytes, above the {}-byte limit",
                            bytes.len(),
                            self.limits.max_call_result_bytes
                        ),
                        &name,
                        Some(call_id),
                        None,
                    )
                }
                Err(error) => {
                    self.calls_failed.fetch_add(1, Ordering::SeqCst);
                    self.error_reply(
                        "invalid_host_result",
                        &format!("tool result is not serializable as JSON: {error}"),
                        &name,
                        Some(call_id),
                        None,
                    )
                }
            },
            Err(CallFailure::Cancelled) => {
                self.calls_cancelled.fetch_add(1, Ordering::SeqCst);
                self.error_reply(
                    "cancelled",
                    "tool call was cancelled",
                    &name,
                    Some(call_id),
                    None,
                )
            }
            Err(CallFailure::Timeout) => {
                self.calls_failed.fetch_add(1, Ordering::SeqCst);
                self.error_reply(
                    "call_timeout",
                    &format!(
                        "tool call exceeded the {:?} timeout",
                        self.limits.call_timeout
                    ),
                    &name,
                    Some(call_id),
                    None,
                )
            }
            Err(CallFailure::Tool(error)) => {
                self.calls_failed.fetch_add(1, Ordering::SeqCst);
                let details = error.details.filter(|details| self.details_fit(details));
                self.error_reply(&error.code, &error.message, &name, Some(call_id), details)
            }
        }
    }

    fn details_fit(&self, details: &Value) -> bool {
        serde_json::to_vec(details)
            .map(|bytes| bytes.len() <= self.limits.max_diagnostic_bytes)
            .unwrap_or(false)
    }

    pub(super) fn error_reply(
        &self,
        code: &str,
        message: &str,
        tool: &str,
        call_id: Option<u64>,
        details: Option<Value>,
    ) -> String {
        let mut error = json!({
            "code": truncate_utf8(code, 64),
            "message": truncate_utf8(message, self.limits.max_diagnostic_bytes),
            "tool": truncate_utf8(tool, self.limits.max_call_name_bytes),
        });
        if let Some(call_id) = call_id {
            error["callId"] = json!(call_id);
        }
        if let Some(details) = details {
            error["details"] = details;
        }
        json!({"ok": false, "error": error}).to_string()
    }

    /// Appends one log line. The JS prelude enforces the same byte limit; this
    /// is the authoritative backstop because it counts real UTF-8 bytes.
    pub(super) fn push_log(&self, text: &str) -> rquickjs::Result<()> {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let separator = if log.entries == 0 { 0 } else { 1 };
        let total = log.text.len() + separator + text.len();
        if total > self.limits.max_log_bytes {
            return Err(rquickjs::Error::new_from_js_message(
                "log output",
                "string",
                format!(
                    "log output exceeded the {}-byte limit",
                    self.limits.max_log_bytes
                ),
            ));
        }
        if separator == 1 {
            log.text.push('\n');
        }
        log.text.push_str(text);
        log.entries += 1;
        Ok(())
    }

    pub(super) fn logs(&self) -> String {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .text
            .clone()
    }
}

enum CallFailure {
    Cancelled,
    Timeout,
    Tool(ToolCallError),
}

struct PendingGuard<'a>(&'a HostState);

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ActiveGuard<'a>(&'a HostState);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
