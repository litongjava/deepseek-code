//! `# note` at the prompt: save a project instruction without a model turn,
//! like the reference CLI's memory shortcut. Requiring "# " (with a space)
//! keeps prompts such as "#1095 fix this" from being saved by accident.
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The instructions file a note goes to: AGENTS.md, else an existing
/// CLAUDE.md, else a new AGENTS.md.
pub fn target(workspace: &Path) -> PathBuf {
    let agents = workspace.join("AGENTS.md");
    let claude = workspace.join("CLAUDE.md");
    if !agents.exists() && claude.exists() { claude } else { agents }
}

/// Appends `- note` to the project's instructions file and returns its path.
pub fn save(workspace: &Path, note: &str) -> Result<PathBuf> {
    let note = note.trim();
    anyhow::ensure!(!note.is_empty(), "Type the note after #, e.g. # Run cargo test before committing");
    let path = target(workspace);
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("- {}\n", note.replace('\n', " ")));
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_append_to_the_project_instructions() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(save(dir.path(), "Use pnpm").unwrap(), dir.path().join("AGENTS.md"));
        save(dir.path(), "  Run tests first ").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(), "- Use pnpm\n- Run tests first\n");
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("CLAUDE.md"), "# Rules").unwrap();
        save(other.path(), "Be brief").unwrap();
        assert_eq!(std::fs::read_to_string(other.path().join("CLAUDE.md")).unwrap(), "# Rules\n- Be brief\n");
        assert!(save(dir.path(), " ").is_err());
    }
}
