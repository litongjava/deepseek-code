//! Durable, explicitly started batch plans with isolated Git worktree workers.
use super::background_sessions::{self, Launch};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WORKER_ENV: &str = "DEEPSEEK_CODE_BATCH_SUPERVISOR";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    pub title: String,
    pub paths: Vec<String>,
    pub instructions: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub summary: String,
    pub verification: String,
    pub units: Vec<Unit>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub title: String,
    pub status: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub job_id: Option<String>,
    pub session_id: Option<String>,
    pub commit: Option<String>,
    pub pr: Option<String>,
    pub note: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    pub parent_session_id: String,
    pub workspace: PathBuf,
    pub base_commit: String,
    pub instruction: String,
    pub proposal: Proposal,
    pub status: String,
    pub concurrency: usize,
    pub publish: bool,
    pub permission_mode: String,
    pub heartbeat_ms: u64,
    pub workers: Vec<Worker>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLaunch {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub model: String,
    pub provider: String,
    pub permission_mode: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "Invalid batch ID");
    Ok(root.join("batches").join(id))
}
fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Missing batch directory")?)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.flush()?;
    let mut attempt = 0;
    loop {
        match file.persist(path) {
            Ok(_) => break,
            Err(error)
                if attempt < 10
                    && matches!(
                        error.error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                    ) =>
            {
                file = error.file;
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.error.into()),
        }
    }
    Ok(())
}
fn bounded_output(command: &mut Command) -> Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let tree = match super::tool_hooks::ProcessTree::new(child.id()) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(10) {
            drop(tree);
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Batch verification command timed out after 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    drop(tree);
    stdout.rewind()?;
    stderr.rewind()?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    ensure!(
        stdout.metadata()?.len() <= 4 * 1024 * 1024,
        "Batch command output exceeds 4 MiB"
    );
    stdout.take(4 * 1024 * 1024).read_to_end(&mut out)?;
    stderr.take(1024 * 1024).read_to_end(&mut err)?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}
fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let result = bounded_output(&mut command).context("Cannot run Git")?;
    ensure!(
        result.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout)?;
    Ok(if args.contains(&"-z") {
        output
    } else {
        output.trim().to_owned()
    })
}

pub fn repository(workspace: &Path) -> Result<(PathBuf, String)> {
    let root = PathBuf::from(git(workspace, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
    ensure!(
        git(
            &root,
            &["status", "--porcelain", "--untracked-files=normal"]
        )?
        .is_empty(),
        "Batch planning requires a clean repository; commit or stash existing changes first"
    );
    let head = git(&root, &["rev-parse", "HEAD"])?;
    Ok((root, head))
}
fn relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}
pub fn validate(proposal: &Proposal) -> Result<()> {
    ensure!(
        (5..=30).contains(&proposal.units.len()),
        "Batch plans need 5–30 independent work units"
    );
    ensure!(
        !proposal.summary.trim().is_empty() && !proposal.verification.trim().is_empty(),
        "Batch summary and verification recipe are required"
    );
    ensure!(
        serde_json::to_vec(proposal)?.len() <= 256 * 1024,
        "Batch plan exceeds 256 KiB"
    );
    let mut assigned = Vec::<(usize, &str)>::new();
    for (index, unit) in proposal.units.iter().enumerate() {
        ensure!(
            !unit.title.trim().is_empty()
                && !unit.instructions.trim().is_empty()
                && !unit.paths.is_empty(),
            "Each unit needs title, paths, and instructions"
        );
        for path in &unit.paths {
            ensure!(
                relative(path),
                "Batch path must be repository-relative without traversal: {path}"
            );
            for (other, prior) in &assigned {
                ensure!(
                    *other == index
                        || !(Path::new(path).starts_with(prior)
                            || Path::new(prior).starts_with(path)),
                    "Units {} and {} have overlapping paths: {} / {}",
                    other + 1,
                    index + 1,
                    prior,
                    path
                );
            }
            assigned.push((index, path));
        }
    }
    Ok(())
}
pub fn schema() -> Value {
    json!({"type":"object","properties":{"summary":{"type":"string"},"verification":{"type":"string"},"units":{"type":"array","minItems":5,"maxItems":30,"items":{"type":"object","properties":{"title":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"minItems":1},"instructions":{"type":"string"}},"required":["title","paths","instructions"],"additionalProperties":false}}},"required":["summary","verification","units"],"additionalProperties":false})
}

pub fn create(
    root: &Path,
    parent: &str,
    workspace: &Path,
    instruction: &str,
    proposal: Proposal,
) -> Result<Batch> {
    validate(&proposal)?;
    let (workspace, base_commit) = repository(workspace)?;
    let id = uuid::Uuid::new_v4().to_string();
    let dir = directory(root, &id)?;
    fs::create_dir_all(&dir)?;
    let workers = proposal
        .units
        .iter()
        .enumerate()
        .map(|(index, unit)| Worker {
            title: unit.title.clone(),
            status: "queued".into(),
            branch: format!("batch/{id}/unit-{}", index + 1),
            worktree: dir.join("worktrees").join(format!("unit-{}", index + 1)),
            job_id: None,
            session_id: None,
            commit: None,
            pr: None,
            note: None,
        })
        .collect();
    let batch = Batch {
        id,
        parent_session_id: parent.into(),
        workspace,
        base_commit,
        instruction: instruction.into(),
        proposal,
        status: "planned".into(),
        concurrency: 4,
        publish: false,
        permission_mode: "ask".into(),
        heartbeat_ms: now(),
        workers,
        error: None,
    };
    save(&dir.join("state.json"), &batch)?;
    Ok(batch)
}
pub fn get(root: &Path, id: &str) -> Result<Batch> {
    let mut batch: Batch =
        serde_json::from_slice(&fs::read(directory(root, id)?.join("state.json"))?)?;
    if batch.status == "running" && now().saturating_sub(batch.heartbeat_ms) > 15_000 {
        batch.status = "unresponsive".into();
        batch.error =
            Some("Batch supervisor heartbeat expired; worker completion is unknown".into());
    }
    Ok(batch)
}
pub fn list(root: &Path, parent: &str) -> Result<Vec<Batch>> {
    let mut batches = Vec::new();
    let path = root.join("batches");
    if !path.exists() {
        return Ok(batches);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if let Some(id) = entry.file_name().to_str() {
            if let Ok(batch) = get(root, id) {
                if batch.parent_session_id == parent {
                    batches.push(batch);
                }
            }
        }
    }
    batches.sort_by_key(|batch| std::cmp::Reverse(batch.heartbeat_ms));
    Ok(batches)
}

/// Build the command that starts the batch supervisor. The parent's worker
/// state is stripped first so the supervisor does not inherit a model snapshot
/// or job it does not own; explicitly forwarded values are then applied.
fn supervisor_command(executable: &Path, dir: &Path, environment: &[(String, String)]) -> Command {
    let mut command = Command::new(executable);
    background_sessions::strip_internal_env(&mut command);
    command
        .env(WORKER_ENV, dir)
        .envs(environment.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub fn start(
    root: &Path,
    id: &str,
    launch: WorkerLaunch,
    concurrency: usize,
    publish: bool,
    environment: &[(String, String)],
) -> Result<Batch> {
    ensure!(
        (1..=30).contains(&concurrency),
        "Batch concurrency must be 1–30"
    );
    ensure!(
        matches!(
            launch.permission_mode.as_str(),
            "ask" | "accept-edits" | "plan" | "bypass"
        ),
        "Invalid batch permission mode"
    );
    let mut batch = get(root, id)?;
    ensure!(batch.status == "planned", "Only a planned batch can start");
    let (_, head) = repository(&batch.workspace)?;
    ensure!(
        head == batch.base_commit,
        "Repository HEAD changed since planning; generate a new plan"
    );
    let dir = directory(root, id)?;
    // Durable one-shot claim prevents two frontends from launching the same plan.
    let _claim = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("started"))
        .context("Batch was already started")?;
    batch.status = "running".into();
    batch.concurrency = concurrency.min(batch.workers.len());
    batch.publish = publish;
    batch.permission_mode = launch.permission_mode.clone();
    batch.heartbeat_ms = now();
    save(&dir.join("launch.json"), &launch)?;
    save(&dir.join("state.json"), &batch)?;
    let mut command = supervisor_command(&launch.executable, &dir, environment);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            batch.status = "failed".into();
            batch.error = Some(error.to_string());
            save(&dir.join("state.json"), &batch)?;
            return Err(error.into());
        }
    }
    Ok(batch)
}
pub fn cancel(root: &Path, id: &str) -> Result<Batch> {
    let batch = get(root, id)?;
    ensure!(
        matches!(batch.status.as_str(), "running" | "unresponsive"),
        "Batch is not running"
    );
    fs::write(directory(root, id)?.join("cancel"), b"cancel")?;
    for worker in &batch.workers {
        if let Some(job) = &worker.job_id {
            let _ = background_sessions::cancel(root, job);
        }
    }
    Ok(batch)
}

fn prompt(batch: &Batch, index: usize) -> String {
    let unit = &batch.proposal.units[index];
    format!("Implement this approved batch unit in its isolated Git worktree.\nOverall goal: {}\nUnit: {}\nAssigned paths (do not change other paths): {}\nInstructions: {}\nConventions/research: {}\nVerification recipe: {}\nReview and simplify your changes, run relevant tests and the verification recipe, fix failures, then commit only assigned paths on the current branch with a clear English commit message. Do not merge other units or modify the primary checkout. {}\nReport actual test results, commit, and PR URL; if no PR exists say PR: none and explain. Never invent a successful command or PR.",batch.instruction,unit.title,unit.paths.join(", "),unit.instructions,batch.proposal.summary,batch.proposal.verification,if batch.publish{"Publication is explicitly authorized: push this branch to the existing origin and create a pull request using gh pr create. If credentials or gh are unavailable, report the failure."}else{"Publication is not authorized: do not push or create a pull request; keep the commit local."})
}
fn inspect_completion(
    batch: &Batch,
    index: usize,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let worker = &batch.workers[index];
    ensure!(
        git(
            &worker.worktree,
            &["status", "--porcelain", "--untracked-files=normal"]
        )?
        .is_empty(),
        "Worker exited with uncommitted changes"
    );
    ensure!(
        git(&worker.worktree, &["branch", "--show-current"])? == worker.branch,
        "Worker changed its assigned branch"
    );
    git(
        &worker.worktree,
        &["merge-base", "--is-ancestor", &batch.base_commit, "HEAD"],
    )
    .context("Worker HEAD no longer descends from the planned base")?;
    let head = git(&worker.worktree, &["rev-parse", "HEAD"])?;
    if head == batch.base_commit {
        return Ok((
            None,
            None,
            Some("Worker produced no commit; no changes were claimed".into()),
        ));
    }
    for path in git(
        &worker.worktree,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &batch.base_commit,
            &head,
        ],
    )?
    .split('\0')
    .filter(|path| !path.is_empty())
    {
        ensure!(
            batch.proposal.units[index]
                .paths
                .iter()
                .any(|allowed| Path::new(path).starts_with(allowed)),
            "Worker changed an unassigned path: {path}"
        );
    }
    if !batch.publish {
        return Ok((Some(head), None, None));
    }
    let mut command = Command::new("gh");
    command
        .current_dir(&worker.worktree)
        .args([
            "pr",
            "view",
            &worker.branch,
            "--json",
            "url,headRefName,headRefOid",
        ])
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let verified = bounded_output(&mut command)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .filter(|pr| pr["headRefName"] == worker.branch && pr["headRefOid"] == head)
        .and_then(|pr| pr["url"].as_str().map(str::to_owned));
    let note = verified
        .is_none()
        .then(|| "Local commit exists, but no matching remote PR was verified".into());
    Ok((Some(head), verified, note))
}
pub fn run_worker_from_env() -> Result<bool> {
    let Some(dir) = std::env::var_os(WORKER_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(WORKER_ENV);
    let dir = PathBuf::from(dir);
    let mut batch: Batch = serde_json::from_slice(&fs::read(dir.join("state.json"))?)?;
    let root = dir
        .parent()
        .and_then(Path::parent)
        .context("Invalid batch directory")?;
    let outcome = supervise(root, &dir, &mut batch);
    if let Err(error) = outcome {
        batch.status = "failed".into();
        batch.error = Some(format!("{error:#}"));
        for worker in &batch.workers {
            if let Some(job) = &worker.job_id {
                let _ = background_sessions::cancel(root, job);
            }
        }
    }
    batch.heartbeat_ms = now();
    save(&dir.join("state.json"), &batch)?;
    Ok(true)
}
fn supervise(root: &Path, dir: &Path, batch: &mut Batch) -> Result<()> {
    let launch: WorkerLaunch = serde_json::from_slice(&fs::read(dir.join("launch.json"))?)?;
    let paths = crate::runtime::RuntimePaths::new(root.into())?;
    let store = crate::runtime::SessionStore::new(paths.open_database()?);
    loop {
        let cancelling = dir.join("cancel").exists();
        for index in 0..batch.workers.len() {
            batch.heartbeat_ms = now();
            save(&dir.join("state.json"), batch)?;
            if let Some(id) = &batch.workers[index].job_id {
                if batch.workers[index].status != "running" {
                    continue;
                }
                let job = background_sessions::get(root, id)?;
                if cancelling {
                    let _ = background_sessions::cancel(root, id);
                }
                match job.status.as_str() {
                    "starting" | "running" => {}
                    "unresponsive" => anyhow::bail!(
                        "Worker supervisor lost heartbeat; inspect task {id} before retrying"
                    ),
                    "completed" => match inspect_completion(batch, index) {
                        Ok((commit, pr, note)) => {
                            let worker = &mut batch.workers[index];
                            worker.status = if commit.is_none() {
                                "no_changes"
                            } else if batch.publish && pr.is_none() {
                                "publication_failed"
                            } else {
                                "completed"
                            }
                            .into();
                            worker.commit = commit;
                            worker.pr = pr;
                            worker.note = note;
                        }
                        Err(error) => {
                            batch.workers[index].status = "failed".into();
                            batch.workers[index].note = Some(error.to_string());
                        }
                    },
                    _ => {
                        batch.workers[index].status = job.status;
                        batch.workers[index].note = job.error.or_else(|| {
                            job.exit_code.map(|code| {
                                format!(
                                    "Worker exited with code {code}; use /batch logs for details"
                                )
                            })
                        });
                    }
                }
            }
        }
        if cancelling {
            for worker in &mut batch.workers {
                if worker.status == "queued" {
                    worker.status = "cancelled".into();
                }
            }
        }
        let mut active = batch
            .workers
            .iter()
            .filter(|worker| worker.status == "running")
            .count();
        if !cancelling {
            for index in 0..batch.workers.len() {
                batch.heartbeat_ms = now();
                save(&dir.join("state.json"), batch)?;
                if active >= batch.concurrency {
                    break;
                }
                if batch.workers[index].status != "queued" {
                    continue;
                }
                let worker = &batch.workers[index];
                fs::create_dir_all(worker.worktree.parent().unwrap())?;
                git(
                    &batch.workspace,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        &worker.branch,
                        worker.worktree.to_str().context("Non-UTF8 worktree path")?,
                        &batch.base_commit,
                    ],
                )?;
                let session = store.create(&worker.worktree, &launch.model, &launch.provider)?;
                // A stronger explicitly selected mode never erases existing deny
                // rules or agent tool restrictions from the originating session.
                store.db.with_conn(|conn| conn.execute(
                    "INSERT INTO peripheral_settings(key,value) SELECT ?1,value FROM peripheral_settings WHERE key=?2 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [format!("cli.permissions.{session}"),format!("cli.permissions.{}",batch.parent_session_id)]
                ))??;
                if let Some(agent) =
                    super::agents::for_session(&store.db, &batch.parent_session_id)?
                {
                    super::agents::bind(&store.db, &session, &agent)?;
                }
                let mut arguments = launch.arguments.clone();
                arguments.extend([
                    "--path".into(),
                    worker.worktree.to_string_lossy().into_owned(),
                    "--resume".into(),
                    session.clone(),
                    "--permission-mode".into(),
                    launch.permission_mode.clone(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--print".into(),
                    "--".into(),
                    prompt(batch, index),
                ]);
                let job = background_sessions::spawn(
                    root,
                    &launch.executable,
                    Launch {
                        executable: launch.executable.clone(),
                        arguments,
                        cwd: worker.worktree.clone(),
                    },
                    &batch.parent_session_id,
                    &session,
                    &worker.title,
                    &[],
                )?;
                batch.workers[index].status = "running".into();
                batch.workers[index].job_id = Some(job.id);
                batch.workers[index].session_id = Some(session);
                active += 1;
            }
        }
        batch.heartbeat_ms = now();
        save(&dir.join("state.json"), batch)?;
        if !batch
            .workers
            .iter()
            .any(|worker| matches!(worker.status.as_str(), "queued" | "running"))
        {
            batch.status = if cancelling {
                "cancelled"
            } else if batch
                .workers
                .iter()
                .all(|worker| matches!(worker.status.as_str(), "completed" | "no_changes"))
            {
                "completed"
            } else {
                "failed"
            }
            .into();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proposal() -> Proposal {
        Proposal {
            summary: "Independent file updates".into(),
            verification: "Run relevant unit tests".into(),
            units: (0..5)
                .map(|index| Unit {
                    title: format!("Unit {index}"),
                    paths: vec![format!("unit-{index}")],
                    instructions: "Update this module".into(),
                })
                .collect(),
        }
    }
    #[test]
    fn rejects_overlaps_traversal_and_wrong_unit_counts() {
        let mut plan = proposal();
        validate(&plan).unwrap();
        plan.units[1].paths = vec!["unit-0/nested".into()];
        assert!(validate(&plan).is_err());
        plan.units[1].paths = vec!["../outside".into()];
        assert!(validate(&plan).is_err());
        plan.units.clear();
        assert!(validate(&plan).is_err());
    }
    #[test]
    fn supervisor_command_drops_inherited_worker_state() {
        let dir = Path::new("fixture-batch");
        let command = supervisor_command(Path::new("fixture"), dir, &[]);
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.iter().any(
            |(name, value)| *name == background_sessions::WORKER_MODEL_ENV && value.is_none()
        ));
        assert!(envs.iter().any(|(name, value)| *name
            == background_sessions::WORKER_AUTO_ROUTE_ENV
            && value.is_none()));
        assert!(envs
            .iter()
            .any(|(name, value)| *name == background_sessions::CONTROL_ENV && value.is_none()));
        // The batch supervisor still gets the directory this plan owns.
        assert!(envs
            .iter()
            .any(|(name, value)| *name == WORKER_ENV && *value == Some(dir.as_os_str())));
    }

    #[test]
    fn stale_supervisor_is_marked_unresponsive_without_faking_completion() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]).unwrap();
        fs::write(repo.join("README.md"), "fixture").unwrap();
        git(&repo, &["add", "README.md"]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        let data = dir.path().join("data");
        let batch = create(&data, "parent", &repo, "Stale", proposal()).unwrap();
        let directory = directory(&data, &batch.id).unwrap();
        let mut state: Batch =
            serde_json::from_slice(&fs::read(directory.join("state.json")).unwrap()).unwrap();
        state.status = "running".into();
        state.heartbeat_ms = 0;
        save(&directory.join("state.json"), &state).unwrap();
        let read = get(&data, &batch.id).unwrap();
        assert_eq!(read.status, "unresponsive");
        assert!(read.error.unwrap().contains("heartbeat"));
        assert!(read.workers.iter().all(|worker| worker.status == "queued"));
    }

    #[test]
    fn local_git_plan_persists_base_and_keeps_worktrees_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]).unwrap();
        fs::write(repo.join("README.md"), "fixture").unwrap();
        git(&repo, &["add", "README.md"]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        let data = dir.path().join("data");
        let batch = create(&data, "parent", &repo, "Change modules", proposal()).unwrap();
        assert_eq!(get(&data, &batch.id).unwrap().status, "planned");
        assert_eq!(list(&data, "other").unwrap().len(), 0);
        let worker = &batch.workers[0];
        fs::create_dir_all(worker.worktree.parent().unwrap()).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &worker.branch,
                worker.worktree.to_str().unwrap(),
                &batch.base_commit,
            ],
        )
        .unwrap();
        fs::write(worker.worktree.join("isolated.txt"), "only here").unwrap();
        assert!(!repo.join("isolated.txt").exists());
        assert!(inspect_completion(&batch, 0)
            .unwrap_err()
            .to_string()
            .contains("uncommitted"));
        fs::remove_file(worker.worktree.join("isolated.txt")).unwrap();
        let unicode = "unit-0/\u{4e2d}\u{6587} \u{7a7a}\u{683c}.txt";
        fs::create_dir(worker.worktree.join("unit-0")).unwrap();
        fs::write(worker.worktree.join(unicode), "change").unwrap();
        git(&worker.worktree, &["add", unicode]).unwrap();
        git(
            &worker.worktree,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "Change assigned Unicode file",
            ],
        )
        .unwrap();
        let (commit, pr, _) = inspect_completion(&batch, 0).unwrap();
        assert!(commit.is_some());
        assert!(pr.is_none());
        git(&worker.worktree, &["checkout", "-b", "unexpected-branch"]).unwrap();
        assert!(inspect_completion(&batch, 0)
            .unwrap_err()
            .to_string()
            .contains("assigned branch"));
        git(&worker.worktree, &["checkout", &worker.branch]).unwrap();
        fs::write(worker.worktree.join("outside.txt"), "outside").unwrap();
        git(&worker.worktree, &["add", "outside.txt"]).unwrap();
        git(
            &worker.worktree,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "Out of scope fixture",
            ],
        )
        .unwrap();
        assert!(inspect_completion(&batch, 0)
            .unwrap_err()
            .to_string()
            .contains("unassigned path"));
    }
}
