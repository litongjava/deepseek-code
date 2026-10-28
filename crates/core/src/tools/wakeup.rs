//! ScheduleWakeup, as in the reference CLI: a self-paced /loop asks to be
//! woken again. The interactive REPL re-runs the saved /loop input once the
//! time has come and the prompt is idle.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MIN_DELAY_SECS: f64 = 60.0;
const MAX_DELAY_SECS: f64 = 3600.0;

pub fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "ScheduleWakeup".into(),
        description: "Schedule when to resume work in /loop dynamic mode — the user invoked /loop without an interval, asking you to self-pace iterations of a specific task.\n\nDo NOT schedule a short-interval wakeup to poll for background work you started — when tracked work finishes, you are woken automatically, so polling is wasted. Instead schedule a long fallback (1200s+) so the loop survives if the work hangs. The exception is external work the runtime cannot track (a CI run, a deploy, a remote queue) — there, pick a delay matched to how fast that state actually changes.\n\nPass the same /loop input back via `prompt` each turn so the next firing repeats the task. To end the loop, call this tool with `stop: true` (omit every other field).\n\nSet `noop: true` if nothing changed — you checked and there's nothing to report. Set `noop: false` if something happened worth keeping. Omit `noop` when stopping.\n\n## Picking delaySeconds\n\nThe runtime clamps the delay to [60, 3600]. Match it to what you're actually waiting for: a CI run that takes ~8 minutes deserves one ~480s check, not eight 60s ones. For idle ticks with no specific signal to watch, default to 1200s–1800s.\n\n## The reason field\n\nOne short sentence on what you chose and why; it is shown to the user. \"watching CI run\" beats \"waiting.\"".into(),
        input_schema: json!({"type":"object","properties":{
            "delaySeconds":{"description":"Seconds from now to wake up. Clamped to [60, 3600] by the runtime. Required unless `stop` is true.","type":"number"},
            "reason":{"description":"One short sentence explaining the chosen delay. Shown to the user. Be specific. Required unless `stop` is true.","type":"string"},
            "prompt":{"description":"The /loop input to fire on wake-up. Pass the same /loop input verbatim each turn so the next firing continues the loop. Required unless `stop` is true.","type":"string"},
            "stop":{"description":"Set to true to end the dynamic loop immediately instead of scheduling another wakeup. When true, all other fields are ignored and no further wakeups fire.","type":"boolean"},
            "noop":{"description":"true = nothing changed (you checked and there is nothing to report). false = something happened worth keeping. Required unless `stop` is true.","type":"boolean"}
        },"additionalProperties":false}),
    }
}

fn file(root: &Path, session: &str) -> PathBuf {
    root.join("wakeups").join(format!("{session}.json"))
}

pub fn execute(input: &Value, root: &Path, session: &str, now: i64) -> Result<Value> {
    let path = file(root, session);
    if input["stop"] == true {
        let _ = std::fs::remove_file(&path);
        return Ok(json!({"stopped":true,"content":"The dynamic loop has ended; no further wakeups are scheduled."}));
    }
    let prompt = input["prompt"].as_str().filter(|p| !p.trim().is_empty()).context("prompt is required unless stop is true")?;
    let reason = input["reason"].as_str().filter(|r| !r.trim().is_empty()).context("reason is required unless stop is true")?;
    let delay = input["delaySeconds"].as_f64().filter(|d| d.is_finite()).context("delaySeconds is required unless stop is true")?;
    let delay = delay.clamp(MIN_DELAY_SECS, MAX_DELAY_SECS) as i64;
    std::fs::create_dir_all(path.parent().context("wakeup directory")?)?;
    std::fs::write(&path, serde_json::to_vec(&json!({"at":now + delay,"prompt":prompt,"reason":reason,"noop":input["noop"]}))?)?;
    Ok(json!({"delaySeconds":delay,"content":format!("Next wakeup in {delay}s: {reason}")}))
}

pub fn is_due(root: &Path, session: &str, now: i64) -> bool {
    std::fs::read(file(root, session)).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).and_then(|saved| saved["at"].as_i64()).is_some_and(|at| at <= now)
}

/// The saved /loop input once its time has come; it fires once.
pub fn take_due(root: &Path, session: &str, now: i64) -> Result<Option<(String, String)>> {
    let path = file(root, session);
    let Ok(bytes) = std::fs::read(&path) else { return Ok(None) };
    let saved: Value = serde_json::from_slice(&bytes)?;
    if saved["at"].as_i64().unwrap_or(i64::MAX) > now { return Ok(None); }
    std::fs::remove_file(&path)?;
    Ok(saved["prompt"].as_str().map(|prompt| (prompt.to_owned(), saved["reason"].as_str().unwrap_or_default().to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeups_clamp_fire_once_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let input = json!({"delaySeconds":5,"reason":"watching CI","prompt":"/loop check CI","noop":true});
        assert_eq!(execute(&input, root.path(), "s", 1000).unwrap()["delaySeconds"], 60);
        assert!(take_due(root.path(), "s", 1059).unwrap().is_none());
        assert!(!is_due(root.path(), "s", 1059) && is_due(root.path(), "s", 1060));
        assert_eq!(take_due(root.path(), "s", 1060).unwrap().unwrap().0, "/loop check CI");
        assert!(take_due(root.path(), "s", 2000).unwrap().is_none());
        execute(&json!({"delaySeconds":99999,"reason":"r","prompt":"p"}), root.path(), "s", 0).unwrap();
        execute(&json!({"stop":true}), root.path(), "s", 0).unwrap();
        assert!(take_due(root.path(), "s", 10_000).unwrap().is_none());
        assert!(execute(&json!({"delaySeconds":60}), root.path(), "s", 0).is_err());
    }
}
