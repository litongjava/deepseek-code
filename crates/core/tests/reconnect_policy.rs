use deepseek_code_core::{
    native_engine::tool_loop::EngineEvent,
    performance::{self, retry_events, ApiError, AttemptBudget},
};
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

#[tokio::test(start_paused = true)]
async fn eight_attempts_have_seven_exponential_waits_and_safe_distinct_errors() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let count = Arc::new(AtomicU32::new(0));
    let calls = count.clone();
    let start = tokio::time::Instant::now();
    let result: anyhow::Result<()> = retry_events::scope(tx, async move {
        let budget = AttemptBudget::configured();
        performance::retry_with_backoff_budget(
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Err(anyhow::Error::new(ApiError {
                        status: if n % 2 == 0 { 500 } else { 502 },
                        retry_after: None,
                        message: "internal_error request_id: 11111111-2222-4333-8444-555555555555 PRIVATE_PROMPT Cookie: secret".into(),
                        // The response boundary captures both values from the
                        // structured provider document; free text is never parsed.
                        server_request_id: Some(
                            "11111111-2222-4333-8444-555555555555".into(),
                        ),
                        remote_code: Some("internal_error".into()),
                    }
                    ).context("Authorization: Bearer nested-secret"))
                }
            },
            &budget,
        )
        .await
    })
    .await;
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 8);
    assert_eq!(start.elapsed(), Duration::from_secs(254));
    let mut records = Vec::new();
    while let Some(EngineEvent::RequestAttempt(event)) = rx.recv().await {
        records.push(event);
    }
    assert_eq!(records.len(), 8);
    for (i, event) in records.iter().enumerate() {
        assert_eq!(event["attempts"], i + 1);
        assert_eq!(event["failure"]["http"], if i % 2 == 0 { 500 } else { 502 });
        assert!(!event.to_string().contains("PRIVATE_PROMPT"));
        assert!(!event.to_string().contains("secret"));
        assert_eq!(event["failure"]["remoteCode"], "internal_error");
        assert_eq!(
            event["failure"]["serverRequestId"],
            "11111111-2222-4333-8444-555555555555"
        );
        assert!(event["elapsedMs"].is_u64());
        assert_eq!(event["requestId"], records[0]["requestId"]);
        if i < 7 {
            assert_eq!(event["waitMs"], 2000u64 << i);
        }
    }
    assert_eq!(records[7]["outcome"], "failed");
    assert!(records[7]["waitMs"].is_null());
    assert!(records[0]["message"].as_str().unwrap().contains("正在重连"));
    assert!(records[6]["message"]
        .as_str()
        .unwrap()
        .contains("第7次重连"));
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_capped_and_cancellation_prevents_another_attempt() {
    assert_eq!(
        retry_events::delay(1, Some(Duration::from_secs(999))),
        Duration::from_secs(300)
    );
    let count = AtomicU32::new(0);
    let budget = AttemptBudget::new(8);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        performance::retry_with_backoff_budget(
            || {
                count.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<(), _>(
                        ApiError {
                            status: 503,
                            retry_after: Some(Duration::from_secs(300)),
                            message: "unavailable".into(),
                            server_request_id: None,
                            remote_code: None,
                        }
                        .into(),
                    )
                }
            },
            &budget,
        ),
    )
    .await;
    assert!(result.is_err());
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(retry_events::notice(2, Duration::from_secs(300), true).contains("Retry-After"));
}
