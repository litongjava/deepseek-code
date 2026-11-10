use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::timeout;

fn default_true() -> bool { true }

/// Only the documented trailing context selector is special; arbitrary custom
/// model IDs and other bracket suffixes remain opaque provider identifiers.
pub fn one_million_context_base(id: &str) -> Option<&str> {
    let end=id.len().checked_sub(4)?;
    if !id.get(end..)?.eq_ignore_ascii_case("[1m]") {return None;}
    let base=id[..end].trim_end();
    (!base.is_empty()).then_some(base)
}

pub fn resolve_model_selection(models:&[ModelConfig],id:&str)->Option<ModelConfig>{
    if let Some(model)=models.iter().find(|m|m.enabled&&m.id==id){return Some(model.clone().with_context_default());}
    let base=one_million_context_base(id)?;
    let mut model=models.iter().find(|m|m.enabled&&m.id==base)?.clone();
    model.context_window=Some(1_000_000);
    Some(model)
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RETRIES: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub api_format: ApiFormat,
    #[serde(default)]
    pub builtin_id: Option<String>,
    pub models: Vec<ModelConfig>,
    pub enabled: bool,
    pub web_search_strategy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApiFormat {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAI,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub max_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub supports_vision: bool,
    pub supports_web_search: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default = "default_true")]
    pub supports_text: bool,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(skip)]
    pub effort: Option<String>,
    #[serde(default)]
    pub fast: bool,
    /// Session request profile. The manager stamps every resolved model with it
    /// so all request paths agree; it is never persisted here.
    #[serde(skip)]
    pub wire_identity: super::wire_identity::WireIdentity,
}

impl ModelConfig {
    fn with_context_default(mut self) -> Self {
        self.context_window = self
            .context_window
            .or_else(|| super::token_counter::TokenCounter::default_context_window(&self.id));
        self
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider: Provider,
    pub model: ModelConfig,
}

#[derive(Debug, Clone)]
struct ProviderCacheEntry {
    pub provider: Provider,
    pub model: ModelConfig,
    pub cached_at: Instant,
}

#[derive(Debug)]
pub struct ProviderManager {
    providers: Vec<Provider>,
    config_path: PathBuf,
    resolve_cache: Mutex<HashMap<String, ProviderCacheEntry>>,
    cache_ttl: Duration,
    request_timeout: Duration,
    max_retries: u32,
    /// Applied to every resolved model, so one session setting reaches every
    /// request path, including summaries and connection tests.
    wire_identity: super::wire_identity::WireIdentity,
}

impl ProviderManager {
    pub fn new(config_path: PathBuf) -> Self {
        let mut manager = Self {
            providers: Vec::new(),
            config_path,
            resolve_cache: Mutex::new(HashMap::new()),
            cache_ttl: Duration::from_secs(300),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_retries: MAX_RETRIES,
            wire_identity: super::wire_identity::WireIdentity::Default,
        };
        manager.load();
        manager
    }

    pub fn from_providers(providers: Vec<Provider>, config_path: PathBuf) -> Self {
        let manager = Self::in_memory(providers, config_path);
        if let Err(e) = manager.save() {
            tracing::error!(module = "ProviderManager", "Failed to save synced providers: {}", e);
        }
        manager
    }

    /// Runtime credentials and CLI overrides must never be written to disk.
    pub fn in_memory(providers: Vec<Provider>, config_path: PathBuf) -> Self {
        Self {
            providers,
            config_path,
            resolve_cache: Mutex::new(HashMap::new()),
            cache_ttl: Duration::from_secs(300),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_retries: MAX_RETRIES,
            wire_identity: super::wire_identity::WireIdentity::Default,
        }
    }

    /// Select the request profile used by every model this manager resolves.
    /// Every resolution stamps the current value, so a cache hit cannot keep a
    /// stale profile.
    pub fn set_wire_identity(&mut self, identity: super::wire_identity::WireIdentity) {
        self.wire_identity = identity;
    }

    fn stamp(&self, mut resolved: ResolvedProvider) -> ResolvedProvider {
        resolved.model.wire_identity = self.wire_identity;
        resolved
    }

    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    pub fn get_request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn load(&mut self) {
        if self.config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&self.config_path) {
                if let Ok(providers) = serde_json::from_str::<Vec<Provider>>(&content) {
                    self.providers = providers;
                    self.invalidate_cache();
                    tracing::info!(module = "ProviderManager", "Loaded {} providers from {}", self.providers.len(), self.config_path.display());
                }
            }
        }
    }

    pub fn save(&self) -> Result<(), anyhow::Error> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&self.providers)?;
        std::fs::write(&self.config_path, content)?;
        Ok(())
    }

    fn invalidate_cache(&self) {
        tracing::info!(module = "ProviderManager", "Resolve cache invalidated (sync)");
    }

    pub async fn invalidate_cache_async(&self) {
        let mut guard = self.resolve_cache.lock().await;
        guard.clear();
        tracing::info!(module = "ProviderManager", "Resolve cache invalidated");
    }

    pub async fn resolve_provider(&self, model_id: &str) -> Option<ResolvedProvider> {
        let now = Instant::now();

        {
            let cache = self.resolve_cache.lock().await;
            if let Some(entry) = cache.get(model_id) {
                if now.duration_since(entry.cached_at) < self.cache_ttl {
                    tracing::info!(
                        module = "ProviderManager",
                        "Cache hit for \"{}\" (age: {:.1}s)",
                        model_id,
                        now.duration_since(entry.cached_at).as_secs_f64()
                    );
                    return Some(self.stamp(ResolvedProvider {
                        provider: entry.provider.clone(),
                        model: entry.model.clone(),
                    }));
                }
            }
        }

        tracing::info!(module = "ProviderManager", "Cache miss for \"{}\", resolving...", model_id);
        let result = self.resolve_provider_internal(model_id);

        if let Some(ref resolved) = result {
            let mut cache = self.resolve_cache.lock().await;
            cache.insert(
                model_id.to_string(),
                ProviderCacheEntry {
                    provider: resolved.provider.clone(),
                    model: resolved.model.clone(),
                    cached_at: Instant::now(),
                },
            );
        }

        result.map(|resolved| self.stamp(resolved))
    }

    pub fn resolve_selected(&self, provider_id: &str, model_id: &str) -> Option<ResolvedProvider> {
        let provider = self.providers.iter().find(|p| p.id == provider_id && p.enabled)?;
        let model = resolve_model_selection(&provider.models,model_id)?;
        Some(self.stamp(ResolvedProvider { provider: provider.clone(), model }))
    }

    fn resolve_provider_internal(&self, model_id: &str) -> Option<ResolvedProvider> {
        if let Some((provider, model)) = model_id.split_once("::") { return self.resolve_selected(provider, model); }
        let mut first_match: Option<ResolvedProvider> = None;
        
        let aliases: std::collections::HashMap<&str, Vec<&str>> = [
            ("deepseek-v4-pro", vec!["deepseek-chat"]),
            ("deepseek-v4-flash", vec!["deepseek-reasoner"]),
        ].iter().cloned().collect();
        
        let mut ids_to_try = vec![model_id];
        if let Some(alias_list) = aliases.get(model_id) {
            ids_to_try.extend(alias_list.iter().copied());
        }
        for (canonical, alias_list) in &aliases {
            if alias_list.contains(&model_id) {
                ids_to_try.push(canonical);
            }
        }
        
        for try_id in &ids_to_try {
            for provider in &self.providers {
                if !provider.enabled {
                    continue;
                }
                
                for model in &provider.models {
                    if model.id == *try_id && model.enabled {
                        if first_match.is_none() {
                            first_match = Some(ResolvedProvider {
                                provider: provider.clone(),
                                model: model.clone().with_context_default(),
                            });
                        } else {
                            return None;
                        }
                        break;
                    }
                }
                
            }
            if first_match.is_some() {
                break;
            }
        }

        if let Some(resolved) = &first_match {
            tracing::info!(
                module = "ProviderManager",
                "Resolved \"{}\" → \"{}\" ({})",
                model_id, resolved.provider.name, resolved.provider.base_url
            );
        } else {
            tracing::warn!(module = "ProviderManager", "No provider found for \"{}\"", model_id);
        }

        if first_match.is_none() {
            if let Some(base)=one_million_context_base(model_id) {
                return self.resolve_provider_internal(base).map(|mut resolved|{resolved.model.context_window=Some(1_000_000);resolved});
            }
        }
        first_match
    }

    pub async fn execute_with_retry<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last_error = None;
        
        for attempt in 0..self.max_retries {
            let result = timeout(self.request_timeout, operation()).await;
            
            match result {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(e)) => {
                    tracing::warn!(
                        module = "ProviderManager",
                        "Request failed (attempt {}/{}): {}",
                        attempt + 1,
                        self.max_retries,
                        e
                    );
                    last_error = Some(e);
                }
                Err(_) => {
                    tracing::warn!(
                        module = "ProviderManager",
                        "Request timeout after {}s (attempt {}/{})",
                        self.request_timeout.as_secs(),
                        attempt + 1,
                        self.max_retries,
                    );
                    last_error = Some(anyhow!("Request timed out"));
                }
            }

            if attempt < self.max_retries - 1 {
                let delay = RETRY_BASE_DELAY * 2u32.pow(attempt);
                tracing::info!(
                    module = "ProviderManager",
                    "Retrying in {:.1}s...",
                    delay.as_secs_f64()
                );
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("All retries exhausted")))
    }

    pub fn normalize_base_url(url: &str) -> String {
        let clean = url.trim_end_matches('/');
        let clean = clean
            .strip_suffix("/chat/completions")
            .or_else(|| clean.strip_suffix("/messages"))
            .or_else(|| clean.strip_suffix("/anthropic"))
            .or_else(|| clean.strip_suffix("/v1"))
            .unwrap_or(clean);
        clean.trim_end_matches('/').strip_suffix("/v1").unwrap_or(clean.trim_end_matches('/')).to_string()
    }

    pub fn list_providers(&self) -> &[Provider] {
        &self.providers
    }

    pub fn set_model_fast(&mut self, provider_id: &str, model_id: &str, enabled: bool) -> Result<()> {
        let provider = self.providers.iter_mut().find(|p| p.id == provider_id).ok_or_else(|| anyhow!("Provider not found"))?;
        let selected=resolve_model_selection(&provider.models,model_id).ok_or_else(||anyhow!("Model not found"))?;
        let model = provider.models.iter_mut().find(|m| m.id == selected.id).ok_or_else(|| anyhow!("Model not found"))?;
        model.fast = enabled;
        // resolve_selected reads the live provider list; discard other cached resolutions.
        if let Ok(mut cache) = self.resolve_cache.try_lock() { cache.clear(); }
        Ok(())
    }

    pub fn update_provider(&mut self, id: &str, provider: Provider) {
        if let Some(idx) = self.providers.iter().position(|p| p.id == id) {
            self.providers[idx] = provider;
            tracing::info!(module = "ProviderManager", "Updated provider: {}", id);
        } else {
            self.providers.push(provider);
            tracing::info!(module = "ProviderManager", "Added new provider: {}", id);
        }
        self.invalidate_cache();
        if let Err(e) = self.save() {
            tracing::error!(module = "ProviderManager", "Failed to save providers: {}", e);
        } else {
            tracing::info!(module = "ProviderManager", "Providers saved successfully to {}", self.config_path.display());
        }
    }

    pub fn delete_provider(&mut self, id: &str) {
        self.providers.retain(|p| p.id != id);
        tracing::info!(module = "ProviderManager", "Deleted provider: {}", id);
        self.invalidate_cache();
        if let Err(e) = self.save() {
            tracing::error!(module = "ProviderManager", "Failed to save providers after deletion: {}", e);
        } else {
            tracing::info!(module = "ProviderManager", "Providers saved successfully after deletion");
        }
    }

    pub async fn get_cache_stats(&self) -> HashMap<String, String> {
        let cache = self.resolve_cache.lock().await;
        let mut stats = HashMap::new();
        stats.insert("cache_size".to_string(), cache.len().to_string());
        stats.insert(
            "cache_ttl".to_string(),
            format!("{}s", self.cache_ttl.as_secs()),
        );
        stats
    }
}

#[cfg(test)]
mod provider_selection_tests {
    use super::*;

    #[test]
    fn astra_context_defaults_apply_to_saved_models_and_preserve_overrides() {
        for configured in [None, Some(128_000)] {
            let provider: Provider = serde_json::from_value(serde_json::json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"","api_format":"anthropic","enabled":true,"models":[{"id":"gpt-6-astra","name":"Astra","enabled":true,"supports_web_search":false,"context_window":configured}]})).unwrap();
            let manager = ProviderManager::in_memory(
                vec![provider],
                std::env::temp_dir().join("astra-context-fixture.json"),
            );
            let expected = configured.or(Some(256_000));
            assert_eq!(
                manager
                    .resolve_selected("p", "gpt-6-astra")
                    .unwrap()
                    .model
                    .context_window,
                expected
            );
            assert_eq!(
                manager
                    .resolve_provider_internal("gpt-6-astra")
                    .unwrap()
                    .model
                    .context_window,
                expected
            );
            assert_eq!(
                manager
                    .resolve_provider_internal("p::gpt-6-astra")
                    .unwrap()
                    .model
                    .context_window,
                expected
            );
            assert_eq!(manager.providers[0].models[0].context_window, configured);
            assert_eq!(
                manager
                    .resolve_selected("p", "gpt-6-astra[1m]")
                    .unwrap()
                    .model
                    .context_window,
                Some(1_000_000)
            );
        }
    }

    #[test]
    fn provider_selection_is_exact_and_rejects_ambiguity() {
        let make = |id: &str| serde_json::from_value::<Provider>(serde_json::json!({"id":id,"name":id,"base_url":"http://localhost","api_key":"","api_format":"openai","enabled":true,"models":[{"id":"same","name":"Same","enabled":true,"max_tokens":32000,"supports_vision":false,"supports_web_search":false}]})).unwrap();
        let mut manager = ProviderManager::new(std::env::temp_dir().join(format!("absent-{}",uuid::Uuid::new_v4())));
        manager.providers = vec![make("a"),make("b")];
        assert!(manager.resolve_provider_internal("same").is_none());
        assert_eq!(manager.resolve_selected("b","same").unwrap().provider.id,"b");
        manager.providers[1].enabled = false;
        assert!(manager.resolve_selected("b","same").is_none());
        assert_eq!(manager.resolve_provider_internal("same").unwrap().provider.id,"a");
    }

    #[test]
    fn context_suffix_resolves_base_preserving_exact_custom_ids(){
        let provider:Provider=serde_json::from_value(serde_json::json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"","api_format":"openai","enabled":true,"models":[{"id":"deepseek-v4.1-flash","name":"Flash","enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":128000}]})).unwrap();
        let mut manager=ProviderManager::in_memory(vec![provider],std::env::temp_dir().join("suffix-fixture.json"));
        for selection in ["deepseek-v4.1-flash[1m]","deepseek-v4.1-flash[1M]"]{
            let model=manager.resolve_selected("p",selection).unwrap().model;
            assert_eq!(model.id,"deepseek-v4.1-flash");assert_eq!(model.context_window,Some(1_000_000));
            assert_eq!(manager.resolve_provider_internal(&format!("p::{selection}")).unwrap().model.context_window,Some(1_000_000));
        }
        assert_eq!(manager.resolve_selected("p","deepseek-v4.1-flash").unwrap().model.context_window,Some(128000));
        for opaque in ["deepseek-v4.1-flash[2m]","deepseek-v4.1-flash[thinking]","deepseek-v4.1-flash[1m]-custom","[1m]"]{assert!(manager.resolve_selected("p",opaque).is_none());}
        let mut exact=manager.providers[0].models[0].clone();exact.id="deepseek-v4.1-flash[1m]".into();exact.context_window=Some(42);
        manager.providers[0].models.push(exact);
        let model=manager.resolve_selected("p","deepseek-v4.1-flash[1m]").unwrap().model;
        assert_eq!(model.id,"deepseek-v4.1-flash[1m]");assert_eq!(model.context_window,Some(42));
        manager.set_model_fast("p","deepseek-v4.1-flash[1M]",true).unwrap();
        assert!(manager.resolve_selected("p","deepseek-v4.1-flash").unwrap().model.fast);
    }
}
