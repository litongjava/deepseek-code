//! The plugins panel shared by `/plugins` and `/skill-doctor`: Installed,
//! Errors and Stats tabs switched with ←/→. This round renders the Stats tab;
//! the other two show a placeholder until their interactions arrive.
use crate::session::Session;
use crate::terminal::input::TerminalInput;
use crate::terminal::picker::details;
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
const PLACEHOLDER: &str = "Coming in the next round.";

pub(crate) struct SkillRow {
    pub name: String,
    pub source: String,
    // The Installed tab shows this column in the next round.
    #[allow(dead_code)]
    pub state: SkillState,
    pub listing_tokens: u64,
    pub usage: Usage,
}

// The Installed tab uses these fields in the next round.
#[allow(dead_code)]
pub(crate) struct PluginRow {
    pub id: String,
    pub enabled: bool,
    pub skills: usize,
    pub uses: u64,
    pub last_used: Option<DateTime<Utc>>,
}

// The Installed tab uses these fields in the next round.
#[allow(dead_code)]
pub(crate) struct McpRow {
    pub name: String,
    pub enabled: bool,
    pub last_used: Option<DateTime<Utc>>,
}

pub(crate) struct Model {
    pub skills: Vec<SkillRow>,
    pub plugins: Vec<PluginRow>,
    // The Installed tab lists MCP servers in the next round.
    #[allow(dead_code)]
    pub mcp: Vec<McpRow>,
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

/// Where a skill comes from: a plugin ID, the CLI's embedded skills, the
/// workspace's `.claude` directory, or the user scope.
fn source_label(name: &str, path: &Path, workspace: &Path) -> String {
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
    skills.sort_by(|a, b| a.name.cmp(&b.name));

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

/// The Stats tab: what each skill costs and how often it ran, and plugins that
/// have gone unused. Plain rows, columns padded with spaces.
pub(crate) fn stats_rows(model: &Model, now: DateTime<Utc>, notice: bool) -> Vec<String> {
    let mut skills: Vec<&SkillRow> = model.skills.iter().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    let name_width = skills
        .iter()
        .map(|skill| skill.name.len())
        .max()
        .unwrap_or(0)
        .max("skill".len());
    let source_width = skills
        .iter()
        .map(|skill| skill.source.len())
        .max()
        .unwrap_or(0)
        .max("source".len());

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
            skill.name,
            skill.source,
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

pub(crate) async fn run(
    start: &str,
    paths: &RuntimePaths,
    session: &mut Session,
    _input: &mut TerminalInput,
) -> Result<bool> {
    let _screen = PluginsScreen::enter()?;
    let model = load(paths, session).await?;
    let mut tab = TABS
        .iter()
        .position(|tab| tab.eq_ignore_ascii_case(start))
        .unwrap_or(0);
    loop {
        let rows = match tab {
            2 => stats_rows(&model, Utc::now(), start == "skill-doctor"),
            _ => vec![PLACEHOLDER.to_owned()],
        };
        let step = match details(&tab_bar(tab), &rows, HINT, &[])? {
            Some('<') => -1,
            Some('>') => 1,
            _ => return Ok(false),
        };
        tab = (tab as isize + step).rem_euclid(TABS.len() as isize) as usize;
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
}
