//! Global orchestration configuration, loaded once from
//! `config/orchestration.toml` at startup.  All previously hard-coded
//! magic numbers live here now.

pub mod api_endpoints;
pub mod command_output;
mod deepseek_search;
pub mod ui_language;
mod web_fetch;
pub use deepseek_search::DeepSeekSearchConfig;
pub use web_fetch::WebFetchConfig;

use anyhow::Result;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

static CONFIG: OnceCell<OrchestrationConfig> = OnceCell::new();

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationConfig {
    pub tool_loop: ToolLoopConfig,
    pub tools: ToolsConfig,
    pub memory: MemoryConfig,
    pub bridge: BridgeConfig,
    pub http: HttpConfig,
    pub logging: LoggingConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolLoopConfig {
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: Option<usize>,
    #[serde(default = "default_token_threshold")]
    pub token_threshold: usize,
    #[serde(default = "default_recent_messages_to_keep")]
    pub recent_messages_to_keep: usize,
    #[serde(default = "default_max_tokens")]
    pub default_max_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_true")]
    pub summarize_long_command_output: bool,
    #[serde(default = "default_bash_timeout_secs")]
    pub default_bash_timeout_secs: u64,
    #[serde(default)]
    pub computer_use: ComputerUseConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComputerUseConfig {
    #[serde(default = "default_display_width")]
    pub display_width: u32,
    #[serde(default = "default_display_height")]
    pub display_height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_bridge_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_pool_idle_timeout_secs")]
    pub pool_idle_timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_backoff_ms")]
    pub retry_base_backoff_ms: u64,
    /// Opt-in: replay one model step that stalled after content arrived.
    ///
    /// Defaults to `false`, which keeps the visible partial output and the
    /// failed turn. Enabled, the uncommitted step is discarded and issued once
    /// more; nothing is replayed after a terminal marker or a complete stream.
    #[serde(default)]
    pub post_content_timeout_recovery: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub log_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub otlp_endpoint: String,
}

// ---- Default value factories ----

fn default_max_tool_iterations() -> Option<usize> { None }
fn default_token_threshold() -> usize { 600_000 }
fn default_recent_messages_to_keep() -> usize { 10 }
fn default_max_tokens() -> u32 { 1000000 }
fn default_max_output_bytes() -> usize { 100 * 1024 }
fn default_bash_timeout_secs() -> u64 { 120 }
fn default_display_width() -> u32 { 1920 }
fn default_display_height() -> u32 { 1080 }
fn default_true() -> bool { true }
fn default_bridge_port() -> u16 { 30085 }
fn default_request_timeout_secs() -> u64 { 300 }
fn default_pool_idle_timeout_secs() -> u64 { 300 }
fn default_max_retries() -> u32 { 7 }
fn default_retry_base_backoff_ms() -> u64 { 2000 }
fn default_log_level() -> String { "info".to_string() }

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            tool_loop: ToolLoopConfig {
                max_tool_iterations: default_max_tool_iterations(),
                token_threshold: default_token_threshold(),
                recent_messages_to_keep: default_recent_messages_to_keep(),
                default_max_tokens: default_max_tokens(),
            },
            tools: ToolsConfig {
                max_output_bytes: default_max_output_bytes(),
                summarize_long_command_output: true,
                default_bash_timeout_secs: default_bash_timeout_secs(),
                computer_use: ComputerUseConfig::default(),
            },
            memory: MemoryConfig {
                enabled: true,
            },
            bridge: BridgeConfig {
                port: default_bridge_port(),
            },
            http: HttpConfig {
                request_timeout_secs: default_request_timeout_secs(),
                pool_idle_timeout_secs: default_pool_idle_timeout_secs(),
                max_retries: default_max_retries(),
                retry_base_backoff_ms: default_retry_base_backoff_ms(),
                post_content_timeout_recovery: false,
            },
            logging: LoggingConfig {
                level: default_log_level(),
                log_dir: String::new(),
            },
            telemetry: TelemetryConfig {
                otlp_endpoint: String::new(),
            },
        }
    }
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            display_width: default_display_width(),
            display_height: default_display_height(),
        }
    }
}

impl OrchestrationConfig {
    /// Load configuration from the default path or return built-in defaults.
    pub fn load() -> Result<Self> {
        // Try several candidate paths
        let candidates = [
            "config/orchestration.toml",
            "../config/orchestration.toml",
        ];
        for path in &candidates {
            if Path::new(path).exists() {
                let content = std::fs::read_to_string(path)?;
                return Ok(toml::from_str(&content)?);
            }
        }
        tracing::info!("orchestration.toml not found, using built-in defaults");
        Ok(Self::default())
    }

    /// Initialise once; multiple frontend components and parallel tests may call this.
    pub fn init_global() -> Result<()> {
        CONFIG.get_or_try_init(Self::load)?;
        Ok(())
    }

    /// Borrow the global config, loading it on first use so every embedder
    /// (Desktop, dcd-server, tests) works without an explicit `init_global()`.
    /// An unreadable configuration file falls back to built-in defaults.
    pub fn get() -> &'static Self {
        CONFIG.get_or_init(|| {
            Self::load().unwrap_or_else(|error| {
                tracing::warn!("Failed to load orchestration.toml, using defaults: {error}");
                Self::default()
            })
        })
    }

    // ---- Convenience accessors (avoids deep .tool_loop.xxx chains everywhere) ----

    pub fn max_tool_iterations() -> Option<usize> { Self::get().tool_loop.max_tool_iterations }
    pub fn token_threshold() -> usize { Self::get().tool_loop.token_threshold }
    pub fn recent_messages_to_keep() -> usize { Self::get().tool_loop.recent_messages_to_keep }
    pub fn default_max_tokens() -> u32 { Self::get().tool_loop.default_max_tokens }
    pub fn max_output_bytes() -> usize { Self::get().tools.max_output_bytes }
    pub fn default_bash_timeout_secs() -> u64 { Self::get().tools.default_bash_timeout_secs }
    pub fn max_retries() -> u32 { Self::get().http.max_retries }
    pub fn retry_base_backoff_ms() -> u64 { Self::get().http.retry_base_backoff_ms }
    pub fn request_timeout_secs() -> u64 { Self::get().http.request_timeout_secs }
    pub fn pool_idle_timeout_secs() -> u64 { Self::get().http.pool_idle_timeout_secs }
    /// Read once per turn; a stalled post-content step is replayed only when set.
    pub fn post_content_timeout_recovery() -> bool { Self::get().http.post_content_timeout_recovery }
    pub fn bridge_port() -> u16 { Self::get().bridge.port }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub max_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub supports_vision: bool,
    #[serde(default = "default_true")]
    pub supports_text: bool,
    #[serde(default)]
    pub api: Option<String>,
    pub supports_tools: bool,
    pub supports_streaming: bool,
    pub context_window: Option<u32>,
    pub cost_per_1k_input: Option<f64>,
    pub cost_per_1k_output: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub api_key: Option<String>,
    pub base_url: String,
    pub models: Vec<ModelConfig>,
    pub enabled: bool,
    pub is_default: bool,
    pub settings: HashMap<String, serde_json::Value>,
    pub supports_web_search: bool,
    pub web_search_strategy: Option<String>,
    pub web_search_tested_at: Option<u64>,
    pub web_search_test_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub deepseek_search: DeepSeekSearchConfig,
    #[serde(default)]
    pub web_fetch: WebFetchConfig,
    #[serde(default)]
    pub default_provider_id: Option<String>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            deepseek_search: DeepSeekSearchConfig::default(),
            web_fetch: WebFetchConfig::default(),
            default_provider_id: None,
            theme: None,
            language: None,
        }
    }
}

pub struct ConfigManager {
    config: AppConfig,
    config_path: PathBuf,
}

impl ConfigManager {
    pub fn new(config_dir: PathBuf) -> Self {
        let config_path =
            crate::data_layout::DataLayout::new(config_dir).config_file("config.json");
        let config = if config_path.exists() {
            std::fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            AppConfig::default()
        };
        Self { config, config_path }
    }

    pub fn get_config(&self) -> &AppConfig {
        &self.config
    }

    /// Refresh shared CLI/Desktop configuration without discarding valid state on failure.
    pub fn reload(&mut self) -> Result<()> {
        if self.config_path.exists() {
            self.config = serde_json::from_str(&std::fs::read_to_string(&self.config_path)?)?;
        }
        Ok(())
    }

    pub fn saved_cli_model(&self) -> Option<String> {
        let path = self.config_path.parent()?.join("cli.json");
        let prefs: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        prefs["model"].as_str().map(str::to_owned)
    }

    pub fn update_config<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut AppConfig),
    {
        let previous = self.config.clone();
        f(&mut self.config);
        if let Err(error) = self.save() { self.config = previous; return Err(error); }
        Ok(())
    }

    pub fn add_provider(&mut self, provider: ProviderConfig) -> Result<()> {
        self.update_config(|c| c.providers.push(provider))
    }

    pub fn remove_provider(&mut self, id: &str) -> Result<()> {
        self.update_config(|c| c.providers.retain(|p| p.id != id))
    }

    pub fn get_provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.config.providers.iter().find(|p| p.id == id)
    }

    pub fn get_provider_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.config.providers.iter_mut().find(|p| p.id == id)
    }

    pub fn get_default_provider(&self) -> Option<&ProviderConfig> {
        if let Some(ref id) = self.config.default_provider_id {
            self.get_provider(id)
        } else {
            self.config.providers.iter().find(|p| p.is_default).or_else(|| self.config.providers.first())
        }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        let temporary = self.config_path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temporary, json)?;
        // The configuration may hold API keys: readable by the owner only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        }
        if let Err(error) = std::fs::rename(&temporary, &self.config_path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_output_summary_defaults_on_and_can_be_disabled() {
        assert!(
            OrchestrationConfig::default()
                .tools
                .summarize_long_command_output
        );
        for (config, expected) in [
            ("", true),
            ("summarize_long_command_output = true", true),
            ("summarize_long_command_output = false", false),
        ] {
            let tools = toml::from_str::<ToolsConfig>(config).unwrap();
            assert_eq!(tools.summarize_long_command_output, expected);
        }
    }

    #[test]
    fn shared_config_reload_observes_changes_and_preserves_state_on_invalid_json() {
        let root = tempfile::tempdir().unwrap();
        let mut desktop = ConfigManager::new(root.path().to_path_buf());
        let mut cli = ConfigManager::new(root.path().to_path_buf());
        cli.update_config(|config| config.default_provider_id = Some("shared".into()))
            .unwrap();
        desktop.reload().unwrap();
        assert_eq!(
            desktop.get_config().default_provider_id.as_deref(),
            Some("shared")
        );
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("cli.json"),
            r#"{"model":"shared::model"}"#,
        )
        .unwrap();
        assert_eq!(desktop.saved_cli_model().as_deref(), Some("shared::model"));
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
            "invalid",
        )
        .unwrap();
        assert!(desktop.reload().is_err());
        assert_eq!(desktop.get_config().default_provider_id.as_deref(), Some("shared"));
    }

    #[test]
    fn post_content_timeout_recovery_defaults_to_false() {
        // Built-in defaults never enable a replay of a stalled model step.
        assert!(
            !OrchestrationConfig::default()
                .http
                .post_content_timeout_recovery
        );
        let document = |http: &str| {
            format!(
                "[tool_loop]\n[tools]\n[memory]\n[bridge]\n[http]\n{http}[logging]\n[telemetry]\n"
            )
        };
        // An older valid file that predates the setting keeps the opt-out.
        let older: OrchestrationConfig = toml::from_str(&document("")).unwrap();
        assert!(!older.http.post_content_timeout_recovery);
        let off: OrchestrationConfig = toml::from_str(&document(
            "request_timeout_secs = 30\npost_content_timeout_recovery = false\n",
        ))
        .unwrap();
        assert!(!off.http.post_content_timeout_recovery);
        let on: OrchestrationConfig = toml::from_str(&document(
            "request_timeout_secs = 30\npost_content_timeout_recovery = true\n",
        ))
        .unwrap();
        assert!(on.http.post_content_timeout_recovery);
    }

    #[test]
    fn global_config_loads_on_first_use_without_explicit_init() {
        // Accessors used by the engine must not panic in embedders that skip init_global().
        assert!(OrchestrationConfig::default_max_tokens() > 0);
        assert!(OrchestrationConfig::init_global().is_ok());
    }

    #[test]
    fn test_default_values() {
        let config = OrchestrationConfig::default();

        // ToolLoopConfig
        assert_eq!(config.tool_loop.max_tool_iterations, None);
        assert_eq!(config.tool_loop.token_threshold, 600_000);
        assert_eq!(config.tool_loop.recent_messages_to_keep, 10);
        assert_eq!(config.tool_loop.default_max_tokens, 1000000);

        // ToolsConfig
        assert_eq!(config.tools.max_output_bytes, 102_400);
        assert_eq!(config.tools.default_bash_timeout_secs, 120);

        // ComputerUseConfig (nested under tools)
        assert_eq!(config.tools.computer_use.display_width, 1920);
        assert_eq!(config.tools.computer_use.display_height, 1080);

        // MemoryConfig
        assert!(config.memory.enabled);

        // BridgeConfig
        assert_eq!(config.bridge.port, 30085);

        // HttpConfig
        assert_eq!(config.http.request_timeout_secs, 300);
        assert_eq!(config.http.pool_idle_timeout_secs, 300);
        assert_eq!(config.http.max_retries, 7);
        assert_eq!(config.http.retry_base_backoff_ms, 2000);

        // LoggingConfig
        assert_eq!(config.logging.level, "info");
        assert!(config.logging.log_dir.is_empty());

        // TelemetryConfig
        assert!(config.telemetry.otlp_endpoint.is_empty());
    }

    #[test]
    fn pool_idle_timeout_defaults_to_300_and_parses_explicit_values() {
        assert_eq!(OrchestrationConfig::default().http.pool_idle_timeout_secs, 300);
        let document = |http: &str| {
            format!(
                "[tool_loop]\n[tools]\n[memory]\n[bridge]\n[http]\n{http}[logging]\n[telemetry]\n"
            )
        };
        // An older valid file without the key keeps the 300-second default.
        let older: OrchestrationConfig = toml::from_str(&document("")).unwrap();
        assert_eq!(older.http.pool_idle_timeout_secs, 300);

        // Older file with only existing [http] keys still loads cleanly.
        let legacy: OrchestrationConfig = toml::from_str(&document(
            "request_timeout_secs = 120\nmax_retries = 5\nretry_base_backoff_ms = 1000\n",
        ))
        .unwrap();
        assert_eq!(legacy.http.pool_idle_timeout_secs, 300);

        // Explicit positive value is respected.
        let explicit: OrchestrationConfig =
            toml::from_str(&document("pool_idle_timeout_secs = 600\n")).unwrap();
        assert_eq!(explicit.http.pool_idle_timeout_secs, 600);
    }

    #[test]
    fn old_memory_backend_url_still_loads() {
        let document = "[tool_loop]\n[tools]\n[memory]\nmemex_backend_url = \"http://127.0.0.1:8765\"\nenabled = false\n[bridge]\n[http]\n[logging]\n[telemetry]\n";
        let config: OrchestrationConfig = toml::from_str(document).unwrap();
        assert!(!config.memory.enabled);
    }

    #[test]
    fn test_computer_use_default() {
        let config = ComputerUseConfig::default();
        assert_eq!(config.display_width, 1920);
        assert_eq!(config.display_height, 1080);
    }

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert!(config.providers.is_empty());
        assert_eq!(config.web_fetch, WebFetchConfig::default());
        assert!(config.default_provider_id.is_none());
        assert!(config.theme.is_none());
        assert!(config.language.is_none());
    }

    #[test]
    fn test_init_global_returns_ok() {
        // init_global uses OnceCell and can only be called once per process.
        // The first call returns Ok(()); subsequent calls return an error
        // indicating the config is already initialised.
        let result = OrchestrationConfig::init_global();
        match result {
            Ok(()) => {
                // First call — verify get() works and values match defaults
                let config = OrchestrationConfig::get();
                assert_eq!(config.tool_loop.max_tool_iterations, None);
                assert_eq!(config.tools.max_output_bytes, 102_400);
                assert_eq!(config.bridge.port, 30085);
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("already initialised"),
                    "Expected 'already initialised' error, got: {}",
                    msg
                );
            }
        }
    }
}
