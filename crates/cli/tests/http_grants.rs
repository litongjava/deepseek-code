use deepseek_code_core::permissions::http_grants::{grant, lookup, GrantScope};
use serde_json::Value;
use std::process::Command;

#[test]
fn list_and_revoke_saved_http_grant_by_id() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let origin = "http://example.org";
    grant(
        data.path(),
        workspace.path(),
        "s",
        origin,
        GrantScope::Project,
    )?;
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dcl"))
            .arg("--data-dir")
            .arg(data.path())
            .args(["config", "http-grants"])
            .args(args)
            .current_dir(workspace.path())
            .output()
    };
    let listed = run(&["list"])?;
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&listed.stdout)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["scope"], "project");
    assert_eq!(rows[0]["origin"], origin);
    let id = rows[0]["id"].as_i64().unwrap().to_string();
    assert!(run(&["revoke", &id])?.status.success());
    assert_eq!(lookup(data.path(), workspace.path(), "s", origin)?, None);
    assert!(!run(&["revoke", &id])?.status.success());
    Ok(())
}
