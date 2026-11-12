//! The plugins panel shared by `/plugins` and `/skill-doctor`: Installed,
//! Errors and Stats tabs switched with ←/→. The Installed tab lists plugins,
//! MCP servers and skills and changes their state in place; the Errors tab
//! shows skill files that could not be read and MCP servers that failed.
use crate::session::Session;
use crate::terminal::input::TerminalInput;
use crate::terminal::picker::{details_from_top, Entry, Key, Picked, Picker};
use anyhow::Result;
use chrono::{DateTime, Utc};
use deepseek_code_core::runtime::RuntimePaths;
use deepseek_code_core::services::skill_usage::{self, Usage};
use deepseek_code_core::skills::states::{self, SkillState};
use std::collections::BTreeSet;
use std::path::Path;

/// Restore the conversation screen on every exit, including errors.
struct PluginsScreen;

impl PluginsScreen {
    fn enter() -> std::io::Result<Self> {
        crossterm::execute!(
            std::io::stderr(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        Ok(Self)
    }
}

impl Drop for PluginsScreen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stderr(), crossterm::terminal::LeaveAlternateScreen);
    }
}

const TABS: [&str; 3] = ["Installed", "Errors", "Stats"];
const HINT: &str = "\u{2190}/\u{2192} to switch tabs \u{b7} Esc to close";
const INSTALLED_HINT: &str = "Type to search \u{b7} Space to toggle \u{b7} Enter to view \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close";

pub(crate) struct SkillRow {
    pub name: String,
    pub source: String,
    pub state: SkillState,
    pub listing_tokens: u64,
    pub usage: Usage,
}

pub(crate) struct PluginRow {
    pub id: String,
    pub enabled: bool,
    pub skills: usize,
    pub uses: u64,
    pub last_used: Option<DateTime<Utc>>,
}

pub(crate) struct McpRow {
    pub name: String,
    pub enabled: bool,
    pub last_used: Option<DateTime<Utc>>,
}

pub(crate) struct Model {
    pub skills: Vec<SkillRow>,
    pub plugins: Vec<PluginRow>,
    pub mcp: Vec<McpRow>,
}

/// What Enter or Space on an Installed row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Header,
    Plugin(String),
    Mcp(String),
    Skill(String),
    ToggleDisabled,
}

/// `Plugins   [Installed]   Errors   Stats`, as in the settings dialog.
fn tab_bar(current: usize) -> String {
    let tabs: Vec<String> = TABS
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            if index == current {
                format!("[{tab}]")
            } else {
                format!(" {tab} ")
            }
        })
        .collect();
    format!("Plugins  {}", tabs.join("  "))
}

/// The part of a skill name people recognise: the last `:` segment.
/// The full name stays the key for states and lookups.
fn display_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// The order skills appear in: the name people see, then the source, then the
/// full name so same-named skills from different sources stay together.
fn skill_order(a: &SkillRow, b: &SkillRow) -> std::cmp::Ordering {
    (display_name(&a.name).to_lowercase(), &a.source, &a.name).cmp(&(
        display_name(&b.name).to_lowercase(),
        &b.source,
        &b.name,
    ))
}

/// Where a skill comes from: a plugin ID, the CLI's embedded skills, the
/// workspace's `.claude` directory, or the user scope.
fn source_label(name: &str, path: &Path, workspace: &Path) -> String {
    if name.starts_with("synced:") {
        return "synced".to_owned();
    }
    if let Some((plugin, _)) = name.split_once(':') {
        return plugin.to_owned();
    }
    if path.parent() == Some(Path::new("__embedded_cli_skills__")) {
        return "built-in".to_owned();
    }
    if path.starts_with(workspace.join(".claude")) {
        return "project".to_owned();
    }
    "user".to_owned()
}

/// Skills include ones set to `off`: the resolver drops them, but the panel
/// must still show them so they can be turned back on.
pub(crate) async fn load(paths: &RuntimePaths, session: &Session) -> Result<Model> {
    let context = crate::services::context(paths)?;
    let mut commands =
        deepseek_code_core::skills::discovery::discover(&paths.data_dir, &session.workspace)?;
    commands.extend(deepseek_code_core::services::plugins::skill_commands(
        &context,
    )?);
    let states = states::load(&paths.data_dir);
    let usages = skill_usage::skill_usage(&session.store.db, Utc::now())?;
    let mut names: BTreeSet<String> = commands.keys().cloned().collect();
    names.extend(states.keys().cloned());
    let mut skills: Vec<SkillRow> = names
        .into_iter()
        .map(|name| {
            let state = states.get(&name).copied().unwrap_or_default();
            let command = commands.get(&name);
            let listing_tokens = if state == SkillState::Off {
                0
            } else {
                command.map_or(0, skill_usage::listing_tokens)
            };
            let path = command.map_or(Path::new(""), |command| command.path.as_path());
            SkillRow {
                source: source_label(&name, path, &session.workspace),
                state,
                listing_tokens,
                usage: usages.get(&name).cloned().unwrap_or_default(),
                name,
            }
        })
        .collect();
    skills.sort_by(|a, b| skill_order(a, b));

    let mut plugins: Vec<PluginRow> = deepseek_code_core::services::plugins::list(&context)?
        .into_iter()
        .map(|plugin| {
            let owned: Vec<&SkillRow> = skills
                .iter()
                .filter(|skill| skill.source == plugin.id)
                .collect();
            PluginRow {
                skills: owned.len(),
                uses: owned.iter().map(|skill| skill.usage.uses).sum(),
                last_used: owned.iter().filter_map(|skill| skill.usage.last_used).max(),
                id: plugin.id,
                enabled: plugin.enabled,
            }
        })
        .collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));

    // The `/mcp` menu's list, without its bundled presets nobody changed.
    session.mcp.initialize().await?;
    let last_used = skill_usage::mcp_last_used(&session.store.db)?;
    let mut mcp: Vec<McpRow> = session
        .mcp
        .list_servers()
        .await
        .into_iter()
        .filter(|server| !crate::mcp_menu::untouched_preset(server))
        .map(|server| McpRow {
            last_used: last_used
                .get(&server.name)
                .copied()
                .or_else(|| last_used.get(&server.id).copied()),
            name: server.name,
            enabled: server.enabled,
        })
        .collect();
    mcp.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Model {
        skills,
        plugins,
        mcp,
    })
}

/// "2.4k" for a four-digit count, the plain number below a thousand.
fn token_label(tokens: u64) -> String {
    if tokens == 0 {
        "-".to_owned()
    } else if tokens < 1000 {
        tokens.to_string()
    } else {
        format!("{:.1}k", tokens as f64 / 1000.0)
    }
}

/// `text` unchanged, or its first `width - 1` characters and an ellipsis.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut fitted: String = text.chars().take(width.saturating_sub(1)).collect();
    fitted.push('\u{2026}');
    fitted
}

/// The Stats tab: what each skill costs and how often it ran, and plugins that
/// have gone unused. Plain rows, columns padded with spaces.
pub(crate) fn stats_rows(model: &Model, now: DateTime<Utc>, notice: bool) -> Vec<String> {
    let mut skills: Vec<&SkillRow> = model.skills.iter().collect();
    skills.sort_by(|a, b| skill_order(a, b));

    // Caps keep the numeric columns on screen: synced and plugin skill names
    // can be far longer than a terminal cares to show.
    let name_width = skills
        .iter()
        .map(|skill| display_name(&skill.name).chars().count())
        .max()
        .unwrap_or(0)
        .clamp("skill".len(), 24);
    let source_width = skills
        .iter()
        .map(|skill| skill.source.chars().count())
        .max()
        .unwrap_or(0)
        .clamp("source".len(), 14);

    let mut rows = Vec::new();
    if notice {
        rows.push("\u{2139} /skill-doctor moved \u{2014} skill usage and context costs now live in this Stats tab.".to_owned());
        rows.push(String::new());
    }
    rows.push("Skills loaded this session".to_owned());
    rows.push(String::new());
    rows.push(format!(
        "  {:<name_width$}  {:<source_width$}  {:<7}  {:<9}  {:<4}  last used",
        "skill", "source", "context", "7d tokens", "uses"
    ));
    for skill in &skills {
        let context = if skill.listing_tokens == 0 {
            "-".to_owned()
        } else {
            format!("~{}", skill.listing_tokens)
        };
        rows.push(format!(
            "  {:<name_width$}  {:<source_width$}  {:<7}  {:<9}  {:<4}  {}",
            fit(display_name(&skill.name), name_width),
            fit(&skill.source, source_width),
            context,
            token_label(skill.usage.tokens_7d),
            format!("{}\u{d7}", skill.usage.uses),
            skill_usage::relative_day(skill.usage.last_used, now),
        ));
    }
    rows.push(String::new());
    rows.push(
        "  context = this skill's one-line listing in the system prompt, included every turn"
            .to_owned(),
    );
    rows.push("  (dash = not in the current listing, costs nothing; full SKILL.md loads only when it runs)".to_owned());
    rows.push("  7d tokens = tokens attributed to the skill over the last 7 days of sessions on this machine".to_owned());
    rows.push(String::new());

    let unused = skills
        .iter()
        .filter(|skill| skill.listing_tokens > 0 && skill.usage.uses == 0)
        .count();
    if unused > 0 {
        let noun = if unused == 1 { "skill" } else { "skills" };
        rows.push(format!(
            "{unused} {noun} loaded but never invoked. Each one adds to the system prompt every turn. Disable it in /plugins, or set /skills state NAME off."
        ));
        rows.push(String::new());
    }

    let mut stale: Vec<&PluginRow> = model
        .plugins
        .iter()
        .filter(|plugin| {
            plugin
                .last_used
                .is_none_or(|last| now - last > chrono::Duration::days(14))
        })
        .collect();
    stale.sort_by(|a, b| a.id.cmp(&b.id));
    if !stale.is_empty() {
        rows.push("Plugins not used recently".to_owned());
        rows.push(String::new());
        for plugin in stale {
            let used = match plugin.last_used {
                Some(last) => format!(
                    "last used {} ago",
                    skill_usage::relative_day(Some(last), now)
                ),
                None => "never used".to_owned(),
            };
            rows.push(format!("  {}  {used}", plugin.id));
        }
        rows.push(String::new());
        rows.push("  Manage these in the Installed tab".to_owned());
    }
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    rows
}

/// Space cycles a skill between `off` and `user`; the other states need the
/// explicit picker behind Enter.
pub(crate) fn space_state(current: SkillState) -> SkillState {
    match current {
        SkillState::Off => SkillState::User,
        _ => SkillState::Off,
    }
}

/// The muted line under a skill: state, source, listing cost and last use.
fn skill_detail(skill: &SkillRow, now: DateTime<Utc>) -> String {
    let mut parts = Vec::new();
    // The default state stays implicit, so `user` there is the source.
    if skill.state != SkillState::User {
        parts.push(skill.state.label().to_owned());
    }
    parts.push(skill.source.clone());
    if skill.listing_tokens > 0 {
        parts.push(format!("~{} tok", skill.listing_tokens));
    } else {
        parts.push("not listed".to_owned());
    }
    parts.push(match skill.usage.last_used {
        Some(_) => format!("used {}", skill_usage::relative_day(skill.usage.last_used, now)),
        None => "never used".to_owned(),
    });
    parts.join(" \u{b7} ")
}

fn plugin_title(plugin: &PluginRow, now: DateTime<Utc>, stale: bool) -> String {
    let skills = format!(
        "{} skill{}",
        plugin.skills,
        if plugin.skills == 1 { "" } else { "s" }
    );
    let mut title = format!(
        "\u{2714} {:<28} Plugin   {skills} \u{b7} {} uses",
        plugin.id, plugin.uses
    );
    if stale {
        match plugin.last_used {
            Some(last) => title.push_str(&format!(
                " \u{b7} not used in {} days",
                (now - last).num_days().max(0)
            )),
            None => title.push_str(" \u{b7} never used"),
        }
    }
    title
}

fn header(name: &str) -> (Entry, Target) {
    (
        Entry {
            title: name.to_owned(),
            detail: String::new(),
        },
        Target::Header,
    )
}

/// The Installed tab's rows in display order, each with what it acts on.
/// Sections that would be empty are omitted together with their header.
pub(crate) fn installed_entries(
    model: &Model,
    show_disabled: bool,
    now: DateTime<Utc>,
) -> Vec<(Entry, Target)> {
    let mut rows = Vec::new();

    let mut plugins: Vec<&PluginRow> = model.plugins.iter().filter(|plugin| plugin.enabled).collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    let (stale, recent): (Vec<&PluginRow>, Vec<&PluginRow>) = plugins.into_iter().partition(|plugin| {
        plugin
            .last_used
            .is_none_or(|last| now - last > chrono::Duration::days(14))
    });
    if !stale.is_empty() {
        rows.push(header("Not used recently"));
        for plugin in stale {
            rows.push((
                Entry {
                    title: plugin_title(plugin, now, true),
                    detail: String::new(),
                },
                Target::Plugin(plugin.id.clone()),
            ));
        }
    }

    let mut mcp: Vec<&McpRow> = model.mcp.iter().filter(|server| server.enabled).collect();
    mcp.sort_by(|a, b| a.name.cmp(&b.name));
    if !recent.is_empty() || !mcp.is_empty() {
        rows.push(header("User"));
        for plugin in recent {
            rows.push((
                Entry {
                    title: plugin_title(plugin, now, false),
                    detail: String::new(),
                },
                Target::Plugin(plugin.id.clone()),
            ));
        }
        for server in mcp {
            rows.push((
                Entry {
                    title: format!("\u{2714} {:<28} MCP", server.name),
                    detail: String::new(),
                },
                Target::Mcp(server.name.clone()),
            ));
        }
    }

    let mut skills: Vec<&SkillRow> = model
        .skills
        .iter()
        .filter(|skill| skill.state != SkillState::Off)
        .collect();
    skills.sort_by(|a, b| skill_order(a, b));
    if !skills.is_empty() {
        rows.push(header("Skills"));
        for skill in skills {
            rows.push((
                Entry {
                    title: format!("\u{2714} {:<28} Skill    {}", display_name(&skill.name), skill_detail(skill, now)),
                    detail: String::new(),
                },
                Target::Skill(skill.name.clone()),
            ));
        }
    }

    let mut disabled_plugins: Vec<&PluginRow> =
        model.plugins.iter().filter(|plugin| !plugin.enabled).collect();
    disabled_plugins.sort_by(|a, b| a.id.cmp(&b.id));
    let mut disabled_mcp: Vec<&McpRow> =
        model.mcp.iter().filter(|server| !server.enabled).collect();
    disabled_mcp.sort_by(|a, b| a.name.cmp(&b.name));
    let mut disabled_skills: Vec<&SkillRow> = model
        .skills
        .iter()
        .filter(|skill| skill.state == SkillState::Off)
        .collect();
    disabled_skills.sort_by(|a, b| skill_order(a, b));
    let disabled = disabled_plugins.len() + disabled_mcp.len() + disabled_skills.len();

    if disabled > 0 {
        if show_disabled {
            rows.push((
                Entry {
                    title: format!("  \u{2193} Hide disabled ({disabled})"),
                    detail: String::new(),
                },
                Target::ToggleDisabled,
            ));
            if !disabled_plugins.is_empty() {
                rows.push(header("Disabled plugins"));
                for plugin in disabled_plugins {
                    rows.push((
                        Entry {
                            title: format!("\u{25cb} {:<28} Plugin   disabled", plugin.id),
                            detail: String::new(),
                        },
                        Target::Plugin(plugin.id.clone()),
                    ));
                }
            }
            if !disabled_mcp.is_empty() {
                rows.push(header("Disabled MCP"));
                for server in disabled_mcp {
                    rows.push((
                        Entry {
                            title: format!("\u{25cb} {:<28} MCP      disabled", server.name),
                            detail: String::new(),
                        },
                        Target::Mcp(server.name.clone()),
                    ));
                }
            }
            if !disabled_skills.is_empty() {
                rows.push(header("Disabled skills"));
                for skill in disabled_skills {
                    rows.push((
                        Entry {
                            title: format!(
                                "\u{25cb} {:<28} Skill    {}",
                                display_name(&skill.name),
                                skill_detail(skill, now)
                            ),
                            detail: String::new(),
                        },
                        Target::Skill(skill.name.clone()),
                    ));
                }
            }
        } else {
            rows.push((
                Entry {
                    title: format!("  \u{2192} Show disabled ({disabled})"),
                    detail: String::new(),
                },
                Target::ToggleDisabled,
            ));
        }
    }
    rows
}

/// The Errors tab: unreadable skill files and failed MCP servers.
async fn error_rows(paths: &RuntimePaths, session: &Session) -> Vec<String> {
    let mut rows = Vec::new();
    let mut invalid =
        deepseek_code_core::skills::discovery::invalid_skill_files(&paths.data_dir, &session.workspace);
    invalid.sort();
    for path in invalid {
        rows.push(format!(
            "\u{2717} {} \u{2014} invalid or unreadable skill file; run /doctor",
            path.display()
        ));
    }
    let mut failed: Vec<(String, String)> = session
        .mcp
        .list_servers()
        .await
        .into_iter()
        .filter_map(|server| {
            let error = crate::mcp_menu::failure(&server)?
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            Some((server.name, error))
        })
        .collect();
    failed.sort();
    for (name, error) in failed {
        rows.push(format!(
            "\u{2717} {name} \u{2014} MCP server failed: {error}"
        ));
    }
    if rows.is_empty() {
        rows.push("No errors.".to_owned());
    }
    rows
}

/// The four states behind Enter on a skill, with the current one marked.
fn pick_skill_state(name: &str, current: SkillState) -> std::io::Result<Option<SkillState>> {
    let choices = [
        (SkillState::Off, "Not listed and cannot be invoked".to_owned()),
        (
            SkillState::User,
            "Listed with its description; you and the model can invoke it".to_owned(),
        ),
        (
            SkillState::NameOnly,
            "Listed by name only to save context; you and the model can invoke it".to_owned(),
        ),
        (
            SkillState::UserOnly,
            format!("Not listed to the model; only you can invoke it with /{name}"),
        ),
    ];
    let entries: Vec<Entry> = choices
        .iter()
        .map(|(state, detail)| Entry {
            title: if *state == current {
                format!("{} (current)", state.label())
            } else {
                state.label().to_owned()
            },
            detail: detail.clone(),
        })
        .collect();
    let heading = format!("Skill state: {}", display_name(name));
    let selected = choices
        .iter()
        .position(|(state, _)| *state == current)
        .unwrap_or(0);
    let picker = Picker {
        heading: &heading,
        subtitle: None,
        entries: &entries,
        search: false,
        hint: "Enter to set \u{b7} Esc to go back",
        selected,
    };
    Ok(picker.run()?.map(|index| choices[index].0))
}

fn plugin_details(plugin: &PluginRow, now: DateTime<Utc>) -> std::io::Result<()> {
    let rows = vec![
        format!("id:        {}", plugin.id),
        "kind:      Plugin".to_owned(),
        format!("enabled:   {}", if plugin.enabled { "yes" } else { "no" }),
        format!("skills:    {}", plugin.skills),
        format!("uses:      {}", plugin.uses),
        format!(
            "last used: {}",
            skill_usage::relative_day(plugin.last_used, now)
        ),
    ];
    details_from_top(
        &plugin.id,
        &rows,
        "\u{2190} to go back \u{b7} Esc to close",
        &[],
    )?;
    Ok(())
}

fn mcp_details(server: &McpRow, now: DateTime<Utc>) -> std::io::Result<()> {
    let rows = vec![
        format!("name:      {}", server.name),
        "kind:      MCP".to_owned(),
        format!("enabled:   {}", if server.enabled { "yes" } else { "no" }),
        format!(
            "last used: {}",
            skill_usage::relative_day(server.last_used, now)
        ),
    ];
    details_from_top(
        &server.name,
        &rows,
        "\u{2190} to go back \u{b7} Esc to close",
        &[],
    )?;
    Ok(())
}

pub(crate) async fn run(
    start: &str,
    paths: &RuntimePaths,
    session: &mut Session,
    _input: &mut TerminalInput,
) -> Result<bool> {
    let _screen = PluginsScreen::enter()?;
    let mut model = load(paths, session).await?;
    let mut tab = TABS
        .iter()
        .position(|tab| tab.eq_ignore_ascii_case(start))
        .unwrap_or(0);
    let mut selected = 0;
    let mut show_disabled = false;
    let mut changed = false;
    loop {
        match tab {
            // The Installed tab is a picker: typing searches, Space toggles,
            // Enter opens details or the state picker.
            0 => {
                let now = Utc::now();
                let rows = installed_entries(&model, show_disabled, now);
                let entries: Vec<Entry> = rows
                    .iter()
                    .map(|(entry, _)| Entry {
                        title: entry.title.clone(),
                        detail: entry.detail.clone(),
                    })
                    .collect();
                let bar = tab_bar(0);
                let picker = Picker {
                    heading: &bar,
                    subtitle: None,
                    entries: &entries,
                    search: true,
                    hint: INSTALLED_HINT,
                    selected,
                };
                let (index, toggled) = match picker.run_keys_filling(&[Key::Left, Key::Right, Key::Space])? {
                    Picked::Key(Key::Left, _) => {
                        tab = (tab + TABS.len() - 1) % TABS.len();
                        continue;
                    }
                    Picked::Key(Key::Right, _) => {
                        tab = (tab + 1) % TABS.len();
                        continue;
                    }
                    Picked::Cancel => return Ok(changed),
                    Picked::Key(Key::Space, Some(index)) => (index, true),
                    Picked::Key(Key::Space, None) => continue,
                    Picked::Entry(index) => (index, false),
                    Picked::Key(_, _) => continue,
                };
                selected = index;
                let Some((_, target)) = rows.into_iter().nth(index) else {
                    continue;
                };
                match target {
                    Target::Header => {}
                    Target::ToggleDisabled => show_disabled = !show_disabled,
                    Target::Skill(name) => {
                        let current = model
                            .skills
                            .iter()
                            .find(|skill| skill.name == name)
                            .map_or(SkillState::User, |skill| skill.state);
                        if toggled {
                            states::set(&paths.data_dir, &name, space_state(current))?;
                            changed = true;
                            model = load(paths, session).await?;
                        } else if let Some(state) = pick_skill_state(&name, current)? {
                            states::set(&paths.data_dir, &name, state)?;
                            changed = true;
                            model = load(paths, session).await?;
                        }
                    }
                    Target::Plugin(id) => {
                        if !toggled {
                            if let Some(plugin) = model.plugins.iter().find(|plugin| plugin.id == id)
                            {
                                plugin_details(plugin, now)?;
                            }
                            continue;
                        }
                        let enabled = model
                            .plugins
                            .iter()
                            .find(|plugin| plugin.id == id)
                            .is_some_and(|plugin| plugin.enabled);
                        let context = crate::services::context(paths)?;
                        deepseek_code_core::services::plugins::enable(&context, &id, !enabled)?;
                        session.reload_plugins().await?;
                        changed = true;
                        model = load(paths, session).await?;
                    }
                    Target::Mcp(name) => {
                        let server = session
                            .mcp
                            .list_servers()
                            .await
                            .into_iter()
                            .find(|server| server.name == name);
                        if !toggled {
                            if let Some(row) = model.mcp.iter().find(|server| server.name == name)
                            {
                                mcp_details(row, now)?;
                            }
                            continue;
                        }
                        let Some(server) = server else { continue };
                        if server.enabled {
                            crate::mcp_menu::disable(session, &server).await?;
                        } else {
                            // A failed start is shown in the Errors tab instead
                            // of closing the panel.
                            let _ = crate::mcp_menu::enable(session, &server).await;
                        }
                        model = load(paths, session).await?;
                    }
                }
            }
            1 => {
                let rows = error_rows(paths, session).await;
                let step = match details_from_top(&tab_bar(1), &rows, HINT, &[])? {
                    Some('<') => -1,
                    Some('>') => 1,
                    _ => return Ok(changed),
                };
                tab = (tab as isize + step).rem_euclid(TABS.len() as isize) as usize;
            }
            _ => {
                let rows = stats_rows(&model, Utc::now(), start == "skill-doctor");
                let step = match details_from_top(&tab_bar(tab), &rows, HINT, &[])? {
                    Some('<') => -1,
                    Some('>') => 1,
                    _ => return Ok(changed),
                };
                tab = (tab as isize + step).rem_euclid(TABS.len() as isize) as usize;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn skill(
        name: &str,
        source: &str,
        state: SkillState,
        listing_tokens: u64,
        usage: Usage,
    ) -> SkillRow {
        SkillRow {
            name: name.to_owned(),
            source: source.to_owned(),
            state,
            listing_tokens,
            usage,
        }
    }

    fn model() -> (Model, DateTime<Utc>) {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let five_days = now - chrono::Duration::days(5);
        let model = Model {
            skills: vec![
                skill("a", "user", SkillState::User, 50, Usage::default()),
                skill(
                    "b",
                    "dsb",
                    SkillState::User,
                    60,
                    Usage {
                        uses: 11,
                        last_used: Some(five_days),
                        tokens_7d: 2390,
                    },
                ),
                skill("c", "user", SkillState::Off, 0, Usage::default()),
            ],
            plugins: vec![
                PluginRow {
                    id: "dsb".to_owned(),
                    enabled: true,
                    skills: 1,
                    uses: 11,
                    last_used: Some(five_days),
                },
                PluginRow {
                    id: "old".to_owned(),
                    enabled: true,
                    skills: 0,
                    uses: 0,
                    last_used: None,
                },
            ],
            mcp: Vec::new(),
        };
        (model, now)
    }

    #[test]
    fn stats_rows_format() {
        let (model, now) = model();
        let rows = stats_rows(&model, now, false);
        println!("{}", rows.join("\n"));
        let text = rows.join("\n");
        assert!(!text.contains("/skill-doctor moved"));
        let fields = |name: &str| {
            rows.iter()
                .map(|row| row.split_whitespace().collect::<Vec<_>>())
                .find(|fields| fields.first() == Some(&name))
                .unwrap_or_else(|| panic!("no row for {name}: {text}"))
        };
        assert_eq!(fields("a"), ["a", "user", "~50", "-", "0\u{d7}", "never"]);
        assert_eq!(
            fields("b"),
            ["b", "dsb", "~60", "2.4k", "11\u{d7}", "5", "days"]
        );
        assert_eq!(fields("c"), ["c", "user", "-", "-", "0\u{d7}", "never"]);
        assert!(text.contains("1 skill loaded but never invoked."), "{text}");

        let start = rows
            .iter()
            .position(|row| row == "Plugins not used recently")
            .expect("plugin block");
        let block = &rows[start..];
        assert!(
            block.iter().any(|row| row.trim() == "old  never used"),
            "{block:?}"
        );
        assert!(
            !block.iter().any(|row| row.trim_start().starts_with("dsb")),
            "{block:?}"
        );

        let noticed = stats_rows(&model, now, true);
        assert!(
            noticed[0].starts_with("\u{2139} /skill-doctor moved"),
            "{:?}",
            noticed[0]
        );
    }

    #[test]
    fn source_labels() {
        let workspace = Path::new("/work/project");
        assert_eq!(
            source_label("dsb-skills:browser", Path::new("/cache/x.md"), workspace),
            "dsb-skills"
        );
        assert_eq!(
            source_label(
                "bundled",
                Path::new("__embedded_cli_skills__/bundled.md"),
                workspace
            ),
            "built-in"
        );
        assert_eq!(
            source_label(
                "project",
                Path::new("/work/project/.claude/skills/p/SKILL.md"),
                workspace
            ),
            "project"
        );
        assert_eq!(
            source_label(
                "mine",
                Path::new("/home/u/.claude/skills/m/SKILL.md"),
                workspace
            ),
            "user"
        );
    }

    fn installed_model(now: DateTime<Utc>) -> Model {
        let five_days = now - chrono::Duration::days(5);
        Model {
            skills: vec![
                skill("dcl", "user", SkillState::User, 39, Usage::default()),
                skill("pdf", "user", SkillState::UserOnly, 0, Usage::default()),
                skill("cu", "user", SkillState::Off, 0, Usage::default()),
            ],
            plugins: vec![
                PluginRow {
                    id: "dsb".to_owned(),
                    enabled: true,
                    skills: 1,
                    uses: 11,
                    last_used: Some(five_days),
                },
                PluginRow {
                    id: "old".to_owned(),
                    enabled: true,
                    skills: 0,
                    uses: 0,
                    last_used: None,
                },
                PluginRow {
                    id: "off-plugin".to_owned(),
                    enabled: false,
                    skills: 0,
                    uses: 0,
                    last_used: None,
                },
            ],
            mcp: vec![
                McpRow {
                    name: "codegraph".to_owned(),
                    enabled: true,
                    last_used: None,
                },
                McpRow {
                    name: "gh".to_owned(),
                    enabled: false,
                    last_used: None,
                },
            ],
        }
    }

    #[test]
    fn installed_sections_and_details() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let model = installed_model(now);

        let rows = installed_entries(&model, false, now);
        let titles: Vec<&str> = rows.iter().map(|(entry, _)| entry.title.as_str()).collect();
        assert_eq!(rows.len(), 9);
        assert_eq!(titles[0], "Not used recently");
        assert_eq!(titles[2], "User");
        assert_eq!(titles[5], "Skills");
        assert!(titles[1].contains("old") && titles[1].ends_with("never used"));
        assert!(titles[3].contains("dsb") && titles[3].contains("1 skill \u{b7} 11 uses"));
        assert!(titles[4].contains("codegraph") && titles[4].contains("MCP"));
        assert!(titles[6].contains("dcl"));
        assert!(
            titles[6].ends_with("user \u{b7} ~39 tok \u{b7} never used"),
            "{}",
            titles[6]
        );
        assert!(titles[7].contains("pdf"));
        assert!(
            titles[7].contains("user-only \u{b7} user \u{b7} not listed"),
            "{}",
            titles[7]
        );
        assert!(titles[8].trim_end().ends_with("\u{2192} Show disabled (3)"));

        let rows = installed_entries(&model, true, now);
        let titles: Vec<&str> = rows.iter().map(|(entry, _)| entry.title.as_str()).collect();
        let hide = titles
            .iter()
            .position(|title| title.trim_end().ends_with("\u{2193} Hide disabled (3)"))
            .expect("hide row");
        assert_eq!(titles[hide + 1], "Disabled plugins");
        assert!(titles[hide + 2].starts_with('\u{25cb}') && titles[hide + 2].contains("off-plugin"));
        assert_eq!(titles[hide + 3], "Disabled MCP");
        assert!(titles[hide + 4].starts_with('\u{25cb}') && titles[hide + 4].contains("gh"));
        assert_eq!(titles[hide + 5], "Disabled skills");
        assert!(titles[hide + 6].starts_with('\u{25cb}') && titles[hide + 6].contains("cu"));
        assert!(
            titles[hide + 6].contains("off \u{b7} user \u{b7} not listed"),
            "{}",
            titles[hide + 6]
        );
        assert_eq!(rows.len(), hide + 7);
    }

    #[test]
    fn space_cycles_between_off_and_user() {
        assert_eq!(space_state(SkillState::Off), SkillState::User);
        assert_eq!(space_state(SkillState::User), SkillState::Off);
        assert_eq!(space_state(SkillState::NameOnly), SkillState::Off);
        assert_eq!(space_state(SkillState::UserOnly), SkillState::Off);
    }

    #[test]
    fn synced_names_are_shortened() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let full = "synced:1234-abcd:translate";
        let long = "x".repeat(40);
        let model = Model {
            skills: vec![
                skill(full, "synced", SkillState::User, 20, Usage::default()),
                skill(&long, "user", SkillState::User, 30, Usage::default()),
            ],
            plugins: Vec::new(),
            mcp: Vec::new(),
        };

        let rows = stats_rows(&model, now, false);
        let text = rows.join("\n");
        let fields = |name: &str| {
            rows.iter()
                .map(|row| row.split_whitespace().collect::<Vec<_>>())
                .find(|fields| fields.first() == Some(&name))
                .unwrap_or_else(|| panic!("no row for {name}: {text}"))
        };
        assert_eq!(fields("translate")[0..2], ["translate", "synced"]);
        let cut = format!("{}\u{2026}", "x".repeat(23));
        assert_eq!(fields(&cut)[0].chars().count(), 24);
        assert!(text.contains(&cut), "{text}");
        assert!(!text.contains("synced:1234-abcd"), "{text}");

        assert_eq!(
            source_label(full, Path::new(""), Path::new("/work/project")),
            "synced"
        );

        let rows = installed_entries(&model, false, now);
        let translate = rows
            .iter()
            .find(|(_, target)| *target == Target::Skill(full.to_owned()))
            .expect("the full name stays the target");
        assert!(translate.0.title.contains("translate"), "{}", translate.0.title);
        assert!(!translate.0.title.contains("synced:1234-abcd"), "{}", translate.0.title);
        assert!(
            rows.iter()
                .any(|(entry, target)| entry.title.contains(&long) && *target == Target::Skill(long.clone())),
            "{:?}",
            rows.iter().map(|(entry, _)| &entry.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn skills_sort_by_shown_name() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let model = Model {
            skills: vec![
                skill("bundled:skill-creator", "bundled", SkillState::User, 10, Usage::default()),
                skill("build", "cli", SkillState::User, 10, Usage::default()),
                skill("claude-api", "cli", SkillState::User, 10, Usage::default()),
                skill("synced:1:alpha", "synced", SkillState::User, 10, Usage::default()),
            ],
            plugins: Vec::new(),
            mcp: Vec::new(),
        };
        let expected = ["alpha", "build", "claude-api", "skill-creator"];
        let known = |name: &str| expected.contains(&name);

        let stats: Vec<String> = stats_rows(&model, now, false)
            .iter()
            .filter_map(|row| row.split_whitespace().next().filter(|name| known(name)))
            .map(|name| name.to_owned())
            .collect();
        assert_eq!(stats, expected, "stats order");

        let installed: Vec<String> = installed_entries(&model, false, now)
            .iter()
            .filter_map(|(entry, _)| entry.title.split_whitespace().nth(1).filter(|name| known(name)))
            .map(|name| name.to_owned())
            .collect();
        assert_eq!(installed, expected, "installed order");
    }
}
