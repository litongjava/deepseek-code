//! Safe per-attempt diagnostics and cancellable reconnect notices.
use super::{ApiError, RequestFailure};
use crate::native_engine::tool_loop::EngineEvent;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

tokio::task_local! { static EVENTS: mpsc::Sender<EngineEvent>; }

pub async fn scope<F: std::future::Future>(
    sender: mpsc::Sender<EngineEvent>,
    future: F,
) -> F::Output {
    EVENTS.scope(sender, future).await
}

pub(super) fn sender() -> Option<mpsc::Sender<EngineEvent>> {
    EVENTS.try_with(Clone::clone).ok()
}

pub fn delay(attempts: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| {
            Duration::from_millis(
                crate::config::OrchestrationConfig::retry_base_backoff_ms()
                    .saturating_mul(1u64 << attempts.saturating_sub(1).min(16)),
            )
        })
        .min(Duration::from_secs(300))
}

pub fn notice(retry: u32, wait: Duration, server: bool) -> String {
    let reconnect = if retry == 1 {
        "正在重连".to_owned()
    } else {
        format!("第{retry}次重连")
    };
    let source = if server {
        "，服务端 Retry-After 指定等待"
    } else {
        "，等待"
    };
    format!(
        "{reconnect}{source} {} 秒后再请求，可取消。",
        wait.as_secs_f64()
    )
}

pub(super) fn safe_failure(error: &anyhow::Error, attempts: u32) -> Value {
    let mut failure = RequestFailure::from_error(error, attempts).as_json();
    if let Some(api) = super::request_failure::find_in_chain::<ApiError>(error) {
        // The correlation ID and provider code were validated at the response
        // boundary, before the free text was reduced, so they survive here.
        failure["cause"] = json!(format!("HTTP {}; remote text omitted", api.status));
        failure["serverRequestId"] = json!(api.server_request_id);
        failure["remoteCode"] = json!(api.remote_code);
    }
    failure
}

pub async fn emit(
    budget: &super::AttemptBudget,
    outcome: &str,
    failure: Option<Value>,
    wait: Option<Duration>,
    server: bool,
    elapsed_ms: Option<u64>,
) {
    let attempts = budget.used();
    let event = json!({"type":"request_attempt", "requestId":budget.id,
        "timestampMs":chrono::Utc::now().timestamp_millis(), "attempts":attempts,
        "retries":attempts.saturating_sub(1), "maxAttempts":budget.max(),
        "outcome":outcome, "failure":failure, "elapsedMs":elapsed_ms,
        "waitMs":wait.map(|d| d.as_millis() as u64), "retryAfter":server,
        "message":wait.map(|d| notice(attempts, d, server))});
    tracing::info!(event = %event, "Model request attempt");
    if let Some(sender) = &budget.events {
        let _ = sender.send(EngineEvent::RequestAttempt(event)).await;
    }
}
