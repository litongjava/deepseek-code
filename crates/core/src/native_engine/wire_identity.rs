//! Optional request shaping for Responses-protocol endpoints.
//!
//! Two layers, both opt-in for a session:
//!
//! - `compat` adds the standard fields a current Responses client sends, which
//!   dcl has so far left out.
//! - `codex` adds the same fields plus the client identity a Codex client
//!   sends, for endpoints that expect that shape.
//!
//! Only values this client can produce honestly are added. State the server
//! issues (`x-codex-turn-state`), private beta flags and server-side replay
//! semantics are never invented, and no account credential is involved.
use super::{provider_manager::ResolvedProvider, request_identity::RequestIds};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use uuid::Uuid;

const CODEX_ORIGINATOR: &str = "codex-tui";
const CODEX_VERSION: &str = "0.166.0";
/// Codex reports the terminal it runs in; this one is present on every platform.
const CODEX_TERMINAL: &str = "xterm-256color";

/// The request profile a session selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WireIdentity {
    /// Send only what dcl has always sent.
    #[default]
    Default,
    /// Add the standard Responses fields, without any client identity.
    Compat,
    /// Add the Responses fields and the Codex client identity.
    Codex,
}

impl WireIdentity {
    /// Accepts the names a user may type; `default`, `off` and `dcl` clear it.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" | "off" | "dcl" => Some(Self::Default),
            "compat" | "responses" => Some(Self::Compat),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// A stored value this build does not know falls back to the default.
    pub fn from_setting(value: Option<&str>) -> Self {
        value.and_then(Self::parse).unwrap_or_default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Compat => "compat",
            Self::Codex => "codex",
        }
    }

    fn includes_fields(self) -> bool {
        !matches!(self, Self::Default)
    }
}

/// The profile in force for this provider. Both layers describe the Responses
/// wire format, so a provider on another protocol keeps its usual request.
pub(super) fn active(provider: &ResolvedProvider) -> WireIdentity {
    let identity = provider.model.wire_identity;
    if !identity.includes_fields() || !is_responses(provider) {
        return WireIdentity::Default;
    }
    identity
}

fn is_responses(provider: &ResolvedProvider) -> bool {
    matches!(
        provider.model.api.as_deref(),
        Some("openai-responses") | Some("openai-codex-responses")
    )
}

/// Replace the client identity and drop the headers that name this client.
pub(super) fn apply_headers(
    headers: &mut HeaderMap,
    provider: &ResolvedProvider,
    ids: &RequestIds,
) {
    if active(provider) != WireIdentity::Codex {
        return;
    }
    let mut set = |name: &'static str, value: String| {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    };
    set("user-agent", user_agent());
    set("originator", CODEX_ORIGINATOR.to_owned());
    if let Some(session) = &ids.session {
        set("session-id", session.clone());
        set("thread-id", session.clone());
        set("x-codex-window-id", window_id(session));
        if let Some(turn) = &ids.turn {
            set("x-codex-turn-metadata", turn_metadata(session, turn));
        }
    }
    // A request must not carry two identities at once.
    headers.remove("x-app");
    for name in [
        "x-dcl-runtime",
        "x-dcl-os",
        "x-dcl-arch",
        "x-dcl-session-id",
        "x-parent-session-id",
        "x-dcl-turn-id",
    ] {
        headers.remove(name);
    }
}

/// Add the fields of the selected profile to a Responses request body.
pub(super) fn apply_to_responses_body(
    body: &mut Value,
    provider: &ResolvedProvider,
    ids: &RequestIds,
) {
    let identity = active(provider);
    if !identity.includes_fields() {
        return;
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };
    // A tool-choice hint without tools is rejected by some endpoints; send the
    // pair only where it has a meaning.
    let has_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        object.insert("tool_choice".into(), json!("auto"));
        // This client executes independent tool calls together, so it declares
        // the capability it actually has.
        object.insert("parallel_tool_calls".into(), json!(true));
    }
    if let Some(session) = &ids.session {
        object.insert("prompt_cache_key".into(), json!(session));
    }
    match object.get_mut("text").and_then(Value::as_object_mut) {
        Some(text) => {
            text.insert("verbosity".into(), json!("low"));
        }
        None => {
            object.insert("text".into(), json!({"verbosity":"low"}));
        }
    }
    // Context replay belongs to the reported reasoning block, which exists only
    // when an effort was chosen.
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.insert("context".into(), json!("all_turns"));
    }
    if identity == WireIdentity::Codex {
        if let (Some(session), Some(turn)) = (&ids.session, &ids.turn) {
            object.insert("client_metadata".into(), client_metadata(session, turn));
        }
    }
}

fn client_metadata(session: &str, turn: &str) -> Value {
    json!({
        "x-codex-installation-id": installation_id(),
        "turn_id": turn,
        "root_turn_id": turn,
        "x-codex-turn-metadata": turn_metadata(session, turn),
        "x-codex-window-id": window_id(session),
        "thread_id": session,
        "session_id": session
    })
}

/// The per-turn metadata a Codex client repeats in the header and the body.
///
/// It carries identifiers only. Local paths, git remotes and sandbox settings
/// are deliberately absent: they describe this machine, not the request.
fn turn_metadata(session: &str, turn: &str) -> String {
    json!({
        "installation_id": installation_id(),
        "session_id": session,
        "thread_id": session,
        "turn_id": turn,
        "root_turn_id": turn,
        "window_id": window_id(session),
        "window_number": 0,
        "context_window_id": context_window_id(session),
        "request_kind": "turn"
    })
    .to_string()
}

fn user_agent() -> String {
    format!(
        "{CODEX_ORIGINATOR}/{CODEX_VERSION} ({}; {}) {CODEX_TERMINAL} ({CODEX_ORIGINATOR}; {CODEX_VERSION})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Stable across restarts without a state file, and meaningless elsewhere.
fn installation_id() -> String {
    static ID: LazyLock<String> = LazyLock::new(|| {
        let seed = ["COMPUTERNAME", "USERNAME", "USER", "HOME"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .collect::<Vec<_>>()
            .join("|");
        stable_id(&seed)
    });
    ID.clone()
}

/// A stable identifier in UUID shape, derived from a seed rather than stored.
fn stable_id(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Name-based UUID layout: version 5, RFC 4122 variant.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

fn window_id(session: &str) -> String {
    format!("{session}:0")
}

fn context_window_id(session: &str) -> String {
    stable_id(session)
}

#[cfg(test)]
#[path = "wire_identity_tests.rs"]
mod tests;
