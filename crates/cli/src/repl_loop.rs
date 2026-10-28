//! /loop as in the reference CLI: repeat a prompt or command on an interval
//! while the session is open.
use anyhow::{bail, ensure, Result};
use std::time::{Duration, Instant};

pub type Loop = (Duration, String, Instant);

/// `30s`, `5m`, `2h` or `1d`.
fn interval(text: &str) -> Option<Duration> {
    let (number, unit) = text.split_at(text.find(|ch: char| !ch.is_ascii_digit())?);
    let count: u64 = number.parse().ok()?;
    let seconds = match unit {
        "s" => count,
        "m" => count * 60,
        "h" => count * 3600,
        "d" => count * 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

fn describe(every: Duration) -> String {
    let seconds = every.as_secs();
    match seconds {
        _ if seconds % 86_400 == 0 => format!("{}d", seconds / 86_400),
        _ if seconds % 3600 == 0 => format!("{}h", seconds / 3600),
        _ if seconds % 60 == 0 => format!("{}m", seconds / 60),
        _ => format!("{seconds}s"),
    }
}

/// Handles `/loop [INTERVAL] PROMPT`, `/loop` (list) and `/loop stop`.
pub fn command(argument: &str, loops: &mut Vec<Loop>) -> Result<String> {
    let argument = argument.trim();
    if argument.is_empty() || argument == "list" {
        if loops.is_empty() {
            return Ok("No loops running. Usage: /loop 5m PROMPT (or a /command); /loop stop ends them.".into());
        }
        return Ok(loops
            .iter()
            .enumerate()
            .map(|(index, (every, prompt, next))| format!("{}. every {} \u{b7} {prompt} \u{b7} next in {}s", index + 1, describe(*every), next.saturating_duration_since(Instant::now()).as_secs()))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    if let Some(which) = argument.strip_prefix("stop") {
        let which = which.trim();
        if which.is_empty() {
            let count = loops.len();
            loops.clear();
            return Ok(format!("Stopped {count} loop{}.", if count == 1 { "" } else { "s" }));
        }
        let index: usize = which.parse().ok().filter(|index| (1..=loops.len()).contains(index)).ok_or_else(|| anyhow::anyhow!("No loop {which}; /loop lists them"))?;
        let (_, prompt, _) = loops.remove(index - 1);
        return Ok(format!("Stopped the loop for: {prompt}"));
    }
    let (first, rest) = argument.split_once(char::is_whitespace).unwrap_or((argument, ""));
    let (every, prompt) = match interval(first) {
        Some(every) => (every, rest.trim()),
        None => (Duration::from_secs(600), argument),
    };
    ensure!(!prompt.is_empty(), "Usage: /loop 5m PROMPT");
    ensure!(every >= Duration::from_secs(30), "Loops run at most every 30 seconds");
    if prompt.starts_with("/loop") {
        bail!("A loop cannot start another loop");
    }
    loops.push((every, prompt.to_owned(), Instant::now()));
    Ok(format!("Looping every {}: {prompt}\nIt runs now and while the session stays open; /loop stop ends it.", describe(every)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loops_start_list_and_stop() {
        let mut loops = Vec::new();
        assert!(command("5m check the build", &mut loops).unwrap().contains("every 5m"));
        assert_eq!(loops[0].0, Duration::from_secs(300));
        assert!(command("summarize new issues", &mut loops).unwrap().contains("every 10m"));
        assert!(command("", &mut loops).unwrap().contains("2. every 10m"));
        assert!(command("10s too fast", &mut loops).is_err());
        assert!(command("stop 1", &mut loops).unwrap().contains("check the build"));
        assert!(command("stop", &mut loops).unwrap().contains("Stopped 1 loop."));
        assert!(loops.is_empty());
    }
}
