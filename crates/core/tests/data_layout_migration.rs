//! End-to-end checks for the v1 -> v2 data-directory migration.
//!
//! The store is built once under the current layout, then its files are moved
//! back to the v1 names. That gives the migration real SQLite shards and blobs
//! to move without duplicating the engine's bootstrap code in the test.

use anyhow::Result;
use deepseek_code_core::data_layout::{DataLayout, LayoutStatus, LAYOUT_VERSION, ROOT_ALLOWLIST};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
use std::fs;
use std::path::Path;

fn reverse(from: &Path, to: &Path) -> Result<()> {
    if !from.try_exists()? {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)?;
    Ok(())
}

/// Move every v2 location the fixture touched back to its v1 name.
fn restore_v1_names(data: &Path) -> Result<()> {
    for (from, to) in [
        ("data/sessions", "sessions"),
        ("data/workspaces", "workspaces"),
        ("data/global-blobs", "global-blobs"),
        ("data/backups", "backups"),
        ("state/locks/session", "session-locks"),
        ("state/locks/workspace", "workspace-locks"),
        (
            "state/locks/publication.lock",
            "background-task-publication.lock",
        ),
        ("state/bootstrap", ".session-bootstrap"),
        ("state/staging", ".global-staging"),
        ("state/presence", "session-presence"),
        ("state/workers", "session-workers"),
        ("state/turn-completions", "turn-completions"),
        ("state/shell-cwd", "shell-cwd"),
        ("state/session-env", "session-env"),
        ("tasks/background", "background-tasks"),
        ("tasks/shell", "shell-tasks"),
        ("tasks/detached-shells", "detached-shells"),
        ("tasks/checkpoints", "checkpoints"),
        ("tasks/plans", "plans"),
        ("tasks/turn-context", "turn-context"),
        ("tasks/design-plans", "design-plans"),
        ("cache/environment", "environment"),
        ("cache/tool-outputs", "tool-outputs"),
        ("cache/tool-output-metadata", "tool-output-metadata"),
        ("cache/mcp-tool-snapshots", "mcp-tool-snapshots"),
        ("cache/git-status", "git-status"),
        ("diagnostics/request-records", "request-records"),
        ("diagnostics/logs", "logs"),
        ("diagnostics/design-conversions", "design-conversions"),
        ("resources/lsp", "lsp"),
        ("config/settings.json", "settings.json"),
        ("config/config.json", "config.json"),
        ("config/customization.json", "customization.json"),
    ] {
        reverse(&data.join(from), &data.join(to))?;
    }
    for file in ["layout.json", "README.md"] {
        let path = data.join(file);
        if path.try_exists()? {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[test]
fn v1_directory_migrates_and_session_data_stays_readable() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let project = temp.path().join("project");
    fs::create_dir(&project)?;

    let session_id = {
        let db = RuntimePaths::new(data.clone())?.open_database()?;
        let store = SessionStore::new(db);
        let id = store.create(&project, "fixture-model", "fixture-provider")?;
        store.append(&id, "user", "migration sentinel")?;
        store.append(&id, "assistant", "migration reply")?;
        id
    };

    // Files that migration must carry across category boundaries.
    fs::create_dir_all(data.join("data/global-blobs"))?;
    fs::write(data.join("data/global-blobs/blob-hash"), b"blob bytes")?;
    fs::create_dir_all(data.join("tasks/checkpoints"))?;
    fs::write(
        data.join("tasks/checkpoints/checkpoint.json"),
        b"checkpoint",
    )?;
    fs::create_dir_all(data.join("cache/environment"))?;
    fs::write(
        data.join("cache/environment/first-message.txt"),
        b"environment",
    )?;
    fs::create_dir_all(data.join("diagnostics/request-records").join(&session_id))?;
    fs::write(
        data.join("diagnostics/request-records")
            .join(&session_id)
            .join("request.json"),
        b"{}",
    )?;
    fs::create_dir_all(data.join("config"))?;
    fs::write(
        data.join("config/settings.json"),
        r#"{"toolDefault":"bash"}"#,
    )?;
    fs::write(data.join("config/config.json"), r#"{"providers":[]}"#)?;
    fs::create_dir_all(data.join("skills/data-dir-check"))?;
    fs::write(
        data.join("skills/data-dir-check/SKILL.md"),
        "---\nname: data-dir-check\ndescription: fixture\n---\n",
    )?;
    fs::write(data.join("my-script.sh"), "#!/bin/sh\n")?;

    restore_v1_names(&data)?;
    assert!(data.join("sessions").is_dir());
    assert!(!data.join("layout.json").exists());

    let layout = DataLayout::from_root(&data);
    let outcome = layout.migrate_if_needed()?;
    assert_eq!(outcome.status, LayoutStatus::Migrated);
    assert!(
        outcome.warnings.iter().any(|entry| entry == "my-script.sh"),
        "{:?}",
        outcome.warnings
    );

    let value: serde_json::Value = serde_json::from_slice(&fs::read(data.join("layout.json"))?)?;
    assert_eq!(value["version"], LAYOUT_VERSION);
    assert_eq!(value["state"], "ready");
    assert!(value.get("moves").is_none(), "{value}");

    assert!(data.join("data/sessions").is_dir());
    assert!(!data.join("sessions").exists());
    assert!(data.join("state/locks/session/.directory").is_file());
    assert_eq!(
        fs::read(data.join("data/global-blobs/blob-hash"))?,
        b"blob bytes"
    );
    assert_eq!(
        fs::read(data.join("tasks/checkpoints/checkpoint.json"))?,
        b"checkpoint"
    );
    assert_eq!(
        fs::read(data.join("cache/environment/first-message.txt"))?,
        b"environment"
    );
    assert!(data
        .join("diagnostics/request-records")
        .join(&session_id)
        .join("request.json")
        .is_file());
    assert_eq!(
        fs::read(data.join("config/settings.json"))?,
        br#"{"toolDefault":"bash"}"#
    );
    assert!(data.join("config/config.json").is_file());
    assert!(data.join("skills/data-dir-check/SKILL.md").is_file());
    assert!(data.join("my-script.sh").is_file());

    // Reopen through the normal entry point and read the session back.
    let db = RuntimePaths::new(data.clone())?.open_database()?;
    let messages = db.open_session(&session_id)?.read(|r| r.messages())?;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "migration sentinel");
    drop(db);

    // A second migration is a no-op and does not rewrite the ready marker.
    let before = fs::read(data.join("layout.json"))?;
    let again = layout.migrate_if_needed()?;
    assert_eq!(again.status, LayoutStatus::Ready);
    assert_eq!(fs::read(data.join("layout.json"))?, before);

    Ok(())
}

#[test]
fn fresh_store_root_entries_are_all_allowlisted() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let db = RuntimePaths::new(data.clone())?.open_database()?;
    drop(db);

    let mut unexpected = Vec::new();
    for entry in fs::read_dir(&data)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !ROOT_ALLOWLIST.contains(&name.as_str()) {
            unexpected.push(name);
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected root entries: {unexpected:?}"
    );
    Ok(())
}
