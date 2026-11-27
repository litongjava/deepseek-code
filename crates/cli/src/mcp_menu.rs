//! `/mcp` as in the reference CLI: the session's servers with their state,
//! and per server its tools, Reconnect, and Enable/Disable.
use crate::session::Session;
use crate::terminal::picker::{Entry, Picker};
use anyhow::Result;
use deepseek_code_core::mcp::McpServerStatus;
use deepseek_code_core::services::session_worker::{McpOp, McpResult};

/// The error the `/mcp` menu shows for a failed server, if any.
pub(crate) fn failure(server: &McpServerStatus) -> Option<&str> {
    match (server.enabled, server.error.as_deref()) {
        (true, Some(error)) => Some(error),
        _ => None,
    }
}

fn state(server: &McpServerStatus) -> String {
    match (server.enabled, server.running, failure(server)) {
        (false, _, _) => "\u{25cb} disabled".into(),
        (true, _, Some(error)) => format!("\u{2718} failed \u{b7} {}", error.lines().next().unwrap_or_default()),
        (true, true, None) => format!(
            "\u{2714} connected \u{b7} {} tool{}",
            server.tools_count,
            if server.tools_count == 1 { "" } else { "s" }
        ),
        (true, false, None) => "\u{25cb} not running".into(),
    }
}

/// Turn a server off, as this menu's Disable action does.
pub(crate) async fn disable(session: &Session, server: &McpServerStatus) -> Result<()> {
    session.mcp.set_server_enabled(&server.id, false).await?;
    if server.running {
        session.mcp.stop_server(&server.id).await?;
    }
    Ok(())
}

/// Turn a server on and start it, as this menu's Enable action does.
pub(crate) async fn enable(session: &Session, server: &McpServerStatus) -> Result<()> {
    session.mcp.set_server_enabled(&server.id, true).await?;
    session.mcp.start_server(&server.id).await
}

/// A bundled preset nobody has turned on or changed; the reference CLI has
/// no presets, so these stay out of the list.
pub(crate) fn untouched_preset(server: &McpServerStatus) -> bool {
    !server.enabled
        && deepseek_code_core::mcp::default_servers()
            .iter()
            .any(|preset| preset.id == server.id && preset.command == server.command && preset.args == server.args)
}

pub async fn run(session: &Session) -> Result<()> {
    let mut selected = 0;
    loop {
        // A worker-backed session lists the servers its worker actually owns.
        let all = match session.mcp_call(McpOp::ListServers).await? {
            McpResult::Servers { servers, .. } => servers,
            _ => anyhow::bail!("Session worker returned an unexpected MCP result"),
        };
        let hidden = all.iter().filter(|server| untouched_preset(server)).count();
        let servers: Vec<McpServerStatus> = all.into_iter().filter(|server| !untouched_preset(server)).collect();
        if servers.is_empty() {
            eprintln!("No MCP servers configured. Run /doctor if this is unexpected. Otherwise, run dcl mcp --help to learn how to add one{}.", if hidden > 0 { format!(", or /mcp list to see the {hidden} bundled presets") } else { String::new() });
            return Ok(());
        }
        let stored = deepseek_code_core::mcp::states::load(&session.paths.data_dir);
        let entries: Vec<Entry> = servers
            .iter()
            .map(|server| Entry {
                title: server.name.clone(),
                detail: format!(
                    "{} \u{b7} {}",
                    state(server),
                    deepseek_code_core::mcp::states::shown(
                        &stored,
                        &server.id,
                        server.enabled
                    )
                    .label()
                ),
            })
            .collect();
        let mut count = format!("{} server{}", servers.len(), if servers.len() == 1 { "" } else { "s" });
        if hidden > 0 {
            count.push_str(&format!(" \u{b7} {hidden} disabled presets hidden (/mcp list)"));
        }
        let picker = Picker {
            heading: "Manage MCP servers",
            subtitle: Some(&count),
            entries: &entries,
            search: false,
            hint: "Enter to view \u{b7} Esc to close",
            selected,
        };
        let Some(index) = picker.run()? else {
            return Ok(());
        };
        selected = index;
        server(session, &servers[index]).await?;
    }
}

async fn server(session: &Session, server: &McpServerStatus) -> Result<()> {
    let mut actions = Vec::new();
    if server.enabled {
        if server.running {
            actions.push("View tools");
        }
        actions.push("Set state");
        actions.push("Reconnect");
        actions.push("Disable");
    } else {
        actions.push("Enable");
    }
    let entries: Vec<Entry> = actions.iter().map(|action| Entry { title: action.to_string(), detail: String::new() }).collect();
    let heading = format!("{} MCP Server", server.name);
    let subtitle = format!("{} \u{b7} {} {}", state(server), server.command, server.args.join(" "));
    let picker = Picker {
        heading: &heading,
        subtitle: Some(subtitle.trim_end()),
        entries: &entries,
        search: false,
        hint: "Enter to select \u{b7} Esc to go back",
        selected: 0,
    };
    let Some(choice) = picker.run()? else {
        return Ok(());
    };
    match actions[choice] {
        "View tools" => {
            let tools = match session.mcp_call(McpOp::ListServers).await? {
                McpResult::Servers { tools, .. } => tools,
                _ => anyhow::bail!("Session worker returned an unexpected MCP result"),
            };
            let tools: Vec<Entry> = tools
                .into_iter()
                .filter(|tool| tool.server_name == server.name || tool.server_name == server.id)
                .map(|tool| Entry { title: tool.name, detail: tool.description.lines().next().unwrap_or_default().to_owned() })
                .collect();
            let heading = format!("Tools for {}", server.name);
            let count = format!("{} tool{}", tools.len(), if tools.len() == 1 { "" } else { "s" });
            Picker { heading: &heading, subtitle: Some(&count), entries: &tools, search: true, hint: "Type to search \u{b7} Esc to go back", selected: 0 }.run()?;
        }
        "Set state" => {
            let stored = deepseek_code_core::mcp::states::load(&session.paths.data_dir);
            let current =
                deepseek_code_core::mcp::states::shown(&stored, &server.id, server.enabled);
            match crate::plugins_panel::pick_mcp_state(&server.name, current)? {
                Some(crate::plugins_panel::McpStateChoice::State(state)) => {
                    let label = state.label().to_owned();
                    let result = session
                        .mcp_call(McpOp::State {
                            server: Some(server.id.clone()),
                            state: Some(label.clone()),
                        })
                        .await?;
                    match result {
                        McpResult::State { failure: Some(failure), .. } => eprintln!(
                            "{} is now {}; it failed to start: {failure}",
                            server.name, label
                        ),
                        McpResult::State { .. } => {
                            eprintln!("{} is now {}.", server.name, label)
                        }
                        _ => anyhow::bail!("Session worker returned an unexpected MCP result"),
                    }
                }
                Some(crate::plugins_panel::McpStateChoice::Details) => eprintln!(
                    "{} \u{b7} {} \u{b7} {} tool{}",
                    server.name,
                    current.label(),
                    server.tools_count,
                    if server.tools_count == 1 { "" } else { "s" }
                ),
                None => {}
            }
        }
        "Reconnect" => {
            let stored = deepseek_code_core::mcp::states::load(&session.paths.data_dir);
            let current =
                deepseek_code_core::mcp::states::shown(&stored, &server.id, true);
            let target = if current == deepseek_code_core::mcp::states::McpState::Off {
                deepseek_code_core::mcp::states::McpState::User
            } else {
                current
            };
            let restart = async {
                session
                    .mcp_call(McpOp::State {
                        server: Some(server.id.clone()),
                        state: Some("off".into()),
                    })
                    .await?;
                session
                    .mcp_call(McpOp::State {
                        server: Some(server.id.clone()),
                        state: Some(target.label().into()),
                    })
                    .await
            }
            .await;
            match restart {
                Ok(McpResult::State { failure: Some(failure), .. }) => {
                    eprintln!("Failed to reconnect to {}: {failure}", server.name)
                }
                Ok(_) => eprintln!("Reconnected to {}.", server.name),
                Err(error) => eprintln!("Failed to reconnect to {}: {error:#}", server.name),
            }
        }
        "Disable" => {
            let result = session
                .mcp_call(McpOp::State {
                    server: Some(server.id.clone()),
                    state: Some("off".into()),
                })
                .await?;
            match result {
                McpResult::State { .. } => eprintln!("Disabled {}.", server.name),
                _ => anyhow::bail!("Session worker returned an unexpected MCP result"),
            }
        }
        _ => {
            // Enabling restores the state the user stored before turning the
            // server off; a stored `off` still means "on" once enabled.
            let stored = deepseek_code_core::mcp::states::load(&session.paths.data_dir);
            let current =
                deepseek_code_core::mcp::states::shown(&stored, &server.id, true);
            let target = if current == deepseek_code_core::mcp::states::McpState::Off {
                deepseek_code_core::mcp::states::McpState::User
            } else {
                current
            };
            match session
                .mcp_call(McpOp::State {
                    server: Some(server.id.clone()),
                    state: Some(target.label().into()),
                })
                .await
            {
                Ok(McpResult::State { failure: Some(failure), .. }) => eprintln!(
                    "Enabled {}, but it failed to start: {failure}",
                    server.name
                ),
                Ok(McpResult::State { .. }) => eprintln!("Enabled {}.", server.name),
                Ok(_) => anyhow::bail!("Session worker returned an unexpected MCP result"),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}
