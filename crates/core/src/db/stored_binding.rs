//! Provider IDs and model IDs occupy separate database columns. Only Desktop
//! picker input and corroborated legacy rows use an encoded provider prefix.
use anyhow::{ensure, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBinding {
    pub provider: Option<String>,
    pub model: String,
}

impl StoredBinding {
    pub fn from_row(provider: Option<String>, model: String) -> Self {
        Self { provider, model }
    }

    /// Inverse of modelKey / splitModelKey in src/utils/modelConfig.ts.
    pub fn desktop_selection(key: &str) -> Result<Self> {
        let Some((encoded, model)) = key.split_once("::") else {
            return Ok(Self::from_row(None, key.to_owned()));
        };
        // decodeURIComponent rejects incomplete escapes and malformed UTF-8.
        let bytes = encoded.as_bytes();
        for (i, byte) in bytes.iter().enumerate() {
            if *byte == b'%' {
                ensure!(
                    bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                        && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit),
                    "Invalid provider escape in Desktop model selection"
                );
            }
        }
        let provider = urlencoding::decode(encoded)?.into_owned();
        ensure!(
            !provider.is_empty(),
            "Missing provider in Desktop model selection"
        );
        Ok(Self::from_row(Some(provider), model.to_owned()))
    }

    pub fn desktop_key(provider: &str, model: &str) -> String {
        // encodeURIComponent also leaves these five ASCII characters unescaped.
        let encoded = urlencoding::encode(provider)
            .replace("%21", "!")
            .replace("%27", "'")
            .replace("%28", "(")
            .replace("%29", ")")
            .replace("%2A", "*");
        format!("{encoded}::{model}")
    }

    /// Resolve against one provider. An old NULL-provider row needs independent
    /// turn evidence; the apparent prefix can never select a destination.
    pub async fn resolve_pinned(
        self,
        recorded: Option<String>,
        engine: &crate::native_engine::NativeEngine,
    ) -> Result<(
        crate::native_engine::provider_manager::ResolvedProvider,
        String,
    )> {
        const PICK: &str = "Pick a model in Desktop before compacting this conversation";
        ensure!(!self.model.is_empty(), "{PICK}");
        let legacy = self.provider.is_none();
        let id = self
            .provider
            .or(recorded)
            .ok_or_else(|| anyhow::anyhow!(PICK))?;
        let picker = if legacy {
            Self::desktop_selection(&self.model).ok()
        } else {
            None
        };
        if let Some(prefix) = picker.as_ref().and_then(|key| key.provider.as_deref()) {
            if prefix != id {
                ensure!(!engine.has_configured_provider(prefix).await, "{PICK}");
            }
        }
        let raw = engine.resolved_provider(&id, &self.model).await.ok();
        if let Some(key) = picker.filter(|key| key.provider.as_ref() == Some(&id)) {
            if let Ok(qualified) = engine.resolved_provider(&id, &key.model).await {
                ensure!(raw.is_none(), "Ambiguous legacy model selection. {PICK}");
                return Ok((qualified, key.model));
            }
        }
        let selected = raw.ok_or_else(|| {
            anyhow::anyhow!(
                "provider {id} is unavailable for model {}. {PICK}",
                self.model
            )
        })?;
        Ok((selected, self.model))
    }
}

impl super::conversation_repo::ConversationRow {
    /// Encode the selection only when serializing it for Desktop.
    pub fn with_desktop_model_key(mut self) -> Self {
        if let (Some(provider), Some(model)) = (&self.provider, &self.model) {
            self.model = Some(StoredBinding::desktop_key(provider, model));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_selection_matches_encode_uri_component_and_preserves_model_bytes() {
        let provider = "a/b::研发 +%!'()*~";
        let model = "raw%2F::model[1m]";
        let key = "a%2Fb%3A%3A%E7%A0%94%E5%8F%91%20%2B%25!'()*~::raw%2F::model[1m]";
        assert_eq!(StoredBinding::desktop_key(provider, model), key);
        assert_eq!(
            StoredBinding::desktop_selection(key).unwrap(),
            StoredBinding::from_row(Some(provider.into()), model.into())
        );
        for malformed in ["missing%::m", "%GG::m", "%FF::m"] {
            assert!(StoredBinding::desktop_selection(malformed).is_err());
        }
    }
}
