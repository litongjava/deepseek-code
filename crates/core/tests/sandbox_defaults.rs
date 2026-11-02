use deepseek_code_core::{sandbox::SandboxConfig, tools::sandbox};
use serde_json::json;

#[test]
fn sandbox_network_defaults_allow_requests_and_preserve_explicit_denial() {
    assert!(SandboxConfig::default().allow_network);
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let policy = sandbox::policy(
        &json!({"enabled": true}),
        workspace.path(),
        data.path(),
        &[],
    )
    .unwrap();
    assert_eq!(policy["network"], true);
    assert!(policy["network_rules"].is_null());
    let denied = sandbox::policy(
        &json!({"enabled": true, "network": "deny"}),
        workspace.path(),
        data.path(),
        &[],
    )
    .unwrap();
    assert_eq!(denied["network"], false);
}

#[cfg(target_os = "macos")]
#[test]
fn default_bash_sandbox_can_reach_a_local_http_server() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0; 4096];
                stream.read(&mut buffer).unwrap();
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .unwrap();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("sandbox did not connect to the HTTP fixture");
    });
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let policy = sandbox::policy(
        &json!({"enabled": true}),
        workspace.path(),
        data.path(),
        &[],
    )
    .unwrap();
    let (program, args) = sandbox::wrap(&policy, "/bin/sh").unwrap();
    let output = std::process::Command::new(program)
        .args(args)
        .args([
            "-c",
            &format!("/usr/bin/curl --noproxy '*' -fsS --max-time 5 http://{address}/"),
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok");
}
