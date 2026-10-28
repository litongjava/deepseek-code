//! CronCreate, CronDelete and CronList, as in the reference CLI: 5-field cron in
//! local time, prompts enqueued while the REPL is idle. Session-only jobs live
//! under the runtime directory for one session; durable jobs are written to the
//! project's `.claude/scheduled_tasks.json` and are picked up by later sessions.
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const RECURRING_LIFETIME_SECS: i64 = 7 * 24 * 60 * 60;
const MAX_JOBS: usize = 50;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
    pub created_at: i64,
    pub next_run: i64,
}

pub fn definitions() -> Vec<super::ToolDefinition> {
    vec![
        super::ToolDefinition {
            name: "CronCreate".into(),
            description: "Schedule a prompt to be enqueued at a future time. Use for both recurring schedules and one-shot reminders.\n\nUses standard 5-field cron in the user's local timezone: minute hour day-of-month month day-of-week. \"0 9 * * *\" means 9am local — no timezone conversion needed.\n\n## One-shot tasks (recurring: false)\n\nFor \"remind me at X\" or \"at <time>, do Y\" requests — fire once then auto-delete.\nPin minute/hour/day-of-month/month to specific values:\n  \"remind me at 2:30pm today to check the deploy\" → cron: \"30 14 <today_dom> <today_month> *\", recurring: false\n  \"tomorrow morning, run the smoke test\" → cron: \"57 8 <tomorrow_dom> <tomorrow_month> *\", recurring: false\n\n## Recurring jobs (recurring: true, the default)\n\nFor \"every N minutes\" / \"every hour\" / \"weekdays at 9am\" requests:\n  \"*/5 * * * *\" (every 5 min), \"0 * * * *\" (hourly), \"0 9 * * 1-5\" (weekdays at 9am local)\n\n## Avoid the :00 and :30 minute marks when the task allows it\n\nWhen the user's request is approximate, pick a minute that is NOT 0 or 30, so scheduled requests do not all reach the API at the same instant:\n  \"every morning around 9\" → \"57 8 * * *\" or \"3 9 * * *\" (not \"0 9 * * *\")\n  \"hourly\" → \"7 * * * *\" (not \"0 * * * *\")\n\nOnly use minute 0 or 30 when the user names that exact time and clearly means it.\n\n## Durability\n\nBy default (durable: false) the job lives only in this session and is gone when the session ends. Pass durable: true to write to .claude/scheduled_tasks.json so the job survives restarts. Only use durable: true when the user explicitly asks for the task to persist.\n\n## Runtime behavior\n\nJobs only fire while the interactive session is idle (not mid-turn); the prompt then runs as if the user typed it. Recurring tasks auto-expire after 7 days — they fire one final time, then are deleted. Tell the user about the 7-day limit when scheduling recurring jobs. The result gives the job's next run time: tell the user that time.\n\nReturns a job ID you can pass to CronDelete.".into(),
            input_schema: json!({"type":"object","properties":{
                "cron":{"description":"Standard 5-field cron expression in local time: \"M H DoM Mon DoW\" (e.g. \"*/5 * * * *\" = every 5 minutes, \"30 14 28 2 *\" = Feb 28 at 2:30pm local once).","type":"string"},
                "prompt":{"description":"The prompt to enqueue at each fire time.","type":"string"},
                "recurring":{"description":"true (default) = fire on every cron match until deleted or auto-expired after 7 days. false = fire once at the next match, then auto-delete. Use false for \"remind me at X\" one-shot requests with pinned minute/hour/dom/month.","type":"boolean"},
                "durable":{"description":"true = persist to .claude/scheduled_tasks.json and survive restarts. false (default) = this session only. Use true only when the user asks the task to survive across sessions.","type":"boolean"}
            },"required":["cron","prompt"],"additionalProperties":false}),
        },
        super::ToolDefinition {
            name: "CronDelete".into(),
            description: "Cancel a cron job previously scheduled with CronCreate. Removes it from .claude/scheduled_tasks.json (durable jobs) or the session store (session-only jobs).".into(),
            input_schema: json!({"type":"object","properties":{"id":{"description":"Job ID returned by CronCreate.","type":"string"}},"required":["id"],"additionalProperties":false}),
        },
        super::ToolDefinition {
            name: "CronList".into(),
            description: "List all cron jobs scheduled via CronCreate, both durable (.claude/scheduled_tasks.json) and session-only.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
    ]
}

pub fn is_cron_tool(name: &str) -> bool {
    matches!(name, "CronCreate" | "CronDelete" | "CronList")
}

/// Where a session's jobs are kept.
pub struct Scope<'a> {
    pub root: &'a Path,
    pub session: &'a str,
    pub project: &'a Path,
}

impl Scope<'_> {
    fn session_file(&self) -> PathBuf {
        self.root.join("cron").join(format!("{}.json", self.session))
    }
    fn durable_file(&self) -> PathBuf {
        self.project.join(".claude").join("scheduled_tasks.json")
    }
}

fn load(path: &Path) -> Result<Vec<Job>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes).with_context(|| format!("Invalid scheduled tasks file {}", path.display()))?;
            Ok(serde_json::from_value(value.get("tasks").cloned().unwrap_or(value))?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("Reading {}", path.display())),
    }
}

fn save(path: &Path, jobs: &[Job]) -> Result<()> {
    if jobs.is_empty() && !path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().context("scheduled tasks directory")?)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&json!({"tasks": jobs}))?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn schedule(expression: &str) -> Result<cron::Schedule> {
    let fields = expression.split_whitespace().count();
    anyhow::ensure!(fields == 5, "cron must have 5 fields: minute hour day-of-month month day-of-week");
    cron::Schedule::from_str(&format!("0 {expression}")).context("Invalid cron expression")
}

fn next_after(expression: &str, after: DateTime<Local>) -> Result<i64> {
    Ok(schedule(expression)?.after(&after).next().context("Cron expression has no future occurrence")?.timestamp())
}

fn local(timestamp: i64) -> String {
    Local.timestamp_opt(timestamp, 0).single().map(|time| time.format("%Y-%m-%d %H:%M %Z").to_string()).unwrap_or_default()
}

fn describe(job: &Job) -> Value {
    json!({"id":job.id,"cron":job.cron,"prompt":job.prompt,"recurring":job.recurring,"durable":job.durable,"next_run":local(job.next_run)})
}

pub fn execute(name: &str, input: &Value, scope: &Scope) -> Result<Value> {
    let mut session_jobs = load(&scope.session_file())?;
    let mut durable_jobs = load(&scope.durable_file())?;
    match name {
        "CronCreate" => {
            let cron = input["cron"].as_str().context("cron is required")?.trim().to_owned();
            let prompt = input["prompt"].as_str().filter(|p| !p.trim().is_empty()).context("prompt is required")?.to_owned();
            anyhow::ensure!(prompt.len() <= 64 * 1024, "prompt exceeds 64 KiB");
            anyhow::ensure!(session_jobs.len() + durable_jobs.len() < MAX_JOBS, "At most {MAX_JOBS} scheduled jobs");
            let now = Local::now();
            let job = Job {
                id: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
                next_run: next_after(&cron, now)?,
                cron,
                prompt,
                recurring: input["recurring"].as_bool().unwrap_or(true),
                durable: input["durable"].as_bool().unwrap_or(false),
                created_at: now.timestamp(),
            };
            let result = describe(&job);
            if job.durable { durable_jobs.push(job); save(&scope.durable_file(), &durable_jobs)?; } else { session_jobs.push(job); save(&scope.session_file(), &session_jobs)?; }
            Ok(json!({"job":result,"content":format!("Scheduled job {} — next run {}{}", result["id"].as_str().unwrap_or_default(), result["next_run"].as_str().unwrap_or_default(), if input["recurring"].as_bool().unwrap_or(true) { ". Recurring jobs expire after 7 days." } else { "" })}))
        }
        "CronDelete" => {
            let id = input["id"].as_str().context("id is required")?;
            let (before_session, before_durable) = (session_jobs.len(), durable_jobs.len());
            session_jobs.retain(|job| job.id != id);
            durable_jobs.retain(|job| job.id != id);
            if session_jobs.len() == before_session && durable_jobs.len() == before_durable { bail!("No scheduled job with id {id}"); }
            save(&scope.session_file(), &session_jobs)?;
            save(&scope.durable_file(), &durable_jobs)?;
            Ok(json!({"deleted":id}))
        }
        "CronList" => {
            let jobs: Vec<Value> = session_jobs.iter().chain(&durable_jobs).map(describe).collect();
            Ok(json!({"jobs":jobs,"count":jobs.len()}))
        }
        _ => bail!("Unknown cron tool {name}"),
    }
}

/// Prompts whose time has come, oldest first. One-shot and expired recurring
/// jobs are removed after firing; the others move to their next run.
pub fn take_due(scope: &Scope, now: DateTime<Local>) -> Result<Vec<String>> {
    let mut prompts = Vec::new();
    for path in [scope.session_file(), scope.durable_file()] {
        let mut jobs = load(&path)?;
        if !jobs.iter().any(|job| job.next_run <= now.timestamp()) { continue; }
        let mut kept = Vec::new();
        for mut job in jobs.drain(..) {
            if job.next_run > now.timestamp() { kept.push(job); continue; }
            prompts.push((job.next_run, job.prompt.clone()));
            let expired = now.timestamp() - job.created_at >= RECURRING_LIFETIME_SECS;
            if job.recurring && !expired {
                job.next_run = next_after(&job.cron, now)?;
                kept.push(job);
            }
        }
        save(&path, &kept)?;
    }
    prompts.sort();
    Ok(prompts.into_iter().map(|(_, prompt)| prompt).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_fire_and_delete_session_and_durable_jobs() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let scope = Scope { root: root.path(), session: "s1", project: project.path() };
        assert!(execute("CronCreate", &json!({"cron":"* * * *","prompt":"x"}), &scope).is_err());
        let every = execute("CronCreate", &json!({"cron":"*/5 * * * *","prompt":"check build"}), &scope).unwrap();
        let once = execute("CronCreate", &json!({"cron":"7 9 1 1 *","prompt":"new year","recurring":false,"durable":true}), &scope).unwrap();
        assert!(project.path().join(".claude/scheduled_tasks.json").exists());
        assert_eq!(execute("CronList", &json!({}), &scope).unwrap()["count"], 2);

        let later = Local::now() + chrono::Duration::days(400);
        let due = take_due(&scope, later).unwrap();
        assert_eq!(due.len(), 2);
        let listed = execute("CronList", &json!({}), &scope).unwrap();
        assert_eq!(listed["count"], 0, "one-shot fired and the recurring job expired: {listed}");

        let again = execute("CronCreate", &json!({"cron":"*/5 * * * *","prompt":"poll"}), &scope).unwrap();
        assert_eq!(take_due(&scope, Local::now() + chrono::Duration::minutes(6)).unwrap(), ["poll"]);
        assert_eq!(execute("CronList", &json!({}), &scope).unwrap()["count"], 1);
        let id = again["job"]["id"].as_str().unwrap();
        execute("CronDelete", &json!({"id":id}), &scope).unwrap();
        assert!(execute("CronDelete", &json!({"id":id}), &scope).is_err());
        assert!(every["content"].as_str().unwrap().contains("expire after 7 days"));
        assert_eq!(once["job"]["durable"], true);
        let other = Scope { root: root.path(), session: "s2", project: project.path() };
        assert_eq!(execute("CronList", &json!({}), &other).unwrap()["count"], 0);
    }
}
