//! Bash adapter only: retain the existing command construction and PTY owner.
use anyhow::{ensure, Context, Result};
use serde_json::Value;

pub(super) fn validate(input: &Value) -> Result<Option<(u16, u16)>> {
    if input["interaction"] != "pty" {
        ensure!(
            input.get("rows").is_none() && input.get("cols").is_none(),
            "Bash rows/cols require interaction:\"pty\""
        );
        return Ok(None);
    }
    ensure!(cfg!(target_os = "linux"),
        "Bash interaction:\"pty\" is supported only on Linux; Windows/ConPTY and macOS are not implemented");
    ensure!(
        input["run_in_background"] == true && input["detach"] != true,
        "Bash interaction:\"pty\" requires run_in_background:true and cannot detach"
    );
    ensure!(!input["__sandbox"].is_object(),
        "Bash interaction:\"pty\" does not support the configured sandbox yet; no process was started");
    ensure!(input.get("timeout").is_none_or(Value::is_null),
        "Bash interaction:\"pty\" does not support a process timeout yet; use TaskGet timeout for bounded reads and TaskStop for termination");
    ensure!(
        input.get("__monitor_timeout_ms").is_none(),
        "Monitor does not support PTY"
    );
    let dimension = |key: &str, default: u64, max: u64| -> Result<u16> {
        Ok(match input.get(key) {
            None => default,
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=max).contains(n))
                .with_context(|| format!("PTY {key} must be an integer in 1..={max}"))?,
        } as u16)
    };
    Ok(Some((
        dimension("rows", 24, 500)?,
        dimension("cols", 80, 1000)?,
    )))
}

#[cfg(target_os = "linux")]
pub(super) fn start(
    command: tokio::process::Command,
    rows: u16,
    cols: u16,
    description: &str,
    command_text: &str,
) -> Result<Value> {
    let (root, session) = super::background_shell::CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .context("Background Bash requires an owning engine session")?;
    // The normal Bash builder inherits its environment, then applies explicit
    // overrides/removals (including removal of worker-private variables). Use
    // those exact program/argv/cwd values, never reconstruct from model text.
    let command = command.as_std();
    let mut environment: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => {
                environment.insert(key.to_owned(), value.to_owned());
            }
            None => {
                environment.remove(key);
            }
        }
    }
    let spec = super::pty_transport::SpawnSpec {
        executable: command.get_program().to_owned(),
        arguments: command.get_args().map(ToOwned::to_owned).collect(),
        cwd: command
            .get_current_dir()
            .context("PTY command cwd is missing")?
            .to_owned(),
        environment: environment.into_iter().collect(),
        rows,
        cols,
    };
    let (value, guard) = super::pty_owner::start(spec, &root, &session, description, command_text)?;
    // The owner already contributes to background_shell::has_active. Retain
    // its guard across turns until real completion. Runtime shutdown drops the
    // guard, executing the existing bounded session cleanup. No second driver.
    tokio::spawn(async move {
        while !guard.is_finished() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        drop(guard);
    });
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dimensions_require_pty() {
        assert!(validate(&json!({"command":"true","rows":24})).is_err());
        assert!(validate(&json!({"interaction":"pipe","cols":80})).is_err());
        assert_eq!(validate(&json!({"interaction":"pipe"})).unwrap(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_launch_validates_all_options_before_creating_a_task() {
        let base = json!({"interaction":"pty","run_in_background":true});
        assert_eq!(validate(&base).unwrap(), Some((24, 80)));
        for (key, values) in [
            ("rows", vec![json!(0), json!(501), json!(1.5), json!(null)]),
            ("cols", vec![json!(0), json!(1001), json!("80")]),
            ("run_in_background", vec![json!(false)]),
            ("detach", vec![json!(true)]),
            ("timeout", vec![json!(100)]),
            ("__sandbox", vec![json!({"enabled":true})]),
        ] {
            for value in values {
                let mut input = base.clone();
                input[key] = value;
                assert!(validate(&input).is_err(), "{input}");
            }
        }
        let mut limits = base;
        limits["rows"] = json!(500);
        limits["cols"] = json!(1000);
        assert_eq!(validate(&limits).unwrap(), Some((500, 1000)));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pty_is_explicitly_unsupported() {
        let error = validate(&json!({"interaction":"pty","run_in_background":true})).unwrap_err();
        assert!(error.to_string().contains("supported only on Linux"));
    }
}
