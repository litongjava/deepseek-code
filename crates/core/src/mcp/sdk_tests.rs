use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct Fixture {
    calls: Mutex<Vec<(String, Value)>>,
    failure: Mutex<String>,
    closed: AtomicBool,
    active: AtomicBool,
    release: tokio::sync::Notify,
}

impl Host for Fixture {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        message: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((server.into(), message.clone()));
            let failure = self.failure.lock().unwrap().clone();
            if failure == "timeout" || failure == "close" {
                struct Active<'a>(&'a AtomicBool);
                impl Drop for Active<'_> {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                self.active.store(true, Ordering::Release);
                let _active = Active(&self.active);
                self.release.notified().await;
                anyhow::bail!("fixture host closed");
            }
            let method = message["method"].as_str().unwrap();
            let result = match method {
                "initialize" => {
                    json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"fixture","version":"1"}})
                }
                "notifications/initialized" => return Ok(Value::Null),
                "tools/list" => {
                    let mut result = json!({"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]},"outputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]});
                    result["tools"][0]["annotations"] = match failure.as_str() {
                        "readonly" => json!({"readOnlyHint":true}),
                        "readonly-false" => json!({"readOnlyHint":false}),
                        "readonly-malformed" => json!({"readOnlyHint":"true"}),
                        _ => Value::Null,
                    };
                    if failure == "schema" {
                        result["tools"][0]["inputSchema"] =
                            json!({"$ref":"https://invalid/schema"});
                    }
                    if failure == "pagination" {
                        result = json!({"tools":[],"nextCursor":"same"});
                    }
                    result
                }
                "tools/call" => match failure.as_str() {
                    "content" => json!({"content":[{"type":"text","text":42}]}),
                    "binary" => {
                        json!({"content":[{"type":"image","data":"!!!!","mimeType":"image/png"}]})
                    }
                    "output" => json!({"content":[],"structuredContent":{"value":42}}),
                    "flag" => json!({"content":[],"isError":"true"}),
                    "tool-error" => {
                        json!({"content":[{"type":"text","text":"remote failure"}],"isError":true})
                    }
                    _ => {
                        json!({"content":[{"type":"text","text":server}],"structuredContent":{"value":message["params"]["arguments"]["value"]}})
                    }
                },
                "resources/list" => {
                    if failure == "resource-pagination" {
                        json!({"resources":[],"nextCursor":"repeat"})
                    } else if message["params"].get("cursor").is_none() {
                        json!({"resources":[{"uri":"fixture:first","name":"First"}],"nextCursor":"page2"})
                    } else {
                        json!({"resources":[{"uri":"fixture:second","name":"Second"}]})
                    }
                }
                "resources/read" => {
                    if failure == "resource-content" {
                        json!({"contents":[{"uri":"fixture:first","text":42}]})
                    } else {
                        json!({"contents":[{"uri":message["params"]["uri"],"text":"RESOURCE"}]})
                    }
                }
                _ => panic!("unexpected method {method}"),
            };
            if failure == "rpc-error" {
                return Ok(
                    json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32603,"message":"RPC failure"}}),
                );
            }
            Ok(
                json!({"jsonrpc":"2.0","id":if failure == "identity" {json!("foreign")} else {message["id"].clone()},"result":result}),
            )
        })
    }
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.release.notify_waiters();
    }
}

fn setup(failure: &str) -> (Arc<Runtime>, Arc<Fixture>) {
    let host = Arc::new(Fixture::default());
    *host.failure.lock().unwrap() = failure.into();
    let runtime = Arc::new(Runtime::default());
    runtime
        .install(vec!["first".into(), "second".into()], host.clone())
        .unwrap();
    (runtime, host)
}

#[tokio::test]
async fn sdk_mcp_namespaces_schemas_resources_and_cleanup() {
    let (runtime, host) = setup("");
    let tools = runtime.tools().await.unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp__first__echo", "mcp__second__echo"]
    );
    for server in ["first", "second"] {
        let result = runtime
            .call(&format!("mcp__{server}__echo"), json!({"value":"input"}))
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"], server);
        assert_eq!(result["structuredContent"]["value"], "input");
    }
    let before = host.calls.lock().unwrap().len();
    assert!(runtime
        .call("mcp__first__echo", json!({"value":42}))
        .await
        .is_err());
    assert_eq!(host.calls.lock().unwrap().len(), before);
    assert_eq!(
        runtime.resource("first", None).await.unwrap()["resources"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        runtime
            .resource("first", Some("fixture:first"))
            .await
            .unwrap()["contents"][0]["text"],
        "RESOURCE"
    );
    assert!(runtime.resource("foreign", None).await.is_err());
    runtime.clear();
    assert!(host.closed.load(Ordering::Acquire));
    assert!(runtime.names().is_empty());
    assert!(runtime.tools().await.unwrap().is_empty());
    assert!(runtime
        .call("mcp__first__echo", json!({"value":"late"}))
        .await
        .is_err());
}

#[tokio::test]
async fn sdk_mcp_rejects_invalid_identity_schema_and_result_shapes() {
    for failure in ["identity", "rpc-error", "schema", "pagination"] {
        let (runtime, _) = setup(failure);
        assert!(runtime.tools().await.is_err(), "{failure}");
    }
    for failure in ["content", "binary", "output", "flag"] {
        let (runtime, _) = setup(failure);
        assert!(
            runtime
                .call("mcp__first__echo", json!({"value":"input"}))
                .await
                .is_err(),
            "{failure}"
        );
    }
    let (runtime, _) = setup("tool-error");
    assert_eq!(
        runtime
            .call("mcp__first__echo", json!({"value":"input"}))
            .await
            .unwrap()["isError"],
        true
    );
    let (runtime, _) = setup("resource-pagination");
    assert!(runtime.resource("first", None).await.is_err());
    let (runtime, _) = setup("resource-content");
    assert!(runtime
        .resource("first", Some("fixture:first"))
        .await
        .is_err());
}

#[tokio::test(start_paused = true)]
async fn sdk_mcp_timeout_and_clear_drop_pending_exchanges() {
    let (runtime, host) = setup("timeout");
    assert!(runtime
        .tools()
        .await
        .unwrap_err()
        .to_string()
        .contains("timed out"));
    assert!(!host.active.load(Ordering::Acquire));
    let (runtime, host) = setup("close");
    let pending = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.tools().await }
    });
    tokio::task::yield_now().await;
    assert!(host.active.load(Ordering::Acquire));
    runtime.clear();
    assert!(pending.await.unwrap().is_err());
    assert!(!host.active.load(Ordering::Acquire));
}

#[test]
fn sdk_mcp_names_reject_collisions_and_protocol_syntax() {
    for names in [
        json!(["same", "same"]),
        json!(["ide"]),
        json!(["bad/name"]),
        json!([""]),
        json!([42]),
        json!({}),
    ] {
        assert!(validate_names(&names).is_err());
    }
    assert_eq!(
        validate_names(&json!(["first", "second-2"])).unwrap(),
        vec!["first", "second-2"]
    );
}

#[tokio::test]
async fn sdk_mcp_preserves_read_only_annotations_without_coercion() {
    for (fixture, expected) in [
        ("readonly", true),
        ("readonly-false", false),
        ("readonly-malformed", false),
        ("", true),
    ] {
        let (runtime, _) = setup(fixture);
        let tools = runtime.tools().await.unwrap();
        assert_eq!(tools[0].is_read_only(), expected);
        assert_eq!(
            tools[0].annotations.as_ref().unwrap()["readOnlyHint"],
            match fixture {
                "readonly" => json!(true),
                "readonly-false" => json!(false),
                "readonly-malformed" => json!("true"),
                _ => Value::Null,
            }
        );
        runtime.clear();
        assert!(runtime.tools().await.unwrap().is_empty());
    }
}
