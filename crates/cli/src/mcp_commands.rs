use crate::args::McpCommands;
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{
    mcp::{McpServerConfig, McpServerManager},
    runtime::RuntimePaths,
    services::session_worker::{McpOp, McpResult},
};
use std::collections::HashMap;

pub(crate) fn operation(action: &McpCommands) -> Result<McpOp> {
    Ok(match action {
        McpCommands::ImportLegacy { source, preview } => McpOp::ImportLegacy {
            source: std::path::absolute(source)?,
            preview: *preview,
        },
        McpCommands::Hosts => McpOp::Hosts,
        McpCommands::Restart { name, host } => McpOp::Restart {
            server: name.clone(),
            host: host.clone(),
        },
        McpCommands::List => McpOp::List,
        McpCommands::State { server, state } => McpOp::State {
            server: server.clone(),
            state: state.clone(),
        },
        McpCommands::Remove { name } => McpOp::Remove {
            server: name.clone(),
        },
        McpCommands::Add {
            transport,
            headers,
            name,
            command,
            args,
        } => {
            let transport = transport.as_deref().map(|t| match t {
                "streamable-http" => "http",
                "websocket" => "ws",
                other => other,
            });
            ensure!(
                matches!(transport, None | Some("stdio" | "http" | "sse" | "ws")),
                "--transport must be stdio, http, sse or ws"
            );
            let remote = matches!(transport, Some("http" | "sse" | "ws"));
            ensure!(
                !remote || args.is_empty(),
                "Remote MCP servers take a URL, not command arguments"
            );
            ensure!(
                remote || headers.is_empty(),
                "--header applies to http, sse and ws servers"
            );
            let headers = headers
                .iter()
                .map(|header| {
                    header
                        .split_once(':')
                        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
                        .filter(|(key, _)| !key.is_empty())
                        .with_context(|| {
                            format!("Header must look like \"Name: value\", not {header}")
                        })
                })
                .collect::<Result<HashMap<_, _>>>()?;
            McpOp::Add {
                config: McpServerConfig {
                    id: name.clone(),
                    name: name.clone(),
                    command: if remote {
                        String::new()
                    } else {
                        command.clone()
                    },
                    args: args.clone(),
                    env: HashMap::new(),
                    cwd: None,
                    enabled: true,
                    transport: transport.filter(|_| remote).map(str::to_owned),
                    url: remote.then(|| command.clone()),
                    headers,
                },
            }
        }
    })
}

pub(crate) fn display(result: McpResult) -> Result<()> {
    match result {
        McpResult::State {
            text,
            failure: Some(error),
        } => eprintln!("{text}; it failed to start: {error}"),
        McpResult::State { text, .. } => println!("{text}"),
        McpResult::Json { value } => crate::services::print(&value)?,
        McpResult::Message { text } => eprintln!("{text}"),
        _ => anyhow::bail!("Unexpected MCP command reply"),
    }
    Ok(())
}

pub(crate) async fn execute(
    paths: &RuntimePaths,
    manager: &McpServerManager,
    op: McpOp,
    interactive: bool,
) -> Result<McpResult> {
    let value = match op {
        McpOp::Add { config } => {
            let id = config.id.clone();
            ensure!(
                manager.get_server(&id).await.is_none(),
                "MCP server already exists"
            );
            manager.add_server(config).await?;
            return Ok(McpResult::Message {
                text: format!("Added {id}"),
            });
        }
        McpOp::Remove { server } => {
            ensure!(
                manager.get_server(&server).await.is_some(),
                "MCP server not found"
            );
            manager.remove_server(&server).await?;
            return Ok(McpResult::Message {
                text: format!("Removed {server}"),
            });
        }
        McpOp::ImportLegacy { source, preview } => {
            serde_json::to_value(manager.import_legacy(&source, preview).await?)?
        }
        McpOp::List => {
            let mut value = serde_json::to_value(manager.list_servers().await)?;
            crate::config::redact(&mut value);
            value
        }
        McpOp::Hosts => serde_json::to_value(deepseek_code_core::mcp::control::hosts(
            &crate::services::context(paths)?,
        )?)?,
        McpOp::Restart { server, host } => {
            let owner = deepseek_code_core::mcp::control::restart(
                &crate::services::context(paths)?,
                &server,
                host.as_deref(),
            )
            .await?;
            serde_json::json!({"restarted": server, "host": owner})
        }
        McpOp::State { server, state } => {
            let text = crate::mcp_states::state_command(
                paths,
                manager,
                server.as_deref(),
                state.as_deref(),
            )
            .await?;
            let mut failure = None;
            if interactive && state.as_deref().is_some_and(|s| s != "off") {
                if let Some(found) = crate::mcp_states::find(
                    &manager.list_servers().await,
                    server.as_deref().unwrap_or_default(),
                ) {
                    if found.enabled {
                        failure = manager
                            .start_server(&found.id)
                            .await
                            .err()
                            .map(|e| format!("{e:#}"));
                    }
                }
            }
            return Ok(McpResult::State { text, failure });
        }
        McpOp::Reconnect { server } => {
            manager.restart_server(&server).await?;
            return Ok(McpResult::Message {
                text: format!("Reconnected to {server}."),
            });
        }
        McpOp::ListServers => {
            return Ok(McpResult::Servers {
                servers: manager.list_servers().await,
                tools: manager.get_all_tools().await,
            })
        }
        McpOp::ListPrompts => {
            return Ok(McpResult::Prompts {
                prompts: manager.get_all_prompts().await,
            })
        }
        McpOp::GetPrompt {
            server,
            name,
            arguments,
        } => {
            return Ok(McpResult::Prompt {
                text: manager.get_prompt(&server, &name, arguments).await?,
            })
        }
        McpOp::ReloadPlugins | McpOp::StopAll => anyhow::bail!("MCP operation requires an owner"),
    };
    Ok(McpResult::Json { value })
}
