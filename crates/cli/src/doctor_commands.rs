//! Invoked explicitly; never called during discovery or startup.
//!
//! `/doctor skills` and `/doctor errors` answer locally, without a model call.
use crate::mcp_menu;
use crate::plugins_panel;
use crate::session::Session;
use anyhow::Result;
use deepseek_code_core::{
    runtime::RuntimePaths,
    services::doctor::{self, DoctorCheck},
};
use std::path::Path;

/// `Some("skills")` or `Some("errors")` only for the exact words, so every
/// other `/doctor` form keeps going to the bundled skill.
pub(crate) fn subcommand(line: &str) -> Option<&'static str> {
    let mut words = line.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("/doctor"), Some("skills"), None) => Some("skills"),
        (Some("/doctor"), Some("errors"), None) => Some("errors"),
        _ => None,
    }
}

/// The first line of an error, as `/mcp` shows it.
fn first_line(error: &str) -> String {
    error.lines().next().unwrap_or_default().to_owned()
}

/// What `/doctor errors` prints: broken definitions from the local checks,
/// then MCP servers that failed to start.
pub(crate) fn errors_text(checks: &[DoctorCheck], failed_mcp: &[(String, String)]) -> String {
    let errors: Vec<&DoctorCheck> = checks.iter().filter(|check| check.status == "error").collect();
    if errors.is_empty() && failed_mcp.is_empty() {
        return "No problems found in skill, agent, settings, keybindings or MCP definitions."
            .to_owned();
    }
    let mut rows = vec!["Problems found:".to_owned()];
    for check in errors {
        rows.push(format!("  {}: {}", check.name, check.detail));
    }
    for (server, error) in failed_mcp {
        rows.push(format!("  MCP server {server}: failed \u{b7} {error}"));
    }
    rows.push(String::new());
    rows.push("Run /doctor to ask the model for help with these.".to_owned());
    rows.join("\n")
}

/// Run one local subcommand. Errors are command failures; a failed MCP
/// initialization is reported as a failed server instead.
pub(crate) async fn run(name: &str, paths: &RuntimePaths, session: &Session) -> Result<String> {
    match name {
        "skills" => {
            let model = plugins_panel::load(paths, session).await?;
            Ok(plugins_panel::stats_rows(&model, chrono::Utc::now(), false).join("\n"))
        }
        "errors" => {
            let mut failed_mcp = Vec::new();
            if let Err(error) = session.ensure_mcp().await {
                failed_mcp.push(("MCP".to_owned(), first_line(&format!("{error:#}"))));
            }
            for server in session.mcp.list_servers().await {
                if mcp_menu::untouched_preset(&server) {
                    continue;
                }
                if let Some(error) = mcp_menu::failure(&server) {
                    let detail = first_line(error);
                    failed_mcp.push((server.name.clone(), detail));
                }
            }
            let mut report = doctor::inspect(&session.workspace, Some(paths));
            // `inspect` checks the structure only; the runtime parser also
            // rejects unsupported actions and reserved shortcuts.
            report.checks.extend(keybindings_check(paths));
            Ok(errors_text(&report.checks, &failed_mcp))
        }
        _ => unreachable!("subcommand only returns skills or errors"),
    }
}

/// The runtime keybinding parser's verdict on `<data dir>/keybindings.json`:
/// an error check when the file exists but is unreadable, oversized or
/// rejected, `None` when it is absent or the parser accepts it.
fn keybindings_check(paths: &RuntimePaths) -> Option<DoctorCheck> {
    let path = paths.data_dir.join("keybindings.json");
    if !path.exists() {
        return None;
    }
    let valid = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.len() <= 65536)
        .and_then(|_| std::fs::read_to_string(&path).ok())
        .is_some_and(|text| crate::keybindings::Keybindings::parse(&text).is_ok());
    (!valid).then(|| DoctorCheck {
        name: "CLI keybindings".to_owned(),
        status: "error".to_owned(),
        detail: "unreadable/oversized file or unsupported context, key, action or reserved shortcut; values withheld".to_owned(),
    })
}

pub fn snapshot(workspace: &Path, paths: &RuntimePaths) -> String {
    let mut report = doctor::inspect(workspace, Some(paths)).render();
    if paths.data_dir.join("keybindings.json").exists() {
        let line = match keybindings_check(paths) {
            Some(check) => format!("CLI keybindings: error \u{2014} {}\n", check.detail),
            None => "CLI keybindings: ok \u{2014} runtime parser accepted the application keybindings\n".to_owned(),
        };
        report.push_str(&line);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, status: &str, detail: &str) -> DoctorCheck {
        DoctorCheck {
            name: name.to_owned(),
            status: status.to_owned(),
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn subcommand_matches_only_the_exact_words() {
        assert_eq!(subcommand("/doctor skills"), Some("skills"));
        assert_eq!(subcommand("  /doctor   errors "), Some("errors"));
        for line in [
            "/doctor",
            "/doctor prompt-audit",
            "/doctor skills now",
            "/doctor Skills",
            "/doctorskills",
        ] {
            assert_eq!(subcommand(line), None, "{line}");
        }
    }

    #[test]
    fn errors_text_reports_nothing_found() {
        let checks = [
            check("runtime", "ok", "fine"),
            check("database", "absent", "not present"),
        ];
        assert_eq!(
            errors_text(&checks, &[]),
            "No problems found in skill, agent, settings, keybindings or MCP definitions."
        );
        assert_eq!(
            errors_text(&[], &[]),
            "No problems found in skill, agent, settings, keybindings or MCP definitions."
        );
    }

    #[test]
    fn errors_text_lists_error_checks_and_failed_servers() {
        let checks = [
            check(
                "runtime",
                "ok",
                "deepseek-code-core 0.0.0 on linux / x86_64",
            ),
            check(
                "skill definition",
                "error",
                "/w/.claude/skills/bad/SKILL.md: Invalid YAML frontmatter at line 1, column 8; definition skipped",
            ),
        ];
        let failed = [("github".to_owned(), "spawn failed".to_owned())];
        let text = errors_text(&checks, &failed);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Problems found:");
        assert!(
            text.contains("  skill definition: /w/.claude/skills/bad/SKILL.md: Invalid YAML frontmatter at line 1, column 8; definition skipped"),
            "{text}"
        );
        assert!(
            text.contains("  MCP server github: failed \u{b7} spawn failed"),
            "{text}"
        );
        assert_eq!(
            lines.last().copied(),
            Some("Run /doctor to ask the model for help with these.")
        );
        assert!(!text.contains("runtime"), "{text}");
    }

    #[test]
    fn errors_text_from_a_real_invalid_skill() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let workspace = temp.path().join("workspace");
        let skill = workspace.join(".claude/skills/bad/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\nname: [DO_NOT_ECHO_SECRET\n---\nbody").unwrap();
        let report = doctor::inspect_with_environment(
            &workspace,
            Some(&paths),
            &doctor::Environment {
                user_config: None,
                executable: None,
                search_path: Vec::new(),
            },
        );
        let text = errors_text(&report.checks, &[]);
        assert!(text.contains("skill definition:"), "{text}");
        assert!(text.contains("definition skipped"), "{text}");
        assert!(!text.contains("DO_NOT_ECHO_SECRET"), "{text}");
    }

    #[test]
    fn errors_include_an_unsupported_keybinding_action() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let file = paths.data_dir.join("keybindings.json");
        std::fs::write(
            &file,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+j":"unknown"}}]}"#,
        )
        .unwrap();
        let check = keybindings_check(&paths).expect("an unsupported action is an error");
        assert_eq!(check.name, "CLI keybindings");
        let text = errors_text(&[check], &[]);
        assert!(text.contains("  CLI keybindings: "), "{text}");
        assert!(!text.contains("No problems found"), "{text}");

        std::fs::write(
            &file,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+j":"input:lineEnd"}}]}"#,
        )
        .unwrap();
        assert!(keybindings_check(&paths).is_none());
    }

    #[test]
    fn errors_include_a_reserved_keybinding() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::write(
            paths.data_dir.join("keybindings.json"),
            r#"{"bindings":[{"context":"Global","bindings":{"ctrl+c":null}}]}"#,
        )
        .unwrap();
        let check = keybindings_check(&paths).expect("a reserved shortcut is an error");
        let text = errors_text(&[check], &[]);
        assert!(text.contains("  CLI keybindings: "), "{text}");
        assert!(!text.contains("No problems found"), "{text}");
    }
}
