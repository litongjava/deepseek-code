//! dcd-server behaviour through a real Bridge: the web UI is public, the API needs
//! the access token (header or login cookie), other origins are refused, and
//! peripheral requests work without the Desktop window.

use deepseek_desktop_tauri_lib::bridge::server_mode::ServeOptions;
use deepseek_desktop_tauri_lib::bridge::BridgeServer;
use deepseek_desktop_tauri_lib::runtime::RuntimePaths;
use serde_json::{json, Value};

const TOKEN: &str = "server-mode-test-token-0123456789";

#[tokio::test]
async fn server_mode_serves_the_ui_and_protects_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let web = dir.path().join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>shell</title>").unwrap();
    std::fs::write(web.join("assets/app.js"), "console.log(1)").unwrap();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let db = paths.open_database().unwrap();
    let bridge = BridgeServer::new(dir.path().join("data"), db);
    let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let mut options = ServeOptions::local(port);
    options.try_next_ports = false;
    options.access_token = Some(TOKEN.into());
    options.static_dir = Some(web);
    options.allowed_origins = vec!["tauri://localhost".into()];
    let server = tokio::spawn(async move { bridge.serve(options).await });

    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..100 {
        if client.get(format!("{base}/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "server did not start");

    // The shell and its assets are public; unknown routes fall back to index.html.
    let shell = client.get(format!("{base}/chat/123")).send().await.unwrap();
    assert_eq!(shell.status(), 200);
    assert!(shell.text().await.unwrap().contains("<title>shell</title>"));
    assert_eq!(client.get(format!("{base}/assets/app.js")).send().await.unwrap().status(), 200);

    // The API needs the token; Desktop's public reads are protected on a server.
    for path in ["/api/conversations", "/api/workspace-config", "/metrics"] {
        assert_eq!(client.get(format!("{base}{path}")).send().await.unwrap().status(), 401, "{path}");
    }
    let bearer = client.get(format!("{base}/api/conversations")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(bearer.status(), 200);

    // Login exchanges the token for an HttpOnly cookie; a wrong token is refused.
    let wrong = client.post(format!("{base}/api/auth/login")).json(&json!({"token": "wrong"})).send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    let login = client.post(format!("{base}/api/auth/login")).json(&json!({"token": TOKEN})).send().await.unwrap();
    assert_eq!(login.status(), 200);
    let cookie = login.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    assert!(login.headers()["set-cookie"].to_str().unwrap().contains("HttpOnly"));
    let status: Value = client.get(format!("{base}/api/auth/status")).header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
    assert_eq!(status, json!({"authenticated": true, "server_mode": true}));
    let with_cookie = client.get(format!("{base}/api/conversations")).header("cookie", &cookie).send().await.unwrap();
    assert_eq!(with_cookie.status(), 200);

    // Another site cannot use the cookie; the mobile app origin is allowed.
    let evil = client.get(format!("{base}/api/conversations")).header("cookie", &cookie)
        .header("origin", "https://evil.example").send().await.unwrap();
    assert_eq!(evil.status(), 403);
    let app = client.get(format!("{base}/api/conversations")).bearer_auth(TOKEN)
        .header("origin", "tauri://localhost").send().await.unwrap();
    assert_eq!(app.status(), 200);
    assert_eq!(app.headers()["access-control-allow-origin"], "tauri://localhost");

    // Peripheral services run through the Bridge with the server's own data directory.
    let templates = client.post(format!("{base}/api/peripheral")).bearer_auth(TOKEN)
        .json(&json!({"request": {"action": "project_templates"}})).send().await.unwrap();
    assert_eq!(templates.status(), 200);
    assert!(templates.json::<Value>().await.unwrap().as_array().is_some_and(|t| !t.is_empty()));
    let learning: Value = client.post(format!("{base}/api/peripheral")).bearer_auth(TOKEN)
        .json(&json!({"request": {"action": "learning_status"}})).send().await.unwrap().json().await.unwrap();
    assert_eq!(learning["enabled"], false);
    let bad = client.post(format!("{base}/api/peripheral")).bearer_auth(TOKEN)
        .json(&json!({"request": {"action": "no_such_action"}})).send().await.unwrap();
    assert!(bad.status().is_client_error());

    // The GitHub connector lives in the Bridge: its routes need the token, except
    // the OAuth callback, which only accepts a state this process issued.
    assert_eq!(client.get(format!("{base}/api/github/status")).send().await.unwrap().status(), 401);
    let github: Value = client.get(format!("{base}/api/github/status")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    assert_eq!(github["connected"], false);
    let callback = client.get(format!("{base}/api/github/callback?code=x&state=forged")).send().await.unwrap();
    assert_eq!(callback.status(), 400);
    assert!(callback.text().await.unwrap().contains("Unknown or expired OAuth state"));
    let repos = client.get(format!("{base}/api/github/repos")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(repos.status(), 401);

    server.abort();
}

#[tokio::test]
async fn network_binding_without_https_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let bridge = BridgeServer::new(dir.path().into(), paths.open_database().unwrap());
    let mut options = ServeOptions::local(0);
    options.host = "0.0.0.0".parse().unwrap();
    options.access_token = Some(TOKEN.into());
    let error = bridge.serve(options).await.unwrap_err().to_string();
    assert!(error.contains("requires HTTPS"), "{error}");
}
