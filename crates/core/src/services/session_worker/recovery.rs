use super::*;

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
fn exited(pid: u32) -> bool {
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
fn exited(pid: u32) -> bool {
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
        assert_eq!(read_worker(&directory(&worker)).unwrap().status, "stopped");
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
        lock.lock().unwrap();
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
