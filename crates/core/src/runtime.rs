//! Host-independent initialization and persistence. No window or HTTP server is needed.
use crate::{
    cache::FileCache,
    config::{AppConfig, ProviderConfig},
    db::{conversation_repo, message_repo, project_repo, DbManager},
    native_engine::{
        provider_manager::{ApiFormat, ModelConfig, Provider, ProviderManager},
        NativeEngine,
    },
    permissions::{AuditLogger, PermissionManager},
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

#[path = "runtime_user_content.rs"]
mod user_content;
pub use user_content::{image_block_from_file, image_media_type, normalize_user_content, MAX_IMAGE_BYTES};

/// Default data directory under the user's home.
pub const DATA_DIR_NAME: &str = ".deepseek-code";
/// SQLite database shared by the CLI and Desktop inside the data directory.
pub const DATABASE_FILE: &str = "deepseek_code.db";
/// Explicit data directory override.
pub const DATA_DIR_ENV: &str = "DEEPSEEK_CODE_DATA_DIR";

/// The data directory: `DEEPSEEK_CODE_DATA_DIR` when set, else `~/.deepseek-code`.
pub fn resolve_default_data_dir() -> Result<PathBuf> {
    resolve(std::env::var_os(DATA_DIR_ENV), dirs::home_dir())
}

fn resolve(explicit: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(home.context("Cannot locate the home directory; use --data-dir")?.join(DATA_DIR_NAME))
}

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub data_dir: PathBuf,
}

impl RuntimePaths {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        ensure!(data_dir.is_absolute(), "Data directory must be absolute");
        Ok(Self { data_dir })
    }

    /// Data directory shared by the CLI and Desktop, see [`resolve_default_data_dir`].
    pub fn default_data_dir() -> Result<PathBuf> {
        resolve_default_data_dir()
    }

    pub fn open_database(&self) -> Result<Arc<DbManager>> {
        let db = Arc::new(DbManager::new(self.data_dir.join(DATABASE_FILE)).context("Opening shared database pool")?);
        db.init().context("Initializing shared database schema")?;
        Ok(db)
    }

    /// Read without silently discarding malformed configuration or creating defaults.
    pub fn read_config(&self) -> Result<AppConfig> {
        let path = self.data_dir.join("config.json");
        if !path.exists() {
            return Ok(AppConfig::default());
        }
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("Invalid config: {}", path.display()))
    }

    pub fn providers(&self) -> Result<Vec<Provider>> {
        if self.data_dir.join("config.json").exists() {
            return Ok(self
                .read_config()?
                .providers
                .iter()
                .map(provider_from_config)
                .collect());
        }
        let path = self.data_dir.join("providers.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("Invalid providers: {}", path.display()))
    }
}

/// One mapping for Desktop's saved configuration and the standalone CLI.
pub fn provider_from_config(p: &ProviderConfig) -> Provider {
    Provider {
        id: p.id.clone(),
        name: p.name.clone(),
        base_url: p.base_url.clone(),
        api_key: p.api_key.clone().unwrap_or_default(),
        api_format: if p.provider_type == "anthropic" && !p.base_url.contains("deepseek") {
            ApiFormat::Anthropic
        } else {
            ApiFormat::OpenAI
        },
        builtin_id: p
            .settings
            .get("builtinId")
            .and_then(Value::as_str)
            .map(String::from),
        enabled: p.enabled,
        web_search_strategy: p.web_search_strategy.clone(),
        models: p
            .models
            .iter()
            .map(|m| ModelConfig {
                id: m.id.clone(),
                name: m.name.clone(),
                enabled: m.enabled,
                max_tokens: m.max_tokens,
                context_window: m.context_window,
                supports_text: m.supports_text,
                supports_vision: m.supports_vision,
                api: m.api.clone(),
                effort: None,
                fast: false,
                supports_web_search: false,
            })
            .collect(),
    }
}

pub fn create_engine(
    paths: &RuntimePaths,
    db: Arc<DbManager>,
    providers: Arc<Mutex<ProviderManager>>,
    permissions: Arc<PermissionManager>,
    cache: Arc<FileCache>,
) -> NativeEngine {
    NativeEngine::new(
        providers,
        db,
        paths.data_dir.join("workspaces"),
        permissions,
        cache,
    )
}

pub fn default_permissions() -> Arc<PermissionManager> {
    Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(1000))))
}

/// Sessions use the same project trust, Code prompt and SQLite schema as Desktop.
pub struct SessionStore {
    pub db: Arc<DbManager>,
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn explicit_session_id_is_durable_and_never_overwrites_existing_history() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = uuid::Uuid::new_v4().to_string();
        assert_eq!(store.create_with_id(root.path(),"model","provider",&id).unwrap(),id);
        store.append(&id,"user","Keep this conversation").unwrap();
        assert!(store.create_with_id(root.path(),"other","provider",&id).is_err());
        assert!(store.create_with_id(root.path(),"model","provider","invalid").is_err());
        assert_eq!(store.resume(&id).unwrap().model.as_deref(),Some("model"));
        assert_eq!(store.history(&id).unwrap()[0]["content"],"Keep this conversation");
    }
}

impl SessionStore {
    pub fn new(db: Arc<DbManager>) -> Self {
        Self { db }
    }

    pub fn create(&self, workspace: &Path, model: &str, provider: &str) -> Result<String> {
        self.create_with_id(workspace, model, provider, &uuid::Uuid::new_v4().to_string())
    }

    /// Create a fresh conversation; existing IDs remain untouched.
    pub fn create_with_id(&self, workspace: &Path, model: &str, provider: &str, id: &str) -> Result<String> {
        let id = uuid::Uuid::parse_str(id).context("Session ID must be a UUID")?.to_string();
        let workspace =
            std::fs::canonicalize(workspace).context("Project directory is unavailable")?;
        ensure!(workspace.is_dir(), "Project path must be a directory");
        let path = workspace.to_string_lossy().into_owned();
        self.db.with_conn(|conn| -> Result<()> {
            // Acquire the writer before reading project state, avoiding a stale WAL snapshot upgrade.
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)", [&id], |row| row.get::<_,bool>(0))?, "Session ID already exists; use --resume to continue it");
            let project = project_repo::list_projects(&tx)?.into_iter().find(|p| {
                !p.is_archived
                    && p.workspace_path
                        .as_deref()
                        .and_then(|s| std::fs::canonicalize(s).ok())
                        .as_ref()
                        == Some(&workspace)
                    && p.folders.iter().all(|f| f.trusted)
            });
            let now = chrono::Utc::now().to_rfc3339();
            let project_id = if let Some(project) = project {
                project.id
            } else {
                let project_id = uuid::Uuid::new_v4().to_string();
                let name = workspace.file_name().unwrap_or_default().to_string_lossy();
                project_repo::insert_project(
                    &tx,
                    &project_id,
                    &name,
                    None,
                    None,
                    Some(&path),
                    false,
                    &now,
                    &now,
                )?;
                project_repo::set_folders(&tx, &project_id, &[path.clone()])?;
                // Invoking CLI in this directory (or passing --path) selects this project.
                project_repo::trust_folders(&tx, &project_id, &[path.clone()])?;
                project_id
            };
            conversation_repo::insert_conversation(
                &tx,
                &id,
                Some("CLI session"),
                Some(model),
                Some(provider),
                Some(&path),
                Some(&project_id),
                false,
                false,
                false,
                &now,
                &now,
                0,
            )?;
            conversation_repo::set_workspace_mode(&tx, &id, "code")?;
            tx.commit()?;
            Ok(())
        })??;
        Ok(id)
    }

    pub fn resume(&self, id: &str) -> Result<conversation_repo::ConversationRow> {
        self.db.with_conn(|conn| -> Result<_> {
            let session =
                conversation_repo::get_conversation(conn, id)?.context("Session not found")?;
            ensure!(
                session.workspace_mode == "code",
                "Only Code sessions can be resumed in CLI"
            );
            ensure!(!session.archived, "Session is archived");
            crate::native_engine::workspace::conversation_project(conn, id)?;
            Ok(session)
        })?
    }

    pub fn history(&self, id: &str) -> Result<Vec<Value>> {
        self.db.with_conn(|conn| -> Result<_> {
            let rows = message_repo::get_messages_by_conversation(conn, id)?;
            Ok(rows
                .into_iter()
                .map(|m| json!({"role":m.role,"content":m.content}))
                .collect())
        })?
    }

    /// Latest usable Code session in this exact directory, shared with Desktop.
    /// Archived sessions and unavailable/untrusted projects are not candidates.
    pub fn latest(&self, workspace: &Path) -> Result<Option<conversation_repo::ConversationRow>> {
        let workspace =
            std::fs::canonicalize(workspace).context("Project directory is unavailable")?;
        ensure!(workspace.is_dir(), "Project path must be a directory");
        self.db.with_conn(|conn| -> Result<_> {
            let mut sessions = conversation_repo::list_conversations(conn)?;
            sessions.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            for session in sessions {
                if session.archived || session.workspace_mode != "code" {
                    continue;
                }
                // Resolve the same project/worktree as the engine, rather than a
                // stale conversation workspace_path or the caller's override.
                let Ok((path, _)) = crate::native_engine::workspace::resolve_workspace(
                    conn,
                    &workspace,
                    &session.id,
                    None,
                ) else {
                    continue;
                };
                if std::fs::canonicalize(path).ok().as_ref() == Some(&workspace) {
                    return Ok(Some(session));
                }
            }
            Ok(None)
        })?
    }

    /// Fork using Desktop's branching implementation, including tools and attachments.
    pub fn fork(&self, id: &str) -> Result<String> {
        self.resume(id)?;
        self.db
            .with_conn(|conn| conversation_repo::branch_conversation(conn, id, None, None))?
    }

    /// Start an empty session in the same project without deleting the original.
    /// Copy bindings atomically so agent restrictions and worktrees survive clear.
    pub fn clear(&self, id: &str) -> Result<String> {
        self.resume(id)?;
        self.derive_empty(id, None)
    }

    /// Derive a child without converting Desktop modes/private folders into Code projects.
    pub fn derive_owned_child(&self, id: &str, workspaces_root: &Path, expected_cwd: &Path) -> Result<String> {
        self.derive_empty(id, Some((workspaces_root, expected_cwd)))
    }

    fn derive_empty(&self, id: &str, owned: Option<(&Path, &Path)>) -> Result<String> {
        self.db.with_conn(|conn| -> Result<String> {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let source = conversation_repo::get_conversation(&tx, id)?.context("Session not found")?;
            ensure!(!source.archived, "Parent session is archived");
            crate::native_engine::workspace::conversation_project(&tx, id)?;
            if let Some((root, expected)) = owned {
                let actual = crate::native_engine::workspace::resolve_workspace(&tx, root, id, None)?.0;
                ensure!(actual.canonicalize()? == expected.canonicalize()?, "Parent runtime workspace differs from its persisted binding");
            }
            let new_id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().to_rfc3339();
            conversation_repo::insert_conversation(
                &tx, &new_id, Some("CLI session"), source.model.as_deref(),
                source.provider.as_deref(), source.workspace_path.as_deref(),
                source.project_id.as_deref(), source.research_mode, false, false,
                &now, &now, 0,
            )?;
            conversation_repo::set_workspace_mode(&tx, &new_id, &source.workspace_mode)?;
            tx.execute(
                "INSERT INTO session_agents (conversation_id, definition) SELECT ?1, definition FROM session_agents WHERE conversation_id = ?2",
                rusqlite::params![new_id, id],
            )?;
            tx.execute(
                "INSERT INTO conversation_worktrees (conversation_id, project_id, source_path, repo_path, path, branch) SELECT ?1, project_id, source_path, repo_path, path, branch FROM conversation_worktrees WHERE conversation_id = ?2",
                rusqlite::params![new_id, id],
            )?;
            tx.execute(
                "INSERT INTO conversation_worktree_backends (conversation_id, backend, metadata) SELECT ?1, backend, metadata FROM conversation_worktree_backends WHERE conversation_id=?2",
                rusqlite::params![new_id, id],
            )?;
            if owned.is_some() && source.project_id.as_deref().is_none_or(str::is_empty) {
                ensure!(source.workspace_mode != "code", "Code children require a trusted project");
                tx.execute("INSERT INTO conversation_private_workspace_bindings(child_id,parent_id) VALUES(?1,?2)", rusqlite::params![new_id,id])?;
            }
            if let Some((root, expected)) = owned {
                let actual = crate::native_engine::workspace::resolve_workspace(&tx, root, &new_id, None)?.0;
                ensure!(actual.canonicalize()? == expected.canonicalize()?, "Child workspace differs from its parent binding");
            }
            if owned.is_some() { crate::services::permission_authority::inherit_conn(&tx, id, &new_id)?; }
            tx.commit()?;
            Ok(new_id)
        })?
    }

    pub fn workspace(&self, id: &str, workspaces_root: &Path) -> Result<PathBuf> {
        self.resume(id)?;
        self.db.with_conn(|conn| {
            crate::native_engine::workspace::resolve_workspace(conn, workspaces_root, id, None)
                .map(|(path, _)| path)
        })?
    }

    /// Rebuild protocol-valid tool history from the shared SQLite tool rows.
    pub fn model_history(&self, id: &str, format: &ApiFormat) -> Result<Vec<Value>> {
        self.db.with_conn(|conn| -> Result<_> {
            let mut history = Vec::new();
            let messages = message_repo::get_messages_by_conversation(conn, id)?;
            let start = messages.iter().rposition(|m| m.is_compact_boundary).unwrap_or(0);
            for message in messages.into_iter().skip(start) {
                let calls = message_repo::get_tool_calls_for_message(conn, &message.id)?;
                if message.role == "assistant" && !calls.is_empty() {
                    match format {
                        ApiFormat::OpenAI => {
                            let tool_calls: Vec<_> = calls.iter().map(|t| json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.input.as_deref().unwrap_or("{}")}})).collect();
                            history.push(json!({"role":"assistant","content":"","tool_calls":tool_calls}));
                            for tool in &calls { history.push(json!({"role":"tool","tool_call_id":tool.id,"content":tool.output.as_deref().unwrap_or("")})); }
                        }
                        ApiFormat::Anthropic => {
                            let blocks: Vec<_> = calls.iter().map(|t| json!({"type":"tool_use","id":t.id,"name":t.name,"input":t.input.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()).unwrap_or(json!({}))})).collect();
                            history.push(json!({"role":"assistant","content":blocks}));
                            let results: Vec<_> = calls.iter().map(|t| json!({"type":"tool_result","tool_use_id":t.id,"content":t.output.as_deref().unwrap_or(""),"is_error":t.is_error})).collect();
                            history.push(json!({"role":"user","content":results}));
                        }
                    }
                }
                use rusqlite::OptionalExtension;
                let structured: Option<String> = conn.query_row("SELECT content_json FROM message_user_content WHERE message_id=?1", [&message.id], |r|r.get(0)).optional()?;
                if let Some(content)=structured {history.push(json!({"role":message.role,"content":serde_json::from_str::<Value>(&content)?}));}
                else if !message.content.is_empty() { history.push(json!({"role":message.role,"content":message.content})); }
            }
            crate::native_engine::mcp_output::restore_history(&mut history, conn, id)?;
            Ok(history)
        })?
    }

    pub fn append(&self, id: &str, role: &str, text: &str) -> Result<String> {
        self.append_with_content(id, role, text, None, false)
    }

    pub fn append_user_content(&self, id: &str, text: &str, content: &Value) -> Result<String> {
        let content = normalize_user_content(content)?;
        self.append_with_content(id, "user", text, Some(&content), false)
    }

    pub fn append_hook_user(&self, id: &str, text: &str) -> Result<String> {
        self.append_with_content(id, "user", text, None, true)
    }

    fn append_with_content(
        &self,
        id: &str,
        role: &str,
        text: &str,
        content: Option<&Value>,
        initial_hook: bool,
    ) -> Result<String> {
        let message_id = uuid::Uuid::new_v4().to_string();
        self.db.with_conn(|conn| -> Result<()> {
            // Message ordering is a read/modify/write operation shared by CLI and Desktop hosts.
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let order: i64 = tx.query_row(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM messages WHERE conversation_id = ?1",
                [id],
                |row| row.get(0),
            )?;
            let now = chrono::Utc::now().to_rfc3339();
            message_repo::insert_message(
                &tx,
                &message_id,
                id,
                role,
                text,
                None,
                &now,
                false,
                order,
            )?;
            if initial_hook {
                tx.execute(
                    "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
                    rusqlite::params![
                        format!("message-origin:{message_id}"),
                        r#"{"type":"hook","hookEvent":"SessionStart"}"#
                    ],
                )?;
            }
            conversation_repo::increment_message_count(&tx, id)?;
            if let Some(content) = content {
                tx.execute(
                    "INSERT INTO message_user_content (message_id,content_json) VALUES (?1,?2)",
                    rusqlite::params![message_id, serde_json::to_string(content)?],
                )?;
            }
            conversation_repo::update_conversation_timestamp(&tx, id, &now)?;
            tx.commit()?;
            Ok(())
        })??;
        Ok(message_id)
    }
}

#[cfg(test)]
mod data_dir_tests {
    use super::*;

    #[test]
    fn explicit_directory_wins_over_the_home_default() {
        let home = PathBuf::from("/home/user");
        assert_eq!(resolve(None, Some(home.clone())).unwrap(), home.join(DATA_DIR_NAME));
        assert_eq!(resolve(Some("".into()), Some(home.clone())).unwrap(), home.join(DATA_DIR_NAME));
        assert_eq!(resolve(Some("/data/dcl".into()), Some(home)).unwrap(), PathBuf::from("/data/dcl"));
        assert!(resolve(None, None).is_err());
    }
}
