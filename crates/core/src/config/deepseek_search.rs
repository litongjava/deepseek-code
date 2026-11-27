//! Independent credentials and endpoint for the DeepSeek native search service.
use crate::native_engine::provider_manager::{ApiFormat, ModelConfig, Provider, ResolvedProvider};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeepSeekSearchConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

impl std::fmt::Debug for DeepSeekSearchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeekSearchConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("has_api_key", &!self.api_key.is_empty())
            .finish()
    }
}

impl Default for DeepSeekSearchConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/anthropic/v1".into(),
            api_key: String::new(),
            model: "deepseek-flash".into(),
            provider: None,
        }
    }
}

impl DeepSeekSearchConfig {
    /// The configured provider, or the one implied by the endpoint. A custom
    /// gateway stays on DeepSeek, which keeps configurations without
    /// `provider` working exactly as before.
    pub fn effective_provider(&self) -> &'static str {
        if let Some(provider) = self.provider.as_deref() {
            match provider.trim().to_ascii_lowercase().as_str() {
                "openai" => return "openai",
                "anthropic" | "claude" => return "anthropic",
                "deepseek" => return "deepseek",
                _ => {}
            }
        }
        match reqwest::Url::parse(self.base_url.trim())
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .as_deref()
        {
            Some("api.openai.com") => "openai",
            Some("api.anthropic.com") => "anthropic",
            _ => "deepseek",
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(provider) = self.provider.as_deref() {
            let provider = provider.trim().to_ascii_lowercase();
            if !matches!(provider.as_str(), "" | "deepseek" | "openai" | "anthropic") {
                bail!(crate::config::ui_language::text(
                    "The search provider must be deepseek, openai or anthropic",
                    "搜索提供商必须是 deepseek、openai 或 anthropic"
                ));
            }
        }
        let url = reqwest::Url::parse(self.base_url.trim())
            .map_err(|_| anyhow::anyhow!(crate::config::ui_language::text("The search API base URL must be a valid HTTP(S) URL", "搜索 API 地址必须是有效的 HTTP(S) URL")))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(crate::config::ui_language::text("The search API base URL must use HTTP(S) without credentials, query parameters, or fragments", "搜索 API 地址必须使用 HTTP(S)，且不能包含凭据、查询参数或片段"));
        }
        if self.base_url.len() > 4096 {
            bail!(crate::config::ui_language::text("The search API base URL is too long", "搜索 API 地址过长"));
        }
        if self.model.trim().is_empty()
            || self.model.len() > 256
            || self.model.chars().any(char::is_whitespace)
            || self.model.chars().any(char::is_control)
        {
            bail!(crate::config::ui_language::text("The search model must be nonempty and contain no whitespace or control characters", "搜索模型不能为空，且不能包含空白或控制字符"));
        }
        if self.api_key.len() > 8192 || self.api_key.chars().any(char::is_control) {
            bail!(crate::config::ui_language::text("Invalid search API key format", "搜索 API Key 格式无效"));
        }
        Ok(())
    }

    pub fn resolved_provider(&self) -> Result<ResolvedProvider> {
        let provider = self.effective_provider();
        if self.api_key.trim().is_empty() {
            let message = match provider {
                "openai" => crate::config::ui_language::text("Configure an OpenAI API key, endpoint, and model in Settings > Web Search or with /search-config.", "请在设置 → 联网搜索或使用 /search-config 配置 OpenAI 密钥、地址和模型。").to_owned(),
                "anthropic" => crate::config::ui_language::text("Configure an Anthropic API key, endpoint, and model in Settings > Web Search or with /search-config.", "请在设置 → 联网搜索或使用 /search-config 配置 Anthropic 密钥、地址和模型。").to_owned(),
                _ => crate::config::ui_language::text("Configure a DeepSeek API key, endpoint, and model in Settings > Web Search. Leave search settings at their defaults to use the current DeepSeek provider.", "请在设置 → 联网搜索中配置 DeepSeek 密钥、地址和模型。保持默认且未配置搜索密钥时使用当前 DeepSeek 供应商。").to_owned(),
            };
            bail!("{message}");
        }
        self.validate()?;
        let (id, name, api_format, model_api, strategy) = match provider {
            "openai" => (
                "openai-search",
                "OpenAI Search",
                ApiFormat::OpenAI,
                "openai-responses",
                "openai_native",
            ),
            "anthropic" => (
                "anthropic-search",
                "Anthropic Search",
                ApiFormat::Anthropic,
                "anthropic-messages",
                "anthropic_native",
            ),
            _ => (
                "deepseek-search",
                "DeepSeek Search",
                ApiFormat::Anthropic,
                "anthropic-messages",
                "deepseek_native",
            ),
        };
        let model = ModelConfig {
            id: self.model.trim().into(),
            name: self.model.trim().into(),
            enabled: true,
            max_tokens: Some(2048),
            supports_vision: true,
            supports_web_search: true,
            context_window: None,
            supports_text: true,
            api: Some(model_api.into()),
            effort: None,
            fast: false,
        };
        Ok(ResolvedProvider {
            provider: Provider {
                lease: None,
                id: id.into(),
                name: name.into(),
                base_url: self.base_url.trim().trim_end_matches('/').into(),
                api_key: self.api_key.trim().into(),
                api_format,
                // Deliberately absent: no chat or environment credential fallback.
                builtin_id: None,
                models: vec![model.clone()],
                enabled: true,
                web_search_strategy: Some(strategy.into()),
            },
            model,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_config_gets_search_defaults_and_independent_key_is_required() {
        let config: crate::config::AppConfig = serde_json::from_str(r#"{"providers":[]}"#).unwrap();
        assert_eq!(config.deepseek_search.model, "deepseek-flash");
        assert!(config
            .deepseek_search
            .resolved_provider()
            .unwrap_err()
            .to_string()
            .contains("DeepSeek"));
        let mut search = config.deepseek_search;
        search.api_key = "independent-search-key".into();
        search.base_url = "https://search.example/anthropic/v1".into();
        search.model = "chosen-search-model".into();
        let resolved = search.resolved_provider().unwrap();
        assert_eq!(resolved.provider.id, "deepseek-search");
        assert_eq!(resolved.provider.api_key, "independent-search-key");
        assert_eq!(resolved.provider.base_url, search.base_url);
        assert_eq!(resolved.model.id, "chosen-search-model");
        assert!(resolved.provider.builtin_id.is_none());
        assert!(!format!("{search:?}").contains("independent-search-key"));
    }

    #[test]
    fn config_without_provider_keeps_the_deepseek_route() {
        let search: super::DeepSeekSearchConfig = serde_json::from_str(
            r#"{"base_url":"https://search.example/anthropic/v1","api_key":"key","model":"model"}"#,
        )
        .unwrap();
        assert!(search.provider.is_none());
        assert_eq!(search.effective_provider(), "deepseek");
        let resolved = search.resolved_provider().unwrap();
        assert_eq!(resolved.provider.id, "deepseek-search");
        assert_eq!(
            resolved.provider.api_format,
            crate::native_engine::provider_manager::ApiFormat::Anthropic
        );
        assert_eq!(
            resolved.provider.web_search_strategy.as_deref(),
            Some("deepseek_native")
        );
        assert_eq!(resolved.model.api.as_deref(), Some("anthropic-messages"));
    }

    #[test]
    fn explicit_provider_selects_its_protocol_and_strategy() {
        let mut search = super::DeepSeekSearchConfig::default();
        search.api_key = "key".into();
        search.base_url = "https://api.openai.com/v1".into();
        search.provider = Some("openai".into());
        let resolved = search.resolved_provider().unwrap();
        assert_eq!(resolved.provider.id, "openai-search");
        assert_eq!(resolved.provider.name, "OpenAI Search");
        assert_eq!(
            resolved.provider.api_format,
            crate::native_engine::provider_manager::ApiFormat::OpenAI
        );
        assert_eq!(
            resolved.provider.web_search_strategy.as_deref(),
            Some("openai_native")
        );
        assert_eq!(resolved.model.api.as_deref(), Some("openai-responses"));

        // The explicit provider wins over the URL's host.
        search.base_url = "https://gateway.example/v1".into();
        search.provider = Some("anthropic".into());
        let resolved = search.resolved_provider().unwrap();
        assert_eq!(resolved.provider.id, "anthropic-search");
        assert_eq!(resolved.provider.name, "Anthropic Search");
        assert_eq!(
            resolved.provider.api_format,
            crate::native_engine::provider_manager::ApiFormat::Anthropic
        );
        assert_eq!(
            resolved.provider.web_search_strategy.as_deref(),
            Some("anthropic_native")
        );
        assert_eq!(resolved.model.api.as_deref(), Some("anthropic-messages"));
    }

    #[test]
    fn provider_is_inferred_from_the_base_url_when_unset() {
        for (base_url, expected) in [
            ("https://api.openai.com/v1", "openai"),
            ("https://api.anthropic.com", "anthropic"),
            ("https://api.deepseek.com/anthropic/v1", "deepseek"),
            ("https://search.example/anthropic/v1", "deepseek"),
        ] {
            let mut search = super::DeepSeekSearchConfig::default();
            search.base_url = base_url.into();
            assert_eq!(search.effective_provider(), expected, "{base_url}");
        }
        let mut search = super::DeepSeekSearchConfig::default();
        search.provider = Some("  Claude  ".into());
        assert_eq!(search.effective_provider(), "anthropic");
    }

    #[test]
    fn invalid_provider_is_rejected() {
        let mut search = super::DeepSeekSearchConfig::default();
        search.provider = Some("gemini".into());
        let error = search.validate().unwrap_err().to_string();
        assert!(error.contains("deepseek"), "{error}");
    }
}
