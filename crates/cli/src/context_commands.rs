//! Inspection only: the engine captures accounting at the actual transport boundary.
use crate::session::Session;
use anyhow::Result;

pub async fn show(session: &Session) -> Result<()> {
    let request = claude_core::services::context_usage::last_request(&session.store.db, &session.id)?;
    if crate::terminal::picker::available() {
        let window = session.context_window().await.map(u64::from);
        let autocompact = crate::session_commands::autocompact(session)?;
        let color = std::env::var_os("NO_COLOR").is_none();
        if let Some(grid) = request.as_ref().and_then(|request| grid(request, window, autocompact, color, "Measured from the last request; the next one includes this turn's messages.")) {
            eprintln!("{grid}");
            return Ok(());
        }
        if request.is_none() {
            // As in the reference CLI, estimate before anything was sent.
            if let Some(grid) = grid(&estimate(session, window)?, window, autocompact, color, "Estimated before the first request, from a recent request\u{2019}s system prompt and tools and this session\u{2019}s messages.") {
                eprintln!("{grid}");
                return Ok(());
            }
        }
    }
    eprintln!("{}", render(request));
    Ok(())
}
fn render(request: Option<claude_core::services::context_usage::LastRequest>) -> String {
    let mut lines = Vec::new();
    let Some(request) = request else {
        lines.push(format!(
            "Context: unknown. No recorded parent-session request is available."
        ));
        return lines.join("\n");
    };
    let time = request
        .started_at_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "unknown time".into());
    lines.push(format!(
        "Last request attempt: {} · {time} · {}",
        request.model, request.status
    ));
    lines.push(format!(
        "Provider-reported input: {}",
        count(request.reported_input_tokens)
    ));
    let Some(usage) = request.usage else {
        lines.push(format!(
            "Context breakdown: unknown. This request has no transport accounting metadata."
        ));
        lines.push(format!(
            "Historical request only; this is not the current or next request budget."
        ));
        return lines.join("\n");
    };
    lines.push(format!(
        "Estimated history: {} tokens",
        usage.history.estimated_tokens
    ));
    lines.push(format!(
        "Estimated system/developer instructions: {} tokens",
        usage.instructions.estimated_tokens
    ));
    lines.push(format!(
        "Estimated tool definitions: {} tokens",
        usage.tools.estimated_tokens
    ));
    lines.push(format!(
        "Estimated output schema: {} tokens",
        usage.output_schema.estimated_tokens
    ));
    lines.push(format!(
        "Estimated input total: {}",
        count(usage.estimated_input_tokens)
    ));
    if usage.unknown_parts() > 0 {
        lines.push(format!(
            "Known text subtotal: {} tokens; {} media/opaque parts have unknown token cost.",
            usage.text_subtotal(),
            usage.unknown_parts()
        ));
    }
    lines.push(format!(
        "Output reserve in request: {}",
        count(usage.sent_output_reserve)
    ));
    if usage.sent_output_reserve.is_none() {
        lines.push(format!(
            "Requested output budget: {} tokens; the protocol did not send an explicit limit.",
            usage.requested_output_budget
        ));
    }
    lines.push(format!(
        "Configured context window: {}",
        count(usage.configured_context_window)
    ));
    if let (Some(input), Some(reserve), Some(window)) = (
        request
            .reported_input_tokens
            .or(usage.estimated_input_tokens),
        usage.sent_output_reserve,
        usage.configured_context_window.filter(|window| *window > 0),
    ) {
        let used = input.saturating_add(reserve);
        lines.push(format!(
            "{} input + output reserve: {used} / {window} ({:.1}%)",
            if request.reported_input_tokens.is_some() {
                "Reported"
            } else {
                "Estimated"
            },
            used as f64 * 100.0 / window as f64
        ));
        if used > window {
            lines.push(format!(
                "Exceeds the configured window by {} tokens.",
                used - window
            ));
        }
    }
    lines.push(format!("Estimates use serialized UTF-8 bytes / 3, rounded up per category; provider tokenization and protocol overhead can differ."));
    lines.push(format!("Historical request only; later responses, model/settings changes and compaction are not included."));
    lines.join("\n")
}
/// What the first request would hold: the base system prompt with the
/// project instructions, the built-in tool definitions and saved history.
fn estimate(session: &Session, window: Option<u64>) -> Result<claude_core::services::context_usage::LastRequest> {
    let mut system = claude_core::prompt::build_self_hosted_system_prompt_with_config("", &claude_core::prompt::SystemPromptConfig::default());
    if let Ok(instructions) = claude_core::project_instructions::load(&session.workspace) {
        system.push_str(&instructions);
    }
    let history = session.store.model_history(&session.id, &claude_core::native_engine::provider_manager::ApiFormat::Anthropic)?;
    let body = serde_json::json!({"system": system, "messages": history, "tools": claude_core::tools::get_tool_definitions(), "max_tokens": 0});
    let window = window.and_then(|window| u32::try_from(window).ok());
    let mut usage = claude_core::native_engine::request_context::ContextUsage::from_body("anthropic-messages", &body, window, 0);
    // The engine's full system prompt and tool list barely change between
    // sessions; a recent measured request estimates them better.
    let recent = session.store.db.with_conn(|conn| claude_core::db::conversation_repo::list_conversations(conn))??;
    for row in recent.iter().filter(|row| row.id != session.id).take(20) {
        if let Some(measured) = claude_core::services::context_usage::last_request(&session.store.db, &row.id)?.and_then(|request| request.usage) {
            usage.instructions = measured.instructions;
            usage.tools = measured.tools;
            break;
        }
    }
    Ok(claude_core::services::context_usage::LastRequest {
        model: session.model.clone(),
        started_at_ms: None,
        status: "estimate".into(),
        usage: Some(usage),
        reported_input_tokens: None,
    })
}

/// `950`, `1.6k` or `200k`.
fn short(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000_000.. => {
            let text = format!("{:.1}", tokens as f64 / 1_048_576.0);
            format!("{}M", text.strip_suffix(".0").unwrap_or(&text))
        }
        _ if tokens % 1000 == 0 || tokens >= 100_000 && tokens % 1000 < 50 => format!("{}k", tokens / 1000),
        _ => format!("{:.1}k", tokens as f64 / 1000.0),
    }
}

/// /context as in the reference CLI: a 10×10 grid of the context window
/// coloured by what fills it, with the estimate per category beside it.
/// None before the session has sent a request.
pub fn grid(request: &claude_core::services::context_usage::LastRequest, window: Option<u64>, autocompact: Option<u8>, color: bool, footer: &str) -> Option<String> {
    let usage = request.usage.as_ref()?;
    let parts: Vec<(&str, u64, &str)> = [
        ("System prompt", usage.instructions.estimated_tokens, "38;5;246"),
        ("System tools", usage.tools.estimated_tokens, "38;5;37"),
        ("Output schema", usage.output_schema.estimated_tokens, "38;5;179"),
        ("Messages", usage.history.estimated_tokens, "38;5;141"),
    ]
    .into_iter()
    .filter(|(_, tokens, _)| *tokens > 0)
    .collect();
    let used: u64 = parts.iter().map(|(_, tokens, _)| tokens).sum();
    let window = window.or(usage.configured_context_window).unwrap_or(used.max(1)).max(1);
    let buffer = autocompact.map_or(0, |percent| window * (100 - percent as u64) / 100);
    let free = window.saturating_sub(used + buffer);
    let paint = |code: &str, text: &str| if color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() };
    let cell = window as f64 / 100.0;
    let buffer_cells = (buffer as f64 / cell).round() as usize;
    let mut cells = Vec::with_capacity(100);
    for index in 0..100usize {
        let start = index as f64 * cell;
        if index >= 100 - buffer_cells {
            cells.push(paint("38;5;240", "\u{26dd}"));
            continue;
        }
        let mut offset = 0.0;
        let mut glyph = paint("38;5;240", "\u{26f6}");
        for (_, tokens, code) in &parts {
            let end = offset + *tokens as f64;
            if start < end {
                glyph = paint(code, if end - start >= cell { "\u{26c1}" } else { "\u{26c0}" });
                break;
            }
            offset = end;
        }
        cells.push(glyph);
    }
    let percent = |tokens: u64| tokens as f64 * 100.0 / window as f64;
    let mut side = vec![
        request.model.clone(),
        format!("{}/{} tokens ({:.0}%)", short(used), short(window), percent(used)),
        String::new(),
        "Estimated usage by category".to_owned(),
    ];
    for (name, tokens, code) in &parts {
        side.push(format!("{} {name}: {} tokens ({:.1}%)", paint(code, "\u{26c1}"), short(*tokens), percent(*tokens)));
    }
    side.push(format!("{} Free space: {} ({:.1}%)", paint("38;5;240", "\u{26f6}"), short(free), percent(free)));
    if buffer > 0 {
        side.push(format!("{} Autocompact buffer: {} tokens ({:.1}%)", paint("38;5;240", "\u{26dd}"), short(buffer), percent(buffer)));
    }
    let mut lines = vec!["  \u{23bf}  Context Usage".to_owned()];
    for row in 0..10 {
        let cells: Vec<&str> = cells[row * 10..row * 10 + 10].iter().map(String::as_str).collect();
        lines.push(format!("     {}   {}", cells.join(" "), side.get(row).map(String::as_str).unwrap_or("")));
    }
    for extra in side.iter().skip(10) {
        lines.push(format!("     {:19}   {extra}", ""));
    }
    lines.push(format!("     {footer}"));
    Some(lines.join("\n"))
}

fn count(value: Option<u64>) -> String {
    value
        .map(|n| format!("{n} tokens"))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::{
        native_engine::request_context::ContextUsage, services::context_usage::LastRequest,
    };
    use serde_json::json;

    #[test]
    fn the_grid_shows_each_category_free_space_and_the_buffer() {
        assert_eq!((short(950), short(1_600), short(200_000), short(156_600), short(1_048_576)), ("950".into(), "1.6k".into(), "200k".into(), "156.6k".into(), "1M".into()));
        let system = "x".repeat(3000);
        let request = LastRequest {
            model: "deepseek-flash".into(),
            started_at_ms: Some(0),
            status: "completed".into(),
            reported_input_tokens: None,
            usage: Some(ContextUsage::from_body(
                "openai-completions",
                &json!({"messages":[{"role":"system","content":system},{"role":"user","content":"hello there"}],"max_tokens":200}),
                Some(10_000),
                200,
            )),
        };
        let drawn = grid(&request, Some(10_000), Some(80), false, "note").unwrap();
        let rows: Vec<&str> = drawn.lines().collect();
        assert_eq!(rows[0], "  \u{23bf}  Context Usage");
        assert!(rows[1].starts_with("     \u{26c1} ") && rows[1].ends_with("deepseek-flash"), "{drawn}");
        assert!(drawn.contains("System prompt:") && drawn.contains("Messages:") && drawn.contains("Free space:"));
        assert!(drawn.contains("Autocompact buffer: 2k tokens (20.0%)"), "{drawn}");
        // The last two rows are the 20% buffer.
        assert_eq!(rows[10].matches('\u{26dd}').count(), 10);
        assert!(grid(&LastRequest { usage: None, ..request }, None, None, false, "note").is_none());
    }

    #[test]
    fn context_output_distinguishes_unknown_estimates_and_last_request() {
        assert!(render(None).contains("Context: unknown"));
        let request = LastRequest {
            model: "previous-model".into(),
            started_at_ms: Some(0),
            status: "completed".into(),
            reported_input_tokens: Some(90),
            usage: Some(ContextUsage::from_body(
                "openai-completions",
                &json!({"messages":[{"role":"system","content":"PRIVATE_SYSTEM"},{"role":"user","content":"PRIVATE_USER"}],"max_tokens":200}),
                Some(1000),
                400,
            )),
        };
        let text = render(Some(request));
        assert!(text.contains("Last request attempt: previous-model"));
        assert!(text.contains("Provider-reported input: 90 tokens"));
        assert!(text.contains("Reported input + output reserve: 290 / 1000 (29.0%)"));
        assert!(text.contains("Estimated system/developer instructions:"));
        assert!(text.contains("Historical request only"));
        assert!(!text.contains("PRIVATE"));
        assert!(!text.contains('\x1b'));
        let request = LastRequest {
            model: "model".into(),
            started_at_ms: None,
            status: "error".into(),
            reported_input_tokens: None,
            usage: Some(ContextUsage::from_body(
                "openai-responses",
                &json!({"input":[{"type":"input_image","image_url":"PRIVATE_DATA"}]}),
                None,
                400,
            )),
        };
        let text = render(Some(request));
        for expected in [
            "Provider-reported input: unknown",
            "Estimated input total: unknown",
            "Output reserve in request: unknown",
            "Configured context window: unknown",
            "unknown token cost",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains('%'));
    }
}
