//! Per-server MCP states stored in `<data dir>/mcp-states.json`.
//!
//! A server without an entry gets [`McpState::User`], so an existing
//! installation keeps today's behavior: every enabled server's tools are part
//! of every request. The stored `off` state is informational; the existing
//! `enabled` switch of the server configuration still decides whether a server
//! runs.
use super::tool_executor::normalize_name;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const FILE_NAME: &str = "mcp-states.json";
const TEMP_NAME: &str = "mcp-states.json.tmp";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpState {
    Off,
    #[default]
    User,
    OnDemand,
    UserOnly,
}

impl McpState {
    pub fn label(self) -> &'static str {
        match self {
            McpState::Off => "off",
            McpState::User => "user",
            McpState::OnDemand => "on-demand",
            McpState::UserOnly => "user-only",
        }
    }
}

impl std::str::FromStr for McpState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(McpState::Off),
            "user" => Ok(McpState::User),
            "on-demand" => Ok(McpState::OnDemand),
            "user-only" => Ok(McpState::UserOnly),
            other => Err(anyhow::anyhow!(
                "Unknown MCP server state {other:?}; expected one of: off, user, on-demand, user-only"
            )),
        }
    }
}

/// On-disk shape: `{"servers": {"<server id>": "<label>"}}`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct McpStatesFile {
    #[serde(default)]
    servers: BTreeMap<String, String>,
}

/// Read the state of every server. Missing, unreadable or invalid files mean
/// that every server stays at its default; an unknown label only skips its own
/// entry.
pub fn load(data_dir: &Path) -> BTreeMap<String, McpState> {
    let path = data_dir.join(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Ignoring unreadable MCP server states");
            return BTreeMap::new();
        }
    };
    let file: McpStatesFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Ignoring invalid MCP server states");
            return BTreeMap::new();
        }
    };
    let mut states = BTreeMap::new();
    for (name, label) in file.servers {
        match label.parse::<McpState>() {
            Ok(state) => {
                states.insert(name, state);
            }
            Err(error) => {
                tracing::warn!(server = %name, %error, "Skipping unknown MCP server state")
            }
        }
    }
    states
}

/// Store one server's state, keeping the other entries. The file is replaced
/// atomically so a concurrent reader never sees a partial document.
pub fn set(data_dir: &Path, server: &str, state: McpState) -> Result<()> {
    let mut states = load(data_dir);
    states.insert(server.to_owned(), state);
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("Creating {}", data_dir.display()))?;
    let file = McpStatesFile {
        servers: states
            .into_iter()
            .map(|(name, state)| (name, state.label().to_owned()))
            .collect(),
    };
    let path = data_dir.join(FILE_NAME);
    let temp = data_dir.join(TEMP_NAME);
    let mut text = serde_json::to_string_pretty(&file)?;
    text.push('\n');
    std::fs::write(&temp, text).with_context(|| format!("Writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

/// Whether two names denote the same server. Tool names carry the normalized
/// form of the configured server id, so the comparison normalizes both sides.
fn same_server(a: &str, b: &str) -> bool {
    normalize_name(a) == normalize_name(b)
}

/// The stored state, or [`McpState::User`] for a server the file does not
/// mention.
pub fn effective(states: &BTreeMap<String, McpState>, server: &str) -> McpState {
    states
        .iter()
        .find(|(name, _)| same_server(name, server))
        .map(|(_, state)| *state)
        .unwrap_or(McpState::User)
}

/// The state shown to people: `off` when the server is disabled (the
/// `enabled` switch is the only authority for that), otherwise the stored
/// state, with a stored `off` read as `user`.
pub fn shown(states: &BTreeMap<String, McpState>, server_id: &str, enabled: bool) -> McpState {
    if !enabled {
        return McpState::Off;
    }
    match effective(states, server_id) {
        McpState::Off => McpState::User,
        state => state,
    }
}

/// The server id part of `mcp__<server>__<tool>`, or None for other names.
pub fn server_of(tool: &str) -> Option<&str> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, name) = rest.split_once("__")?;
    (!server.is_empty() && !name.is_empty()).then_some(server)
}

/// The first non-empty line of a description, trimmed and cut to 120 characters.
/// Shared by the on-demand prompt listing and the CLI's context estimate.
pub fn description_summary(description: &str) -> String {
    let line = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() > 120 {
        let mut summary: String = line.chars().take(119).collect();
        summary.push('…');
        summary
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_states_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "github", McpState::OnDemand).unwrap();
        set(dir.path(), "github", McpState::UserOnly).unwrap();
        set(dir.path(), "filesystem", McpState::Off).unwrap();

        let states = load(dir.path());
        assert_eq!(states.len(), 2);
        assert_eq!(states.get("github"), Some(&McpState::UserOnly));
        assert_eq!(states.get("filesystem"), Some(&McpState::Off));
        assert_eq!(effective(&states, "github"), McpState::UserOnly);
        assert_eq!(effective(&states, "filesystem"), McpState::Off);
        assert_eq!(effective(&states, "unknown"), McpState::User);
        assert_eq!(effective(&BTreeMap::new(), "github"), McpState::User);

        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"github\": \"user-only\""), "{text}");
        assert!(text.contains("\"servers\""), "{text}");
        assert!(!dir.path().join(TEMP_NAME).exists());
    }

    #[test]
    fn missing_or_invalid_file_means_every_server_is_user() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());

        std::fs::write(dir.path().join(FILE_NAME), "not json").unwrap();
        assert!(load(dir.path()).is_empty());

        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"servers":{"known":"on-demand","future":"brand-new"}}"#,
        )
        .unwrap();
        let states = load(dir.path());
        assert_eq!(effective(&states, "known"), McpState::OnDemand);
        assert_eq!(effective(&states, "future"), McpState::User);
    }

    #[test]
    fn labels_and_parsing_cover_the_four_states() {
        let expected = [
            (McpState::Off, "off"),
            (McpState::User, "user"),
            (McpState::OnDemand, "on-demand"),
            (McpState::UserOnly, "user-only"),
        ];
        for (state, label) in expected {
            assert_eq!(state.label(), label);
            assert_eq!(label.parse::<McpState>().unwrap(), state);
        }
        assert_eq!(McpState::default(), McpState::User);
        let error = "marketplace".parse::<McpState>().unwrap_err().to_string();
        for label in ["off", "user", "on-demand", "user-only"] {
            assert!(error.contains(label), "{error}");
        }
    }

    #[test]
    fn shown_reads_disabled_as_off_and_stored_off_as_user() {
        let states = BTreeMap::from([
            ("gh".to_owned(), McpState::OnDemand),
            ("old".to_owned(), McpState::Off),
        ]);
        assert_eq!(shown(&states, "gh", true), McpState::OnDemand);
        assert_eq!(shown(&states, "gh", false), McpState::Off);
        assert_eq!(shown(&states, "old", true), McpState::User);
        assert_eq!(shown(&states, "old", false), McpState::Off);
        assert_eq!(shown(&BTreeMap::new(), "gh", true), McpState::User);
        assert_eq!(shown(&BTreeMap::new(), "gh", false), McpState::Off);
    }

    #[test]
    fn server_names_match_after_normalization() {
        let states = BTreeMap::from([("my server".to_owned(), McpState::OnDemand)]);
        assert_eq!(effective(&states, "my_server"), McpState::OnDemand);
        assert_eq!(effective(&states, "my-server"), McpState::User);

        assert_eq!(server_of("mcp__gh__create_issue"), Some("gh"));
        assert_eq!(server_of("mcp__gh__a__b"), Some("gh"));
        assert_eq!(server_of("mcp__gh__"), None);
        assert_eq!(server_of("ListMcpResources"), None);
        assert_eq!(server_of("gh__create_issue"), None);
    }
}
