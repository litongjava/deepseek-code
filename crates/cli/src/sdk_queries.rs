//! SDK control requests that report or adjust session state: MCP status,
//! context usage, settings layers and the thinking budget.
use crate::session::Session;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

/// Requests answered without mutating the session; safe while a turn runs.
pub async fn read_only(session: &Session, request: &Value) -> Option<Result<Value>> {
    Some(match request["subtype"].as_str()? {
        "mcp_status" => Ok(json!({"mcpServers":session.sdk_mcp_status().await})),
        "get_context_usage" => context_usage(session).await,
        "get_settings" => settings(session).await,
        _ => return None,
    })
}

async fn context_usage(session: &Session) -> Result<Value> {
    let window = session.context_window().await.map(u64::from);
    let last = claude_core::services::context_usage::last_request(&session.store.db, &session.id)?;
    let mut categories = Vec::new();
    let mut total = 0;
    if let Some(usage) = last.as_ref().and_then(|request| request.usage.as_ref()) {
        for (name, color, tokens) in [
            ("System prompt", "promptBorder", usage.instructions.estimated_tokens),
            ("System tools", "inactive", usage.tools.estimated_tokens),
            ("Messages", "purple", usage.history.estimated_tokens),
            ("Output schema", "suggestion", usage.output_schema.estimated_tokens),
        ] {
            total += tokens;
            categories.push(json!({"name":name,"tokens":tokens,"color":color}));
        }
    }
    // Provider-reported input is exact; estimates only fill in when it is absent.
    if let Some(reported) = last.as_ref().and_then(|request| request.reported_input_tokens) {
        total = reported;
    }
    let max = window.unwrap_or(0);
    if max > total {
        categories.push(json!({"name":"Free space","tokens":max - total,"color":"promptBorder"}));
    }
    Ok(json!({
        "categories":categories,
        "totalTokens":total,
        "maxTokens":max,
        "rawMaxTokens":max,
        "percentage":if max == 0 { 0.0 } else { total as f64 * 100.0 / max as f64 },
        "model":last.as_ref().map_or(session.model.clone(), |request| request.model.clone()),
        "measured":last.is_some(),
    }))
}

fn read_object(path: &std::path::Path) -> Result<Map<String, Value>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
            match serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .with_context(|| format!("Invalid settings {}", path.display()))?
            {
                Value::Object(map) => Ok(map),
                _ => bail!("{} must contain a JSON object", path.display()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error.into()),
    }
}

fn merge(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(update)) => merge(existing, update),
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

async fn settings(session: &Session) -> Result<Value> {
    let workspace = &session.workspace;
    let mut sources = Vec::new();
    for (source, path) in [
        ("userSettings", session.paths.data_dir.join("settings.json")),
        ("projectSettings", workspace.join(".claude").join("settings.json")),
        ("localSettings", workspace.join(".claude").join("settings.local.json")),
    ] {
        sources.push((source, read_object(&path)?));
    }
    sources.push(("flagSettings", session.sdk_overrides.flag_settings.clone()));
    let mut effective = Map::new();
    for (_, settings) in &sources {
        merge(&mut effective, settings);
    }
    let effort = crate::session_commands::effective_effort(session, None)?
        .filter(|effort| ["low", "medium", "high", "max"].contains(&effort.as_str()));
    Ok(json!({
        "effective":effective,
        "sources":sources.into_iter().map(|(source, settings)| json!({"source":source,"settings":settings})).collect::<Vec<_>>(),
        "applied":{"model":session.model,"effort":effort},
    }))
}

/// Maps a reference thinking-token budget onto this engine's effort levels.
pub fn thinking_effort(request: &Value) -> Result<Option<&'static str>> {
    let budget = &request["max_thinking_tokens"];
    if budget.is_null() {
        return Ok(None);
    }
    let tokens = budget
        .as_u64()
        .context("max_thinking_tokens must be a non-negative integer or null")?;
    Ok(Some(match tokens {
        0 => "off",
        1..=4_096 => "low",
        4_097..=16_384 => "medium",
        16_385..=32_768 => "high",
        _ => "max",
    }))
}

/// Applies the supported flag settings and records them as the flag layer.
pub async fn apply_flag_settings(session: &mut Session, request: &Value) -> Result<Value> {
    let settings = request["settings"]
        .as_object()
        .context("apply_flag_settings.settings must be an object")?;
    for (key, value) in settings {
        match key.as_str() {
            "model" => ensure!(value.is_string() || value.is_null(), "flag setting model must be a string"),
            "effortLevel" => ensure!(
                value.is_null() || value.as_str().is_some_and(|v| ["low", "medium", "high", "max"].contains(&v)),
                "flag setting effortLevel must be low, medium, high or max"
            ),
            "permissions" => {
                let permissions = value.as_object().context("flag setting permissions must be an object")?;
                for (field, mode) in permissions {
                    ensure!(field == "defaultMode", "Unsupported flag setting permissions.{field}");
                    ensure!(mode.is_string(), "permissions.defaultMode must be a string");
                }
            }
            other => bail!("Unsupported flag setting: {other}"),
        }
    }
    if let Some(model) = settings.get("model").and_then(Value::as_str) {
        session.sdk_set_model(model).await?;
    }
    if let Some(effort) = settings.get("effortLevel") {
        crate::session_commands::set_effort(session, effort.as_str())?;
    }
    if let Some(mode) = settings.get("permissions").and_then(|p| p.get("defaultMode")).and_then(Value::as_str) {
        session.set_permission_mode(crate::sdk::internal_mode(mode)).await?;
    }
    merge(&mut session.sdk_overrides.flag_settings, settings);
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_budget_maps_to_named_effort_levels() {
        for (budget, effort) in [(json!(null), None), (json!(0), Some("off")), (json!(1024), Some("low")), (json!(10000), Some("medium")), (json!(32768), Some("high")), (json!(64000), Some("max"))] {
            assert_eq!(thinking_effort(&json!({"max_thinking_tokens":budget})).unwrap(), effort);
        }
        assert!(thinking_effort(&json!({"max_thinking_tokens":-1})).is_err());
        assert!(thinking_effort(&json!({"max_thinking_tokens":"many"})).is_err());
    }

    #[test]
    fn merge_overrides_nested_objects_in_source_order() {
        let mut target = json!({"permissions":{"allow":["Read"],"defaultMode":"default"},"theme":"dark"}).as_object().unwrap().clone();
        merge(&mut target, json!({"permissions":{"defaultMode":"plan"}}).as_object().unwrap());
        assert_eq!(Value::Object(target), json!({"permissions":{"allow":["Read"],"defaultMode":"plan"},"theme":"dark"}));
    }
}
