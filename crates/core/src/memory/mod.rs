//! MemEx - Infinite Memory Engine for DeepSeek Code (Tauri)
//!
//! Provides: local semantic search, auto-ingest, context compression
//! Backend: persistent local SQLite, retained for the legacy archive,
//! migration, export and import. Runtime memory is file-based.
//!
//! Also includes Caveman + RTK (Recursive Token Keeping) compression
//! and RLM (Reinforcement Learning from Memory) system

mod caveman_rtk;
mod local;
pub(crate) use local::search_terms;
/// Scoped file storage primitives; runtime policy lives in services::memory_files.
pub mod files;
mod transfer;
pub mod dream;
pub use transfer::{MemoryArchive, MemoryImportReport};
pub use caveman_rtk::{CavemanRTKStats, CavemanRTKSystem, RTKConfig, TokenImportance};

use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::info;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Data Types
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub content: String,
    pub importance: f64,
    pub created_at: String,
    pub metadata: Option<HashMap<String, String>>,
    pub similarity_score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchRequest {
    pub query: String,
    pub top_k: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryIngestRequest {
    pub content: String,
    pub importance: Option<f64>,
    pub metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_memories: usize,
    pub total_tokens_approx: usize,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub search_top_k: usize,
    pub compression_threshold_tokens: usize,
    pub min_importance_threshold: f64,
}

impl MemoryConfig {
    pub fn validate(&self) -> Result<()> {
        if !(1..=50).contains(&self.search_top_k) { return Err(anyhow!("Search result count must be between 1 and 50")); }
        if !self.min_importance_threshold.is_finite() || !(0.0..=1.0).contains(&self.min_importance_threshold) {
            return Err(anyhow!("Importance threshold must be between 0 and 1"));
        }
        if self.compression_threshold_tokens == 0 { return Err(anyhow!("Compression threshold must be positive")); }
        Ok(())
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search_top_k: 5,
            compression_threshold_tokens: 2_500_000,
            min_importance_threshold: 0.3,
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// MemEx local client
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone)]
pub struct MemExClient {
    config: Arc<RwLock<MemoryConfig>>,
    local: local::LocalMemory,
}

impl MemExClient {
    pub fn new() -> Self {
        // The shared data directory, where the CLI and Desktop keep memory.
        let directory = crate::runtime::resolve_default_data_dir()
            .unwrap_or_else(|_| std::env::temp_dir().join("deepseek-code"));
        Self::with_path(directory.join("data/memory.sqlite3"))
    }

    pub fn with_path(path: std::path::PathBuf) -> Self {
        // Both frontends retain a handle for explicit archive operations.
        // Constructing that handle during file-mode startup must not open SQLite.
        Self {
            config: Arc::new(RwLock::new(MemoryConfig::default())),
            local: local::LocalMemory { path },
        }
    }

    pub async fn get_config(&self) -> MemoryConfig {
        // Separate bridge/native-engine clients observe settings changed by the UI.
        if let Ok(Some(config)) = self.local.config() { return config; }
        self.config.read().await.clone()
    }

    pub fn get_config_blocking(&self) -> MemoryConfig {
        if let Ok(Some(config)) = self.local.config() { return config; }
        self.config.try_read().map(|guard| guard.clone()).unwrap_or_default()
    }

    pub async fn update_config(&self, config: MemoryConfig) -> Result<()> {
        config.validate()?;
        self.local.save_config(&config)?;
        *self.config.write().await = config;
        Ok(())
    }

    pub async fn search(&self, query: &str, top_k: Option<usize>) -> Result<Vec<MemoryItem>> {
        let config = self.get_config().await;
        if !config.enabled { return Ok(vec![]); }
        let top_k = top_k.unwrap_or(config.search_top_k).clamp(1, 100);
        let local = self.local.clone();
        let query = query.to_string();
        tokio::task::spawn_blocking(move || local.search(&query, top_k)).await?
    }

    pub async fn ingest(&self, content: &str, importance: Option<f64>, metadata: Option<HashMap<String, String>>) -> Result<()> {
        let config = self.get_config().await;
        if !config.enabled { return Err(anyhow!("Memory is disabled")); }
        let content = content.trim();
        if content.is_empty() { return Err(anyhow!("Memory content cannot be empty")); }
        if content.chars().count() > 32_000 { return Err(anyhow!("Memory must be at most 32000 characters")); }
        let importance = importance.unwrap_or_else(|| Self::estimate_importance(content));
        if !importance.is_finite() || !(0.0..=1.0).contains(&importance) { return Err(anyhow!("Importance must be between 0 and 1")); }
        if importance < config.min_importance_threshold { return Err(anyhow!("Memory importance is below the configured threshold")); }
        let local = self.local.clone();
        let content = content.to_string();
        tokio::task::spawn_blocking(move || local.ingest(&content, importance, metadata)).await?
    }

    /// Replaces one entry in the local store.
    pub async fn update(&self, id: &str, content: &str, importance: f64) -> Result<bool> {
        let content = content.trim();
        if content.is_empty() { return Err(anyhow!("Memory content cannot be empty")); }
        if content.chars().count() > 32_000 { return Err(anyhow!("Memory must be at most 32000 characters")); }
        if !importance.is_finite() || !(0.0..=1.0).contains(&importance) { return Err(anyhow!("Importance must be between 0 and 1")); }
        let local = self.local.clone();
        let (id, content) = (id.to_string(), content.to_string());
        tokio::task::spawn_blocking(move || local.update(&id, &content, importance)).await?
    }

    /// Removes one entry from the local store.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let local = self.local.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || local.delete(&id)).await?
    }

    pub async fn stats(&self) -> Result<MemoryStats> {
        let local = self.local.clone();
        tokio::task::spawn_blocking(move || local.stats()).await?
    }

    pub async fn clear(&self) -> Result<()> {
        let local = self.local.clone();
        tokio::task::spawn_blocking(move || local.clear()).await?
    }

    fn estimate_importance(content: &str) -> f64 {
        let content_lower = content.to_lowercase();
        let mut score: f64 = 0.3;
        let high_signals = ["important", "critical", "remember", "config", "必须记住", "重要", "关键"];
        for s in &high_signals { if content_lower.contains(s) { score += 0.15; } }
        let medium_signals = ["architecture", "design", "decision", "pattern", "workflow", "架构", "设计", "决策", "模式"];
        for s in &medium_signals { if content_lower.contains(s) { score += 0.08; } }
        if content.len() > 200 { score += 0.05; }
        if content.len() > 500 { score += 0.05; }
        score.min(1.0_f64)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Tiered Compressor
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug, Clone)]
pub enum MemoryTier {
    Hot,
    Warm,
    Cold,
}

#[derive(Debug, Clone)]
pub struct TieredMessage {
    pub role: String,
    pub content: String,
    pub tier: MemoryTier,
    pub summary: Option<String>,
    pub keywords: Vec<String>,
    pub tool_calls_count: usize,
    pub is_subtask_boundary: bool,
}

pub struct TieredCompressor {
    hot_threshold: usize,
    warm_threshold: usize,
}

impl TieredCompressor {
    pub fn new() -> Self {
        Self {
            hot_threshold: 10,
            warm_threshold: 30,
        }
    }

    pub fn with_thresholds(mut self, hot: usize, warm: usize) -> Self {
        self.hot_threshold = hot;
        self.warm_threshold = warm;
        self
    }

    pub fn classify_messages(&self, messages: &[Value]) -> Vec<TieredMessage> {
        let total_rounds = Self::count_rounds(messages);
        let boundaries = Self::detect_subtask_boundaries(messages);
        let boundary_set: HashSet<usize> = boundaries.into_iter().collect();

        let mut round_idx = 0usize;
        let mut result = Vec::with_capacity(messages.len());

        for (i, msg) in messages.iter().enumerate() {
            let role = msg["role"].as_str().unwrap_or("unknown").to_string();
            let content = Self::extract_text_content(msg);
            let tool_calls_count = Self::count_tool_uses_in_message(msg);
            let is_boundary = boundary_set.contains(&i);

            if role == "assistant" {
                round_idx += 1;
            }

            let rounds_from_end = total_rounds.saturating_sub(round_idx);
            let tier = if rounds_from_end < self.hot_threshold {
                MemoryTier::Hot
            } else if rounds_from_end < self.warm_threshold {
                MemoryTier::Warm
            } else {
                MemoryTier::Cold
            };

            result.push(TieredMessage {
                role,
                content,
                tier,
                summary: None,
                keywords: Vec::new(),
                tool_calls_count,
                is_subtask_boundary: is_boundary,
            });
        }

        result
    }

    fn count_rounds(messages: &[Value]) -> usize {
        messages.iter().filter(|m| m["role"].as_str() == Some("assistant")).count()
    }

    pub fn detect_subtask_boundaries(messages: &[Value]) -> Vec<usize> {
        let mut boundaries = Vec::new();
        let len = messages.len();
        if len < 3 {
            return boundaries;
        }

        for i in 0..len.saturating_sub(2) {
            let curr_role = messages[i]["role"].as_str().unwrap_or("");
            let next_role = messages[i + 1]["role"].as_str().unwrap_or("");
            let next2_role = messages[i + 2]["role"].as_str().unwrap_or("");

            if curr_role == "assistant" && Self::has_tool_use(&messages[i])
                && next_role == "user" && Self::has_tool_result(&messages[i + 1])
                && next2_role == "assistant" && !Self::has_tool_use(&messages[i + 2])
            {
                boundaries.push(i + 2);
            }
        }

        boundaries
    }

    fn has_tool_use(msg: &Value) -> bool {
        if let Some(arr) = msg["content"].as_array() {
            arr.iter().any(|b| b["type"].as_str() == Some("tool_use"))
        } else {
            false
        }
    }

    fn has_tool_result(msg: &Value) -> bool {
        if let Some(arr) = msg["content"].as_array() {
            arr.iter().any(|b| b["type"].as_str() == Some("tool_result"))
        } else {
            false
        }
    }

    fn result_hint(text: &str, limit: usize, collapsed: bool) -> &str {
        let Some((cut, _)) = text.char_indices().nth(limit) else {
            return text;
        };
        let prefix = &text[..cut];
        // Hints are optional. Leave a reference in the stored result when
        // the whole line would exceed this summary's character budget.
        if let Some(start) = prefix.rfind("[base64 data omitted:").filter(|_| collapsed) {
            if text.contains("\nbase64: ") && !prefix[start..].contains('\n') {
                return &prefix[..start];
            }
        }
        prefix
    }

    fn extract_text_content(msg: &Value) -> String {
        if let Some(s) = msg["content"].as_str() {
            return s.to_string();
        }
        if let Some(arr) = msg["content"].as_array() {
            let parts: Vec<String> = arr.iter().filter_map(|b| {
                match b["type"].as_str() {
                    Some("text") => b["text"].as_str().map(String::from),
                    Some("tool_use") => {
                        Some(format!("[ToolCall: {}]", b["name"].as_str().unwrap_or("?")))
                    }
                    Some("tool_result") => Some(format!(
                        "[ToolResult: {}]",
                        Self::result_hint(
                            b["content"].as_str().unwrap_or(""),
                            200,
                            b["_collapsed"] == true
                        )
                    )),
                    _ => None,
                }
            }).collect();
            return parts.join("\n");
        }
        String::new()
    }

    fn count_tool_uses_in_message(msg: &Value) -> usize {
        if let Some(arr) = msg["content"].as_array() {
            arr.iter().filter(|b| b["type"].as_str() == Some("tool_use")).count()
        } else {
            0
        }
    }

    pub fn generate_summary(messages: &[Value]) -> String {
        let boundaries = Self::detect_subtask_boundaries(messages);
        let mut subtask_ranges: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for &b in &boundaries {
            subtask_ranges.push((start, b));
            start = b;
        }
        if start < messages.len() {
            subtask_ranges.push((start, messages.len()));
        }

        if subtask_ranges.is_empty() {
            return ContextManager::simple_summarize(
                &messages.iter().map(|m| Self::extract_text_content(m)).collect::<Vec<_>>()
            );
        }

        let mut summary_parts = Vec::new();
        for (subtask_idx, (start, end)) in subtask_ranges.iter().enumerate() {
            let subtask_msgs = &messages[*start..*end];
            let mut tool_chain: Vec<String> = Vec::new();
            let mut result_hint = String::new();

            for msg in subtask_msgs {
                if let Some(arr) = msg["content"].as_array() {
                    for block in arr {
                        if block["type"].as_str() == Some("tool_use") {
                            if let Some(name) = block["name"].as_str() {
                                tool_chain.push(name.to_string());
                            }
                        }
                        if block["type"].as_str() == Some("tool_result") {
                            let content = block["content"].as_str().unwrap_or("");
                            if !content.is_empty() {
                                result_hint =
                                    Self::result_hint(content, 80, block["_collapsed"] == true)
                                        .to_owned();
                            }
                        }
                    }
                }
            }

            let chain_str = if tool_chain.is_empty() {
                "no tools".to_string()
            } else {
                tool_chain.join("→")
            };

            let result_str = if result_hint.is_empty() {
                String::new()
            } else {
                format!(": {}", result_hint)
            };

            summary_parts.push(format!("[Subtask {}] {}{}", subtask_idx + 1, chain_str, result_str));
        }

        summary_parts.join("\n")
    }

    pub async fn generate_summary_async(messages: &[Value], memex: &MemExClient) -> String {
        let sync_summary = Self::generate_summary(messages);
        let combined_text: String = messages.iter()
            .map(|m| Self::extract_text_content(m))
            .collect::<Vec<_>>()
            .join("\n");

        if combined_text.len() < 200 {
            return sync_summary;
        }

        let prompt = format!(
            "Summarize the following conversation turns concisely, preserving key decisions, tool usage chains, and outcomes:\n\n{}",
            combined_text.chars().take(4000).collect::<String>()
        );

        match memex.search(&prompt, Some(1)).await {
            Ok(results) if !results.is_empty() => {
                if results[0].similarity_score.unwrap_or(0.0) > 0.85 {
                    format!("{}\n[LLM-enhanced] {}", sync_summary, &results[0].content.chars().take(500).collect::<String>())
                } else {
                    sync_summary
                }
            }
            _ => sync_summary,
        }
    }

    pub fn extract_keywords(messages: &[Value]) -> Vec<String> {
        let combined: String = messages.iter()
            .map(|m| Self::extract_text_content(m))
            .collect::<Vec<_>>()
            .join(" ");

        let mut keywords = Vec::new();

        if let Ok(file_pattern) = Regex::new(r"[\w./-]+\.[\w]+") {
            for cap in file_pattern.find_iter(&combined) {
                let f = cap.as_str().to_string();
                if f.len() > 3 && f.len() < 80 && !keywords.contains(&f) {
                    keywords.push(f);
                }
            }
        }

        let decision_signals = [
            "decided", "decision", "chosen", "selected", "concluded",
            "resolved", "fixed", "implemented", "refactored", "migrated",
            "决定", "选择", "结论", "解决", "修复", "实现", "重构", "迁移",
        ];
        let combined_lower = combined.to_lowercase();
        for signal in &decision_signals {
            if combined_lower.contains(signal) && !keywords.contains(&signal.to_string()) {
                keywords.push(signal.to_string());
            }
        }

        if let Ok(ident_pattern) = Regex::new(r"\b[A-Z][a-zA-Z]+(?:Trait|Impl|Struct|Enum|Class|Module|Component|Service|Manager|Handler)\b") {
            for cap in ident_pattern.find_iter(&combined) {
                let ident = cap.as_str().to_string();
                if !keywords.contains(&ident) {
                    keywords.push(ident);
                }
            }
        }

        if let Ok(fn_pattern) = Regex::new(r"\b(?:fn|def|function|async fn)\s+(\w+)") {
            for cap in fn_pattern.captures_iter(&combined) {
                if let Some(m) = cap.get(1) {
                    let name = m.as_str().to_string();
                    if !keywords.contains(&name) {
                        keywords.push(name);
                    }
                }
            }
        }

        keywords.truncate(30);
        keywords
    }

    pub fn compress(&self, messages: &[Value]) -> Vec<Value> {
        let total_rounds = Self::count_rounds(messages);
        if total_rounds <= self.hot_threshold {
            return messages.to_vec();
        }

        let classified = self.classify_messages(messages);

        let mut cold_end = 0usize;
        let mut warm_end = 0usize;
        {
            let mut round_idx = 0usize;
            for (i, tm) in classified.iter().enumerate() {
                if tm.role == "assistant" {
                    round_idx += 1;
                }
                let rounds_from_end = total_rounds.saturating_sub(round_idx);
                if rounds_from_end >= self.warm_threshold && cold_end == 0 {
                    cold_end = i + 1;
                }
                if rounds_from_end >= self.hot_threshold && warm_end == 0 {
                    warm_end = i + 1;
                }
            }
        }

        let mut result = Vec::new();

        if cold_end > 0 {
            let cold_msgs = &messages[..cold_end];
            let keywords = Self::extract_keywords(cold_msgs);
            let kw_str = keywords.join(", ");
            result.push(json!({
                "role": "system",
                "content": format!("[Earlier context keywords: {}] Use ContextSearch to retrieve details", kw_str)
            }));
        }

        if warm_end > cold_end {
            let warm_msgs = &messages[cold_end..warm_end];
            let summary = Self::generate_summary(warm_msgs);
            result.push(json!({
                "role": "system",
                "content": format!("[Summary of earlier subtask] {}", summary)
            }));
        }

        if warm_end < messages.len() {
            for msg in &messages[warm_end..] {
                result.push(msg.clone());
            }
        }

        if result.is_empty() {
            return messages.to_vec();
        }

        result
    }

    /// Aggressive compression for emergency situations when even standard compression is not enough
    /// Keeps only the most recent messages and compresses everything else heavily
    pub fn compress_aggressive(&self, messages: &[Value], keep_recent: usize) -> Vec<Value> {
        let total_len = messages.len();
        if total_len <= keep_recent {
            return messages.to_vec();
        }

        let mut result = Vec::new();

        // Add a super compact summary of everything before the recent messages
        let old_msgs = &messages[0..total_len - keep_recent];
        let keywords = Self::extract_keywords(old_msgs);
        let kw_str = if keywords.len() > 15 {
            keywords[0..15].join(", ")
        } else {
            keywords.join(", ")
        };

        result.push(json!({
            "role": "system",
            "content": format!("[Compressed context] Key topics: {}. Use ContextSearch for details.", kw_str)
        }));

        // Add the most recent messages
        for msg in &messages[total_len - keep_recent..] {
            result.push(msg.clone());
        }

        result
    }

    /// Progressive compression that applies multiple compression levels until within limits
    pub fn compress_progressive(
        &self,
        messages: &[Value],
        target_token_limit: usize,
        token_estimator: impl Fn(&[Value]) -> usize,
    ) -> Vec<Value> {
        let mut compressed = messages.to_vec();

        // First try standard compression
        let mut estimated = token_estimator(&compressed);
        if estimated <= target_token_limit {
            return compressed;
        }

        compressed = self.compress(&compressed);
        estimated = token_estimator(&compressed);
        if estimated <= target_token_limit {
            return compressed;
        }

        // Then try more aggressive compression with varying keep counts
        let keep_counts = [20, 15, 10, 8, 5];
        for &keep in &keep_counts {
            compressed = self.compress_aggressive(messages, keep);
            estimated = token_estimator(&compressed);
            if estimated <= target_token_limit {
                return compressed;
            }
        }

        // Last resort: keep only the absolute minimum (last 3 messages)
        self.compress_aggressive(messages, 3)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Context Manager
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Opening sentence of every recalled-memory block.
pub const RECALL_LEAD_IN: &str = "Retrieved for possible relevance \u{2014} use only if it actually applies to what the user asked.";

#[derive(Clone)]
pub struct ContextManager {
    memex: Arc<MemExClient>,
    caveman_rtk: Arc<Mutex<CavemanRTKSystem>>,
}

impl ContextManager {
    pub(crate) fn contains_credentials(content: &str) -> bool {
        static CREDENTIALS: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(
            r#"(?i)(?:-----BEGIN [A-Z ]*PRIVATE KEY-----|\bbearer\s+[a-z0-9._~+/=-]+|\bsk-[a-z0-9_-]{8,}|\b(?:api[-_ ]?key|password|passwd|secret|access[-_ ]?token|refresh[-_ ]?token)["']?\s*(?:[:=]|is\b)\s*["']?\S+|(?:密码|密钥|令牌)\s*(?:是|为|[:：=])\s*\S+)"#,
        ).expect("credential detection regex is valid"));
        CREDENTIALS.is_match(content)
    }

    pub fn new(memex: Arc<MemExClient>) -> Self {
        Self {
            memex,
            caveman_rtk: Arc::new(Mutex::new(CavemanRTKSystem::new(None))),
        }
    }

    pub fn memex(&self) -> &MemExClient { &self.memex }

    /// Get Caveman RTK stats for dashboard
    pub async fn get_caveman_stats(&self) -> CavemanRTKStats {
        let system = self.caveman_rtk.lock().await;
        system.get_stats()
    }

    /// Add content to Caveman memory
    pub async fn add_to_caveman_memory(&self, content: &str, role: &str) -> Result<String> {
        let mut system = self.caveman_rtk.lock().await;
        system.add_memory_segment(content, role)
    }

    /// Get relevant context from Caveman
    pub async fn get_caveman_context(&self, query: &str, max_segments: usize) -> String {
        let mut system = self.caveman_rtk.lock().await;
        system.get_context_string(query, max_segments)
    }

    /// Record RLM feedback
    pub async fn record_rlm_feedback(&self, memory_id: &str, was_useful: bool, context: &str) {
        let mut system = self.caveman_rtk.lock().await;
        system.record_rlm_feedback(memory_id, was_useful, context);
    }

    /// Run RLM iteration
    pub async fn run_rlm_iteration(&self) -> u32 {
        let mut system = self.caveman_rtk.lock().await;
        system.rlm_iterate()
    }

    pub fn needs_compression(&self, message_count: usize, total_chars: usize) -> bool {
        let estimated_tokens = total_chars / 4;
        let config = self.memex.get_config_blocking();
        message_count > 50 || estimated_tokens > config.compression_threshold_tokens
    }

    pub async fn compress_context(&self, conversation_id: &str, old_messages: &[String]) -> Result<String> {
        let summary = Self::simple_summarize(old_messages);
        let mut meta = HashMap::new();
        meta.insert("type".to_string(), "context_compression".to_string());
        meta.insert("conversation_id".to_string(), conversation_id.to_string());
        meta.insert("compressed_count".to_string(), old_messages.len().to_string());
        if !old_messages.iter().any(|message| Self::contains_credentials(message)) {
            let _ = self.memex.ingest(&summary, Some(0.9), Some(meta)).await;
        }
        info!("[MemEx] Compressed {} messages for conv {}", old_messages.len(), conversation_id);
        Ok(summary)
    }

    fn simple_summarize(messages: &[String]) -> String {
        let combined = messages.join("\n");
        let key_signals = ["important", "key", "decision", "architecture", "design", "pattern", "bug", "fix", "error", "solution", "conclusion", "重要", "关键", "决定", "架构", "设计", "错误", "修复", "解决方案", "结论"];
        let mut key_sentences: Vec<&str> = Vec::new();
        for line in combined.lines() {
            let line_lower = line.to_lowercase();
            for signal in &key_signals {
                if line_lower.contains(signal) && !key_sentences.contains(&line) {
                    key_sentences.push(line);
                    break;
                }
            }
        }
        if key_sentences.is_empty() {
            let lines: Vec<&str> = combined.lines().collect();
            if lines.is_empty() {
                return "[Context Summary]\n(empty)".to_string();
            }
            let total = lines.len();
            let take = 10.min(total);
            let head_take = take.min(total.max(1) / 2).max(1);
            let tail_start = total.saturating_sub(take);
            // Avoid overlapping head and tail sections
            let tail_start = if tail_start <= head_take { total.min(head_take + take) } else { tail_start };
            let tail_start = tail_start.min(lines.len());
            let mut summary = String::from("[Context Summary]\n");
            for line in &lines[..head_take] { summary.push_str(line); summary.push('\n'); }
            if total > take * 2 && tail_start > head_take { summary.push_str("...\n"); }
            if tail_start < lines.len() {
                for line in &lines[tail_start..] { summary.push_str(line); summary.push('\n'); }
            }
            summary
        } else {
            format!("[Context Summary - {} key points]\n{}", key_sentences.len(), key_sentences.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_memory_survives_restart_and_retrieves_chinese() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data/memory.sqlite3");
        let client = MemExClient::with_path(path.clone());
        client.ingest("用户偏好中文回复，项目使用 Rust", Some(0.8), None).await.unwrap();
        client.ingest("用户偏好中文回复，项目使用 Rust", Some(0.8), None).await.unwrap();
        drop(client);
        let reopened = MemExClient::with_path(path);
        assert_eq!(reopened.stats().await.unwrap().total_memories, 1);
        assert_eq!(reopened.search("我之前的中文偏好是什么", None).await.unwrap().len(), 1);
        assert!(reopened.search("unrelated database", None).await.unwrap().is_empty());
        reopened.clear().await.unwrap();
        assert_eq!(reopened.stats().await.unwrap().total_memories, 0);
    }

    #[tokio::test]
    async fn settings_persist_and_apply_to_other_clients() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data/memory.sqlite3");
        let client = MemExClient::with_path(path.clone());
        let runtime = MemExClient::with_path(path);
        let mut config = client.get_config().await;
        config.enabled = false;
        client.update_config(config.clone()).await.unwrap();
        assert!(!runtime.get_config().await.enabled);
        assert!(runtime.ingest("important preference", Some(0.9), None).await.is_err());
    }

    #[tokio::test]
    async fn invalid_content_and_config_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let client = MemExClient::with_path(directory.path().join("data/memory.sqlite3"));
        assert!(client.ingest("  ", None, None).await.is_err());
        assert!(client.ingest("test", Some(2.0), None).await.is_err());
        let mut config = client.get_config().await;
        config.search_top_k = 0;
        assert!(client.update_config(config).await.is_err());
        assert_eq!(client.get_config().await.search_top_k, 5);
    }

    #[test]
    fn saved_settings_with_removed_remote_keys_still_load() {
        let config: MemoryConfig = serde_json::from_str(
            r#"{"enabled":true,"backend_url":"http://127.0.0.1:8765","auto_ingest":true,"auto_search":true,"search_top_k":3}"#,
        ).unwrap();
        assert!(config.enabled);
        assert_eq!(config.search_top_k, 3);
    }

    #[test]
    fn compressor_hints_use_context_search() {
        let messages = (0..70)
            .map(|index| {
                json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("message {index} about architecture")
                })
            })
            .collect::<Vec<_>>();
        let compressed = TieredCompressor::new().compress(&messages);
        let hints = compressed.iter().map(|message| message.to_string()).collect::<String>();
        assert!(hints.contains("ContextSearch"), "{hints}");
        assert!(!hints.contains("memory_search"), "{hints}");
        let aggressive = TieredCompressor::new().compress_aggressive(&messages, 3);
        assert!(aggressive[0].to_string().contains("ContextSearch"));
    }

    #[test]
    fn test_importance() {
        let low = MemExClient::estimate_importance("hello world");
        let high = MemExClient::estimate_importance("IMPORTANT: Remember the project architecture decision!");
        assert!(high > low);
    }

    #[test]
    fn test_summarize() {
        let msgs = vec!["The architecture uses microservices".to_string(), "Bug: memory leak in pool".to_string(), "Random chat".to_string()];
        let s = ContextManager::simple_summarize(&msgs);
        assert!(s.contains("architecture") || s.contains("Bug"));
    }
}

#[cfg(test)]
mod model_hint_tests {
    #[test]
    fn base64_model_boundary_summary_hint_preserves_recovery_lines() {
        let line=format!("log: [base64 data omitted: about 768 bytes decoded; full text: {}, line 1]\nbase64: 1 block collapsed (1024 encoded bytes)","path/".repeat(80));
        for budget in [80, 200] {
            assert_eq!(
                super::TieredCompressor::result_hint(&line, budget, true),
                "log: "
            );
            assert_eq!(
                super::TieredCompressor::result_hint("ordinary 日志", budget, false),
                "ordinary 日志"
            );
        }
    }
}
