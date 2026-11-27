//! Typed coordination records: no free-form metadata and no conversation text.
//!
//! Only identifiers, typed lifecycle/activity values, owner identity, native
//! path bytes and Git reference bytes are stored. Prompts, titles, argv,
//! command hashes and environments never enter this module or the registry.

use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// Coordination-specific failure categories.
#[derive(Debug, thiserror::Error)]
pub enum CoordinationError {
    #[error("coordination store is busy; retry later")]
    Busy,
    #[error("unsupported coordination schema version {0}")]
    UnsupportedSchema(u32),
    #[error("coordination store is corrupt or unreadable")]
    Corrupt,
    #[error("coordination database is missing")]
    MissingDatabase,
    #[error("coordination domain marker does not match the database")]
    DomainMismatch,
    #[error("this platform has no coordination owner adapter")]
    UnsupportedPlatform,
    #[error("schema upgrade requires every earlier owner to drain first")]
    SchemaUpgradeRequiresDrain,
    #[error("nested coordination request")]
    NestedRequest,
    #[error("local coordination is disabled")]
    Disabled,
    #[error("invalid coordination state transition")]
    InvalidTransition,
    #[error("coordination cleanup could not be verified")]
    CleanupUnverified,
    #[error("the coordination data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("workspace metadata cannot be resolved")]
    WorkspaceUnavailable,
    #[error("invalid coordination record: {0}")]
    InvalidRecord(String),
    #[error("coordination store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Bounded on purpose: provider SQL text is not surfaced or persisted.
    #[error("coordination store database error")]
    Sql(#[source] rusqlite::Error),
}

/// Coordination result. `anyhow::Result` stays the ordinary product Result.
pub type CoordResult<T> = std::result::Result<T, CoordinationError>;

impl From<rusqlite::Error> for CoordinationError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Self::Busy
            }
            // Definite storage corruption, so the later error category is reachable.
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
                ) =>
            {
                Self::Corrupt
            }
            _ => Self::Sql(error),
        }
    }
}

/// Native path bytes. `unix_bytes` is the raw `OsStr` byte sequence;
/// `windows_utf16le` is the little-endian code-unit sequence. Bytes are stored
/// as BLOBs and appear only as base64 in JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativePath {
    pub encoding: String,
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

/// Only used by the Unix codec; Windows builds keep the label for validation.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const UNIX_BYTES: &str = "unix_bytes";
/// Only used by the Windows codec; Linux builds keep the label for validation.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const WINDOWS_UTF16LE: &str = "windows_utf16le";

/// Base64 for serialized native bytes, so JSON stays lossless and textual.
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(D::Error::custom)
    }
}

impl NativePath {
    pub fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self {
                encoding: UNIX_BYTES.into(),
                bytes: path.as_os_str().as_bytes().to_vec(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let units: Vec<u16> = path.as_os_str().encode_wide().collect();
            Self {
                encoding: WINDOWS_UTF16LE.into(),
                bytes: units.iter().flat_map(|unit| unit.to_le_bytes()).collect(),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {
                encoding: UNIX_BYTES.into(),
                bytes: path.to_string_lossy().into_owned().into_bytes(),
            }
        }
    }

    /// Structural validation for the persistence and decode boundaries.
    ///
    /// Enforces a bounded length, this platform's encoding label and the
    /// platform's structure. Rejected values are never echoed in errors.
    pub(crate) fn validate_native(&self) -> CoordResult<()> {
        if !(1..=8192).contains(&self.bytes.len()) {
            return Err(CoordinationError::InvalidRecord(
                "workspace path must be 1..=8192 native bytes".into(),
            ));
        }
        #[cfg(unix)]
        {
            if self.encoding != UNIX_BYTES {
                return Err(CoordinationError::InvalidRecord(
                    "workspace path encoding is not this platform's encoding".into(),
                ));
            }
            if self.bytes.contains(&0) {
                return Err(CoordinationError::InvalidRecord(
                    "workspace path contains a NUL byte".into(),
                ));
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            if self.encoding != WINDOWS_UTF16LE {
                return Err(CoordinationError::InvalidRecord(
                    "workspace path encoding is not this platform's encoding".into(),
                ));
            }
            if self.bytes.len() % 2 != 0 {
                return Err(CoordinationError::InvalidRecord(
                    "workspace path has an odd UTF-16LE byte length".into(),
                ));
            }
            if self.bytes.chunks_exact(2).any(|pair| pair == [0, 0]) {
                return Err(CoordinationError::InvalidRecord(
                    "workspace path contains a zero code unit".into(),
                ));
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(CoordinationError::UnsupportedPlatform)
        }
    }

    /// Decode only this platform's encoding; never decode lossily.
    pub fn to_path(&self) -> CoordResult<PathBuf> {
        self.validate_native()?;
        #[cfg(unix)]
        {
            use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
            Ok(PathBuf::from(OsStr::from_bytes(&self.bytes)))
        }
        #[cfg(windows)]
        {
            use std::{ffi::OsString, os::windows::ffi::OsStringExt};
            // `from_wide` keeps unpaired code units lossless.
            let units: Vec<u16> = self
                .bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            Ok(PathBuf::from(OsString::from_wide(&units)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(CoordinationError::UnsupportedPlatform)
        }
    }
}

impl fmt::Display for NativePath {
    /// Paths are not printed here; display intentionally stays opaque.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<native path {} bytes>", self.bytes.len())
    }
}

/// Declares a closed text enum with one snake_case spelling per value.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub(crate) fn from_text(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

text_enum!(ParticipantKind {
    Cli => "cli",
    BackgroundAgent => "background_agent",
    WorkflowAgent => "workflow_agent",
    ExternalAgent => "external_agent",
    CommandRunner => "command_runner",
});

text_enum!(ParticipantRole {
    Frontend => "frontend",
    Executor => "executor",
    Runner => "runner",
});

text_enum!(Activity {
    Idle => "idle",
    Active => "active",
    WaitingResource => "waiting_resource",
    HeavyCommand => "heavy_command",
    Stopping => "stopping",
});

text_enum!(Lifecycle {
    Open => "open",
    Closed => "closed",
    Gone => "gone",
});

/// Owner incarnation identity. `generation` fences every later mutation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerRecord {
    pub platform: String,
    pub pid: u32,
    pub boot_id: String,
    pub creation_id: String,
    pub generation: String,
}

/// One registry row. Every field is an allowlisted identifier, enum or byte
/// sequence; there is deliberately no free-form metadata map.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registration {
    pub instance_id: String,
    pub data_namespace_id: String,
    pub session_id: Option<String>,
    pub kind: ParticipantKind,
    pub role: ParticipantRole,
    pub parent_instance_id: Option<String>,
    pub worker_id: Option<String>,
    pub job_id: Option<String>,
    pub owner: OwnerRecord,
    pub workspace: NativePath,
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub branch_ref: Option<Vec<u8>>,
    pub activity: Activity,
    pub activity_since_ms: i64,
    pub started_ms: i64,
    pub heartbeat_ms: i64,
    pub last_git_observed_ms: Option<i64>,
    pub lifecycle: Lifecycle,
}

pub(crate) fn is_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

/// Longest branch reference accepted from Git or a caller.
pub(super) const MAX_BRANCH_REF_BYTES: usize = 1024;

/// Branch reference bytes: nonempty, bounded, and free of NUL and LF.
///
/// The Git producer and every persistence boundary share this rule, so a
/// reference that could not be stored safely is dropped instead of written.
pub(super) fn valid_branch_ref(reference: &[u8]) -> bool {
    !reference.is_empty()
        && reference.len() <= MAX_BRANCH_REF_BYTES
        && !reference.contains(&0)
        && !reference.contains(&b'\n')
}

/// Whether a validated native path names an absolute location.
///
/// A workspace is always resolved before it is stored, so a relative path is a
/// caller error rather than a second way to name the same directory.
fn absolute_native(path: &NativePath) -> bool {
    #[cfg(unix)]
    {
        path.bytes.first() == Some(&b'/')
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<u16> = path
            .bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&units)).is_absolute()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        false
    }
}

/// The workspace fields shared by registration, refresh and row decoding.
///
/// One implementation means a value that `insert` accepts cannot make a later
/// snapshot decode fail: `update_workspace` calls this before writing anything.
pub(super) fn validate_workspace_fields(
    path: &NativePath,
    repo_id: Option<&str>,
    worktree_id: Option<&str>,
    branch_ref: Option<&[u8]>,
    observed_ms: Option<i64>,
) -> CoordResult<()> {
    path.validate_native()?;
    if !absolute_native(path) {
        return Err(CoordinationError::InvalidRecord(
            "workspace path must be absolute".into(),
        ));
    }
    for (field, value) in [("repo_id", repo_id), ("worktree_id", worktree_id)] {
        if let Some(value) = value {
            if !is_hex64(value) {
                return Err(CoordinationError::InvalidRecord(format!(
                    "{field} must be 64 lowercase hex characters"
                )));
            }
        }
    }
    if let Some(reference) = branch_ref {
        if !valid_branch_ref(reference) {
            return Err(CoordinationError::InvalidRecord(format!(
                "branch_ref must be 1..={MAX_BRANCH_REF_BYTES} bytes without NUL or newline"
            )));
        }
    }
    if observed_ms.is_some_and(|value| value < 0) {
        return Err(CoordinationError::InvalidRecord(
            "last_git_observed_ms must not be negative".into(),
        ));
    }
    Ok(())
}

pub(crate) fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn bounded_token(field: &str, value: &str, max: usize) -> CoordResult<()> {
    let ok = !value.is_empty()
        && value.len() <= max
        && !value.chars().any(|character| character.is_control());
    if ok {
        Ok(())
    } else {
        Err(CoordinationError::InvalidRecord(format!(
            "{field} must be 1..={max} printable bytes"
        )))
    }
}

impl OwnerRecord {
    pub(crate) fn validate(&self) -> CoordResult<()> {
        if self.platform != "linux" {
            return Err(CoordinationError::UnsupportedPlatform);
        }
        if self.pid == 0 {
            return Err(CoordinationError::InvalidRecord(
                "owner pid must be positive".into(),
            ));
        }
        if !is_uuid(&self.boot_id) {
            return Err(CoordinationError::InvalidRecord(
                "owner boot_id must be a canonical UUID".into(),
            ));
        }
        bounded_token("owner creation_id", &self.creation_id, 128)?;
        if !is_uuid(&self.generation) {
            return Err(CoordinationError::InvalidRecord(
                "owner generation must be a canonical UUID".into(),
            ));
        }
        Ok(())
    }
}

impl Registration {
    /// Reject records that could not be addressed or interpreted later.
    pub(crate) fn validate(&self) -> CoordResult<()> {
        if !is_uuid(&self.instance_id) {
            return Err(CoordinationError::InvalidRecord(
                "instance_id must be a canonical UUID".into(),
            ));
        }
        if !is_hex64(&self.data_namespace_id) {
            return Err(CoordinationError::InvalidRecord(
                "data_namespace_id must be 64 lowercase hex characters".into(),
            ));
        }
        for (field, value) in [
            ("session_id", self.session_id.as_deref()),
            ("parent_instance_id", self.parent_instance_id.as_deref()),
        ] {
            if let Some(value) = value {
                if !is_uuid(value) {
                    return Err(CoordinationError::InvalidRecord(format!(
                        "{field} must be a canonical UUID"
                    )));
                }
            }
        }
        for (field, value) in [
            ("worker_id", self.worker_id.as_deref()),
            ("job_id", self.job_id.as_deref()),
        ] {
            if let Some(value) = value {
                bounded_token(field, value, 128)?;
            }
        }
        // Structural path rules live in one place, shared with `to_path`, with
        // the refresh path and with row decoding.
        validate_workspace_fields(
            &self.workspace,
            self.repo_id.as_deref(),
            self.worktree_id.as_deref(),
            self.branch_ref.as_deref(),
            self.last_git_observed_ms,
        )?;
        for (field, value) in [
            ("activity_since_ms", self.activity_since_ms),
            ("started_ms", self.started_ms),
            ("heartbeat_ms", self.heartbeat_ms),
        ] {
            if value < 0 {
                return Err(CoordinationError::InvalidRecord(format!(
                    "{field} must not be negative"
                )));
            }
        }
        self.owner.validate()
    }
}
