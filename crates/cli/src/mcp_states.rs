//! Shared `dcl mcp state` and `/mcp state` implementation.
//!
//! Servers are found by id first and then by display name; the states file is
//! always keyed by the id, which is also what qualified tool names carry.
use anyhow::Result;
use deepseek_code_core::mcp::states::{self, McpState};
use deepseek_code_core::mcp::{McpServerManager, McpServerStatus};
use deepseek_code_core::runtime::RuntimePaths;
use std::collections::BTreeMap;

/// The configured server named by `query`, by id first and display name second.
fn find<'a>(servers: &'a [McpServerStatus], query: &str) -> Option<&'a McpServerStatus> {
    servers
        .iter()
        .find(|server| server.id == query)
        .or_else(|| servers.iter().find(|server| server.name == query))
}

fn line(states: &BTreeMap<String, McpState>, server: &McpServerStatus) -> String {
    format!(
        "{}  {}",
        server.name,
        states::shown(states, &server.id, server.enabled).label()
    )
}

/// Apply one state through `manager`: `off` disables and stops the server and
/// is not written to the states file, so the previous state comes back when it
/// is enabled again. Every other state is stored and enables the server when
/// it was disabled. The interactive session starts the server separately.
pub(crate) async fn apply(
    paths: &RuntimePaths,
    manager: &McpServerManager,
    server: &McpServerStatus,
    state: McpState,
) -> Result<()> {
    if state == McpState::Off {
        manager.set_server_enabled(&server.id, false).await?;
        if server.running {
            manager.stop_server(&server.id).await?;
        }
    } else {
        states::set(&paths.data_dir, &server.id, state)?;
        if !server.enabled {
            manager.set_server_enabled(&server.id, true).await?;
        }
    }
    Ok(())
}

/// Shared by `dcl mcp state` and `/mcp state`. No server: every `NAME  STATE`
/// line (configured servers without untouched presets, sorted by name).
/// Server only: its line. Server and state: apply it and return its line.
pub async fn state_command(
    paths: &RuntimePaths,
    manager: &McpServerManager,
    server: Option<&str>,
    state: Option<&str>,
) -> anyhow::Result<String> {
    let servers = manager.list_servers().await;
    let stored = states::load(&paths.data_dir);
    match (server, state) {
        (None, _) => {
            let mut listed: Vec<&McpServerStatus> = servers
                .iter()
                .filter(|server| !crate::mcp_menu::untouched_preset(server))
                .collect();
            listed.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(listed
                .iter()
                .map(|server| line(&stored, server))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        (Some(query), None) => {
            let found = find(&servers, query)
                .ok_or_else(|| anyhow::anyhow!("Unknown MCP server {query}"))?;
            Ok(line(&stored, found))
        }
        (Some(query), Some(label)) => {
            let found = find(&servers, query)
                .ok_or_else(|| anyhow::anyhow!("Unknown MCP server {query}"))?
                .clone();
            let update: McpState = label.parse()?;
            apply(paths, manager, &found, update).await?;
            let stored = states::load(&paths.data_dir);
            Ok(format!(
                "{}  {}",
                found.name,
                states::shown(&stored, &found.id, update != McpState::Off).label()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, RuntimePaths, McpServerManager) {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::write(
            paths.data_dir.join("mcp-servers.json"),
            serde_json::json!({
                "mcpServers": {
                    "gh": {"name": "GitHub", "command": "fixture", "enabled": true},
                    "fs": {"name": "filesystem", "command": "fixture", "enabled": false}
                }
            })
            .to_string(),
        )
        .unwrap();
        let manager = McpServerManager::new(paths.data_dir.join("mcp-servers.json"));
        manager.initialize().await.unwrap();
        (temp, paths, manager)
    }

    #[tokio::test]
    async fn mcp_state_command_sets_lists_and_rejects_unknown() {
        let (_temp, paths, manager) = fixture().await;

        // The display name is accepted, the id is stored.
        let set = state_command(&paths, &manager, Some("GitHub"), Some("on-demand"))
            .await
            .unwrap();
        assert_eq!(set, "GitHub  on-demand");
        assert_eq!(
            states::load(&paths.data_dir).get("gh"),
            Some(&McpState::OnDemand)
        );

        let listing = state_command(&paths, &manager, None, None).await.unwrap();
        assert!(
            listing.lines().any(|line| line == "GitHub  on-demand"),
            "{listing}"
        );
        assert!(
            listing.lines().any(|line| line == "filesystem  off"),
            "{listing}"
        );

        let error = state_command(&paths, &manager, Some("nope"), Some("off"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unknown MCP server nope"), "{error}");

        let error = state_command(&paths, &manager, Some("GitHub"), Some("bogus"))
            .await
            .unwrap_err()
            .to_string();
        for label in ["off", "user", "on-demand", "user-only"] {
            assert!(error.contains(label), "{error}");
        }

        // `off` flips the enabled switch and leaves the stored state alone.
        let off = state_command(&paths, &manager, Some("GitHub"), Some("off"))
            .await
            .unwrap();
        assert_eq!(off, "GitHub  off");
        assert!(!manager.get_server("gh").await.unwrap().enabled);
        assert_eq!(
            states::load(&paths.data_dir).get("gh"),
            Some(&McpState::OnDemand)
        );
        assert_eq!(
            state_command(&paths, &manager, Some("GitHub"), None)
                .await
                .unwrap(),
            "GitHub  off"
        );

        // Setting a state on a disabled server enables it.
        let user = state_command(&paths, &manager, Some("filesystem"), Some("user"))
            .await
            .unwrap();
        assert_eq!(user, "filesystem  user");
        assert!(manager.get_server("fs").await.unwrap().enabled);
        assert_eq!(
            state_command(&paths, &manager, Some("fs"), None)
                .await
                .unwrap(),
            "filesystem  user"
        );

        // The state stored before `off` comes back when the server is enabled.
        assert_eq!(
            state_command(&paths, &manager, Some("gh"), Some("on-demand"))
                .await
                .unwrap(),
            "GitHub  on-demand"
        );
        assert_eq!(
            state_command(&paths, &manager, Some("gh"), None)
                .await
                .unwrap(),
            "GitHub  on-demand"
        );

        manager.shutdown_all().await.unwrap();
    }
}
