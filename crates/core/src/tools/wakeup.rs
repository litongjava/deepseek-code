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
        summary: Some("Ask the runtime to wake this /loop session again later".into()),
        name: "ScheduleWakeup".into(),
        description: "Ask the runtime to wake this /loop session again later. Save the loop input in prompt each time: when the delay expires and the session is idle, that input fires once, so pass the same text to keep the loop going. delaySeconds is clamped to 60-3600 seconds; choose it from how fast the thing you watch changes. reason is a short note shown to the user. Set noop to true when the check found nothing new and false when it did. To end the loop, send stop: true alone; the saved wakeup is removed and no other field is used.".into(),
        input_schema: json!({"type":"object","properties":{
            "delaySeconds":{"description":"Seconds until the wakeup; values outside 60-3600 are clamped. Required unless stop is true.","type":"number"},
            "reason":{"description":"Short note for the user explaining this delay. Required unless stop is true.","type":"string"},
            "prompt":{"description":"The /loop input to replay when the wakeup fires; reuse the same text each turn.","type":"string"},
            "stop":{"description":"True ends the dynamic loop and deletes the saved wakeup; every other field is discarded.","type":"boolean"},
            "noop":{"description":"True when the check found nothing new, false when it found something to keep.","type":"boolean"}
        },"additionalProperties":false}),
    }
}

fn file(root: &Path, session: &str) -> PathBuf {
    root.join("tasks/wakeups").join(format!("{session}.json"))
}

pub fn execute(input: &Value, root: &Path, session: &str, now: i64) -> Result<Value> {
    let path = file(root, session);
    if input["stop"] == true {
        let _ = std::fs::remove_file(&path);
        return Ok(
            json!({"stopped":true,"content":"The dynamic loop has ended; no further wakeups are scheduled."}),
        );
    }
    let prompt = input["prompt"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .context("prompt is required unless stop is true")?;
    let reason = input["reason"]
        .as_str()
        .filter(|r| !r.trim().is_empty())
        .context("reason is required unless stop is true")?;
    let delay = input["delaySeconds"]
        .as_f64()
        .filter(|d| d.is_finite())
        .context("delaySeconds is required unless stop is true")?;
    let delay = delay.clamp(MIN_DELAY_SECS, MAX_DELAY_SECS) as i64;
    std::fs::create_dir_all(path.parent().context("wakeup directory")?)?;
    std::fs::write(
        &path,
        serde_json::to_vec(
            &json!({"at":now + delay,"prompt":prompt,"reason":reason,"noop":input["noop"]}),
        )?,
    )?;
    Ok(json!({"delaySeconds":delay,"content":format!("Next wakeup in {delay}s: {reason}")}))
}

pub fn is_due(root: &Path, session: &str, now: i64) -> bool {
    std::fs::read(file(root, session))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|saved| saved["at"].as_i64())
        .is_some_and(|at| at <= now)
}

/// The saved /loop input once its time has come; it fires once.
pub fn take_due(root: &Path, session: &str, now: i64) -> Result<Option<(String, String)>> {
    let path = file(root, session);
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(None);
    };
    let saved: Value = serde_json::from_slice(&bytes)?;
    if saved["at"].as_i64().unwrap_or(i64::MAX) > now {
        return Ok(None);
    }
    std::fs::remove_file(&path)?;
    Ok(saved["prompt"].as_str().map(|prompt| {
        (
            prompt.to_owned(),
            saved["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeups_clamp_fire_once_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let input =
            json!({"delaySeconds":5,"reason":"watching CI","prompt":"/loop check CI","noop":true});
        assert_eq!(
            execute(&input, root.path(), "s", 1000).unwrap()["delaySeconds"],
            60
        );
        assert!(take_due(root.path(), "s", 1059).unwrap().is_none());
        assert!(!is_due(root.path(), "s", 1059) && is_due(root.path(), "s", 1060));
        assert_eq!(
            take_due(root.path(), "s", 1060).unwrap().unwrap().0,
            "/loop check CI"
        );
        assert!(take_due(root.path(), "s", 2000).unwrap().is_none());
        execute(
            &json!({"delaySeconds":99999,"reason":"r","prompt":"p"}),
            root.path(),
            "s",
            0,
        )
        .unwrap();
        execute(&json!({"stop":true}), root.path(), "s", 0).unwrap();
        assert!(take_due(root.path(), "s", 10_000).unwrap().is_none());
        assert!(execute(&json!({"delaySeconds":60}), root.path(), "s", 0).is_err());
    }
}
