//! Real `dcl sessions live` runs: read-only inspection of the coordination
//! registry, with no provider, conversation database or registration of its own.

use std::path::Path;
use std::process::{Command, Output};

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn dcl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dcl"))
}

/// The CLI with the coordination/data environment removed and no stdin, so the
/// only inputs are the arguments a test adds.
fn clean_live_command() -> Command {
    let mut command = dcl();
    command
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null());
    command
}

/// Run the CLI with a clean coordination/data environment so the flags under
/// test are the only inputs.
fn run_live(args: &[&str], env: &[(&str, &str)], cwd: Option<&Path>) -> Run {
    let mut command = clean_live_command();
    command.args(["sessions", "live"]).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    finish(command.output().expect("the CLI runs"))
}

fn finish(output: Output) -> Run {
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn empty_text(root: &Path) -> String {
    format!(
        "Local coordination: {}\nCoverage: participating CLI sessions and persistent workers.\nNo live local sessions.\n",
        root.display()
    )
}

/// An absent registry is an ordinary empty view, and the command must not
/// create the coordination root, the data directory or any log.
#[test]
fn sessions_live_empty_does_not_create_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("coord");
    let data = temp.path().join("data");
    let root_arg = root.to_str().unwrap().to_owned();
    let data_arg = data.to_str().unwrap().to_owned();
    let args = ["--coordination-dir", &root_arg, "--data-dir", &data_arg];

    let text = run_live(&args, &[], None);
    assert!(text.status.success(), "stderr: {}", text.stderr);
    assert_eq!(text.stdout, empty_text(&root));
    assert!(text.stderr.is_empty());

    let json = run_live(&[&args[..], &["--json"]].concat(), &[], None);
    assert!(json.status.success(), "stderr: {}", json.stderr);
    let value: serde_json::Value = serde_json::from_str(json.stdout.trim()).unwrap();
    assert_eq!(value["format_version"], 1);
    assert_eq!(value["domain_id"], serde_json::Value::Null);
    assert_eq!(value["coverage"], "cli_and_persistent_workers");
    assert_eq!(value["groups"], serde_json::json!([]));
    assert_eq!(value["truncated"], false);
    assert_eq!(value["root"]["display"], root.display().to_string());
    assert_eq!(value["root"]["encoding"], "unix_bytes");
    assert!(value["observed_ms"].as_i64().unwrap() > 0);

    assert!(
        !root.exists(),
        "a read must not create the coordination root"
    );
    assert!(!data.exists(), "a read must not create the data directory");
    assert!(!temp.path().join("diagnostics/logs").exists());
}

#[cfg(target_os = "linux")]
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
#[cfg(target_os = "linux")]
use deepseek_code_core::services::local_coordination::WorkspaceIdentity;
#[cfg(target_os = "linux")]
use deepseek_code_core::services::local_coordination::{
    data_namespace, registry_snapshot, CoordinationPaths, Lifecycle, NativePath, ParticipantKind,
    ParticipantRole, Registration, RegistrationInput, RegistryGuard, Store,
};
#[cfg(target_os = "linux")]
use deepseek_code_core::services::session_worker as worker;
#[cfg(target_os = "linux")]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

/// One live incarnation in this test process, for the CLI to observe.
#[cfg(target_os = "linux")]
struct Participant {
    guard: RegistryGuard,
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn register(
    paths: &CoordinationPaths,
    namespace: &str,
    session: &str,
    role: ParticipantRole,
    worker: Option<&str>,
    workspace: WorkspaceIdentity,
    now_ms: i64,
) -> Participant {
    let guard = RegistryGuard::register(
        paths.clone(),
        RegistrationInput {
            data_namespace_id: namespace.to_owned(),
            session_id: Some(session.to_owned()),
            kind: ParticipantKind::Cli,
            role,
            parent_instance_id: None,
            worker_id: worker.map(Into::into),
            job_id: None,
            workspace,
        },
        now_ms,
    )
    .expect("the fixture registers");
    Participant { guard }
}

#[cfg(target_os = "linux")]
fn workspace(path: &str) -> WorkspaceIdentity {
    WorkspaceIdentity {
        workspace: NativePath::from_path(Path::new(path)),
        repo_id: None,
        worktree_id: None,
        branch_ref: None,
        observed_ms: 1,
    }
}

/// A helper process this test owns, reaped on every path including unwinding.
#[cfg(target_os = "linux")]
struct Helper {
    child: std::process::Child,
}

#[cfg(target_os = "linux")]
impl Helper {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait for a normal exit within `deadline`; `Drop` still ends it otherwise.
    fn wait_success(&mut self, deadline: std::time::Instant) {
        loop {
            match self.child.try_wait().expect("the helper is waitable") {
                Some(status) => {
                    assert!(status.success(), "the helper exited with {status}");
                    return;
                }
                None if std::time::Instant::now() >= deadline => {
                    panic!("the helper did not exit in time")
                }
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Helper {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[cfg(target_os = "linux")]
const HELPER_TEST: &str = "lc_registry_helper";
#[cfg(target_os = "linux")]
const HELPER_STARTUP: std::time::Duration = std::time::Duration::from_secs(30);

/// Start one real helper process that registers in `dir` and holds its
/// incarnation until the test writes `release`.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn spawn_registry_helper(
    dir: &Path,
    coord: &Path,
    data: &Path,
    session: &str,
    role: &str,
    worker: Option<&str>,
) -> Helper {
    std::fs::create_dir_all(dir).unwrap();
    let nonce = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    std::fs::write(dir.join("nonce"), &nonce).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(HELPER_TEST)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("DCL_LC_CLI_DIR", dir)
        .env("DCL_LC_CLI_NONCE", &nonce)
        .env("DCL_LC_CLI_COORD", coord)
        .env("DCL_LC_CLI_DATA", data)
        .env("DCL_LC_CLI_SESSION", session)
        .env("DCL_LC_CLI_ROLE", role)
        .env("DCL_LC_CLI_WORKER", worker.unwrap_or(""))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    Helper {
        child: command.spawn().expect("the helper starts"),
    }
}

/// Wait until the helper has registered and published its instance identity.
#[cfg(target_os = "linux")]
fn wait_helper_ready(helper: &mut Helper, dir: &Path) -> String {
    let deadline = std::time::Instant::now() + HELPER_STARTUP;
    loop {
        if let Ok(instance) = std::fs::read_to_string(dir.join("instance")) {
            return instance;
        }
        if let Some(status) = helper.child.try_wait().unwrap() {
            panic!("the helper exited before registering: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the helper did not register in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Two real helper processes in one session form one group with separate
/// owners; the same session id under another data namespace is a second group.
#[cfg(target_os = "linux")]
#[test]
fn sessions_live_groups_frontend_and_worker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("coord");
    let data_a = temp.path().join("data-a");
    let data_b = temp.path().join("data-b");
    for dir in [&data_a, &data_b] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let session = uuid::Uuid::new_v4().to_string();
    let namespace_a = data_namespace(&data_a).unwrap();
    let namespace_b = data_namespace(&data_b).unwrap();
    assert_ne!(namespace_a, namespace_b);

    // Start and confirm one helper at a time so the assertion is about
    // overlapping lifetimes, not about concurrent bootstrapping.
    let front_dir = temp.path().join("front");
    let mut front = spawn_registry_helper(&front_dir, &root, &data_a, &session, "frontend", None);
    let front_id = wait_helper_ready(&mut front, &front_dir);
    let exec_dir = temp.path().join("exec");
    let mut exec = spawn_registry_helper(
        &exec_dir,
        &root,
        &data_a,
        &session,
        "executor",
        Some("worker-1"),
    );
    let exec_id = wait_helper_ready(&mut exec, &exec_dir);

    let root_arg = root.to_str().unwrap().to_owned();
    let run = run_live(&["--json", "--coordination-dir", &root_arg], &[], None);
    assert!(run.status.success(), "stderr: {}", run.stderr);
    let value: serde_json::Value = serde_json::from_str(run.stdout.trim()).unwrap();
    let groups = value["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "one session in one namespace is one group");
    let group = &groups[0];
    assert_eq!(group["data_namespace_id"], namespace_a.as_str());
    assert_eq!(group["session_id"], session.as_str());
    let instances = group["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2);

    let ids: Vec<&str> = instances
        .iter()
        .map(|instance| instance["instance_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&front_id.as_str()));
    assert!(ids.contains(&exec_id.as_str()));
    // Separate owners are separate processes, not one process twice.
    let mut observed: Vec<u64> = instances
        .iter()
        .map(|instance| instance["owner"]["pid"].as_u64().unwrap())
        .collect();
    let mut expected = vec![front.pid() as u64, exec.pid() as u64];
    observed.sort_unstable();
    expected.sort_unstable();
    assert_eq!(observed, expected, "each owner is its own helper process");

    let roles: Vec<&str> = instances
        .iter()
        .map(|instance| instance["role"].as_str().unwrap())
        .collect();
    assert!(roles.contains(&"frontend"));
    assert!(roles.contains(&"executor"));
    assert!(instances
        .iter()
        .any(|instance| instance["worker_id"] == "worker-1"));
    let generations: Vec<&str> = instances
        .iter()
        .map(|instance| instance["owner"]["generation"].as_str().unwrap())
        .collect();
    assert_ne!(generations[0], generations[1], "owners are separate");
    assert!(instances
        .iter()
        .all(|instance| instance["owner_proof"] == "alive"));

    // The same session id under another data namespace is a second group.
    let other_dir = temp.path().join("other");
    let mut other = spawn_registry_helper(&other_dir, &root, &data_b, &session, "frontend", None);
    let other_id = wait_helper_ready(&mut other, &other_dir);
    let run = run_live(&["--json", "--coordination-dir", &root_arg], &[], None);
    assert!(run.status.success(), "stderr: {}", run.stderr);
    let value: serde_json::Value = serde_json::from_str(run.stdout.trim()).unwrap();
    let groups = value["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "another namespace is another group");
    let second = groups
        .iter()
        .find(|group| group["data_namespace_id"] == namespace_b.as_str())
        .expect("the other namespace group exists");
    let other_instances = second["instances"].as_array().unwrap();
    assert_eq!(other_instances.len(), 1);
    assert_eq!(other_instances[0]["instance_id"], other_id.as_str());

    // Release and reap one helper at a time, keeping the others alive.
    for (helper, dir) in [
        (&mut front, &front_dir),
        (&mut exec, &exec_dir),
        (&mut other, &other_dir),
    ] {
        std::fs::write(dir.join("release"), b"").unwrap();
        helper.wait_success(std::time::Instant::now() + std::time::Duration::from_secs(30));
        assert!(dir.join("closed").exists(), "the helper closed its row");
    }
}

/// The helper process: register in the shared root, hold until `release`, then
/// close. Infrastructure for the fixtures above, never an acceptance test.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "process helper: started only by the sessions live fixtures"]
fn lc_registry_helper() {
    let dir = std::path::PathBuf::from(std::env::var_os("DCL_LC_CLI_DIR").expect("helper dir"));
    let nonce = std::env::var("DCL_LC_CLI_NONCE").expect("helper nonce");
    assert_eq!(
        std::fs::read_to_string(dir.join("nonce")).expect("fixture nonce"),
        nonce
    );
    let coord = std::path::PathBuf::from(std::env::var_os("DCL_LC_CLI_COORD").expect("root"));
    let data = std::path::PathBuf::from(std::env::var_os("DCL_LC_CLI_DATA").expect("data dir"));
    let session = std::env::var("DCL_LC_CLI_SESSION").expect("session");
    let role = std::env::var("DCL_LC_CLI_ROLE").expect("role");
    let worker = std::env::var("DCL_LC_CLI_WORKER")
        .ok()
        .filter(|value| !value.is_empty());

    let paths = CoordinationPaths { root: coord };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let input = RegistrationInput {
            data_namespace_id: data_namespace(&data).expect("the data dir is usable"),
            session_id: Some(session),
            kind: ParticipantKind::Cli,
            role: if role == "executor" {
                ParticipantRole::Executor
            } else {
                ParticipantRole::Frontend
            },
            parent_instance_id: None,
            worker_id: worker,
            job_id: None,
            workspace: WorkspaceIdentity {
                workspace: NativePath::from_path(&dir),
                repo_id: None,
                worktree_id: None,
                branch_ref: None,
                observed_ms: 1,
            },
        };
        let mut guard =
            RegistryGuard::register(paths, input, 1_000).expect("the fixture registers");
        std::fs::write(dir.join("instance"), guard.instance_id()).unwrap();
        std::fs::write(dir.join("ready"), b"").unwrap();
        while !dir.join("release").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        guard.close().await.expect("the fixture closes");
        std::fs::write(dir.join("closed"), b"").unwrap();
    });
}

fn prepare(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
}

fn first_line(run: &Run) -> &str {
    run.stdout.lines().next().unwrap_or_default()
}

/// The explicit flag wins over the environment, the environment wins over the
/// home default, and an unrelated `--data-dir` or `-C` never moves the root.
/// A relative override resolves against the launch directory, not `-C`.
#[test]
fn sessions_live_respects_root_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let explicit = temp.path().join("explicit");
    let from_env = temp.path().join("from-env");
    let launch = temp.path().join("launch");
    let project = temp.path().join("project");
    let data = temp.path().join("unrelated-data");
    for dir in [&launch, &project] {
        prepare(dir);
    }
    let env = [("DEEPSEEK_CODE_COORDINATION_DIR", from_env.to_str().unwrap())];

    let run = run_live(
        &[
            "--coordination-dir",
            explicit.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "-C",
            project.to_str().unwrap(),
        ],
        &env,
        Some(&launch),
    );
    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert_eq!(
        first_line(&run),
        format!("Local coordination: {}", explicit.display())
    );

    // Without the flag the environment root is used, still ignoring `-C`.
    let run = run_live(
        &[
            "--data-dir",
            data.to_str().unwrap(),
            "-C",
            project.to_str().unwrap(),
        ],
        &env,
        Some(&launch),
    );
    assert_eq!(
        first_line(&run),
        format!("Local coordination: {}", from_env.display())
    );

    // A relative override is anchored at the launch directory.
    let run = run_live(
        &[
            "--coordination-dir",
            "relative-root",
            "-C",
            project.to_str().unwrap(),
        ],
        &[],
        Some(&launch),
    );
    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert_eq!(
        first_line(&run),
        format!(
            "Local coordination: {}",
            launch.join("relative-root").display()
        )
    );
}

/// Nothing but the registry is rendered: a newline, ESC and invalid UTF-8 in a
/// path stay escaped in text and lossless in JSON, and unrelated secrets or
/// prompt text never appear.
#[cfg(target_os = "linux")]
#[test]
fn sessions_live_redacts_and_escapes() {
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("coord");
    let paths = CoordinationPaths { root: root.clone() };
    let raw = b"/tmp/seek\x1bret\nline\xff".to_vec();
    let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&raw));
    let participant = register(
        &paths,
        &"c".repeat(64),
        &uuid::Uuid::new_v4().to_string(),
        ParticipantRole::Frontend,
        None,
        WorkspaceIdentity {
            workspace: NativePath::from_path(&path),
            repo_id: None,
            worktree_id: None,
            branch_ref: Some(b"refs/heads/\x1bbranch".to_vec()),
            observed_ms: 1,
        },
        1_000,
    );

    let root_arg = root.to_str().unwrap().to_owned();
    let env = [
        ("DEEPSEEK_CODE_CHILD_API_KEY", "SENTINEL-API-KEY"),
        (
            "DEEPSEEK_CODE_BACKGROUND_TOKEN",
            "SENTINEL-BACKGROUND-TOKEN",
        ),
    ];
    let text = run_live(
        &["--coordination-dir", &root_arg, "--model", "SENTINEL-MODEL"],
        &env,
        None,
    );
    assert!(text.status.success(), "stderr: {}", text.stderr);
    let owner_line = text
        .stdout
        .lines()
        .find(|line| line.contains("workspace="))
        .expect("the participant line is rendered");
    assert!(
        !owner_line.contains('\u{1b}'),
        "ESC must be escaped in text"
    );
    assert!(owner_line.contains("\\x1b"), "ESC is visible as an escape");
    assert!(
        owner_line.contains("\\x0a"),
        "a newline inside a path is escaped"
    );
    assert!(owner_line.contains("\\xff"), "invalid UTF-8 is escaped");
    assert!(owner_line.contains("branch=refs/heads/\\x1bbranch"));
    for sentinel in [
        "SENTINEL-API-KEY",
        "SENTINEL-BACKGROUND-TOKEN",
        "SENTINEL-MODEL",
    ] {
        assert!(
            !text.stdout.contains(sentinel),
            "{sentinel} leaked to stdout"
        );
        assert!(
            !text.stderr.contains(sentinel),
            "{sentinel} leaked to stderr"
        );
    }

    let json = run_live(&["--json", "--coordination-dir", &root_arg], &env, None);
    assert!(json.status.success(), "stderr: {}", json.stderr);
    let value: serde_json::Value = serde_json::from_str(json.stdout.trim()).unwrap();
    let instance = &value["groups"][0]["instances"][0];
    assert_eq!(instance["instance_id"], participant.guard.instance_id());
    let encoded = instance["workspace"]["base64"].as_str().unwrap();
    {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("the native bytes are base64");
        assert_eq!(decoded, raw, "JSON path bytes are lossless");
    }
    assert_eq!(
        instance["workspace"]["display"],
        "/tmp/seek\\x1bret\\x0aline\\xff"
    );
    assert_eq!(instance["branch_ref"]["display"], "refs/heads/\\x1bbranch");
    assert!(!json.stdout.contains('\u{1b}'), "JSON carries no raw ESC");
    drop(participant);
}

#[cfg(unix)]
fn private_file(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

/// A root the product itself creates: `prepare_private` makes it 0700 and
/// refuses to change the mode of a directory it did not create.
#[cfg(target_os = "linux")]
fn private_root(dir: &Path) -> CoordinationPaths {
    let paths = CoordinationPaths {
        root: dir.to_path_buf(),
    };
    paths.prepare_private().unwrap();
    paths
}

/// A failed read reports one fixed category, exits nonzero, and leaves every
/// piece of evidence and the conversation data directory untouched.
#[cfg(target_os = "linux")]
fn assert_category(root: &Path, data: &Path, expected: &str) {
    let run = run_live(
        &[
            "--coordination-dir",
            root.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
        &[],
        None,
    );
    assert!(
        !run.status.success(),
        "expected a nonzero exit for {expected}"
    );
    assert!(run.stdout.is_empty(), "no partial view: {}", run.stdout);
    assert_eq!(
        run.stderr,
        format!("Error: Local coordination unavailable: {expected}. No state was reset.\n"),
        "the category is exact and carries no raw error text"
    );
    assert!(!data.exists(), "no conversation database is created");
}

/// Corrupt, missing and newer-schema registries are reported and preserved:
/// inspection never resets, recreates or migrates anything.
#[cfg(target_os = "linux")]
#[test]
fn sessions_live_errors_do_not_reset() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");

    // Any rollback journal is unreadable evidence, not something to recover.
    let corrupt = private_root(&temp.path().join("corrupt"));
    private_file(
        &corrupt.root.join("coordination.sqlite3-journal"),
        b"evidence",
    );
    assert_category(&corrupt.root, &data, "corrupt");
    assert_eq!(
        std::fs::read(corrupt.root.join("coordination.sqlite3-journal")).unwrap(),
        b"evidence",
        "the journal is preserved"
    );

    // A marker without its database is a missing database, not an empty view.
    let missing = private_root(&temp.path().join("missing"));
    let domain = uuid::Uuid::new_v4().to_string();
    private_file(&missing.root.join("domain-id"), domain.as_bytes());
    assert_category(&missing.root, &data, "missing_database");
    assert_eq!(
        std::fs::read_to_string(missing.root.join("domain-id")).unwrap(),
        domain,
        "the marker is preserved"
    );

    #[cfg(target_os = "linux")]
    {
        let (newer, store) = the_newer_schema_fixture(&temp.path().join("newer"));
        assert_category(&newer.root, &data, "unsupported_schema");
        assert!(
            newer.root.join("coordination.sqlite3").exists(),
            "the database is preserved"
        );
        drop(store);
    }
}

/// A valid v1 registry whose recorded schema version is newer than this build.
///
/// The writer stays open so its WAL sidecars remain, which is what the reader
/// validates; the version bump goes through SQLite's own pragma.
#[cfg(target_os = "linux")]
fn the_newer_schema_fixture(
    dir: &Path,
) -> (
    deepseek_code_core::services::local_coordination::CoordinationPaths,
    deepseek_code_core::services::local_coordination::Store,
) {
    use deepseek_code_core::services::local_coordination::{CoordinationPaths, Store};
    let paths = CoordinationPaths {
        root: dir.to_path_buf(),
    };
    let store = Store::open_rw(&paths).unwrap();
    let connection = rusqlite::Connection::open(dir.join("coordination.sqlite3")).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    // The writable handle keeps its WAL sidecars alive for the read below.
    (paths, store)
}

/// Run a saved-session command with the same clean environment as `live`.
fn run_sessions(args: &[&str]) -> Run {
    let mut command = dcl();
    command
        .args(args)
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null());
    finish(command.output().expect("the CLI runs"))
}

/// Read-only saving commands keep their original JSON shapes and never create
/// coordination state.
#[test]
fn sessions_saved_list_behavior_is_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let coord = temp.path().join("coord");
    let data_arg = data.to_str().unwrap().to_owned();
    let coord_arg = coord.to_str().unwrap().to_owned();

    let list = run_sessions(&[
        "sessions",
        "list",
        "--data-dir",
        &data_arg,
        "--coordination-dir",
        &coord_arg,
    ]);
    assert!(list.status.success(), "stderr: {}", list.stderr);
    let value: serde_json::Value = serde_json::from_str(list.stdout.trim()).unwrap();
    assert_eq!(
        value,
        serde_json::json!([]),
        "an empty list is still an array"
    );
    assert!(
        !coord.exists(),
        "listing must not create coordination state"
    );

    // A saved session keeps the documented `{session, messages}` object.
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.clone()).unwrap();
    let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
    let id = store.create(&workspace, "fixture", "cli-runtime").unwrap();

    let show = run_sessions(&[
        "sessions",
        "show",
        &id,
        "--data-dir",
        &data_arg,
        "--coordination-dir",
        &coord_arg,
    ]);
    assert!(show.status.success(), "stderr: {}", show.stderr);
    let value: serde_json::Value = serde_json::from_str(show.stdout.trim()).unwrap();
    assert!(
        value.get("session").is_some(),
        "the session key is unchanged"
    );
    assert!(
        value.get("messages").is_some(),
        "the messages key is unchanged"
    );
    assert_eq!(value["session"]["id"], id);
    assert!(
        !coord.exists(),
        "showing must not create coordination state"
    );
}

/// Sorted entry names of one directory, for before/after comparisons.
#[cfg(unix)]
fn dir_entry_names(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

/// A real native `--coordination-dir` value reaches the resolver: a missing
/// root whose own name contains ESC, LF and invalid UTF-8 is accepted in both
/// argument positions, rendered escaped in text and losslessly in JSON.
#[cfg(target_os = "linux")]
#[test]
fn sessions_live_accepts_non_utf8_coordination_dir_argv() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let temp = tempfile::tempdir().unwrap();
    let parent = std::fs::canonicalize(temp.path()).unwrap();
    let leaf = std::ffi::OsString::from_vec(b"coord-\x1b-\n-\xff".to_vec());
    let root = parent.join(&leaf);
    let data = parent.join("missing-data");
    let names_before = dir_entry_names(&parent);

    // Global flag before the subcommand, JSON output.
    let output = clean_live_command()
        .arg("--coordination-dir")
        .arg(&root)
        .args(["sessions", "live", "--json", "--data-dir"])
        .arg(&data)
        .output()
        .expect("the CLI runs");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(
        output.stderr.is_empty(),
        "stderr must be empty: {:?}",
        output.stderr
    );

    // Byte-level framing first, parsing second.
    let raw = &output.stdout;
    assert_eq!(raw.first(), Some(&b'{'), "JSON must start with an object");
    assert!(
        raw.ends_with(b"}\n"),
        "JSON must end with one closing brace and LF"
    );
    assert_eq!(
        raw.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "exactly one LF"
    );
    assert!(!raw.contains(&0x1b), "no raw ESC in JSON");
    assert!(!raw.contains(&0xff), "no raw invalid byte in JSON");

    let value: serde_json::Value = serde_json::from_slice(raw).expect("the JSON parses");
    assert_eq!(value["root"]["encoding"], "unix_bytes");
    let display = value["root"]["display"].as_str().expect("display");
    assert!(
        display.ends_with("coord-\\x1b-\\x0a-\\xff"),
        "escaped display: {display}"
    );
    {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value["root"]["base64"].as_str().expect("base64"))
            .expect("the base64 decodes");
        assert_eq!(
            decoded,
            root.as_os_str().as_bytes(),
            "the resolved root round-trips losslessly"
        );
    }

    // Flag after the subcommand, text output.
    let output = clean_live_command()
        .args(["sessions", "live", "--coordination-dir"])
        .arg(&root)
        .arg("--data-dir")
        .arg(&data)
        .output()
        .expect("the CLI runs");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b), "no raw ESC in text");
    assert!(
        !output.stdout.contains(&0xff),
        "no raw invalid byte in text"
    );
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        3,
        "the empty view is exactly three lines"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("Local coordination: ") && first.ends_with("coord-\\x1b-\\x0a-\\xff"),
        "escaped first line: {first}"
    );

    assert!(
        !root.exists(),
        "a read must not create the coordination root"
    );
    assert!(!data.exists(), "a read must not create the data directory");
    assert_eq!(dir_entry_names(&parent), names_before);
}

/// A root lookup that fails on a native path is reported with the fixed
/// category, one line, no path text and no state change.
#[cfg(target_os = "linux")]
#[test]
fn sessions_live_invalid_native_root_error_is_bounded() {
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir().unwrap();
    let parent = std::fs::canonicalize(temp.path()).unwrap();
    let leaf = std::ffi::OsString::from_vec(b"private-sentinel-\x1b[31m\n\xff".to_vec());
    let file = parent.join(&leaf);
    std::fs::write(&file, b"evidence-bytes").unwrap();
    // A regular file cannot be an ancestor of the coordination root.
    let root = file.join("coord");
    let data = parent.join("missing-data");
    let names_before = dir_entry_names(&parent);

    let output = clean_live_command()
        .arg("--coordination-dir")
        .arg(&root)
        .args(["sessions", "live", "--json", "--data-dir"])
        .arg(&data)
        .output()
        .expect("the CLI runs");

    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "no partial view on stderr failures"
    );
    assert_eq!(
        output.stderr, b"Error: Local coordination unavailable: unavailable. No state was reset.\n",
        "the fixed line carries no ESC, path text, extra newline or panic"
    );

    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"evidence-bytes",
        "the evidence file is untouched"
    );
    assert_eq!(dir_entry_names(&parent), names_before);
    assert!(!data.exists(), "no data directory is created");
}

/// The private sandbox-bridge role still dispatches before ordinary parsing:
/// missing operands fail with the fixed message and exit 126.
#[cfg(unix)]
#[test]
fn sandbox_bridge_exact_role_still_dispatches() {
    let output = Command::new(env!("CARGO_BIN_EXE_dcl"))
        .arg("__sandbox-bridge")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the CLI runs");
    assert_eq!(output.status.code(), Some(126));
    assert!(output.stdout.is_empty(), "stdout must be empty");
    assert_eq!(output.stderr, b"sandbox bridge: socket path missing\n");
}

// ---------------------------------------------------------------------------
// Round 06: real frontend and persistent-worker registration lifetimes.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct Gate {
    requests: Arc<AtomicUsize>,
    gated_requests: Arc<AtomicUsize>,
    gated: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::watch::Sender<bool>>,
}

/// The local model fixture. A request containing the gate marker is held until
/// the test has inspected the registry, so every participant is observed while
/// the turn is still in flight.
#[cfg(target_os = "linux")]
async fn gated_turn(
    axum::extract::State(gate): axum::extract::State<Gate>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    gate.requests.fetch_add(1, Ordering::SeqCst);
    if body["messages"].to_string().contains("ROUND06_GATE") {
        gate.gated_requests.fetch_add(1, Ordering::SeqCst);
        gate.gated.add_permits(1);
        let mut release = gate.release.subscribe();
        let _ = release.wait_for(|released| *released).await;
    }
    let frames = serde_json::json!([
        {"choices":[{"index":0,"delta":{"content":"round-06 fixture turn complete"},"finish_reason":null}]},
        {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}
    ]);
    let mut stream = String::new();
    for frame in frames.as_array().unwrap() {
        stream.push_str(&format!("data: {frame}\n\n"));
    }
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[cfg(target_os = "linux")]
async fn start_gate() -> (String, Gate, tokio::task::JoinHandle<()>) {
    let (release, _) = tokio::sync::watch::channel(false);
    let gate = Gate {
        requests: Arc::new(AtomicUsize::new(0)),
        gated_requests: Arc::new(AtomicUsize::new(0)),
        gated: Arc::new(tokio::sync::Semaphore::new(0)),
        release: Arc::new(release),
    };
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(gated_turn))
        .with_state(gate.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, gate, server)
}

#[cfg(target_os = "linux")]
fn marker(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), b"").unwrap();
}

#[cfg(target_os = "linux")]
async fn wait_markers(dir: &Path, names: &[&str], timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        for name in names {
            if dir.join(name).exists() {
                return (*name).to_owned();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for one of {names:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Exact identity of one test-owned process: state and start tick.
#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let parts: Vec<_> = stat.rsplit(')').next()?.split_whitespace().collect();
    Some((parts[0].chars().next()?, parts[19].parse().ok()?))
}

#[cfg(target_os = "linux")]
fn process_exited(pid: u32, tick: u64) -> bool {
    process_identity(pid).is_none_or(|(state, observed)| observed != tick || state == 'Z')
}

/// Kills only the exact processes this fixture started, by PID and start tick.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct ExactCleanup(Vec<(u32, u64)>);

#[cfg(target_os = "linux")]
impl ExactCleanup {
    fn add(&mut self, pid: Option<u32>) {
        if let Some(pid) = pid {
            if let Some((_, tick)) = process_identity(pid) {
                self.0.push((pid, tick));
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ExactCleanup {
    fn drop(&mut self) {
        for (pid, tick) in &self.0 {
            if !process_exited(*pid, *tick) {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", "--", &pid.to_string()])
                    .status();
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn create_session(data: &Path, workspace: &Path) -> String {
    std::fs::create_dir_all(workspace).unwrap();
    let paths = RuntimePaths::new(data.to_path_buf()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    store
        .create(workspace, "fixture-model", "cli-runtime")
        .unwrap()
}

#[cfg(target_os = "linux")]
fn write_settings(data: &Path, enabled: bool) {
    std::fs::create_dir_all(data).unwrap();
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/settings.json"),
        format!(r#"{{"localCoordination":{{"enabled":{enabled},"registry":true}}}}"#),
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
#[path = "support/linux_terminal.rs"]
mod linux_terminal;

#[cfg(target_os = "linux")]
use linux_terminal::write_provider;

#[cfg(target_os = "linux")]
fn rows_in(root: &Path) -> Vec<Registration> {
    let paths = CoordinationPaths {
        root: root.to_path_buf(),
    };
    match Store::open_read(&paths).unwrap() {
        Some(store) => store.rows(1000).unwrap(),
        None => Vec::new(),
    }
}

/// Polling view: a peer's bootstrap may briefly make the store unreadable.
#[cfg(target_os = "linux")]
fn poll_rows(root: &Path) -> Vec<Registration> {
    let paths = CoordinationPaths {
        root: root.to_path_buf(),
    };
    match Store::open_read(&paths) {
        Ok(Some(store)) => store.rows(1000).unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(target_os = "linux")]
fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
const LIFECYCLE_HELPER: &str = "lc_registry_lifecycle_helper";

/// One process helper this test starts. It owns the worker and frontend
/// processes so the parent never mutates the process-global environment.
#[cfg(target_os = "linux")]
struct LifecycleHelper {
    child: tokio::process::Child,
}

#[cfg(target_os = "linux")]
impl LifecycleHelper {
    fn spawn(mode: &str, fixture: &Path, endpoint: &str, root: &Path) -> Self {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg(LIFECYCLE_HELPER)
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("DCL_LC_LIFECYCLE_MODE", mode)
            .env("DCL_LC_FIXTURE_DIR", fixture)
            .env("DCL_LC_ENDPOINT", endpoint)
            .env("DCL_LC_COORD", root)
            // The worker host inherits this from us, independent of its data dir.
            .env("DEEPSEEK_CODE_COORDINATION_DIR", root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        Self {
            child: command.spawn().expect("the lifecycle helper starts"),
        }
    }

    async fn wait_success(&mut self, timeout: Duration) -> bool {
        matches!(
            tokio::time::timeout(timeout, self.child.wait()).await,
            Ok(Ok(status)) if status.success()
        )
    }
}

#[cfg(target_os = "linux")]
impl Drop for LifecycleHelper {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// The helper process: one real frontend plus one real persistent worker, or a
/// worker restart across two data directories. Infrastructure, never an
/// acceptance test.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "process helper: started only by the registry lifetime fixtures"]
fn lc_registry_lifecycle_helper() {
    let mode = std::env::var("DCL_LC_LIFECYCLE_MODE").expect("helper mode");
    let fixture =
        std::path::PathBuf::from(std::env::var_os("DCL_LC_FIXTURE_DIR").expect("fixture"));
    let endpoint = std::env::var("DCL_LC_ENDPOINT").expect("endpoint");
    let root = std::path::PathBuf::from(std::env::var_os("DCL_LC_COORD").expect("root"));
    assert_eq!(
        std::env::var_os("DEEPSEEK_CODE_COORDINATION_DIR"),
        Some(root.clone().into_os_string()),
        "the helper was started with the inherited root"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        match mode.as_str() {
            "frontend_worker" => lifecycle_frontend_worker(fixture, endpoint).await,
            "restart" => lifecycle_restart(fixture).await,
            "branch_refresh" => lifecycle_branch_refresh(fixture, endpoint).await,
            "same_session_restart" => lifecycle_same_session_restart(fixture).await,
            other => panic!("unknown lifecycle helper mode {other}"),
        }
    });
}

#[cfg(target_os = "linux")]
async fn lifecycle_frontend_worker(fixture: std::path::PathBuf, endpoint: String) {
    let data = fixture.join("data");
    let workspace = fixture.join("workspace");
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
    let session = create_session(&data, &workspace);
    std::fs::write(fixture.join("session"), &session).unwrap();

    // A real stream-json frontend opens the session and stays idle: it never
    // submits the turn, which the persistent worker owns.
    let frontend_log = std::fs::File::create(fixture.join("frontend-stderr.log")).unwrap();
    let mut frontend = tokio::process::Command::new(&executable)
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            workspace.to_str().unwrap(),
            "--resume",
            session.as_str(),
            "--base-url",
            format!("{endpoint}/v1").as_str(),
            "--api-format",
            "openai",
            "--api-key-env",
            "DCL_LC_SDK_KEY",
            "--model",
            "fixture-model",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--print",
            "--permission-mode",
            "accept-edits",
        ])
        .env("DCL_LC_SDK_KEY", "fixture-only")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(frontend_log)
        .kill_on_drop(true)
        .spawn()
        .expect("the SDK frontend starts");
    let mut frontend_stdin = frontend.stdin.take();
    marker(&fixture, "frontend-started");

    let wait_start = wait_markers(&fixture, &["start-worker"], Duration::from_secs(120));
    tokio::pin!(wait_start);
    tokio::select! {
        _ = &mut wait_start => {}
        status = frontend.wait() => {
            let status = status.map(|status| status.to_string()).unwrap_or_default();
            std::fs::write(fixture.join("frontend-exited"), &status).unwrap();
            return;
        }
    }
    let host = worker::ensure_worker(&data, &executable, &session, &workspace)
        .await
        .expect("the worker host starts");
    let mut cleanup = ExactCleanup::default();
    cleanup.add(host.supervisor_pid);
    cleanup.add(host.child_pid);
    std::fs::write(
        fixture.join("worker-info.json"),
        serde_json::json!({
            "id": host.id,
            "session": session,
            "supervisor_pid": host.supervisor_pid,
            "health_pid": host.child_pid,
        })
        .to_string(),
    )
    .unwrap();
    marker(&fixture, "worker-started");

    let job_id = uuid::Uuid::new_v4().to_string();
    let prompt = "ROUND06_GATE worker turn".to_owned();
    let arguments = vec![
        "--data-dir".into(),
        data.display().to_string(),
        "--resume".into(),
        session.clone(),
        "--base-url".into(),
        format!("{endpoint}/v1"),
        "--api-format".into(),
        "openai".into(),
        "--api-key-env".into(),
        "DEEPSEEK_CODE_CHILD_API_KEY".into(),
        "--model".into(),
        "fixture-model".into(),
        "--permission-mode".into(),
        "accept-edits".into(),
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--".into(),
        prompt.clone(),
    ];
    worker::call(
        &host,
        &worker::Request::Submit {
            id: job_id.clone(),
            prompt,
            arguments,
            environment: vec![("DEEPSEEK_CODE_CHILD_API_KEY".into(), "fixture-only".into())],
        },
    )
    .await
    .expect("the worker admits the turn");
    marker(&fixture, "worker-submitted");

    // The parent decides whether this frontend is closed normally or killed
    // while the worker's turn is still gated.
    let choice = wait_markers(
        &fixture,
        &["kill-frontend", "close-frontend"],
        Duration::from_secs(120),
    )
    .await;
    if choice == "kill-frontend" {
        let _ = frontend.start_kill();
        let _ = frontend.wait().await;
        marker(&fixture, "frontend-killed");
    } else {
        drop(frontend_stdin.take());
        let _ = tokio::time::timeout(Duration::from_secs(10), frontend.wait()).await;
        let _ = frontend.start_kill();
    }

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = deepseek_code_core::services::background_sessions::get(&data, &job_id).unwrap();
        if job.finished_ms.is_some() {
            assert_eq!(job.status, "completed", "{job:?}");
            break;
        }
        assert!(Instant::now() < deadline, "the gated turn never finished");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    worker::shutdown(&data, &session).await.unwrap();
    drop(cleanup);
    marker(&fixture, "done");
}

#[cfg(target_os = "linux")]
async fn lifecycle_restart(fixture: std::path::PathBuf) {
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
    let mut hosts = Vec::new();
    for (name, data_name) in [("a", "data-a"), ("b", "data-b")] {
        let data = fixture.join(data_name);
        let workspace = fixture.join(format!("workspace-{name}"));
        let session = create_session(&data, &workspace);
        if name == "a" {
            std::fs::write(fixture.join("session-a"), &session).unwrap();
            let host = worker::ensure_worker(&data, &executable, &session, &workspace)
                .await
                .unwrap();
            std::fs::write(
                fixture.join("worker-a.json"),
                serde_json::json!({"id": host.id, "session": session}).to_string(),
            )
            .unwrap();
            hosts.push(host);
            marker(&fixture, "first-started");
            wait_markers(&fixture, &["stop-first"], Duration::from_secs(120)).await;
            worker::shutdown(&data, &session).await.unwrap();
            hosts.clear();
            marker(&fixture, "first-stopped");
        } else {
            std::fs::write(fixture.join("session-b"), &session).unwrap();
            wait_markers(&fixture, &["start-second"], Duration::from_secs(120)).await;
            let host = worker::ensure_worker(&data, &executable, &session, &workspace)
                .await
                .unwrap();
            std::fs::write(
                fixture.join("worker-b.json"),
                serde_json::json!({"id": host.id, "session": session}).to_string(),
            )
            .unwrap();
            hosts.push(host);
            marker(&fixture, "second-started");
            wait_markers(&fixture, &["release"], Duration::from_secs(120)).await;
            worker::shutdown(&data, &session).await.unwrap();
            hosts.clear();
        }
    }
    marker(&fixture, "done");
}

/// A real disabled session performs its normal turn and creates no coordination
/// state or extra model request.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_disabled_real_session_creates_no_state() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&workspace).unwrap();
    write_settings(&data, false);
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            workspace.to_str().unwrap(),
            "--coordination-dir",
            root.to_str().unwrap(),
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--print",
            "--output-format",
            "json",
            "--permission-mode",
            "accept-edits",
            "--",
            "a normal disabled-registry turn",
        ])
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("the CLI runs");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the turn result is JSON");
    assert_eq!(result["is_error"], false);
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1, "one model request");
    assert!(
        !root.exists(),
        "disabled coordination must not create a root"
    );
    assert!(rows_in(&root).is_empty());
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Local coordination"),
        "a disabled startup says nothing about coordination"
    );
    server.abort();
}

/// A corrupt root warns once, creates nothing, keeps the turn's execution lock
/// in force and lets the original turn succeed exactly once.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_failure_does_not_break_session() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&workspace).unwrap();
    write_settings(&data, true);
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let paths = CoordinationPaths { root: root.clone() };
    paths.prepare_private().unwrap();
    private_file(&root.join("coordination.sqlite3-journal"), b"evidence");
    let before = dir_entry_names(&root);

    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            workspace.to_str().unwrap(),
            "--coordination-dir",
            root.to_str().unwrap(),
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--print",
            "--output-format",
            "json",
            "--permission-mode",
            "accept-edits",
            "--",
            "ROUND06_GATE corrupt registry turn",
        ])
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the CLI starts");

    // While the turn is gated the execution lease is live: a second acquirer
    // must be refused rather than bypassing it because registration failed.
    let _permit = tokio::time::timeout(Duration::from_secs(60), gate.gated.acquire())
        .await
        .expect("the turn reaches the model")
        .expect("the gate is open");
    let session = SessionStore::new(
        RuntimePaths::new(data.clone())
            .unwrap()
            .open_database()
            .unwrap(),
    )
    .latest(&workspace)
    .unwrap()
    .expect("the turn created a session");
    assert!(
        deepseek_code_core::db::session_store::Stores::open(&data).unwrap().open_session(&session.id).unwrap().acquire_execution()
            .is_err(),
        "the failed registration must not bypass the execution lease"
    );
    gate.release.send(true).unwrap();

    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("the CLI exits")
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["is_error"], false);
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1, "one model request");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("Local coordination registry unavailable"))
        .collect();
    assert_eq!(
        warnings,
        vec![
            "Local coordination registry unavailable: corrupt. Session continues without peer visibility."
        ],
        "one bounded warning, no raw error chain"
    );
    assert_eq!(
        std::fs::read(root.join("coordination.sqlite3-journal")).unwrap(),
        b"evidence",
        "the corrupt evidence is preserved"
    );
    assert_eq!(
        dir_entry_names(&root),
        before,
        "nothing was created or reset"
    );
    server.abort();
}

#[cfg(target_os = "linux")]
async fn wait_frontend_row(
    root: &Path,
    session: &str,
    fixture: &Path,
    timeout: Duration,
) -> Registration {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(row) = poll_rows(root).into_iter().find(|row| {
            row.session_id.as_deref() == Some(session)
                && row.role == ParticipantRole::Frontend
                && row.lifecycle == Lifecycle::Open
        }) {
            return row;
        }
        if let Ok(status) = std::fs::read_to_string(fixture.join("frontend-exited")) {
            panic!(
                "the frontend exited before registering ({status}): {}",
                std::fs::read_to_string(fixture.join("frontend-stderr.log")).unwrap_or_default()
            );
        }
        assert!(Instant::now() < deadline, "timed out waiting for Frontend");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A real frontend and the real persistent worker host form one group with two
/// distinct incarnations, exactly one host Executor and no third session row.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_real_frontend_and_worker_share_group() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&fixture).unwrap();
    write_settings(&fixture.join("data"), true);
    let (endpoint, gate, server) = start_gate().await;

    let mut helper = LifecycleHelper::spawn("frontend_worker", &fixture, &endpoint, &root);
    wait_markers(&fixture, &["frontend-started"], Duration::from_secs(60)).await;
    let session = std::fs::read_to_string(fixture.join("session")).unwrap();
    let frontend = wait_frontend_row(&root, &session, &fixture, Duration::from_secs(120)).await;

    marker(&fixture, "start-worker");
    wait_markers(&fixture, &["worker-started"], Duration::from_secs(120)).await;
    let worker_info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("worker-info.json")).unwrap()).unwrap();

    // Both participants exist while the worker's only turn is still gated.
    let _permit = tokio::time::timeout(Duration::from_secs(120), gate.gated.acquire())
        .await
        .expect("the worker reaches the model")
        .expect("the gate is open");
    let rows = rows_in(&root);
    let group: Vec<&Registration> = rows
        .iter()
        .filter(|row| {
            row.session_id.as_deref() == Some(&session) && row.lifecycle == Lifecycle::Open
        })
        .collect();
    assert_eq!(group.len(), 2, "one frontend and one host executor");
    let executor = group
        .iter()
        .find(|row| row.role == ParticipantRole::Executor)
        .expect("the host executor is registered");
    assert_eq!(group[0].data_namespace_id, group[1].data_namespace_id);
    assert_eq!(frontend.kind, ParticipantKind::Cli);
    assert_eq!(executor.kind, ParticipantKind::BackgroundAgent);
    assert_eq!(
        executor.worker_id.as_deref(),
        Some(worker_info["id"].as_str().unwrap()),
        "the registered worker id is the host's validated incarnation UUID"
    );
    assert_ne!(frontend.instance_id, executor.instance_id);
    assert_ne!(frontend.owner.generation, executor.owner.generation);
    assert_ne!(
        frontend.owner.pid, executor.owner.pid,
        "the frontend and the host executor are separate processes"
    );

    gate.release.send(true).unwrap();
    marker(&fixture, "close-frontend");
    assert!(
        helper.wait_success(Duration::from_secs(120)).await,
        "the fixture frontend and worker exit normally"
    );
    assert_eq!(
        gate.requests.load(Ordering::SeqCst),
        1,
        "the gated turn made exactly one model request"
    );
    for row in rows_in(&root)
        .iter()
        .filter(|row| row.session_id.as_deref() == Some(&session))
    {
        assert_eq!(
            row.lifecycle,
            Lifecycle::Closed,
            "normal exit closes its row"
        );
    }
    server.abort();
}

/// Killing the frontend while its worker's turn is gated leaves the executor
/// alive and visible, its execution lease held, and the killed owner `Gone`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_worker_survives_frontend_exit() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&fixture).unwrap();
    write_settings(&fixture.join("data"), true);
    let (endpoint, gate, server) = start_gate().await;

    let mut helper = LifecycleHelper::spawn("frontend_worker", &fixture, &endpoint, &root);
    wait_markers(&fixture, &["frontend-started"], Duration::from_secs(60)).await;
    let session = std::fs::read_to_string(fixture.join("session")).unwrap();
    wait_frontend_row(&root, &session, &fixture, Duration::from_secs(120)).await;
    marker(&fixture, "start-worker");
    wait_markers(&fixture, &["worker-started"], Duration::from_secs(120)).await;
    let _permit = tokio::time::timeout(Duration::from_secs(120), gate.gated.acquire())
        .await
        .expect("the worker reaches the model")
        .expect("the gate is open");

    marker(&fixture, "kill-frontend");
    wait_markers(&fixture, &["frontend-killed"], Duration::from_secs(60)).await;

    let paths = CoordinationPaths { root: root.clone() };
    let snapshot = registry_snapshot(&paths, wall_clock_ms()).unwrap();
    let group = snapshot
        .groups
        .iter()
        .find(|group| group.session_id.as_deref() == Some(&session))
        .expect("the shared group is visible");
    let frontend = group
        .instances
        .iter()
        .find(|view| view.registration.role == ParticipantRole::Frontend)
        .expect("the killed frontend row is still visible");
    let executor = group
        .instances
        .iter()
        .find(|view| view.registration.role == ParticipantRole::Executor)
        .expect("the executor row is still visible");
    assert_eq!(
        frontend.owner_proof,
        deepseek_code_core::services::local_coordination::OwnerProof::Gone,
        "owner proof reports the killed frontend as gone"
    );
    assert_eq!(
        executor.owner_proof,
        deepseek_code_core::services::local_coordination::OwnerProof::Alive,
        "the persistent worker survives its frontend"
    );

    // The running turn still owns the execution lease; no one bypassed it.
    assert!(
        deepseek_code_core::db::session_store::Stores::open(&fixture.join("data")).unwrap().open_session(&session).unwrap().acquire_execution()
        .is_err(),
        "the executor's execution lease stays active"
    );
    let active =
        deepseek_code_core::services::background_sessions::list(&fixture.join("data"), None)
            .unwrap()
            .into_iter()
            .find(|job| job.session_id == session && job.finished_ms.is_none())
            .expect("the worker's turn is still running");
    assert!(matches!(active.status.as_str(), "starting" | "running"));

    gate.release.send(true).unwrap();
    marker(&fixture, "release-worker");
    assert!(helper.wait_success(Duration::from_secs(120)).await);
    assert_eq!(
        gate.requests.load(Ordering::SeqCst),
        1,
        "the killed frontend never caused a second request"
    );
    // The killed frontend left an open row that owner proof reports as gone;
    // the executor closed normally.
    let rows = rows_in(&root);
    let executor_row = rows
        .iter()
        .find(|row| {
            row.session_id.as_deref() == Some(&session) && row.role == ParticipantRole::Executor
        })
        .expect("the executor row exists");
    assert_eq!(executor_row.lifecycle, Lifecycle::Closed);
    server.abort();
}

/// Relaunching a stopped worker under the same inherited root creates a fresh
/// incarnation and generation even when the new host uses another data dir.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_worker_restart_has_new_owner() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&fixture).unwrap();
    write_settings(&fixture.join("data-a"), true);
    write_settings(&fixture.join("data-b"), true);
    let (endpoint, _gate, server) = start_gate().await;

    let mut helper = LifecycleHelper::spawn("restart", &fixture, &endpoint, &root);
    wait_markers(&fixture, &["first-started"], Duration::from_secs(120)).await;
    let first: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("worker-a.json")).unwrap()).unwrap();
    let first_id = first["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    let first_row = loop {
        if let Some(row) = poll_rows(&root)
            .into_iter()
            .find(|row| row.worker_id.as_deref() == Some(first_id.as_str()))
        {
            break row;
        }
        assert!(Instant::now() < deadline, "the first host is registered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let first_instance = first_row.instance_id.clone();
    assert_eq!(first_row.lifecycle, Lifecycle::Open);

    marker(&fixture, "stop-first");
    wait_markers(&fixture, &["first-stopped"], Duration::from_secs(120)).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let closed = poll_rows(&root)
            .into_iter()
            .find(|row| row.instance_id == first_instance)
            .is_some_and(|row| row.lifecycle == Lifecycle::Closed);
        if closed {
            break;
        }
        assert!(Instant::now() < deadline, "the old host row never closed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    marker(&fixture, "start-second");
    wait_markers(&fixture, &["second-started"], Duration::from_secs(120)).await;
    let second: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("worker-b.json")).unwrap()).unwrap();
    let second_id = second["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    let second_row = loop {
        if let Some(row) = poll_rows(&root)
            .into_iter()
            .find(|row| row.worker_id.as_deref() == Some(second_id.as_str()))
        {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the second host registered under the same inherited root"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let second_instance = second_row.instance_id.clone();
    assert_eq!(second_row.lifecycle, Lifecycle::Open);
    assert_ne!(
        first_instance, second_instance,
        "a restart is a fresh incarnation"
    );
    assert_ne!(first_id, second_id, "a restart is a fresh worker instance");
    assert_ne!(
        first_row.owner.generation, second_row.owner.generation,
        "a restart takes a fresh owner generation"
    );
    assert_ne!(
        first_row.data_namespace_id, second_row.data_namespace_id,
        "separate data dirs stay separate groups"
    );
    assert_eq!(
        poll_rows(&root)
            .iter()
            .filter(|row| row.instance_id == first_instance)
            .count(),
        1,
        "the old row is not replaced by a duplicate"
    );

    marker(&fixture, "release");
    assert!(helper.wait_success(Duration::from_secs(120)).await);
    assert!(rows_in(&root)
        .iter()
        .find(|row| row.instance_id == second_instance)
        .is_some_and(|row| row.lifecycle == Lifecycle::Closed));
    server.abort();
}

// ---------------------------------------------------------------------------
// Round 06b: one launch root, publication after startup, argv handoff.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Round 06b fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Round 06b fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// An empty temp repository with one commit, for worktree startup.
#[cfg(target_os = "linux")]
fn committed_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    std::fs::write(path.join("README.md"), "round 06b fixture\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "initial"]);
    // A stable branch name keeps the observed byte reference predictable.
    git(path, &["branch", "-M", "main"]);
}

#[cfg(target_os = "linux")]
async fn wait_text(path: &Path, prefix: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if let Some(line) = text.lines().find(|line| line.starts_with(prefix)) {
            return line.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {prefix:?} in {path:?}: {text}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A relative override is anchored at the launch directory even under `-C`, the
/// frontend row is published there and closed normally, and the SessionStart
/// child inherits that same absolute native root.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_relative_override_uses_launch_root_for_frontend_and_child() {
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("launch");
    let project = temp.path().join("project");
    let data = temp.path().join("data");
    let marker = temp.path().join("child-root");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    // A synchronous SessionStart hook records the exact value its child inherits.
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/settings.json"),
        serde_json::json!({
            "localCoordination": {"enabled": true, "registry": true},
            "hooks": {"SessionStart": [{"hooks": [{
                "type": "command",
                "command": "printf '%s' \"$DEEPSEEK_CODE_COORDINATION_DIR\" > \"$DCL_LC_ENV_FILE\""
            }]}]}
        })
        .to_string(),
    )
    .unwrap();
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "-C",
            project.to_str().unwrap(),
            "--coordination-dir",
            "coord-rel",
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--print",
            "--output-format",
            "json",
            "--permission-mode",
            "accept-edits",
            "--",
            "a relative-root turn",
        ])
        .current_dir(&launch)
        .env("DCL_LC_ENV_FILE", &marker)
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("the CLI runs");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1, "one model request");

    let root = CoordinationPaths::resolve(
        Some(Path::new("coord-rel")),
        None,
        None,
        &launch.canonicalize().unwrap(),
    )
    .unwrap()
    .root;
    let rows = rows_in(&root);
    assert_eq!(rows.len(), 1, "one frontend incarnation");
    assert_eq!(rows[0].role, ParticipantRole::Frontend);
    assert_eq!(
        rows[0].lifecycle,
        Lifecycle::Closed,
        "normal exit closes it"
    );
    assert!(
        !project.join("coord-rel").exists(),
        "the relative override is not re-anchored at -C"
    );
    let inherited = std::fs::read(&marker).expect("the SessionStart child ran");
    assert_eq!(
        inherited,
        root.as_os_str().as_bytes(),
        "the child inherited the same absolute native root"
    );
    server.abort();
}

/// The first published row already names the linked worktree, and an idle SDK
/// frontend makes no model request before its first turn.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_worktree_startup_publishes_final_workspace_before_first_turn() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    let root = temp.path().join("coord");
    let stderr_path = temp.path().join("stderr.log");
    committed_repo(&repo);
    write_settings(&data, true);
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            repo.to_str().unwrap(),
            "--coordination-dir",
            root.to_str().unwrap(),
            "--worktree",
            "reviewer",
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--print",
            "--permission-mode",
            "accept-edits",
        ])
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .kill_on_drop(true)
        .spawn()
        .expect("the SDK CLI starts");
    // stdin stays open and idle, so the frontend must publish before a turn.
    let announcement = wait_text(&stderr_path, "Worktree: ", Duration::from_secs(120)).await;
    let announced = Path::new(announcement.trim_start_matches("Worktree: ")).to_path_buf();

    let deadline = Instant::now() + Duration::from_secs(120);
    let row = loop {
        if let Some(row) = poll_rows(&root).into_iter().next() {
            break row;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the CLI exited before publishing: {announcement}"
        );
        assert!(Instant::now() < deadline, "no live row was published");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(row.lifecycle, Lifecycle::Open);
    assert_eq!(
        row.workspace.to_path().unwrap().canonicalize().unwrap(),
        announced.canonicalize().unwrap(),
        "the first row is the linked worktree, not the source checkout"
    );
    assert_eq!(
        row.branch_ref.as_deref(),
        Some(b"refs/heads/worktree/reviewer".as_slice())
    );
    assert!(row.worktree_id.is_some(), "the linked tree has an identity");
    assert_ne!(
        row.workspace.to_path().unwrap().canonicalize().unwrap(),
        repo.canonicalize().unwrap()
    );
    assert_eq!(
        gate.requests.load(Ordering::SeqCst),
        0,
        "no turn, no request"
    );

    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(120), child.wait())
        .await
        .expect("the CLI exits on stdin EOF")
        .expect("the CLI is waitable");
    assert!(status.success(), "the SDK frontend exits normally");
    let closed = poll_rows(&root)
        .into_iter()
        .find(|stored| stored.instance_id == row.instance_id)
        .expect("the row is retained");
    assert_eq!(closed.lifecycle, Lifecycle::Closed, "normal exit closes it");
    server.abort();
}

/// A fault injected after worktree validation fails startup: no model request,
/// no registry root, no row and no owner lease are left behind.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_failed_worktree_startup_publishes_no_row() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    let root = temp.path().join("coord");
    let wrapper = temp.path().join("bin");
    committed_repo(&repo);
    write_settings(&data, true);
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let real_git = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("git is installed")
            .stdout,
    )
    .unwrap();
    std::fs::create_dir_all(&wrapper).unwrap();
    let script = wrapper.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = worktree ] && [ \"$2\" = add ]; then\n  echo 'ROUND06B_WORKTREE_FAILURE' >&2\n  exit 7\nfi\nexec '{}' \"$@\"\n",
            real_git.trim()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        wrapper.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            repo.to_str().unwrap(),
            "--coordination-dir",
            root.to_str().unwrap(),
            "--worktree",
            "reject",
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--print",
            "--output-format",
            "json",
            "--permission-mode",
            "accept-edits",
            "--",
            "a failing worktree turn",
        ])
        .env("PATH", &path)
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("the CLI runs");

    assert!(!output.status.success(), "the injected fault fails startup");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ROUND06B_WORKTREE_FAILURE"),
        "the fixture diagnostic is reported: {stderr}"
    );
    assert_eq!(gate.requests.load(Ordering::SeqCst), 0, "no model request");
    assert!(
        !root.exists(),
        "a failed startup publishes no root, row or owner lease"
    );
    server.abort();
}

/// A host whose listener cannot bind publishes no incarnation, and its endpoint
/// evidence is untouched. The host fixture mirrors the real service layout.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_failed_host_bind_publishes_no_row() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    let endpoint = temp.path().join("occupied.sock");
    let evidence = b"fixture file blocks listener bind";
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(&endpoint, evidence).unwrap();
    write_settings(&data, true);

    let session = uuid::Uuid::new_v4().to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let dir = data.join("state/workers").join(&session).join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("host.lock"), b"").unwrap();
    std::fs::write(
        dir.join("worker.json"),
        serde_json::json!({
            "id": id,
            "session_id": session,
            "root": data,
            "workspace": workspace,
            "executable": env!("CARGO_BIN_EXE_dcl"),
            "endpoint": endpoint,
            "status": "running",
            "supervisor_pid": null,
            "child_pid": null,
            "heartbeat_ms": u64::try_from(wall_clock_ms()).unwrap() + 60_000,
            "active_job": null,
            "error": null,
        })
        .to_string(),
    )
    .unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .current_dir(&workspace)
        .env("DEEPSEEK_CODE_SESSION_HOST", &dir)
        .env("DEEPSEEK_CODE_COORDINATION_DIR", &root)
        .env_remove("DEEPSEEK_CODE_SESSION_SUPERVISOR")
        .env_remove("DEEPSEEK_CODE_SESSION_SUPERVISOR_PID")
        .env_remove("DEEPSEEK_CODE_BACKGROUND_CONTROL")
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("the host runs");

    assert_eq!(
        output.status.code(),
        Some(1),
        "the host reports its failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Session host failed"),
        "a bind failure is reported: {stderr}"
    );
    assert!(
        !root.exists(),
        "binding precedes registration, so nothing was published"
    );
    assert_eq!(
        std::fs::read(&endpoint).unwrap(),
        evidence,
        "the endpoint evidence is unchanged"
    );
}

/// One real host in a shared repository: the parent changes the branch while the
/// host is idle, then releases a gated turn.
#[cfg(target_os = "linux")]
async fn lifecycle_branch_refresh(fixture: std::path::PathBuf, endpoint: String) {
    let data = fixture.join("data");
    let workspace = fixture.join("workspace");
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
    let session = create_session(&data, &workspace);
    std::fs::write(fixture.join("session"), &session).unwrap();
    let host = worker::ensure_worker(&data, &executable, &session, &workspace)
        .await
        .expect("the worker host starts");
    let mut cleanup = ExactCleanup::default();
    cleanup.add(host.supervisor_pid);
    cleanup.add(host.child_pid);
    std::fs::write(
        fixture.join("worker-info.json"),
        serde_json::json!({
            "id": host.id,
            "session": session,
            "supervisor_pid": host.supervisor_pid,
            "child_pid": host.child_pid,
        })
        .to_string(),
    )
    .unwrap();
    marker(&fixture, "worker-started");

    wait_markers(&fixture, &["submit-worker"], Duration::from_secs(120)).await;
    let job_id = uuid::Uuid::new_v4().to_string();
    let prompt = "ROUND06_GATE branch refresh turn".to_owned();
    let arguments = vec![
        "--data-dir".into(),
        data.display().to_string(),
        "--resume".into(),
        session.clone(),
        "--base-url".into(),
        format!("{endpoint}/v1"),
        "--api-format".into(),
        "openai".into(),
        "--api-key-env".into(),
        "DEEPSEEK_CODE_CHILD_API_KEY".into(),
        "--model".into(),
        "fixture-model".into(),
        "--permission-mode".into(),
        "accept-edits".into(),
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--".into(),
        prompt.clone(),
    ];
    worker::call(
        &host,
        &worker::Request::Submit {
            id: job_id.clone(),
            prompt,
            arguments,
            environment: vec![("DEEPSEEK_CODE_CHILD_API_KEY".into(), "fixture-only".into())],
        },
    )
    .await
    .expect("the worker admits the turn");
    marker(&fixture, "worker-submitted");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = deepseek_code_core::services::background_sessions::get(&data, &job_id).unwrap();
        if job.finished_ms.is_some() {
            assert_eq!(job.status, "completed", "{job:?}");
            break;
        }
        assert!(Instant::now() < deadline, "the gated turn never finished");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    worker::shutdown(&data, &session).await.unwrap();
    drop(cleanup);
    marker(&fixture, "done");
}

/// Two real hosts for the same conversation and data directory, in sequence.
#[cfg(target_os = "linux")]
async fn lifecycle_same_session_restart(fixture: std::path::PathBuf) {
    let data = fixture.join("data");
    let workspace = fixture.join("workspace");
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
    let session = create_session(&data, &workspace);
    std::fs::write(fixture.join("session"), &session).unwrap();
    let mut cleanup = ExactCleanup::default();

    let first = worker::ensure_worker(&data, &executable, &session, &workspace)
        .await
        .expect("the first host starts");
    cleanup.add(first.supervisor_pid);
    cleanup.add(first.child_pid);
    std::fs::write(
        fixture.join("worker-1.json"),
        serde_json::json!({"id": first.id, "session": session}).to_string(),
    )
    .unwrap();
    marker(&fixture, "first-started");
    wait_markers(&fixture, &["stop-first"], Duration::from_secs(120)).await;
    worker::shutdown(&data, &session).await.unwrap();
    marker(&fixture, "first-stopped");

    wait_markers(&fixture, &["start-second"], Duration::from_secs(120)).await;
    let second = worker::ensure_worker(&data, &executable, &session, &workspace)
        .await
        .expect("the second host starts");
    cleanup.add(second.supervisor_pid);
    cleanup.add(second.child_pid);
    std::fs::write(
        fixture.join("worker-2.json"),
        serde_json::json!({"id": second.id, "session": session}).to_string(),
    )
    .unwrap();
    marker(&fixture, "second-started");
    wait_markers(&fixture, &["release"], Duration::from_secs(120)).await;
    worker::shutdown(&data, &session).await.unwrap();
    drop(cleanup);
    marker(&fixture, "done");
}

/// Polls for the open host row that belongs to one validated worker instance.
#[cfg(target_os = "linux")]
async fn wait_worker_row(root: &Path, worker_id: &str, timeout: Duration) -> Registration {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(row) = poll_rows(root).into_iter().find(|row| {
            row.worker_id.as_deref() == Some(worker_id) && row.lifecycle == Lifecycle::Open
        }) {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for worker {worker_id}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Admitting a turn makes the same host incarnation observe the new branch of
/// the unchanged workspace path, creating no second row.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_host_owned_turn_refreshes_branch_without_duplicate_row() {
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&fixture).unwrap();
    write_settings(&fixture.join("data"), true);
    let workspace = fixture.join("workspace");
    committed_repo(&workspace);
    let (endpoint, gate, server) = start_gate().await;

    let mut helper = LifecycleHelper::spawn("branch_refresh", &fixture, &endpoint, &root);
    wait_markers(&fixture, &["worker-started"], Duration::from_secs(120)).await;
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("worker-info.json")).unwrap()).unwrap();
    let worker_id = info["id"].as_str().unwrap().to_owned();
    let session = info["session"].as_str().unwrap().to_owned();
    let executor = wait_worker_row(&root, &worker_id, Duration::from_secs(120)).await;
    let executor_id = executor.instance_id.clone();
    let observed = executor.workspace.to_path().unwrap();
    assert_eq!(
        observed.canonicalize().unwrap(),
        workspace.canonicalize().unwrap()
    );
    assert_eq!(
        executor.branch_ref.as_deref(),
        Some(b"refs/heads/main".as_slice()),
        "the initial branch is published"
    );

    // Same directory, new branch; the turn is admitted afterwards.
    git(&workspace, &["checkout", "-q", "-b", "after"]);
    marker(&fixture, "submit-worker");
    wait_markers(&fixture, &["worker-submitted"], Duration::from_secs(120)).await;
    let _permit = tokio::time::timeout(Duration::from_secs(120), gate.gated.acquire())
        .await
        .expect("the worker reaches the model")
        .expect("the gate is open");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let rows = poll_rows(&root);
        let current = rows
            .iter()
            .find(|row| row.instance_id == executor_id)
            .expect("the same row exists");
        if current.branch_ref.as_deref() == Some(b"refs/heads/after".as_slice()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the host never refreshed its branch: {:?}",
            current.branch_ref
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let rows = poll_rows(&root);
    let executors: Vec<&Registration> = rows
        .iter()
        .filter(|row| {
            row.session_id.as_deref() == Some(&session) && row.role == ParticipantRole::Executor
        })
        .collect();
    assert_eq!(executors.len(), 1, "no per-turn incarnation");
    assert_eq!(executors[0].instance_id, executor_id);
    assert_eq!(
        executors[0]
            .workspace
            .to_path()
            .unwrap()
            .as_os_str()
            .as_bytes(),
        observed.as_os_str().as_bytes(),
        "the workspace bytes are unchanged"
    );
    assert!(
        deepseek_code_core::db::session_store::Stores::open(&fixture.join("data")).unwrap().open_session(&session).unwrap().acquire_execution()
        .is_err(),
        "the running turn still holds its execution lease"
    );

    gate.release.send(true).unwrap();
    marker(&fixture, "release");
    assert!(helper.wait_success(Duration::from_secs(120)).await);
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1, "one model request");
    assert!(poll_rows(&root)
        .iter()
        .find(|row| row.instance_id == executor_id)
        .is_some_and(|row| row.lifecycle == Lifecycle::Closed));
    server.abort();
}

/// A host whose registry identity is not a canonical UUID keeps serving its real
/// IPC while publishing nothing and warning exactly once.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_rejects_noncanonical_worker_id_without_stopping_host() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&workspace).unwrap();
    write_settings(&data, true);

    let sentinel = "sentinel-free-form-worker-id";
    let session = uuid::Uuid::new_v4().to_string();
    let dir = data.join("state/workers").join(&session).join(sentinel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("host.lock"), b"").unwrap();
    // A short path keeps this Unix socket address under the platform limit.
    let endpoint = std::path::PathBuf::from("/tmp")
        .join(format!("lc06c-{}.sock", uuid::Uuid::new_v4().simple()));
    assert!(
        !endpoint.exists(),
        "the endpoint is free before the host starts"
    );
    std::fs::write(
        dir.join("worker.json"),
        serde_json::json!({
            "id": sentinel,
            "session_id": session,
            "root": data,
            "workspace": workspace,
            "executable": env!("CARGO_BIN_EXE_dcl"),
            "endpoint": endpoint,
            "status": "running",
            "supervisor_pid": null,
            "child_pid": null,
            "heartbeat_ms": u64::try_from(wall_clock_ms()).unwrap() + 60_000,
            "active_job": null,
            "error": null,
        })
        .to_string(),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .current_dir(&workspace)
        .env("DEEPSEEK_CODE_SESSION_HOST", &dir)
        .env("DEEPSEEK_CODE_COORDINATION_DIR", &root)
        .env_remove("DEEPSEEK_CODE_SESSION_SUPERVISOR")
        .env_remove("DEEPSEEK_CODE_SESSION_SUPERVISOR_PID")
        .env_remove("DEEPSEEK_CODE_BACKGROUND_CONTROL")
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the host starts");

    let worker = worker::read_worker(&dir).expect("the fixture metadata parses");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match worker::call(&worker, &worker::Request::Ping).await {
            Ok(_) => break,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the host never served: {error:#}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    worker::call(&worker, &worker::Request::Shutdown)
        .await
        .expect("the rejected host still serves Shutdown");

    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("the host exits")
        .expect("the host is waitable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the worker itself is not rejected: {stderr}"
    );
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("Local coordination registry unavailable"))
        .collect();
    assert_eq!(
        warnings,
        vec![
            "Local coordination registry unavailable: unavailable. Session continues without peer visibility."
        ],
        "one exact bounded warning"
    );
    assert!(!root.exists(), "no registry root, row or owner lease");
    assert!(
        !poll_rows(&root)
            .iter()
            .any(|row| row.worker_id.as_deref() == Some(sentinel)),
        "the sentinel never reached the registry"
    );
    let _ = std::fs::remove_file(&endpoint);
}

/// Restarting a host for the same conversation and data directory replaces the
/// owner: a new worker UUID, incarnation and generation, one new open row, and
/// the old row retained as Closed.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_same_session_worker_restart_replaces_owner() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&fixture).unwrap();
    write_settings(&fixture.join("data"), true);
    let (endpoint, _gate, server) = start_gate().await;

    let mut helper = LifecycleHelper::spawn("same_session_restart", &fixture, &endpoint, &root);
    wait_markers(&fixture, &["first-started"], Duration::from_secs(120)).await;
    let first: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("worker-1.json")).unwrap()).unwrap();
    let first_id = first["id"].as_str().unwrap().to_owned();
    let session = first["session"].as_str().unwrap().to_owned();
    let first_row = wait_worker_row(&root, &first_id, Duration::from_secs(120)).await;
    let first_instance = first_row.instance_id.clone();

    marker(&fixture, "stop-first");
    wait_markers(&fixture, &["first-stopped"], Duration::from_secs(120)).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if poll_rows(&root)
            .iter()
            .find(|row| row.instance_id == first_instance)
            .is_some_and(|row| row.lifecycle == Lifecycle::Closed)
        {
            break;
        }
        assert!(Instant::now() < deadline, "the old host row never closed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    marker(&fixture, "start-second");
    wait_markers(&fixture, &["second-started"], Duration::from_secs(120)).await;
    let second: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("worker-2.json")).unwrap()).unwrap();
    let second_id = second["id"].as_str().unwrap().to_owned();
    assert_eq!(
        second["session"].as_str().unwrap(),
        session,
        "the same conversation"
    );
    let second_row = wait_worker_row(&root, &second_id, Duration::from_secs(120)).await;

    assert_ne!(first_id, second_id, "a restart is a fresh worker instance");
    assert_ne!(
        first_instance, second_row.instance_id,
        "a restart is a fresh incarnation"
    );
    assert_ne!(
        first_row.owner.generation, second_row.owner.generation,
        "a restart takes a fresh owner generation"
    );
    assert_eq!(
        first_row.data_namespace_id, second_row.data_namespace_id,
        "the same data directory keeps one namespace"
    );
    assert_eq!(first_row.session_id, second_row.session_id);
    let rows = poll_rows(&root);
    let in_session: Vec<&Registration> = rows
        .iter()
        .filter(|row| row.session_id.as_deref() == Some(&session))
        .collect();
    assert_eq!(
        in_session
            .iter()
            .filter(|row| row.lifecycle == Lifecycle::Open)
            .count(),
        1,
        "exactly one new open row"
    );
    assert_eq!(
        in_session
            .iter()
            .filter(|row| row.instance_id == first_instance)
            .count(),
        1,
        "the old row is retained once"
    );
    assert!(in_session
        .iter()
        .any(|row| row.instance_id == first_instance && row.lifecycle == Lifecycle::Closed));

    marker(&fixture, "release");
    assert!(helper.wait_success(Duration::from_secs(120)).await);
    assert!(poll_rows(&root)
        .iter()
        .find(|row| row.instance_id == second_row.instance_id)
        .is_some_and(|row| row.lifecycle == Lifecycle::Closed));
    server.abort();
}

// ---------------------------------------------------------------------------
// Round 06d: print startup validation and post-commit hook diagnostics.
// ---------------------------------------------------------------------------

/// A one-shot run with no prompt and no hook message fails before anything is
/// published: no root, no row (open or closed) and no owner lease.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_print_without_prompt_publishes_no_incarnation() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&workspace).unwrap();
    write_settings(&data, true);
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            workspace.to_str().unwrap(),
            "--coordination-dir",
            root.to_str().unwrap(),
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--print",
            "--permission-mode",
            "accept-edits",
        ])
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("the CLI runs");

    assert_eq!(output.status.code(), Some(1), "the run is rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line == "Error: A prompt or piped stdin is required"),
        "the exact bounded diagnostic is reported: {stderr}"
    );
    assert_eq!(gate.requests.load(Ordering::SeqCst), 0, "no model request");
    // Observed before any helper could open the store and create the root.
    assert!(
        !root.exists(),
        "a rejected startup creates no coordination root"
    );
    assert!(rows_in(&root).is_empty(), "and no row, open or closed");
    assert_eq!(
        std::fs::read_dir(root.join("owners"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "and no owner lease"
    );
    server.abort();
}

/// A hook-supplied initial message counts as one-shot input: the run publishes
/// before its single model request, which sees the hook text.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_print_with_hook_prompt_publishes_incarnation() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    assert!(!root.exists(), "the fixture root starts absent");
    let hook = temp.path().join("hook.sh");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"initialUserMessage\":\"ROUND06_GATE\"}}'\n",
    )
    .unwrap();
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/settings.json"),
        serde_json::json!({
            "localCoordination": {"enabled": true, "registry": true},
            "hooks": {"SessionStart": [{"matcher": "startup", "hooks": [{
                "type": "command",
                "command": format!("sh {}", hook.display()),
                "timeout": 20
            }]}]}
        })
        .to_string(),
    )
    .unwrap();
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            workspace.to_str().unwrap(),
            "--coordination-dir",
            root.to_str().unwrap(),
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--print",
            "--output-format",
            "json",
            "--permission-mode",
            "accept-edits",
        ])
        .env_remove("DEEPSEEK_CODE_COORDINATION_DIR")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the CLI starts");

    let _permit = tokio::time::timeout(Duration::from_secs(120), gate.gated.acquire())
        .await
        .expect("the hook message reaches the model")
        .expect("the gate is open");
    assert_eq!(gate.gated_requests.load(Ordering::SeqCst), 1);
    let open: Vec<Registration> = poll_rows(&root)
        .into_iter()
        .filter(|row| row.lifecycle == Lifecycle::Open)
        .collect();
    assert_eq!(open.len(), 1, "one published frontend row");
    assert_eq!(open[0].role, ParticipantRole::Frontend);
    let instance = open[0].instance_id.clone();
    let session = open[0].session_id.clone().expect("a session id");
    gate.release.send(true).unwrap();

    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("the CLI exits")
        .expect("the CLI is waitable");
    assert_eq!(output.status.code(), Some(0));
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["session_id"].as_str(), Some(session.as_str()));
    assert_eq!(gate.requests.load(Ordering::SeqCst), 1, "one model request");
    let stored = poll_rows(&root);
    assert!(stored
        .iter()
        .find(|row| row.instance_id == instance)
        .is_some_and(|row| row.lifecycle == Lifecycle::Closed));
    server.abort();
}

/// `/clear` commits the switch before its SessionStart hook runs, so a Ctrl-C
/// during that hook must report one bounded diagnostic and still return the
/// successful switch: the new incarnation stays Open until ordinary exit.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn registry_clear_sigint_after_commit_returns_success() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{Read, Write};

    /// Releases the hook and cleans up the child if it is still running.
    struct ClearGuard<'a> {
        release: &'a Path,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    }
    impl Drop for ClearGuard<'_> {
        fn drop(&mut self) {
            let _ = std::fs::write(self.release, b"");
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
                while let Err(error) = self.child.wait() {
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let root = temp.path().join("coord");
    let entered = temp.path().join("hook-entered");
    let release = temp.path().join("hook-release");
    let log_path = temp.path().join("transcript.log");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    // The clear-only hook blocks on a marker the test owns; it never runs at
    // startup, so ordinary startup stays prompt-free.
    let script = temp.path().join("clear-hook.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n",
            entered.display(),
            release.display()
        ),
    )
    .unwrap();
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/settings.json"),
        serde_json::json!({
            "localCoordination": {"enabled": true, "registry": true},
            "hooks": {"SessionStart": [{"matcher": "clear", "hooks": [{
                "type": "command",
                "command": format!("sh {}", script.display()),
                "timeout": 20
            }]}]}
        })
        .to_string(),
    )
    .unwrap();
    let (endpoint, gate, server) = start_gate().await;
    write_provider(&data, &endpoint);

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("a pseudo-terminal opens");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_dcl"));
    command.args([
        "--data-dir",
        data.to_str().unwrap(),
        "--path",
        workspace.to_str().unwrap(),
        "--coordination-dir",
        root.to_str().unwrap(),
        "--provider",
        "fixture",
        "--model",
        "fixture-model",
        "--permission-mode",
        "accept-edits",
        "--ax-screen-reader",
    ]);
    let mut guard = ClearGuard {
        release: &release,
        child: pty
            .slave
            .spawn_command(command)
            .expect("the interactive CLI starts"),
    };
    drop(pty.slave);
    let pid = guard.child.process_id().expect("the owned child has a pid");
    let mut writer = pty.master.take_writer().expect("pty writer");
    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    let log = std::fs::File::create(&log_path).unwrap();
    let reader_thread = std::thread::spawn(move || {
        let mut log = log;
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if log.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                    let _ = log.flush();
                }
            }
        }
    });

    // The startup frontend incarnation is the row the clear must close.
    let deadline = Instant::now() + Duration::from_secs(120);
    let old = loop {
        let open: Vec<Registration> = poll_rows(&root)
            .into_iter()
            .filter(|row| row.lifecycle == Lifecycle::Open)
            .collect();
        if let Some(row) = open.first() {
            assert_eq!(open.len(), 1, "one startup incarnation");
            break row.clone();
        }
        assert!(Instant::now() < deadline, "the frontend never published");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    writer.write_all(b"/clear\r").unwrap();
    writer.flush().unwrap();
    let hook_deadline = Instant::now() + Duration::from_secs(120);
    while !entered.exists() {
        assert!(Instant::now() < hook_deadline, "the clear hook never ran: {}",std::fs::read_to_string(&log_path).unwrap_or_default());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The switch is already committed while its hook is still held.
    let rows = poll_rows(&root);
    assert!(rows
        .iter()
        .find(|row| row.instance_id == old.instance_id)
        .is_some_and(|row| row.lifecycle == Lifecycle::Closed));
    let open: Vec<&Registration> = rows
        .iter()
        .filter(|row| row.lifecycle == Lifecycle::Open)
        .collect();
    assert_eq!(open.len(), 1, "one new incarnation");
    let new = open[0].clone();
    assert_ne!(new.session_id, old.session_id, "a distinct new session");
    assert_ne!(new.instance_id, old.instance_id, "and a new instance");

    assert!(
        guard.child.try_wait().unwrap().is_none(),
        "the CLI is still running before the signal"
    );
    let status = std::process::Command::new("kill")
        .args(["-s", "INT", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success(), "the owned child received SIGINT");

    let transcripts = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = transcripts(&log_path);
        if text.contains("New session:") && text.contains("Previous history retained") {
            break;
        }
        assert!(Instant::now() < deadline, "no clear response: {text}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = transcripts(&log_path);
    assert!(
        text.contains("SessionStart hook: SessionStart hooks interrupted"),
        "one bounded interruption diagnostic: {text}"
    );
    assert!(
        text.contains(&format!(
            "New session: {}. Previous history retained: {}",
            new.session_id.as_deref().unwrap_or_default(),
            old.session_id.as_deref().unwrap_or_default()
        )),
        "the committed switch is reported: {text}"
    );
    assert!(
        !text.contains("Error: SessionStart hooks interrupted"),
        "the switch is not turned into an error: {text}"
    );
    assert_eq!(gate.requests.load(Ordering::SeqCst), 0, "no model request");
    assert!(
        guard.child.try_wait().unwrap().is_none(),
        "the CLI keeps running after the interruption"
    );
    assert!(poll_rows(&root)
        .iter()
        .find(|row| row.instance_id == new.instance_id)
        .is_some_and(|row| row.lifecycle == Lifecycle::Open));

    std::fs::write(&release, b"").unwrap();
    writer.write_all(b"/exit\r").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let exit = loop {
        if let Some(status) = guard.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the CLI never exited");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(exit.success(), "ordinary exit succeeds");
    assert!(poll_rows(&root)
        .iter()
        .find(|row| row.instance_id == new.instance_id)
        .is_some_and(|row| row.lifecycle == Lifecycle::Closed));
    drop(guard);
    drop(writer);
    drop(pty.master);
    let _ = reader_thread.join();
    server.abort();
}

#[cfg(target_os = "linux")]
#[path = "support/terminal_hangup.rs"]
mod terminal_hangup;
