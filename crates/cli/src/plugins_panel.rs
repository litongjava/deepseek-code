//! The shared capabilities panel behind `/plugins`, `/skills` and `/tools`.
//!
//! Plugins and Skills use the Installed, Errors and Stats tabs; Tools uses
//! Built-in, Skills, Mcp and Plugin. Rows change state in place: skills and MCP
//! servers cycle through their four states, plugins enable or disable, and
//! built-in tools keep the resident/on-demand switches the Desktop page writes.
use crate::session::Session;
use crate::tool_settings;
use crate::terminal::input::editor::Screen;
use crate::terminal::input::TerminalInput;
use crate::terminal::picker::{details_from_top_with, Entry, Key, Picked, Picker};
use anyhow::Result;
use chrono::{DateTime, Utc};
use deepseek_code_core::mcp::states::{self as mcp_states, McpState};
use deepseek_code_core::native_engine::tool_loop::builtin_tool_catalog;
use deepseek_code_core::runtime::RuntimePaths;
use deepseek_code_core::services::customization::ToolLoading;
use deepseek_code_core::services::skill_usage::{self, Usage};
use deepseek_code_core::skills::states::{self, SkillState};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;


/// Restore the conversation screen on every exit, including errors.
struct PanelScreen;

impl PanelScreen {
    fn enter() -> std::io::Result<Self> {
        // Drop the finished turn's input box before the alternate screen takes
        // over; releasing it afterwards would jump the cursor on the new screen.
        crate::terminal::release_handoff();
        crossterm::execute!(
            crate::terminal_output::stderr(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        Ok(Self)
    }
}

impl Drop for PanelScreen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(crate::terminal_output::stderr(), crossterm::terminal::LeaveAlternateScreen);
    }
}

const TABS: [&str; 3] = ["Installed", "Errors", "Stats"];
const TOOL_TABS: [&str; 4] = ["Built-in", "Skills", "Mcp", "Plugin"];
const HINT: &str = "\u{2190}/\u{2192} to switch tabs \u{b7} Esc to close";
const INSTALLED_HINT: &str = "Type to search \u{b7} Space to toggle \u{b7} Enter to view \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close";
const TOOL_HINT: &str = "Type to search \u{b7} Space: enabled \u{b7} Tab: on demand \u{b7} \u{2190}/\u{2192} to switch tabs \u{b7} Esc to close";

/// What a panel shows and which tabs it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Plugins,
    Skills,
    Tools,
}

impl Scope {
    fn title(self) -> &'static str {
        match self {
            Scope::Plugins => "Plugins",
            Scope::Skills => "Skills",
            Scope::Tools => "Tools",
        }
    }

    fn tabs(self) -> &'static [&'static str] {
        match self {
            Scope::Plugins | Scope::Skills => &TABS,
            Scope::Tools => &TOOL_TABS,
        }
    }
}

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
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub state: McpState,
    /// Estimated tokens the server's tools cost in every request.
    pub context_tokens: u64,
    pub last_used: Option<DateTime<Utc>>,
    /// First line of a start failure, shown with the row and in its details.
    pub failure: Option<String>,
}

/// One built-in tool with the switches the Desktop Tools page writes.
pub(crate) struct ToolRow {
    pub name: String,
    pub core: bool,
    pub enabled: bool,
    pub on_demand: bool,
    /// Estimated tokens the tool's schema costs when it is resident.
    pub context_tokens: u64,
}

pub(crate) struct Model {
    pub skills: Vec<SkillRow>,
    pub plugins: Vec<PluginRow>,
    pub mcp: Vec<McpRow>,
    pub tools: Vec<ToolRow>,
}

/// What Enter or Space on an Installed row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Header,
    Plugin(String),
    Mcp(String),
    Skill(String),
    Tool(String),
    ToggleDisabled,
}

/// `Plugins  [Installed]   Errors   Stats`, as in the settings dialog.
fn tab_bar(title: &str, tabs: &[&str], current: usize) -> String {
    let tabs: Vec<String> = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            if index == current {
                format!("[{tab}]")
            } else {
                (*tab).to_owned()
            }
        })
        .collect();
    format!("{title}  {}", tabs.join("   "))
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

/// Where a skill comes from: a plugin ID, the installed built-in skills, the
/// workspace's `.claude` directory, or the user scope.
fn source_label(name: &str, path: &Path, workspace: &Path, data_dir: &Path) -> String {
    if name.starts_with("synced:") {
        return "synced".to_owned();
    }
    if let Some((plugin, _)) = name.split_once(':') {
        return plugin.to_owned();
    }
    if deepseek_code_core::skills::discovery::is_bundled_path(path, data_dir) {
        return "built-in".to_owned();
    }
    if path.starts_with(workspace.join(".claude")) {
        return "project".to_owned();
    }
    "user".to_owned()
}

/// Skills include ones set to `off`: the resolver drops them, but the panel
/// must still show them so they can be turned back on.
pub(crate) async fn load(
    paths: &RuntimePaths,
    session: &Session,
    scope: Scope,
) -> Result<Model> {
    let context = crate::services::context(paths)?;
    deepseek_code_core::skills::SkillsManager::with_path(paths.data_dir.join("skills"))
        .ensure_installed()?;
    let mut commands =
        deepseek_code_core::skills::discovery::discover(&paths.data_dir, &session.workspace)?;
    commands.extend(deepseek_code_core::services::plugins::skill_commands(
        &context,
    )?);
    let states = states::load(&paths.data_dir);
    let bundled = deepseek_code_core::skills::discovery::bundled_dir(&paths.data_dir);
    let installed =
        deepseek_code_core::skills::discovery::installed_bundled_skills(&paths.data_dir);
    let usages = session.store.db.skill_usage(Utc::now(), || false)?;
    let mut names: BTreeSet<String> = commands.keys().cloned().collect();
    names.extend(installed.keys().cloned());
    names.extend(states.keys().cloned());
    let mut skills: Vec<SkillRow> = names
        .into_iter()
        .map(|name| {
            let command = commands.get(&name);
            let is_bundled = command.map_or_else(
                || installed.contains_key(&name),
                |command| command.path.starts_with(&bundled),
            );
            let state = states::effective_in(&states, &name, is_bundled);
            let listing_tokens = match state {
                SkillState::Off => 0,
                SkillState::Always => {
                    command.map_or(0, |command| skill_usage::estimate_tokens(&command.content))
                }
                _ => command.map_or(0, skill_usage::listing_tokens),
            };
            let path = command.map_or_else(
                || installed.get(&name).map_or(Path::new(""), |path| path.as_path()),
                |command| command.path.as_path(),
            );
            SkillRow {
                source: source_label(&name, path, &session.workspace, &paths.data_dir),
                state,
                listing_tokens,
                usage: usages.get(&name).cloned().unwrap_or_default(),
                name,
            }
        })
        .collect();
    skills.sort_by(|a, b| skill_order(a, b));

    let mut plugins: Vec<PluginRow> = if scope == Scope::Skills {
        Vec::new()
    } else {
        deepseek_code_core::services::plugins::list(&context)?
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
            .collect()
    };
    plugins.sort_by(|a, b| a.id.cmp(&b.id));

    // MCP belongs to the Tools panel only. The `/mcp` menu's list is used
    // without its bundled presets nobody changed; `ensure_mcp` starts enabled
    // servers once and keeps live connectors, so the token estimate sees their
    // tools and opening the panel does not disconnect running servers.
    let mcp: Vec<McpRow> = if scope == Scope::Tools {
        session.ensure_mcp().await?;
        let last_used = session.store.db.mcp_last_used(|| false)?;
        let stored_mcp = mcp_states::load(&paths.data_dir);
        let live_tools = session.mcp.get_all_tools().await;
        let mut mcp: Vec<McpRow> = session
            .mcp
            .list_servers()
            .await
            .into_iter()
            .filter(|server| !crate::mcp_menu::untouched_preset(server))
            .map(|server| {
                let state = mcp_states::shown(&stored_mcp, &server.id, server.enabled);
                let tools: Vec<_> = if server.running {
                    live_tools
                        .iter()
                        .filter(|tool| {
                            tool.server_name == server.id || tool.server_name == server.name
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let qualified = |tool: &deepseek_code_core::mcp::McpTool| {
                    deepseek_code_core::mcp::tool_executor::qualified_tool_name(
                        &server.id,
                        &tool.name,
                    )
                };
                let context_tokens = match state {
                    McpState::Off | McpState::UserOnly => 0,
                    McpState::User => tools
                        .iter()
                        .map(|tool| {
                            skill_usage::estimate_tokens(&format!(
                                "{}\n{}\n{}",
                                qualified(tool),
                                tool.description,
                                tool.input_schema
                            ))
                        })
                        .sum(),
                    McpState::OnDemand => {
                        let lines: Vec<String> = tools
                            .iter()
                            .map(|tool| {
                                format!(
                                    "- {}: {}",
                                    qualified(tool),
                                    mcp_states::description_summary(&tool.description)
                                )
                            })
                            .collect();
                        skill_usage::estimate_tokens(&lines.join("\n"))
                    }
                };
                McpRow {
                    failure: crate::mcp_menu::failure(&server)
                        .map(|text| text.lines().next().unwrap_or_default().to_owned()),
                    last_used: last_used
                        .get(&server.name)
                        .copied()
                        .or_else(|| last_used.get(&server.id).copied()),
                    id: server.id,
                    name: server.name,
                    enabled: server.enabled,
                    state,
                    context_tokens,
                }
            })
            .collect();
        mcp.sort_by(|a, b| a.name.cmp(&b.name));
        mcp
    } else {
        Vec::new()
    };

    let tools = if scope == Scope::Tools {
        tool_rows(paths)?
    } else {
        Vec::new()
    };

    Ok(Model {
        skills,
        plugins,
        mcp,
        tools,
    })
}

/// The built-in catalog with each tool's saved switches and schema cost.
fn tool_rows(paths: &RuntimePaths) -> Result<Vec<ToolRow>> {
    let tokens: BTreeMap<String, u64> = builtin_tool_catalog()
        .into_iter()
        .map(|tool| {
            (
                tool.name.clone(),
                skill_usage::estimate_tokens(&format!(
                    "{}\n{}\n{}",
                    tool.name, tool.description, tool.input_schema
                )),
            )
        })
        .collect();
    Ok(tool_settings::rows(paths)?
        .into_iter()
        .map(|row| ToolRow {
            context_tokens: tokens.get(&row.name).copied().unwrap_or(0),
            name: row.name,
            core: row.core,
            enabled: row.enabled,
            on_demand: row.on_demand,
        })
        .collect())
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

/// The Stats tab. Skills shows listing cost and usage; Plugins shows bundle
/// usage. The Tools panel has no Stats tab.
pub(crate) fn stats_rows(
    scope: Scope,
    model: &Model,
    now: DateTime<Utc>,
    notice: bool,
) -> Vec<String> {
    match scope {
        Scope::Skills => skill_stats_rows(model, now, notice),
        Scope::Plugins => plugin_stats_rows(model, now),
        Scope::Tools => Vec::new(),
    }
}

fn skill_stats_rows(model: &Model, now: DateTime<Utc>, notice: bool) -> Vec<String> {
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
            "{unused} {noun} loaded but never invoked. Each one adds to the system prompt every turn. Disable it in /skills, or set /skills state NAME off."
        ));
        rows.push(String::new());
    }
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    rows
}

fn plugin_stats_rows(model: &Model, now: DateTime<Utc>) -> Vec<String> {
    let mut plugins: Vec<&PluginRow> = model.plugins.iter().collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    let name_width = plugins
        .iter()
        .map(|plugin| plugin.id.chars().count())
        .max()
        .unwrap_or(0)
        .clamp("plugin".len(), 32);
    let mut rows = vec![
        "Plugins installed".to_owned(),
        String::new(),
        format!(
            "  {:<name_width$}  {:<6}  {:<4}  last used",
            "plugin", "skills", "uses"
        ),
    ];
    for plugin in &plugins {
        rows.push(format!(
            "  {:<name_width$}  {:<6}  {:<4}  {}",
            fit(&plugin.id, name_width),
            plugin.skills,
            format!("{}\u{d7}", plugin.uses),
            skill_usage::relative_day(plugin.last_used, now),
        ));
    }
    rows.push(String::new());
    let stale = plugins
        .iter()
        .filter(|plugin| {
            plugin
                .last_used
                .is_none_or(|last| now - last > chrono::Duration::days(14))
        })
        .count();
    if stale > 0 {
        let noun = if stale == 1 { "plugin" } else { "plugins" };
        rows.push(format!(
            "{stale} {noun} not used in 14 days; manage them in the Installed tab."
        ));
        rows.push(String::new());
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

/// Space cycles an MCP server between `off` and `user`, as for skills.
pub(crate) fn space_mcp_state(current: McpState) -> McpState {
    match current {
        McpState::Off => McpState::User,
        _ => McpState::Off,
    }
}

/// Apply a state to the live session, starting or stopping the server.
async fn set_mcp_state(
    paths: &RuntimePaths,
    session: &Session,
    server: &deepseek_code_core::mcp::McpServerStatus,
    state: McpState,
) -> Result<()> {
    crate::mcp_states::apply(paths, &session.mcp, server, state).await?;
    if state == McpState::Off {
        crate::mcp_menu::disable(session, server).await?;
    } else {
        // A failed start is shown in the Errors tab instead of closing the panel.
        let _ = crate::mcp_menu::enable(session, server).await;
    }
    Ok(())
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

fn mcp_title(server: &McpRow) -> String {
    let mut title = format!(
        "\u{2714} {:<28} MCP      {}",
        server.name,
        server.state.label()
    );
    if server.failure.is_some() {
        title.push_str(" \u{b7} failed");
    }
    if server.context_tokens > 0 {
        title.push_str(&format!(" \u{b7} ~{} tok", server.context_tokens));
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

/// The Installed tab's rows for one scope. Sections that would be empty are
/// omitted together with their header.
pub(crate) fn installed_entries(
    scope: Scope,
    model: &Model,
    show_disabled: bool,
    now: DateTime<Utc>,
) -> Vec<(Entry, Target)> {
    match scope {
        Scope::Plugins => plugin_entries(model, show_disabled, now),
        Scope::Skills => skill_entries(model, show_disabled, now),
        Scope::Tools => Vec::new(),
    }
}

fn toggle_disabled(show_disabled: bool, hidden: usize) -> (Entry, Target) {
    let arrow = if show_disabled {
        "\u{2193} Hide disabled"
    } else {
        "\u{2192} Show disabled"
    };
    (
        Entry {
            title: format!("  {arrow} ({hidden})"),
            detail: String::new(),
        },
        Target::ToggleDisabled,
    )
}

fn plugin_entries(model: &Model, show_disabled: bool, now: DateTime<Utc>) -> Vec<(Entry, Target)> {
    let mut rows = Vec::new();
    let mut plugins: Vec<&PluginRow> = model.plugins.iter().filter(|plugin| plugin.enabled).collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    let (stale, recent): (Vec<&PluginRow>, Vec<&PluginRow>) = plugins
        .into_iter()
        .partition(|plugin| {
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
    if !recent.is_empty() {
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
    }

    let mut disabled: Vec<&PluginRow> =
        model.plugins.iter().filter(|plugin| !plugin.enabled).collect();
    disabled.sort_by(|a, b| a.id.cmp(&b.id));
    if !disabled.is_empty() {
        rows.push(toggle_disabled(show_disabled, disabled.len()));
        if show_disabled {
            rows.push(header("Disabled plugins"));
            for plugin in disabled {
                rows.push((
                    Entry {
                        title: format!("\u{25cb} {:<28} Plugin   disabled", plugin.id),
                        detail: String::new(),
                    },
                    Target::Plugin(plugin.id.clone()),
                ));
            }
        }
    }
    rows
}

fn skill_entries(model: &Model, show_disabled: bool, now: DateTime<Utc>) -> Vec<(Entry, Target)> {
    let mut rows = Vec::new();
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
                    title: format!(
                        "\u{2714} {:<28} Skill    {}",
                        display_name(&skill.name),
                        skill_detail(skill, now)
                    ),
                    detail: String::new(),
                },
                Target::Skill(skill.name.clone()),
            ));
        }
    }

    let mut disabled: Vec<&SkillRow> = model
        .skills
        .iter()
        .filter(|skill| skill.state == SkillState::Off)
        .collect();
    disabled.sort_by(|a, b| skill_order(a, b));
    if !disabled.is_empty() {
        rows.push(toggle_disabled(show_disabled, disabled.len()));
        if show_disabled {
            rows.push(header("Disabled skills"));
            for skill in disabled {
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
    }
    rows
}

fn mcp_entries(model: &Model, show_disabled: bool) -> Vec<(Entry, Target)> {
    let mut rows = Vec::new();
    let mut servers: Vec<&McpRow> = model.mcp.iter().filter(|server| server.enabled).collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    if !servers.is_empty() {
        rows.push(header("MCP"));
        for server in servers {
            rows.push((
                Entry {
                    title: mcp_title(server),
                    detail: String::new(),
                },
                Target::Mcp(server.name.clone()),
            ));
        }
    }
    let mut disabled: Vec<&McpRow> = model.mcp.iter().filter(|server| !server.enabled).collect();
    disabled.sort_by(|a, b| a.name.cmp(&b.name));
    if !disabled.is_empty() {
        rows.push(toggle_disabled(show_disabled, disabled.len()));
        if show_disabled {
            rows.push(header("Disabled MCP"));
            for server in disabled {
                rows.push((
                    Entry {
                        title: format!(
                            "\u{25cb} {:<28} MCP      {}",
                            server.name,
                            server.state.label()
                        ),
                        detail: String::new(),
                    },
                    Target::Mcp(server.name.clone()),
                ));
            }
        }
    }
    rows
}

/// Stable display groups for the built-in catalog. A tool missing here falls
/// into "Other" instead of disappearing; `every_builtin_tool_has_one_group`
/// keeps the map aligned with the catalog.
const TOOL_GROUPS: &[(&str, &[&str])] = &[
    (
        "Files & workspace",
        &[
            "ApplyPatch",
            "Edit",
            "EnterWorktree",
            "ExitWorktree",
            "Glob",
            "Grep",
            "LSP",
            "ListDir",
            "MultiEdit",
            "NotebookEdit",
            "Read",
            "Write",
        ],
    ),
    ("Shell & system", &["Bash", "Monitor", "Python", "computer_use"]),
    ("Web & services", &["GitHub", "ImageGenerate", "WebFetch", "WebSearch"]),
    (
        "Agents & delegation",
        &[
            "Agent",
            "ClaudeCode",
            "Codex",
            "DeepSeekCode",
            "ListAgents",
            "SendMessage",
            "team",
        ],
    ),
    (
        "Tasks & plans",
        &[
            "EnterPlanMode",
            "ExitPlanMode",
            "TaskCreate",
            "TaskGet",
            "TaskInput",
            "TaskList",
            "TaskStop",
            "TaskUpdate",
            "TodoRead",
            "TodoWrite",
        ],
    ),
    (
        "Automation",
        &[
            "CronCreate",
            "CronDelete",
            "CronList",
            "PluginRun",
            "ProposeGoal",
            "ReportFindings",
            "ScheduleWakeup",
            "ToolScript",
            "Workflow",
        ],
    ),
    (
        "Context & interaction",
        &[
            "AskUserQuestion",
            "CompactContext",
            "Config",
            "ContextSearch",
            "Skill",
            "SyntheticOutput",
            "ToolLoad",
            "brief",
        ],
    ),
];

const OTHER_TOOL_GROUP: &str = "Other";

#[cfg(test)]
fn tool_group(name: &str) -> &'static str {
    TOOL_GROUPS
        .iter()
        .find(|(_, names)| names.contains(&name))
        .map_or(OTHER_TOOL_GROUP, |(group, _)| *group)
}

/// Every built-in tool with its two switches and schema cost, grouped so the
/// 50+ row catalog stays scannable.
fn tool_entries(model: &Model) -> Vec<(Entry, Target)> {
    let width = model
        .tools
        .iter()
        .map(|tool| tool.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp("tool".len(), 28);
    let mut rows = Vec::new();
    let mut remaining: Vec<&ToolRow> = model.tools.iter().collect();
    for (group, names) in TOOL_GROUPS {
        let mut tools: Vec<&ToolRow> = remaining
            .iter()
            .copied()
            .filter(|tool| names.contains(&tool.name.as_str()))
            .collect();
        if tools.is_empty() {
            continue;
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        rows.push(header(group));
        for tool in tools {
            rows.push(tool_entry(tool, width));
        }
        remaining.retain(|tool| !names.contains(&tool.name.as_str()));
    }
    if !remaining.is_empty() {
        remaining.sort_by(|a, b| a.name.cmp(&b.name));
        rows.push(header(OTHER_TOOL_GROUP));
        for tool in remaining {
            rows.push(tool_entry(tool, width));
        }
    }
    rows
}

fn tool_entry(tool: &ToolRow, width: usize) -> (Entry, Target) {
    let mark = if tool.enabled { "\u{2714}" } else { "\u{25cb}" };
    let status = tool_settings::summary(ToolLoading {
        enabled: tool.enabled,
        on_demand: tool.on_demand,
    });
    let mut title = format!(
        "{mark} {:<width$} Tool     {status} \u{b7} ~{} tok",
        tool.name,
        tool.context_tokens,
        width = width
    );
    if tool.core {
        title.push_str(" \u{b7} core");
    }
    (
        Entry {
            title,
            detail: String::new(),
        },
        Target::Tool(tool.name.clone()),
    )
}

/// The Errors tab. Skills lists unreadable skill files; Plugins has no failure
/// source of its own. MCP failures travel with the MCP rows in `/tools`.
async fn error_rows(scope: Scope, paths: &RuntimePaths, session: &Session) -> Vec<String> {
    let mut rows = Vec::new();
    match scope {
        Scope::Skills => {
            if let Err(error) =
                deepseek_code_core::skills::SkillsManager::with_path(paths.data_dir.join("skills"))
                    .ensure_installed()
            {
                rows.push(format!(
                    "\u{2717} bundled skills \u{2014} install failed: {error}"
                ));
            }
            let mut invalid = deepseek_code_core::skills::discovery::invalid_skill_files(
                &paths.data_dir,
                &session.workspace,
            );
            invalid.sort();
            for path in invalid {
                rows.push(format!(
                    "\u{2717} {} \u{2014} invalid or unreadable skill file; run /doctor",
                    path.display()
                ));
            }
        }
        Scope::Plugins => rows.push("No plugin errors.".to_owned()),
        Scope::Tools => {}
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
        (
            SkillState::Always,
            "Full instructions injected on turn 1; you and the model can invoke it".to_owned(),
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
    let mut screen = Screen::top_anchored();
    let picked = picker.run_keys_with(&[], &mut screen)?;
    Ok(match picked {
        Picked::Entry(index) => Some(choices[index].0),
        _ => None,
    })
}

/// What the MCP state picker selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpStateChoice {
    State(McpState),
    Details,
}

/// The four MCP states plus the details view, with the current one marked.
pub(crate) fn pick_mcp_state(
    name: &str,
    current: McpState,
) -> std::io::Result<Option<McpStateChoice>> {
    let choices = [
        (
            McpState::Off,
            "Not started; its tools and prompts are unavailable".to_owned(),
        ),
        (
            McpState::User,
            "All tool schemas are sent with every request".to_owned(),
        ),
        (
            McpState::OnDemand,
            "Tools are listed by name and loaded with ToolLoad when needed".to_owned(),
        ),
        (
            McpState::UserOnly,
            "Connected; the model cannot see or call its tools; its prompts stay available to you"
                .to_owned(),
        ),
    ];
    let mut entries: Vec<Entry> = choices
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
    entries.push(Entry {
        title: "Details".to_owned(),
        detail: String::new(),
    });
    let heading = format!("MCP state: {name}");
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
    let mut screen = Screen::top_anchored();
    let picked = picker.run_keys_with(&[], &mut screen)?;
    Ok(match picked {
        Picked::Entry(index) => Some(
            choices
                .get(index)
                .map(|(state, _)| McpStateChoice::State(*state))
                .unwrap_or(McpStateChoice::Details),
        ),
        _ => None,
    })
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
    let mut screen = Screen::top_anchored();
    details_from_top_with(
        &plugin.id,
        &rows,
        "\u{2190} to go back \u{b7} Esc to close",
        &[],
        &mut screen,
    )?;
    Ok(())
}

fn mcp_details(server: &McpRow, now: DateTime<Utc>) -> std::io::Result<()> {
    let mut rows = vec![
        format!("name:      {}", server.name),
        format!("id:        {}", server.id),
        "kind:      MCP".to_owned(),
        format!("enabled:   {}", if server.enabled { "yes" } else { "no" }),
        format!("state:     {}", server.state.label()),
        format!(
            "context:   {}",
            if server.context_tokens == 0 {
                "-".to_owned()
            } else {
                format!("~{} tok", server.context_tokens)
            }
        ),
        format!(
            "last used: {}",
            skill_usage::relative_day(server.last_used, now)
        ),
    ];
    if let Some(failure) = &server.failure {
        rows.push(format!("failed:    {failure}"));
    }
    let mut screen = Screen::top_anchored();
    details_from_top_with(
        &server.name,
        &rows,
        "\u{2190} to go back \u{b7} Esc to close",
        &[],
        &mut screen,
    )?;
    Ok(())
}

/// The tab a panel opens on: `/skill-doctor` lands on Stats, a tab name on
/// that tab, anything else on the first tab.
fn start_tab(scope: Scope, start: &str) -> usize {
    let tabs = scope.tabs();
    if scope != Scope::Tools && start.eq_ignore_ascii_case("skill-doctor") {
        return tabs.iter().position(|tab| *tab == "Stats").unwrap_or(0);
    }
    tabs.iter()
        .position(|tab| tab.eq_ignore_ascii_case(start))
        .unwrap_or(0)
}

/// The rows of the picker a tab shows. Errors and Stats are read-only pages and
/// are handled before this point.
fn picker_rows(
    scope: Scope,
    tab: usize,
    model: &Model,
    show_disabled: bool,
) -> Vec<(Entry, Target)> {
    match (scope, tab) {
        (Scope::Plugins | Scope::Skills, 0) => {
            installed_entries(scope, model, show_disabled, Utc::now())
        }
        (Scope::Tools, 0) => tool_entries(model),
        (Scope::Tools, 1) => skill_entries(model, show_disabled, Utc::now()),
        (Scope::Tools, 2) => mcp_entries(model, show_disabled),
        (Scope::Tools, 3) => plugin_entries(model, show_disabled, Utc::now()),
        _ => Vec::new(),
    }
}

fn picker_hint(scope: Scope, tab: usize) -> (&'static str, &'static [Key]) {
    if scope == Scope::Tools && tab == 0 {
        (
            TOOL_HINT,
            &[Key::Left, Key::Right, Key::Space, Key::Tab],
        )
    } else {
        (INSTALLED_HINT, &[Key::Left, Key::Right, Key::Space])
    }
}

pub(crate) async fn run(
    scope: Scope,
    start: &str,
    paths: &RuntimePaths,
    session: &mut Session,
    _input: &mut TerminalInput,
) -> Result<bool> {
    let _screen = PanelScreen::enter()?;
    let mut model = load(paths, session, scope).await?;
    let tabs = scope.tabs();
    let mut tab = start_tab(scope, start);
    let mut selected = 0;
    let mut show_disabled = false;
    let mut changed = false;
    let mut notice: Option<String> = None;
    // Reused across picker calls so a Space toggle repaints the changed row
    // instead of rebuilding and scrolling the whole view.
    let mut frame = Screen::top_anchored();
    loop {
        // Errors and Stats are read-only pages.
        if matches!(scope, Scope::Plugins | Scope::Skills) && tab > 0 {
            let rows = if tab == 1 {
                error_rows(scope, paths, session).await
            } else {
                stats_rows(
                    scope,
                    &model,
                    Utc::now(),
                    start.eq_ignore_ascii_case("skill-doctor"),
                )
            };
            let step =
                match details_from_top_with(&tab_bar(scope.title(), tabs, tab), &rows, HINT, &[], &mut frame)? {
                    Some('<') => -1,
                    Some('>') => 1,
                    _ => return Ok(changed),
                };
            // The details view used its own frame; force the picker to repaint.
            frame.reflow();
            tab = (tab as isize + step).rem_euclid(tabs.len() as isize) as usize;
            continue;
        }

        let rows = picker_rows(scope, tab, &model, show_disabled);
        // Group headers are separators: start on the first row that acts.
        if matches!(scope, Scope::Tools) {
            if let Some((_, Target::Header)) = rows.get(selected) {
                if let Some(next) = rows
                    .iter()
                    .position(|(_, target)| !matches!(target, Target::Header))
                {
                    selected = next;
                }
            }
        }
        let entries: Vec<Entry> = rows
            .iter()
            .map(|(entry, _)| Entry {
                title: entry.title.clone(),
                detail: entry.detail.clone(),
            })
            .collect();
        let bar = tab_bar(scope.title(), tabs, tab);
        // Keep the notice on the always-present heading line: a subtitle adds
        // a row and shifts the whole list, which turns one toggle into a full
        // repaint.
        let heading = match &notice {
            Some(note) => format!("{bar}  \u{b7}  {note}"),
            None => bar,
        };
        let (hint, keys) = picker_hint(scope, tab);
        let picker = Picker {
            heading: &heading,
            subtitle: None,
            entries: &entries,
            search: true,
            hint,
            selected,
        };
        let (index, key) = match picker.run_keys_filling_with(keys, &mut frame)? {
            Picked::Key(Key::Left, _) => {
                tab = (tab + tabs.len() - 1) % tabs.len();
                continue;
            }
            Picked::Key(Key::Right, _) => {
                tab = (tab + 1) % tabs.len();
                continue;
            }
            Picked::Cancel => return Ok(changed),
            Picked::Key(key, Some(index)) => (index, Some(key)),
            Picked::Key(_, None) => continue,
            Picked::Entry(index) => (index, None),
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
                match key {
                    Some(Key::Space) => {
                        states::set(&paths.data_dir, &name, space_state(current))?;
                        changed = true;
                        model = load(paths, session, scope).await?;
                    }
                    None => {
                        if let Some(state) = pick_skill_state(&name, current)? {
                            states::set(&paths.data_dir, &name, state)?;
                            changed = true;
                            model = load(paths, session, scope).await?;
                        }
                        frame.reflow();
                    }
                    _ => {}
                }
            }
            Target::Plugin(id) => {
                if key.is_none() {
                    if let Some(plugin) = model.plugins.iter().find(|plugin| plugin.id == id) {
                        plugin_details(plugin, Utc::now())?;
                    }
                    frame.reflow();
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
                model = load(paths, session, scope).await?;
            }
            Target::Mcp(name) => {
                let Some(row) = model.mcp.iter().find(|server| server.name == name) else {
                    continue;
                };
                let (id, current) = (row.id.clone(), row.state);
                let Some(server) = session
                    .mcp
                    .list_servers()
                    .await
                    .into_iter()
                    .find(|server| server.id == id)
                else {
                    continue;
                };
                match key {
                    Some(Key::Space) => {
                        set_mcp_state(paths, session, &server, space_mcp_state(current)).await?;
                    }
                    None => {
                        let choice = pick_mcp_state(&name, current)?;
                        frame.reflow();
                        match choice {
                            Some(McpStateChoice::State(state)) => {
                                set_mcp_state(paths, session, &server, state).await?
                            }
                            Some(McpStateChoice::Details) => {
                                if let Some(row) = model.mcp.iter().find(|server| server.name == name) {
                                    mcp_details(row, Utc::now())?;
                                }
                                frame.reflow();
                                continue;
                            }
                            None => continue,
                        }
                    }
                    _ => continue,
                }
                changed = true;
                model = load(paths, session, scope).await?;
            }
            Target::Tool(name) => {
                let Some(key) = key else {
                    continue;
                };
                let Some(row) = model.tools.iter().find(|tool| tool.name == name) else {
                    continue;
                };
                let switch = match key {
                    Key::Space => tool_settings::Switch::Enabled,
                    Key::Tab => tool_settings::Switch::OnDemand,
                    _ => continue,
                };
                let current = match switch {
                    tool_settings::Switch::Enabled => row.enabled,
                    tool_settings::Switch::OnDemand => row.on_demand,
                };
                match tool_settings::set(paths, &name, switch, !current) {
                    Ok((name, saved)) => {
                        notice = Some(format!("{name}: {}", tool_settings::summary(saved.loading(&name))));
                        // Update in memory: a full reload also reopens MCP and
                        // would repaint the list for a single switch.
                        if let Some(row) = model.tools.iter_mut().find(|tool| tool.name == name) {
                            match switch {
                                tool_settings::Switch::Enabled => row.enabled = !current,
                                tool_settings::Switch::OnDemand => row.on_demand = !current,
                            }
                        }
                    }
                    Err(error) => notice = Some(format!("{error:#}")),
                }
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
            tools: Vec::new(),
        };
        (model, now)
    }

    #[test]
    fn stats_rows_format() {
        let (model, now) = model();
        let rows = stats_rows(Scope::Skills, &model, now, false);
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
        assert!(!text.contains("Plugins not used recently"), "{text}");

        let noticed = stats_rows(Scope::Skills, &model, now, true);
        assert!(
            noticed[0].starts_with("\u{2139} /skill-doctor moved"),
            "{:?}",
            noticed[0]
        );
    }

    #[test]
    fn plugin_stats_measure_bundles_not_skills() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let model = installed_model(now);
        let rows = stats_rows(Scope::Plugins, &model, now, false);
        let text = rows.join("\n");
        assert!(text.contains("Plugins installed"), "{text}");
        assert!(text.contains("2 plugins not used in 14 days"), "{text}");
        assert!(!text.contains("Skills loaded this session"), "{text}");
        assert!(!text.contains("MCP servers"), "{text}");
        let fields = |name: &str| {
            rows.iter()
                .map(|row| row.split_whitespace().collect::<Vec<_>>())
                .find(|fields| fields.first() == Some(&name))
                .unwrap_or_else(|| panic!("no row for {name}: {text}"))
        };
        assert_eq!(fields("dsb"), ["dsb", "1", "11\u{d7}", "5", "days"]);
        assert_eq!(fields("old"), ["old", "0", "0\u{d7}", "never"]);
    }

    #[test]
    fn tab_bars_name_their_scope() {
        assert_eq!(tab_bar("Plugins", &TABS, 0), "Plugins  [Installed]   Errors   Stats");
        assert_eq!(tab_bar("Skills", &TABS, 2), "Skills  Installed   Errors   [Stats]");
        assert_eq!(tab_bar("Tools", &TOOL_TABS, 1), "Tools  Built-in   [Skills]   Mcp   Plugin");
    }

    #[test]
    fn every_builtin_tool_has_one_group() {
        let total: usize = TOOL_GROUPS.iter().map(|(_, names)| names.len()).sum();
        let grouped: std::collections::BTreeSet<String> = TOOL_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().map(|name| (*name).to_owned()))
            .collect();
        assert_eq!(total, grouped.len(), "a tool is listed in two groups");
        let catalog: std::collections::BTreeSet<String> = builtin_tool_catalog()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            catalog, grouped,
            "classify new tools in TOOL_GROUPS or drop removed ones"
        );
        assert_eq!(tool_group("NoSuchTool"), OTHER_TOOL_GROUP);
    }

    #[test]
    fn tool_rows_are_grouped_with_an_other_fallback() {
        let tool = |name: &str| ToolRow {
            name: name.to_owned(),
            core: false,
            enabled: false,
            on_demand: true,
            context_tokens: 10,
        };
        let model = Model {
            skills: Vec::new(),
            plugins: Vec::new(),
            mcp: Vec::new(),
            tools: vec![tool("Write"), tool("Bash"), tool("Agent"), tool("Mystery")],
        };
        let rows = tool_entries(&model);
        let titles: Vec<&str> = rows.iter().map(|(entry, _)| entry.title.as_str()).collect();
        assert_eq!(titles[0], "Files & workspace");
        assert!(titles[1].contains("Write"), "{}", titles[1]);
        assert_eq!(titles[2], "Shell & system");
        assert!(titles[3].contains("Bash"), "{}", titles[3]);
        assert_eq!(titles[4], "Agents & delegation");
        assert!(titles[5].contains("Agent"), "{}", titles[5]);
        assert_eq!(titles[6], OTHER_TOOL_GROUP);
        assert!(titles[7].contains("Mystery"), "{}", titles[7]);
        assert!(rows
            .iter()
            .any(|(_, target)| *target == Target::Tool("Mystery".to_owned())));
    }

    #[test]
    fn start_tab_maps_skill_doctor_to_stats() {
        let stats = TABS.iter().position(|tab| *tab == "Stats").unwrap();
        let errors = TABS.iter().position(|tab| *tab == "Errors").unwrap();
        assert_eq!(start_tab(Scope::Skills, "skill-doctor"), stats);
        assert_eq!(start_tab(Scope::Plugins, "installed"), 0);
        assert_eq!(start_tab(Scope::Plugins, "errors"), errors);
        assert_eq!(start_tab(Scope::Plugins, "nonsense"), 0);
        assert_eq!(start_tab(Scope::Tools, "built-in"), 0);
        assert_eq!(start_tab(Scope::Tools, "mcp"), 2);
        assert_eq!(start_tab(Scope::Tools, "plugin"), 3);
    }

    #[test]
    fn source_labels() {
        let workspace = Path::new("/work/project");
        let data_dir = Path::new("/data/dcl");
        assert_eq!(
            source_label(
                "dsb-skills:browser",
                Path::new("/cache/x.md"),
                workspace,
                data_dir
            ),
            "dsb-skills"
        );
        assert_eq!(
            source_label(
                "bundled",
                Path::new("/data/dcl/skills/bundled/bundled/SKILL.md"),
                workspace,
                data_dir
            ),
            "built-in"
        );
        assert_eq!(
            source_label(
                "project",
                Path::new("/work/project/.claude/skills/p/SKILL.md"),
                workspace,
                data_dir
            ),
            "project"
        );
        assert_eq!(
            source_label(
                "mine",
                Path::new("/home/u/.claude/skills/m/SKILL.md"),
                workspace,
                data_dir
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
                    id: "codegraph".to_owned(),
                    name: "codegraph".to_owned(),
                    enabled: true,
                    state: McpState::User,
                    context_tokens: 1234,
                    last_used: None,
                    failure: None,
                },
                McpRow {
                    id: "gh".to_owned(),
                    name: "gh".to_owned(),
                    enabled: false,
                    state: McpState::Off,
                    context_tokens: 0,
                    last_used: None,
                    failure: None,
                },
                McpRow {
                    id: "search".to_owned(),
                    name: "search".to_owned(),
                    enabled: true,
                    state: McpState::OnDemand,
                    context_tokens: 80,
                    last_used: Some(five_days),
                    failure: None,
                },
                McpRow {
                    id: "pdfmcp".to_owned(),
                    name: "pdfmcp".to_owned(),
                    enabled: true,
                    state: McpState::UserOnly,
                    context_tokens: 0,
                    last_used: None,
                    failure: None,
                },
            ],
            tools: Vec::new(),
        }
    }

    #[test]
    fn installed_sections_are_scope_specific() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let model = installed_model(now);

        let plugins = installed_entries(Scope::Plugins, &model, false, now);
        let titles: Vec<&str> = plugins.iter().map(|(entry, _)| entry.title.as_str()).collect();
        assert_eq!(titles[0], "Not used recently");
        assert_eq!(titles[2], "User");
        assert!(titles[1].contains("old") && titles[1].ends_with("never used"));
        assert!(titles[3].contains("dsb") && titles[3].contains("1 skill \u{b7} 11 uses"));
        assert!(titles.last().unwrap().trim_end().ends_with("\u{2192} Show disabled (1)"));
        assert!(
            !titles.iter().any(|title| title.contains("dcl") || title.contains("MCP")),
            "plugins must not list skills or MCP: {titles:?}"
        );

        let plugins = installed_entries(Scope::Plugins, &model, true, now);
        let titles: Vec<&str> = plugins.iter().map(|(entry, _)| entry.title.as_str()).collect();
        let hide = titles
            .iter()
            .position(|title| title.trim_end().ends_with("\u{2193} Hide disabled (1)"))
            .expect("hide row");
        assert_eq!(titles[hide + 1], "Disabled plugins");
        assert!(titles[hide + 2].contains("off-plugin"));
        assert_eq!(plugins.len(), hide + 3);

        let skills = installed_entries(Scope::Skills, &model, false, now);
        let titles: Vec<&str> = skills.iter().map(|(entry, _)| entry.title.as_str()).collect();
        assert_eq!(titles[0], "Skills");
        assert!(titles[1].contains("dcl") && titles[1].ends_with("user \u{b7} ~39 tok \u{b7} never used"));
        assert!(titles[2].contains("pdf") && titles[2].contains("user-only \u{b7} user \u{b7} not listed"));
        assert!(titles.last().unwrap().trim_end().ends_with("\u{2192} Show disabled (1)"));
        assert!(
            !titles.iter().any(|title| title.contains("dsb") || title.contains("MCP")),
            "skills must not list plugins or MCP: {titles:?}"
        );

        let skills = installed_entries(Scope::Skills, &model, true, now);
        let titles: Vec<&str> = skills.iter().map(|(entry, _)| entry.title.as_str()).collect();
        let hide = titles
            .iter()
            .position(|title| title.trim_end().ends_with("\u{2193} Hide disabled (1)"))
            .expect("hide row");
        assert_eq!(titles[hide + 1], "Disabled skills");
        assert!(titles[hide + 2].contains("cu"));
        assert!(titles[hide + 2].contains("off \u{b7} user \u{b7} not listed"));
        assert_eq!(skills.len(), hide + 3);
    }

    #[test]
    fn installed_mcp_rows_show_state_and_tokens() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let model = installed_model(now);
        let rows = mcp_entries(&model, true);
        let title = |name: &str| {
            rows.iter()
                .find(|(_, target)| *target == Target::Mcp(name.to_owned()))
                .map(|(entry, _)| entry.title.clone())
                .unwrap_or_else(|| panic!("no MCP row for {name}"))
        };
        assert!(
            title("codegraph").ends_with("user \u{b7} ~1234 tok"),
            "{}",
            title("codegraph")
        );
        assert!(
            title("search").ends_with("on-demand \u{b7} ~80 tok"),
            "{}",
            title("search")
        );
        assert!(title("pdfmcp").ends_with("user-only"), "{}", title("pdfmcp"));
        let gh = title("gh");
        assert!(gh.starts_with('\u{25cb}') && gh.ends_with("off"), "{gh}");
    }

    #[test]
    fn mcp_failures_travel_with_the_row() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let mut model = installed_model(now);
        model.mcp[0].failure = Some("spawn failed: missing binary".to_owned());
        let rows = mcp_entries(&model, false);
        let row = rows
            .iter()
            .find(|(_, target)| *target == Target::Mcp("codegraph".to_owned()))
            .expect("codegraph row");
        assert!(row.0.title.contains("failed"), "{}", row.0.title);
        assert!(!rows.iter().any(|(entry, _)| entry.title.contains("missing binary")));
        assert!(!stats_rows(Scope::Skills, &model, now, false)
            .iter()
            .any(|line| line.contains("MCP")));
    }

    #[test]
    fn space_cycles_between_off_and_user() {
        assert_eq!(space_state(SkillState::Off), SkillState::User);
        assert_eq!(space_state(SkillState::User), SkillState::Off);
        assert_eq!(space_state(SkillState::NameOnly), SkillState::Off);
        assert_eq!(space_state(SkillState::UserOnly), SkillState::Off);
        assert_eq!(space_state(SkillState::Always), SkillState::Off);
        assert_eq!(space_mcp_state(McpState::Off), McpState::User);
        assert_eq!(space_mcp_state(McpState::User), McpState::Off);
        assert_eq!(space_mcp_state(McpState::OnDemand), McpState::Off);
        assert_eq!(space_mcp_state(McpState::UserOnly), McpState::Off);
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
            tools: Vec::new(),
        };

        let rows = stats_rows(Scope::Skills, &model, now, false);
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
            source_label(
                full,
                Path::new(""),
                Path::new("/work/project"),
                Path::new("/data/dcl")
            ),
            "synced"
        );

        let rows = installed_entries(Scope::Skills, &model, false, now);
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
            tools: Vec::new(),
        };
        let expected = ["alpha", "build", "claude-api", "skill-creator"];
        let known = |name: &str| expected.contains(&name);

        let stats: Vec<String> = stats_rows(Scope::Skills, &model, now, false)
            .iter()
            .filter_map(|row| row.split_whitespace().next().filter(|name| known(name)))
            .map(|name| name.to_owned())
            .collect();
        assert_eq!(stats, expected, "stats order");

        let installed: Vec<String> = installed_entries(Scope::Skills, &model, false, now)
            .iter()
            .filter_map(|(entry, _)| entry.title.split_whitespace().nth(1).filter(|name| known(name)))
            .map(|name| name.to_owned())
            .collect();
        assert_eq!(installed, expected, "installed order");
    }
}
