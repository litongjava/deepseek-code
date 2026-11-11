//! Built-in list prices for models whose provider publishes a fixed per-token
//! rate. Used only when the provider reports no cost and the user configured
//! no price for the model.
use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use serde_json::Value;

/// USD per million tokens at peak time.
struct Rates {
    cache_hit: f64,
    cache_miss: f64,
    output: f64,
}

/// DeepSeek list prices (api-docs.deepseek.com/quick_start/pricing, 2026-10).
/// The legacy V4 Flash names are billed at the Flash price.
fn rates(model: &str) -> Option<Rates> {
    let model = model.to_ascii_lowercase();
    let model = model.rsplit("::").next().unwrap_or(&model);
    if model == "deepseek-flash"
        || model.starts_with("deepseek-flash-")
        || model.starts_with("deepseek-v4.1-flash")
        || model.starts_with("deepseek-v4-flash")
    {
        Some(Rates { cache_hit: 0.006, cache_miss: 0.30, output: 1.20 })
    } else if model.starts_with("deepseek-v4-pro") {
        Some(Rates { cache_hit: 0.044, cache_miss: 1.32, output: 3.96 })
    } else {
        None
    }
}

/// DeepSeek peak hours: 01:00-04:00 and 06:00-10:00 UTC, Monday to Friday.
/// Other times are billed at half price. Chinese public holidays are also
/// off-peak but are not known here, so they are priced as ordinary days.
fn is_peak(time: DateTime<Utc>) -> bool {
    if matches!(time.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    matches!(time.hour(), 1..=3 | 6..=9)
}

/// Estimated cost of one recorded request, or None when the model has no
/// built-in price or the token counters are missing.
pub fn list_price(
    model: &str,
    started_at_ms: Option<i64>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    usage: &Value,
) -> Option<f64> {
    let rates = rates(model)?;
    let (input, output) = (input?, output?);
    let cache_read = cache_read.unwrap_or(0);
    // Anthropic-format input_tokens excludes cached input; OpenAI-format
    // prompt_tokens includes it.
    let miss = if let Some(miss) = usage["prompt_cache_miss_tokens"].as_u64() {
        miss
    } else if usage.get("input_tokens").is_some() {
        input + cache_write.unwrap_or(0)
    } else {
        input.saturating_sub(cache_read)
    };
    let factor = match started_at_ms.and_then(DateTime::<Utc>::from_timestamp_millis) {
        Some(time) if !is_peak(time) => 0.5,
        _ => 1.0,
    };
    let cost = (cache_read as f64 * rates.cache_hit
        + miss as f64 * rates.cache_miss
        + output as f64 * rates.output)
        * factor
        / 1_000_000.0;
    cost.is_finite().then_some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(text: &str) -> Option<i64> {
        Some(DateTime::parse_from_rfc3339(text).unwrap().timestamp_millis())
    }

    #[test]
    fn deepseek_prices_split_cache_hits_and_peak_hours() {
        let anthropic = json!({"input_tokens":1_000_000,"cache_read_input_tokens":1_000_000});
        // Friday 02:00 UTC is peak.
        let peak = list_price("deepseek-flash", at("2026-10-23T02:00:00Z"), Some(1_000_000), Some(1_000_000), Some(1_000_000), Some(0), &anthropic).unwrap();
        assert!((peak - (0.006 + 0.30 + 1.20)).abs() < 1e-9);
        // Saturday is off-peak at half price.
        let weekend = list_price("deepseek-flash", at("2026-10-24T02:00:00Z"), Some(1_000_000), Some(1_000_000), Some(1_000_000), Some(0), &anthropic).unwrap();
        assert!((weekend - peak / 2.0).abs() < 1e-9);
        // Friday 04:00 UTC falls between the peak windows.
        let gap = list_price("deepseek-v4-flash", at("2026-10-23T04:00:00Z"), Some(1_000_000), Some(0), Some(0), Some(0), &anthropic).unwrap();
        assert!((gap - 0.15).abs() < 1e-9);
    }

    #[test]
    fn openai_prompt_tokens_include_cached_input() {
        let openai = json!({"prompt_tokens":1_000_000,"completion_tokens":0,"prompt_cache_hit_tokens":400_000});
        let cost = list_price("deepseek-v4-pro", at("2026-10-23T07:00:00Z"), Some(1_000_000), Some(0), Some(400_000), None, &openai).unwrap();
        assert!((cost - (0.4 * 0.044 + 0.6 * 1.32)).abs() < 1e-9);
        let explicit = json!({"prompt_tokens":1_000_000,"prompt_cache_miss_tokens":100_000});
        let cost = list_price("cli::deepseek-v4-pro", at("2026-10-23T07:00:00Z"), Some(1_000_000), Some(0), Some(900_000), None, &explicit).unwrap();
        assert!((cost - (0.9 * 0.044 + 0.1 * 1.32)).abs() < 1e-9);
    }

    #[test]
    fn unknown_models_and_missing_counters_stay_unpriced() {
        assert!(list_price("gpt-x", None, Some(1), Some(1), None, None, &json!({})).is_none());
        assert!(list_price("deepseek-flash", None, None, Some(1), None, None, &json!({})).is_none());
    }
}
