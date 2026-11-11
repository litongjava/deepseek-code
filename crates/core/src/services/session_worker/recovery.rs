use super::*;

pub(super) fn startup_lock(base: &Path) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(base.join("startup.lock"))?;
    crate::fs::try_lock_file(&lock)
        .context("Session worker startup or recovery is already in progress; retry shortly")?;
    Ok(lock)
}

pub(super) fn prepare_lifetime_locks(dir: &Path) -> Result<()> {
    for name in ["supervisor.lock", "host.lock"] {
        fs::File::create(dir.join(name))?.sync_all()?;
    }
    Ok(())
}

pub(super) fn lifetime_lock(dir: &Path, name: &str) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(name))?;
    crate::fs::try_lock_file(&lock)
        .context("Session worker process is already running")?;
    ensure!(
        !dir.join("retired.json").exists(),
        "Session worker generation was retired; refusing delayed startup"
    );
    Ok(lock)
}

// Migration for workers created before lifetime locks existed. A record from
// before this boot cannot refer to a surviving process, even if a PID was reused.
fn predates_boot(dir: &Path) -> Result<bool> {
    let Some(uptime) = uptime() else {
        return Ok(false);
    };
    let Some(boot) = std::time::SystemTime::now().checked_sub(uptime + Duration::from_secs(5))
    else {
        return Ok(false);
    };
    let path = dir.join("worker.json");
    Ok(path.metadata()?.modified()? < boot)
}

#[cfg(windows)]
fn uptime() -> Option<Duration> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTickCount64() -> u64;
    }
    Some(Duration::from_millis(unsafe { GetTickCount64() }))
}
#[cfg(target_os = "linux")]
fn uptime() -> Option<Duration> {
    let text = fs::read_to_string("/proc/uptime").ok()?;
    let seconds = text.split_whitespace().next()?.parse::<f64>().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}
#[cfg(not(any(windows, target_os = "linux")))]
fn uptime() -> Option<Duration> {
    None
}

fn damaged(error: &anyhow::Error) -> bool {
    error.downcast_ref::<serde_json::Error>().is_some()
        || error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

// The recovery journal is written before ownership is detached. A retry can
// finish recovery after a crash without guessing whether the old turn succeeded.
fn archive(path: &Path, backup: &Path) -> Result<()> {
    match fs::read(path) {
        Ok(bytes) => {
            if !backup.exists() {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(backup)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            fs::remove_file(path)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn repair(root: &Path, session: &str) -> Result<bool> {
    let base = worker_path(root, session)?;
    let claim_path = root
        .join("background-tasks")
        .join(format!("session-{session}.claim"));
    if !base.join("current.json").exists() && !claim_path.exists() {
        return Ok(false);
    }
    let observed = current(root, session);
    let worker = match observed {
        Ok(worker) => worker,
        Err(e) if damaged(&e) => None,
        Err(e) => return Err(e),
    };
    if let Some(worker) = &worker {
        ensure!(
            worker.root == root && worker.session_id == session,
            "Session worker metadata does not match the requested session"
        );
        if worker.status == "stopped" && !claim_path.exists() {
            return Ok(false);
        }
    }
    let mut guards = Vec::new();
    let mut generations = Vec::new();
    // Include all generations when either pointer is unreadable; a live old host
    // must never disappear merely because its JSON was damaged.
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err()
        {
            continue;
        }
        let dir = entry.path();
        let metadata = read_worker(&dir).ok();
        if dir.join("retired.json").exists() {
            generations.push(dir);
            continue;
        }
        if dir.join("supervisor.lock").exists() && dir.join("host.lock").exists() {
            for name in ["supervisor.lock", "host.lock"] {
                let lock = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(dir.join(name))?;
                match crate::fs::try_lock_file(&lock) {
                    Ok(()) => guards.push(lock),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        ensure!(worker.is_some(), "Worker metadata is damaged but its process is still running; retry after it exits");
                        return Ok(false);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        } else {
            // Older binaries do not hold lifetime locks. Require positive process
            // exit evidence rather than interpreting a missing lock as a dead owner.
            if predates_boot(&dir)? {
                generations.push(dir);
                continue;
            }
            let Some(old) = metadata else {
                anyhow::bail!("Legacy worker metadata is damaged and process ownership cannot be verified; restart the computer before recovery");
            };
            if old.status != "stopped"
                && !(old.supervisor_pid.is_some_and(exited) && old.child_pid.is_some_and(exited))
            {
                ensure!(
                    worker.is_some(),
                    "Worker metadata is damaged and an older owner may still be alive"
                );
                return Ok(false);
            }
        }
        generations.push(dir);
    }
    ensure!(
        !generations.is_empty() || worker.as_ref().is_some_and(|w| w.status == "stopped"),
        "Session worker ownership could not be verified; original records preserved"
    );
    for dir in &generations {
        jobs::save(
            &dir.join("retired.json"),
            &serde_json::json!({"reason":"Recovered abandoned worker; previous task outcome is unknown", "at_ms":jobs::now()}),
        )?;
    }
    // Only foreground turns belong to this worker. Detached agents are independent.
    for mut job in jobs::list(root, None)? {
        if job.session_id == session && job.foreground && job.finished_ms.is_none() {
            let path = jobs::directory(root, &job.id)?.join("state.json");
            let backup = path.with_file_name("state.before-recovery.json");
            if !backup.exists() {
                fs::copy(&path, &backup)?;
                fs::OpenOptions::new()
                    .write(true)
                    .open(&backup)?
                    .sync_all()?;
            }
            job.status = "failed".into();
            job.error = Some(
                "Worker exited unexpectedly; previous task outcome is unknown and was not replayed"
                    .into(),
            );
            job.finished_ms = Some(jobs::now());
            job.heartbeat_ms = jobs::now();
            jobs::save(&path, &job)?;
        }
    }
    let backup = base.join(format!("recovery-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&backup)?;
    archive(&base.join("current.json"), &backup.join("current.json"))?;
    archive(&claim_path, &backup.join("owner.claim"))?;
    tracing::warn!(
        session,
        "Recovered abandoned session worker; original generation and task logs preserved"
    );
    drop(guards);
    Ok(true)
}

/// Positive evidence that a job's processes are gone. An unknown supervisor,
/// or a process that cannot be inspected, counts as possibly alive.
pub(super) fn job_abandoned(job: &Job) -> bool {
    job.supervisor_pid.is_some_and(exited) && job.child_pid.is_none_or(exited)
}

/// Records the unknown outcome of a job whose processes exited; never replays it.
pub(super) fn fail_abandoned_job(root: &Path, mut job: Job) -> Result<()> {
    let path = jobs::directory(root, &job.id)?.join("state.json");
    job.status = "failed".into();
    job.error = Some("Worker exited unexpectedly; previous task outcome is unknown and was not replayed".into());
    job.finished_ms = Some(jobs::now());
    job.heartbeat_ms = jobs::now();
    jobs::save(&path, &job)
}

/// A stale heartbeat alone is never proof that ownership can be taken over.
pub(super) fn recover(worker: &Worker) -> Result<()> {
    ensure!(
        worker.supervisor_pid.is_some_and(exited) && worker.child_pid.is_some_and(exited),
        "Session worker heartbeat expired, but its processes are alive or their state is unknown (session {}); retry after they exit",
        worker.session_id
    );
    let dir = directory(worker);
    let active = match fs::read(dir.join("active.json")) {
        Ok(bytes) => Some(serde_json::from_slice::<String>(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    for id in worker.active_job.iter().chain(active.iter()) {
        let job = jobs::get(&worker.root, id)?;
        ensure!(
            job.finished_ms.is_some(),
            "Session worker exited with unfinished task {id}; inspect that task before recovery"
        );
    }
    let mut stopped = worker.clone();
    stopped.status = "stopped".into();
    stopped.error =
        Some("Recovered stale worker after verifying both owner processes exited".into());
    // Release only our own claim; preserve task records and the previous worker.
    SessionClaim::release(&worker.root, &worker.session_id, &worker.id)?;
    jobs::save(&dir.join("worker.json"), &stopped)?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn exited(pid: u32) -> bool {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    if pid == 0 {
        return false;
    }
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(handle) => {
                let result = WaitForSingleObject(handle, 0);
                let _ = CloseHandle(handle);
                result == windows::Win32::Foundation::WAIT_OBJECT_0
            }
            Err(error) => {
                error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0)
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn exited(pid: u32) -> bool {
    // ESRCH is 3 on supported Unix targets. Permission errors remain unknown.
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    pid > 0
        && pid <= i32::MAX as u32
        && unsafe { kill(pid as i32, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path, pid: u32) -> Worker {
        let worker = Worker {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            root: root.into(),
            workspace: root.into(),
            executable: root.join("missing-executable"),
            endpoint: "unused".into(),
            status: "running".into(),
            supervisor_pid: Some(pid),
            child_pid: Some(pid),
            heartbeat_ms: 0,
            active_job: None,
            error: None,
        };
        fs::create_dir_all(directory(&worker)).unwrap();
        fs::create_dir_all(root.join("background-tasks")).unwrap();
        jobs::save(&directory(&worker).join("worker.json"), &worker).unwrap();
        jobs::save(
            &worker_path(root, &worker.session_id)
                .unwrap()
                .join("current.json"),
            &worker.id,
        )
        .unwrap();
        let mut claim = SessionClaim::acquire(root, &worker.session_id, &worker.id).unwrap();
        claim.committed = true;
        worker
    }

    fn dead_pid() -> u32 {
        let mut command = if cfg!(windows) {
            Command::new("cmd.exe")
        } else {
            Command::new("sh")
        };
        if cfg!(windows) {
            command.args(["/C", "exit 0"]);
        } else {
            command.args(["-c", "exit 0"]);
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(exited(pid));
        pid
    }

    #[test]
    fn corrupt_metadata_recovers_without_replaying_or_discarding_evidence() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        prepare_lifetime_locks(&directory(&worker)).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let job = begin_job(&worker, &id, "unfinished").unwrap();
        let state = jobs::directory(root.path(), &id)
            .unwrap()
            .join("state.json");
        let original = fs::read(&state).unwrap();
        for name in ["worker.json", "active.json"] {
            fs::write(directory(&worker).join(name), [0; 478]).unwrap();
        }
        let pointer = worker_path(root.path(), &worker.session_id)
            .unwrap()
            .join("current.json");
        fs::write(&pointer, [0; 38]).unwrap();
        assert!(recover_session(root.path(), &worker.session_id).unwrap());
        assert!(current(root.path(), &worker.session_id).unwrap().is_none());
        assert!(!recover_session(root.path(), &worker.session_id).unwrap());
        assert_eq!(
            fs::read(directory(&worker).join("worker.json")).unwrap(),
            vec![0; 478]
        );
        assert_eq!(
            fs::read(state.with_file_name("state.before-recovery.json")).unwrap(),
            original
        );
        assert_eq!(jobs::get(root.path(), &job.id).unwrap().status, "failed");
        assert!(lifetime_lock(&directory(&worker), "host.lock").is_err());
        assert!(SessionClaim::acquire(root.path(), &worker.session_id, "next").is_ok());
    }

    #[test]
    fn corrupt_task_and_claim_do_not_prevent_next_turn() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        prepare_lifetime_locks(&directory(&worker)).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        begin_job(&worker, &id, "unfinished").unwrap();
        let state = jobs::directory(root.path(), &id)
            .unwrap()
            .join("state.json");
        fs::write(&state, [0; 409]).unwrap();
        fs::write(
            root.path()
                .join("background-tasks")
                .join(format!("session-{}.claim", worker.session_id)),
            [0; 36],
        )
        .unwrap();
        assert!(recover_session(root.path(), &worker.session_id).unwrap());
        assert_eq!(fs::read(&state).unwrap(), vec![0; 409]);
        assert!(jobs::list(root.path(), Some(&worker.session_id))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn live_host_blocks_recovery_even_when_all_json_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        prepare_lifetime_locks(&directory(&worker)).unwrap();
        let _host = lifetime_lock(&directory(&worker), "host.lock").unwrap();
        fs::write(directory(&worker).join("worker.json"), [0; 478]).unwrap();
        assert!(recover_session(root.path(), &worker.session_id)
            .unwrap_err()
            .to_string()
            .contains("still running"));
        assert!(!directory(&worker).join("retired.json").exists());
        assert!(SessionClaim::acquire(root.path(), &worker.session_id, "other").is_err());
    }

    #[test]
    fn damaged_legacy_owner_is_not_assumed_dead() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), std::process::id());
        fs::write(directory(&worker).join("worker.json"), [0; 478]).unwrap();
        assert!(recover_session(root.path(), &worker.session_id).is_err());
        assert!(SessionClaim::acquire(root.path(), &worker.session_id, "other").is_err());
    }

    #[test]
    fn legacy_corrupt_record_from_previous_boot_recovers() {
        let Some(uptime) = uptime() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), std::process::id());
        let dir = directory(&worker);
        fs::write(dir.join("worker.json"), [0; 478]).unwrap();
        let before_boot = std::time::SystemTime::now() - uptime - Duration::from_secs(60);
        // Only file timestamps are used: directory timestamps can change when
        // diagnostics or recovery receipts are created after reboot.
        fs::OpenOptions::new()
            .write(true)
            .open(dir.join("worker.json"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before_boot))
            .unwrap();
        assert!(recover_session(root.path(), &worker.session_id).unwrap());
        assert!(current(root.path(), &worker.session_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_idle_worker_reaches_new_startup() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        // Missing executable proves startup was attempted, without launching a model.
        let error = ensure_worker(
            root.path(),
            &worker.executable,
            &worker.session_id,
            root.path(),
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains("heartbeat expired"));
        assert!(directory(&worker).join("retired.json").exists());
        assert_ne!(
            current(root.path(), &worker.session_id)
                .unwrap()
                .unwrap()
                .id,
            worker.id
        );
        assert!(!root
            .path()
            .join("background-tasks")
            .join(format!("session-{}.claim", worker.session_id))
            .exists());
    }

    #[test]
    fn live_or_unknown_owner_is_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let mut worker = fixture(root.path(), std::process::id());
        assert!(recover(&worker).is_err());
        worker.child_pid = None;
        assert!(recover(&worker).is_err());
        assert_eq!(read_worker(&directory(&worker)).unwrap().status, "running");
        assert!(SessionClaim::acquire(root.path(), &worker.session_id, "other").is_err());
    }

    #[test]
    fn unresolved_active_metadata_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        jobs::save(
            &directory(&worker).join("active.json"),
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        assert!(recover(&worker).is_err());
        assert_eq!(read_worker(&directory(&worker)).unwrap().status, "running");
        assert!(SessionClaim::acquire(root.path(), &worker.session_id, "other").is_err());
    }

    #[test]
    fn unfinished_job_blocks_recovery_but_finished_job_allows_it() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        let id = uuid::Uuid::new_v4().to_string();
        let mut job = begin_job(&worker, &id, "test prompt").unwrap();
        assert!(recover(&worker)
            .unwrap_err()
            .to_string()
            .contains("unfinished task"));
        job.finished_ms = Some(jobs::now());
        job.status = "failed".into();
        jobs::save(
            &jobs::directory(root.path(), &id)
                .unwrap()
                .join("state.json"),
            &job,
        )
        .unwrap();
        recover(&worker).unwrap();
        assert_eq!(jobs::get(root.path(), &id).unwrap().status, "failed");
        assert_eq!(read_worker(&directory(&worker)).unwrap().status, "stopped");
    }

    #[tokio::test]
    async fn concurrent_recovery_does_not_change_owner() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), dead_pid());
        let lock = fs::File::create(
            worker_path(root.path(), &worker.session_id)
                .unwrap()
                .join("startup.lock"),
        )
        .unwrap();
        crate::fs::lock_file(&lock).unwrap();
        let error = ensure_worker(
            root.path(),
            &worker.executable,
            &worker.session_id,
            root.path(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("already in progress"));
        assert_eq!(read_worker(&directory(&worker)).unwrap().status, "running");
    }
}
