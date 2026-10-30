//! Persisted CLI presentation and inspection commands.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use deepseek_code_core::runtime::RuntimePaths;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    pub color: String,
    pub focus: bool,
    pub output_style: String,
    pub statusline: Option<String>,
    pub tui: String,
    pub diff_panel: bool,
    pub diff_staged: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            color: "default".into(),
            focus: false,
            output_style: "default".into(),
            tui: "default".into(),
            statusline: None,
            diff_panel: false,
            diff_staged: false,
        }
    }
}

pub fn load(session: &Session) -> Result<Settings> {
    let mut settings = session.store.db.with_conn(|conn| -> Result<Settings> {
        let mut stmt = conn.prepare("SELECT value FROM peripheral_settings WHERE key=?1")?;
        let mut rows = stmt.query([format!("cli.display.{}", session.id)])?;
        Ok(match rows.next()? {
            Some(row) => serde_json::from_str(&row.get::<_, String>(0)?)?,
            None => Settings::default(),
        })
    })??;
    if let Some(global) = deepseek_code_core::services::statusline_setup::load(&session.paths.data_dir)? { settings.statusline = global.command; }
    Ok(settings)
}

fn save(session: &Session, settings: &Settings) -> Result<()> {
    let value = serde_json::to_string(settings)?;
    session.store.db.with_conn(|conn| conn.execute(
        "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [format!("cli.display.{}", session.id), value],
    ))??;
    crate::terminal::style::configure(&settings.theme, &settings.color);
    Ok(())
}

pub fn style_prompt(session: &Session) -> Result<String> {
    let style = load(session)?.output_style;
    Ok(match style.as_str() {
        "default" => String::new(),
        "explanatory" => "\nOutput style: explain relevant implementation decisions and useful codebase insights alongside completing the requested work.\n".into(),
        "learning" => "\nOutput style: guide the user through the reasoning and use focused questions or small exercises when helpful to learning; still complete work they explicitly delegate.\n".into(),
        custom => {
            let path = session.workspace.join(".claude/output-styles").join(format!("{custom}.md"));
            ensure!(path.metadata()?.len() <= 65536, "Output style exceeds 64 KiB");
            format!("\nUser-selected output style:\n{}\n", std::fs::read_to_string(path)?)
        }
    })
}

pub async fn handle(
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    let argument = argument.trim();
    match command {
        "/doctor" if argument.is_empty() || argument == "status" => {
            print!(
                "{}",
                crate::doctor_commands::snapshot(&session.workspace, paths)
            );
        }
        "/debug" => {
            match argument {
                "" | "on" => crate::debug_logging::configure(Some(
                    paths.data_dir.join(format!("debug-{}.log", session.id)),
                ))?,
                "off" => crate::debug_logging::configure(None)?,
                "status" => {}
                _ => anyhow::bail!("Usage: /debug [on|off|status]"),
            }
            match crate::debug_logging::destination() {
                Some(path) => eprintln!("Debug logging: {}", path.display()),
                None => eprintln!("Debug logging is off."),
            }
        }
        "/tui" => {
            let mut settings = load(session)?;
            if argument.is_empty() {
                eprintln!(
                    "Terminal renderer: {}. /tui default|fullscreen",
                    settings.tui
                );
            } else {
                ensure!(
                    ["default", "fullscreen"].contains(&argument),
                    "Usage: /tui default|fullscreen"
                );
                settings.tui = argument.into();
                save(session, &settings)?;
            }
        }
        "/context" => {
            ensure!(argument.is_empty(), "Usage: /context");
            crate::context_commands::show(session).await?;
        }
        "/keybindings" => {
            ensure!(argument.is_empty(), "Usage: /keybindings");
            let path = paths.data_dir.join("keybindings.json");
            crate::keybindings::ensure_file(&path)?;
            edit_file(&path, "").await?;
            crate::keybindings::load(&path)?;
            eprintln!("Keybindings validated; they take effect at the next prompt.");
        }
        "/ide" => {
            if crate::ide_commands::handle(session, argument).await? { return Ok(true); }
            let context = crate::services::context(paths)?;
            let root = session.workspace.to_string_lossy();
            deepseek_code_core::tools::configure_lsp_servers(
                &root,
                deepseek_code_core::services::plugins::all_language_servers(&context)?,
            )?;
            let words = split_arguments(argument)?;
            if words.is_empty() || words == ["status"] {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&deepseek_code_core::tools::lsp_status(&root).await?)?
                );
                eprintln!("LSP language services. Use /ide lsp FILE OPERATION [LINE CHARACTER] (1-based).");
            } else {
                ensure!(
                    words.first().is_some_and(|s| s == "lsp")
                        && (words.len() == 3 || words.len() == 5),
                    "Usage: /ide [status|lsp FILE OPERATION [LINE CHARACTER]]"
                );
                let mut params = json!({"filePath":words[1],"operation":words[2]});
                if words.len() == 5 {
                    params["line"] =
                        json!(words[3].parse::<u32>().context("Invalid line number")?);
                    params["character"] = json!(words[4]
                        .parse::<u32>()
                        .context("Invalid character number")?);
                }
                let result = tokio::select! {
                    result = deepseek_code_core::tools::execute_tool_async("LSP", params, &root) => result?,
                    _ = tokio::signal::ctrl_c() => anyhow::bail!("LSP query cancelled"),
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
        }
        "/theme" | "/color" | "/focus" | "/output-style" | "/statusline" => {
            let mut settings = load(session)?;
            match command {
                "/theme" => {
                    if argument.is_empty() {
                        eprintln!("Theme: {}. Available: dark, light, dark-daltonized, light-daltonized, dark-ansi, light-ansi", settings.theme);
                        return Ok(true);
                    }
                    ensure!(
                        [
                            "dark",
                            "light",
                            "dark-daltonized",
                            "light-daltonized",
                            "dark-ansi",
                            "light-ansi"
                        ]
                        .contains(&argument),
                        "Unknown theme"
                    );
                    settings.theme = argument.into();
                }
                "/color" => {
                    if argument.is_empty() {
                        eprintln!("Prompt color: {}. Available: default, red, orange, yellow, green, blue, purple, pink, cyan", settings.color);
                        return Ok(true);
                    }
                    ensure!(
                        [
                            "default", "red", "orange", "yellow", "green", "blue", "purple",
                            "pink", "cyan"
                        ]
                        .contains(&argument),
                        "Unknown prompt color"
                    );
                    settings.color = argument.into();
                }
                "/focus" => {
                    settings.focus = match argument {
                        "" => !settings.focus,
                        "on" => true,
                        "off" => false,
                        _ => anyhow::bail!("Usage: /focus [on|off]"),
                    };
                }
                "/output-style" => {
                    if argument.is_empty() {
                        eprintln!(
                            "Output style: {}\n  default\n  explanatory\n  learning",
                            settings.output_style
                        );
                        if let Ok(entries) =
                            std::fs::read_dir(session.workspace.join(".claude/output-styles"))
                        {
                            for entry in entries
                                .flatten()
                                .filter(|e| e.path().extension().is_some_and(|s| s == "md"))
                            {
                                eprintln!(
                                    "  {}",
                                    entry.path().file_stem().unwrap().to_string_lossy()
                                );
                            }
                        }
                        return Ok(true);
                    }
                    ensure!(
                        !argument.is_empty()
                            && argument
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
                        "Invalid output style name"
                    );
                    if !["default", "explanatory", "learning"].contains(&argument) {
                        ensure!(
                            session
                                .workspace
                                .join(".claude/output-styles")
                                .join(format!("{argument}.md"))
                                .is_file(),
                            "Output style not found"
                        );
                    }
                    settings.output_style = argument.into();
                }
                "/statusline" => {
                    anyhow::bail!("Use the interactive /statusline setup, command, off or show command");
                }
                _ => unreachable!(),
            }
            save(session, &settings)?;
            eprintln!(
                "{} updated for this session.",
                command.trim_start_matches('/')
            );
        }
        "/diff" => {
            let mut settings = load(session)?;
            match argument {
                "" => settings.diff_panel = !settings.diff_panel,
                "off" => settings.diff_panel = false,
                "on" => {
                    settings.diff_panel = true;
                    settings.diff_staged = false;
                }
                "staged" => {
                    settings.diff_panel = true;
                    settings.diff_staged = true;
                }
                _ => anyhow::bail!("Usage: /diff [on|off|staged]"),
            }
            if settings.diff_panel {
                deepseek_code_core::services::worktree_diff::snapshot(
                    &session.workspace,
                    settings.diff_staged,
                )
                .await?;
            }
            save(session, &settings)?;
            eprintln!("Diff panel {}. Ctrl+O expands or collapses; PgUp/PgDn scrolls. /diff toggles the panel.", if settings.diff_panel { "on" } else { "off" });
        }
        // /cost is the reference CLI's name for the same session cost view.
        "/usage" | "/cost" => {
            ensure!(matches!(argument,""|"local"|"refresh"), "Usage: /usage [local|refresh]");
            let mut value=usage(session)?;
            value["billing"]=deepseek_code_core::services::usage::session_cost(&session.store.db,&session.id,&paths.read_config()?)?;
            if argument!="local" {
                match deepseek_code_core::services::usage::plan(&paths.data_dir).await {
                    Ok(Some(plan))=>value["account_plan"]=serde_json::to_value(plan)?,
                    Ok(None)=>value["account_plan_status"]=json!("Requires this application's Anthropic login with profile scope; third-party provider quotas are not available."),
                    Err(error)=>value["account_plan_error"]=json!(error.to_string()),
                }
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        "/memory" => {
            let paths_by_scope = memory_paths(&paths.data_dir, &session.workspace);
            if argument.trim().is_empty() && crate::terminal::picker::available() {
                // As in the reference CLI: pick the instructions file to edit.
                use crate::terminal::picker::{Entry, Picker};
                let describe = |path: &std::path::Path, place: &str| match std::fs::metadata(path) {
                    Ok(metadata) => format!("{place} {} \u{b7} {} bytes", crate::terminal::home_path(path), metadata.len()),
                    Err(_) => format!("{place} {} \u{b7} not created yet", crate::terminal::home_path(path)),
                };
                let entries = vec![
                    Entry { title: "User instructions".into(), detail: describe(&paths_by_scope[0].1, "Saved in") },
                    Entry { title: "Project instructions".into(), detail: describe(&paths_by_scope[1].1, "Checked in at") },
                    Entry { title: "Local instructions".into(), detail: describe(&paths_by_scope[2].1, "Not checked in:") },
                    Entry { title: "Show loaded project instructions".into(), detail: String::new() },
                ];
                let picker = Picker { heading: "Memory", subtitle: Some("Instructions DeepSeek Code reads in every session"), entries: &entries, search: false, hint: "Enter to open \u{b7} Esc to cancel", selected: 0 };
                let scope = match picker.run()? {
                    Some(0) => "user",
                    Some(1) => "project",
                    Some(2) => "local",
                    Some(3) => "show",
                    _ => return Ok(true),
                };
                return Box::pin(handle("/memory", scope, paths, session)).await;
            }
            match argument.trim() {
                "" | "list" => {
                    for (scope, path) in &paths_by_scope {
                        let status = match std::fs::metadata(path) {
                            Ok(metadata) => format!("{} bytes", metadata.len()),
                            Err(_) => "missing".into(),
                        };
                        eprintln!("{scope:7} {} [{status}]", path.display());
                    }
                    eprintln!("Use /memory user|project|local to open a file (created if missing); /memory show prints loaded project instructions.");
                }
                "show" => {
                    let instructions = deepseek_code_core::project_instructions::render(
                        &deepseek_code_core::project_instructions::load_all(Some(&paths.data_dir), &session.workspace)?,
                    );
                    if instructions.is_empty() {
                        eprintln!("No project instructions found. /memory project opens the file.");
                    } else {
                        println!("{instructions}");
                    }
                }
                "user" => {
                    let (_, path) = &paths_by_scope[0];
                    edit_file(path, "# User instructions\n\n").await?;
                    eprintln!("Instructions will be loaded on the next session.");
                }
                "project" | "edit" => {
                    let (_, path) = &paths_by_scope[1];
                    edit_file(path, "# Project instructions\n\n").await?;
                    eprintln!("Instructions will be loaded on the next turn.");
                }
                "local" => {
                    let (_, path) = &paths_by_scope[2];
                    edit_file(path, "# Local project instructions\n\n").await?;
                    eprintln!("Instructions will be loaded on the next turn.");
                }
                _ => anyhow::bail!("Usage: /memory [list|show|user|project|local|edit]"),
            }
        }
        "/plugin" => {
            use crate::args::{Cli, Commands};
            use clap::Parser;
            let mut words = split_arguments(argument)?;
            if words.is_empty() {
                words.push("list".into());
            }
            let parsed = Cli::try_parse_from(
                ["dcl".into(), "plugin".into()]
                    .into_iter()
                    .chain(words),
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let Some(Commands::Plugin { action }) = parsed.command else {
                unreachable!()
            };
            crate::plugins::run(paths, &action).await?;
        }
        "/reload-plugins" => {
            ensure!(argument.is_empty(), "Usage: /reload-plugins");
            session.reload_plugins().await?;
            let context = crate::services::context(paths)?;
            let plugins = deepseek_code_core::services::plugins::list(&context)?;
            eprintln!("Reloaded {} enabled plugins, including commands, hooks, MCP servers and language services.", plugins.iter().filter(|plugin| plugin.enabled).count());
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn usage(session: &Session) -> Result<Value> {
    session.store.db.with_conn(|conn| -> Result<Value> {
        Ok(conn.query_row("SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(total_duration_ms) FROM model_requests WHERE conversation_id=?1", [&session.id], |row| {
            Ok(json!({"session_id":session.id,"requests":row.get::<_,i64>(0)?,"input_tokens":row.get::<_,Option<i64>>(1)?,"output_tokens":row.get::<_,Option<i64>>(2)?,"cache_read_tokens":row.get::<_,Option<i64>>(3)?,"cache_write_tokens":row.get::<_,Option<i64>>(4)?,"duration_ms":row.get::<_,Option<i64>>(5)?}))
        })?)
    })?
}

/// Quotes group arguments; backslashes remain literal for Windows paths.
pub fn split_arguments(text: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in text.chars() {
        if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
            started = true;
        } else if quote.is_none() && ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    ensure!(quote.is_none(), "Unclosed quote");
    if started {
        words.push(word);
    }
    Ok(words)
}

/// Upstream MemoryFileSelector scopes: user, project and local files.
pub(crate) fn memory_paths(
    data_dir: &Path,
    workspace: &Path,
) -> [(&'static str, std::path::PathBuf); 3] {
    [
        ("user", data_dir.join("CLAUDE.md")),
        ("project", workspace.join("CLAUDE.md")),
        ("local", workspace.join("CLAUDE.local.md")),
    ]
}

pub(crate) async fn edit_file(path: &Path, initial: &str) -> Result<()> {
    if !path.exists() {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?
            .write_all(initial.as_bytes())?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad.exe".into()
            } else {
                "vi".into()
            }
        });
    let words = split_arguments(&editor)?;
    let executable = words.first().context("Editor command is empty")?;
    let status = tokio::process::Command::new(executable)
        .args(&words[1..])
        .arg(path)
        .kill_on_drop(true)
        .status()
        .await
        .context("Opening editor")?;
    ensure!(status.success(), "Editor exited with {status}");
    Ok(())
}

pub async fn statusline(session: &Session, settings: &Settings) -> Result<Option<String>> {
    let Some(script) = settings.statusline.as_ref() else {
        return Ok(None);
    };
    let payload = crate::statusline_commands::payload(session).await?;
    let result =
        deepseek_code_core::services::statusline::run(script, &session.workspace, &payload).await?;
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_paths_cover_user_project_and_local_scopes() {
        let paths = memory_paths(Path::new("/data"), Path::new("/work"));
        assert_eq!(paths[0].0, "user");
        assert!(paths[0].1.ends_with("CLAUDE.md"));
        assert!(paths[0].1.starts_with("/data"));
        assert_eq!(paths[1].0, "project");
        assert!(paths[1].1.starts_with("/work"));
        assert_eq!(paths[2].0, "local");
        assert!(paths[2].1.ends_with("CLAUDE.local.md"));
    }

    #[test]
    fn argument_parser_preserves_windows_paths_and_rejects_unclosed_quotes() {
        assert_eq!(
            split_arguments(r#"install "D:\some project\plugin.json""#).unwrap(),
            ["install", r"D:\some project\plugin.json"]
        );
        assert!(split_arguments("install 'unfinished").is_err());
    }
}
