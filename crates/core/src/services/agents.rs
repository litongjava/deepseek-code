//! Definitions only: all execution remains in NativeEngine and its existing tool loop.
use super::ServiceContext;
use crate::db::DbManager;
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    #[serde(alias = "name")]
    pub id: String,
    pub description: String,
    pub system_prompt: String,
    /// None uses the host's tools and permissions; a list is a strict additional limit.
    pub tools: Option<Vec<String>>,
    /// When set, an agent that also has Bash may run only commands that
    /// `permissions::shell_read_only::is_read_only` accepts, and never a
    /// background or detached one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only_shell: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<super::agent_import::LegacyDefinition>,
}
pub fn canonical(id: &str) -> &str {
    match id {
        "claude-code-guide" => "guide",
        "verification" => "verify",
        "general-purpose" => "general",
        other => other,
    }
}
pub fn builtins() -> Vec<Definition> {
    let read_only_tools = || {
        [
            "Read",
            "Glob",
            "Grep",
            "ListDir",
            "Bash",
            "WebFetch",
            "WebSearch",
            "memory_search",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    };
    let guide_prompt =
        "Explain application and project workflows using available evidence.";
    vec![
        Definition {
            id: "guide".into(),
            description: guide_prompt.into(),
            system_prompt: guide_prompt.into(),
            tools: Some(
                [
                    "Read",
                    "ListDir",
                    "Grep",
                    "WebFetch",
                    "WebSearch",
                    "memory_search",
                    "AskUserQuestion",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
            ),
            read_only_shell: false,
            legacy: None,
        },
        Definition {
            id: "explore".into(),
            description: "Read-only search across the codebase: finds files, symbols and call sites and reports conclusions with file:line references.".into(),
            system_prompt: r#"You are a read-only code search agent. Your job is to answer the caller's question about the codebase and report what you found; you never change files.

- Search before you read: use Glob and Grep to locate candidates, then Read only the relevant parts of a file (offset and limit) instead of whole files.
- Shell commands must be read-only (git log, git blame, git show, ls, find, cat); anything else is refused.
- Match the depth the caller asked for: "quick" means the first good answer, "medium" checks the main alternatives, "very thorough" covers every location and naming convention.
- Report conclusions, not file dumps: name each relevant file with path:line and say in one line what is there. Quote code only when a few lines settle the answer.
- Say plainly what you could not find or did not check."#
                .into(),
            tools: Some(read_only_tools()),
            read_only_shell: true,
            legacy: None,
        },
        Definition {
            id: "plan".into(),
            description: "Read-only investigation that returns a step-by-step implementation plan with the files to change, risks and how to verify.".into(),
            system_prompt: r#"You are a read-only planning agent. Investigate the codebase for the caller's task and return an implementation plan; you never change files.

- Read the code the change touches and the tests around it before planning. Shell commands must be read-only.
- The plan names each file to change with path:line, says what changes there and in which order, and keeps each step small enough to verify on its own.
- Point out trade-offs between reasonable approaches and recommend one; list risks and the existing behavior that must not break.
- End with how to verify the change: the tests to add or run and any manual check.
- If something the plan depends on is unclear, state the assumption you made instead of asking."#
                .into(),
            tools: Some(read_only_tools()),
            read_only_shell: true,
            legacy: None,
        },
        Definition {
            id: "verify".into(),
            description: "Verify the requested behavior with relevant tests and evidence. Report failures and limitations accurately.".into(),
            system_prompt: "Verify the requested behavior with relevant tests and evidence. Report failures and limitations accurately.".into(),
            tools: None,
            read_only_shell: false,
            legacy: None,
        },
        Definition {
            id: "general".into(),
            description: "Complete the requested task using the shared tools, respecting project instructions and host permissions.".into(),
            system_prompt: "Complete the requested task using the shared tools, respecting project instructions and host permissions.".into(),
            tools: None,
            read_only_shell: false,
            legacy: None,
        },
    ]
}
impl Definition {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "Invalid agent ID"
        );
        ensure!(
            !self.system_prompt.trim().is_empty() && self.system_prompt.len() <= 32_000,
            "Invalid agent system prompt"
        );
        if let Some(tools) = &self.tools {
            ensure!(
                tools.len() <= 100 && tools.iter().all(|t| !t.trim().is_empty()),
                "Invalid agent tools"
            );
        }
        Ok(())
    }
}
pub fn list(context: &ServiceContext) -> Result<Vec<Definition>> {
    let mut definitions = builtins();
    definitions.extend(
        context
            .setting::<Vec<Definition>>("agent-definitions")?
            .unwrap_or_default(),
    );
    Ok(definitions)
}
pub fn resolve(context: &ServiceContext, id: &str) -> Result<Definition> {
    let definitions = list(context)?;
    if let Some(definition) = definitions.iter().find(|d| d.id == id) {
        return Ok(definition.clone());
    }
    let id = id.to_lowercase();
    definitions
        .into_iter()
        .find(|d| d.id == canonical(&id))
        .context("Unknown agent type; use agents list")
}
pub fn validate_custom(definition: &Definition) -> Result<()> {
    definition.validate()?;
    let normalized = definition.id.to_lowercase();
    ensure!(
        canonical(&normalized) == normalized && !builtins().iter().any(|d| d.id == normalized),
        "Cannot replace a built-in agent or alias"
    );
    Ok(())
}
pub fn install(context: &ServiceContext, definition: Definition) -> Result<()> {
    validate_custom(&definition)?;
    context.db.with_conn(|c| -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx.query_row("SELECT value FROM peripheral_settings WHERE key='agent-definitions'",[],|r|r.get(0)).optional()?;
        let mut definitions: Vec<Definition> = raw.map(|v|serde_json::from_str(&v)).transpose()?.unwrap_or_default();
        definitions.retain(|d|d.id!=definition.id); definitions.push(definition);
        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES('agent-definitions',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(&definitions)?])?;
        tx.commit()?; Ok(())
    })?
}
pub fn bind(db: &DbManager, session: &str, definition: &Definition) -> Result<()> {
    definition.validate()?;
    let payload = serde_json::to_string(definition)?;
    db.with_conn(|c|c.execute("INSERT INTO session_agents(conversation_id,definition) VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET definition=excluded.definition", rusqlite::params![session,payload]))??;
    Ok(())
}
pub fn for_session(db: &DbManager, session: &str) -> Result<Option<Definition>> {
    let value: Option<String> = db.with_conn(|c| {
        c.query_row(
            "SELECT definition FROM session_agents WHERE conversation_id=?1",
            [session],
            |r| r.get(0),
        )
        .optional()
    })??;
    value
        .map(|v| serde_json::from_str(&v).map_err(Into::into))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explore_and_plan_are_read_only_search_agents() {
        let definitions = builtins();
        for id in ["explore", "plan"] {
            let definition = definitions
                .iter()
                .find(|definition| definition.id == id)
                .unwrap_or_else(|| panic!("{id} must be built in"));
            let tools = definition.tools.as_ref().expect("a tool allowlist");
            for tool in ["Glob", "Bash"] {
                assert!(
                    tools.iter().any(|granted| granted == tool),
                    "{id} needs {tool}: {tools:?}"
                );
            }
            assert!(
                !tools.iter().any(|granted| granted == "AskUserQuestion"),
                "a background child cannot use AskUserQuestion: {tools:?}"
            );
            assert!(definition.read_only_shell, "{id} limits Bash to reads");
            assert_ne!(
                definition.description, definition.system_prompt,
                "{id} needs a one-line description and a full system prompt"
            );
        }
    }

    #[test]
    fn stored_definitions_without_read_only_shell_still_load() {
        // Definitions saved before the field existed must read as unrestricted.
        let definition: Definition = serde_json::from_value(serde_json::json!({
            "id": "legacy",
            "description": "Legacy definition",
            "system_prompt": "Inspect evidence",
            "tools": ["Read"]
        }))
        .unwrap();
        assert!(!definition.read_only_shell);
        let encoded = serde_json::to_value(&definition).unwrap();
        assert!(
            encoded.get("read_only_shell").is_none(),
            "false stays out of stored definitions: {encoded}"
        );
    }
}
