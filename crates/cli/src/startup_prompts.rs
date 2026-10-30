//! The reference CLI's startup safety prompts: trust a new folder before
//! its project settings and hooks load, and accept bypass mode once (which
//! trusts the folder as well).
use crate::terminal::picker::{Entry, Picker};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Answers {
    #[serde(default)]
    trusted_folders: Vec<PathBuf>,
    #[serde(default)]
    bypass_accepted: bool,
    #[serde(default)]
    full_access_accepted: bool,
}

fn file(data_dir: &Path) -> PathBuf {
    data_dir.join("startup-prompts.json")
}

fn load(data_dir: &Path) -> Answers {
    std::fs::read(file(data_dir)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn save(data_dir: &Path, answers: &Answers) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(file(data_dir), serde_json::to_vec_pretty(answers)?)?;
    Ok(())
}

/// Whether a folder, or one of its parents, was trusted before.
fn trusted(answers: &Answers, folder: &Path) -> bool {
    answers.trusted_folders.iter().any(|trusted| folder.starts_with(trusted))
}

fn choose(heading: &str, body: &str, yes: &str) -> Result<bool> {
    let entries = [
        Entry { title: "No, exit".into(), detail: String::new() },
        Entry { title: yes.into(), detail: String::new() },
    ];
    let picker = Picker { heading, subtitle: Some(body), entries: &entries, search: false, hint: "Enter to confirm \u{b7} Esc to cancel", selected: 0 };
    Ok(picker.run()? == Some(1))
}

/// Asks before an interactive session starts in a folder not trusted yet,
/// and before the first start in bypass mode. Declining exits.
pub fn confirm(data_dir: &Path, folder: &Path, bypass: bool, full_access: bool) -> Result<()> {
    if !crate::terminal::picker::available() {
        return Ok(());
    }
    let folder = folder.canonicalize().unwrap_or_else(|_| folder.to_path_buf());
    let mut answers = load(data_dir);
    // Accepting bypass mode covers the folder too: one prompt, not two.
    if !trusted(&answers, &folder) && !bypass {
        let body = format!(
            "{}\n\nQuick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first.\n\nDeepSeek Code will be able to read, edit, and execute files here.",
            folder.display()
        );
        if !choose("Accessing workspace:", &body, "Yes, I trust this folder")? {
            bail!("Folder not trusted; nothing was changed.");
        }
        answers.trusted_folders.push(folder.clone());
        save(data_dir, &answers)?;
    }
    if full_access && !answers.full_access_accepted {
        let body = "In Full Access mode, DeepSeek Code runs every command without asking, ignores your deny rules and runs commands outside the sandbox, with full network and file access.\n\nUse it only on a disposable machine or container that you can restore.\n\nBy proceeding, you accept all responsibility for actions taken while running in Full Access mode.";
        if !choose("WARNING: DeepSeek Code running in Full Access mode", body, "Yes, I accept")? {
            bail!("Full Access mode not accepted.");
        }
        answers.full_access_accepted = true;
        answers.bypass_accepted = true;
        save(data_dir, &answers)?;
    }
    if bypass && !answers.bypass_accepted {
        let body = "In Bypass Permissions mode, DeepSeek Code will not ask for your approval before running potentially dangerous commands. Explicit deny rules still apply.\n\nThis mode should only be used in a sandboxed container or VM that has restricted internet access and can easily be restored if damaged.\n\nBy proceeding, you accept all responsibility for actions taken while running in Bypass Permissions mode.";
        if !choose("WARNING: DeepSeek Code running in Bypass Permissions mode", body, "Yes, I accept")? {
            bail!("Bypass Permissions mode not accepted.");
        }
        answers.bypass_accepted = true;
        save(data_dir, &answers)?;
    }
    if bypass && !trusted(&answers, &folder) {
        answers.trusted_folders.push(folder);
        save(data_dir, &answers)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_covers_subfolders_and_answers_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut answers = load(dir.path());
        assert!(!trusted(&answers, Path::new("/work/app")));
        answers.trusted_folders.push("/work".into());
        answers.bypass_accepted = true;
        save(dir.path(), &answers).unwrap();
        let reloaded = load(dir.path());
        assert!(trusted(&reloaded, Path::new("/work/app/src")));
        assert!(!trusted(&reloaded, Path::new("/other")));
        assert!(reloaded.bypass_accepted);
    }
}
