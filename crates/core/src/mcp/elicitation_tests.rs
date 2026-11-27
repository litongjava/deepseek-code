use super::elicitation::*;
use crate::services::tool_hooks::HookRunner;
use crate::services::{EnvironmentCredentials, ServiceContext};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn params() -> Value {
    json!({"message":"Choose a number","requestedSchema":{"type":"object","properties":{"number":{"type":"integer","minimum":1}},"required":["number"],"additionalProperties":false}})
}
fn fixture(hooks: Value, answer: Value) -> (tempfile::TempDir, RequestContext, Arc<AtomicUsize>) {
    let root = tempfile::tempdir().unwrap();
    crate::data_layout::write_config_file(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
        json!({"hooks":hooks}).to_string(),
    )
    .unwrap();
    let service = ServiceContext::open(
        crate::runtime::RuntimePaths::new(root.path().into()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let runner = HookRunner::load_with_plugins(&service, root.path()).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let context = RequestContext {
        hooks: Some(Arc::new(runner)),
        session: uuid::Uuid::new_v4().to_string(),
        ask: Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            let answer = answer.clone();
            Box::pin(async move { Ok(answer) })
        }),
    };
    (root, context, count)
}
fn command(event: &str, action: &str, content: Option<Value>) -> Value {
    let mut output = json!({"hookSpecificOutput":{"hookEventName":event,"action":action}});
    if let Some(content) = content {
        output["hookSpecificOutput"]["content"] = content;
    }
    // JSON is encoded as bytes to avoid platform shell quoting differences.
    let bytes = serde_json::to_vec(&output).unwrap();
    json!({"matcher":"^fixture$","hooks":[{"type":"command","command":format!("node -e \"process.stdout.write(Buffer.from({bytes:?}))\"")} ]})
}

#[tokio::test]
async fn direct_accept_decline_cancel_and_payload_matcher() {
    for action in ["accept", "decline", "cancel"] {
        let (root, context, count) = fixture(
            json!({"Elicitation":[command("Elicitation",action,Some(json!({"number":2})))]}),
            json!({"action":"cancel"}),
        );
        let result = CONTEXT
            .scope(context, handle("fixture", params()))
            .await
            .unwrap();
        assert_eq!(result["action"], action);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        if action != "accept" {
            assert!(result.get("content").is_none());
        }
        drop(root);
    }
    let (_root, context, count) = fixture(
        json!({"Elicitation":[command("Elicitation","accept",Some(json!({"number":2})))]}),
        json!({"action":"cancel"}),
    );
    assert_eq!(
        CONTEXT
            .scope(context, handle("other", params()))
            .await
            .unwrap()["action"],
        "cancel"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let payload = input("fixture", &params()).unwrap();
    assert_eq!(
        payload,
        json!({"mcp_server_name":"fixture","message":"Choose a number","mode":"form","requested_schema":params()["requestedSchema"]})
    );
}

#[tokio::test]
async fn user_flow_and_noninteractive_cancel() {
    let (_root, context, count) =
        fixture(json!({}), json!({"action":"accept","content":{"number":3}}));
    assert_eq!(
        CONTEXT
            .scope(context, handle("fixture", params()))
            .await
            .unwrap()["content"]["number"],
        3
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        handle("fixture", params()).await.unwrap(),
        json!({"action":"cancel"})
    );
}

#[tokio::test]
async fn result_modifies_blocks_and_preserves_content() {
    for (action, content, expected) in [
        (
            "accept",
            Some(json!({"number":9})),
            json!({"action":"accept","content":{"number":9}}),
        ),
        (
            "accept",
            None,
            json!({"action":"accept","content":{"number":3}}),
        ),
        ("decline", None, json!({"action":"decline"})),
        ("cancel", None, json!({"action":"cancel"})),
    ] {
        let (_root, context, _) = fixture(
            json!({"ElicitationResult":[command("ElicitationResult",action,content)]}),
            json!({"action":"accept","content":{"number":3}}),
        );
        assert_eq!(
            CONTEXT
                .scope(context, handle("fixture", params()))
                .await
                .unwrap(),
            expected
        );
    }
    let (_root, context, _) = fixture(
        json!({"ElicitationResult":[{"hooks":[{"type":"command","command":"node -e \"process.exit(2)\""}]}]}),
        json!({"action":"accept","content":{"number":3}}),
    );
    assert_eq!(
        CONTEXT
            .scope(context, handle("fixture", params()))
            .await
            .unwrap()["action"],
        "decline"
    );
}

#[tokio::test]
async fn invalid_schema_content_and_unsupported_url_return_rpc_errors() {
    for event in ["Elicitation", "ElicitationResult"] {
        let (_root, context, _) = fixture(
            json!({event:[command(event,"accept",Some(json!({"number":"bad"})))]}),
            json!({"action":"accept","content":{"number":3}}),
        );
        let result = CONTEXT.scope(context, reply("fixture", &json!({"id":"server-string-id","method":"elicitation/create","params":params()}))).await;
        assert_eq!(result["id"], "server-string-id");
        assert_eq!(result["error"]["code"], -32602);
    }
    assert!(input(
        "fixture",
        &json!({"message":"url","mode":"url","url":"https://example.invalid"})
    )
    .is_err());
    assert!(input(
        "fixture",
        &json!({"message":"bad","requestedSchema":{"$ref":"https://example.invalid/schema"}})
    )
    .is_err());
}

#[tokio::test]
async fn hook_timeout_and_failure_never_supply_accept() {
    for command in [
        "node -e \"setTimeout(()=>{},5000)\"",
        "node -e \"process.exit(1)\"",
        "node -e \"console.log('{bad')\"",
    ] {
        let (_root, context, count) = fixture(
            json!({"Elicitation":[{"hooks":[{"type":"command","command":command,"timeout":1}]}]}),
            json!({"action":"cancel"}),
        );
        assert_eq!(
            CONTEXT
                .scope(context, handle("fixture", params()))
                .await
                .unwrap()["action"],
            "cancel"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn result_callback_timeout_keeps_cancel_and_contexts_do_not_leak() {
    struct Slow;
    impl crate::services::hook_callbacks::Host for Slow {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            _: Value,
            _: Option<String>,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Value>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"ElicitationResult","action":"accept","content":{"number":9}}}),
                )
            })
        }
    }
    let (_root, context, _) = fixture(json!({}), json!({"action":"cancel"}));
    let host: Arc<dyn crate::services::hook_callbacks::Host> = Arc::new(Slow);
    let _guard = crate::services::hook_callbacks::install(
        &context.session,
        crate::services::hook_callbacks::validate(
            &json!({"ElicitationResult":[{"hookCallbackIds":["slow"],"timeout":0.01}]}),
        )
        .unwrap(),
        host.clone(),
    );
    // Reload after registration, as the engine does for each turn.
    let service = ServiceContext::open(
        crate::runtime::RuntimePaths::new(_root.path().into()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let context = RequestContext {
        hooks: Some(Arc::new(
            HookRunner::load_with_plugins(&service, _root.path()).unwrap(),
        )),
        ..context
    };
    assert_eq!(
        CONTEXT
            .scope(context, handle("fixture", params()))
            .await
            .unwrap()["action"],
        "cancel"
    );
    let (_a, a, _) = fixture(json!({}), json!({"action":"accept","content":{"number":1}}));
    let (_b, b, _) = fixture(json!({}), json!({"action":"accept","content":{"number":2}}));
    let (a, b) = tokio::join!(
        CONTEXT.scope(a, handle("fixture", params())),
        CONTEXT.scope(b, handle("fixture", params()))
    );
    assert_eq!(a.unwrap()["content"]["number"], 1);
    assert_eq!(b.unwrap()["content"]["number"], 2);
    assert_eq!(
        handle("fixture", params()).await.unwrap()["action"],
        "cancel"
    );
}

#[test]
fn terminal_input_and_async_configuration_are_explicit() {
    assert_eq!(
        user_response("{\"number\":4}").unwrap(),
        json!({"action":"accept","content":{"number":4}})
    );
    assert_eq!(user_response("decline").unwrap()["action"], "decline");
    assert_eq!(user_response("cancel").unwrap()["action"], "cancel");
    assert!(user_response("yes").is_err());
    let root = tempfile::tempdir().unwrap();
    let service = ServiceContext::open(
        crate::runtime::RuntimePaths::new(root.path().into()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    for option in ["async", "asyncRewake"] {
        crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),json!({"hooks":{"Elicitation":[{"hooks":[{"type":"command","command":"echo ignored",option:true}]}]}}).to_string()).unwrap();
        assert!(HookRunner::load_with_plugins(&service, root.path()).is_err());
    }
}
