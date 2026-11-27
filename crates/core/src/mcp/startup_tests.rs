use super::*;

#[test]
fn empty_or_unrelated_config_means_no_servers() {
    assert!(McpServerManager::parse_config(b"{}").unwrap().is_empty());
    assert!(McpServerManager::parse_config(br#"{"other":1}"#)
        .unwrap()
        .is_empty());
    assert!(McpServerManager::parse_config(br#"{"mcpServers":null}"#)
        .unwrap()
        .is_empty());
    assert!(McpServerManager::parse_config(br#"{"mcpServers":[]}"#)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn hot_reload_rejects_a_non_object_mcp_servers_and_keeps_the_last_config() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp-servers.json");
    std::fs::write(
        &path,
        br#"{"mcpServers":{"kept":{"command":"missing","enabled":false}}}"#,
    )
    .unwrap();
    let manager = McpServerManager::new(path.clone());
    manager.load_config().await.unwrap();

    std::fs::write(&path, br#"{"mcpServers":[]}"#).unwrap();
    assert!(manager.reconcile_config().await.is_err());
    assert!(manager.get_server("kept").await.is_some());
}

#[tokio::test]
async fn live_reconcile_preserves_processes_and_drains_calls() {
    use serde_json::{json, Value};
    use tokio::time::{sleep, timeout, Duration};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp-servers.json");
    let gate = root.path().join("gate");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live-mcp.cjs");
    let config = |rev, enabled| {
        json!({"command":"node", "args":[fixture], "enabled":enabled,
        "env":{"REVISION":rev, "GATE":gate}})
    };
    let save =
        |servers: Value| std::fs::write(&path, json!({"mcpServers":servers}).to_string()).unwrap();
    save(json!({}));
    let manager = Arc::new(McpServerManager::new(path.clone()));
    manager.load_config().await.unwrap();
    let pid = |server| manager.call_tool(server, "probe", json!({}));
    let stable = config("stable", true);
    save(json!({"stable":stable, "live":config("first",true)}));
    assert_eq!(manager.reconcile_config().await.unwrap().len(), 2);
    let stable_pid = pid("stable").await.unwrap();
    let live_pid = pid("live").await.unwrap();
    std::fs::write(&path, "{").unwrap();
    assert!(manager.reconcile_config().await.is_err());
    assert_eq!(pid("live").await.unwrap(), live_pid);
    save(json!({"stable":stable, "live":config("second",true)}));
    assert_eq!(
        manager.reconcile_config().await.unwrap(),
        [("live".into(), "restarted".into())]
    );
    assert_ne!(pid("live").await.unwrap(), live_pid);
    save(json!({"stable":stable, "live":config("second",false)}));
    manager.reconcile_config().await.unwrap();
    assert!(pid("live").await.is_err());
    save(json!({"stable":stable, "live":config("second",true)}));
    manager.reconcile_config().await.unwrap();
    let enabled_pid = pid("live").await.unwrap();
    let active = manager.clone();
    let call = tokio::spawn(async move {
        active
            .call_tool("live", "probe", json!({"wait":true}))
            .await
    });
    timeout(Duration::from_secs(5), async {
        while !gate.with_extension("waiting").exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    save(json!({"stable":stable}));
    let reload = manager.reconcile_config();
    tokio::pin!(reload);
    assert!(timeout(Duration::from_millis(100), &mut reload)
        .await
        .is_err());
    std::fs::write(gate, "release").unwrap();
    assert_eq!(call.await.unwrap().unwrap(), enabled_pid);
    timeout(Duration::from_secs(5), reload)
        .await
        .unwrap()
        .unwrap();
    assert!(manager.get_server("live").await.is_none());
    assert_eq!(pid("stable").await.unwrap(), stable_pid);
    manager.shutdown_all().await.unwrap();
}

#[tokio::test]
async fn fresh_hosts_reconcile_default_creation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp-servers.json");
    let first = McpServerManager::new(path.clone());
    let second = McpServerManager::new(path.clone());
    assert!(!path.exists());

    // Both hosts have selected the missing-file branch. Force that interleaving
    // without relying on a scheduler to race initialize's existence checks.
    let (a, b) = tokio::join!(
        first.create_default_config(),
        second.create_default_config()
    );
    a.unwrap();
    b.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(*first.revision.lock().unwrap(), Some(bytes.clone()));
    assert_eq!(*second.revision.lock().unwrap(), Some(bytes));
    assert_eq!(first.list_servers().await.len(), default_servers().len());
    assert_eq!(second.list_servers().await.len(), default_servers().len());

    first.set_server_enabled("filesystem", true).await.unwrap();
    let edited = std::fs::read(&path).unwrap();
    let error = second.set_server_enabled("git", true).await.unwrap_err();
    assert!(error.to_string().contains("changed in another process"));
    assert_eq!(std::fs::read(&path).unwrap(), edited);
    assert!(!second.get_server("git").await.unwrap().enabled);
}

#[tokio::test]
async fn late_default_creation_adopts_user_configuration_without_merging_presets() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp-servers.json");
    let late = McpServerManager::new(path.clone());
    assert!(!path.exists());
    let bytes = br#"{"mcpServers":{"custom":{"command":"fixture","enabled":false}}}"#;
    std::fs::write(&path, bytes).unwrap();

    late.create_default_config().await.unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(late.list_servers().await.len(), 1);
    assert_eq!(late.get_server("custom").await.unwrap().command, "fixture");
    assert_eq!(*late.revision.lock().unwrap(), Some(bytes.to_vec()));
    late.set_server_enabled("custom", true).await.unwrap();
}

#[tokio::test]
async fn late_default_creation_does_not_hide_invalid_configuration() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp-servers.json");
    let late = McpServerManager::new(path.clone());
    std::fs::write(&path, b"invalid json").unwrap();

    assert!(late.create_default_config().await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid json");
    assert!(late.list_servers().await.is_empty());
}

#[tokio::test]
async fn a_failed_start_is_kept_in_the_server_status() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpServerManager::new(root.path().join("mcp-servers.json"));
    manager.initialize().await.unwrap();
    let missing = root.path().join("missing-mcp-binary");
    manager
        .add_server(McpServerConfig {
            id: "brokensrv".into(),
            name: "brokensrv".into(),
            command: missing.to_string_lossy().into(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            enabled: true,
            ..Default::default()
        })
        .await
        .unwrap();

    let error = manager.start_server("brokensrv").await.unwrap_err();
    let server = manager.get_server("brokensrv").await.unwrap();
    assert!(!server.running);
    let kept = server.error.expect("the start failure is kept");
    assert!(
        kept.contains("No such file or directory") || kept.contains("os error 2"),
        "kept: {kept}"
    );
    assert_eq!(kept, format!("{error:#}"));

    // Repairing the same server keeps the old failure until a start succeeds, so
    // the clearing below proves the success path clears it, not a fresh entry.
    let script = root.path().join("mcp.cjs");
    std::fs::write(
        &script,
        r#"require('readline').createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result:r.method==='tools/list'?{tools:[]}:r.method==='resources/list'?{resources:[]}:{protocolVersion:'2024-11-05',capabilities:{},serverInfo:{name:'fixture',version:'1'}}})+'\n');});"#,
    )
    .unwrap();
    manager
        .update_server(
            "brokensrv",
            McpServerConfig {
                id: "brokensrv".into(),
                name: "brokensrv".into(),
                command: "node".into(),
                args: vec![script.to_string_lossy().into()],
                env: Default::default(),
                cwd: None,
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        manager.get_server("brokensrv").await.unwrap().error,
        Some(kept.clone()),
        "updating the config must not hide the old failure"
    );
    manager.start_server("brokensrv").await.unwrap();
    let server = manager.get_server("brokensrv").await.unwrap();
    assert!(server.running);
    assert_eq!(server.error, None);
    manager.shutdown_all().await.unwrap();
}

#[tokio::test]
async fn plugin_reconcile_failure_is_shared_until_an_explicit_start_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mcp-servers.json");
    let cwd = root.path().join("temporarily-unavailable");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live-mcp.cjs");
    let id = "plugin:fixture:probe";
    let manager = McpServerManager::new(path.clone());
    std::fs::write(&path, "{}").unwrap();
    manager.load_config().await.unwrap();
    let config = serde_json::json!({"mcpServers":{(id):{
        "command":"node", "args":[fixture], "cwd":cwd, "enabled":true
    }}});
    std::fs::write(path, config.to_string()).unwrap();

    let changes = manager.reconcile_config().await.unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0, id);
    assert!(changes[0].1.starts_with("failed to start"));
    std::fs::create_dir(&cwd).unwrap();
    assert!(!manager.start_automatically(id).await.unwrap());
    assert!(!manager.get_server(id).await.unwrap().running);

    manager.start_server(id).await.unwrap();
    assert!(manager.get_server(id).await.unwrap().error.is_none());
    manager.stop_server(id).await.unwrap();
    assert!(manager.start_automatically(id).await.unwrap());
    // A repair also unlocks automatic startup, while doctor continues to
    // show the previous error until the replacement actually starts.
    let good = manager.list_servers_full().await.pop().unwrap().config;
    let mut broken = good.clone();
    broken.cwd = Some(root.path().join("missing-again"));
    manager.update_server(id, broken).await.unwrap();
    assert!(manager.start_automatically(id).await.is_err());
    manager.update_server(id, good).await.unwrap();
    assert!(manager.get_server(id).await.unwrap().error.is_some());
    assert!(manager.start_automatically(id).await.unwrap());
    assert!(manager.get_server(id).await.unwrap().error.is_none());
    manager.shutdown_all().await.unwrap();
}
