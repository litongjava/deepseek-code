//! The settings dialog shared by /status, /config and /cost, as in the
//! reference CLI: Status, Config and Usage tabs switched with ←/→.
use crate::session::Session;
use crate::terminal::input::TerminalInput;
use crate::terminal::picker::{details, Entry, Key, Picked, Picker};
use anyhow::Result;
use claude_core::runtime::RuntimePaths;

const TABS: [&str; 4] = ["Status", "Config", "Usage", "Stats"];

/// `Settings   Status   [Config]   Usage`.
fn tab_bar(current: usize) -> String {
    let tabs: Vec<String> = TABS
        .iter()
        .enumerate()
        .map(|(index, tab)| if index == current { format!("[{tab}]") } else { format!(" {tab} ") })
        .collect();
    format!("Settings  {}", tabs.join("  "))
}

pub async fn run(start: &str, paths: &RuntimePaths, session: &mut Session, input: &mut TerminalInput) -> Result<()> {
    let mut tab = TABS.iter().position(|tab| tab.eq_ignore_ascii_case(start)).unwrap_or(0);
    let mut selected = 0;
    let mut range = 0;
    loop {
        let step = match tab {
            3 => {
                let rows = crate::stats_view::rows(&crate::stats_view::load(session)?, range, chrono::Local::now().date_naive());
                match details(&tab_bar(tab), &rows, "r to change range \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close", &['r'])? {
                    Some('r') => {
                        range = (range + 1) % crate::stats_view::RANGES.len();
                        continue;
                    }
                    Some('<') => -1,
                    Some('>') => 1,
                    _ => return Ok(()),
                }
            }
            1 => match config(paths, session, input, selected).await? {
                ConfigOutcome::Switch(step) => step,
                ConfigOutcome::Stay(index) => {
                    selected = index;
                    continue;
                }
                ConfigOutcome::Close => return Ok(()),
            },
            _ => {
                let rows = if tab == 0 { status_rows(session).await? } else { usage_rows(session, paths)? };
                match details(&tab_bar(tab), &rows, "\u{2190}/\u{2192} to switch tabs \u{b7} Esc to close", &[])? {
                    Some('<') => -1,
                    Some('>') => 1,
                    _ => return Ok(()),
                }
            }
        };
        tab = (tab as isize + step).rem_euclid(TABS.len() as isize) as usize;
    }
}

async fn status_rows(session: &Session) -> Result<Vec<String>> {
    Ok(crate::status_commands::text(session, "")
        .await?
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect())
}

/// The reference CLI's Usage tab: cost, time, code changes and tokens.
fn usage_rows(session: &Session, paths: &RuntimePaths) -> Result<Vec<String>> {
    let usage = crate::display_commands::usage(session)?;
    let billing = claude_core::services::usage::session_cost(&session.store.db, &session.id, &paths.read_config()?)?;
    let count = |key: &str| usage[key].as_i64().unwrap_or(0);
    let seconds = |ms: i64| {
        let seconds = ms / 1000;
        if seconds < 60 { format!("{seconds}s") } else { format!("{}m {}s", seconds / 60, seconds % 60) }
    };
    let (added, removed) = session.store.db.with_conn(|conn| {
        conn.query_row(
            "SELECT COALESCE(SUM(json_extract(t.output,'$.file_change.added')),0), COALESCE(SUM(json_extract(t.output,'$.file_change.removed')),0) FROM tool_calls t JOIN messages m ON t.message_id=m.id WHERE m.conversation_id=?1 AND json_valid(t.output) AND t.is_error=0",
            [&session.id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
    })??;
    let wall = crate::terminal::STARTED.get().map_or(0, |started| started.elapsed().as_millis() as i64);
    let cost = match billing["total_usd"].as_f64() {
        Some(total) if billing["unpriced_requests"].as_i64().unwrap_or(0) == 0 => format!("${total:.4}"),
        Some(total) => format!("${total:.4} (some requests unpriced)"),
        None => "unknown".into(),
    };
    Ok(vec![
        "Session".into(),
        format!("Total cost:            {cost}"),
        format!("Total duration (API):  {}", seconds(count("duration_ms"))),
        format!("Total duration (wall): {}", seconds(wall)),
        format!("Total code changes:    {added} lines added, {removed} lines removed"),
        format!(
            "Usage:                 {} input, {} output, {} cache read, {} cache write",
            count("input_tokens"),
            count("output_tokens"),
            count("cache_read_tokens"),
            count("cache_write_tokens")
        ),
    ])
}

enum ConfigOutcome {
    Switch(isize),
    Stay(usize),
    Close,
}

/// The Config tab: each setting with its value; Enter changes it.
async fn config(paths: &RuntimePaths, session: &mut Session, input: &mut TerminalInput, selected: usize) -> Result<ConfigOutcome> {
    let display = crate::display_commands::load(session)?;
    let effort = crate::session_commands::session_effort(session)?.unwrap_or_else(|| "default".into());
    let autocompact = crate::session_commands::autocompact(session)?.map_or_else(|| "off".to_owned(), |percent| format!("{percent}%"));
    let settings: Vec<(&str, &str, String)> = vec![
        ("model", "Model", session.model.clone()),
        ("provider", "Provider", session.provider_id.clone()),
        ("permissions", "Permission mode", session.permission_mode().to_owned()),
        ("effort", "Effort", effort),
        ("autocompact", "Auto-compact", autocompact),
        ("theme", "Theme", display.theme.clone()),
        ("color", "Prompt color", display.color.clone()),
        ("focus", "Focus mode", display.focus.to_string()),
        ("output-style", "Output style", display.output_style.clone()),
        ("tui", "Terminal renderer", display.tui.clone()),
        ("statusline", "Status line", "configure \u{203a}".into()),
    ];
    let width = settings.iter().map(|(_, label, _)| label.len()).max().unwrap_or(0) + 4;
    let entries: Vec<Entry> = settings
        .iter()
        .map(|(_, label, value)| Entry { title: format!("{label:<width$}{value}"), detail: String::new() })
        .collect();
    let bar = tab_bar(1);
    let picker = Picker { heading: &bar, subtitle: None, entries: &entries, search: true, hint: "Type to filter \u{b7} Enter to change \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close", selected };
    let index = match picker.run_keys(&[Key::Left, Key::Right])? {
        Picked::Entry(index) => index,
        Picked::Key(Key::Left, _) => return Ok(ConfigOutcome::Switch(-1)),
        Picked::Key(Key::Right, _) => return Ok(ConfigOutcome::Switch(1)),
        _ => return Ok(ConfigOutcome::Close),
    };
    let (key, label, value) = &settings[index];
    let choices: &[&str] = match *key {
        "permissions" => &["ask", "accept-edits", "plan", "auto", "dont-ask"],
        "effort" => &crate::session_commands::EFFORTS,
        "autocompact" => &["off", "70%", "80%", "90%"],
        "theme" => &["dark", "light", "dark-daltonized", "light-daltonized", "dark-ansi", "light-ansi"],
        "color" => &["default", "red", "orange", "yellow", "green", "blue", "purple", "pink", "cyan"],
        "focus" => &["on", "off"],
        "output-style" => &["default", "explanatory", "learning"],
        "tui" => &["default", "fullscreen"],
        _ => &[],
    };
    let result = if choices.is_empty() {
        // Model, provider and the status line keep their own pickers or prompts.
        Box::pin(crate::interactive_config::handle(&format!("/{}", if *key == "model" { "model" } else { key }), paths, session, input)).await.map(|_| ())
    } else {
        let options: Vec<Entry> = choices.iter().map(|choice| Entry { title: if *choice == value { format!("{choice} \u{2714}") } else { choice.to_string() }, detail: String::new() }).collect();
        let current = choices.iter().position(|choice| choice == value).unwrap_or(0);
        let heading = format!("{label}");
        match (Picker { heading: &heading, subtitle: None, entries: &options, search: false, hint: "Enter to choose \u{b7} Esc to go back", selected: current }).run()? {
            Some(choice) if choices[choice] != value => match *key {
                "permissions" => session.set_permission_mode(choices[choice]).await,
                "effort" | "autocompact" => crate::session_commands::handle(&format!("/{key}"), choices[choice], paths, session).await.map(|_| ()),
                _ => crate::display_commands::handle(&format!("/{key}"), choices[choice], paths, session).await.map(|_| ()),
            },
            _ => Ok(()),
        }
    };
    if let Err(error) = result {
        eprintln!("{label}: {error:#}");
    }
    Ok(ConfigOutcome::Stay(index))
}
