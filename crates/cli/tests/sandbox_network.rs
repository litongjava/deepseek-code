//! Sandboxed commands reach only allowed domains, through the host proxy and
//! the in-sandbox bridge (this crate's dcl binary).
use serde_json::json;

#[tokio::test]
async fn allowlisted_domains_pass_and_others_are_refused() {
    if !std::path::Path::new("/usr/bin/bwrap").exists() || !std::path::Path::new("/usr/bin/curl").exists() {
        return;
    }
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    std::env::set_var("DCL_SANDBOX_BRIDGE", env!("CARGO_BIN_EXE_dcl"));
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = server.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nhello-sandbox").await;
            });
        }
    });
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let settings = json!({"enabled": true, "network": {"allowedDomains": ["localhost"]}});
    let policy = deepseek_code_core::tools::sandbox::policy(&settings, workspace.path(), data.path(), &[]).unwrap();
    let run = |command: String| deepseek_code_core::tools::execute_tool_async("Bash", json!({"command": command, "__sandbox": policy.clone()}), workspace.path().to_str().unwrap());
    // NO_PROXY covers localhost inside the sandbox; force the proxy for the fixture host.
    let allowed = run(format!("curl -s --noproxy '' --max-time 10 http://localhost:{port}/")).await.unwrap();
    assert_eq!(allowed["stdout"].as_str().unwrap().trim(), "hello-sandbox", "{allowed}");
    let refused = run(format!("curl -s --noproxy '' --max-time 10 http://127.0.0.1:{port}/")).await.unwrap();
    assert!(refused["stdout"].as_str().unwrap().contains("blocked by the sandbox network allowlist"), "{refused}");
    assert!(refused["sandbox_network"].as_str().unwrap().contains("blocked network access to 127.0.0.1"), "{refused}");
    assert!(allowed.get("sandbox_network").is_none());
    // Bypassing the proxy finds no network at all.
    let direct = run(format!("curl -s --noproxy '*' --max-time 5 http://localhost:{port}/ || echo unreachable")).await.unwrap();
    assert!(direct["stdout"].as_str().unwrap().contains("unreachable"), "{direct}");
}
