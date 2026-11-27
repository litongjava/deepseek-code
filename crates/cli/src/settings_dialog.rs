//! The settings dialog shared by /status, /config and /cost, as in the
//! reference CLI: Status, Config and Usage tabs switched with ←/→.
use crate::session::Session;
use crate::terminal::input::editor::Screen;
use crate::terminal::input::TerminalInput;
use crate::terminal::picker::{details_from_top_with, Entry, Key, Picked, Picker};
use anyhow::Result;
use deepseek_code_core::runtime::RuntimePaths;

/// Restore the conversation screen on every exit, including errors.
struct SettingsScreen;

impl SettingsScreen {
    fn enter() -> std::io::Result<Self> {
        // Drop the finished turn's input box before the alternate screen takes
        // over; releasing it afterwards would jump the cursor on the new screen.
        crate::terminal::release_handoff();
        crossterm::execute!(crate::terminal_output::stderr(), crossterm::terminal::EnterAlternateScreen,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0))?;
        Ok(Self)
    }
}

impl Drop for SettingsScreen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(crate::terminal_output::stderr(), crossterm::terminal::LeaveAlternateScreen);
    }
}

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
    let _screen = SettingsScreen::enter()?;
    let mut frame = Screen::top_anchored();
    let mut tab = TABS.iter().position(|tab| tab.eq_ignore_ascii_case(start)).unwrap_or(0);
    let mut selected = 0;
    let mut range = 0;
    let mut notice: Option<String> = None;
    loop {
        let step = match tab {
            3 => {
                notice = None;
                let rows = crate::stats_view::rows(&crate::stats_view::load(session)?, range, chrono::Local::now().date_naive());
                match details_from_top_with(&tab_bar(tab), &rows, "r to change range \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close", &['r'], &mut frame)? {
                    Some('r') => {
                        range = (range + 1) % crate::stats_view::RANGES.len();
                        continue;
                    }
                    Some('<') => -1,
                    Some('>') => 1,
                    _ => return Ok(()),
                }
            }
            1 => match config(paths, session, input, selected, notice.as_deref(), &mut frame).await? {
                ConfigOutcome::Switch(step) => {
                    notice = None;
                    step
                }
                ConfigOutcome::Stay(index, next_notice) => {
                    selected = index;
                    notice = next_notice;
                    continue;
                }
                ConfigOutcome::Close => return Ok(()),
            },
            _ => {
                notice = None;
                let rows = if tab == 0 { status_rows(session).await? } else { usage_rows(session, paths)? };
                match details_from_top_with(&tab_bar(tab), &rows, "\u{2190}/\u{2192} to switch tabs \u{b7} Esc to close", &[], &mut frame)? {
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
    let billing = deepseek_code_core::services::usage::session_cost(&session.store.db, &session.id, &paths.read_config()?)?;
    let count = |key: &str| usage[key].as_i64().unwrap_or(0);
    let seconds = |ms: i64| {
        let seconds = ms / 1000;
        if seconds < 60 { format!("{seconds}s") } else { format!("{}m {}s", seconds / 60, seconds % 60) }
    };
    let (added, removed) = session.selected_session()?.read(|r| r.changed_lines())?;
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
    Stay(usize, Option<String>),
    Close,
}

/// The message a Config action leaves on the panel. A failure keeps its text
/// visible after the screen is redrawn instead of being cleared at once.
fn outcome_notice(label: &str, result: &Result<()>) -> Option<String> {
    result
        .as_ref()
        .err()
        .map(|error| format!("{label}: {error:#}"))
}

/// The Config tab: each setting with its value; Enter changes it.
async fn config(paths: &RuntimePaths, session: &mut Session, input: &mut TerminalInput, selected: usize, notice: Option<&str>, frame: &mut Screen) -> Result<ConfigOutcome> {
    let display = crate::display_commands::load(session)?;
    let effort = crate::session_commands::session_effort(session)?.unwrap_or_else(|| "default".into());
    let autocompact = crate::session_commands::autocompact_label(session)?;
    let memory = deepseek_code_core::services::memory_files::config(&crate::services::context(paths)?)
        .map(|config| config.enabled)
        .unwrap_or(true);
    let experience = deepseek_code_core::experience::enabled(&crate::services::context(paths)?);
    let saved = paths.read_config()?;
    let settings: Vec<(&str, &str, String)> = vec![
        ("model", "Model", session.binding_state().model.clone()),
        ("provider", "Provider", session.binding_state().provider_id.clone()),
        ("permissions", "Permission mode", session.permission_mode().to_owned()),
        ("effort", "Effort", effort),
        ("autocompact", "Auto-compact", autocompact),
        ("memory", "Long-term memory", if memory { "on" } else { "off" }.into()),
        ("experience", "Experience", if experience { "on" } else { "off" }.into()),
        ("theme", "Theme", display.theme.clone()),
        // As in the reference CLI; /vim toggles the same setting.
        ("editor", "Editor mode", if crate::terminal::vim::enabled(&paths.data_dir) { "vim" } else { "normal" }.into()),
        ("color", "Prompt color", display.color.clone()),
        ("focus", "Focus mode", display.focus.to_string()),
        ("output-style", "Output style", display.output_style.clone()),
        ("tui", "Terminal renderer", display.tui.clone()),
        ("search", "Web search", crate::interactive_config::search_status(&saved.deepseek_search)),
        ("fetch-proxy", "Web fetch proxy", crate::interactive_config::fetch_proxy_status(&saved.web_fetch)),
        ("statusline", "Status line", "configure \u{203a}".into()),
    ];
    let width = settings.iter().map(|(_, label, _)| label.len()).max().unwrap_or(0) + 4;
    let entries: Vec<Entry> = settings
        .iter()
        .map(|(_, label, value)| Entry { title: format!("{label:<width$}{value}"), detail: String::new() })
        .collect();
    let bar = tab_bar(1);
    let picker = Picker { heading: &bar, subtitle: notice, entries: &entries, search: true, hint: "Type to filter \u{b7} Enter/Space to change \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close", selected };
    let index = match picker.run_keys_filling_with(&[Key::Left, Key::Right, Key::Space], frame)? {
        Picked::Entry(index) | Picked::Key(Key::Space, Some(index)) => index,
        Picked::Key(Key::Left, _) => return Ok(ConfigOutcome::Switch(-1)),
        Picked::Key(Key::Right, _) => return Ok(ConfigOutcome::Switch(1)),
        _ => return Ok(ConfigOutcome::Close),
    };
    let (key, label, value) = &settings[index];
    let choices: &[&str] = match *key {
        "permissions" => &["ask", "accept-edits", "plan", "auto", "dont-ask"],
        "effort" => &crate::session_commands::EFFORTS,
        "autocompact" => &["default", "off", "70%", "80%", "90%"],
        "memory" => &["on", "off"],
        "experience" => &["on", "off"],
        "theme" => &["dark", "light", "dark-daltonized", "light-daltonized", "dark-ansi", "light-ansi"],
        "color" => &["default", "red", "orange", "yellow", "green", "blue", "purple", "pink", "cyan"],
        "focus" => &["on", "off"],
        "editor" => &["normal", "vim"],
        "output-style" => &["default", "explanatory", "learning"],
        "tui" => &["default", "fullscreen"],
        _ => &[],
    };
    let result = if choices.is_empty() {
        // Model, provider, search and the proxy keep their own pickers or prompts.
        // The line-oriented prompt starts on a clean screen and cursor.
        let _ = crossterm::execute!(
            crate::terminal_output::stderr(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        );
        let command = match *key {
            "model" => "/model".to_owned(),
            "search" => "/search-config".to_owned(),
            "fetch-proxy" => "/config fetch-proxy".to_owned(),
            _ => format!("/{key}"),
        };
        let result = Box::pin(crate::interactive_config::handle(&command, paths, session, input)).await.map(|_| ());
        // The line-oriented prompt drew over the alternate screen; repaint this frame.
        frame.reflow();
        result
    } else {
        // As in the reference CLI, Enter or Space moves to the next value in place.
        let choice = choices.iter().position(|choice| choice == value).map_or(0, |current| (current + 1) % choices.len());
        match Some(choice) {
            Some(choice) if choices[choice] != value => match *key {
                "permissions" => session.set_permission_mode(choices[choice]).await,
                "editor" => crate::terminal::vim::set_enabled(&paths.data_dir, choices[choice] == "vim").map(|()| input.set_vim(choices[choice] == "vim")),
                "memory" => {
                    let context = crate::services::context(paths)?;
                    let mut config = deepseek_code_core::services::memory_files::config(&context)?;
                    config.enabled = choices[choice] == "on";
                    deepseek_code_core::services::memory_files::save_config(&context, &config)
                }
                "experience" => deepseek_code_core::experience::set_enabled(
                    &crate::services::context(paths)?,
                    choices[choice] == "on",
                ),
                "effort" | "autocompact" => crate::session_commands::handle(&format!("/{key}"), choices[choice], paths, session).await.map(|_| ()),
                _ => crate::display_commands::handle(&format!("/{key}"), choices[choice], paths, session).await.map(|_| ()),
            },
            _ => Ok(()),
        }
    };
    let mut notice = outcome_notice(label, &result);
    if result.is_ok() {
        match *key {
            "search" => {
                notice = Some(format!(
                    "{label}: {}",
                    crate::interactive_config::search_status(&paths.read_config()?.deepseek_search)
                ))
            }
            "fetch-proxy" => {
                notice = Some(format!(
                    "{label}: {}",
                    crate::interactive_config::fetch_proxy_status(&paths.read_config()?.web_fetch)
                ))
            }
            _ => {}
        }
    }
    Ok(ConfigOutcome::Stay(index, notice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_config_actions_keep_their_message_for_the_redraw() {
        let error: Result<()> = Err(anyhow::anyhow!("bad value"));
        assert_eq!(
            outcome_notice("Web fetch proxy", &error).as_deref(),
            Some("Web fetch proxy: bad value")
        );
        let ok: Result<()> = Ok(());
        assert!(outcome_notice("Theme", &ok).is_none());
    }
}
