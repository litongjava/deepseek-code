use serde_json::Value;
use std::collections::HashMap;

pub struct TokenCounter;

impl TokenCounter {
    /// Shared defaults for optional model windows; configured values take priority.
    pub fn default_context_window(model_id: &str) -> Option<u32> {
        let model_id = model_id.rsplit("::").next().unwrap_or(model_id);
        let model_id = model_id.rsplit('/').next().unwrap_or(model_id);
        let model_id = model_id.strip_suffix(":batch").unwrap_or(model_id);
        model_id
            .eq_ignore_ascii_case("gpt-6-astra")
            .then_some(256_000)
    }

    /// Use transport accounting, including structured tool arguments/results.
    pub fn estimate(messages: &[Value]) -> usize {
        super::request_context::ContextUsage::from_body(
            "openai-completions", &serde_json::json!({"messages": messages}), None, 0,
        ).history.estimated_tokens.min(usize::MAX as u64) as usize
    }

    /// Estimate tokens for a single message
    pub fn estimate_message(msg: &Value) -> usize {
        Self::estimate(std::slice::from_ref(msg))
    }

    /// Check if token count exceeds threshold
    pub fn exceeds_threshold(messages: &[Value], threshold: usize) -> bool {
        Self::estimate(messages) > threshold
    }

    /// Get maximum context window size for common models
    pub fn get_model_context_limit(model_id: &str) -> usize {
        if let Some(limit) = Self::default_context_window(model_id) {
            return limit as usize;
        }
        let mut limits = HashMap::new();
        
        // OpenAI models
        limits.insert("gpt-4o", 128000);
        limits.insert("gpt-4o-2024-08-06", 128000);
        limits.insert("gpt-4o-2024-11-20", 128000);
        limits.insert("gpt-4o-mini", 128000);
        limits.insert("gpt-4-turbo", 128000);
        limits.insert("gpt-4-turbo-2024-04-09", 128000);
        limits.insert("gpt-4", 8192);
        limits.insert("gpt-4-32k", 32768);
        limits.insert("gpt-3.5-turbo", 16384);
        limits.insert("gpt-3.5-turbo-16k", 16384);
        
        // Anthropic models
        limits.insert("claude-3-5-sonnet-20241022", 200000);
        limits.insert("claude-3-opus-20240229", 200000);
        limits.insert("claude-3-sonnet-20240229", 200000);
        limits.insert("claude-3-haiku-20240307", 200000);
        limits.insert("claude-2.1", 200000);
        limits.insert("claude-2", 100000);
        
        // DeepSeek models
        limits.insert("deepseek-chat", 128000);
        limits.insert("deepseek-coder", 128000);
        
        // Default fallback for unknown models - use a conservative limit
        let default_limit = 80000;
        
        // Check if model_id contains any of the known patterns
        for (pattern, &limit) in &limits {
            if model_id.contains(pattern) {
                return limit;
            }
        }
        
        default_limit
    }

    /// Check if messages exceed a model's context limit, with safety margin
    pub fn exceeds_model_limit(messages: &[Value], model_id: &str, safety_margin: f64) -> bool {
        let model_limit = Self::get_model_context_limit(model_id);
        let estimated_tokens = Self::estimate(messages);
        let safe_limit = (model_limit as f64 * safety_margin) as usize;
        
        estimated_tokens > safe_limit
    }

    /// Calculate safe token limit for a model (with safety margin)
    pub fn get_safe_limit(model_id: &str, safety_margin: f64) -> usize {
        let model_limit = Self::get_model_context_limit(model_id);
        (model_limit as f64 * safety_margin) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn astra_context_budget_uses_256k_for_standard_model_ids() {
        for id in [
            "gpt-6-astra",
            "GPT-6-Astra",
            "openai/gpt-6-astra",
            "proxy::gpt-6-astra",
            "openai/gpt-6-astra:batch",
        ] {
            assert_eq!(TokenCounter::get_model_context_limit(id), 256_000);
            assert_eq!(TokenCounter::get_safe_limit(id, 0.9), 230_400);
        }
        assert_eq!(
            TokenCounter::default_context_window("gpt-6-astra-pro"),
            None
        );
        assert_eq!(
            TokenCounter::default_context_window("gpt-6-astra-fast"),
            None
        );
        assert_eq!(
            TokenCounter::get_model_context_limit("unknown-model"),
            80_000
        );
    }
}
