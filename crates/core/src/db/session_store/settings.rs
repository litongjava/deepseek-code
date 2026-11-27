//! Session-local keys and their persisted value types. The shard supplies the session identity.
use super::{state, SessionRead, SessionWrite};
use crate::permissions::rules::PermissionAction;
use crate::super_agent::Role;
use anyhow::{ensure, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::HashMap,
    marker::PhantomData,
    path::{Path, PathBuf},
};

pub struct Setting<T> {
    pub(super) name: Cow<'static, str>,
    pub(super) legacy: Option<String>,
    value: PhantomData<fn() -> T>,
}
impl<T> Setting<T> {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            legacy: None,
            value: PhantomData,
        }
    }
    pub(crate) fn memory_receipt(key: String) -> Self {
        Self {
            name: Cow::Owned(format!("memory.{key}")),
            legacy: None,
            value: PhantomData,
        }
    }
    pub(crate) fn write(&self, c: &rusqlite::Connection, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        state::set_local(c, &self.name, &serde_json::to_string(value)?)
    }
    fn workspace(name: &str, workspace: &Path) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
        Self {
            name: Cow::Owned(format!("{name}.{digest:x}")),
            legacy: Some(if name.ends_with(".rejected") {
                format!("{}:rejected", workspace.display())
            } else {
                workspace.display().to_string()
            }),
            value: PhantomData,
        }
    }
    pub(crate) fn read(&self, c: &rusqlite::Connection) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        state::local(c, &self.name)?
            .map(|raw| Ok(serde_json::from_str(&raw)?))
            .transpose()
    }
}

impl SessionRead<'_> {
    pub fn setting<T: DeserializeOwned>(&self, key: Setting<T>) -> Result<Option<T>> {
        key.read(self.0)
    }
}
impl SessionWrite<'_> {
    pub fn setting<T: DeserializeOwned>(&self, key: Setting<T>) -> Result<Option<T>> {
        key.read(self.0)
    }
    pub fn set_setting<T: Serialize>(&self, key: Setting<T>, value: &T) -> Result<()> {
        key.write(self.0, value)
    }
    pub fn clear_setting<T>(&self, key: Setting<T>) -> Result<()> {
        self.0.execute(
            "DELETE FROM peripheral_settings WHERE key=?1",
            [key.name.as_ref()],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Off,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8")]
pub struct CompactPercent(u8);
impl TryFrom<u8> for CompactPercent {
    type Error = anyhow::Error;
    fn try_from(percent: u8) -> Result<Self> {
        ensure!(
            (10..=95).contains(&percent),
            "Autocompact threshold must be between 10 and 95 percent"
        );
        Ok(Self(percent))
    }
}
impl From<CompactPercent> for u8 {
    fn from(value: CompactPercent) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliGoal {
    pub text: String,
    pub completed: bool,
    pub last_check: Option<String>,
}

/// Compacts at 80% of the window by default, leaving a fifth for the next
/// response and for summarization, e.g. 160k of a 200k window.
pub fn default_autocompact_tokens(window: u64) -> Option<u64> {
    Some(window * 4 / 5).filter(|tokens| *tokens > 0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub effort: Option<Effort>,
    pub autocompact_percent: Option<CompactPercent>,
    pub autocompact_off: bool,
    pub goal: Option<CliGoal>,
}

impl Preferences {
    /// Estimated history tokens at which the conversation is automatically compacted.
    pub fn autocompact_tokens(&self, window: u64) -> Option<u64> {
        if window == 0 {
            return None;
        }
        match self.autocompact_percent {
            Some(percent) => Some(window * u64::from(u8::from(percent)) / 100),
            None if self.autocompact_off => None,
            None => default_autocompact_tokens(window),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    pub theme: String,
    pub color: String,
    pub focus: bool,
    pub output_style: String,
    pub statusline: Option<String>,
    pub tui: String,
    pub diff_panel: bool,
    pub diff_staged: bool,
}
impl Default for Display {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            color: "default".into(),
            focus: false,
            output_style: "default".into(),
            statusline: None,
            tui: "default".into(),
            diff_panel: false,
            diff_staged: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Editor {
    pub port: u16,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Microcompact {
    pub through: usize,
    #[serde(default)]
    pub cleared_results: usize,
    pub boundary: String,
}

impl Microcompact {
    pub fn for_boundary(boundary: impl Into<String>) -> Self {
        Self {
            through: 0,
            cleared_results: 0,
            boundary: boundary.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnteredWorktree {
    pub created: bool,
}

/// Keep only the CLI's accounting allowlist. Prompt and transport fields are dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_boundary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<serde_json::Value>,
}

pub(crate) const MODEL_REQUEST_OPTIONS: Setting<
    crate::native_engine::engine_core::SessionRequestOptions,
> = Setting::new("model.request_options");
pub const PREFERENCES: Setting<Preferences> = Setting::new("cli.session");
pub const PERMISSIONS: Setting<HashMap<String, PermissionAction>> = Setting::new("cli.permissions");
pub const DISPLAY: Setting<Display> = Setting::new("cli.display");
pub const EDITOR: Setting<Editor> = Setting::new("cli.ide");
pub const LIVE_REQUESTS: Setting<Vec<LiveRequest>> = Setting::new("cli.live-requests");
pub const MICROCOMPACT: Setting<Microcompact> = Setting::new("microcompact");
pub const SESSION_START_CONTEXT: Setting<String> = Setting::new("session-start-context");
pub const SEEN_INSTRUCTIONS: Setting<Vec<(PathBuf, String)>> =
    Setting::new("lifecycle-instructions");
pub const INSTRUCTIONS_BOUNDARY: Setting<String> = Setting::new("lifecycle-instructions.boundary");
pub const ROLE: Setting<Role> = Setting::new("super-agent.role");
/// Agent-tool fork provenance used by the engine's nested-fork check.
pub const AGENT_FORK_PARENT: Setting<String> = Setting::new("fork-of");
pub const AGENT_DESCRIPTION: Setting<String> = Setting::new("agent-description");
pub const CHILD_PROGRESS: Setting<serde_json::Map<String, serde_json::Value>> =
    Setting::new("delegation.progress");
pub const ENTERED_WORKTREE: Setting<EnteredWorktree> = Setting::new("entered-worktree");

pub fn accepted_configuration(workspace: &Path) -> Setting<Vec<Option<String>>> {
    Setting::workspace("lifecycle-config.accepted", workspace)
}
pub fn rejected_configuration(workspace: &Path) -> Setting<(Vec<Option<String>>, String)> {
    Setting::workspace("lifecycle-config.rejected", workspace)
}

pub(crate) const MEMORY_AGENT: Setting<String> = Setting::new("memory.agent-definition");
pub(crate) const MEMORY_SCOPE: Setting<(Option<String>, String)> = Setting::new("memory.scope");
pub(crate) const MEMORY_OWNER: Setting<(Option<String>, String)> =
    Setting::new("memory.private-owner");
