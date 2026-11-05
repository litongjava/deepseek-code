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
