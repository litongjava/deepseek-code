//! Exercise the Windows sharing violation seen when a reader holds state.json open.
#[cfg(windows)]
#[test]
fn a_locked_heartbeat_does_not_kill_the_child_and_exit_is_observable() {
    use deepseek_code_core::services::background_sessions::{owned_background_activity, spawn, Launch};
    use std::{
        fs,
        os::windows::fs::OpenOptionsExt,
        path::Path,
        time::{Duration, Instant},
    };

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "timed out awaiting supervisor fixture"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_dcl"));
    let launch = Launch {
        executable: "powershell.exe".into(),
        arguments: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "while (!(Test-Path done)) { Start-Sleep -Milliseconds 100 }; exit 0".into(),
        ],
        cwd: tmp.path().to_owned(),
    };
    let job = spawn(
        tmp.path(),
        exe,
        launch,
        "parent",
        "child",
        "heartbeat test",
        &[],
    )
    .unwrap();
    let dir = tmp.path().join("tasks/background").join(job.id);
    let state = dir.join("state.json");
    let read = |path: &Path| -> Option<serde_json::Value> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    };
    until(|| read(&state).is_some_and(|s| s["status"] == "running"));
    assert_eq!(owned_background_activity().0, 1);
    // Permit reads and writes but not deletion/replacement. Release once a write failed.
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&state)
        .unwrap();
    let diagnostics = dir.join("supervisor.stderr.log");
    until(|| {
        fs::read_to_string(&diagnostics)
            .unwrap_or_default()
            .contains("heartbeat write failed")
    });
    drop(lock);
    until(|| {
        fs::read_to_string(&diagnostics)
            .unwrap_or_default()
            .contains("recovered")
    });
    fs::write(tmp.path().join("done"), []).unwrap();
    until(|| read(&dir.join("supervisor.exit.json")).is_some_and(|s| s["exit_code"] == 0));
    until(|| matches!(owned_background_activity(), (0, Some(_))));
    let terminal = read(&state).unwrap();
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["exit_code"], 0);
}
