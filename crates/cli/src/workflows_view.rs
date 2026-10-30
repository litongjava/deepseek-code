//! `/workflows`, as in the reference CLI: browse this session's running and
//! completed workflow runs, watch one's phases and agents as they progress,
//! and stop it. Runs live in `<data-dir>/workflows/<session>/<run-id>/`:
//! `run.json` from the Workflow tool, `progress.jsonl` from the runner and
//! `status.json` once it ends.
use anyhow::Result;
use deepseek_code_core::runtime::RuntimePaths;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub label: String,
    pub phase: String,
    /// running, done, cached or failed.
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub dir: PathBuf,
    pub name: String,
    pub description: String,
    pub task_id: String,
    pub started_ms: i64,
    pub status: String,
    pub error: Option<String>,
    pub phase: Option<String>,
    pub agents: Vec<Agent>,
    pub logs: Vec<String>,
}

impl Run {
    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "running" | "unresponsive")
    }

    fn counts(&self) -> String {
        let finished = self.agents.iter().filter(|agent| matches!(agent.state.as_str(), "done" | "cached")).count();
        let failed = self.agents.iter().filter(|agent| agent.state == "failed").count();
        let mut text = format!("{finished}/{} agents done", self.agents.len());
        if failed > 0 { text.push_str(&format!(", {failed} failed")); }
        text
    }

    /// One line for the list.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.status.clone()];
        if let Some(phase) = &self.phase { parts.push(format!("phase {phase}")); }
        parts.push(self.counts());
        parts.push(ago(self.started_ms));
        parts.join(" \u{b7} ")
    }

    /// The details view: phases, agents and the latest log lines.
    pub fn details(&self) -> Vec<String> {
        let mut rows = vec![format!("{} \u{b7} {}", self.id, self.summary())];
        if !self.description.is_empty() { rows.push(self.description.clone()); }
        if let Some(error) = &self.error { rows.push(format!("Error: {error}")); }
        rows.push(String::new());
        let mut phases: Vec<&str> = Vec::new();
        for agent in &self.agents { if !phases.contains(&agent.phase.as_str()) { phases.push(&agent.phase); } }
        for phase in phases {
            if !phase.is_empty() { rows.push(format!("{phase}:")); }
            for agent in self.agents.iter().filter(|agent| agent.phase == phase) {
                let mark = match agent.state.as_str() { "done" => "\u{2713}", "cached" => "\u{21bb}", "failed" => "\u{2717}", "stopped" => "\u{25a0}", _ => "\u{25cf}" };
                rows.push(format!("  {mark} {} ({})", agent.label, agent.state));
            }
        }
        if self.agents.is_empty() { rows.push("No agents yet.".into()); }
        if !self.logs.is_empty() {
            rows.push(String::new());
            rows.extend(self.logs.iter().rev().take(5).rev().map(|line| format!("log: {line}")));
        }
        rows
    }
}

fn ago(started_ms: i64) -> String {
    let seconds = (chrono::Utc::now().timestamp_millis() - started_ms).max(0) / 1000;
    match seconds {
        0..=59 => format!("started {seconds}s ago"),
        60..=3599 => format!("started {}m ago", seconds / 60),
        _ => format!("started {}h ago", seconds / 3600),
    }
}

/// Phase, agents and logs from a run's progress events.
fn progress(dir: &Path) -> (Option<String>, Vec<Agent>, Vec<String>) {
    let (mut phase, mut agents, mut logs) = (None, Vec::<Agent>::new(), Vec::new());
    let text = std::fs::read_to_string(dir.join("progress.jsonl")).unwrap_or_default();
    for event in text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
        let label = event["label"].as_str().unwrap_or_default().to_owned();
        let agent_phase = event["phase"].as_str().unwrap_or_default().to_owned();
        let state = match event["kind"].as_str().unwrap_or_default() {
            "phase" => { phase = event["title"].as_str().map(str::to_owned); continue }
            "log" => { logs.push(event["text"].as_str().unwrap_or_default().to_owned()); continue }
            "agent_start" => "running",
            "agent_done" => "done",
            "agent_cached" => "cached",
            "agent_failed" => "failed",
            _ => continue,
        };
        // A finished event updates the oldest running agent with that label.
        match agents.iter_mut().find(|agent| agent.label == label && agent.phase == agent_phase && agent.state == "running") {
            Some(agent) if state != "running" => agent.state = state.into(),
            _ => agents.push(Agent { label, phase: agent_phase, state: state.into() }),
        }
    }
    (phase, agents, logs)
}

fn task_status(paths: &RuntimePaths, session: &str, task_id: &str) -> String {
    let Ok(task) = deepseek_code_core::tools::tasks::execute_for_session("TaskGet", json!({"taskId": task_id}), &paths.data_dir, session) else { return "stopped".into() };
    match deepseek_code_core::tools::background_shell::task_status(&paths.data_dir, session, &task).unwrap_or_default().as_str() {
        "running" => "running".into(),
        "unresponsive" => "unresponsive".into(),
        "" | "unknown" => "stopped".into(),
        // Anything that ended without status.json was stopped or crashed.
        _ => "stopped".into(),
    }
}

/// This session's runs, newest first.
pub fn runs(paths: &RuntimePaths, session: &str) -> Vec<Run> {
    let root = paths.data_dir.join("workflows").join(session);
    let mut runs: Vec<Run> = std::fs::read_dir(&root).into_iter().flatten().flatten().filter_map(|entry| {
        let dir = entry.path();
        let meta: Value = serde_json::from_slice(&std::fs::read(dir.join("run.json")).ok()?).ok()?;
        let ended: Option<Value> = std::fs::read(dir.join("status.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let task_id = meta["task_id"].as_str().unwrap_or_default().to_owned();
        let status = match &ended {
            Some(ended) => ended["status"].as_str().unwrap_or("completed").to_owned(),
            None => task_status(paths, session, &task_id),
        };
        let (phase, mut agents, logs) = progress(&dir);
        // Agents still marked running in a run that ended were cut off.
        if !matches!(status.as_str(), "running" | "unresponsive") {
            for agent in agents.iter_mut().filter(|agent| agent.state == "running") { agent.state = "stopped".into(); }
        }
        Some(Run {
            id: meta["run_id"].as_str().unwrap_or_default().to_owned(),
            name: meta["name"].as_str().unwrap_or("workflow").to_owned(),
            description: meta["description"].as_str().unwrap_or_default().to_owned(),
            started_ms: meta["started_ms"].as_i64().unwrap_or_default(),
            error: ended.as_ref().and_then(|ended| ended["error"].as_str().map(str::to_owned)),
            task_id, status, phase, agents, logs, dir,
        })
    }).collect();
    runs.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
    runs
}

fn stop(paths: &RuntimePaths, session: &str, run: &Run) -> Result<()> {
    deepseek_code_core::tools::tasks::execute_for_session("TaskStop", json!({"task_id": run.task_id}), &paths.data_dir, session)?;
    // Recorded at once, so the list does not show it running meanwhile.
    std::fs::write(run.dir.join("status.json"), json!({"status":"stopped","finished_ms":chrono::Utc::now().timestamp_millis()}).to_string())?;
    Ok(())
}

/// Plain text for terminals without a picker.
pub fn print(paths: &RuntimePaths, session: &str) {
    let runs = runs(paths, session);
    if runs.is_empty() {
        eprintln!("No workflow runs in this session.");
    }
    for run in runs {
        eprintln!("{}  {}  {}", run.id, run.name, run.summary());
    }
}

/// The interactive browser: pick a run, watch it, stop it with `s`.
pub fn browse(paths: &RuntimePaths, session: &str) -> Result<()> {
    use crate::terminal::picker::{details_live, Entry, Picker};
    let mut selected = 0;
    loop {
        let list = runs(paths, session);
        if list.is_empty() {
            eprintln!("No workflow runs in this session. Ask for a workflow, or start a saved one with /workflows run NAME.");
            return Ok(());
        }
        let entries: Vec<Entry> = list.iter().map(|run| Entry { title: run.name.clone(), detail: run.summary() }).collect();
        let picker = Picker { heading: "Workflows", subtitle: Some("Running and completed workflows in this session"), entries: &entries, search: false, hint: "Enter to view \u{b7} Esc to close", selected };
        let Some(index) = picker.run()? else { return Ok(()) };
        selected = index;
        let (id, dir) = (list[index].id.clone(), list[index].dir.clone());
        let current = || runs(paths, session).into_iter().find(|run| run.dir == dir);
        let hint = |run: &Run| if run.running() { "s to stop \u{b7} \u{2190} back \u{b7} Esc to close" } else { "\u{2190} back \u{b7} Esc to close" };
        let first = current();
        let heading = format!("Workflow {}", first.as_ref().map_or(id.as_str(), |run| run.name.as_str()));
        let hint_text = first.as_ref().map(hint).unwrap_or("Esc to close");
        let mut rows = || current().map(|run| run.details()).unwrap_or_else(|| vec!["This run is gone.".into()]);
        match details_live(&heading, &mut rows, hint_text, &['s'])? {
            Some('s') => {
                if let Some(run) = current().filter(Run::running) {
                    stop(paths, session, &run)?;
                    eprintln!("Stopped workflow {} ({}).", run.name, run.id);
                }
            }
            Some('<') => continue,
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_events_build_the_list_and_details() {
        let data = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(data.path().to_path_buf()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let dir = data.path().join("workflows").join(&session).join("wf_abc123def456");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.json"), json!({"run_id":"wf_abc123def456","task_id":"none","name":"review","description":"Review the diff","started_ms":chrono::Utc::now().timestamp_millis()}).to_string()).unwrap();
        for event in [
            json!({"kind":"phase","title":"Review"}),
            json!({"kind":"agent_start","label":"review:security","phase":"Review"}),
            json!({"kind":"agent_start","label":"review:style","phase":"Review"}),
            json!({"kind":"agent_done","label":"review:security","phase":"Review"}),
            json!({"kind":"agent_failed","label":"review:style","phase":"Review","error":"boom"}),
            json!({"kind":"phase","title":"Verify"}),
            json!({"kind":"agent_cached","label":"verify:a","phase":"Verify"}),
            json!({"kind":"log","text":"two findings"}),
            json!({"kind":"agent_start","label":"verify:b","phase":"Verify"}),
        ] { crate::workflow_runner::record(&dir, event); }
        let list = runs(&paths, &session);
        assert_eq!(list.len(), 1);
        let run = &list[0];
        assert_eq!(run.phase.as_deref(), Some("Verify"));
        assert!(run.summary().contains("2/4 agents done, 1 failed"), "{}", run.summary());
        let details = run.details().join("\n");
        assert!(details.contains("Review:\n  \u{2713} review:security (done)\n  \u{2717} review:style (failed)"), "{details}");
        assert!(details.contains("\u{21bb} verify:a (cached)") && details.contains("log: two findings"));
        // No status.json and no live task: the run was stopped.
        assert_eq!(run.status, "stopped");
        // The run ended without status.json, so its unfinished agent was cut off.
        assert!(details.contains("\u{25a0} verify:b (stopped)"), "{details}");
        std::fs::write(dir.join("status.json"), json!({"status":"completed"}).to_string()).unwrap();
        assert_eq!(runs(&paths, &session)[0].status, "completed");
    }
}
