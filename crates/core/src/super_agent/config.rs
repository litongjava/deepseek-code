//! Validated Super Agent configuration.
//!
//! Settings live under `superAgent` in the shared peripheral settings store so
//! CLI and Desktop read the same values. Values are bounded before use: a bad
//! configuration fails loudly instead of silently disabling supervision.
use crate::native_engine::provider_manager::{ModelConfig, Provider};
use crate::services::ServiceContext;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const SETTINGS_KEY: &str = "superAgent";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlanApprovalPolicy {
    /// The user confirms every plan, including after a specification change.
    #[default]
    Manual,
    /// The Router confirms a plan that satisfies the configured policy.
    Auto,
}

impl PlanApprovalPolicy {
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            other => bail!("Unknown plan approval policy: {other}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentSelection {
    /// A configured model ID. Omitted means "the Router's current model".
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentsConfig {
    pub coder: AgentSelection,
    pub reviewer: AgentSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApprovalsConfig {
    pub plan: PlanApprovalPolicy,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            plan: PlanApprovalPolicy::Manual,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReviewConfig {
    pub max_rounds: u32,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self { max_rounds: 5 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SchedulerConfig {
    pub max_concurrent_pairs: usize,
    pub max_spawned_tasks_per_task: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_pairs: 3,
            max_spawned_tasks_per_task: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SuperAgentConfig {
    pub router: AgentSelection,
    pub agents: AgentsConfig,
    pub approvals: ApprovalsConfig,
    pub review: ReviewConfig,
    pub scheduler: SchedulerConfig,
}

impl SuperAgentConfig {
    pub fn load(context: &ServiceContext) -> Result<Self> {
        let config: Self = context.setting(SETTINGS_KEY)?.unwrap_or_default();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (label, selection) in [
            ("router", &self.router),
            ("coder", &self.agents.coder),
            ("reviewer", &self.agents.reviewer),
        ] {
            if let Some(model) = &selection.model {
                anyhow::ensure!(
                    !model.trim().is_empty()
                        && model.len() <= 200
                        && !model.chars().any(char::is_control),
                    "superAgent {label} model must be 1-200 printable characters"
                );
            }
        }
        anyhow::ensure!(
            (1..=20).contains(&self.review.max_rounds),
            "superAgent review.maxRounds must be between 1 and 20"
        );
        anyhow::ensure!(
            (1..=16).contains(&self.scheduler.max_concurrent_pairs),
            "superAgent scheduler.maxConcurrentPairs must be between 1 and 16"
        );
        anyhow::ensure!(
            self.scheduler.max_spawned_tasks_per_task <= 20,
            "superAgent scheduler.maxSpawnedTasksPerTask must be at most 20"
        );
        Ok(())
    }

    /// Resolves the model for one role inside the Router's provider. A model
    /// that is not configured in this provider falls back to the Router's model
    /// and produces a warning that the caller must persist as evidence.
    pub fn resolve_agent_model(
        &self,
        role: AgentModelRole,
        router_model: &ModelConfig,
        provider: &Provider,
    ) -> (ModelConfig, Option<ModelFallback>) {
        let configured = match role {
            AgentModelRole::Coder => self.agents.coder.model.as_deref(),
            AgentModelRole::Reviewer => self.agents.reviewer.model.as_deref(),
        };
        let Some(configured) = configured.filter(|value| !value.trim().is_empty()) else {
            return (router_model.clone(), None);
        };
        match provider
            .models
            .iter()
            .find(|model| model.id == configured && model.enabled)
        {
            Some(model) => (model.clone(), None),
            None => (
                router_model.clone(),
                Some(ModelFallback {
                    role,
                    configured: configured.to_owned(),
                    used: router_model.id.clone(),
                }),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentModelRole {
    Coder,
    Reviewer,
}

impl AgentModelRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coder => "coder",
            Self::Reviewer => "reviewer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFallback {
    pub role: AgentModelRole,
    pub configured: String,
    pub used: String,
}

impl ModelFallback {
    /// A stable, credential-free message for the task trace.
    pub fn message(&self) -> String {
        format!(
            "Configured {} model {} is not available in this provider; using {}",
            self.role.as_str(),
            self.configured,
            self.used
        )
    }
}

/// Reads the Router's configured model, tolerating the documented `default`
/// placeholder, and returns why the fallback was used when it is not a model.
pub fn router_model(
    config: &SuperAgentConfig,
    provider: &Provider,
    current: &ModelConfig,
) -> (ModelConfig, Option<String>) {
    let Some(configured) = config
        .router
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return (current.clone(), None);
    };
    if configured == "default" || configured == current.id {
        return (current.clone(), None);
    }
    match provider
        .models
        .iter()
        .find(|model| model.id == configured && model.enabled)
    {
        Some(model) => (model.clone(), None),
        None => (
            current.clone(),
            Some(format!(
                "Configured router model {configured} is not available; using {}",
                current.id
            )),
        ),
    }
}

/// Parses and validates a raw settings value. Kept separate from loading so a
/// test can prove rejection without a database.
pub fn parse(value: &serde_json::Value) -> Result<SuperAgentConfig> {
    let config: SuperAgentConfig =
        serde_json::from_value(value.clone()).context("Invalid superAgent settings")?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(id: &str, enabled: bool) -> ModelConfig {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "enabled": enabled,
            "supports_vision": false,
            "supports_web_search": false,
            "supports_tools": true,
            "supports_streaming": true
        }))
        .unwrap()
    }

    fn provider() -> Provider {
        serde_json::from_value(json!({
            "id": "fixture",
            "name": "Fixture",
            "base_url": "http://127.0.0.1",
            "api_key": "secret",
            "api_format": "openai",
            "models": [
                {"id":"router-model","name":"router-model","enabled":true,"supports_vision":false,"supports_web_search":false},
                {"id":"coder-model","name":"coder-model","enabled":true,"supports_vision":false,"supports_web_search":false}
            ],
            "enabled": true
        }))
        .unwrap()
    }

    #[test]
    fn defaults_match_the_documented_policy_and_are_valid() {
        let config = SuperAgentConfig::default();
        config.validate().unwrap();
        assert_eq!(config.approvals.plan, PlanApprovalPolicy::Manual);
        assert_eq!(config.review.max_rounds, 5);
        assert_eq!(config.scheduler.max_concurrent_pairs, 3);
        assert!(config.agents.coder.model.is_none());
    }

    #[test]
    fn settings_round_trip_and_limits_are_rejected() {
        let config = parse(&json!({
            "agents": {"coder": {"model": "coder-model"}, "reviewer": {}},
            "approvals": {"plan": "auto"},
            "review": {"maxRoundS": 9}
        }))
        .unwrap();
        assert_eq!(config.agents.coder.model.as_deref(), Some("coder-model"));
        assert_eq!(config.approvals.plan, PlanApprovalPolicy::Auto);

        for value in [
            json!({"review": {"maxRounds": 0}}),
            json!({"review": {"maxRounds": 21}}),
            json!({"scheduler": {"maxConcurrentPairs": 0}}),
            json!({"scheduler": {"maxSpawnedTasksPerTask": 21}}),
            json!({"agents": {"coder": {"model": "  "}}}),
            json!({"agents": {"coder": {"model": "bad\u{0}model"}}}),
        ] {
            assert!(parse(&value).is_err(), "accepted {value}");
        }
        assert!(parse(&json!({"approvals": {"plan": "sometimes"}})).is_err());
    }

    #[test]
    fn an_unconfigured_agent_model_falls_back_with_a_warning() {
        let config = parse(&json!({"agents": {"coder": {"model": "missing"}}})).unwrap();
        let fixture = provider();
        let (resolved, warning) = config.resolve_agent_model(
            AgentModelRole::Coder,
            &model("router-model", true),
            &fixture,
        );
        assert_eq!(resolved.id, "router-model");
        let warning = warning.unwrap();
        assert_eq!(warning.configured, "missing");
        assert!(warning.message().contains("using router-model"));
        assert!(!warning.message().contains("api_key"));

        let config = parse(&json!({"agents": {"coder": {"model": "coder-model"}}})).unwrap();
        let (resolved, warning) = config.resolve_agent_model(
            AgentModelRole::Coder,
            &model("router-model", true),
            &fixture,
        );
        assert_eq!(resolved.id, "coder-model");
        assert!(warning.is_none());

        // A disabled model is not usable either.
        let mut disabled = fixture;
        disabled.models[1].enabled = false;
        let (resolved, warning) = config.resolve_agent_model(
            AgentModelRole::Coder,
            &model("router-model", true),
            &disabled,
        );
        assert_eq!(resolved.id, "router-model");
        assert!(warning.is_some());
    }

    #[test]
    fn router_model_accepts_default_and_reports_missing_models() {
        let provider = provider();
        let current = model("router-model", true);
        let config = parse(&json!({"router": {"model": "default"}})).unwrap();
        assert_eq!(
            router_model(&config, &provider, &current).0.id,
            "router-model"
        );
        let config = parse(&json!({"router": {"model": "coder-model"}})).unwrap();
        assert_eq!(
            router_model(&config, &provider, &current).0.id,
            "coder-model"
        );
        let config = parse(&json!({"router": {"model": "missing"}})).unwrap();
        let (resolved, warning) = router_model(&config, &provider, &current);
        assert_eq!(resolved.id, "router-model");
        assert!(warning.unwrap().contains("missing"));
    }
}
