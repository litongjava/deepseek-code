//! Shared CLI/Desktop session permission syntax. Restricted shell grants never
//! authorize compound commands or substitutions without a real shell parser.
use super::{rules::PermissionAction, PermissionContext};
use anyhow::{ensure, Result};
use std::path::{Component, Path, PathBuf};

pub struct Rule {
    tool: String,
    content: Option<String>,
}

/// Server-only and server wildcard rules share the upstream namespace boundary.
fn mcp_server_rule(name: &str) -> Option<&str> {
    let suffix = name.strip_prefix("mcp__")?;
    let server = match suffix.split_once("__") {
        Some((server, "*")) => server,
        Some(_) => return None,
        None => suffix,
    };
    (!server.is_empty()
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
    .then_some(server)
}

impl Rule {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty() && !value.chars().any(char::is_control),
            "Permission rule is empty or contains control characters"
        );
        let (tool, content) = if let Some((tool, rest)) = value.split_once('(') {
            let content = rest
                .strip_suffix(')')
                .ok_or_else(|| anyhow::anyhow!("Permission rule must end with ')'"))?;
            ensure!(
                !content.trim().is_empty(),
                "Permission pattern cannot be empty"
            );
            ensure!(
                matches!(tool, "Bash" | "Read" | "Edit" | "Write" | "MultiEdit"),
                "Patterns are supported for Bash, Read, Edit, Write and MultiEdit only"
            );
            let mut decoded = String::new();
            let mut chars = content.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '\\' && chars.peek().is_some_and(|c| matches!(c, '(' | ')' | '\\')) {
                    decoded.push(chars.next().unwrap());
                } else {
                    ensure!(
                        !matches!(ch, '(' | ')'),
                        "Escape parentheses inside a permission pattern with a backslash"
                    );
                    decoded.push(ch);
                }
            }
            if tool != "Bash" {
                glob::Pattern::new(&decoded.replace('\\', "/"))?;
                ensure!(
                    !decoded.replace('\\', "/").split('/').any(|p| p == ".."),
                    "Permission path patterns cannot contain '..'"
                );
            }
            (tool, Some(decoded))
        } else {
            (value, None)
        };
        ensure!(
            !tool.is_empty()
                && (tool.chars().all(|c| c.is_ascii_alphanumeric()
                    || matches!(c, '_' | '-' | ':')
                    || (tool.starts_with("mcp__") && c == '.'))
                    || mcp_server_rule(tool).is_some_and(|_| tool.ends_with("__*"))),
            "Invalid tool name in permission rule"
        );
        Ok(Self {
            tool: tool.into(),
            content,
        })
    }

    pub fn matches(&self, context: &PermissionContext, action: &PermissionAction) -> bool {
        if self.tool != context.tool_name {
            let server_match = self.content.is_none()
                && mcp_server_rule(&self.tool).is_some_and(|server| {
                    context
                        .tool_name
                        .strip_prefix("mcp__")
                        .and_then(|value| value.split_once("__"))
                        .is_some_and(|(target, tool)| target == server && !tool.is_empty())
                });
            if !server_match {
                return false;
            }
        }
        let Some(pattern) = &self.content else {
            return true;
        };
        if self.tool == "Bash" {
            let Some(command) = context.tool_input["command"].as_str() else {
                return false;
            };
            let complex = command.chars().any(|c| {
                matches!(
                    c,
                    ';' | '&' | '|' | '`' | '$' | '<' | '>' | '\n' | '\r' | '(' | ')'
                )
            });
            // Without a shell AST, never let substitutions or nested shells hide
            // a restricted command. Conservative deny/ask wins for complex input.
            if complex {
                return *action != PermissionAction::Allow;
            }
            return shell_matches(pattern, command.trim());
        }
        let Some(path) = context.tool_input["file_path"]
            .as_str()
            .or_else(|| context.tool_input["path"].as_str())
        else {
            return false;
        };
        let Some(workspace) = context.workspace_path.as_deref() else {
            return false;
        };
        let root = resolved(Path::new(workspace));
        let path = Path::new(path);
        let lexical = normalize(&if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        });
        let real = resolved(&lexical);
        let pattern = pattern.replace('\\', "/");
        let absolute_pattern = if let Some(suffix) = pattern.strip_prefix("//") {
            if suffix.as_bytes().get(1) == Some(&b':') {
                suffix.to_owned()
            } else {
                format!("/{suffix}")
            }
        } else if let Some(suffix) = pattern.strip_prefix("~/") {
            let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
            else {
                return false;
            };
            format!("{}/{}", PathBuf::from(home).to_string_lossy(), suffix)
        } else if pattern.as_bytes().get(1) == Some(&b':') {
            pattern
        } else {
            format!(
                "{}/{}",
                root.to_string_lossy(),
                pattern.trim_start_matches('/').trim_start_matches("./")
            )
        }
        .replace('\\', "/");
        let Ok(glob) = glob::Pattern::new(&absolute_pattern) else {
            return false;
        };
        let options = glob::MatchOptions {
            case_sensitive: !cfg!(windows),
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        let matches =
            |p: &Path| glob.matches_with(&p.to_string_lossy().replace('\\', "/"), options);
        if *action == PermissionAction::Allow {
            matches(&lexical) && matches(&real)
        } else {
            matches(&lexical) || matches(&real)
        }
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return normalize(Path::new(
            &real.to_string_lossy().trim_start_matches("\\\\?\\"),
        ));
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if parent != path {
            return resolved(parent).join(name);
        }
    }
    normalize(path)
}

fn shell_matches(pattern: &str, command: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return command == prefix
            || command
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(' '));
    }
    let mut regex = String::from("(?s)^");
    let mut chars = pattern.chars().peekable();
    let mut stars = 0;
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(|c| matches!(c, '*' | '\\')) {
            regex.push_str(&regex::escape(&chars.next().unwrap().to_string()));
        } else if ch == '*' {
            regex.push_str(".*");
            stars += 1;
        } else {
            regex.push_str(&regex::escape(&ch.to_string()));
        }
    }
    if stars == 1 && regex.ends_with(" .*") {
        regex.truncate(regex.len() - 3);
        regex.push_str("(?: .*)?");
    }
    regex.push('$');
    regex::Regex::new(&regex).is_ok_and(|r| r.is_match(command))
}

/// Legacy tool names accepted in rules and hook conditions.
pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "Task" => "Agent",
        "KillShell" => "TaskStop",
        "AgentOutputTool" | "BashOutputTool" => "TaskOutput",
        name => name,
    }
}

/// Drops empty or match-all content and maps legacy tool names.
pub fn canonical_rule(rule: &str) -> String {
    let rule = rule
        .strip_suffix("()")
        .or_else(|| rule.strip_suffix("(*)"))
        .unwrap_or(rule);
    match rule.split_once('(') {
        Some((tool, rest)) => format!("{}({rest}", canonical_tool_name(tool)),
        None => canonical_tool_name(rule).to_owned(),
    }
}

pub fn validate(value: &str) -> Result<()> {
    Rule::parse(value).map(|_| ())
}

pub fn matching_action(
    rules: &std::collections::HashMap<String, PermissionAction>,
    context: &PermissionContext,
) -> Option<PermissionAction> {
    let mut selected = None;
    for (text, action) in rules {
        if let Ok(rule) = Rule::parse(text) {
            // A restricted shell allow must fall back to confirmation for
            // complex syntax, even if the manager's default mode allows tools.
            if *action == PermissionAction::Allow
                && rule.content.is_some()
                && rule.tool == "Bash"
                && !rule.matches(context, action)
                && rule.matches(context, &PermissionAction::Confirm)
            {
                selected = Some(PermissionAction::Confirm);
            }
            if !rule.matches(context, action) {
                continue;
            }
            match action {
                PermissionAction::Deny => return Some(PermissionAction::Deny),
                PermissionAction::Confirm => selected = Some(PermissionAction::Confirm),
                PermissionAction::Allow if selected.is_none() => {
                    selected = Some(PermissionAction::Allow)
                }
                _ => {}
            }
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(tool: &str, input: serde_json::Value, workspace: &Path) -> PermissionContext {
        PermissionContext {
            tool_name: tool.into(),
            tool_input: input,
            conversation_id: "test".into(),
            user_id: None,
            workspace_path: Some(workspace.to_string_lossy().into()),
        }
    }

    #[test]
    fn shell_boundaries_escapes_and_complex_commands() {
        assert!(shell_matches("git:*", "git status"));
        assert!(!shell_matches("git:*", "github status"));
        assert!(shell_matches("git *", "git"));
        // Remembered approval prefixes, as `npm run *` from the approval menu.
        assert!(shell_matches("npm run *", "npm run lint --fix"));
        assert!(!shell_matches("npm run *", "npm runner"));
        assert!(shell_matches("git * status", "git -C repo status"));
        assert!(shell_matches(r"echo \*", "echo *"));
        assert!(!shell_matches(r"echo \*", "echo anything"));
        let rule = Rule::parse("Bash(git *)").unwrap();
        for command in [
            "git status; evil",
            "git status && evil",
            "git $(evil)",
            "git status > secret",
            "git status\nevil",
        ] {
            let context = context("Bash", json!({"command":command}), Path::new("."));
            assert!(!rule.matches(&context, &PermissionAction::Allow));
            assert!(rule.matches(&context, &PermissionAction::Deny));
        }
    }

    #[test]
    fn invalid_rules_do_not_silently_become_broad_grants() {
        for text in [
            "",
            "Bash()",
            "Bash(git",
            "Bash(git) extra",
            "Read(../secret)",
            "Read([)",
            "Unknown(*)",
            "Bash(echo (x))",
            "Bash\n",
        ] {
            assert!(Rule::parse(text).is_err(), "{text}");
        }
        assert!(Rule::parse(r"Bash(echo \(x\))").is_ok());
    }

    #[test]
    fn deny_then_ask_precede_grants_independent_of_hash_order() {
        let context = context("Bash", json!({"command":"git status"}), Path::new("."));
        let mut rules = std::collections::HashMap::from([
            ("Bash".into(), PermissionAction::Allow),
            ("Bash(git *)".into(), PermissionAction::Confirm),
        ]);
        assert_eq!(
            matching_action(&rules, &context),
            Some(PermissionAction::Confirm)
        );
        rules.insert("Bash(git status)".into(), PermissionAction::Deny);
        assert_eq!(
            matching_action(&rules, &context),
            Some(PermissionAction::Deny)
        );
        rules.insert("Bash".into(), PermissionAction::Deny);
        rules.insert("Bash(git status)".into(), PermissionAction::Allow);
        assert_eq!(
            matching_action(&rules, &context),
            Some(PermissionAction::Deny)
        );
    }

    #[test]
    fn path_rules_anchor_to_workspace_and_normalize_traversal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let rule = Rule::parse("Edit(/src/**)").unwrap();
        assert!(rule.matches(
            &context("Edit", json!({"file_path":"src/new.rs"}), root.path()),
            &PermissionAction::Allow
        ));
        assert!(!rule.matches(
            &context(
                "Edit",
                json!({"file_path":"src/../../outside.rs"}),
                root.path()
            ),
            &PermissionAction::Allow
        ));
        assert!(!rule.matches(
            &context("Read", json!({"file_path":"src/new.rs"}), root.path()),
            &PermissionAction::Allow
        ));
        let one = Rule::parse("Read(src/*)").unwrap();
        assert!(!one.matches(
            &context("Read", json!({"path":"src/sub/new.rs"}), root.path()),
            &PermissionAction::Allow
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_never_inherits_a_path_grant() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let rule = Rule::parse("Write(/**)").unwrap();
        let context = context("Write", json!({"file_path":"link/new.txt"}), root.path());
        assert!(!rule.matches(&context, &PermissionAction::Allow));
        assert!(rule.matches(&context, &PermissionAction::Deny));
    }
}
