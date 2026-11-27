pub mod knowledge;
pub mod peripheral;
pub mod workspace_files;
/// Data directory shared with the CLI (`~/.deepseek-code`, or the legacy
/// directory while it still holds the only database). Resolved once so the
/// whole process keeps using one directory.
pub fn app_data_dir() -> Result<std::path::PathBuf, String> {
    static DIR: std::sync::OnceLock<Result<std::path::PathBuf, String>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        deepseek_code_core::runtime::RuntimePaths::default_data_dir().map_err(|e| e.to_string())
    })
    .clone()
}

use deepseek_code_core::db::session_store::{HostAccess,SessionAccess};
use deepseek_code_core::db::session_store::Schedules;
use crate::computer_use::{ComputerUseManager, ComputerAction, MouseButton};
use crate::db::session_store::Stores;
use crate::db::diff_repo::CodeDiffRow;
use crate::db::h5_repo::H5TokenRow;
use crate::db::permission_repo::{PermissionApprovalRow, AlwaysAllowRuleRow};
use crate::db::task_repo::{ScheduledTaskRow, TaskRunRow};
use crate::diff::{DiffResult, generate_diff as diff_generate, apply_diff_to_content};
use crate::im_integration::{
    ImIntegrationManager, ImPlatformConfig, ImConnectionInfo, ImConnectionStatusResult,
    ImMessageStatsResult, ImErrorLogInfo,
};
use crate::im_integration::permission_manager::{PermissionMode as ImPermissionMode, UserPermission};
use crate::mcp::{McpServerConfig, McpServerStatus, McpTool};
use crate::mcp::states as mcp_states;
use crate::mcp::states::McpState;
use crate::native_engine::engine_core::ChatRequest as NativeChatRequest;
use crate::native_engine::provider_manager::{Provider, ProviderManager};
use crate::native_engine::session_manager::{Attachment, Conversation, Message, ToolCallRecord};
use crate::permissions::{DangerousTool, PermissionMode};
use crate::slash_commands::{SlashCommand, SlashCommandRegistry};
use crate::cost_tracker::{CostSummary, CostTracker, SessionCost};

pub mod bridge;
pub mod general_settings;
pub mod feishu_chat;
pub use bridge::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::Mutex;

// Bug #16 fix: Cache SlashCommandRegistry to avoid recreating on every call
static SLASH_COMMAND_REGISTRY: std::sync::OnceLock<SlashCommandRegistry> = std::sync::OnceLock::new();

fn get_slash_command_registry() -> &'static SlashCommandRegistry {
    SLASH_COMMAND_REGISTRY.get_or_init(SlashCommandRegistry::new)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    pub platform: String,
    pub version: String,
}

#[tauri::command]
pub fn get_platform() -> String {
    std::env::consts::OS.to_string()
}

#[tauri::command]
pub async fn select_directory() -> Result<Option<String>, String> {
    
    // This is a placeholder - actual implementation would use the dialog plugin
    Ok(None)
}

#[tauri::command]
pub fn show_item_in_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .args(["/select,", &path])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .args(["-R", &path])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn open_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn open_external_url(url: String) -> Result<(), String> {
    open::that(&url).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resize_window(window: tauri::Window, width: f64, height: f64) -> Result<(), String> {
    window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn show_main_window(app_handle: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window("main") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn export_workspace() -> Result<String, String> {
    let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let workspace_dir = data_dir.join("data/workspaces");
    Ok(workspace_dir.to_string_lossy().to_string())
}

#[tauri::command]
pub fn get_system_status() -> SystemStatus {
    SystemStatus {
        platform: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[tauri::command]
pub async fn chat_send(
    _app_handle: tauri::AppHandle,
    _request: crate::native_engine::engine_core::ChatRequest,
) -> Result<serde_json::Value, String> {
    // Placeholder - actual implementation would use the bridge
    Err("chat_send not implemented in commands".to_string())
}

#[tauri::command]
pub async fn chat_stream(
    _app_handle: tauri::AppHandle,
    _request: crate::native_engine::engine_core::ChatRequest,
) -> Result<serde_json::Value, String> {
    // Placeholder - actual implementation would use the bridge
    Err("chat_stream not implemented in commands".to_string())
}

#[tauri::command]
pub async fn execute_tool(
    name: String,
    input: serde_json::Value,
    cwd: Option<String>,
) -> Result<serde_json::Value, String> {
    let cwd_str = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().to_string());
    match crate::tools::execute_tool(&name, input, &cwd_str) {
        Ok(result) => Ok(result),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn get_app_path() -> Result<String, String> {
    app_data_dir()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn check_update() -> Result<Option<crate::updater::UpdateInfo>, String> {
    let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let updater = crate::updater::AutoUpdater::configured(
        deepseek_code_core::data_layout::DataLayout::from_root(&data_dir).updates(),
    )
    .map_err(|e| e.to_string())?;
    updater.check_for_updates().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn install_update(_app_handle: tauri::AppHandle) -> Result<(), String> {
    Err("install_update not implemented".to_string())
}

#[tauri::command]
pub fn list_slash_commands() -> Vec<SlashCommand> {
    let registry = get_slash_command_registry();
    registry.list_commands().into_iter().cloned().collect()
}

#[tauri::command]
pub fn search_slash_commands(query: String) -> Vec<SlashCommand> {
    let registry = get_slash_command_registry();
    registry.search_commands(&query).into_iter().cloned().collect()
}

#[tauri::command]
pub fn get_slash_command_categories() -> Vec<String> {
    let registry = get_slash_command_registry();
    registry.get_categories()
}

#[tauri::command]
pub async fn get_cost_summary(
    app_handle: tauri::AppHandle,
    conversation_id: String,
) -> Result<CostSummary, String> {
    // Bug #18 fix: Try to use app-level CostTracker from state, fall back to creating one
    let tracker = match app_handle.try_state::<Arc<CostTracker>>() {
        Some(state) => state.inner().clone(),
        None => {
            let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            Arc::new(CostTracker::new(
                deepseek_code_core::data_layout::DataLayout::from_root(&data_dir).costs(),
            ))
        }
    };
    Ok(tracker.get_conversation_cost(&conversation_id).await)
}

#[tauri::command]
pub async fn get_all_session_costs(
    app_handle: tauri::AppHandle,
) -> Result<Vec<SessionCost>, String> {
    // Bug #18 fix: Try to use app-level CostTracker from state, fall back to creating one
    let tracker = match app_handle.try_state::<Arc<CostTracker>>() {
        Some(state) => state.inner().clone(),
        None => {
            let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            Arc::new(CostTracker::new(
                deepseek_code_core::data_layout::DataLayout::from_root(&data_dir).costs(),
            ))
        }
    };
    Ok(tracker.get_all_sessions().await)
}

#[tauri::command]
pub async fn native_engine_init(
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let engine_state = app_handle.state::<Arc<Mutex<Option<crate::native_engine::NativeEngine>>>>();
    let mut engine_guard = engine_state.lock().await;
    if engine_guard.is_none() {
        let data_dir = app_data_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let provider_manager = Arc::new(Mutex::new(ProviderManager::new(
            crate::data_layout::DataLayout::from_root(&data_dir).config_file("providers.json"),
        )));
        let db_manager = match app_handle.try_state::<Arc<Stores>>() {
            Some(state) => state.inner().clone(),
            None => return Err("Stores not found".to_string()),
        };
        let permission_manager = app_handle
            .state::<Arc<Mutex<Option<Arc<crate::permissions::PermissionManager>>>>>()
            .lock().await.clone()
            .ok_or_else(|| "PermissionManager not initialized".to_string())?;
        let engine = crate::runtime::create_engine(
            &crate::runtime::RuntimePaths { data_dir: data_dir.clone() },
            db_manager,
            provider_manager,
            permission_manager,
            Arc::new(crate::cache::FileCache::new()),
        )
        .with_resource_monitor(crate::start_resource_monitor(
            &data_dir,
            data_dir.clone(),
        ));
        *engine_guard = Some(engine);
    }
    Ok(())
}

#[tauri::command]
pub async fn native_chat(
    _app_handle: tauri::AppHandle,
    _request: NativeChatRequest,
) -> Result<serde_json::Value, String> {
    Err("native_chat not implemented in commands".to_string())
}

fn native_conversation(
    row: crate::db::conversation_repo::ConversationRow,
    workspace: std::path::PathBuf,
) -> Conversation {
    Conversation {
        id: row.id,
        title: row.title,
        model: row.model.unwrap_or_default(),
        workspace_path: workspace,
        created_at: row.created_at,
        updated_at: row.updated_at,
        project_id: row.project_id,
        research_mode: row.research_mode,
        claude_session_id: None,
        pending_resume_at: None,
    }
}

async fn native_history<R: tauri::Runtime, T: Send + 'static>(
    app: tauri::AppHandle<R>,
    operation: impl FnOnce(&Stores) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    let db = app.state::<Arc<Stores>>().inner().clone();
    tokio::task::spawn_blocking(move || operation(&db))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn native_create_conversation<R: tauri::Runtime>(
    app_handle: tauri::AppHandle<R>,
    model: String,
    title: Option<String>,
) -> Result<Conversation, String> {
    native_history(app_handle, move |db| {
        let id = uuid::Uuid::new_v4().to_string();
        let shard = db.create(
            &id,
            crate::db::session_store::CreateOptions {
                title,
                model: Some(model),
                ..Default::default()
            },
        )?;
        let row = db
            .conversation_metadata(&id)?
            .ok_or_else(|| anyhow::anyhow!("Conversation retired"))?;
        Ok(native_conversation(row, shard.workspace()?))
    })
    .await
}

#[tauri::command]
pub async fn native_list_conversations<R: tauri::Runtime>(
    app_handle: tauri::AppHandle<R>,
) -> Result<Vec<Conversation>, String> {
    native_history(app_handle, |db| {
        let root = db.data_dir().join("data/workspaces");
        Ok(db
            .global()
            .list_sessions(Default::default())?
            .into_iter()
            .map(|entry| {
                let private = root.join(&entry.workspace_incarnation);
                // Older syncs could persist the sender's path for a private child.
                let workspace = if entry.workspace_incarnation != entry.incarnation {
                    private
                } else {
                    entry.conversation.workspace_path.as_ref().map(std::path::PathBuf::from).unwrap_or(private)
                };
                native_conversation(entry.conversation, workspace)
            })
            .collect())
    })
    .await
}

#[tauri::command]
pub async fn native_delete_conversation<R: tauri::Runtime>(
    app_handle: tauri::AppHandle<R>,
    conversation_id: String,
) -> Result<(), String> {
    native_history(app_handle, move |db| {
        db.sync_delete(&conversation_id)?;
        deepseek_code_core::prompt::forget_session_files(db.data_dir(), &conversation_id);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn native_get_messages<R: tauri::Runtime>(
    app_handle: tauri::AppHandle<R>,
    conversation_id: String,
) -> Result<Vec<Message>, String> {
    native_history(app_handle, move |db| {
        if db.conversation_metadata(&conversation_id)?.is_none() {
            return Ok(Vec::new());
        }
        db.open_session(&conversation_id)?.read(|r| {
            r.messages()?
                .into_iter()
                .map(|m| {
                    let calls = r
                        .tools(&m.id)?
                        .into_iter()
                        .map(|call| ToolCallRecord {
                            id: call
                                .id
                                .strip_prefix(&format!("{}:", m.id))
                                .unwrap_or(&call.id)
                                .to_owned(),
                            name: call.name,
                            input: call
                                .input
                                .as_deref()
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or_else(|| serde_json::json!({})),
                            output: call.output,
                            is_error: Some(call.is_error),
                        })
                        .collect::<Vec<_>>();
                    let attachments = r
                        .attachments(&m.id)?
                        .into_iter()
                        .map(|a| {
                            Ok::<_, anyhow::Error>(Attachment {
                                file_id: a.id,
                                file_name: a.name.unwrap_or_default(),
                                file_type: a.kind.unwrap_or_default(),
                                mime_type: a.mime.unwrap_or_default(),
                                size: a.size.unwrap_or_default().try_into()?,
                                source: a.source,
                                gh_repo: None,
                                gh_ref: None,
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    Ok(Message {
                        id: m.id,
                        conversation_id: m.conversation_id,
                        role: m.role,
                        content: m.content,
                        created_at: m.created_at,
                        tool_calls: (!calls.is_empty()).then_some(calls),
                        is_compact_boundary: m.is_compact_boundary,
                        engine_uuid_synced: false,
                        attachments: (!attachments.is_empty()).then_some(attachments),
                        research: None,
                    })
                })
                .collect()
        })
    })
    .await
}

#[tauri::command]
pub async fn native_list_providers() -> Result<Vec<Provider>, String> {
    let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let provider_manager = ProviderManager::new(
        crate::data_layout::DataLayout::from_root(&data_dir).config_file("providers.json"),
    );
    Ok(provider_manager.list_providers().to_vec())
}

#[tauri::command]
pub async fn native_update_provider(provider: Provider) -> Result<(), String> {
    let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut provider_manager = ProviderManager::new(
        crate::data_layout::DataLayout::from_root(&data_dir).config_file("providers.json"),
    );
    provider_manager.update_provider(&provider.id.clone(), provider);
    Ok(())
}

#[tauri::command]
pub async fn native_delete_provider(provider_id: String) -> Result<(), String> {
    let data_dir = app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut provider_manager = ProviderManager::new(
        crate::data_layout::DataLayout::from_root(&data_dir).config_file("providers.json"),
    );
    provider_manager.delete_provider(&provider_id);
    Ok(())
}

#[tauri::command]
pub async fn mcp_list_servers(
    app_handle: tauri::AppHandle,
) -> Result<Vec<McpServerStatus>, String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        Ok(manager.list_servers().await)
    } else {
        Ok(Vec::new())
    }
}

#[tauri::command]
pub async fn mcp_start_server(
    app_handle: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        manager.start_server(&id).await.map_err(|e| e.to_string())
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn mcp_stop_server(
    app_handle: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        manager.stop_server(&id).await.map_err(|e| e.to_string())
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn mcp_restart_server(
    app_handle: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        manager.restart_server(&id).await.map_err(|e| e.to_string())
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn mcp_import_legacy(
    app_handle: tauri::AppHandle,
    source: String,
    preview: bool,
) -> Result<deepseek_code_core::mcp::transfer::Report, String> {
    let mcp_state =
        app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let guard = mcp_state.lock().await;
    let manager = guard.as_ref().ok_or("MCP manager not initialized")?;
    let manager = manager.lock().await;
    manager
        .import_legacy(std::path::Path::new(&source), preview)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mcp_add_server(
    app_handle: tauri::AppHandle,
    config: McpServerConfig,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        manager.add_server(config).await.map_err(|e| e.to_string())
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn mcp_update_server(
    app_handle: tauri::AppHandle,
    id: String,
    config: McpServerConfig,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        manager.update_server(&id, config).await.map_err(|e| e.to_string())
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn mcp_remove_server(
    app_handle: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        manager.remove_server(&id).await.map_err(|e| e.to_string())
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn mcp_toggle_server(
    app_handle: tauri::AppHandle,
    id: String,
    enabled: Option<bool>,
) -> Result<(), String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        if let Some(enabled_val) = enabled {
            manager.set_server_enabled(&id, enabled_val).await.map_err(|e| e.to_string())
        } else {
            manager.toggle_server(&id).await.map_err(|e| e.to_string())
        }
    } else {
        Err("MCP manager not initialized".to_string())
    }
}

/// The work one state request implies for a server. `off` disables the server
/// and stops it when it runs, but leaves `mcp-states.json` alone so the stored
/// state returns the next time the server is enabled. The other states are
/// stored; they also enable and start a disabled server.
#[derive(Debug, PartialEq, Eq)]
struct StatePlan {
    /// State to write; `None` for `off`.
    stored: Option<McpState>,
    /// New value of the `enabled` switch; `None` keeps it.
    enabled: Option<bool>,
    /// Whether to stop a running server.
    stop: bool,
    /// Whether to start the server after enabling it.
    start: bool,
}

fn plan_state(requested: McpState, enabled: bool) -> StatePlan {
    match requested {
        McpState::Off => StatePlan {
            stored: None,
            enabled: Some(false),
            stop: true,
            start: false,
        },
        state if enabled => StatePlan {
            stored: Some(state),
            enabled: None,
            stop: false,
            start: false,
        },
        state => StatePlan {
            stored: Some(state),
            enabled: Some(true),
            stop: false,
            start: true,
        },
    }
}

/// Every configured server's shown state, keyed by server id: `off` when the
/// server is disabled, otherwise its stored state (default `user`).
#[tauri::command]
pub async fn mcp_get_states(
    app_handle: tauri::AppHandle,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let data_dir = app_data_dir()?;
    let stored = mcp_states::load(&data_dir);
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    let Some(mcp_manager) = mcp_guard.as_ref() else {
        return Ok(std::collections::BTreeMap::new());
    };
    let manager = mcp_manager.lock().await;
    Ok(manager
        .list_servers()
        .await
        .into_iter()
        .map(|server| {
            let shown = mcp_states::shown(&stored, &server.id, server.enabled);
            (server.id, shown.label().to_owned())
        })
        .collect())
}

/// Set one server's state. `off` disables the server (and stops it) without
/// writing the file, so the stored state returns when it is enabled again;
/// the other states are stored and enable a disabled server.
#[tauri::command]
pub async fn mcp_set_state(
    app_handle: tauri::AppHandle,
    id: String,
    state: String,
) -> Result<String, String> {
    let requested = state.parse::<McpState>().map_err(|e| e.to_string())?;
    let data_dir = app_data_dir()?;
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    let mcp_manager = mcp_guard.as_ref().ok_or("MCP manager not initialized")?;
    let manager = mcp_manager.lock().await;
    let server = manager
        .get_server(&id)
        .await
        .ok_or_else(|| format!("Unknown MCP server {id}"))?;
    let plan = plan_state(requested, server.enabled);

    if let Some(stored) = plan.stored {
        mcp_states::set(&data_dir, &id, stored).map_err(|e| e.to_string())?;
    }
    if plan.stop && server.running {
        manager.stop_server(&id).await.map_err(|e| e.to_string())?;
    }
    if let Some(enabled) = plan.enabled {
        manager
            .set_server_enabled(&id, enabled)
            .await
            .map_err(|e| e.to_string())?;
    }
    if plan.start {
        manager.start_server(&id).await.map_err(|e| e.to_string())?;
    }

    let enabled = plan.enabled.unwrap_or(server.enabled);
    Ok(mcp_states::shown(&mcp_states::load(&data_dir), &id, enabled)
        .label()
        .to_owned())
}

#[tauri::command]
pub async fn mcp_list_tools(
    app_handle: tauri::AppHandle,
) -> Result<Vec<McpTool>, String> {
    let mcp_state = app_handle.state::<Arc<Mutex<Option<Arc<Mutex<crate::mcp::McpServerManager>>>>>>();
    let mcp_guard = mcp_state.lock().await;
    if let Some(mcp_manager) = mcp_guard.as_ref() {
        let manager = mcp_manager.lock().await;
        Ok(manager.get_all_tools().await)
    } else {
        Ok(Vec::new())
    }
}

#[tauri::command]
pub fn get_bridge_endpoint() -> Option<(u16, String)> {
    crate::bridge::local_endpoint()
}

#[tauri::command]
pub fn get_bridge_api_key(app_handle: tauri::AppHandle) -> Result<String, String> {
    let api_key_store = app_handle.state::<Arc<std::sync::Mutex<Option<String>>>>();
    let guard = api_key_store.lock().map_err(|e| e.to_string())?;
    guard.clone().ok_or_else(|| "API key not available".to_string())
}

#[tauri::command]
pub fn generate_diff(original: String, modified: String) -> DiffResult {
    diff_generate(&original, &modified)
}

#[tauri::command]
pub fn apply_diff(content: String, diff_text: String) -> Result<String, String> {
    apply_diff_to_content(&content, &diff_text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reject_diff(
    app_handle: tauri::AppHandle,
    conversation_id: Option<String>,
    diff_id: String,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.reject_diff(conversation_id.as_deref(), &diff_id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_code_diffs(
    app_handle: tauri::AppHandle,
    conversation_id: String,
) -> Result<Vec<CodeDiffRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.select_session(&conversation_id)?.diffs()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn generate_h5_token(
    app_handle: tauri::AppHandle,
    conversation_id: String,
    expires_in_hours: Option<i64>,
) -> Result<String, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let token = format!("h5_{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
    let hours = expires_in_hours.unwrap_or(24);
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(hours);
    let now = chrono::Utc::now().to_rfc3339();
    let expires_str = expires_at.to_rfc3339();
    let token_clone = token.clone();

    tokio::task::spawn_blocking(move || {
        db_manager.insert_h5_token(&id, &token_clone, &conversation_id, &expires_str, &now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    Ok(token)
}

#[tauri::command]
pub async fn revoke_h5_token(
    app_handle: tauri::AppHandle,
    token_id: String,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.revoke_h5_token(&token_id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_h5_tokens(
    app_handle: tauri::AppHandle,
    conversation_id: String,
) -> Result<Vec<H5TokenRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.list_h5_tokens_by_conversation(&conversation_id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn validate_h5_token(
    app_handle: tauri::AppHandle,
    token: String,
) -> Result<Option<H5TokenRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.validate_h5_token(&token)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cleanup_expired_h5_tokens(
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.cleanup_expired_tokens()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn create_scheduled_task(
    app_handle: tauri::AppHandle,
    name: String,
    description: Option<String>,
    cron_expression: String,
    task_type: String,
    task_config: String,
    conversation_id: Option<String>,
    is_enabled: bool,
) -> Result<ScheduledTaskRow, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let next_run = crate::task::cron::calc_next_run_local(&cron_expression, &chrono::Utc::now())
        .unwrap_or_default();
    let next_run_opt = if next_run.is_empty() { None } else { Some(next_run) };

    let id_clone = id.clone();
    let name_clone = name.clone();
    let description_clone = description.clone();
    let cron_clone = cron_expression.clone();
    let task_type_clone = task_type.clone();
    let task_config_clone = task_config.clone();
    let conversation_id_clone = conversation_id.clone();
    let next_run_opt_clone = next_run_opt.clone();
    let now_clone = now.clone();

    tokio::task::spawn_blocking(move || {
        db_manager.insert_scheduled_task(&id_clone, &name_clone, description_clone.as_deref(), &cron_clone,
                &task_type_clone, &task_config_clone, conversation_id_clone.as_deref(),
                is_enabled, next_run_opt_clone.as_deref(), &now_clone, &now_clone,
            )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    Ok(ScheduledTaskRow {
        id,
        name,
        description,
        cron_expression,
        task_type,
        task_config,
        conversation_id,
        is_enabled,
        last_run_at: None,
        last_run_status: None,
        last_run_output: None,
        next_run_at: next_run_opt,
        created_at: now.clone(),
        updated_at: now,
    })
}

#[tauri::command]
pub async fn list_scheduled_tasks(
    app_handle: tauri::AppHandle,
) -> Result<Vec<ScheduledTaskRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.list_scheduled_tasks()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_scheduled_task(
    app_handle: tauri::AppHandle,
    task_id: String,
    name: Option<String>,
    description: Option<String>,
    cron_expression: Option<String>,
    task_type: Option<String>,
    task_config: Option<String>,
    conversation_id: Option<String>,
    is_enabled: Option<bool>,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let now = chrono::Utc::now().to_rfc3339();
    let next_run_opt = if let Some(ref cron) = cron_expression {
        let next = crate::task::cron::calc_next_run_local(cron, &chrono::Utc::now()).unwrap_or_default();
        if next.is_empty() { None } else { Some(next) }
    } else {
        None
    };

    tokio::task::spawn_blocking(move || {
        db_manager.update_scheduled_task(&task_id,
                name.as_deref(), description.as_deref(),
                cron_expression.as_deref(), task_type.as_deref(),
                task_config.as_deref(), conversation_id.as_deref(),
                is_enabled, next_run_opt.as_deref(), &now,
            )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_scheduled_task(
    app_handle: tauri::AppHandle,
    task_id: String,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.delete_scheduled_task(&task_id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn execute_task_now(
    app_handle: tauri::AppHandle,
    task_id: String,
) -> Result<String, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let task = tokio::task::spawn_blocking({
        let db = db_manager.clone();
        move || {
            db.get_scheduled_task(&task_id)
        }
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if let Some(task) = task {
        let mut scheduler = crate::scheduler::ScheduledTaskScheduler::new(db_manager);
        if let Ok(data_dir) = app_data_dir() { scheduler = scheduler.with_data_dir(data_dir); }
        let result = scheduler.execute_task(&task).await;
        Ok(format!("Task executed: success={}, output={:?}, error={:?}",
            result.success, result.output, result.error))
    } else {
        Err("Task not found".to_string())
    }
}

#[tauri::command]
pub async fn get_task_runs(
    app_handle: tauri::AppHandle,
    task_id: String,
    limit: Option<usize>,
) -> Result<Vec<TaskRunRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let limit = limit.unwrap_or(50);
    tokio::task::spawn_blocking(move || {
        db_manager.get_task_runs(&task_id, limit)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn request_permission_approval(
    app_handle: tauri::AppHandle,
    conversation_id: String,
    message_id: String,
    tool_name: String,
    action: String,
    risk_level: String,
) -> Result<String, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let id_clone = id.clone();

    tokio::task::spawn_blocking(move || {
        db_manager.select_session(&conversation_id)?.request_approval(&id_clone, &message_id, &tool_name, &action, &risk_level, &now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    Ok(id)
}

#[tauri::command]
pub async fn approve_permission(
    app_handle: tauri::AppHandle,
    approval_id: String,
    conversation_id: Option<String>,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let now = chrono::Utc::now().to_rfc3339();

    tokio::task::spawn_blocking(move || {
        db_manager.decide_approval(conversation_id.as_deref(), &approval_id, "approved", None, &now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reject_permission(
    app_handle: tauri::AppHandle,
    approval_id: String,
    conversation_id: Option<String>,
    reason: Option<String>,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let now = chrono::Utc::now().to_rfc3339();

    tokio::task::spawn_blocking(move || {
        db_manager.decide_approval(conversation_id.as_deref(), &approval_id, "rejected", reason.as_deref(), &now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_pending_approvals(
    app_handle: tauri::AppHandle,
    conversation_id: String,
) -> Result<Vec<PermissionApprovalRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.select_session(&conversation_id)?.pending_approvals()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn always_allow_permission(
    app_handle: tauri::AppHandle,
    tool_name: String,
    action: String,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let rule_pattern = format!("{}:{}", tool_name, action);

    tokio::task::spawn_blocking(move || {
        db_manager.insert_always_allow_rule(&id, &rule_pattern, "combined", &now, &now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_dangerous_tools_list() -> Vec<DangerousTool> {
    crate::permissions::get_dangerous_tools()
}

#[tauri::command]
pub async fn im_connect_platform(
    app_handle: tauri::AppHandle,
    platform: String,
    config: ImPlatformConfig,
) -> Result<ImConnectionInfo, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.connect_platform(&platform, config).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_disconnect_platform(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<(), String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.disconnect_platform(&platform).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_list_connections(
    app_handle: tauri::AppHandle,
) -> Result<Vec<ImConnectionInfo>, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    Ok(im_manager.list_connections().await)
}

#[tauri::command]
pub async fn im_send_message(
    app_handle: tauri::AppHandle,
    platform: String,
    chat_id: String,
    message: String,
) -> Result<(), String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.send_message(&platform, &chat_id, &message).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_get_config(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<Option<ImConnectionInfo>, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    Ok(im_manager.get_connection(&platform).await)
}

#[tauri::command]
pub async fn im_update_config(
    app_handle: tauri::AppHandle,
    platform: String,
    config: ImPlatformConfig,
) -> Result<ImConnectionInfo, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.connect_platform(&platform, config).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_generate_qr_code(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<String, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.generate_qr_code_url(&platform).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_check_auth_status(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<(bool, String), String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.check_auth_status(&platform).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_get_connection_status(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<ImConnectionStatusResult, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.get_connection_status(&platform).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_get_message_stats(
    app_handle: tauri::AppHandle,
    platform: Option<String>,
) -> Result<ImMessageStatsResult, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    let platform_ref = platform.as_deref();
    im_manager.get_message_stats(platform_ref).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_set_permission_mode(
    app_handle: tauri::AppHandle,
    platform: String,
    mode: String,
) -> Result<(), String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    let permission_mode = ImPermissionMode::from_str(&mode)
        .ok_or_else(|| format!("Invalid permission mode: {}", mode))?;
    im_manager.set_permission_mode(&platform, permission_mode).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_get_permission_mode(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<String, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    let mode = im_manager.get_permission_mode(&platform).await;
    Ok(mode.as_str().to_string())
}

#[tauri::command]
pub async fn im_generate_pairing_code(
    app_handle: tauri::AppHandle,
    platform: String,
    user_id: String,
) -> Result<String, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.generate_pairing_code(&platform, &user_id).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_get_pending_pairing_requests(
    app_handle: tauri::AppHandle,
    platform: String,
) -> Result<Vec<UserPermission>, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.get_pending_pairing_requests(&platform).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_approve_pairing_request(
    app_handle: tauri::AppHandle,
    platform: String,
    user_id: String,
) -> Result<(), String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.approve_pairing_request(&platform, &user_id).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_reject_pairing_request(
    app_handle: tauri::AppHandle,
    platform: String,
    user_id: String,
) -> Result<(), String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    im_manager.reject_pairing_request(&platform, &user_id).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn im_get_error_logs(
    app_handle: tauri::AppHandle,
    platform: Option<String>,
) -> Result<Vec<ImErrorLogInfo>, String> {
    let im_manager = match app_handle.try_state::<Arc<ImIntegrationManager>>() {
        Some(state) => state.inner().clone(),
        None => return Err("ImIntegrationManager not found".to_string()),
    };
    let platform_ref = platform.as_deref();
    im_manager.get_error_logs(platform_ref).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn add_always_allow_rule(
    app_handle: tauri::AppHandle,
    rule_pattern: String,
    rule_type: String,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    tokio::task::spawn_blocking(move || {
        db_manager.insert_always_allow_rule(&id, &rule_pattern, &rule_type, &now, &now)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_always_allow_rule(
    app_handle: tauri::AppHandle,
    rule_id: String,
) -> Result<(), String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.delete_always_allow_rule(&rule_id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_always_allow_rules(
    app_handle: tauri::AppHandle,
) -> Result<Vec<AlwaysAllowRuleRow>, String> {
    let db_manager = match app_handle.try_state::<Arc<Stores>>() {
        Some(state) => state.inner().clone(),
        None => return Err("Stores not found".to_string()),
    };
    tokio::task::spawn_blocking(move || {
        db_manager.get_always_allow_rules()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_permission_mode(app_handle: tauri::AppHandle) -> Result<String, String> {
    let state = app_handle.state::<Arc<Mutex<Option<Arc<crate::permissions::PermissionManager>>>>>();
    let guard = state.lock().await;
    guard.as_ref().map(|manager| manager.get_mode().as_str().to_string())
        .ok_or_else(|| "PermissionManager not initialized".to_string())
}

#[tauri::command]
pub async fn set_permission_mode(
    app_handle: tauri::AppHandle,
    mode: String,
) -> Result<(), String> {
    let permission_state = app_handle.state::<Arc<Mutex<Option<Arc<crate::permissions::PermissionManager>>>>>();
    let permission_guard = permission_state.lock().await;
    if let Some(perm_mgr) = permission_guard.as_ref() {
        let mode_enum = PermissionMode::parse(&mode).map_err(|error| error.to_string())?;
        if mode_enum == PermissionMode::BypassPermissions && !perm_mgr.bypass_available() {
            return Err("Bypass is unavailable for this execution host".into());
        }
        perm_mgr.set_mode(mode_enum);
        Ok(())
    } else {
        Err("PermissionManager not initialized".to_string())
    }
}

#[tauri::command]
pub async fn computer_use_screenshot() -> Result<String, String> {
    crate::computer_use::take_screenshot_powershell()
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn computer_use_mouse_click(
    x: i32,
    y: i32,
    button: Option<String>,
) -> Result<serde_json::Value, String> {
    let config = crate::computer_use::ComputerUseConfig::default();
    let manager = ComputerUseManager::new(config);
    let mouse_button = match button.as_deref() {
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        _ => MouseButton::Left,
    };
    let action = crate::computer_use::action_helpers::mouse_click(x, y, mouse_button);
    let result = manager.execute_action(action).await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "success": result.success,
        "screenshot": result.screenshot,
        "error": result.error,
    }))
}

#[tauri::command]
pub async fn computer_use_keyboard_type(
    text: String,
) -> Result<serde_json::Value, String> {
    let config = crate::computer_use::ComputerUseConfig::default();
    let manager = ComputerUseManager::new(config);
    let action = crate::computer_use::action_helpers::type_text(&text);
    let result = manager.execute_action(action).await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "success": result.success,
        "screenshot": result.screenshot,
        "error": result.error,
    }))
}

#[tauri::command]
pub async fn computer_use_keyboard_key(
    key: String,
) -> Result<serde_json::Value, String> {
    let config = crate::computer_use::ComputerUseConfig::default();
    let manager = ComputerUseManager::new(config);
    let action = crate::computer_use::action_helpers::key_press(&key);
    let result = manager.execute_action(action).await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "success": result.success,
        "screenshot": result.screenshot,
        "error": result.error,
    }))
}

#[tauri::command]
pub async fn computer_use_mouse_scroll(
    scroll_y: i32,
    scroll_x: Option<i32>,
) -> Result<serde_json::Value, String> {
    let config = crate::computer_use::ComputerUseConfig::default();
    let manager = ComputerUseManager::new(config);
    let action = ComputerAction {
        action_type: crate::computer_use::ComputerActionType::MouseScroll,
        coordinate: None,
        button: None,
        key: None,
        text: None,
        scroll_y: Some(scroll_y),
        scroll_x,
        duration_ms: None,
    };
    let result = manager.execute_action(action).await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "success": result.success,
        "screenshot": result.screenshot,
        "error": result.error,
    }))
}

#[tauri::command]
pub async fn computer_use_get_screen_info() -> Result<serde_json::Value, String> {
    let config = crate::computer_use::ComputerUseConfig::default();
    let manager = ComputerUseManager::new(config);
    let info = manager.get_screen_info();
    Ok(serde_json::json!({
        "width": info.width,
        "height": info.height,
        "scale_factor": info.scale_factor,
    }))
}

#[tauri::command]
pub async fn app_studio_generate_project(
    spec: crate::app_studio::AppProjectSpec,
) -> Result<crate::app_studio::ProjectStructure, String> {
    let data_dir = deepseek_code_core::data_layout::DataLayout::from_root(
        &app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("deepseek-code")),
    )
    .app_studio();
    let studio = crate::app_studio::AppStudio::new(data_dir);
    studio.generate_project(&spec)
}

#[tauri::command]
pub async fn get_context_size(conversation_id: String) -> Result<serde_json::Value, String> {
    let (port, api_key) = crate::bridge::local_endpoint().ok_or_else(|| "Bridge error: server is not running".to_string())?;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/api/conversations/{}/context-size", port, urlencoding::encode(&conversation_id));
    let resp = client.get(&url).header("x-api-key", api_key).send().await.map_err(|e| format!("Bridge error: {}", e))?;
    let status = resp.status();
    let data: serde_json::Value = resp.json().await.map_err(|e| format!("Parse error: {}", e))?;
    if !status.is_success() {
        return Err(data["error"].as_str().map(String::from).unwrap_or_else(|| format!("Bridge error: {}", status)));
    }
    Ok(data)
}

#[cfg(test)]
mod mcp_state_tests {
    use super::*;

    #[test]
    fn mcp_state_labels_parse_and_unknown_labels_fail() {
        for (label, state) in [
            ("off", McpState::Off),
            ("user", McpState::User),
            ("on-demand", McpState::OnDemand),
            ("user-only", McpState::UserOnly),
        ] {
            assert_eq!(label.parse::<McpState>().unwrap(), state);
        }
        let error = "always".parse::<McpState>().unwrap_err().to_string();
        for label in ["off", "user", "on-demand", "user-only"] {
            assert!(error.contains(label), "{error}");
        }
    }

    #[test]
    fn mcp_state_off_disables_and_stops_without_storing() {
        let expected = StatePlan {
            stored: None,
            enabled: Some(false),
            stop: true,
            start: false,
        };
        assert_eq!(plan_state(McpState::Off, true), expected);
        assert_eq!(plan_state(McpState::Off, false), expected);
    }

    #[test]
    fn mcp_state_stored_states_leave_an_enabled_server_alone() {
        for state in [McpState::User, McpState::OnDemand, McpState::UserOnly] {
            assert_eq!(
                plan_state(state, true),
                StatePlan {
                    stored: Some(state),
                    enabled: None,
                    stop: false,
                    start: false,
                }
            );
        }
    }

    #[test]
    fn mcp_state_stored_states_enable_and_start_a_disabled_server() {
        for state in [McpState::User, McpState::OnDemand, McpState::UserOnly] {
            assert_eq!(
                plan_state(state, false),
                StatePlan {
                    stored: Some(state),
                    enabled: Some(true),
                    stop: false,
                    start: true,
                }
            );
        }
    }
}
