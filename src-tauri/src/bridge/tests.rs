//! Integration tests for the bridge HTTP API.
//!
//! Covers:
//! - Auth middleware rejects requests without a valid API key
//! - Auth middleware accepts x-api-key header
//! - Auth middleware accepts Bearer token
//! - CORS preflight (`OPTIONS`) bypasses auth
//! - Health endpoint is public and returns 200
//! - Content-Security-Policy header is present on responses

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderName, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::json;
use tower::ServiceExt;

/// CSP header value applied to all bridge responses.
const CSP_VALUE: &str =
    "default-src 'self'; script-src 'self' 'nonce-${nonce}'; \
     style-src 'self' 'nonce-${nonce}' https://fonts.googleapis.com; \
     img-src 'self' asset: https: data: blob:; \
     font-src 'self' https://fonts.gstatic.com; \
     connect-src 'self' http://127.0.0.1:30085 http://127.0.0.1:30090; \
     frame-src 'self' blob: data:; \
     media-src 'self' blob:; \
     worker-src 'self' blob:;";

/// Public health-endpoint handler (mirrors the real bridge behaviour).
async fn health_stub() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"status": "healthy"})))
}

/// Stub handler for protected routes used in auth tests.
async fn protected_stub() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Test-helper: build a minimal router with the same middleware stack as the
// real bridge (auth + CSP).
// ---------------------------------------------------------------------------

fn test_router(api_key: &str) -> Router {
    let key = api_key.to_string();

    // Auth middleware — logic mirrors the closure inside BridgeServer::start()
    let auth = middleware::from_fn(move |req: Request<Body>, next: Next| {
        let key = key.clone();
        async move {
            // CORS preflight — always pass
            if req.method() == "OPTIONS" {
                return next.run(req).await;
            }

            // Public endpoints
            let path = req.uri().path();
            if path == "/health" || path == "/metrics" {
                return next.run(req).await;
            }

            // Validate x-api-key header
            if let Some(val) = req.headers().get(HeaderName::from_static("x-api-key")) {
                if let Ok(val) = val.to_str() {
                    if val == key {
                        return next.run(req).await;
                    }
                }
            }

            // Validate Authorization: Bearer <token>
            if let Some(val) = req.headers().get(axum::http::header::AUTHORIZATION) {
                if let Ok(val) = val.to_str() {
                    if let Some(token) = val.strip_prefix("Bearer ") {
                        if token == key {
                            return next.run(req).await;
                        }
                    }
                }
            }

            let body = Json(json!({"error": "Invalid or missing API key"}));
            (StatusCode::UNAUTHORIZED, body).into_response()
        }
    });

    // Security-headers middleware (sets CSP on every response)
    let security_headers = middleware::from_fn(|req: Request<Body>, next: Next| async move {
        let mut response = next.run(req).await;
        response.headers_mut().insert(
            HeaderName::from_static("content-security-policy"),
            CSP_VALUE.parse().unwrap(),
        );
        response
    });

    Router::new()
        .route("/health", get(health_stub))
        .route("/api/protected", get(protected_stub))
        .layer(security_headers)
        .layer(auth)
}

// ---------------------------------------------------------------------------
// Auth middleware tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_middleware_rejects_without_api_key() {
    let app = test_router("test-api-key-123");
    let response = app
        .oneshot(
            Request::get("/api/protected")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 when no API key is provided"
    );
}

#[tokio::test]
async fn test_auth_middleware_accepts_x_api_key_header() {
    let app = test_router("test-api-key-123");
    let response = app
        .oneshot(
            Request::get("/api/protected")
                .header(HeaderName::from_static("x-api-key"), "test-api-key-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "expected 200 when valid x-api-key is provided"
    );
}

#[tokio::test]
async fn test_auth_middleware_accepts_bearer_token() {
    let app = test_router("test-api-key-123");
    let response = app
        .oneshot(
            Request::get("/api/protected")
                .header(
                    axum::http::header::AUTHORIZATION,
                    "Bearer test-api-key-123",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "expected 200 when valid Bearer token is provided"
    );
}

#[tokio::test]
async fn test_auth_middleware_rejects_wrong_api_key() {
    let app = test_router("correct-key");
    let response = app
        .oneshot(
            Request::get("/api/protected")
                .header(HeaderName::from_static("x-api-key"), "wrong-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 when a wrong API key is provided"
    );
}

#[tokio::test]
async fn test_auth_middleware_allows_cors_preflight() {
    let app = test_router("test-api-key-123");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/protected")
                .method("OPTIONS")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Auth middleware passes OPTIONS through (CorsLayer handles it above auth).
    // The key assertion is that auth does NOT reject it with 401.
    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "CORS preflight (OPTIONS) should bypass auth middleware"
    );
}

// ---------------------------------------------------------------------------
// Health endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_endpoint_public_no_auth() {
    let app = test_router("test-api-key-123");
    let response = app
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "health endpoint is public and should return 200 without auth"
    );
}

// ---------------------------------------------------------------------------
// Security header tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_csp_header_present_on_responses() {
    let app = test_router("test-api-key-123");
    let response = app
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let csp = response
        .headers()
        .get(HeaderName::from_static("content-security-policy"));

    assert!(
        csp.is_some(),
        "Content-Security-Policy header must be present on bridge responses"
    );

    let csp_value = csp.unwrap().to_str().unwrap();
    assert!(
        csp_value.contains("default-src 'self'"),
        "CSP should restrict default-src to 'self', got: {}",
        csp_value
    );
}

// ---------------------------------------------------------------------------
// Filesystem tree depth and read preview
// ---------------------------------------------------------------------------

fn tree_names(tree: &serde_json::Value) -> Vec<&str> {
    tree.as_array()
        .unwrap()
        .iter()
        .map(|node| node["name"].as_str().unwrap())
        .collect()
}

fn tree_child<'a>(tree: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    tree.as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == name)
        .unwrap()
}

fn sample_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    std::fs::write(root.join("A.md"), "a").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(root.join("src/nested/deep.txt"), "deep").unwrap();
    dir
}

async fn tree_json(path: &std::path::Path, depth: Option<u32>) -> serde_json::Value {
    let query = super::FsPathQuery {
        path: Some(path.to_string_lossy().to_string()),
        depth,
        max_bytes: None,
    };
    let axum::Json(body) = super::fs_tree(axum::extract::Query(query)).await.unwrap();
    body["tree"].clone()
}

#[tokio::test]
async fn test_fs_tree_depth_one_lists_only_direct_entries() {
    let dir = sample_workspace();
    let tree = tree_json(dir.path(), Some(1)).await;

    // Directories first, then case-insensitive name order.
    assert_eq!(tree_names(&tree), vec![".git", "empty", "src", "A.md", "b.txt"]);
    // Directories at the last listed level are unloaded: no `children` key.
    for name in [".git", "empty", "src"] {
        let node = tree_child(&tree, name);
        assert_eq!(node["is_dir"], true);
        assert!(node.get("children").is_none(), "{name} should not be loaded at depth 1");
    }
    assert!(tree_child(&tree, "A.md").get("children").is_none());
    assert_eq!(tree_child(&tree, "b.txt")["size"], 1);
}

#[tokio::test]
async fn test_fs_tree_depth_two_loads_one_more_level() {
    let dir = sample_workspace();
    let tree = tree_json(dir.path(), Some(2)).await;

    let src = &tree_child(&tree, "src")["children"];
    assert_eq!(tree_names(src), vec!["nested", "main.rs"]);
    assert!(tree_child(src, "nested").get("children").is_none());
    // A loaded empty directory has an empty list, unlike an unloaded one.
    assert_eq!(tree_child(&tree, "empty")["children"], json!([]));
}

#[tokio::test]
async fn test_fs_tree_default_and_clamped_depth() {
    let dir = sample_workspace();
    // Default (no depth) keeps the eager 10-level listing.
    let tree = tree_json(dir.path(), None).await;
    let nested = tree_child(&tree_child(&tree, "src")["children"], "nested");
    assert_eq!(tree_names(&nested["children"]), vec!["deep.txt"]);
    // Out-of-range values are clamped to 1..=10.
    assert_eq!(tree_json(dir.path(), Some(0)).await, tree_json(dir.path(), Some(1)).await);
    assert_eq!(tree_json(dir.path(), Some(99)).await, tree);
}

#[cfg(unix)]
#[tokio::test]
async fn test_fs_tree_symlink_cycle_is_not_followed() {
    let dir = sample_workspace();
    std::os::unix::fs::symlink(dir.path(), dir.path().join("src/loop")).unwrap();
    // The walk terminates: entries report symlink metadata, so the link is listed
    // but never descended into, and the visited set guards any other re-entry.
    let tree = tree_json(dir.path(), None).await;
    let looped = tree_child(&tree_child(&tree, "src")["children"], "loop");
    assert_eq!(looped["is_dir"], false);
    assert!(looped.get("children").is_none());
}

#[test]
fn test_fs_read_preview_limits_and_detects_binary() {
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("text.txt");
    std::fs::write(&text, "line 1\r\nline 2\n").unwrap();
    let body = super::read_file_preview(&text.to_string_lossy(), 1024).unwrap();
    assert_eq!(body["content"], "line 1\r\nline 2\n");
    assert_eq!(body["size"], 15);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["binary"], false);

    let body = super::read_file_preview(&text.to_string_lossy(), 4).unwrap();
    assert_eq!(body["content"], "line");
    assert_eq!(body["truncated"], true);

    // A multi-byte character split at the limit is dropped, not reported as binary.
    let utf8 = dir.path().join("utf8.txt");
    std::fs::write(&utf8, "aé").unwrap();
    let body = super::read_file_preview(&utf8.to_string_lossy(), 2).unwrap();
    assert_eq!(body["content"], "a");
    assert_eq!(body["binary"], false);

    let binary = dir.path().join("image.bin");
    std::fs::write(&binary, [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
    let body = super::read_file_preview(&binary.to_string_lossy(), 1024).unwrap();
    assert_eq!(body["binary"], true);
    assert_eq!(body["content"], "");

    let latin1 = dir.path().join("latin1.txt");
    std::fs::write(&latin1, [b'c', b'a', b'f', 0xE9]).unwrap();
    assert_eq!(super::read_file_preview(&latin1.to_string_lossy(), 1024).unwrap()["binary"], true);

    let missing = dir.path().join("missing.txt");
    assert_eq!(
        super::read_file_preview(&missing.to_string_lossy(), 1024).unwrap_err(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        super::read_file_preview(&dir.path().to_string_lossy(), 1024).unwrap_err(),
        StatusCode::BAD_REQUEST
    );
}
