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
        json!({"id":"older", "source":"native_engine",
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
    let snapshot = json!({"id":"current","source":"native_engine","model":"gpt-6-astra",
        "startedAt":2,"status":"running","inputTokens":null,"outputTokens":null,
        "contextUsage":usage});
    crate::session_commands::save_live_requests(&session, &[snapshot.clone()]).unwrap();
    let live = consistent_report(&session).await;
    assert_eq!(field(&live, "Context window"), "256K tokens");
    assert_eq!(
        field(&live, "Context usage"),
        "unknown (latest request input not reported or fully estimable)"
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
            "source":"native_engine", "startedAt":index+1, "status":"completed",
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
        json!({"id":"r","source":"native_engine",
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
