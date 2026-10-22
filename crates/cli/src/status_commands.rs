//! CLI rendering of read-only shared runtime observations.
use crate::session::Session;
use anyhow::{ensure, Result};
use claude_core::services::{
    anthropic_auth,
    runtime_status::{self, RuntimeStatus},
    session_worker,
};
use std::time::Duration;

pub async fn run(session: &Session, argument: &str) -> Result<()> {
    ensure!(
        matches!(argument.trim(), "" | "--check"),
        "Usage: /status [--check]"
    );
    let provider = session.selected_provider().await?;
    let (worker, probe) = tokio::join!(
        session_worker::runtime_status(&session.paths.data_dir, &session.id),
        async {
            if argument.trim() == "--check" {
                Some(
                    runtime_status::http_reachability(
                        &provider.provider.base_url,
                        Duration::from_secs(3),
                    )
                    .await,
                )
            } else {
                None
            }
        }
    );
    let (scope, tools) = match worker.tools {
        Some(tools) => ("persistent worker", Some(tools)),
        None => (
            "frontend process only; worker tools not observed",
            tokio::time::timeout(
                Duration::from_secs(1),
                session.runtime_status_source().snapshot(),
            )
            .await
            .ok(),
        ),
    };
    let account = if anthropic_auth::is_oauth_marker(&provider.provider.api_key) {
        match anthropic_auth::status(&session.paths.data_dir) {
            Ok(status) if status.authenticated => {
                if status
                    .expires_at
                    .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp())
                {
                    "OAuth, expired locally (refresh not checked)"
                } else {
                    "OAuth (not verified with the server)"
                }
            }
            Ok(_) => "OAuth selected but no local credentials",
            Err(_) => "OAuth status unavailable",
        }
    } else if provider.provider.api_key.trim().is_empty() {
        "missing"
    } else {
        "configured (not verified)"
    };
    let reachability = if let Some(check) = probe {
        format!(
            "{}{} (HTTP only; no model request)",
            state_label(&check.state),
            check
                .http_status
                .map(|status| format!(", HTTP {status}"))
                .unwrap_or_default()
        )
    } else {
        "not checked \u{b7} /status --check".into()
    };
    let title = session
        .store
        .db
        .with_conn(|conn| claude_core::db::conversation_repo::get_conversation(conn, &session.id))
        .ok()
        .and_then(Result::ok)
        .flatten()
        .and_then(|row| row.title)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "(unnamed) \u{b7} /rename".into());
    let language_servers = claude_core::tools::lsp_servers::status(&session.workspace);
    let installed = language_servers.iter().filter(|row| row["installed"] == true).count();
    let mut sections = vec![
        vec![
            ("Version", env!("CARGO_PKG_VERSION").to_owned()),
            ("Session name", runtime_status::label(&title)),
            ("Session ID", runtime_status::label(&session.id)),
            ("cwd", runtime_status::label(&session.workspace.to_string_lossy())),
            ("Data directory", runtime_status::label(&session.paths.data_dir.to_string_lossy())),
        ],
        vec![
            ("Provider", format!("{} ({})", runtime_status::label(&provider.provider.name), runtime_status::label(&session.provider_id))),
            ("Model", format!("{} \u{b7} /model", runtime_status::label(&session.model))),
            ("API endpoint", runtime_status::origin(&provider.provider.base_url)),
            ("API key", account.to_owned()),
            ("Reachability", reachability),
            ("Permission mode", format!("{} \u{b7} Shift+Tab to cycle", session.permission_mode())),
            ("Routing", if session.auto_route { "automatic" } else { "selected provider" }.to_owned()),
        ],
    ];
    let mut tools_section = vec![(
        "Worker",
        format!("{}{}", state_label(&worker.state), worker.pid.map(|pid| format!(" (PID {pid})")).unwrap_or_default()),
    )];
    match &tools {
        Some(tools) => {
            let running = tools.mcp.iter().filter(|server| server.state == "manager_reports_running").count();
            tools_section.push(("MCP servers", format!("{running} running, {} configured \u{b7} /mcp", tools.mcp.len())));
            tools_section.push(("Built-in tools", tools.native_tools.to_string()));
        }
        None => tools_section.push(("Tools", "snapshot unavailable (inspection timed out)".into())),
    }
    tools_section.push((
        "Language servers",
        format!("{installed} of {} installed \u{b7} /lsp", language_servers.len()),
    ));
    sections.push(tools_section);
    let width = sections.iter().flatten().map(|(label, _)| label.len()).max().unwrap_or(0) + 2;
    let mut output = String::from("Status\n");
    for section in &sections {
        output.push('\n');
        for (label, value) in section {
            output.push_str(&format!("  {:<width$}{value}\n", format!("{label}:")));
        }
    }
    if let Some(tools) = tools {
        output.push_str(&render_tools(&tools, scope));
    }
    eprint!("{output}");
    Ok(())
}
fn state_label(state: &str) -> String {
    runtime_status::label(state).replace('_', " ")
}

/// Per-server lines; states are manager observations, not protocol probes.
fn render_tools(tools: &RuntimeStatus, scope: &str) -> String {
    let mut output = String::new();
    if !tools.mcp.is_empty() {
        output.push_str("\n  MCP servers (as reported by the manager):\n");
    }
    for server in &tools.mcp {
        output.push_str(&format!(
            "    {}: {}, {} tools{}\n",
            runtime_status::label(&server.name),
            state_label(&server.state).replace("manager reports running", "running"),
            server.tools,
            server.pid.map(|pid| format!(", PID {pid}")).unwrap_or_default()
        ));
    }
    if !tools.lsp_pooled.is_empty() {
        output.push_str("\n  Running language servers:\n");
    }
    for server in &tools.lsp_pooled {
        output.push_str(&format!(
            "    {}: {}{}\n",
            server.name,
            state_label(&server.state),
            server.pid.map(|pid| format!(", PID {pid}")).unwrap_or_default()
        ));
    }
    output.push_str(&format!("\n  Observed in: {scope}\n"));
    output
}
