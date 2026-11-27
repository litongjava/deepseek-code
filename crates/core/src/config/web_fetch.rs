//! WebFetch transport settings. The proxy is configuration-only: model input
//! never chooses where a page request is routed.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WebFetchConfig {
    pub proxy: Option<String>,
}

impl WebFetchConfig {
    /// The configured proxy without surrounding whitespace; blank means none.
    pub fn normalized_proxy(&self) -> Option<&str> {
        self.proxy
            .as_deref()
            .map(str::trim)
            .filter(|proxy| !proxy.is_empty())
    }

    /// Set or clear the proxy, keeping the previous value when validation fails.
    pub fn set_proxy(&mut self, value: &str) -> Result<()> {
        let trimmed = value.trim();
        if trimmed.is_empty()
            || matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "none" | "off" | "clear"
            )
        {
            self.proxy = None;
            return Ok(());
        }
        let candidate = Self {
            proxy: Some(trimmed.to_owned()),
        };
        candidate.validate()?;
        self.proxy = candidate.proxy;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let Some(proxy) = self.normalized_proxy() else {
            return Ok(());
        };
        if proxy.len() > 2048 {
            bail!("The WebFetch proxy URL is too long");
        }
        if proxy.chars().any(|c| c.is_control() || c.is_whitespace()) {
            bail!("The WebFetch proxy URL must not contain whitespace or control characters");
        }
        let url = reqwest::Url::parse(proxy).map_err(|_| {
            anyhow::anyhow!("The WebFetch proxy must be a valid URL such as http://127.0.0.1:10808")
        })?;
        if url.host_str().is_none() {
            bail!("The WebFetch proxy URL must include a host");
        }
        match url.scheme() {
            "http" | "https" => Ok(()),
            scheme @ ("socks5" | "socks5h" | "socks4") => bail!(
                "The WebFetch proxy scheme {scheme}:// is not supported; use an HTTP proxy address such as http://127.0.0.1:10808"
            ),
            scheme => bail!(
                "The WebFetch proxy scheme {scheme}:// is not supported; use http:// or https://, for example http://127.0.0.1:10808"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_no_proxy() {
        let config = WebFetchConfig::default();
        assert_eq!(config, WebFetchConfig { proxy: None });
        assert_eq!(config.normalized_proxy(), None);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn old_config_deserializes_with_web_fetch_defaults() {
        let config: crate::config::AppConfig = serde_json::from_str(r#"{"providers":[]}"#).unwrap();
        assert_eq!(config.web_fetch, WebFetchConfig::default());
        assert_eq!(config.web_fetch.normalized_proxy(), None);
    }

    #[test]
    fn set_proxy_trims_clears_and_keeps_the_previous_value_on_error() {
        let mut config = WebFetchConfig::default();
        config.set_proxy("  http://127.0.0.1:10808  ").unwrap();
        assert_eq!(config.normalized_proxy(), Some("http://127.0.0.1:10808"));

        assert!(config.set_proxy("socks5://127.0.0.1:1080").is_err());
        assert_eq!(
            config.normalized_proxy(),
            Some("http://127.0.0.1:10808"),
            "a rejected value must not discard the saved proxy"
        );

        for clear in ["", "   ", "none", "OFF", "Clear"] {
            config.set_proxy(clear).unwrap();
            assert_eq!(config.proxy, None, "{clear:?}");
        }
    }

    #[test]
    fn accepts_http_loopback_and_rejects_socks() {
        let config = WebFetchConfig {
            proxy: Some("http://127.0.0.1:10808".into()),
        };
        assert!(config.validate().is_ok());
        assert!(WebFetchConfig {
            proxy: Some("https://proxy.example:8443".into()),
        }
        .validate()
        .is_ok());
        for proxy in [
            "socks5://127.0.0.1:1080",
            "socks5h://127.0.0.1:1080",
            "socks4://127.0.0.1:1080",
        ] {
            let error = WebFetchConfig {
                proxy: Some(proxy.into()),
            }
            .validate()
            .unwrap_err()
            .to_string();
            assert!(error.contains("socks"), "{error}");
            assert!(error.contains("http://127.0.0.1:10808"), "{error}");
        }
        for proxy in [
            "http://",
            "not a url",
            "http://exa mple.com",
            "http://exa\nmple.com",
        ] {
            assert!(
                WebFetchConfig {
                    proxy: Some(proxy.into())
                }
                .validate()
                .is_err(),
                "{proxy:?}"
            );
        }
        assert!(WebFetchConfig {
            proxy: Some(format!("http://{}/", "a".repeat(2050))),
        }
        .validate()
        .is_err());
    }
}
