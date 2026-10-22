use super::DoctorReport;
use crate::skills::discovery;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// Explicit observations make diagnostics testable without inspecting personal data.
pub struct Environment {
    pub user_config: Option<PathBuf>,
    pub executable: Option<PathBuf>,
    pub search_path: Vec<PathBuf>,
}
impl Environment {
    pub fn current() -> Self {
        Self {
            user_config: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".claude"))),
            executable: std::env::current_exe().ok(),
            search_path: std::env::var_os("PATH")
                .map(|value| std::env::split_paths(&value).take(128).collect())
                .unwrap_or_default(),
        }
    }
}
fn path_label(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(240)
        .collect()
}
fn read(path: &Path) -> Option<String> {
    (std::fs::metadata(path).ok()?.len() <= 1024 * 1024)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}
fn json(path: &Path) -> Option<Value> {
    serde_json::from_str(read(path)?.trim_start_matches('\u{feff}')).ok()
}

pub fn inspect(
    report: &mut DoctorReport,
    workspace: &Path,
    data: Option<&Path>,
    env: &Environment,
) {
    let (_, invalid) = discovery::discover_available_from(env.user_config.as_deref(), workspace);
    for path in invalid {
        let reason = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() > 1024 * 1024 => "Definition exceeds 1 MiB".to_owned(),
            Ok(_) => match read(&path) {
                None => "Cannot read definition as UTF-8".to_owned(),
                Some(text) => match discovery::frontmatter(&text) {
                    Err(error) => error
                        .downcast_ref::<serde_yaml::Error>()
                        .and_then(|error| error.location())
                        .map(|location| {
                            format!(
                                "Invalid YAML frontmatter at line {}, column {}",
                                location.line() + 1,
                                location.column()
                            )
                        })
                        .unwrap_or_else(|| "Invalid frontmatter mapping or delimiter".into()),
                    Ok(_) => "Invalid skill metadata types, name, or local plugin metadata".into(),
                },
            },
            Err(_) => "Cannot inspect definition or directory".to_owned(),
        };
        report.add(
            "skill definition",
            "error",
            &format!("{}: {reason}; definition skipped", path_label(&path)),
        );
    }
    let mut agents = HashMap::new();
    for root in env
        .user_config
        .iter()
        .cloned()
        .chain(std::iter::once(workspace.join(".claude")))
    {
        settings(report, &root.join("settings.json"));
        settings(report, &root.join("settings.local.json"));
        keybindings(report, &root.join("keybindings.json"));
        markdown_agents(report, &root.join("agents"), &mut agents);
    }
    if let Some(data) = data {
        settings(report, &data.join("settings.json"));
        keybindings(report, &data.join("keybindings.json"));
        persisted_agents(report, &data.join(crate::runtime::DATABASE_FILE));
        report.json("application MCP", &data.join("mcp-servers.json"));
    }
    installation(report, env);
}

fn settings(report: &mut DoctorReport, path: &Path) {
    if !path.exists() {
        return;
    }
    let Some(value) = json(path) else {
        report.add(
            "settings schema",
            "error",
            &format!(
                "{}: unreadable or invalid UTF-8/JSON (maximum 1 MiB)",
                path_label(path)
            ),
        );
        return;
    };
    let valid = value.is_object()
        && ["env", "permissions", "hooks"]
            .iter()
            .all(|key| value.get(key).is_none_or(Value::is_object))
        && ["allow", "deny", "ask"].iter().all(|key| {
            value
                .get("permissions")
                .and_then(|p| p.get(key))
                .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
        })
        && value.get("env").is_none_or(|v| {
            v.as_object()
                .is_some_and(|o| o.values().all(Value::is_string))
        });
    report.add("settings schema", if valid { "ok" } else { "error" }, &format!("{}: {}", path_label(path), if valid { "Known settings fields have valid types; values and full schema not verified" } else { "Expected object; env values must be strings, hooks/permissions objects, permission rules string arrays" }));
}

fn keybindings(report: &mut DoctorReport, path: &Path) {
    if !path.exists() {
        return;
    }
    let value = json(path);
    let blocks = value
        .as_ref()
        .and_then(|v| v.get("bindings"))
        .and_then(Value::as_array);
    let valid = blocks.is_some_and(|blocks| {
        blocks.len() <= 32
            && blocks.iter().all(|block| {
                matches!(block["context"].as_str(), Some("Chat" | "Global"))
                    && block["bindings"].as_object().is_some_and(|bindings| {
                        bindings.len() <= 256
                            && bindings.iter().all(|(key, action)| {
                                (1..=2).contains(&key.split_whitespace().count())
                                    && (action.is_null() || action.is_string())
                            })
                    })
            })
    });
    report.add(
        "keybindings structure",
        if valid { "ok" } else { "error" },
        &format!(
            "{}: {}",
            path_label(path),
            if valid {
                "Valid structure; host must validate supported keys/actions"
            } else {
                "Invalid bindings array/context/shortcut/action structure"
            }
        ),
    );
}

fn markdown_agents(report: &mut DoctorReport, root: &Path, names: &mut HashMap<String, PathBuf>) {
    if !root.exists() {
        return;
    }
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .take(512)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.add(
                    "agent definition",
                    "error",
                    "Cannot inspect an agent directory entry",
                );
                continue;
            }
        };
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let path = entry.path();
        let parsed = read(path).and_then(|text| discovery::frontmatter(&text).ok());
        let valid = parsed.as_ref().is_some_and(|(meta, body)| {
            meta["name"].as_str().is_some_and(|name| {
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            }) && meta["description"]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
                && !body.trim().is_empty()
                && meta.get("tools").is_none_or(|v| {
                    v.is_string() || v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
                })
        });
        if !valid {
            report.add("agent definition", "error", &format!("{}: invalid frontmatter/name/description/tools or empty prompt; source file not activated by this check", path_label(path)));
            continue;
        }
        let name = parsed.unwrap().0["name"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase();
        if super::super::agents::canonical(&name) != name
            || super::super::agents::builtins()
                .iter()
                .any(|d| d.id == name)
        {
            report.add(
                "agent collision",
                "warning",
                &format!(
                    "{}: name collides with a built-in agent or alias",
                    path_label(path)
                ),
            );
        }
        if let Some(previous) = names.insert(name, path.into()) {
            report.add(
                "agent collision",
                "warning",
                &format!(
                    "{} and {}: same declared name; source files inspected, not imported",
                    path_label(&previous),
                    path_label(path)
                ),
            );
        }
    }
}

fn persisted_agents(report: &mut DoctorReport, db: &Path) {
    if !db.is_file() {
        return;
    }
    let result = (|| -> anyhow::Result<Option<String>> {
        let connection =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(std::time::Duration::from_millis(250))?;
        Ok(connection
            .query_row(
                "SELECT value FROM peripheral_settings WHERE key='agent-definitions'",
                [],
                |row| row.get(0),
            )
            .optional()?)
    })();
    match result {
        Ok(None) => report.add("stored agents", "absent", "No custom definitions stored"),
        Ok(Some(raw)) => {
            let definitions = (raw.len() <= 1024 * 1024)
                .then(|| serde_json::from_str::<Vec<super::super::agents::Definition>>(&raw).ok())
                .flatten();
            let Some(definitions) = definitions else {
                report.add(
                    "stored agents",
                    "error",
                    "Invalid or oversized definition array; raw content withheld",
                );
                return;
            };
            let mut names = std::collections::HashSet::new();
            for (index, definition) in definitions.iter().enumerate() {
                if super::super::agents::validate_custom(definition).is_err()
                    || !names.insert(definition.id.to_ascii_lowercase())
                {
                    report.add(
                        "stored agent definition",
                        "error",
                        &format!(
                            "Entry {}: invalid schema or colliding ID; definition values withheld",
                            index + 1
                        ),
                    );
                }
            }
        }
        Err(_) => report.add(
            "stored agents",
            "not checked",
            "Read-only definition query failed or database busy",
        ),
    }
}

fn installation(report: &mut DoctorReport, env: &Environment) {
    if let Some(executable) = &env.executable {
        report.add(
            "Rust executable",
            if executable.is_file() {
                "ok"
            } else {
                "warning"
            },
            &path_label(executable),
        );
    } else {
        report.add(
            "Rust executable",
            "not checked",
            "Executable path unavailable",
        );
    }
    let mut found = Vec::new();
    for directory in env.search_path.iter().take(128) {
        let names: &[&str] = if cfg!(windows) {
            &["dcl.exe", "dcl.cmd", "dcl.bat"]
        } else {
            &["dcl"]
        };
        for name in names {
            let path = directory.join(name);
            if path.is_file() {
                let canonical = path.canonicalize().unwrap_or(path);
                if !found.contains(&canonical) {
                    found.push(canonical);
                }
            }
        }
    }
    report.add("Rust CLI PATH", if found.len() > 1 { "warning" } else if found.is_empty() { "info" } else { "ok" }, &format!("{} distinct dcl launchers in the first 128 PATH entries; no executable was run; npm/official Claude installation not inferred", found.len()));
    for path in found {
        report.add("Rust CLI PATH candidate", "info", &path_label(&path));
    }
}
