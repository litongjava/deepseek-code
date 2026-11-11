//! Embedded skill-creator: invocation metadata, the read-only manager entry,
//! and migration away from the old placeholder written by earlier versions.
use super::{bundled_resources, Skill, SkillFile, SkillSource};
use anyhow::{anyhow, ensure, Context, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(super) const NAME: &str = "skill-creator";
/// Stable id used by hosts that list the embedded skill without a disk copy.
pub(super) const EMBEDDED_ID: &str = "embedded:skill-creator";
pub(super) const DESCRIPTION: &str = "Create or update a DeepSeek Code skill with appropriately scoped instructions and any needed supporting resources.";
pub(super) const WHEN_TO_USE: &str = "Use when creating a new skill, revising an existing skill, or adding scripts, references, or assets to a skill.";

pub(super) struct Embedded {
    pub content: &'static str,
    pub files: Vec<SkillFile>,
}

/// Embedded bytes are the only source of truth; the manager entry mirrors the
/// same manifest used for invocation materialization.
pub(super) fn embedded() -> Embedded {
    let mut files = Vec::new();
    for (relative, _) in bundled_resources::files(NAME) {
        insert_file(&mut files, &relative.split('/').collect::<Vec<_>>());
    }
    sort_tree(&mut files);
    Embedded {
        content: include_str!("bundled/resources/skill-creator/SKILL.md"),
        files,
    }
}

fn insert_file(nodes: &mut Vec<SkillFile>, parts: &[&str]) {
    let Some((head, rest)) = parts.split_first() else {
        return;
    };
    if rest.is_empty() {
        if !nodes.iter().any(|node| node.name == *head) {
            nodes.push(SkillFile {
                name: (*head).to_string(),
                file_type: "file".to_string(),
                children: None,
            });
        }
        return;
    }
    let index = match nodes
        .iter()
        .position(|node| node.name == *head && node.file_type == "folder")
    {
        Some(index) => index,
        None => {
            nodes.push(SkillFile {
                name: (*head).to_string(),
                file_type: "folder".to_string(),
                children: Some(Vec::new()),
            });
            nodes.len() - 1
        }
    };
    insert_file(nodes[index].children.get_or_insert_with(Vec::new), rest);
}

fn sort_tree(nodes: &mut [SkillFile]) {
    nodes.sort_by(
        |left, right| match (&left.file_type[..], &right.file_type[..]) {
            ("folder", "file") => std::cmp::Ordering::Less,
            ("file", "folder") => std::cmp::Ordering::Greater,
            _ => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
        },
    );
    for node in nodes.iter_mut() {
        if let Some(children) = node.children.as_mut() {
            sort_tree(children);
        }
    }
}

pub(super) fn manager_skill() -> Skill {
    let embedded = embedded();
    Skill {
        id: EMBEDDED_ID.to_string(),
        name: NAME.to_string(),
        description: DESCRIPTION.to_string(),
        content: Some(embedded.content.to_string()),
        when_to_use: Some(WHEN_TO_USE.to_string()),
        enabled: true,
        source: SkillSource::Bundled,
        loaded_from: "bundled".to_string(),
        source_dir: None,
        is_example: false,
        files: embedded.files,
        created_at: None,
        od_metadata: None,
    }
}

/// Read one embedded file for the read-only manager entry. Relative paths must
/// stay inside the skill; only manifest-registered names are served.
pub(super) fn embedded_file(relative: &str) -> Result<&'static [u8]> {
    let normalized = relative.replace('\\', "/");
    ensure!(
        !normalized.is_empty()
            && !normalized.contains(':')
            && normalized
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Skill file path must stay within its directory"
    );
    bundled_resources::asset(NAME, &normalized)
        .ok_or_else(|| anyhow!("Bundled skill file not found: {relative}"))
}

/// Called on every skill load. Creating the bundled root is enough for a new
/// install; the placeholder files are never written again.
pub(super) fn install_and_migrate(bundled_path: &Path) -> Result<()> {
    std::fs::create_dir_all(bundled_path)?;
    match remove_pristine_placeholder(bundled_path) {
        Ok(true) => tracing::info!(
            module = "SkillsManager::migration",
            "Removed the pristine bundled skill-creator placeholder"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            module = "SkillsManager::migration",
            "Kept bundled/skill-creator because it could not be inspected safely: {error}"
        ),
    }
    Ok(())
}

/// Remove only the exact stub written by earlier DeepSeek Code versions. Any
/// changed byte, extra file or extra directory keeps the user's copy untouched.
pub(super) fn remove_pristine_placeholder(bundled_path: &Path) -> Result<bool> {
    let directory = bundled_path.join(NAME);
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(false);
    }

    let mut files = BTreeMap::new();
    let mut directories = Vec::new();
    collect_tree(&directory, &directory, &mut files, &mut directories)?;
    let expected_directories = ["agents".to_string()];
    if directories != expected_directories {
        return Ok(false);
    }
    let expected = [
        ("SKILL.md", STUB_SKILL_MD),
        ("agents/analyzer.md", STUB_ANALYZER),
        ("agents/grader.md", STUB_GRADER),
    ];
    if files.len() != expected.len()
        || expected
            .iter()
            .any(|(name, content)| files.get(*name).map(String::as_str) != Some(*content))
    {
        return Ok(false);
    }

    match std::fs::remove_dir_all(&directory) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn collect_tree(
    directory: &Path,
    root: &Path,
    files: &mut BTreeMap<String, String>,
    directories: &mut Vec<String>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let relative = path
            .strip_prefix(root)
            .context("Placeholder path escaped its directory")?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(anyhow!("Symbolic links are not allowed in skills"));
        }
        if metadata.is_dir() {
            directories.push(relative);
            collect_tree(&path, root, files, directories)?;
        } else if metadata.is_file() {
            files.insert(relative, std::fs::read_to_string(&path)?);
        } else {
            return Err(anyhow!("Unexpected file type in skill directory"));
        }
    }
    directories.sort();
    Ok(())
}

pub(super) const STUB_SKILL_MD: &str = r#"---
name: skill-creator
description: A toolkit for creating, evaluating, and improving Claude skills
whenToUse: When you want to build a new skill, evaluate an existing one, or generate synthetic training data for skills
---

# Skill Creator

A comprehensive toolkit for creating, evaluating, and improving Claude skills.

## Usage

This skill provides utilities for:
- Creating new skills from templates
- Evaluating skill quality
- Generating training data
- Packaging skills for distribution

## Files

- `agents/` - Agent definitions for different evaluation tasks
- `scripts/` - Utility scripts for skill development
- `eval-viewer/` - Evaluation result visualization
- `references/` - Schema definitions and documentation
"#;
pub(super) const STUB_ANALYZER: &str = "# Analyzer\n\nAnalyzes skill structure and quality.";
pub(super) const STUB_GRADER: &str = "# Grader\n\nGrades skill performance.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillsManager;

    fn write_stub(root: &Path) -> PathBuf {
        let directory = root.join(NAME);
        std::fs::create_dir_all(directory.join("agents")).unwrap();
        std::fs::write(directory.join("SKILL.md"), STUB_SKILL_MD).unwrap();
        std::fs::write(directory.join("agents/analyzer.md"), STUB_ANALYZER).unwrap();
        std::fs::write(directory.join("agents/grader.md"), STUB_GRADER).unwrap();
        directory
    }

    fn write_sibling(root: &Path) {
        let directory = root.join("legacy-review");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), "user sibling").unwrap();
    }

    #[test]
    fn bundled_instructions_and_command_body_match_the_skill_markdown() {
        let skill = include_str!("bundled/resources/skill-creator/SKILL.md");
        let instructions = include_str!("bundled/resources/skill-creator/INSTRUCTIONS.md");
        let body = &skill[skill.find("\n---\n").unwrap() + "\n---\n".len()..];
        assert_eq!(body, instructions);
        assert_eq!(
            include_str!("bundled/skill-creator.md"),
            instructions,
            "the embedded command body must stay in sync with INSTRUCTIONS.md"
        );
        assert!(instructions.contains("DeepSeek Code"));
        assert!(instructions.contains("references/frontmatter.md"));
    }

    #[test]
    fn embedded_skill_creator_is_discoverable_once_and_invocable() {
        let temp = tempfile::tempdir().unwrap();
        let commands = crate::skills::discovery::discover_from(None, temp.path()).unwrap();
        let matching: Vec<_> = commands
            .values()
            .filter(|command| command.name == NAME)
            .collect();
        assert_eq!(matching.len(), 1);
        let command = matching[0];
        assert!(command.model_invocable());
        assert!(command.user_invocable);
        assert!(command.content.contains("DeepSeek Code"));
        assert!(!command.content.contains("$CODEX_HOME"));
        assert!(!command.content.contains("$skill-name"));
        assert!(command
            .render("create a PDF skill", "session")
            .unwrap()
            .contains("create a PDF skill"));
    }

    #[tokio::test]
    async fn manager_lists_the_embedded_skill_once_and_serves_its_files() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("skills");
        let manager = SkillsManager::with_path(skills_dir.clone());
        let skills = manager.load_skills().await.unwrap();
        assert!(!skills_dir.join("bundled").join(NAME).exists());
        let matching: Vec<_> = skills.iter().filter(|skill| skill.name == NAME).collect();
        assert_eq!(matching.len(), 1);
        let skill = matching[0];
        assert_eq!(skill.source, SkillSource::Bundled);
        assert!(skill
            .content
            .as_deref()
            .unwrap()
            .contains("name: skill-creator"));
        let names: Vec<_> = skill.files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "references",
                "scripts",
                "INSTRUCTIONS.md",
                "license.txt",
                "NOTICE.md",
                "SKILL.md"
            ]
        );
        let script = manager
            .get_skill_content(&skill.id, "scripts/quick_validate.py")
            .unwrap();
        assert!(script.contains("def validate_skill"));
        assert!(manager.get_skill_content(&skill.id, "../SKILL.md").is_err());
        assert!(manager
            .get_skill_content(&skill.id, "agents/openai.yaml")
            .is_err());
    }

    #[tokio::test]
    async fn pristine_placeholder_is_removed_and_siblings_survive() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("skills");
        let bundled = skills_dir.join("bundled");
        write_stub(&bundled);
        write_sibling(&bundled);
        let manager = SkillsManager::with_path(skills_dir.clone());
        let skills = manager.load_skills().await.unwrap();
        assert!(!bundled.join(NAME).exists());
        assert_eq!(
            std::fs::read_to_string(bundled.join("legacy-review/SKILL.md")).unwrap(),
            "user sibling"
        );
        let matching: Vec<_> = skills.iter().filter(|skill| skill.name == NAME).collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].source, SkillSource::Bundled);
        assert!(matching[0].source_dir.is_none());
    }

    #[tokio::test]
    async fn modified_placeholder_and_extra_files_are_kept_untouched() {
        for extra_file in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let skills_dir = temp.path().join("skills");
            let bundled = skills_dir.join("bundled");
            let directory = write_stub(&bundled);
            if extra_file {
                std::fs::write(directory.join("notes.md"), "keep me").unwrap();
            } else {
                std::fs::write(
                    directory.join("agents/grader.md"),
                    "# Grader\n\nUser-edited grading rules.",
                )
                .unwrap();
            }
            let manager = SkillsManager::with_path(skills_dir.clone());
            let skills = manager.load_skills().await.unwrap();
            assert!(directory.exists());
            assert!(directory.join("SKILL.md").exists());
            let matching: Vec<_> = skills.iter().filter(|skill| skill.name == NAME).collect();
            assert_eq!(matching.len(), 1);
            // The user's copy wins over the synthesized entry.
            assert_eq!(
                matching[0].source_dir.as_deref(),
                Some(directory.to_string_lossy().as_ref())
            );
            assert_eq!(
                std::fs::read_to_string(directory.join("SKILL.md")).unwrap(),
                STUB_SKILL_MD
            );
        }
    }

    #[tokio::test]
    async fn user_skill_with_the_same_name_is_not_duplicated() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("skills");
        let user = skills_dir.join("my-skill-creator");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("SKILL.md"),
            "---\nname: skill-creator\ndescription: Custom user copy\n---\nbody\n",
        )
        .unwrap();
        let manager = SkillsManager::with_path(skills_dir);
        let skills = manager.load_skills().await.unwrap();
        let matching: Vec<_> = skills.iter().filter(|skill| skill.name == NAME).collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].description, "Custom user copy");
    }
}
