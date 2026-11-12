//! The two built-in tool switches, readable and writable from the terminal.
//!
//! The file, the defaults and the legacy `disabled_tools` migration belong to
//! `deepseek_code_core::services::customization`; this module only resolves a
//! tool name and routes one write through `save_tools`, the same call the
//! Desktop Tools page makes.
use anyhow::{bail, Context, Result};
use deepseek_code_core::{
    native_engine::tool_loop::builtin_tool_catalog,
    runtime::RuntimePaths,
    services::customization::{self, Customization, ToolLoading},
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    Enabled,
    OnDemand,
}

/// One catalog entry with the switches that apply to it.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub name: String,
    pub core: bool,
    pub enabled: bool,
    pub on_demand: bool,
}

impl Row {
    pub fn status(&self) -> &'static str {
        summary(ToolLoading { enabled: self.enabled, on_demand: self.on_demand })
    }
}

pub fn summary(loading: ToolLoading) -> &'static str {
    match (loading.enabled, loading.on_demand) {
        (true, true) => "resident, on demand",
        (true, false) => "resident only",
        (false, true) => "on demand only",
        (false, false) => "off",
    }
}

fn names() -> Vec<String> {
    builtin_tool_catalog().into_iter().map(|tool| tool.name).collect()
}

pub fn rows(paths: &RuntimePaths) -> Result<Vec<Row>> {
    let saved = customization::load(&paths.data_dir)?;
    Ok(builtin_tool_catalog()
        .into_iter()
        .map(|tool| {
            let loading = saved.loading(&tool.name);
            Row {
                core: customization::is_core_tool(&tool.name),
                name: tool.name,
                enabled: loading.enabled,
                on_demand: loading.on_demand,
            }
        })
        .collect())
}

fn normalized(name: &str) -> String {
    name.chars()
        .filter(|ch| *ch != '-' && *ch != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Case- and separator-insensitive match against the catalog. An ambiguous
/// name is an error rather than a guess.
pub fn resolve(name: &str) -> Result<String> {
    let wanted = normalized(name);
    let mut matches = names().into_iter().filter(|candidate| normalized(candidate) == wanted);
    let Some(first) = matches.next() else {
        bail!("Unknown tool: {name}. Run `dcl config show` to list the tools");
    };
    if let Some(second) = matches.next() {
        bail!("Tool name {name} is ambiguous: {first} or {second}");
    }
    Ok(first)
}

pub fn parse_switch(field: &str) -> Result<Switch> {
    match normalized(field).as_str() {
        "enabled" | "enable" | "resident" => Ok(Switch::Enabled),
        "ondemand" | "demand" => Ok(Switch::OnDemand),
        _ => bail!("Unknown tool setting: {field}. Use enabled or on_demand"),
    }
}

pub fn parse_value(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        other => bail!("Expected true or false, not {other}"),
    }
}

/// Loads the file, changes one switch, and saves the tool half through core.
/// Any `tools.*` write also folds every legacy `disabled_tools` built-in into
/// the switches, exactly as the Desktop Tools page does.
pub fn set(paths: &RuntimePaths, name: &str, switch: Switch, value: bool) -> Result<(String, Customization)> {
    let name = resolve(name)?;
    let mut current = customization::load(&paths.data_dir)?;
    let mut loading = current.loading(&name);
    match switch {
        Switch::Enabled => loading.enabled = value,
        Switch::OnDemand => loading.on_demand = value,
    }
    current.tool_loading.insert(name.clone(), loading);
    let saved = customization::save_tools(&paths.data_dir, &current, &names())?;
    Ok((name, saved))
}

/// Applies `tools.<Name>.<switch>` and reports the tool's effective switches.
pub fn set_key(paths: &RuntimePaths, key: &str, value: &str) -> Result<String> {
    let rest = key
        .strip_prefix("tools.")
        .with_context(|| format!("Expected tools.<name>.enabled|on_demand, not {key}"))?;
    let (name, field) = rest
        .rsplit_once('.')
        .with_context(|| format!("Expected tools.<name>.enabled|on_demand, not {key}"))?;
    let switch = parse_switch(field)?;
    let value = parse_value(value)?;
    let (name, saved) = set(paths, name, switch, value)?;
    Ok(format!("{name}: {}", summary(saved.loading(&name))))
}

/// The text listing shown outside a terminal, and by `/tools` there.
pub fn print(paths: &RuntimePaths) -> Result<()> {
    let rows = rows(paths)?;
    let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
    for row in &rows {
        command_eprintln!(
            "{:<width$}  {:<19}  {}",
            row.name,
            row.status(),
            if row.core { "core" } else { "" },
            width = width
        );
    }
    command_eprintln!(
        "Switch one with `dcl config set tools.<name>.enabled|on_demand true|false`; /tools toggles them in a terminal."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, RuntimePaths) {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        (root, paths)
    }

    #[test]
    fn switches_are_independent_and_both_can_be_off() {
        let (_root, paths) = fixture();
        set(&paths, "ListDir", Switch::Enabled, true).unwrap();
        assert_eq!(customization::load(&paths.data_dir).unwrap().loading("ListDir"), ToolLoading { enabled: true, on_demand: true });
        set(&paths, "ListDir", Switch::OnDemand, false).unwrap();
        assert_eq!(customization::load(&paths.data_dir).unwrap().loading("ListDir"), ToolLoading { enabled: true, on_demand: false });
        set(&paths, "ListDir", Switch::Enabled, false).unwrap();
        assert_eq!(customization::load(&paths.data_dir).unwrap().loading("ListDir"), ToolLoading { enabled: false, on_demand: false });
    }

    #[test]
    fn a_tools_write_folds_legacy_entries_and_keeps_unknown_names() {
        let (_root, paths) = fixture();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::write(customization::path(&paths.data_dir), r#"{"disabled_tools":["Write","mcp__fixture__tool"]}"#).unwrap();
        set(&paths, "Read", Switch::Enabled, false).unwrap();
        let saved = customization::load(&paths.data_dir).unwrap();
        assert_eq!(saved.loading("Write"), ToolLoading { enabled: false, on_demand: true });
        assert!(!saved.disabled_tools.iter().any(|name| name == "Write"));
        assert!(saved.disabled_tools.iter().any(|name| name == "mcp__fixture__tool"));
        assert_eq!(saved.loading("Read"), ToolLoading { enabled: false, on_demand: true });
    }

    #[test]
    fn unknown_names_fields_and_values_are_rejected_without_writing() {
        let (_root, paths) = fixture();
        assert!(set(&paths, "NoSuchTool", Switch::Enabled, true).is_err());
        assert!(parse_switch("visible").is_err());
        assert!(parse_value("maybe").is_err());
        assert!(set_key(&paths, "tools.ListDir", "true").is_err());
        assert!(set_key(&paths, "search.ListDir.enabled", "true").is_err());
        assert!(!customization::path(&paths.data_dir).exists());
    }

    #[test]
    fn names_resolve_case_and_separator_insensitively() {
        assert_eq!(resolve("list_dir").unwrap(), "ListDir");
        // The duplicate plan entry is retired; it is no longer a catalog name.
        assert!(resolve("PLAN-MODE").is_err());
    }

    #[test]
    fn core_tools_default_resident_and_the_rest_are_on_demand() {
        let (_root, paths) = fixture();
        let rows = rows(&paths).unwrap();
        let core: Vec<&Row> = rows.iter().filter(|row| row.core).collect();
        assert_eq!(core.len(), customization::CORE_TOOLS.len());
        assert!(core.iter().all(|row| row.enabled && row.on_demand));
        let list_dir = rows.iter().find(|row| row.name == "ListDir").unwrap();
        assert!(!list_dir.core && !list_dir.enabled && list_dir.on_demand);
        let task_get = rows.iter().find(|row| row.name == "TaskGet").unwrap();
        assert!(!task_get.core && task_get.on_demand);
    }
}
