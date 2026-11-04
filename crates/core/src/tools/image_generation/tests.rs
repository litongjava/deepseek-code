use super::*;
use crate::native_engine::provider_manager::Provider;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use std::sync::{Arc, Mutex};

// A complete one-pixel PNG, rather than a header-only image fixture.
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";

async fn gateway(
    value: Value,
    status: StatusCode,
) -> (
    ResolvedProvider,
    Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    tokio::task::JoinHandle<()>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = post(
        move |State(seen): State<Arc<Mutex<Vec<(HeaderMap, Value)>>>>,
              headers: HeaderMap,
              Json(input): Json<Value>| {
            let value = value.clone();
            async move {
                seen.lock().unwrap().push((headers, input));
                (status, Json(value))
            }
        },
    );
    let app = Router::new()
        .route("/tools/codex/v1/images/generations", handler.clone())
        .route("/tools/codex/v1/images/edits", handler)
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider: Provider = serde_json::from_value(json!({"id":"gateway","name":"Gateway","api_key":"gateway-key","api_format":"openai","base_url":format!("http://{}/tools/codex/v1",listener.local_addr().unwrap()),"enabled":true,"models":[{"id":"gpt-6-astra","name":"Chat model","enabled":true,"supports_web_search":false}]})).unwrap();
    let resolved = ResolvedProvider {
        model: provider.models[0].clone(),
        provider,
    };
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (resolved, seen, task)
}

#[tokio::test]
async fn generate_and_edit_use_only_gateway_and_preserve_reference_order() {
    let (provider, seen, server) = gateway(
        json!({"data":[{"b64_json":PNG}],"usage":{"total_tokens":12}}),
        StatusCode::OK,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let generated = generate(json!({"prompt":"A tiny image","file_path":"中文 space/generated.png","transparent_background":true}), cwd, &provider).await.unwrap();
    assert_eq!(generated["image"]["data"], PNG);
    assert_eq!(
        std::fs::read(dir.path().join("中文 space/generated.png")).unwrap(),
        STANDARD.decode(PNG).unwrap()
    );
    std::fs::write(dir.path().join("reference.jpg"), [0xff, 0xd8, 0xff, 0xd9]).unwrap();
    generate(json!({"prompt":"Edit","file_path":"edited.png","referenced_image_paths":["reference.jpg","中文 space/generated.png"]}), cwd, &provider).await.unwrap();
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0["authorization"], "Bearer gateway-key");
    assert_eq!(requests[0].1["model"], "gpt-image-2");
    assert_eq!(requests[0].1["background"], "transparent");
    assert!(requests[0].1.get("images").is_none());
    assert!(requests[1].1["images"][0]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/jpeg;base64,"));
    assert_eq!(
        requests[1].1["images"][1]["image_url"],
        format!("data:image/png;base64,{PNG}")
    );
    server.abort();
}

#[tokio::test]
async fn invalid_response_and_gateway_failure_never_create_files_or_retry() {
    for (value, status) in [
        (json!({"data":[{"b64_json":"invalid!"}]}), StatusCode::OK),
        (json!({"data":[{"b64_json":"aGVsbG8="}]}), StatusCode::OK),
        (
            json!({"error":"unavailable"}),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let (provider, seen, server) = gateway(value, status).await;
        let dir = tempfile::tempdir().unwrap();
        assert!(generate(
            json!({"prompt":"image","file_path":"result.png"}),
            dir.path().to_str().unwrap(),
            &provider
        )
        .await
        .is_err());
        assert!(!dir.path().join("result.png").exists());
        assert_eq!(seen.lock().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn existing_files_and_escaping_paths_are_rejected_before_network() {
    let (provider, seen, server) =
        gateway(json!({"data":[{"b64_json":PNG}]}), StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.png"), b"preserve").unwrap();
    for path in ["existing.png", "../outside.png", "result.jpg"] {
        assert!(generate(
            json!({"prompt":"image","file_path":path}),
            dir.path().to_str().unwrap(),
            &provider
        )
        .await
        .is_err());
    }
    assert_eq!(
        std::fs::read(dir.path().join("existing.png")).unwrap(),
        b"preserve"
    );
    assert!(seen.lock().unwrap().is_empty());
    server.abort();
}
