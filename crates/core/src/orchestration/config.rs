//! Settings of the multi-agent orchestrator, kept in `orchestrator.toml` in the
//! data directory. They are separate from the bundled
//! `config/orchestration.toml`, which holds the engine tunables read by
//! `crate::config::OrchestrationConfig` in a different format.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// File name of the orchestrator settings inside the data directory.
pub const ORCHESTRATOR_CONFIG_FILE: &str = "orchestrator.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfigFile {
    pub orchestrator: OrchestratorSection,
    pub priority_scheduling: PrioritySchedulingSection,
    pub logging: LoggingSection,
    pub queue: QueueSection,
    pub workflow: WorkflowSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorSection {
    pub max_concurrent_agents: usize,
    pub default_model: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrioritySchedulingSection {
    pub enabled: bool,
    pub priority_adjust_interval_ms: u64,
    pub aging_factor: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSection {
    pub log_level: String,
    pub task_logging_enabled: bool,
    pub dependency_tracking_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSection {
    pub queue_check_interval_ms: u64,
    pub max_tasks_per_cycle: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSection {
    pub openspace_enabled: bool,
    pub superpowers_enabled: bool,
    pub gstack_enabled: bool,
    pub auto_validate_plan: bool,
    pub auto_execute: bool,
}

impl Default for OrchestratorConfigFile {
    fn default() -> Self {
        Self {
            orchestrator: OrchestratorSection {
                max_concurrent_agents: 8,
                default_model: "claude-sonnet-4-6".to_string(),
                timeout_ms: 300000,
            },
            priority_scheduling: PrioritySchedulingSection {
                enabled: true,
                priority_adjust_interval_ms: 10000,
                aging_factor: 0.1,
            },
            logging: LoggingSection {
                log_level: "trace".to_string(),
                task_logging_enabled: true,
                dependency_tracking_enabled: true,
            },
            queue: QueueSection {
                queue_check_interval_ms: 100,
                max_tasks_per_cycle: 5,
            },
            workflow: WorkflowSection {
                openspace_enabled: true,
                superpowers_enabled: true,
                gstack_enabled: true,
                auto_validate_plan: true,
                auto_execute: false,
            },
        }
    }
}

impl OrchestratorConfigFile {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let content = fs::read_to_string(&path).map_err(|e| format!("Failed to read config file: {}", e))?;
        let config: OrchestratorConfigFile = toml::from_str(&content).map_err(|e| format!("Failed to parse config: {}", e))?;
        Ok(config)
    }

    /// Where the orchestrator settings live for a data directory.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join(ORCHESTRATOR_CONFIG_FILE)
    }

    /// The saved settings, or the defaults when there is no file yet.
    pub fn load_or_default<P: AsRef<Path>>(path: P) -> Self {
        if !path.as_ref().exists() {
            return Self::default();
        }
        match Self::load_from_file(path) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(module = "OrchestratorConfig", "Failed to load config: {}, using defaults", e);
                Self::default()
            }
        }
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), String> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }
        let content = toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {}", e))?;
        fs::write(&path, content).map_err(|e| format!("Failed to write config file: {}", e))?;
        Ok(())
    }
}

impl From<&OrchestratorConfigFile> for super::OrchestratorConfig {
    fn from(file_config: &OrchestratorConfigFile) -> Self {
        Self {
            max_concurrent_agents: file_config.orchestrator.max_concurrent_agents,
            default_model: file_config.orchestrator.default_model.clone(),
            timeout_ms: file_config.orchestrator.timeout_ms,
            enable_priority_scheduling: file_config.priority_scheduling.enabled,
            priority_adjust_interval_ms: file_config.priority_scheduling.priority_adjust_interval_ms,
            aging_factor: file_config.priority_scheduling.aging_factor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_in_the_data_directory_and_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = OrchestratorConfigFile::path_in(dir.path());
        assert_eq!(path, dir.path().join("orchestrator.toml"));
        let defaults = OrchestratorConfigFile::load_or_default(&path);
        assert_eq!(defaults.orchestrator.max_concurrent_agents, 8);

        let mut config = defaults.clone();
        config.orchestrator.max_concurrent_agents = 3;
        config.workflow.auto_execute = true;
        config.save_to_file(&path).unwrap();
        let loaded = OrchestratorConfigFile::load_from_file(&path).unwrap();
        assert_eq!(loaded.orchestrator.max_concurrent_agents, 3);
        assert!(loaded.workflow.auto_execute);
    }

    #[test]
    fn missing_sections_take_their_defaults() {
        let config: OrchestratorConfigFile = toml::from_str(
            "[orchestrator]\nmax_concurrent_agents = 2\ndefault_model = \"m\"\ntimeout_ms = 1000\n",
        )
        .unwrap();
        assert_eq!(config.orchestrator.max_concurrent_agents, 2);
        assert!(config.priority_scheduling.enabled);
    }

    #[test]
    fn the_bundled_engine_config_is_not_this_format() {
        // config/orchestration.toml belongs to crate::config::OrchestrationConfig.
        let bundled = include_str!("../../../../src-tauri/config/orchestration.toml");
        assert!(toml::from_str::<crate::config::OrchestrationConfig>(bundled).is_ok());
        assert!(!bundled.contains("[orchestrator]"));
    }
}
