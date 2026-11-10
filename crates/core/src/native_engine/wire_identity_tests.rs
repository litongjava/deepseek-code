use super::*;
use crate::native_engine::{
    provider_manager::{Provider, ResolvedProvider},
    request_identity::{ids, RequestIdentity, TurnIdentity},
};
use serde_json::json;

fn provider(api: Option<&str>, identity: WireIdentity) -> ResolvedProvider {
    let mut p: Provider = serde_json::from_value(json!({
        "id": "fixture", "name": "fixture", "base_url": "http://127.0.0.1:1",
        "api_key": "test-only", "api_format": "openai", "enabled": true,
        "models": [{"id":"fixture-model","name":"fixture","enabled":true,"max_tokens":8192,"supports_web_search":false,"api":api}]
    }))
    .unwrap();
    let mut model = p.models.remove(0);
    model.wire_identity = identity;
    ResolvedProvider {
        provider: p,
        model,
    }
}

#[test]
fn profiles_parse_and_unknown_values_fall_back() {
    assert_eq!(WireIdentity::parse("codex"), Some(WireIdentity::Codex));
    assert_eq!(WireIdentity::parse(" COMPAT "), Some(WireIdentity::Compat));
    assert_eq!(WireIdentity::parse("off"), Some(WireIdentity::Default));
    assert_eq!(WireIdentity::parse("turbo"), None);
    assert_eq!(
        WireIdentity::from_setting(Some("turbo")),
        WireIdentity::Default
    );
    assert_eq!(WireIdentity::from_setting(None), WireIdentity::Default);
    assert_eq!(WireIdentity::Compat.as_str(), "compat");
    assert_eq!(WireIdentity::Codex.as_str(), "codex");
}

#[test]
fn profiles_apply_only_to_responses_models() {
    assert_eq!(
        active(&provider(Some("openai-responses"), WireIdentity::Codex)),
        WireIdentity::Codex
    );
    assert_eq!(
        active(&provider(
            Some("openai-codex-responses"),
            WireIdentity::Compat
        )),
        WireIdentity::Compat
    );
    assert_eq!(
        active(&provider(Some("openai-completions"), WireIdentity::Codex)),
        WireIdentity::Default
    );
    assert_eq!(
        active(&provider(Some("openai-responses"), WireIdentity::Default)),
        WireIdentity::Default
    );
}

#[test]
fn default_profile_leaves_the_request_untouched() {
    let mut body = json!({"model":"m","input":[],"max_output_tokens":8});
    apply_to_responses_body(
        &mut body,
        &provider(Some("openai-responses"), WireIdentity::Default),
        &RequestIds::default(),
    );
    assert_eq!(body, json!({"model":"m","input":[],"max_output_tokens":8}));
}

#[tokio::test]
async fn compat_adds_fields_without_identifying_another_client() {
    let provider = provider(Some("openai-responses"), WireIdentity::Compat);
    let body = TurnIdentity::new(Some("session-one".into()), None)
        .scope(async {
            let mut body = json!({
                "model":"m","input":[],
                "tools":[{"type":"function","name":"Read"}],
                "reasoning":{"effort":"high","summary":"auto"}
            });
            apply_to_responses_body(&mut body, &provider, &ids());
            body
        })
        .await;
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["prompt_cache_key"], "session-one");
    assert_eq!(body["text"]["verbosity"], "low");
    assert_eq!(body["reasoning"]["context"], "all_turns");
    // The reasoning block keeps the fields it already had.
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body.get("client_metadata").is_none());
}

#[test]
fn tool_choice_is_not_sent_without_tools() {
    let provider = provider(Some("openai-responses"), WireIdentity::Compat);
    let mut body = json!({"model":"m","input":[],"tools":[]});
    apply_to_responses_body(
        &mut body,
        &provider,
        &RequestIds {
            session: Some("session-one".into()),
            turn: Some("turn-one".into()),
            ..Default::default()
        },
    );
    // A hint without a tool list is rejected by some endpoints.
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
    // The remaining fields are unrelated to the tool list.
    assert_eq!(body["prompt_cache_key"], "session-one");
    assert_eq!(body["text"]["verbosity"], "low");
}

#[tokio::test]
async fn codex_adds_client_metadata_bound_to_the_turn() {
    let provider = provider(Some("openai-responses"), WireIdentity::Codex);
    let (body, turn) = TurnIdentity::new(Some("session-one".into()), None)
        .scope(async {
            let turn = ids().turn.expect("turn ID inside a turn");
            let mut body = json!({"model":"m","input":[]});
            apply_to_responses_body(&mut body, &provider, &ids());
            (body, turn)
        })
        .await;
    assert_eq!(body["client_metadata"]["session_id"], "session-one");
    assert_eq!(body["client_metadata"]["thread_id"], "session-one");
    assert_eq!(body["client_metadata"]["x-codex-window-id"], "session-one:0");
    assert_eq!(body["client_metadata"]["turn_id"], turn.as_str());
    assert_eq!(body["client_metadata"]["root_turn_id"], turn.as_str());
    let metadata: Value = serde_json::from_str(
        body["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["turn_id"], turn.as_str());
    assert_eq!(metadata["session_id"], "session-one");
    assert_eq!(metadata["request_kind"], "turn");
    // Identifiers only: nothing about this machine's paths or remotes.
    assert!(metadata.get("workspaces").is_none());
}

#[tokio::test]
async fn codex_headers_replace_the_client_identity() {
    let provider = provider(Some("openai-responses"), WireIdentity::Codex);
    let headers = TurnIdentity::new(Some("session-one".into()), None)
        .scope(async { RequestIdentity::new(None, &provider).headers() })
        .await;
    assert_eq!(headers["originator"], "codex-tui");
    assert!(headers["user-agent"]
        .to_str()
        .unwrap()
        .starts_with("codex-tui/"));
    assert_eq!(headers["session-id"], "session-one");
    assert_eq!(headers["thread-id"], "session-one");
    assert_eq!(headers["x-codex-window-id"], "session-one:0");
    assert!(headers["x-codex-turn-metadata"]
        .to_str()
        .unwrap()
        .contains("\"turn_id\""));
    for name in [
        "x-app",
        "x-dcl-runtime",
        "x-dcl-session-id",
        "x-parent-session-id",
        "x-dcl-turn-id",
        "x-dcl-retry-count",
    ] {
        assert!(!headers.contains_key(name), "{name} was still sent");
    }
    assert!(headers.contains_key("x-request-id"));
}

#[tokio::test]
async fn compat_keeps_the_client_identity() {
    let provider = provider(Some("openai-responses"), WireIdentity::Compat);
    let headers = TurnIdentity::new(Some("session-one".into()), None)
        .scope(async { RequestIdentity::new(None, &provider).headers() })
        .await;
    // The compat layer adds fields only; the client keeps its own identity.
    assert_ne!(headers["originator"], "codex-tui");
    assert_eq!(headers["x-dcl-session-id"], "session-one");
    assert!(!headers.contains_key("session-id"));
    assert!(!headers.contains_key("x-codex-window-id"));
}
