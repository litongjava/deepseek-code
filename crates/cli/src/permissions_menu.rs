//! /permissions as in the reference CLI: Recently denied, Allow, Ask, Deny,
//! Auto mode and Workspace tabs switched with ←/→, each with its rules.
use crate::session::Session;
use crate::terminal::picker::{text_input, Entry, Key, Picked, Picker};
use anyhow::Result;
use deepseek_code_core::permissions::rules::PermissionAction;

const TABS: [&str; 6] = ["Recently denied", "Allow", "Ask", "Deny", "Auto mode", "Workspace"];

fn tab_bar(current: usize) -> String {
    let tabs: Vec<String> = TABS
        .iter()
        .enumerate()
        .map(|(index, tab)| if index == current { format!("[{tab}]") } else { format!(" {tab} ") })
        .collect();
    format!("Permissions  {}", tabs.join(" "))
}

fn action(tab: usize) -> Option<PermissionAction> {
    match tab {
        1 => Some(PermissionAction::Allow),
        2 => Some(PermissionAction::Confirm),
        3 => Some(PermissionAction::Deny),
        _ => None,
    }
}

/// Tool calls rejected or denied in this session, newest first.
fn recently_denied(session: &Session) -> Result<Vec<Entry>> {
    session.store.db.with_conn(|conn| -> Result<Vec<Entry>> {
        let mut statement = conn.prepare(
            "SELECT t.name, t.input, t.output FROM tool_calls t JOIN messages m ON t.message_id=m.id WHERE m.conversation_id=?1 AND t.is_error=1 ORDER BY m.sort_order DESC, t.sort_order DESC LIMIT 200",
        )?;
        let rows = statement.query_map([&session.id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?)))?;
        let mut entries = Vec::new();
        for row in rows {
            let (name, input, output) = row?;
            let output = output.unwrap_or_default();
            let denied = output.contains(crate::terminal::tool_view::REJECTED) || output.contains("denied") || output.contains("Denied");
            if !denied {
                continue;
            }
            let input: serde_json::Value = input.as_deref().and_then(|input| serde_json::from_str(input).ok()).unwrap_or_default();
            entries.push(Entry {
                title: crate::terminal::tool_view::label(&name, &input, Some(&session.workspace)),
                detail: output.lines().next().unwrap_or_default().chars().take(120).collect(),
            });
            if entries.len() == 20 {
                break;
            }
        }
        Ok(entries)
    })?
}

pub async fn run(session: &mut Session) -> Result<()> {
    let mut tab = 1;
    let mut selected = 0;
    loop {
        let mut rules = crate::permission_commands::load(session)?;
        let launch = session.permissions.launch_rules(&session.id);
        let (subtitle, entries, editable): (String, Vec<Entry>, Vec<Option<String>>) = match tab {
            0 => {
                let denied = recently_denied(session)?;
                let count = denied.len();
                ("Tool calls denied or rejected in this session.".into(), if denied.is_empty() { vec![Entry { title: "No denied tool calls yet".into(), detail: String::new() }] } else { denied }, vec![None; count.max(1)])
            }
            1..=3 => {
                let wanted = action(tab).unwrap();
                let mut entries = vec![Entry { title: "Add a new rule\u{2026}".into(), detail: String::new() }];
                let mut editable = vec![None];
                let mut session_rules: Vec<_> = rules.iter().filter(|(_, action)| **action == wanted).map(|(rule, _)| rule.clone()).collect();
                session_rules.sort();
                for rule in session_rules {
                    entries.push(Entry { title: rule.clone(), detail: "this session".into() });
                    editable.push(Some(rule));
                }
                let mut fixed: Vec<_> = launch.iter().filter(|(_, action)| **action == wanted).map(|(rule, _)| rule.clone()).collect();
                fixed.sort();
                for rule in fixed {
                    entries.push(Entry { title: rule, detail: "command line or settings (read-only)".into() });
                    editable.push(None);
                }
                let subtitle = match tab {
                    1 => "DeepSeek Code won't ask before using allowed tools.",
                    2 => "DeepSeek Code will always ask for confirmation before using these tools.",
                    _ => "DeepSeek Code will always reject requests to use denied tools.",
                };
                (subtitle.into(), entries, editable)
            }
            4 => {
                let context = crate::services::context(&session.paths)?;
                let config = deepseek_code_core::permissions::auto_config::load(&context)?;
                let value = serde_json::to_value(&config)?;
                let mut entries = vec![Entry { title: format!("Enabled: {}", value["enabled"].as_bool().unwrap_or(false)), detail: String::new() }];
                entries.push(Entry { title: format!("Classifier: {}", value["classifier"]["kind"].as_str().unwrap_or("not set")), detail: String::new() });
                entries.push(Entry { title: format!("On classifier failure: {}", value["failure_policy"].as_str().unwrap_or("deny")), detail: String::new() });
                let count = entries.len();
                ("Auto mode asks a classifier model whether each call is within what you asked. Change it with /permissions auto-config.".into(), entries, vec![None; count])
            }
            _ => {
                let mut entries = vec![Entry { title: "Add directory\u{2026}".into(), detail: String::new() }];
                entries.push(Entry { title: session.workspace.display().to_string(), detail: "working directory".into() });
                let folders = session.store.db.with_conn(|conn| deepseek_code_core::native_engine::workspace::conversation_project(conn, &session.id))??;
                for folder in folders.map(|project| project.folders).unwrap_or_default() {
                    if std::path::Path::new(&folder.path) != session.workspace {
                        entries.push(Entry { title: folder.path, detail: "added directory".into() });
                    }
                }
                let count = entries.len();
                ("DeepSeek Code can read files in these directories, and edit them when edits are allowed.".into(), entries, vec![None; count])
            }
        };
        let bar = tab_bar(tab);
        let picker = Picker { heading: &bar, subtitle: Some(&subtitle), entries: &entries, search: true, hint: "\u{2190}/\u{2192} to switch \u{b7} Enter to select \u{b7} Esc to cancel", selected };
        match picker.run_keys(&[Key::Left, Key::Right])? {
            Picked::Key(Key::Left, _) => {
                tab = (tab + TABS.len() - 1) % TABS.len();
                selected = 0;
            }
            Picked::Key(Key::Right, _) => {
                tab = (tab + 1) % TABS.len();
                selected = 0;
            }
            Picked::Entry(index) => {
                selected = index;
                if let (Some(wanted), 0) = (action(tab), index) {
                    let Some(rule) = text_input(&format!("Add {} rule (for example Bash(npm test *) or Read(.env)):", TABS[tab].to_lowercase()), "")?.filter(|rule| !rule.is_empty()) else {
                        continue;
                    };
                    match deepseek_code_core::permissions::patterns::validate(&rule) {
                        Ok(_) => {
                            rules.insert(rule, wanted);
                            crate::permission_commands::save_rules(session, &rules)?;
                        }
                        Err(error) => eprintln!("Invalid rule: {error:#}"),
                    }
                } else if tab == 5 && index == 0 {
                    if let Some(path) = text_input("Add directory:", "")?.filter(|path| !path.is_empty()) {
                        if let Err(error) = crate::session_commands::handle("/add-dir", &path, &session.paths.clone(), session).await {
                            eprintln!("Could not add {path}: {error:#}");
                        }
                    }
                } else if let Some(Some(rule)) = editable.get(index) {
                    let options = [Entry { title: "Remove this rule".into(), detail: String::new() }, Entry { title: "Keep it".into(), detail: String::new() }];
                    let heading = format!("Rule: {rule}");
                    if (Picker { heading: &heading, subtitle: None, entries: &options, search: false, hint: "Enter to confirm \u{b7} Esc to go back", selected: 1 }).run()? == Some(0) {
                        rules.remove(rule);
                        crate::permission_commands::save_rules(session, &rules)?;
                    }
                }
            }
            _ => return Ok(()),
        }
    }
}
