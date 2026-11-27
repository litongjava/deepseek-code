//! User-owned configuration for the Rust automatic permission extension.
use crate::{
    native_engine::provider_manager::{ApiFormat, Provider, ResolvedProvider},
    services::ServiceContext,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub const KEY: &str = "permissions.auto";

#[derive(Clone, Debug, Default, Serialize)]
pub struct AutoStats {
    pub requests: u64,
    pub denied_consecutive: u32,
    pub denied_total: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub unknown_usage_requests: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClassifierSelection {
    Current,
    Named {
        provider_id: String,
        model_id: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    Deny,
    #[default]
    Manual,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AutoConfig {
    pub enabled: bool,
    pub classifier: Option<ClassifierSelection>,
    pub timeout_ms: u64,
    pub max_input_tokens: u32,
    pub failure_policy: FailurePolicy,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            classifier: None,
            timeout_ms: 15_000,
            max_input_tokens: 8192,
            failure_policy: FailurePolicy::Manual,
        }
    }
}

impl AutoConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (100..=60_000).contains(&self.timeout_ms),
            "Auto timeout_ms must be 100..60000"
        );
        ensure!(
            (256..=32_768).contains(&self.max_input_tokens),
            "Auto max_input_tokens must be 256..32768"
        );
        ensure!(
            !self.enabled || self.classifier.is_some(),
            "Auto requires an explicit classifier selection (named or current)"
        );
        if let Some(ClassifierSelection::Named {
            provider_id,
            model_id,
        }) = &self.classifier
        {
            ensure!(
                !provider_id.trim().is_empty() && !model_id.trim().is_empty(),
                "Auto classifier requires nonempty provider_id and model_id"
            );
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        providers: &[Provider],
        current: &ResolvedProvider,
    ) -> Result<ResolvedProvider> {
        self.validate()?;
        ensure!(
            self.enabled,
            "Auto permissions are disabled; configure permissions.auto explicitly"
        );
        let mut selected = match self
            .classifier
            .as_ref()
            .context("Auto classifier selection missing")?
        {
            ClassifierSelection::Current => current.clone(),
            ClassifierSelection::Named {
                provider_id,
                model_id,
            } => {
                let provider = providers
                    .iter()
                    .find(|p| p.id == *provider_id && p.enabled)
                    .context("Auto classifier provider unavailable")?;
                let model = provider
                    .models
                    .iter()
                    .find(|m| m.id == *model_id && m.enabled)
                    .context("Auto classifier model unavailable")?;
                ResolvedProvider {
                    provider: provider.clone(),
                    model: model.clone(),
                }
            }
        };
        ensure!(
            selected.provider.enabled && selected.model.enabled && selected.model.supports_text,
            "Auto classifier requires an enabled text model"
        );
        // Native direct adapters, plus the two Responses routes the classifier
        // can now speak. Other catalog transports still need an explicit opt-in.
        let api = selected.model.api.as_deref();
        let native = selected.provider.builtin_id.is_none()
            && match selected.provider.api_format {
                ApiFormat::OpenAI => matches!(
                    api,
                    None | Some("openai-completions" | "openai-responses")
                ),
                ApiFormat::Anthropic => matches!(api, None | Some("anthropic-messages")),
            };
        let builtin_responses = selected.provider.builtin_id.as_deref() == Some("openai")
            && selected.provider.api_format == ApiFormat::OpenAI
            && api == Some("openai-responses");
        ensure!(
            native || builtin_responses,
            "Auto classifier requires a native Anthropic or OpenAI-compatible provider, \
             or OpenAI Responses; this provider-specific transport is not supported"
        );
        ensure!(
            !selected.provider.api_key.trim().is_empty(),
            "Auto classifier credentials unavailable"
        );
        selected.model.effort = None;
        selected.model.fast = false;
        Ok(selected)
    }
}

pub fn load<D:crate::db::session_store::GlobalPreferences>(context: &ServiceContext<D>) -> Result<AutoConfig> {
    let config = context.db.global_preference::<AutoConfig>(crate::db::session_store::GlobalSetting::MachinePreference(KEY))?.unwrap_or_default();
    config.validate()?;
    Ok(config)
}

pub fn save<D:crate::db::session_store::GlobalPreferences>(context: &ServiceContext<D>, config: &AutoConfig) -> Result<()> {
    config.validate()?;
    context.db.save_global_preference(crate::db::session_store::GlobalSetting::MachinePreference(KEY), config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolved(builtin: Option<&str>, api_format: &str, api: Option<&str>) -> ResolvedProvider {
        let provider: Provider = serde_json::from_value(json!({
            "id":"classifier",
            "name":"Classifier",
            "base_url":"http://127.0.0.1:1",
            "api_key":"fixture-only",
            "api_format":api_format,
            "builtin_id":builtin,
            "enabled":true,
            "models":[{
                "id":"classifier-model",
                "name":"Classifier model",
                "enabled":true,
                "supports_vision":false,
                "supports_web_search":false,
                "supports_text":true,
                "api":api
            }]
        }))
        .unwrap();
        ResolvedProvider {
            model: provider.models[0].clone(),
            provider,
        }
    }

    fn config() -> AutoConfig {
        AutoConfig {
            enabled: true,
            classifier: Some(ClassifierSelection::Current),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_accepts_native_adapters_and_openai_responses() {
        for (builtin, format, api) in [
            (None, "openai", None),
            (None, "openai", Some("openai-completions")),
            (None, "openai", Some("openai-responses")),
            (None, "anthropic", None),
            (None, "anthropic", Some("anthropic-messages")),
            (Some("openai"), "openai", Some("openai-responses")),
        ] {
            let current = resolved(builtin, format, api);
            assert!(
                config().resolve(&[], &current).is_ok(),
                "expected {builtin:?}/{format}/{api:?} to be supported"
            );
        }
    }

    #[test]
    fn resolve_rejects_other_catalog_transports() {
        for (builtin, format, api) in [
            (Some("openai"), "openai", None),
            (Some("openai"), "openai", Some("openai-completions")),
            (Some("google"), "openai", Some("openai-responses")),
            (Some("amazon-bedrock"), "openai", Some("bedrock-converse-stream")),
            (None, "openai", Some("google-generative-ai")),
            (None, "anthropic", Some("openai-responses")),
        ] {
            let current = resolved(builtin, format, api);
            assert!(
                config().resolve(&[], &current).is_err(),
                "expected {builtin:?}/{format}/{api:?} to be rejected"
            );
        }
    }

    #[test]
    fn config_rejects_unknown_fields_missing_selection_and_unsafe_limits() {
        assert!(
            serde_json::from_value::<AutoConfig>(serde_json::json!({"api_key":"secret"})).is_err()
        );
        assert!(AutoConfig {
            enabled: true,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(AutoConfig {
            timeout_ms: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(AutoConfig {
            max_input_tokens: u32::MAX,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert_eq!(AutoConfig::default().failure_policy, FailurePolicy::Manual);
    }
}
