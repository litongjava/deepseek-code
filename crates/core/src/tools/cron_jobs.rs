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
            summary: Some("Schedule a prompt to run later".into()),
            name: "CronCreate".into(),
            description: "Schedule a prompt to run later. `cron` has five local-time fields (minute, hour, day-of-month, month, day-of-week); the result shows the first fire time. `recurring: true` (default) fires at every match, `recurring: false` fires once and deletes the job. Jobs run only while the session is idle, so a missed match fires once at the next idle check. Recurring jobs expire 7 days after creation, firing once more at the first due time and then being removed. Session jobs (default) end with the session; `durable: true` saves them in `.claude/scheduled_tasks.json`. Prompts are limited to 64 KiB.".into(),
            input_schema: json!({"type":"object","properties":{
                "cron":{"description":"Five-field cron expression in local time: minute, hour, day-of-month, month, day-of-week.","type":"string"},
                "prompt":{"description":"Text to send as a user turn whenever the job fires.","type":"string"},
                "recurring":{"description":"true (default) repeats on every match until the 7-day expiry; false fires once and removes the job.","type":"boolean"},
                "durable":{"description":"true saves the job in .claude/scheduled_tasks.json so later sessions of the project can fire it; false (default) keeps it in this session.","type":"boolean"}
            },"required":["cron","prompt"],"additionalProperties":false}),
        },
        super::ToolDefinition {
            summary: Some("Remove a scheduled job created by CronCreate".into()),
            name: "CronDelete".into(),
            description: "Remove a scheduled job created by CronCreate. The job is deleted from whichever store holds it, the session's job file or the project's .claude/scheduled_tasks.json, so it will not fire again. An unknown id returns an error and leaves other jobs alone.".into(),
            input_schema: json!({"type":"object","properties":{"id":{"description":"Identifier CronCreate returned for the job to remove.","type":"string"}},"required":["id"],"additionalProperties":false}),
        },
        super::ToolDefinition {
            summary: Some("Read the session scheduled prompts and the project saved ones".into()),
            name: "CronList".into(),
            description: "Read the current session’s scheduled prompts together with those saved for the project. Each entry gives its ID, cron expression, prompt, recurrence and durability flags, and next run in local time. The result includes the total count.".into(),
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
        self.root
            .join("cron")
            .join(format!("{}.json", self.session))
    }
    fn durable_file(&self) -> PathBuf {
        self.project.join(".claude").join("scheduled_tasks.json")
    }
}

fn load(path: &Path) -> Result<Vec<Job>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("Invalid scheduled tasks file {}", path.display()))?;
            Ok(serde_json::from_value(
                value.get("tasks").cloned().unwrap_or(value),
            )?)
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
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&json!({"tasks": jobs}))?,
    )?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn schedule(expression: &str) -> Result<cron::Schedule> {
    let fields = expression.split_whitespace().count();
    anyhow::ensure!(
        fields == 5,
        "cron must have 5 fields: minute hour day-of-month month day-of-week"
    );
    cron::Schedule::from_str(&format!("0 {expression}")).context("Invalid cron expression")
}

fn next_after(expression: &str, after: DateTime<Local>) -> Result<i64> {
    Ok(schedule(expression)?
        .after(&after)
        .next()
        .context("Cron expression has no future occurrence")?
        .timestamp())
}

fn local(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_default()
}

fn describe(job: &Job) -> Value {
    json!({"id":job.id,"cron":job.cron,"prompt":job.prompt,"recurring":job.recurring,"durable":job.durable,"next_run":local(job.next_run)})
}

pub fn execute(name: &str, input: &Value, scope: &Scope) -> Result<Value> {
    let mut session_jobs = load(&scope.session_file())?;
    let mut durable_jobs = load(&scope.durable_file())?;
    match name {
        "CronCreate" => {
            let cron = input["cron"]
                .as_str()
                .context("cron is required")?
                .trim()
                .to_owned();
            let prompt = input["prompt"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
                .context("prompt is required")?
                .to_owned();
            anyhow::ensure!(prompt.len() <= 64 * 1024, "prompt exceeds 64 KiB");
            anyhow::ensure!(
                session_jobs.len() + durable_jobs.len() < MAX_JOBS,
                "At most {MAX_JOBS} scheduled jobs"
            );
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
            if job.durable {
                durable_jobs.push(job);
                save(&scope.durable_file(), &durable_jobs)?;
            } else {
                session_jobs.push(job);
                save(&scope.session_file(), &session_jobs)?;
            }
            Ok(
                json!({"job":result,"content":format!("Scheduled job {} — next run {}{}", result["id"].as_str().unwrap_or_default(), result["next_run"].as_str().unwrap_or_default(), if input["recurring"].as_bool().unwrap_or(true) { ". Recurring jobs expire after 7 days." } else { "" })}),
            )
        }
        "CronDelete" => {
            let id = input["id"].as_str().context("id is required")?;
            let (before_session, before_durable) = (session_jobs.len(), durable_jobs.len());
            session_jobs.retain(|job| job.id != id);
            durable_jobs.retain(|job| job.id != id);
            if session_jobs.len() == before_session && durable_jobs.len() == before_durable {
                bail!("No scheduled job with id {id}");
            }
            save(&scope.session_file(), &session_jobs)?;
            save(&scope.durable_file(), &durable_jobs)?;
            Ok(json!({"deleted":id}))
        }
        "CronList" => {
            let jobs: Vec<Value> = session_jobs
                .iter()
                .chain(&durable_jobs)
                .map(describe)
                .collect();
            Ok(json!({"jobs":jobs,"count":jobs.len()}))
        }
        _ => bail!("Unknown cron tool {name}"),
    }
}

fn claim(root: &Path, jobs_file: &Path) -> Result<Option<crate::fs::OwnedFileLock>> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    jobs_file.hash(&mut hash);
    let directory = root.join("state/locks/cron");
    std::fs::create_dir_all(&directory)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{:016x}.lock", hash.finish())))?;
    Ok(crate::fs::OwnedFileLock::try_acquire(file)?)
}

/// Prompts whose time has come, oldest first. One-shot and expired recurring
/// jobs are removed after firing; the others move to their next run.
pub fn take_due(scope: &Scope, now: DateTime<Local>) -> Result<Vec<String>> {
    let mut prompts = Vec::new();
    for path in [scope.session_file(), scope.durable_file()] {
        if !load(&path)?
            .iter()
            .any(|job| job.next_run <= now.timestamp())
        {
            continue;
        }
        // Sessions of one project share durable jobs: whoever holds the lock fires
        // them and moves them on, so the others find nothing due when they look again.
        let Some(_claim) = claim(scope.root, &path)? else {
            continue;
        };
        let mut jobs = load(&path)?;
        let mut kept = Vec::new();
        for mut job in jobs.drain(..) {
            if job.next_run > now.timestamp() {
                kept.push(job);
                continue;
            }
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
    fn a_durable_job_fires_in_only_one_session() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let first = Scope {
            root: root.path(),
            session: "s1",
            project: project.path(),
        };
        let second = Scope {
            root: root.path(),
            session: "s2",
            project: project.path(),
        };
        execute(
            "CronCreate",
            &json!({"cron":"*/5 * * * *","prompt":"shared","durable":true}),
            &first,
        )
        .unwrap();
        let later = Local::now() + chrono::Duration::minutes(6);

        let held = claim(root.path(), &first.durable_file()).unwrap().unwrap();
        assert!(
            take_due(&second, later).unwrap().is_empty(),
            "another session is firing it"
        );
        drop(held);

        assert_eq!(take_due(&first, later).unwrap(), ["shared"]);
        assert!(
            take_due(&second, later).unwrap().is_empty(),
            "it already moved to its next run"
        );
    }

    #[test]
    fn create_list_fire_and_delete_session_and_durable_jobs() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let scope = Scope {
            root: root.path(),
            session: "s1",
            project: project.path(),
        };
        assert!(execute(
            "CronCreate",
            &json!({"cron":"* * * *","prompt":"x"}),
            &scope
        )
        .is_err());
        let every = execute(
            "CronCreate",
            &json!({"cron":"*/5 * * * *","prompt":"check build"}),
            &scope,
        )
        .unwrap();
        let once = execute(
            "CronCreate",
            &json!({"cron":"7 9 1 1 *","prompt":"new year","recurring":false,"durable":true}),
            &scope,
        )
        .unwrap();
        assert!(project.path().join(".claude/scheduled_tasks.json").exists());
        assert_eq!(execute("CronList", &json!({}), &scope).unwrap()["count"], 2);

        let later = Local::now() + chrono::Duration::days(400);
        let due = take_due(&scope, later).unwrap();
        assert_eq!(due.len(), 2);
        let listed = execute("CronList", &json!({}), &scope).unwrap();
        assert_eq!(
            listed["count"], 0,
            "one-shot fired and the recurring job expired: {listed}"
        );

        let again = execute(
            "CronCreate",
            &json!({"cron":"*/5 * * * *","prompt":"poll"}),
            &scope,
        )
        .unwrap();
        assert_eq!(
            take_due(&scope, Local::now() + chrono::Duration::minutes(6)).unwrap(),
            ["poll"]
        );
        assert_eq!(execute("CronList", &json!({}), &scope).unwrap()["count"], 1);
        let id = again["job"]["id"].as_str().unwrap();
        execute("CronDelete", &json!({"id":id}), &scope).unwrap();
        assert!(execute("CronDelete", &json!({"id":id}), &scope).is_err());
        assert!(every["content"]
            .as_str()
            .unwrap()
            .contains("expire after 7 days"));
        assert_eq!(once["job"]["durable"], true);
        let other = Scope {
            root: root.path(),
            session: "s2",
            project: project.path(),
        };
        assert_eq!(execute("CronList", &json!({}), &other).unwrap()["count"], 0);
    }

    #[cfg(unix)]
    #[test]
    fn claim_after_release_survives_concurrent_process_spawns() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("jobs.json");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::process::Command::new("true").status();
                }
            })
        };
        let mut failures = 0;
        for _ in 0..200 {
            let held = claim(root.path(), &file).unwrap().unwrap();
            drop(held);
            match claim(root.path(), &file).unwrap() {
                Some(again) => drop(again),
                None => failures += 1,
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        spawner.join().unwrap();
        assert_eq!(failures, 0, "claims after release failed {failures} times");
    }
}
