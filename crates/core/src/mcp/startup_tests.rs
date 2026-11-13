use super::*;

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
