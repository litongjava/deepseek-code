//! Embedded workflow discovery; resource bundles are materialized on invocation.
use super::discovery::SkillCommand;
use std::{collections::BTreeMap, path::Path};

pub fn commands() -> BTreeMap<String, SkillCommand> {
    let definitions = [
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
            "Inspect local CLI configuration and diagnose failures; /doctor prompt-audit audits the instruction files loaded into sessions; /doctor skills and /doctor errors answer without the model",
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
            super::skill_creator::NAME,
            super::skill_creator::DESCRIPTION,
            include_str!("bundled/skill-creator.md"),
        ),
        (
            "auto-mode-setup",
            "Prepare auto mode for your environment, plus permission rules",
            include_str!("bundled/auto-mode-setup.md"),
        ),
        // User-only by default (see `states::default_state`): hidden from the
        // model listing until the user changes their state to `user`.
        (
            "translate-zh-en",
            "Translate Chinese text into English",
            include_str!("bundled/translate-zh-en.md"),
        ),
        (
            "translate-en-zh",
            "Translate English text into Chinese",
            include_str!("bundled/translate-en-zh.md"),
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, description, body)| {
            let content = body.to_owned();
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
                    // The translation skills restrict model invocation through
                    // their default state instead, so switching them to `user`
                    // really makes them listable and invocable.
                    disable_model_invocation: !super::bundled_resources::supports(name)
                        && !matches!(name, "translate-zh-en" | "translate-en-zh"),
                    listing_name_only: false,
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
            "code-review",
            "dataviz",
            "deep-research",
            "doctor",
            "security-review",
            "skill-creator",
        ] {
            let command = &commands[name];
            let rendered = command
                .render("inspect my project", "session-test")
                .unwrap();
            assert!(rendered.contains("inspect my project"));
            assert!(!rendered.contains("__embedded_cli_skills__"));
            assert_eq!(command.disable_model_invocation, !super::super::bundled_resources::supports(name));
        }
        assert!(!commands.contains_key("claude-api"));
        assert!(!commands["doctor"].content.contains("Read-only local diagnostic snapshot:"));
    }
    #[test]
    fn translation_skills_discover_as_user_only_until_set_to_user() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");

        let (commands, invalid) =
            super::super::discovery::discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        for name in ["translate-zh-en", "translate-en-zh"] {
            let command = &commands[name];
            assert!(command.disable_model_invocation, "{name}");
            assert!(command.user_invocable, "{name}");
        }

        crate::skills::states::set(
            &data,
            "translate-zh-en",
            crate::skills::states::SkillState::User,
        )
        .unwrap();
        let (commands, invalid) =
            super::super::discovery::discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        assert!(!commands["translate-zh-en"].disable_model_invocation);
        assert!(commands["translate-zh-en"].user_invocable);
        assert!(commands["translate-en-zh"].disable_model_invocation);
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
