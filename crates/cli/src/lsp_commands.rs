//! `/lsp` and `dcl lsp`: language server status and opt-in managed installs.
use anyhow::{bail, Context, Result};
use claude_core::tools::lsp_servers;
use serde_json::Value;
use std::path::Path;

pub fn status_text(workspace: &Path, plugin_servers: &[Value]) -> String {
    let mut text = String::from("Language servers (used by the LSP tool for definitions, references and diagnostics)\n");
    for row in lsp_servers::status(workspace) {
        let name = row["name"].as_str().unwrap_or_default();
        let server = row["server"].as_str().unwrap_or_default();
        if let Some(path) = row["path"].as_str() {
            text.push_str(&format!("  \u{2713} {name:<22} {server:<28} {path}\n"));
        } else {
            text.push_str(&format!(
                "  \u{2717} {name:<22} {server:<28} not installed \u{b7} {}\n",
                row["install"].as_str().unwrap_or_default()
            ));
        }
    }
    let plugins: Vec<&Value> = plugin_servers.iter().collect();
    if !plugins.is_empty() {
        text.push_str("From plugins:\n");
        for server in plugins {
            let extensions = server["extensionToLanguage"]
                .as_object()
                .map(|map| map.keys().cloned().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            let command = server["command"].as_str().unwrap_or_default();
            let found = lsp_servers::resolve_command(command, workspace).is_some();
            text.push_str(&format!(
                "  {} {:<48} {command:<28} {extensions}\n",
                if found { "\u{2713}" } else { "\u{2717}" },
                server["name"].as_str().unwrap_or_default().trim_start_matches("claude-code:"),
            ));
        }
    }
    if let Ok(dir) = lsp_servers::install_dir() {
        text.push_str(&format!("Managed installs: {}", dir.display()));
    }
    text
}

/// The confirmation question for installing `language`.
pub fn install_question(language: &lsp_servers::Language) -> Result<String> {
    if !lsp_servers::can_install(language) {
        bail!(
            "{} ({}) cannot be installed automatically; install it with: {}",
            language.name,
            language.command,
            lsp_servers::install_hint(language)
        );
    }
    Ok(format!(
        "Install {} for {}? This {}, under {}. [y/N]",
        language.command,
        language.name,
        lsp_servers::install_description(language),
        lsp_servers::install_dir()?.display()
    ))
}

pub fn language(id: &str) -> Result<&'static lsp_servers::Language> {
    lsp_servers::by_id(id).with_context(|| {
        let ids: Vec<_> = lsp_servers::LANGUAGES.iter().map(|language| language.id).collect();
        format!("Unknown language '{id}'; choose one of: {}", ids.join(", "))
    })
}

pub async fn install(language: &lsp_servers::Language) -> Result<()> {
    eprintln!("Installing {} for {}\u{2026}", language.command, language.name);
    let launch = tokio::select! {
        result = lsp_servers::install(language, &|line: &str| eprintln!("  {line}")) => result?,
        _ = tokio::signal::ctrl_c() => bail!("Installation cancelled"),
    };
    eprintln!("Installed. {} starts as {}", language.name, launch.command.display());
    Ok(())
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes" | "1")
}

/// `/lsp [status] | /lsp install LANGUAGE` in the REPL.
pub async fn handle(
    line: &str,
    session: &crate::session::Session,
    input: &mut crate::terminal::input::TerminalInput,
) -> Result<bool> {
    let mut words = line.split_whitespace();
    if words.next() != Some("/lsp") {
        return Ok(false);
    }
    match (words.next(), words.next()) {
        (None | Some("status" | "list"), None) => {
            let context = crate::services::context(&session.paths)?;
            let plugins = claude_core::services::plugins::all_language_servers(&context)?;
            eprintln!("{}", status_text(&session.workspace, &plugins));
        }
        (Some("install"), Some(id)) => {
            let language = language(id)?;
            let question = install_question(language)?;
            let answer = input.read_answer(&format!("{question} ")).await?.unwrap_or_default();
            if is_yes(&answer) {
                install(language).await?;
            } else {
                eprintln!("Not installed.");
            }
        }
        _ => bail!("Usage: /lsp [status] | /lsp install LANGUAGE"),
    }
    Ok(true)
}

/// `dcl lsp [status] | dcl lsp install LANGUAGE [--yes]`.
pub async fn command(paths: &claude_core::runtime::RuntimePaths, workspace: &Path, action: Option<&str>, id: Option<&str>, yes: bool) -> Result<()> {
    match (action, id) {
        (None | Some("status" | "list"), None) => {
            let context = crate::services::context(paths)?;
            let plugins = claude_core::services::plugins::all_language_servers(&context)?;
            println!("{}", status_text(workspace, &plugins));
            Ok(())
        }
        (Some("install"), Some(id)) => {
            let language = language(id)?;
            let question = install_question(language)?;
            if !yes {
                use std::io::{BufRead, IsTerminal, Write};
                anyhow::ensure!(std::io::stdin().is_terminal(), "Pass --yes to install without a terminal");
                eprint!("{question} ");
                std::io::stderr().flush()?;
                let mut answer = String::new();
                std::io::stdin().lock().read_line(&mut answer)?;
                if !is_yes(&answer) {
                    eprintln!("Not installed.");
                    return Ok(());
                }
            }
            install(language).await
        }
        _ => bail!("Usage: dcl lsp [status] | dcl lsp install LANGUAGE [--yes]"),
    }
}
