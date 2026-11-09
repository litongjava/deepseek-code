use super::*;
use clap::Parser;
use deepseek_code_core::{
    db::message_repo,
    native_engine::request_context::ContextUsage,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};

async fn fixture(model: &str, window: Option<u32>) -> (tempfile::TempDir, Session) {
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let provider = serde_json::from_value(json!({
        "id":"fixture", "name":"Fixture", "base_url":"http://127.0.0.1:1",
        "api_key":"fixture-only", "api_format":"openai", "enabled":true,
        "models":[{"id":model,"name":model,"enabled":true,"context_window":window,
            "supports_vision":false,"supports_web_search":false}]
    }))
    .unwrap();
    let selection = crate::config::Selection {
        providers: vec![provider],
        provider_id: "fixture".into(),
        model: model.into(),
    };
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(root.path(), model, "fixture").unwrap();
    let cli = crate::args::Cli::parse_from(["dcl", "--print"]);
    let session = Session::open(&cli, &paths, selection, Some(&id))
        .await
        .unwrap();
    (root, session)
}

fn field<'a>(text: &'a str, name: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&format!("{name}:")))
        .unwrap_or_else(|| panic!("Missing {name}: {text}"))
        .trim()
}

async fn consistent_report(session: &Session) -> String {
    let idle = text(session, "").await.unwrap();
    let running = crate::session_commands::live_inspection(session, "/status")
        .await
        .unwrap();
    assert_eq!(idle, running);
    idle
}

fn save_snapshot(session: &Session, snapshot: Value) {
    let message = session
        .store
        .append(&session.id, "assistant", "fixture")
        .unwrap();
    session
        .store
        .db
        .with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &json!([snapshot])))
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn known_window_survives_missing_request_and_opaque_live_input() {
    let (_root, session) = fixture("gpt-6-astra", None).await;
    let idle = consistent_report(&session).await;
    assert_eq!(field(&idle, "Context window"), "256K tokens");
    assert_eq!(
        field(&idle, "Context usage"),
        "not available (no request yet)"
    );

    save_snapshot(
        &session,
        json!({"id":"older", "source":"native_engine","providerId":"fixture",
        "model":"gpt-6-astra", "startedAt":1, "status":"completed",
        "inputTokens":72000, "outputTokens":100,
        "usage":{"input_tokens":72000,"output_tokens":100}}),
    );
    let usage = ContextUsage::from_body(
        "openai-responses",
        &json!({
            "input":[{"type":"compaction","encrypted_content":"opaque"}]
        }),
        Some(256000),
        8192,
    );
    assert!(usage.estimated_input_tokens.is_none());
    let snapshot = json!({"id":"current","source":"native_engine","providerId":"fixture","model":"gpt-6-astra",
        "startedAt":2,"status":"running","inputTokens":null,"outputTokens":null,
        "contextUsage":usage});
    crate::session_commands::save_live_requests(&session, &[snapshot.clone()]).unwrap();
    let live = consistent_report(&session).await;
    assert_eq!(field(&live, "Context window"), "256K tokens");
    assert_eq!(
        field(&live, "Context usage"),
        "72.0K used (last reported input; latest request input not yet available)"
    );
    assert!(!field(&live, "Context usage").contains('%'));

    save_snapshot(&session, snapshot);
    crate::session_commands::save_live_requests(&session, &[]).unwrap();
    let saved = consistent_report(&session).await;
    assert_eq!(
        field(&saved, "Context window"),
        field(&live, "Context window")
    );
    assert_eq!(
        field(&saved, "Context usage"),
        field(&live, "Context usage")
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn reported_and_estimated_usage_agree_before_and_after_persistence() {
    let (_root, session) = fixture("gpt-6-astra", Some(128000)).await;
    for (index, (reported, expected)) in [
        (Some(32000), "75% left (32.0K used / 128K; last request)"),
        (None, "50% left (64.0K used / 128K; estimated last request)"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut usage =
            ContextUsage::from_body("openai-responses", &json!({"input":[]}), Some(128000), 8192);
        usage.estimated_input_tokens = Some(64000);
        let snapshot = json!({"id":format!("r-{index}"), "model":"gpt-6-astra",
            "source":"native_engine","providerId":"fixture", "startedAt":index+1, "status":"completed",
            "inputTokens":reported, "outputTokens":10,
            "usage":{"input_tokens":reported,"output_tokens":10},"contextUsage":usage});
        crate::session_commands::save_live_requests(&session, &[snapshot.clone()]).unwrap();
        let live = consistent_report(&session).await;
        assert_eq!(field(&live, "Context window"), "128K tokens");
        assert_eq!(field(&live, "Context usage"), expected);
        save_snapshot(&session, snapshot);
        crate::session_commands::save_live_requests(&session, &[]).unwrap();
        let saved = consistent_report(&session).await;
        assert_eq!(field(&saved, "Context usage"), expected);
        assert_eq!(field(&saved, "Token usage"), field(&live, "Token usage"));
    }
    session.close().await.unwrap();
}

#[tokio::test]
async fn unconfigured_unknown_model_does_not_invent_a_window_or_percentage() {
    let (_root, session) = fixture("custom-unknown-model", None).await;
    save_snapshot(
        &session,
        json!({"id":"r","source":"native_engine","providerId":"fixture",
        "model":"custom-unknown-model","status":"completed","usage":{"input_tokens":1000}}),
    );
    let report = consistent_report(&session).await;
    assert_eq!(
        field(&report, "Context window"),
        "unknown (no configured or model default window)"
    );
    assert_eq!(
        field(&report, "Context usage"),
        "1.0K used (last request; remaining capacity unknown)"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn auxiliary_search_cannot_replace_live_parent_context() {
    let (_root, session) = fixture("gpt-6-astra", None).await;
    let parent = json!({"id":"parent", "source":"native_engine","providerId":"fixture", "model":"gpt-6-astra",
        "startedAt":1, "inputTokens":72000, "outputTokens":100});
    let search = json!({"id":"search", "source":"web_search", "model":"gpt-6-astra",
        "startedAt":2, "inputTokens":10, "outputTokens":5});
    crate::session_commands::save_live_requests(&session, &[parent.clone(), search]).unwrap();
    let report = consistent_report(&session).await;
    assert_eq!(
        field(&report, "Context usage"),
        "71% left (72.0K used / 256K; last request)"
    );
    let pending = json!({"id":"pending", "source":"native_engine","providerId":"fixture", "model":"gpt-6-astra",
        "startedAt":3, "inputTokens":null});
    crate::session_commands::save_live_requests(&session, &[parent, pending]).unwrap();
    let report = consistent_report(&session).await;
    assert_eq!(
        field(&report, "Context usage"),
        "72.0K used (last reported input; latest request input not yet available)"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_committed_compaction_replaces_pre_compaction_usage_and_counts_once() {
    let (_root, session) = fixture("gpt-6-astra", Some(128000)).await;
    session.store.append(&session.id, "user", "task").unwrap();
    let answer = session
        .store
        .append(&session.id, "assistant", "progress")
        .unwrap();
    let estimate = |tokens: u64| json!({"serialized_utf8_bytes":tokens * 3,"estimated_tokens":tokens,"unknown_parts":0});
    let usage = json!({"version":1,"history":estimate(1),"instructions":estimate(500),"tools":estimate(700),"output_schema":estimate(0),
        "configured_context_window":null,"requested_output_budget":0,"sent_output_reserve":null,"estimated_input_tokens":null});
    session
        .store
        .db
        .with_conn(|conn| {
            message_repo::save_request_snapshots(
                conn,
                &answer,
                &json!([{"id":"before","source":"native_engine","providerId":"fixture","model":"gpt-6-astra","startedAt":1000,"status":"completed","inputTokens":90000,"usage":{"input_tokens":90000},"contextUsage":usage}]),
            )
        })
        .unwrap()
        .unwrap();
    let prepared =
        deepseek_code_core::services::compaction::prepare(&session.store.db, &session.id).unwrap();
    deepseek_code_core::services::compaction::commit(
        &session.store.db,
        prepared,
        &format!("short summary {}", "z".repeat(200)),
    )
    .unwrap();
    let boundary = deepseek_code_core::services::context_usage::latest_boundary(
        &session.store.db,
        &session.id,
    )
    .unwrap()
    .expect("a boundary");
    let boundary_id = boundary.id.clone();
    let estimate = deepseek_code_core::services::context_usage::estimated_after_compaction(
        &session.store,
        &session.id,
        &session.model,
        Some(&session.provider_id),
    )
    .unwrap()
    .expect("a post-compaction estimate");
    assert!(estimate.complete, "{estimate:?}");
    let report = consistent_report(&session).await;
    assert_eq!(field(&report, "Compactions"), "1");
    let usage = field(&report, "Context usage");
    assert!(usage.ends_with("; estimated after compact)"), "{usage}");
    assert!(!usage.contains("90.0K"), "{usage}");
    let percent = 128_000u64.saturating_sub(estimate.tokens) * 100 / 128_000;
    assert!(
        usage.starts_with(&format!(
            "{percent}% left ({} used",
            token_count(estimate.tokens)
        )),
        "{usage}"
    );

    // A pending request has an estimate but no provider usage yet.
    let mut pending_usage = ContextUsage::from_body(
        "openai-completions",
        &json!({"messages":[]}),
        Some(128000),
        4096,
    );
    pending_usage.estimated_input_tokens = Some(16200);
    let pending = json!({"id":"pending","source":"native_engine","providerId":"fixture","contextBoundary":boundary_id,"model":"gpt-6-astra","status":"running", "contextUsage":pending_usage});
    crate::session_commands::save_live_requests(&session, &[pending.clone()]).unwrap();
    let report = consistent_report(&session).await;
    assert!(
        field(&report, "Context usage").contains("estimated after compact"),
        "{report}"
    );
    save_snapshot(&session, pending);
    crate::session_commands::save_live_requests(&session, &[]).unwrap();
    let report = consistent_report(&session).await;
    assert!(
        field(&report, "Context usage").contains("estimated after compact"),
        "{report}"
    );

    // A request that recorded this boundary restores measured usage.
    save_snapshot(
        &session,
        json!({"id":"after","source":"native_engine","providerId":"fixture","contextBoundary":boundary_id,"model":"gpt-6-astra","startedAt":boundary.created_at_ms.unwrap_or(0) + 1000,"status":"completed","inputTokens":48000,"outputTokens":10,
            "usage":{"input_tokens":48000,"output_tokens":10}}),
    );
    let report = consistent_report(&session).await;
    assert_eq!(
        field(&report, "Context usage"),
        "62% left (48.0K used / 128K; last request)"
    );
    // A late request from the pre-compaction context cannot override it, even
    // when it started in the boundary's own millisecond.
    save_snapshot(
        &session,
        json!({"id":"late-old","source":"native_engine","providerId":"fixture","model":"gpt-6-astra","startedAt":boundary.created_at_ms.unwrap_or(0),"status":"completed","inputTokens":120000,"outputTokens":10,
            "usage":{"input_tokens":120000,"output_tokens":10}}),
    );
    let report = consistent_report(&session).await;
    assert_eq!(
        field(&report, "Context usage"),
        "62% left (48.0K used / 128K; last request)"
    );
    assert_eq!(field(&report, "Compactions"), "1");

    // A second committed compaction counts again.
    session
        .store
        .append(&session.id, "user", "next task")
        .unwrap();
    session
        .store
        .append(&session.id, "assistant", "next progress")
        .unwrap();
    let prepared =
        deepseek_code_core::services::compaction::prepare(&session.store.db, &session.id).unwrap();
    deepseek_code_core::services::compaction::commit(&session.store.db, prepared, "second summary")
        .unwrap();
    let report = consistent_report(&session).await;
    assert_eq!(field(&report, "Compactions"), "2");
    session.close().await.unwrap();
}
