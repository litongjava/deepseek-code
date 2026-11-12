//! Versioned upstream skill assets, loaded only when a skill is invoked.
//!
//! Each process publishes assets into a fresh private temporary directory. Read
//! serves only registered, exact asset names from the executable: registering a
//! bundle never grants access to its parent directory or write permissions.
use anyhow::{ensure, Context, Result};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

#[path = "bundled_manifest.rs"]
mod manifest;

struct Bundle {
    directory: tempfile::TempDir,
    files: HashMap<PathBuf, &'static [u8]>,
}

// Weak registry exposes published immutable bytes without keeping temporary
// files alive. The engine and its active children own the actual resources.
static REGISTRY: LazyLock<Mutex<Vec<Weak<Bundle>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Default)]
pub struct Resources {
    bundles: Mutex<HashMap<String, Arc<Bundle>>>,
}

pub fn supports(name: &str) -> bool {
    matches!(name, "dataviz" | "skill-creator")
}

/// Embedded files for one bundled skill, in manifest order.
pub(crate) fn files(name: &str) -> impl Iterator<Item = (&'static str, &'static [u8])> + '_ {
    manifest::FILES
        .iter()
        .filter(move |(skill, _, _)| *skill == name)
        .map(|(_, relative, bytes)| (*relative, *bytes))
}

/// Exact embedded bytes for a registered relative path.
pub(crate) fn asset(name: &str, relative: &str) -> Option<&'static [u8]> {
    manifest::FILES
        .iter()
        .find(|(skill, path, _)| *skill == name && *path == relative)
        .map(|(_, _, bytes)| *bytes)
}

impl Resources {
    pub fn prepare(&self, name: &str, arguments: &str, workspace: &Path) -> Result<String> {
        ensure!(supports(name), "Unknown bundled resource skill");
        let mut bundles = self
            .bundles
            .lock()
            .map_err(|_| anyhow::anyhow!("Skill resource cache unavailable"))?;
        if !bundles.contains_key(name) {
            let prefix = if name == "skill-creator" {
                "deepseek-skill-creator-".to_string()
            } else {
                format!("claude-skill-2.1.295-{name}-")
            };
            let directory = tempfile::Builder::new().prefix(&prefix).tempdir()?;
            let mut files = HashMap::new();
            for &(skill, relative, bytes) in manifest::FILES {
                if skill != name {
                    continue;
                }
                let relative = Path::new(relative);
                ensure!(
                    relative
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                    "Invalid embedded asset path"
                );
                let target = directory.path().join(relative);
                std::fs::create_dir_all(target.parent().context("Missing asset parent")?)?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                file.write_all(bytes)?;
                files.insert(target, bytes);
            }
            let bundle = Arc::new(Bundle { directory, files });
            let mut registry = REGISTRY
                .lock()
                .map_err(|_| anyhow::anyhow!("Skill resource registry unavailable"))?;
            registry.retain(|entry| entry.strong_count() > 0);
            registry.push(Arc::downgrade(&bundle));
            bundles.insert(name.into(), bundle);
        }
        let bundle = &bundles[name];
        // Never silently execute a changed script on a subsequent invocation.
        // There are no overwrites or repairs of paths supplied by the workspace.
        for (path, bytes) in &bundle.files {
            let metadata = std::fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Bundled skill resource changed; restart the session"
            );
            ensure!(
                std::fs::read(path)?.as_slice() == *bytes,
                "Bundled skill resource changed; restart the session"
            );
        }
        let instructions = match name {
            "dataviz" => include_str!("bundled/resources/dataviz/INSTRUCTIONS.md"),
            "skill-creator" => include_str!("bundled/resources/skill-creator/INSTRUCTIONS.md"),
            _ => unreachable!(),
        };
        let provenance = if name == "skill-creator" {
            "Bundled skill-creator embedded in DeepSeek Code; no network or prior install is required. Adapted from OpenAI Codex's skill-creator under the Apache-2.0 license; see NOTICE.md and license.txt in the base directory."
        } else {
            "Bundled documentation snapshot: Claude Code 2.1.295."
        };
        Ok(format!(
        "Base directory for this skill: {}\n\n{} Resolve resource paths against the base directory. Use Read for referenced documents. Execute scripts only through normal Bash permissions; invocation itself executes no scripts. After restarting a worker, invoke this skill again to obtain its current resource directory.\n\n{instructions}\n\nCurrent workspace: {}\n\n## User Request\n\n{arguments}",
        bundle.directory.path().display(), provenance, workspace.display()
    ))
    }
}

/// Exact registered filenames only; no directory, glob, alias or traversal grant.
pub(crate) fn read(path: &Path) -> Option<&'static [u8]> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    REGISTRY
        .lock()
        .ok()?
        .iter()
        .filter_map(Weak::upgrade)
        .find_map(|bundle| bundle.files.get(path).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_resources_match_acquired_sha256_manifest() {
        let provenance: serde_json::Value =
            serde_json::from_str(include_str!("bundled/resources/manifest.json")).unwrap();
        let files = provenance["files"].as_array().unwrap();
        assert_eq!(files.len(), manifest::FILES.len());
        for entry in files {
            let (_, _, bytes) = manifest::FILES
                .iter()
                .find(|(name, path, _)| entry["skill"] == *name && entry["path"] == *path)
                .expect("Missing upstream asset");
            let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
            let hash: String = hash
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(hash, entry["sha256"].as_str().unwrap());
            assert_eq!(bytes.len() as u64, entry["bytes"].as_u64().unwrap());
        }
    }

    #[test]
    fn changed_files_are_not_repaired_or_served_as_new_instructions() {
        let resources = Resources::default();
        let workspace = tempfile::tempdir().unwrap();
        resources.prepare("dataviz", "", workspace.path()).unwrap();
        let path = resources.bundles.lock().unwrap()["dataviz"]
            .directory
            .path()
            .join("references/palette.md");
        let original = read(&path).unwrap();
        std::fs::write(&path, "modified externally").unwrap();
        assert_eq!(read(&path).unwrap(), original);
        assert!(resources
            .prepare("dataviz", "", workspace.path())
            .unwrap_err()
            .to_string()
            .contains("resource changed"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "modified externally"
        );
        drop(resources);
        assert!(read(&path).is_none());
        assert!(!path.exists());
    }

    #[test]
    fn skill_creator_bundle_is_available_and_materialized_on_invocation() {
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        let prompt = resources
            .prepare("skill-creator", "build a PDF skill", workspace.path())
            .unwrap();
        assert!(prompt.contains("build a PDF skill"));
        assert!(prompt.contains("NOTICE.md"));
        assert!(prompt.contains("Apache-2.0"));
        let bundle = resources.bundles.lock().unwrap()["skill-creator"]
            .directory
            .path()
            .to_path_buf();
        assert!(prompt.contains(&bundle.display().to_string()));
        for (relative, expected) in [
            (
                "SKILL.md",
                &include_bytes!("bundled/resources/skill-creator/SKILL.md")[..],
            ),
            (
                "INSTRUCTIONS.md",
                &include_bytes!("bundled/resources/skill-creator/INSTRUCTIONS.md")[..],
            ),
            (
                "NOTICE.md",
                &include_bytes!("bundled/resources/skill-creator/NOTICE.md")[..],
            ),
            (
                "license.txt",
                &include_bytes!("bundled/resources/skill-creator/license.txt")[..],
            ),
            (
                "references/frontmatter.md",
                &include_bytes!("bundled/resources/skill-creator/references/frontmatter.md")[..],
            ),
            (
                "scripts/init_skill.py",
                &include_bytes!("bundled/resources/skill-creator/scripts/init_skill.py")[..],
            ),
            (
                "scripts/quick_validate.py",
                &include_bytes!("bundled/resources/skill-creator/scripts/quick_validate.py")[..],
            ),
        ] {
            let materialized = std::fs::read(bundle.join(relative)).unwrap();
            assert_eq!(materialized, expected, "{relative} is not byte-exact");
        }
        let skill_md = std::fs::read(bundle.join("SKILL.md")).unwrap();
        assert!(String::from_utf8_lossy(&skill_md).contains("name: skill-creator"));
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            &std::fs::read(bundle.join("license.txt")).unwrap(),
        );
        let digest: String = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            digest,
            "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
        );
        assert!(read(&bundle.join("scripts/init_skill.py")).is_some());
        assert!(read(&bundle.join("agents/openai.yaml")).is_none());
        assert!(std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn python_scripts_create_and_validate_skills_without_site_packages() {
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        resources
            .prepare("skill-creator", "", workspace.path())
            .unwrap();
        let bundle = resources.bundles.lock().unwrap()["skill-creator"]
            .directory
            .path()
            .to_path_buf();
        let python = if cfg!(windows) { "python" } else { "python3" };
        if std::process::Command::new(python)
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: {python} is not available");
            return;
        }
        let output_dir = tempfile::tempdir().unwrap();
        let skill_dir = output_dir.path().join("test-skill");
        // -S disables site-packages, proving the scripts use only the standard library.
        let init = std::process::Command::new(python)
            .arg("-S")
            .arg(bundle.join("scripts/init_skill.py"))
            .arg("test-skill")
            .arg("--path")
            .arg(output_dir.path())
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        for (relative, needle) in [
            ("SKILL.md", "name: test-skill"),
            ("SKILL.md", "description: \"Explain"),
        ] {
            assert!(std::fs::read_to_string(skill_dir.join(relative))
                .unwrap()
                .contains(needle));
        }
        let validate = std::process::Command::new(python)
            .arg("-S")
            .arg(bundle.join("scripts/quick_validate.py"))
            .arg(&skill_dir)
            .output()
            .unwrap();
        assert!(
            validate.status.success(),
            "{}",
            String::from_utf8_lossy(&validate.stdout)
        );
        assert!(String::from_utf8_lossy(&validate.stdout).contains("Skill is valid!"));

        for (label, content, needle) in [
            (
                "missing name",
                "---\ndescription: test\n---\nbody\n",
                "Missing 'name'",
            ),
            (
                "missing description",
                "---\nname: test\n---\nbody\n",
                "Missing 'description'",
            ),
            (
                "bad name",
                "---\nname: \"Bad Name\"\ndescription: test\n---\nbody\n",
                "may contain only",
            ),
            (
                "unterminated frontmatter",
                "---\nname: test\ndescription: test\nbody\n",
                "Unterminated",
            ),
            (
                "non-boolean enabled",
                "---\nname: test\ndescription: test\nenabled: yes\n---\nbody\n",
                "must be a YAML boolean",
            ),
            (
                "missing space after key colon",
                "---\nname:test\ndescription: test\n---\nbody\n",
                "Invalid frontmatter line",
            ),
        ] {
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
            let output = std::process::Command::new(python)
                .arg("-S")
                .arg(bundle.join("scripts/quick_validate.py"))
                .arg(&skill_dir)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(!output.status.success(), "{label} was accepted");
            assert!(stdout.contains(needle), "{label}: {stdout}");
        }

        // Quoted keys and block scalars are valid YAML that DeepSeek Code reads.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\"name\": \"quoted:name\"\ndescription: |\n  A block description\n  on two lines.\nwhenToUse: test\n---\nbody\n",
        )
        .unwrap();
        let output = std::process::Command::new(python)
            .arg("-S")
            .arg(bundle.join("scripts/quick_validate.py"))
            .arg(&skill_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "block scalar/quoted key must be accepted: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        // DeepSeek Code ignores unknown keys, so the validator must not reject them.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test\ndescription: test\nmetadata:\n  short-description: fixture\ncustom: true\n---\nbody\n",
        )
        .unwrap();
        let output = std::process::Command::new(python)
            .arg("-S")
            .arg(bundle.join("scripts/quick_validate.py"))
            .arg(&skill_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "unknown keys must be tolerated: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn materializes_complete_resources_lazily_and_keeps_arguments_literal() {
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        for (name, count, example) in [
            ("dataviz", 9, "scripts/validate_palette.js"),
        ] {
            let prompt = resources
                .prepare(
                    name,
                    "literal !`echo should-not-run` $ARGUMENTS 中文😀",
                    workspace.path(),
                )
                .unwrap();
            assert!(prompt.contains("literal !`echo should-not-run` $ARGUMENTS 中文😀"));
            assert!(!prompt.contains("official-skills-mq4AvY"));
            assert!(!prompt.contains("Describe the available local instructions only."));
            let bundles = resources.bundles.lock().unwrap();
            let bundle = &bundles[name];
            assert_eq!(bundle.files.len(), count);
            let path = bundle.directory.path().join(example);
            assert_eq!(std::fs::read(&path).unwrap(), bundle.files[&path]);
            let root = bundle.directory.path().to_path_buf();
            drop(bundles);
            assert!(read(&path).is_some());
            let response = crate::tools::execute_tool(
                "Read",
                serde_json::json!({"file_path":path,"limit":4}),
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
            assert!(response["content"].as_str().unwrap().contains("1\t"));
            assert!(response["lines"].as_u64().unwrap() > 4);
            assert!(read(&root.join("../unregistered.txt")).is_none());
            assert!(read(&root.join("not-in-manifest.md")).is_none());
            assert!(crate::tools::resolve_path(
                path.to_str().unwrap(),
                workspace.path().to_str().unwrap()
            )
            .is_err());
        }
        assert!(std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none());
        let directories: Vec<_> = resources
            .bundles
            .lock()
            .unwrap()
            .values()
            .map(|b| b.directory.path().to_path_buf())
            .collect();
        drop(resources);
        assert!(directories.iter().all(|path| !path.exists()));
    }
}
