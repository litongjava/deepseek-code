//! Retry a disconnected model stream only before any content is exposed.
use super::request_identity::RequestIdentity;
use crate::performance::{AttemptBudget, AttemptCounter, RequestFailure};
use anyhow::Result;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;
#[cfg(test)]
use std::time::Duration;

async fn send(
    request: &reqwest::RequestBuilder,
    identity: &RequestIdentity,
    budget: &AttemptBudget,
    attempts: &AttemptCounter,
) -> Result<reqwest::Response> {
    let result = crate::performance::retry_with_backoff_budget(
        || async {
            let request = request
                .try_clone()
                .ok_or_else(|| anyhow::anyhow!("Model request cannot be replayed"))?;
            let response = crate::performance::checked_response(
                request.headers(identity.headers()).send().await?,
            )
            .await?;
            // The status of the attempt that produced this response stays known,
            // so a stream ending without a completion marker can report it.
            attempts.set_status(response.status().as_u16());
            Ok(response)
        },
        budget,
    )
    .await;
    // Record what the server really saw, including the failing attempt.
    attempts.set_attempts(budget.used());
    result
}

/// Empty handshakes are withheld so a retry cannot duplicate observable content.
///
/// Send retries and pre-content stream retries share one attempt budget, so a
/// model request never exceeds `1 + http.max_retries` HTTP attempts in total.
pub(super) async fn response(
    request: reqwest::RequestBuilder,
    identity: RequestIdentity,
) -> Result<(
    Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
    AttemptCounter,
)> {
    let mut budget = AttemptBudget::configured();
    budget.id = identity.id().to_owned();
    let attempts = AttemptCounter::default();
    let first = send(&request, &identity, &budget, &attempts).await?;
    let counter = attempts.clone();
    let retries = crate::config::OrchestrationConfig::max_retries();
    let stream = Box::pin(async_stream::try_stream! {
        let mut next = Some(first);
        for attempt in 0..=retries {
            let stream = super::utf8_stream::response(next.take().unwrap());
            futures::pin_mut!(stream);
            let mut pending = String::new();
            let mut unparsed = String::new();
            let mut exposed = false;
            let mut failure = None;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => { failure = Some(error); break; }
                };
                if exposed {
                    yield chunk;
                    continue;
                }
                pending.push_str(&chunk);
                unparsed.push_str(&chunk);
                let parsed = crate::streaming::sse_parser::consume_sse_payloads(&unparsed);
                unparsed = parsed.remainder;
                // Unknown events, completion, errors and any content commit the stream.
                if pending.len() > 65_536 || parsed.payloads.iter().any(|event| !empty_prelude(event)) {
                    exposed = true;
                    unparsed.clear();
                    yield std::mem::take(&mut pending);
                }
            }
            let transport_failure = failure.as_ref().is_none_or(|error| {
                error.downcast_ref::<reqwest::Error>().is_some_and(|http| http.is_body() || http.is_decode() || http.is_timeout() || http.is_connect())
            });
            // A partial event is not proof of an empty response; preserve it unmodified.
            if exposed || !unparsed.trim().is_empty() || !transport_failure {
                if !pending.is_empty() { yield pending; }
                if let Some(ref error) = failure {
                    let detail = RequestFailure::stream(error, Some(budget.used()), attempts.status(), exposed);
                    crate::performance::retry_events::emit(&budget, "stream_failed", Some(detail.as_json()), None, false, None).await;
                    Err(anyhow::Error::new(detail))?;
                }
                break;
            }
            if attempt == retries || budget.remaining() == 0 {
                if !pending.is_empty() { yield pending; }
                let detail = match &failure {
                    Some(error) => RequestFailure::stream(error, Some(budget.used()), attempts.status(), false),
                    None => RequestFailure::stream_exhausted(budget.used(), attempts.status()),
                };
                crate::performance::retry_events::emit(&budget, "stream_failed", Some(detail.as_json()), None, false, None).await;
                Err(anyhow::Error::new(detail))?;
            }
            let delay = crate::performance::retry_events::delay(budget.used(), None);
            let detail = match &failure {
                Some(error) => RequestFailure::stream(error, Some(budget.used()), attempts.status(), false),
                None => RequestFailure::stream_exhausted(budget.used(), attempts.status()),
            };
            crate::performance::retry_events::emit(&budget, "retry_wait", Some(detail.as_json()), Some(delay), false, None).await;
            // Dropping the consumer cancels this delay and the next attempt.
            tokio::time::sleep(delay).await;
            next = Some(send(&request, &identity, &budget, &attempts).await?);
        }
    });
    Ok((stream, counter))
}

fn empty_prelude(payload: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(payload) else {
        return false;
    };
    if !event["error"].is_null()
        || !event["provider_content"].is_null()
        || event["response_content_started"] == true
    {
        return false;
    }
    match event["type"].as_str() {
        Some("ping") => true,
        Some("message_start") => {
            let message = &event["message"];
            message["content"].as_array().is_some_and(Vec::is_empty)
                && message["stop_reason"].is_null()
                && message["usage"]["output_tokens"].as_u64().unwrap_or(0) == 0
        }
        Some("content_block_start") => {
            let block = &event["content_block"];
            matches!(block["type"].as_str(), Some("text" | "thinking"))
                && ["text", "thinking", "signature"]
                    .iter()
                    .all(|key| block[*key].is_null() || block[*key] == "")
        }
        None => event["choices"].as_array().is_some_and(|choices| {
            event["usage"]["completion_tokens"].as_u64().unwrap_or(0) == 0
                && choices.iter().all(|choice| {
                    choice["finish_reason"].is_null()
                        && choice["delta"].as_object().is_some_and(|delta| {
                            delta.iter().all(|(key, value)| {
                                (key == "role" && value == "assistant")
                                    || (matches!(
                                        key.as_str(),
                                        "content" | "reasoning_content" | "reasoning"
                                    ) && (value.is_null() || value == ""))
                            })
                        })
                })
        }),
        _ => false,
    }
}

#[cfg(test)]
#[path = "stream_retry_tests.rs"]
mod tests;
