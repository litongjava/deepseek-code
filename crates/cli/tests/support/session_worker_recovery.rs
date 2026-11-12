use super::*;

/// Reports every descriptor this process still has open for the session's
/// startup lock. A duplicated or forked copy is what keeps the `flock` alive
/// after the logical owner dropped its `File`.
fn startup_lock_diagnostics(root: &Path, session: &str) -> String {
    let base = root.join("session-workers").join(session);
    let path = base.join("startup.lock");
    let canonical = std::fs::canonicalize(&path).ok();
    let mut open = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        for entry in entries.flatten() {
            let link = std::fs::read_link(entry.path()).ok();
            if link.as_ref().is_some_and(|link| canonical.as_ref() == Some(link)) {
                open.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map(|file| format!("{:?}", file.try_lock()))
        .unwrap_or_else(|error| format!("<{error}>"));
    format!(
        "startup.lock exists={} open_descriptors={open:?} try_lock={probe}",
        path.exists()
    )
}


/// Whether `pid` is still visible to the OS, and the kernel's process state when
/// it is. A `Z` state is a zombie: it still answers `kill(pid, 0)` but cannot
/// run, so `recovery::exited` reports it alive and an orphan host that waits for
/// its supervisor to disappear would never exit.
fn liveness(pid: u32) -> String {
    #[cfg(unix)]
    {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success());
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| {
                text.rsplit(')')
                    .next()
                    .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
            })
            .unwrap_or_else(|| "<unknown>".into());
        format!("kill0_visible={alive} state={state}")
    }
    #[cfg(not(unix))]
    {
        format!("pid={pid}")
    }
}

/// Who currently holds one lock file, without keeping it.
fn lock_state(path: &Path) -> String {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return "<missing>".into();
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = std::fs::File::unlock(&file);
            "free".into()
        }
        Err(std::fs::TryLockError::WouldBlock) => "held".into(),
        Err(std::fs::TryLockError::Error(error)) => format!("<{error}>"),
    }
}

/// Failure-only state for the orphan-exit wait. Local fixture paths only; worker
/// metadata is reduced to the ownership fields, never endpoint or configuration.
fn orphan_diagnostics(
    root: &Path,
    generation: &Path,
    owner: &worker::Worker,
    job: &str,
    last_error: Option<&anyhow::Error>,
) -> String {
    let supervisor = owner.supervisor_pid;
    let host = owner.child_pid;
    let metadata = std::fs::read(generation.join("worker.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .map(|value| {
            format!(
                "status={} supervisor={} child={} active_job={} heartbeat={}",
                value["status"],
                value["supervisor_pid"],
                value["child_pid"],
                value["active_job"],
                value["heartbeat_ms"]
            )
        })
        .unwrap_or_else(|| "<unreadable>".into());
    let job_state = jobs::get(root, job)
        .map(|job| {
            format!(
                "status={} finished={:?} error={:?} supervisor={:?} child={:?}",
                job.status, job.finished_ms, job.error, job.supervisor_pid, job.child_pid
            )
        })
        .unwrap_or_else(|error| format!("<{error}>"));
    let stderr = std::fs::read_to_string(generation.join("stderr.log"))
        .map(|text| {
            text.lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_else(|error| format!("<{error}>"));
    format!(
        "last_recover_error={:?} supervisor_pid={supervisor:?} [{}] host_pid={host:?} [{}] \
         host_lock={} supervisor_lock={} worker_json={metadata} \
         active_json={:?} current_json={:?} job={job_state} stderr_tail={stderr:?}",
        last_error.map(|error| format!("{error:#}")),
        supervisor.map(liveness).unwrap_or_else(|| "-".into()),
        host.map(liveness).unwrap_or_else(|| "-".into()),
        lock_state(&generation.join("host.lock")),
        lock_state(&generation.join("supervisor.lock")),
        std::fs::metadata(generation.join("active.json")).map(|m| m.len()).ok(),
        std::fs::metadata(generation.parent().unwrap().join("current.json"))
            .map(|m| m.len())
            .ok(),
    )
}

/// One bounded sample of what a live process is waiting on. Failures above used
/// to say only "still alive"; the wait channel separates a process that is
/// running its orphan check from one blocked elsewhere.
fn wait_state(pid: u32) -> String {
    #[cfg(target_os = "linux")]
    {
        let read = |path: String| std::fs::read_to_string(path).unwrap_or_default();
        let state = read(format!("/proc/{pid}/stat"))
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
            .unwrap_or_else(|| "?".into());
        let channel = read(format!("/proc/{pid}/wchan"));
        let channel = channel.trim();
        let channel = if channel.is_empty() { "?" } else { channel };
        let threads = std::fs::read_dir(format!("/proc/{pid}/task"))
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0);
        format!("state={state} wchan={channel} threads={threads}")
    }
    #[cfg(not(target_os = "linux"))]
    {
        format!("pid={pid}")
    }
}

/// Accumulates bounded samples of the orphan host during the existing wait, and
/// prints them only if that wait expires.
struct HostSamples {
    pid: Option<u32>,
    started: std::time::Instant,
    next: Duration,
    samples: Vec<String>,
}

impl HostSamples {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pid,
            started: std::time::Instant::now(),
            next: Duration::from_secs(1),
            samples: Vec::new(),
        }
    }

    /// At most one sample per second, for the length of the existing deadline.
    fn observe(&mut self) {
        if self.started.elapsed() < self.next {
            return;
        }
        self.next += Duration::from_secs(1);
        let Some(pid) = self.pid else { return };
        self.samples
            .push(format!("{:?}: {}", self.started.elapsed(), wait_state(pid)));
    }

    fn report(&self) -> String {
        self.samples.join(" | ")
    }
}

#[tokio::test]
async fn power_loss_metadata_damage_allows_new_request_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), temp.path().join("unused.js")));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = test_cli_executable();
    let old = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let mut cleanup = Cleanup(vec![old.supervisor_pid.unwrap()]);
    let old_job = submit(&old, &endpoint, "slow interrupted request", "fixture", 128).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    // Corruption while the executor is alive must not create a second executor.
    // The live supervisor rewrites worker.json every heartbeat, so this damage
    // can already be repaired by the time recovery looks: both "refused because
    // the record is damaged" and "nothing to recover, the record is valid again"
    // are correct. What must never happen is reporting a recovery while the host
    // that owns the session is still running.
    let dir = worker::directory(&old);
    std::fs::write(dir.join("worker.json"), [0; 478]).unwrap();
    let refusal = worker::recover_session(&paths.data_dir, &session);
    assert!(
        !matches!(refusal, Ok(true)),
        "recovery reported success while the executor was alive: {refusal:?}\n{}",
        orphan_diagnostics(&paths.data_dir, &dir, &old, &old_job, None)
    );
    assert!(
        !worker::process_exited(old.child_pid.unwrap()),
        "the first turn's host must still be running when recovery is refused"
    );
    drop(Cleanup(vec![old.supervisor_pid.unwrap()]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(dir.join("worker.json"), [0; 478]).unwrap();
    std::fs::write(dir.join("active.json"), [0; 38]).unwrap();
    std::fs::write(
        paths
            .data_dir
            .join("background-tasks")
            .join(&old_job)
            .join("state.json"),
        [0; 409],
    )
    .unwrap();
    std::fs::write(dir.parent().unwrap().join("current.json"), [0; 38]).unwrap();
    // A Unix host keeps its own process group, so it outlives the supervisor
    // until it notices the lost owner and exits by itself. Recovery stays refused
    // until then rather than starting a second executor.
    let deadline = Duration::from_secs(30);
    let mut last_error = None;
    let mut samples = HostSamples::new(old.child_pid);
    let recovered = tokio::time::timeout(deadline, async {
        loop {
            match worker::recover_session(&paths.data_dir, &session) {
                Ok(recovered) => break recovered,
                Err(error) => {
                    last_error = Some(error);
                    samples.observe();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the orphaned host must exit by itself within {deadline:?}\nhost_samples={}\n{}",
            samples.report(),
            orphan_diagnostics(
                &paths.data_dir,
                &dir,
                &old,
                &old_job,
                last_error.as_ref()
            )
        )
    });
    assert!(recovered);
    let fresh = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "ensure_worker after successful recovery: {error:#}\n{}",
                startup_lock_diagnostics(&paths.data_dir, &session)
            )
        });
    cleanup.0.push(fresh.supervisor_pid.unwrap());
    assert_ne!(fresh.id, old.id);
    let next = submit(&fresh, &endpoint, "new question", "fixture", 128).await;
    assert_eq!(completed(&paths.data_dir, &next).await.status, "completed");
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "old request must not be replayed"
    );
    assert_eq!(
        std::fs::read(dir.join("worker.json")).unwrap(),
        vec![0; 478]
    );
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    server.abort();
}
