use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::Deserialize;
use serde_json;
use std::collections::HashMap;

use super::super::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncProviderPayload {
    id: String,
    name: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    models: Vec<serde_json::Value>,
    #[serde(default)]
    enabled: bool,
}

use deepseek_code_core::db::{message_repo::MessageRow, session_store::SyncHeader};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncPushRequest {
    #[serde(default)]
    #[allow(dead_code)]
    device_id: String,
    #[serde(default)]
    providers: Vec<SyncProviderPayload>,
    #[serde(default)]
    conversations: Vec<SyncHeader>,
    #[serde(default)]
    messages_per_conversation: HashMap<String, Vec<MessageRow>>,
    #[serde(default)]
    deleted_provider_ids: Vec<String>,
    #[serde(default)]
    deleted_conversation_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SyncPullRequest {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    last_pull_at: Option<String>,
}

async fn sync_push(
    State(state): State<AppState>,
    Json(req): Json<SyncPushRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!(
        "Sync Push: {} providers, {} conversations, {} message buckets, {} deleted providers, {} deleted conversations",
        req.providers.len(),
        req.conversations.len(),
        req.messages_per_conversation.len(),
        req.deleted_provider_ids.len(),
        req.deleted_conversation_ids.len()
    );

    if !req.providers.is_empty() {
        let config_manager = state.config_manager.clone();
        let mut manager = config_manager.lock().await;
        if let Some(m) = manager.as_mut() {
            let _ = m.update_config(|config| {
                for sync_p in &req.providers {
                    let provider_type = if sync_p.format == "anthropic" {
                        "anthropic".to_string()
                    } else {
                        "openai".to_string()
                    };
                    let models: Vec<crate::config::ModelConfig> = sync_p
                        .models
                        .iter()
                        .map(|m| crate::config::ModelConfig {
                            id: m
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            name: m
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            enabled: m.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                            max_tokens: m
                                .get("maxTokens")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as u32),
                            supports_vision: true,
                            supports_text: true,
                            api: None,
                            supports_tools: true,
                            supports_streaming: true,
                            context_window: None,
                            cost_per_1k_input: None,
                            cost_per_1k_output: None,
                        })
                        .collect();

                    let existing = config.providers.iter().position(|p| p.id == sync_p.id);
                    let merged = crate::config::ProviderConfig {
                        id: sync_p.id.clone(),
                        name: sync_p.name.clone(),
                        base_url: if sync_p.base_url.is_empty() {
                            String::from("https://api.openai.com/v1")
                        } else {
                            sync_p.base_url.clone()
                        },
                        api_key: Some(sync_p.api_key.clone()),
                        provider_type,
                        models,
                        enabled: sync_p.enabled,
                        is_default: false,
                        settings: Default::default(),
                        supports_web_search: false,
                        web_search_strategy: None,
                        web_search_tested_at: None,
                        web_search_test_reason: None,
                    };
                    if let Some(idx) = existing {
                        config.providers[idx] = merged;
                    } else {
                        config.providers.push(merged);
                    }
                    tracing::info!("Sync Upserted provider: {} ({})", sync_p.name, sync_p.id);
                }
            });
        }
    }

    for deleted_id in &req.deleted_provider_ids {
        let config_manager = state.config_manager.clone();
        let mut manager = config_manager.lock().await;
        if let Some(m) = manager.as_mut() {
            let _ = m.update_config(|config| {
                config.providers.retain(|p| p.id != *deleted_id);
            });
            tracing::info!("Sync Deleted provider: {}", deleted_id);
        }
    }

    let db = state.db_manager.clone();
    let mut buckets = req.messages_per_conversation;
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        for header in req.conversations {
            let messages = buckets.remove(&header.id).unwrap_or_default();
            db.sync_conversation(&header.id, Some(&header), &messages)?;
        }
        for (id, messages) in buckets {
            db.sync_conversation(&id, None, &messages)?;
        }
        for id in req.deleted_conversation_ids {
            db.sync_delete(&id)?;
            deepseek_code_core::prompt::forget_session_files(state.data_dir.as_ref(), &id);
        }
        Ok(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|error| {
        tracing::error!(%error, "Sync push failed");
        StatusCode::CONFLICT
    })?;

    let server_timestamp = chrono::Utc::now().to_rfc3339();
    tracing::info!("Sync Push complete at {}", server_timestamp);

    Ok(Json(serde_json::json!({
        "ok": true,
        "serverTimestamp": server_timestamp,
        "deletedProviderIds": [],
        "deletedConversationIds": [],
    })))
}

async fn sync_pull(
    State(state): State<AppState>,
    Json(req): Json<SyncPullRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!(
        "Sync Pull: device={}, lastPullAt={:?}",
        req.device_id,
        req.last_pull_at
    );

    let config_manager = state.config_manager.clone();
    let manager = config_manager.lock().await;
    let providers: Vec<serde_json::Value> = if let Some(m) = manager.as_ref() {
        let config = m.get_config();
        config
            .providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "apiKey": p.api_key,
                    "baseUrl": p.base_url,
                    "format": p.provider_type,
                    "models": p.models.iter().map(|m| serde_json::json!({
                        "id": m.id,
                        "name": m.name,
                        "enabled": m.enabled,
                    })).collect::<Vec<_>>(),
                    "enabled": p.enabled,
                })
            })
            .collect()
    } else {
        vec![]
    };
    drop(manager);

    let db = state.db_manager.clone();
    let (conversations, messages_per_conversation) =
        tokio::task::spawn_blocking(move ||db.sync_contents())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|error| {
            tracing::error!(%error, "Sync pull failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let conv_jsons: Vec<serde_json::Value> = conversations
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "title": c.title,
                "model": c.model,
                "provider": c.provider,
                "workspace_path": c.workspace_path,
                "project_id": c.project_id,
                "workspace_mode": c.workspace_mode,
                "research_mode": c.research_mode,
                "pinned": c.pinned,
                "archived": c.archived,
                "created_at": c.created_at,
                "updated_at": c.updated_at,
                "message_count": c.message_count,
            })
        })
        .collect();

    let total_messages: usize = messages_per_conversation.values().map(|v| v.len()).sum();
    let total_convs = conversations.len();
    let server_timestamp = chrono::Utc::now().to_rfc3339();

    tracing::info!(
        "Sync Pull: {} providers, {} conversations, {} messages at {}",
        providers.len(),
        total_convs,
        total_messages,
        server_timestamp
    );

    Ok(Json(serde_json::json!({
        "providers": providers,
        "conversations": conv_jsons,
        "messagesPerConversation": messages_per_conversation,
        "deletedProviderIds": [],
        "deletedConversationIds": [],
        "serverTimestamp": server_timestamp,
    })))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/data/sync/push", post(sync_push))
        .route("/data/sync/pull", post(sync_pull))
}
