//! Command-line tool grants are scoped to the launch, separate from SDK updates.
use crate::args::Cli;
use anyhow::{ensure, Result};
use deepseek_code_core::permissions::{patterns, rules::PermissionAction};
use std::collections::HashMap;

fn split(value: &str) -> Result<Vec<String>> {
    let mut rules = Vec::new();
    let mut current = String::new();
    let mut parenthesized = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if parenthesized => {
                current.push(ch);
                escaped = true;
            }
            '(' => {
                ensure!(
                    !parenthesized,
                    "Escape parentheses inside permission patterns"
                );
                parenthesized = true;
                current.push(ch);
            }
            ')' => {
                ensure!(parenthesized, "Unexpected ')' in permission rule");
                parenthesized = false;
                current.push(ch);
            }
            ',' | ' ' if !parenthesized => {
                if !current.is_empty() {
                    rules.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    ensure!(!parenthesized, "Permission rule must end with ')'");
    if !current.is_empty() {
        rules.push(current);
    }
    Ok(rules)
}

pub fn rules(cli: &Cli) -> Result<HashMap<String, PermissionAction>> {
    let mut result = HashMap::new();
    // Applying denials last retains their precedence when the same pattern occurs twice.
    for (values, action) in [
        (&cli.allowed_tools, PermissionAction::Allow),
        (&cli.disallowed_tools, PermissionAction::Deny),
    ] {
        for value in values {
            ensure!(
                value.len() <= 64 * 1024,
                "CLI tool rule list exceeds 64 KiB"
            );
            for rule in split(value)? {
                let rule = patterns::canonical_rule(&rule);
                patterns::validate(&rule)?;
                result.insert(rule, action.clone());
                ensure!(result.len() <= 1024, "Too many CLI tool permission rules");
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn legacy_names_and_empty_pattern_have_effective_canonical_rules() {
        let cli = Cli::try_parse_from([
            "dcl",
            "--disallowedTools",
            "Task(*) KillShell() AgentOutputTool(*) BashOutputTool Bash() Read(*) mcp__fixture__*",
        ])
        .unwrap();
        let result = rules(&cli).unwrap();
        for name in [
            "Agent",
            "TaskStop",
            "TaskOutput",
            "Bash",
            "Read",
            "mcp__fixture__*",
        ] {
            assert_eq!(result.get(name), Some(&PermissionAction::Deny));
        }
    }

    #[test]
    fn aliases_repeated_values_and_pattern_separators_preserve_rules() {
        let cli = Cli::try_parse_from([
            "dcl",
            "--allowedTools",
            "Bash(git status *), Read(my files/**)",
            "Edit",
            "--allowed-tools",
            r"Bash(echo a,b) Read(my\(files\)/**)",
            "--disallowedTools",
            "Edit,Write",
            "--",
            "prompt",
        ])
        .unwrap();
        let result = rules(&cli).unwrap();
        for pattern in [
            "Bash(git status *)",
            "Read(my files/**)",
            "Bash(echo a,b)",
            r"Read(my\(files\)/**)",
        ] {
            assert_eq!(result.get(pattern), Some(&PermissionAction::Allow));
        }
        assert_eq!(result.get("Edit"), Some(&PermissionAction::Deny));
        assert_eq!(result.get("Write"), Some(&PermissionAction::Deny));
        assert_eq!(cli.prompt.as_deref(), Some("prompt"));
        for value in [
            "Read(ok) Broken(",
            "Read(../secret)",
            "Read(a))",
            "Read(a(b))",
        ] {
            let cli = Cli::try_parse_from(["dcl", "--allowed-tools", value]).unwrap();
            assert!(rules(&cli).is_err(), "{value}");
        }
    }
}
