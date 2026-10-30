//! `/mcp` as in the reference CLI: the session's servers with their state,
//! and per server its tools, Reconnect, and Enable/Disable.
use crate::session::Session;
use crate::terminal::picker::{Entry, Picker};
use anyhow::Result;
use deepseek_code_core::mcp::McpServerStatus;

fn state(server: &McpServerStatus) -> String {
    match (server.enabled, server.running, &server.error) {
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

/// A bundled preset nobody has turned on or changed; the reference CLI has
/// no presets, so these stay out of the list.
fn untouched_preset(server: &McpServerStatus) -> bool {
    !server.enabled
        && deepseek_code_core::mcp::default_servers()
            .iter()
            .any(|preset| preset.id == server.id && preset.command == server.command && preset.args == server.args)
}

pub async fn run(session: &Session) -> Result<()> {
    session.ensure_mcp().await?;
    let mut selected = 0;
    loop {
        let all = session.mcp.list_servers().await;
        let hidden = all.iter().filter(|server| untouched_preset(server)).count();
        let servers: Vec<McpServerStatus> = all.into_iter().filter(|server| !untouched_preset(server)).collect();
        if servers.is_empty() {
            eprintln!("No MCP servers configured. Run /doctor if this is unexpected. Otherwise, run dcl mcp --help to learn how to add one{}.", if hidden > 0 { format!(", or /mcp list to see the {hidden} bundled presets") } else { String::new() });
            return Ok(());
        }
        let entries: Vec<Entry> = servers
            .iter()
            .map(|server| Entry { title: server.name.clone(), detail: state(server) })
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
            let tools: Vec<Entry> = session
                .mcp
                .get_all_tools()
                .await
                .into_iter()
                .filter(|tool| tool.server_name == server.name || tool.server_name == server.id)
                .map(|tool| Entry { title: tool.name, detail: tool.description.lines().next().unwrap_or_default().to_owned() })
                .collect();
            let heading = format!("Tools for {}", server.name);
            let count = format!("{} tool{}", tools.len(), if tools.len() == 1 { "" } else { "s" });
            Picker { heading: &heading, subtitle: Some(&count), entries: &tools, search: true, hint: "Type to search \u{b7} Esc to go back", selected: 0 }.run()?;
        }
        "Reconnect" => match session.mcp.restart_server(&server.id).await {
            Ok(()) => eprintln!("Reconnected to {}.", server.name),
            Err(error) => eprintln!("Failed to reconnect to {}: {error:#}", server.name),
        },
        "Disable" => {
            session.mcp.set_server_enabled(&server.id, false).await?;
            if server.running {
                session.mcp.stop_server(&server.id).await?;
            }
            eprintln!("Disabled {}.", server.name);
        }
        _ => {
            session.mcp.set_server_enabled(&server.id, true).await?;
            match session.mcp.start_server(&server.id).await {
                Ok(()) => eprintln!("Enabled {}.", server.name),
                Err(error) => eprintln!("Enabled {}, but it failed to start: {error:#}", server.name),
            }
        }
    }
    Ok(())
}
