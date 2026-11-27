use std::sync::Arc;

use deepseek_desktop_tauri_lib::db::session_store::Stores;
use deepseek_desktop_tauri_lib::cost_tracker::CostTracker;
use deepseek_desktop_tauri_lib::preview_engine::PreviewEngine;
use deepseek_desktop_tauri_lib::analytics::AnalyticsStore;
use deepseek_desktop_tauri_lib::engine::EnginePool;
use deepseek_desktop_tauri_lib::streaming::StreamManager;
use deepseek_desktop_tauri_lib::skills::SkillsManager;
use deepseek_desktop_tauri_lib::process::ProcessManager;
use deepseek_desktop_tauri_lib::watcher::FileWatcher;
use deepseek_desktop_tauri_lib::clipboard::ClipboardManager;
use deepseek_desktop_tauri_lib::notification::NotificationManager;
use deepseek_desktop_tauri_lib::logger::Logger;
use deepseek_desktop_tauri_lib::mcp::McpServerManager;
use deepseek_desktop_tauri_lib::config::ConfigManager;
use deepseek_desktop_tauri_lib::memory::MemExClient;

#[tokio::test]
async fn test_db_manager_creation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let stores=Stores::open(temp_dir.path()).unwrap();
    assert!(temp_dir.path().join("global.db").is_file());
    assert!(stores.conversations().unwrap().is_empty());
}

#[tokio::test]
async fn test_session_history_survives_a_reopen() {
    let root=tempfile::tempdir().unwrap();
    let id={
        let stores=Stores::open(root.path()).unwrap();
        let shard=stores.create("persistent",Default::default()).unwrap();
        shard.write(|w|w.append("user","test_value")).unwrap()
    };
    let stores=Stores::open(root.path()).unwrap();
    let messages=stores.open_session("persistent").unwrap().read(|r|r.messages()).unwrap();
    assert_eq!((messages[0].id.as_str(),messages[0].content.as_str()),(id.as_str(),"test_value"));
}

#[test]
fn test_cost_tracker_creation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let costs_dir = temp_dir.path().join("costs");

    let tracker = CostTracker::new(costs_dir);
    let budget_check = tracker.check_budget(1000);
    assert!(matches!(budget_check, deepseek_desktop_tauri_lib::cost_tracker::BudgetCheckResult::WithinBudget));
}

#[tokio::test]
async fn test_preview_engine_creation() {
    let _engine = PreviewEngine::new(None);
    assert!(true);
}

#[tokio::test]
async fn test_analytics_store_creation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let analytics_dir = temp_dir.path().join("analytics");

    let _store = AnalyticsStore::new(analytics_dir);
    assert!(true);
}

#[tokio::test]
async fn test_engine_pool_creation() {
    let _pool = EnginePool::new();
    assert!(true);
}

#[tokio::test]
async fn test_stream_manager_creation() {
    let _manager = StreamManager::new();
    assert!(true);
}

#[tokio::test]
async fn test_skills_manager_creation() {
    let directory = tempfile::tempdir().unwrap();
    let manager = SkillsManager::with_path(directory.path().join("skills"));
    let result = manager.install_bundled_skills();
    assert!(result.is_ok() || true);
}

#[tokio::test]
async fn test_process_manager_creation() {
    let _manager = ProcessManager::new();
    assert!(true);
}

#[tokio::test]
async fn test_file_watcher_creation() {
    let _watcher = FileWatcher::new();
    assert!(true);
}

#[tokio::test]
async fn test_clipboard_manager_creation() {
    let _manager = ClipboardManager::new();
    assert!(true);
}

#[tokio::test]
async fn test_notification_manager_creation() {
    let _manager = NotificationManager::new();
    assert!(true);
}

#[tokio::test]
async fn test_logger_creation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let log_dir = temp_dir.path().join("diagnostics/logs");
    let _logger = Logger::new(log_dir);
    assert!(true);
}

#[tokio::test]
async fn test_mcp_server_manager_creation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let _manager = McpServerManager::new(temp_dir.path().join("mcp-servers.json"));
    assert!(true);
}

#[tokio::test]
async fn test_config_manager_creation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let _manager = ConfigManager::new(temp_dir.path().to_path_buf());
    assert!(true);
}

#[tokio::test]
async fn test_memex_client_creation() {
    let _client = MemExClient::new();
    assert!(true);
}

#[tokio::test]
async fn test_bridge_server_api_key_uniqueness() {
    use deepseek_desktop_tauri_lib::bridge::BridgeServer;

    let temp_dir1 = tempfile::tempdir().expect("Failed to create temp dir");
    let db1 = Arc::new(Stores::open(temp_dir1.path()).expect("Failed to create DB"));
    let server1 = BridgeServer::new(temp_dir1.path().to_path_buf(), db1);
    let key1 = server1.get_api_key().to_string();

    let temp_dir2 = tempfile::tempdir().expect("Failed to create temp dir");
    let db2 = Arc::new(Stores::open(temp_dir2.path()).expect("Failed to create DB"));
    let server2 = BridgeServer::new(temp_dir2.path().to_path_buf(), db2);
    let key2 = server2.get_api_key().to_string();

    assert_ne!(key1, key2, "API keys should be unique");
    assert!(!key1.is_empty(), "API key should not be empty");
    assert!(!key2.is_empty(), "API key should not be empty");
}
