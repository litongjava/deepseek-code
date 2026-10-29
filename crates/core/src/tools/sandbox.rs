//! Bash sandboxing, as in the reference CLI: bubblewrap on Linux and
//! sandbox-exec on macOS keep commands from writing outside the project
//! (plus temp and configured paths) and can cut network access. Enabled by
//! `"sandbox": {"enabled": true}` in settings.json.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The effective `sandbox` settings: user, then project, then local.
pub fn settings(data_dir: &Path, workspace: &Path) -> Value {
    let mut merged = json!({});
    for source in [data_dir.join("settings.json"), workspace.join(".claude/settings.json"), workspace.join(".claude/settings.local.json")] {
        let Some(sandbox) = std::fs::read(&source).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).map(|value| value["sandbox"].clone()) else { continue };
        if let (Some(target), Some(layer)) = (merged.as_object_mut(), sandbox.as_object()) {
            for (key, value) in layer { target.insert(key.clone(), value.clone()); }
        }
    }
    merged
}

/// What the tool receives when the sandbox is on: writable paths and network.
pub fn policy(settings: &Value, workspace: &Path, extra_writable: &[PathBuf]) -> Option<Value> {
    if settings["enabled"] != true { return None; }
    let mut writable: Vec<String> = vec![workspace.to_string_lossy().into_owned(), std::env::temp_dir().to_string_lossy().into_owned()];
    writable.extend(extra_writable.iter().map(|path| path.to_string_lossy().into_owned()));
    writable.extend(settings["filesystem"]["allowWrite"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
    Some(json!({
        "writable": writable,
        "network": settings["network"] != "deny",
        "allow_unsandboxed": settings["allowUnsandboxedCommands"] != false,
    }))
}

/// The program and leading arguments that run `shell` inside the sandbox.
pub fn wrap(policy: &Value, shell: &str) -> Result<(String, Vec<String>)> {
    let writable: Vec<&str> = policy["writable"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|path| Path::new(path).exists()).collect();
    let network = policy["network"] != false;
    if cfg!(target_os = "linux") {
        let Some(bwrap) = super::lsp_servers::find_on_path("bwrap") else {
            bail!("The sandbox is enabled but bubblewrap (bwrap) is not installed; install it or set sandbox.enabled to false");
        };
        let mut args: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--die-with-parent"].map(String::from).to_vec();
        for path in writable { args.extend(["--bind".into(), path.into(), path.into()]); }
        if !network { args.push("--unshare-net".into()); }
        args.extend(["--".into(), shell.into()]);
        return Ok((bwrap.to_string_lossy().into_owned(), args));
    }
    if cfg!(target_os = "macos") {
        let quote = |path: &str| format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""));
        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write* (literal \"/dev/null\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\")");
        for path in writable { profile.push_str(&format!(" (subpath {})", quote(path))); }
        profile.push_str(")\n");
        if !network { profile.push_str("(deny network*)\n"); }
        return Ok(("/usr/bin/sandbox-exec".into(), vec!["-p".into(), profile, shell.into()]));
    }
    bail!("The sandbox is enabled but is not supported on this platform; set sandbox.enabled to false")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn settings_merge_and_policy_lists_writable_paths() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".claude")).unwrap();
        std::fs::write(data.path().join("settings.json"), r#"{"sandbox":{"enabled":true,"network":"deny"}}"#).unwrap();
        std::fs::write(workspace.path().join(".claude/settings.local.json"), r#"{"sandbox":{"filesystem":{"allowWrite":["/var/cache/x"]}}}"#).unwrap();
        let merged = settings(data.path(), workspace.path());
        let policy = policy(&merged, workspace.path(), &[]).unwrap();
        assert_eq!(policy["network"], false);
        assert_eq!(policy["allow_unsandboxed"], true);
        assert!(policy["writable"].as_array().unwrap().iter().any(|p| p == "/var/cache/x"));
        assert!(super::policy(&json!({"enabled":false}), workspace.path(), &[]).is_none());
    }

    #[tokio::test]
    async fn sandboxed_commands_write_only_to_allowed_paths_and_can_lose_network() {
        if super::super::lsp_servers::find_on_path("bwrap").is_none() { return; }
        crate::config::OrchestrationConfig::init_global().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let cwd = workspace.path().to_str().unwrap();
        let policy = policy(&json!({"enabled":true,"network":"deny"}), workspace.path(), &[]).unwrap();
        let run = |command: String| super::super::tool_bash_async(json!({"command":command,"__sandbox":policy.clone()}), cwd);
        let inside = run("echo ok > inside.txt && cat inside.txt".into()).await.unwrap();
        assert_eq!(inside["stdout"].as_str().unwrap().trim(), "ok");
        let blocked = run(format!("echo no > {}/x.txt", outside.path().display())).await;
        assert!(blocked.is_err() || blocked.unwrap()["exit_code"] != 0);
        assert!(!outside.path().join("x.txt").exists());
        let escaped = super::super::tool_bash_async(json!({"command":format!("echo yes > {}/y.txt", outside.path().display()),"__sandbox":policy.clone(),"dangerouslyDisableSandbox":true}), cwd).await.unwrap();
        assert_eq!(escaped["exit_code"], 0);
        assert!(outside.path().join("y.txt").exists());
    }
}
