//! Embedded workflow discovery; resource bundles are materialized on invocation.
use super::discovery::SkillCommand;
use std::{collections::BTreeMap, path::Path};

pub fn commands(workspace: &Path) -> BTreeMap<String, SkillCommand> {
    let definitions = [
        (
            "claude-api",
            "Build apps with the Claude API or Anthropic SDK. Use for anthropic / @anthropic-ai/sdk imports or explicit Claude API requests; not OpenAI or unrelated programming tasks.",
            include_str!("bundled/claude-api.md"),
        ),
        (
            "code-review",
            "Review a diff for actionable correctness and security issues",
            include_str!("bundled/code-review.md"),
        ),
        (
            "security-review",
            "Review pending changes for exploitable security vulnerabilities",
            include_str!("bundled/security-review.md"),
        ),
        (
            "dataviz",
            "Create charts and data visualizations, choose chart forms, and validate palettes, contrast and interaction using runnable reference assets.",
            include_str!("bundled/dataviz.md"),
        ),
        (
            "deep-research",
            "Research a question using cited, verified sources",
            include_str!("bundled/deep-research.md"),
        ),
        (
            "doctor",
            "Inspect local CLI configuration and diagnose failures",
            include_str!("bundled/doctor.md"),
        ),
        // The reference CLI's bundled workflow commands.
        (
            "simplify",
            "Clean up the changed code without changing behavior",
            include_str!("bundled/simplify.md"),
        ),
        (
            "fewer-permission-prompts",
            "Pre-approve safe read-only commands for this project",
            include_str!("bundled/fewer-permission-prompts.md"),
        ),
        (
            "update-config",
            "Change settings: hooks and permission rules",
            include_str!("bundled/update-config.md"),
        ),
        (
            "insights",
            "Generate a report analyzing your DeepSeek Code sessions",
            include_str!("bundled/insights.md"),
        ),
        (
            "team-onboarding",
            "Help teammates ramp up on DeepSeek Code with a guide for this project",
            include_str!("bundled/team-onboarding.md"),
        ),
        (
            "run-skill-generator",
            "Create a skill that knows how to run this project's app",
            include_str!("bundled/run-skill-generator.md"),
        ),
        (
            "auto-mode-setup",
            "Prepare auto mode for your environment, plus permission rules",
            include_str!("bundled/auto-mode-setup.md"),
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, description, body)| {
            let mut content = body.to_owned();
            if name == "claude-api" {
                let detected: Vec<_> = [
                    ("Cargo.toml", "Rust (HTTP API)"),
                    ("package.json", "TypeScript/JavaScript"),
                    ("pyproject.toml", "Python"),
                    ("requirements.txt", "Python"),
                    ("go.mod", "Go"),
                    ("pom.xml", "Java"),
                    ("Gemfile", "Ruby"),
                    ("composer.json", "PHP"),
                ]
                .into_iter()
                .filter(|(file, _)| workspace.join(file).is_file())
                .map(|(_, language)| language)
                .collect();
                content.push_str(&format!(
                    "\nDetected manifest languages (confirm with Read): {}\n",
                    if detected.is_empty() {
                        "undetermined".into()
                    } else {
                        detected.join(", ")
                    }
                ));
            }
            // Discovery runs during SDK turns while the input reader owns stdin.
            // Keep diagnostics lazy: terminal probing here can block on that lock
            // until the SDK client closes its stream. /doctor runs the checks.
            (
                name.into(),
                SkillCommand {
                    name: name.into(),
                    description: description.into(),
                    path: Path::new("__embedded_cli_skills__").join(name),
                    content,
                    user_invocable: true,
                    disable_model_invocation: !super::bundled_resources::supports(name),
                    context: None,
                    shell: None,
                    agent: None,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_workflows_render_arguments_without_nonexistent_file_references() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();
        let commands = super::super::discovery::discover_from(None, temp.path()).unwrap();
        for name in [
            "claude-api",
            "code-review",
            "dataviz",
            "deep-research",
            "doctor",
            "security-review",
        ] {
            let command = &commands[name];
            let rendered = command
                .render("inspect my project", "session-test")
                .unwrap();
            assert!(rendered.contains("inspect my project"));
            assert!(!rendered.contains("__embedded_cli_skills__"));
            assert_eq!(command.disable_model_invocation, !super::super::bundled_resources::supports(name));
        }
        assert!(commands["claude-api"].content.contains("Rust (HTTP API)"));
        assert!(!commands["doctor"].content.contains("Read-only local diagnostic snapshot:"));
    }
    #[test]
    fn project_workflow_overrides_embedded_default() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".claude/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("code-review.md"), "Custom review $ARGUMENTS").unwrap();
        let commands = super::super::discovery::discover_from(None, temp.path()).unwrap();
        assert!(commands["code-review"]
            .render("HEAD", "id")
            .unwrap()
            .contains("Custom review HEAD"));
    }
}
