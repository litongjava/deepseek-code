use anyhow::{bail, Result};
use deepseek_code_core::db::session_store::{
    CreateOptions, FirstMessage, Phase, ProjectSelection, Stores,
};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[path = "session_db_lifecycle/child_upgrade.rs"]
mod child_upgrade;

const ID: &str = "12345678-abcd-4abc-89ef-123456789abc";
const CHILD_ENV: &str = "I46_LIFECYCLE_CHILD_DIR";
const WAIT: Duration = Duration::from_secs(20);

fn global(root: &Path) -> Connection {
    Connection::open_with_flags(root.join("global.db"), OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
}
fn shard(root: &Path) -> PathBuf {
    root.join("data/sessions").join(ID).join("session.db")
}
fn query<T: rusqlite::types::FromSql>(db: &Connection, sql: &str) -> T {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn first_message() -> FirstMessage {
    FirstMessage {
        origin: "typed".into(),
        text: "first line\n第二行".into(),
        typed: Some(serde_json::json!([{"type":"text","text":"第一"}])),
    }
}
fn first() -> CreateOptions {
    CreateOptions {
        first_message: Some(first_message()),
        ..Default::default()
    }
}
fn stop_at(stores: &Stores, expected: Phase) -> Stores {
    stores.with_fault_hook(move |phase| {
        if phase == expected {
            bail!("interrupt at {phase:?}")
        }
        Ok(())
    })
}
fn pending(root: &Path) -> i64 {
    query(
        &global(root),
        "SELECT count(*) FROM lifecycle WHERE quarantine_reason IS NULL",
    )
}
fn count(root: &Path, path: &str) -> usize {
    fs::read_dir(root.join(path)).unwrap().count()
}

#[test]
fn concurrent_first_open_shares_one_directory_and_pool() {
    let tmp = tempdir().unwrap();
    let gate = Arc::new(Barrier::new(8));
    let stores: Vec<_> = thread::scope(|scope| {
        let mut jobs = vec![];
        for _ in 0..8 {
            let gate = gate.clone();
            let root = tmp.path().join(".");
            jobs.push(scope.spawn(move || {
                gate.wait();
                Stores::open(&root).unwrap()
            }));
        }
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    for s in &stores {
        assert!(std::ptr::eq(s.global(), stores[0].global()));
    }
    assert_eq!(
        query::<i64>(&global(tmp.path()), "SELECT count(*) FROM store_identity"),
        1
    );
}

#[test]
fn two_creates_racing_for_one_id_publish_exactly_once() {
    let tmp = tempdir().unwrap();
    let stores = Stores::open(tmp.path()).unwrap();
    let gate = Barrier::new(2);
    let outcomes = thread::scope(|scope| {
        let jobs: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    gate.wait();
                    stores.create(ID, first())
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(outcomes.iter().filter(|s| s.is_ok()).count(), 1);
    assert_eq!(pending(tmp.path()), 0);
    assert_eq!(
        stores
            .open_session(ID)
            .unwrap()
            .read(|r| r.initial_message())
            .unwrap(),
        Some(first_message())
    );
}

#[test]
fn legacy_database_wal_and_shm_remain_untouched() {
    let tmp = tempdir().unwrap();
    let names = [
        "deepseek_code.db",
        "deepseek_code.db-wal",
        "deepseek_code.db-shm",
    ];
    for name in names {
        fs::write(tmp.path().join(name), name).unwrap();
    }
    #[cfg(target_os = "linux")]
    let watch = {
        use rustix::fs::inotify::*;
        let w = init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).unwrap();
        for name in names {
            add_watch(&w, tmp.path().join(name), WatchFlags::ALL_EVENTS).unwrap();
        }
        w
    };
    let run = || {
        let s = Stores::open(tmp.path()).unwrap();
        let session = s.create(ID, first()).unwrap();
        session.set_archived(true).unwrap();
        drop(s.open_session(ID).unwrap());
        session.delete().unwrap();
    };
    run();
    #[cfg(target_os = "linux")]
    {
        let mut buffer = [std::mem::MaybeUninit::uninit(); 512];
        let mut events = rustix::fs::inotify::Reader::new(watch, &mut buffer);
        assert!(
            matches!(events.next(), Err(rustix::io::Errno::AGAIN)),
            "legacy file was accessed"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in names {
            fs::set_permissions(tmp.path().join(name), fs::Permissions::from_mode(0)).unwrap();
        }
        run();
        for name in names {
            fs::set_permissions(tmp.path().join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    for name in names {
        assert_eq!(fs::read(tmp.path().join(name)).unwrap(), name.as_bytes());
    }
}

#[test]
fn exact_logical_ids_have_distinct_safe_storage_keys() {
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    for id in [
        ID,
        &ID.to_uppercase(),
        "../outside",
        "/absolute/path",
        "a\\..\\b",
        ".staging",
        "CON",
        "a\0b",
        "e\u{301}",
        "é",
        &"é".repeat(128),
    ] {
        let session = s.create(id, Default::default()).unwrap();
        let key = if id == ID {
            id.into()
        } else {
            format!("sha256-{:x}", Sha256::digest(id.as_bytes()))
        };
        let file = tmp
            .path()
            .join("data/sessions")
            .join(&key)
            .join("session.db");
        assert!(tmp.path().join("state/locks/session").join(key).is_file());
        let c = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(query::<String>(&c, "SELECT id FROM manifest"), id);
        assert_eq!(
            s.open_session(id).unwrap().incarnation(),
            session.incarnation()
        );
    }
    for id in ["", &"é".repeat(129)] {
        assert!(s.create(id, Default::default()).is_err());
    }
    assert!(!tmp.path().join("outside").exists());
}

#[test]
fn archive_and_stale_handles_are_fenced_by_incarnation() {
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    let old = s.create(ID, first()).unwrap();
    let previous = old.incarnation().to_owned();
    old.set_archived(true).unwrap();
    old.set_archived(true).unwrap();
    assert!(s.global().conversation(ID).unwrap().unwrap().archived);
    let g = global(tmp.path());
    assert_eq!(query::<i64>(&g, "SELECT archived FROM conversations"), 1);
    assert_eq!(
        query::<i64>(&g, "SELECT metadata_revision FROM conversations"),
        1
    );
    assert_eq!(
        s.open_session(ID)
            .unwrap()
            .read(|r| r.initial_message())
            .unwrap(),
        Some(first_message())
    );
    old.set_archived(false).unwrap();
    assert_eq!(query::<i64>(&g, "SELECT archived FROM conversations"), 0);
    assert_eq!(old.write(|tx| tx.synchronous_for_test()).unwrap(), 1);
    old.delete().unwrap();
    assert!(s.open_session(ID).is_err());
    let new = s.create(ID, Default::default()).unwrap();
    assert_ne!(previous, new.incarnation());
    assert!(old.read(|r| r.initial_message()).is_err());
    assert!(old.write(|_| Ok(())).is_err());
    assert!(old.set_archived(true).is_err());
    assert!(old.delete().is_err());
    assert_eq!(new.read(|r| r.initial_message()).unwrap(), None);
    assert_eq!(
        query::<i64>(&g, "SELECT message_count FROM conversations"),
        0
    );
}

#[test]
fn invalid_session_files_fail_closed_without_recreation() {
    for damage in [
        "missing",
        "corrupt",
        "kind",
        "newer",
        "wal",
        "store",
        "id",
        "incarnation",
    ] {
        let tmp = tempdir().unwrap();
        let s = Stores::open(tmp.path()).unwrap();
        let h = s.create(ID, first()).unwrap();
        let path = shard(tmp.path());
        match damage {
            "missing" => fs::remove_file(&path).unwrap(),
            "corrupt" => fs::write(&path, "invalid SQLite").unwrap(),
            other => {
                let c = Connection::open(&path).unwrap();
                c.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
                c.execute_batch(match other {
                    "kind" => "PRAGMA application_id=0x44434c47",
                    "newer" => "PRAGMA user_version=99",
                    "wal" => "PRAGMA journal_mode=delete",
                    "store" => "UPDATE manifest SET store_uuid='other'",
                    "id" => "UPDATE manifest SET id='other'",
                    _ => "UPDATE manifest SET incarnation='other'",
                })
                .unwrap();
            }
        }
        assert!(s.open_session(ID).is_err(), "{damage}");
        assert!(h.read(|_| Ok(())).is_err(), "{damage}");
        assert!(h.write(|_| Ok(())).is_err(), "{damage}");
        if damage == "missing" {
            assert!(!path.exists());
        }
    }
}

#[test]
fn invalid_global_files_fail_closed_without_reinitialization() {
    for damage in ["missing", "corrupt", "kind", "newer", "empty", "wal"] {
        let tmp = tempdir().unwrap();
        drop(Stores::open(tmp.path()).unwrap());
        let path = tmp.path().join("global.db");
        match damage {
            "missing" => fs::remove_file(&path).unwrap(),
            "corrupt" => fs::write(&path, "invalid global").unwrap(),
            "empty" => fs::write(&path, []).unwrap(),
            d => global(tmp.path())
                .execute_batch(match d {
                    "kind" => "PRAGMA application_id=0x44434c53",
                    "newer" => "PRAGMA user_version=99",
                    _ => "PRAGMA journal_mode=delete",
                })
                .unwrap(),
        }
        assert!(Stores::open(tmp.path()).is_err(), "{damage}");
        if damage == "missing" {
            assert!(!path.exists());
        }
    }
}

#[test]
fn recover_every_create_boundary() {
    use Phase::*;
    for phase in [
        BeforeCreateIntentCommit,
        CreateIntent,
        StageDirectory,
        BeforeSeal,
        Sealed,
        Checkpointed,
        Renamed,
        Installed,
        BeforePublishCommit,
        Published,
    ] {
        let tmp = tempdir().unwrap();
        let s = Stores::open(tmp.path()).unwrap();
        let result = stop_at(&s, phase).create(ID, first());
        assert!(
            format!("{:#}", result.err().unwrap()).contains("interrupt at"),
            "{phase:?}"
        );
        assert_eq!(
            query::<i64>(&global(tmp.path()), "SELECT count(*) FROM conversations"),
            i64::from(phase == Published)
        );
        drop(s);
        let reopened = Stores::open(tmp.path()).unwrap();
        assert_eq!(pending(tmp.path()), 0);
        assert_eq!(count(tmp.path(), "data/sessions/.staging"), 0);
        if matches!(
            phase,
            BeforeCreateIntentCommit | CreateIntent | StageDirectory | BeforeSeal
        ) {
            assert!(reopened.open_session(ID).is_err());
            assert!(!shard(tmp.path()).exists());
        } else {
            let h = reopened.open_session(ID).unwrap();
            assert_eq!(
                h.read(|r| r.initial_message()).unwrap(),
                Some(first_message())
            );
            assert_eq!(
                query::<String>(&global(tmp.path()), "SELECT incarnation FROM conversations"),
                h.incarnation()
            );
            assert_eq!(
                query::<i64>(
                    &global(tmp.path()),
                    "SELECT message_count FROM conversations"
                ),
                1
            );
            assert!(reopened.create(ID, Default::default()).is_err());
        }
    }
}

#[test]
fn recover_every_delete_boundary_without_resurrection() {
    use Phase::*;
    for phase in [
        BeforeDeleteIntentCommit,
        DeleteIntent,
        TrashRenamed,
        Trashed,
        Removed,
        Retired,
    ] {
        let tmp = tempdir().unwrap();
        let s = Stores::open(tmp.path()).unwrap();
        let crash = stop_at(&s, phase);
        let h = crash.create(ID, first()).unwrap();
        let old = h.incarnation().to_owned();
        assert!(format!("{:#}", h.delete().unwrap_err()).contains("interrupt at"));
        if phase == BeforeDeleteIntentCommit {
            drop((h, s, crash));
            let reopened = Stores::open(tmp.path()).unwrap();
            assert_eq!(reopened.open_session(ID).unwrap().incarnation(), old);
            assert_eq!(pending(tmp.path()), 0);
            continue;
        }
        assert!(s.open_session(ID).is_err());
        drop((h, s, crash));
        let reopened = Stores::open(tmp.path()).unwrap();
        assert!(reopened.open_session(ID).is_err());
        assert_eq!(pending(tmp.path()), 0);
        assert_eq!(count(tmp.path(), "data/sessions/.trash"), 0);
        assert!(!shard(tmp.path()).exists());
        assert_ne!(
            reopened
                .create(ID, Default::default())
                .unwrap()
                .incarnation(),
            old
        );
    }
}

#[test]
fn publication_rechecks_project_trust_and_quarantines_conflict() {
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    let workspace = tmp.path().join("project");
    fs::create_dir(&workspace).unwrap();
    s.create(
        "a",
        CreateOptions {
            project: Some(ProjectSelection::Workspace(workspace)),
            ..Default::default()
        },
    )
    .unwrap();
    let g = global(tmp.path());
    let project: String = query(&g, "SELECT project_id FROM conversations WHERE id='a'");
    assert!(stop_at(&s, Phase::Installed)
        .create(
            ID,
            CreateOptions {
                project: Some(ProjectSelection::Existing(project)),
                ..first()
            }
        )
        .is_err());
    g.execute("UPDATE project_folders SET trusted=0", [])
        .unwrap();
    drop(s);
    let reopened = Stores::open(tmp.path()).unwrap();
    assert!(reopened.open_session(ID).is_err());
    assert!(reopened.open_session("a").is_ok());
    assert!(!shard(tmp.path()).exists());
    assert_eq!(pending(tmp.path()), 0);
    assert_eq!(
        query::<i64>(
            &g,
            "SELECT count(*) FROM lifecycle WHERE quarantine_reason IS NOT NULL"
        ),
        1
    );
    assert_eq!(count(tmp.path(), "data/sessions/.quarantine"), 1);
}

#[test]
fn orphaned_session_never_becomes_visible() {
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    s.create(ID, first()).unwrap();
    global(tmp.path())
        .execute("DELETE FROM conversations", [])
        .unwrap();
    drop(s);
    let reopened = Stores::open(tmp.path()).unwrap();
    assert!(reopened.open_session(ID).is_err());
    assert!(!shard(tmp.path()).exists());
    assert_eq!(count(tmp.path(), "data/sessions/.quarantine"), 1);
}

#[test]
fn damaged_or_foreign_seals_are_quarantined_while_other_sessions_open() {
    for damage in ["bytes", "operation", "incarnation", "newer"] {
        let tmp = tempdir().unwrap();
        let s = Stores::open(tmp.path()).unwrap();
        s.create("other", first()).unwrap();
        assert!(stop_at(&s, Phase::Sealed).create(ID, first()).is_err());
        let stage = fs::read_dir(tmp.path().join("data/sessions/.staging"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("session.db");
        if damage == "bytes" {
            fs::write(&stage, "invalid sqlite").unwrap();
        } else {
            Connection::open(&stage)
                .unwrap()
                .execute_batch(match damage {
                    "operation" => "UPDATE manifest SET sealed_operation='foreign'",
                    "newer" => "PRAGMA user_version=99",
                    _ => "UPDATE manifest SET incarnation='foreign'",
                })
                .unwrap();
        }
        drop(s);
        let s = Stores::open(tmp.path()).unwrap();
        assert!(s.open_session(ID).is_err());
        assert!(s.open_session("other").is_ok());
        assert_eq!(pending(tmp.path()), 0);
        assert_eq!(count(tmp.path(), "data/sessions/.quarantine"), 1);
        assert_eq!(
            query::<i64>(
                &global(tmp.path()),
                "SELECT count(*) FROM lifecycle WHERE quarantine_reason IS NOT NULL"
            ),
            1
        );
    }
}

#[test]
fn journal_paths_and_cross_root_identity_are_checked_before_use() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let sa = Stores::open(a.path()).unwrap();
    let sb = Stores::open(b.path()).unwrap();
    let a_handle = sa.create(ID, first()).unwrap();
    let b_handle = sb.create(ID, first()).unwrap();
    // Match logical ID and incarnation so that only the store UUID distinguishes the files.
    let c = Connection::open(shard(a.path())).unwrap();
    c.execute(
        "UPDATE manifest SET incarnation=?1",
        [b_handle.incarnation()],
    )
    .unwrap();
    drop(c);
    fs::copy(shard(a.path()), shard(b.path())).unwrap();
    assert!(sb.open_session(ID).is_err());
    assert!(b_handle.write(|_| Ok(())).is_err());
    assert!(a_handle.read(|_| Ok(())).is_err());

    assert!(stop_at(&sa, Phase::CreateIntent)
        .create("pending", first())
        .is_err());
    global(a.path())
        .execute("UPDATE lifecycle SET operation='../outside'", [])
        .unwrap();
    fs::write(a.path().join("outside"), "sentinel").unwrap();
    drop((a_handle, sa));
    assert!(Stores::open(a.path()).is_err());
    assert_eq!(fs::read(a.path().join("outside")).unwrap(), b"sentinel");
}

#[test]
fn session_capacity_is_process_wide_and_releases_after_snapshots() {
    let dirs = [tempdir().unwrap(), tempdir().unwrap()];
    let stores: Vec<_> = dirs
        .iter()
        .map(|p| Stores::open(p.path()).unwrap())
        .collect();
    let handles: Vec<_> = stores
        .iter()
        .map(|s| s.create(ID, first()).unwrap())
        .collect();
    let (entered, observed) = mpsc::channel();
    let mut releases = vec![];
    let mut jobs = vec![];
    for i in 0..17 {
        let (release, wait) = mpsc::channel();
        releases.push(release);
        let entered = entered.clone();
        let h = handles[i % 2].clone();
        jobs.push(thread::spawn(move || {
            h.read(|r| {
                assert_eq!(r.initial_message()?, Some(first_message()));
                entered.send(i)?;
                wait.recv_timeout(WAIT)?;
                Ok(())
            })
        }));
        if i < 16 {
            assert_eq!(observed.recv_timeout(WAIT).unwrap(), i);
        }
    }
    assert!(observed.recv_timeout(Duration::from_millis(200)).is_err());
    // G remains usable while the seventeenth S checkout is waiting.
    stores[0].global().with_read_for_test(|| ()).unwrap();
    releases[0].send(()).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), 16);
    for release in &releases[1..] {
        release.send(()).unwrap();
    }
    for job in jobs {
        job.join().unwrap().unwrap();
    }
}

#[test]
fn global_pool_allows_four_concurrent_checkouts() {
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    let (entered, seen) = mpsc::channel();
    let mut jobs = vec![];
    let mut releases = vec![];
    for i in 0..5 {
        let (release, wait) = mpsc::channel();
        releases.push(release);
        let s = s.clone();
        let entered = entered.clone();
        jobs.push(thread::spawn(move || {
            s.global()
                .with_read_for_test(|| {
                    entered.send(i).unwrap();
                    wait.recv_timeout(WAIT).unwrap()
                })
                .unwrap()
        }));
        if i < 4 {
            assert_eq!(seen.recv_timeout(WAIT).unwrap(), i);
        }
    }
    assert!(seen.recv_timeout(Duration::from_millis(150)).is_err());
    releases[0].send(()).unwrap();
    assert_eq!(seen.recv_timeout(WAIT).unwrap(), 4);
    for release in &releases[1..] {
        release.send(()).unwrap();
    }
    for job in jobs {
        job.join().unwrap();
    }
}

fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.is_file() {
        assert!(
            start.elapsed() < WAIT,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct Worker(Child);
impl Worker {
    fn start(root: &Path, mode: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "subprocess", "--ignored", "--nocapture"])
                .env(CHILD_ENV, root)
                .env("I46_LIFECYCLE_ACTION", mode)
                .spawn()
                .unwrap(),
        )
    }
    fn expect_success(&mut self) {
        let start = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "child exited with {status}");
                return;
            }
            assert!(start.elapsed() < WAIT, "child blocked");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn kill_and_wait(&mut self) {
        self.0.kill().unwrap();
        assert!(!self.0.wait().unwrap().success());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "entry point for externally controlled lifecycle child processes"]
fn subprocess() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(CHILD_ENV).unwrap());
    let mode = std::env::var("I46_LIFECYCLE_ACTION")?;
    eprintln!(
        "child pid={} root={} action={mode}",
        std::process::id(),
        root.display()
    );
    if mode.starts_with("race") {
        fs::write(root.join(&mode), [])?;
        wait_file(&root.join("go"));
        let s = Stores::open(&root)?;
        let result = s.create(ID, first());
        fs::write(
            root.join(format!("{mode}.result")),
            if result.is_ok() {
                "created"
            } else {
                "rejected"
            },
        )?;
        return Ok(());
    }
    if mode == "reject-delete" {
        let deletion = Stores::open(&root).and_then(|s| s.open_session(ID)?.delete());
        anyhow::ensure!(
            deletion.is_err(),
            "deleted under another process's shared lock"
        );
        return Ok(());
    }
    if mode == "reject-open" {
        anyhow::ensure!(
            Stores::open(&root).is_err(),
            "acknowledged failed directory barrier"
        );
        return Ok(());
    }
    let s = Stores::open(&root)?;
    let hold = || -> Result<()> {
        fs::write(root.join("ready"), [])?;
        wait_file(&root.join("release"));
        Ok(())
    };
    match mode.as_str() {
        "resume-old-child" | "contended-upgrade" => {
            child_upgrade::run(&s, &root, mode == "contended-upgrade")?;
        }
        "nested-archive" => {
            let h = s.open_session(ID)?;
            h.write(|_| {
                anyhow::ensure!(h.set_archived(true).is_err());
                Ok(())
            })?;
        }
        "disjoint-registration" => {
            let source = s.open_session(ID)?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .build()?;
            rt.block_on(async {
                let (tx, rx) = mpsc::channel();
                let mut worker = None;
                source.read(|snapshot| {
                    let stores = s.clone();
                    worker = Some(tokio::spawn(async move {
                        let result = stores
                            .create("destination", first())
                            .and_then(|h| h.delete());
                        tx.send(result).unwrap();
                    }));
                    rx.recv_timeout(Duration::from_secs(5))??;
                    assert_eq!(snapshot.initial_message()?, Some(first_message()));
                    Ok(())
                })?;
                worker.unwrap().await?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        "nested-capacity" => {
            let h = s.open_session(ID)?;
            let gate = Barrier::new(16);
            thread::scope(|scope| -> Result<()> {
                let jobs: Vec<_> = (0..16)
                    .map(|_| {
                        scope.spawn(|| {
                            h.read(|_| {
                                gate.wait();
                                anyhow::ensure!(h.read(|_| Ok(())).is_err());
                                Ok(())
                            })
                        })
                    })
                    .collect();
                for job in jobs {
                    job.join().unwrap()?;
                }
                Ok(())
            })?;
        }
        "snapshot" => s.open_session(ID)?.read(|r| {
            assert_eq!(r.initial_message()?, Some(first_message()));
            hold()
        })?,
        "before-seal" | "sealed-wal" | "installed" => {
            let phase = match mode.as_str() {
                "before-seal" => Phase::BeforeSeal,
                "sealed-wal" => Phase::Sealed,
                _ => Phase::Installed,
            };
            s.with_fault_hook(move |at| {
                if at == phase {
                    fs::write(root.join("ready"), [])?;
                    thread::sleep(WAIT);
                }
                Ok(())
            })
            .create(ID, first())?;
        }
        _ => bail!("unknown child action"),
    }
    Ok(())
}

#[test]
fn simultaneous_processes_bootstrap_and_race_create() {
    let tmp = tempdir().unwrap();
    let mut a = Worker::start(tmp.path(), "race-a");
    let mut b = Worker::start(tmp.path(), "race-b");
    wait_file(&tmp.path().join("race-a"));
    wait_file(&tmp.path().join("race-b"));
    fs::write(tmp.path().join("go"), []).unwrap();
    wait_file(&tmp.path().join("race-a.result"));
    wait_file(&tmp.path().join("race-b.result"));
    assert!(a.0.wait().unwrap().success());
    assert!(b.0.wait().unwrap().success());
    let mut outcomes = ["race-a.result", "race-b.result"]
        .map(|name| fs::read_to_string(tmp.path().join(name)).unwrap());
    outcomes.sort();
    assert_eq!(outcomes, ["created", "rejected"]);
    let s = Stores::open(tmp.path()).unwrap();
    assert_eq!(
        s.open_session(ID)
            .unwrap()
            .read(|r| r.initial_message())
            .unwrap(),
        Some(first_message())
    );
}

#[test]
fn killing_writer_releases_lock_and_replays_only_sealed_work() {
    for mode in ["before-seal", "sealed-wal", "installed"] {
        let tmp = tempdir().unwrap();
        let mut child = Worker::start(tmp.path(), mode);
        wait_file(&tmp.path().join("ready"));
        let lock = fs::File::open(tmp.path().join("state/locks/session").join(ID)).unwrap();
        assert!(matches!(
            lock.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert_eq!(pending(tmp.path()), 1);
        assert_eq!(
            query::<i64>(&global(tmp.path()), "SELECT count(*) FROM conversations"),
            0
        );
        if mode == "sealed-wal" {
            let stage = fs::read_dir(tmp.path().join("data/sessions/.staging"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert!(fs::metadata(stage.join("session.db-wal")).unwrap().len() > 32);
        }
        child.kill_and_wait();
        let s = Stores::open(tmp.path()).unwrap();
        assert_eq!(pending(tmp.path()), 0);
        if mode == "before-seal" {
            assert!(s.open_session(ID).is_err());
            s.create(ID, Default::default()).unwrap();
        } else {
            assert_eq!(
                s.open_session(ID)
                    .unwrap()
                    .read(|r| r.initial_message())
                    .unwrap(),
                Some(first_message())
            );
        }
    }
}

#[test]
fn active_child_snapshot_holds_off_delete_until_connection_closes() {
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    let h = s.create(ID, first()).unwrap();
    let mut child = Worker::start(tmp.path(), "snapshot");
    wait_file(&tmp.path().join("ready"));
    let (started, ready) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let job = thread::spawn(move || {
        started.send(()).unwrap();
        done.send(h.delete()).unwrap();
    });
    ready.recv_timeout(WAIT).unwrap();
    let start = Instant::now();
    assert!(result.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(
        query::<i64>(&global(tmp.path()), "SELECT count(*) FROM conversations"),
        1
    );
    fs::write(tmp.path().join("release"), []).unwrap();
    result.recv_timeout(WAIT).unwrap().unwrap();
    eprintln!(
        "pid={} root={} delete waited {:?} for child snapshot",
        std::process::id(),
        tmp.path().display(),
        start.elapsed()
    );
    job.join().unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(!shard(tmp.path()).exists());
}

#[cfg(windows)]
#[test]
fn windows_sharing_violation_preserves_tombstone_for_retry() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempdir().unwrap();
    let s = Stores::open(tmp.path()).unwrap();
    let h = s.create(ID, first()).unwrap();
    let file = fs::File::options()
        .read(true)
        .share_mode(3)
        .open(shard(tmp.path()))
        .unwrap();
    assert!(h.delete().is_err());
    assert!(s.open_session(ID).is_err());
    assert_eq!(pending(tmp.path()), 1);
    assert!(Stores::open(tmp.path())
        .unwrap()
        .open_session(ID)
        .is_err());
    drop((file, h, s));
    drop(Stores::open(tmp.path()).unwrap());
    assert_eq!(pending(tmp.path()), 0);
}

#[test]
fn fix_round_supported_format_checks_columns_tables_and_live_seal() {
    for change in [
        "DROP TABLE conversations",
        "ALTER TABLE project_folders DROP COLUMN trusted",
    ] {
        let root = tempdir().unwrap();
        drop(Stores::open(root.path()).unwrap());
        global(root.path()).execute_batch(change).unwrap();
        assert!(Stores::open(root.path()).is_err(), "{change}");
    }
    for change in [
        "DROP TABLE message_user_content",
        "DROP TRIGGER transcript_thinking_insert; DROP TRIGGER transcript_thinking_update; DROP TRIGGER transcript_thinking_delete; ALTER TABLE messages DROP COLUMN thinking",
        "UPDATE manifest SET sealed_operation=''",
        "UPDATE manifest SET sealed_operation='bogus'",
    ] {
        let root = tempdir().unwrap();
        let stores = Stores::open(root.path()).unwrap();
        let h = stores.create(ID, first()).unwrap();
        Connection::open(shard(root.path()))
            .unwrap()
            .execute_batch(change)
            .unwrap();
        assert!(stores.open_session(ID).is_err(), "{change}");
        assert!(h.read(|_| Ok(())).is_err(), "{change}");
        assert!(h.write(|_| Ok(())).is_err(), "{change}");
    }
}

#[test]
fn fix_round_structural_damage_is_quarantined_before_publication_or_retirement() -> Result<()> {
    for (deleting, change) in [
        (false, "DROP TABLE manifest"),
        (false, "DROP TABLE message_user_content"),
        (false, "DELETE FROM messages"),
        (false, "DELETE FROM message_user_content"),
        (true, "DROP TABLE manifest"),
        (true, "corrupt file"),
    ] {
        let root = tempdir()?;
        let stores = Stores::open(root.path())?;
        stores.create("healthy", first())?;
        let path = if deleting {
            stores.create(ID, first())?;
            assert!(stop_at(&stores, Phase::DeleteIntent)
                .open_session(ID)?
                .delete()
                .is_err());
            shard(root.path())
        } else {
            assert!(stop_at(&stores, Phase::Sealed).create(ID, first()).is_err());
            fs::read_dir(root.path().join("data/sessions/.staging"))?
                .next()
                .unwrap()?
                .path()
                .join("session.db")
        };
        if change == "corrupt file" {
            fs::write(path, b"damaged retiring session")?;
        } else {
            let db = Connection::open(path)?;
            db.execute_batch("PRAGMA foreign_keys=OFF")?;
            db.execute_batch(change)?;
        }
        drop(stores);
        for _ in 0..2 {
            let s = Stores::open(root.path())?;
            assert!(s.open_session(ID).is_err());
            s.open_session("healthy")?;
            assert_eq!(pending(root.path()), 0);
            assert_eq!(
                query::<i64>(
                    &global(root.path()),
                    "SELECT count(*) FROM lifecycle WHERE length(quarantine_reason)>0"
                ),
                1
            );
            assert_eq!(count(root.path(), "data/sessions/.quarantine"), 1);
            if deleting {
                assert!(s.create(ID, first()).is_err());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn fix_round_namespace_loss_cannot_split_an_active_snapshot() -> Result<()> {
    for name in [
        "state/locks/session",
        "state/locks/session/.initialized",
        &format!("state/locks/session/{ID}"),
    ] {
        let root = tempdir()?;
        let stores = Stores::open(root.path())?;
        let handle = stores.create(ID, first())?;
        let mut holder = Worker::start(root.path(), "snapshot");
        wait_file(&root.path().join("ready"));
        let old = root.path().join(name);
        fs::rename(&old, root.path().join("held"))?;
        Worker::start(root.path(), "reject-delete").expect_success();
        assert!(!old.exists(), "{name} was recreated");
        assert!(handle.read(|_| Ok(())).is_err());
        assert_eq!(
            query::<i64>(&global(root.path()), "SELECT count(*) FROM conversations"),
            1
        );
        fs::write(root.path().join("release"), [])?;
        holder.expect_success();
    }
    Ok(())
}

#[test]
fn fix_round_transaction_callback_cannot_upgrade_a_shared_lock() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    stores.create(ID, first())?;
    Worker::start(root.path(), "nested-archive").expect_success();
    assert!(!stores.global().conversation(ID)?.unwrap().archived);
    Ok(())
}

#[test]
fn fix_round_nested_read_cannot_wait_for_a_seventeenth_permit() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    stores.create(ID, first())?;
    Worker::start(root.path(), "nested-capacity").expect_success();
    stores.open_session(ID)?.delete()
}

#[test]
fn fix_round_resident_readers_finish_a_killed_creators_sealed_intent() -> Result<()> {
    for directory_first in [true, false] {
        let root = tempdir()?;
        let stores = Stores::open(root.path())?;
        let mut child = Worker::start(root.path(), "sealed-wal");
        wait_file(&root.path().join("ready"));
        child.kill_and_wait();
        assert_eq!(pending(root.path()), 1);
        if directory_first {
            assert_eq!(stores.global().conversation(ID)?.unwrap().message_count, 1);
        }
        let session = stores.open_session(ID)?;
        assert_eq!(
            session.read(|r| r.initial_message())?,
            Some(first_message())
        );
        assert_eq!(pending(root.path()), 0);
        assert_eq!(
            session.incarnation(),
            query::<String>(
                &global(root.path()),
                "SELECT incarnation FROM conversations"
            )
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn fix_round_pooled_global_reads_detect_path_replacement() -> Result<()> {
    let a = tempdir()?;
    let b = tempdir()?;
    let stores = Stores::open(a.path())?;
    let handle = stores.create(ID, first())?;
    drop(Stores::open(b.path())?);
    assert!(stores.global().conversation(ID)?.is_some());
    fs::rename(b.path().join("global.db"), a.path().join("global.db"))?;
    assert!(stores.global().conversation(ID).is_err());
    assert!(handle.read(|_| Ok(())).is_err());
    Ok(())
}

#[test]
fn fix_round_orphan_sweep_requires_proof_of_local_ownership() -> Result<()> {
    let root = tempdir()?;
    let other = tempdir()?;
    let stores = Stores::open(root.path())?;
    Stores::open(other.path())?.create(ID, first())?;
    let foreign = shard(other.path());
    let seal: String = query(
        &Connection::open(&foreign)?,
        "SELECT sealed_operation FROM manifest",
    );
    let sessions = root.path().join("data/sessions");
    let legacy_key = "aaaaaaaa-1111-2222-3333-000000000000";
    let quarantine = sessions.join(".quarantine");
    let unrelated = quarantine.join(".orphan-inspection");
    let foreign_scratch = quarantine.join(".orphan-inspection-foreign");
    let mixed = quarantine.join(".orphan-inspection-mixed");
    let interrupted = quarantine.join(".orphan-inspection-interrupted");
    for dir in [&unrelated, &foreign_scratch, &mixed, &interrupted] {
        fs::create_dir(dir)?;
    }
    fs::write(unrelated.join("sentinel"), b"original")?;
    fs::write(foreign_scratch.join("store-uuid"), legacy_key)?;
    fs::write(mixed.join("store-uuid"), stores.global().store_uuid())?;
    fs::write(mixed.join("sentinel"), b"original")?;
    fs::write(interrupted.join("store-uuid"), stores.global().store_uuid())?;
    fs::write(interrupted.join("session.db"), b"partial copy")?;
    let paths = [
        sessions.join(legacy_key),
        sessions.join(".staging").join(legacy_key),
        sessions.join(ID),
        sessions.join(".staging").join(&seal),
    ];
    for (i, dir) in paths.iter().enumerate() {
        fs::create_dir(dir)?;
        if i < 2 {
            Connection::open(dir.join("session.db"))?
                .execute_batch("CREATE TABLE legacy_data(value TEXT)")?;
        } else {
            fs::copy(&foreign, dir.join("session.db"))?;
        }
    }
    assert!(stop_at(&stores, Phase::Sealed)
        .create("unowned", first())
        .is_err());
    global(root.path()).execute("DELETE FROM lifecycle", [])?;
    drop(stores);
    drop(Stores::open(root.path())?);
    for dir in paths {
        assert!(dir.join("session.db").is_file());
        assert_eq!(
            fs::read_dir(dir)?.count(),
            1,
            "orphan inspection created sidecars"
        );
    }
    assert_eq!(fs::read(unrelated.join("sentinel"))?, b"original");
    assert_eq!(
        fs::read(foreign_scratch.join("store-uuid"))?,
        legacy_key.as_bytes()
    );
    assert_eq!(fs::read(mixed.join("sentinel"))?, b"original");
    assert!(!interrupted.exists());
    assert_eq!(count(root.path(), "data/sessions/.quarantine"), 4);
    Ok(())
}

#[test]
fn orphan_sweep_recognizes_a_local_seal_in_a_killed_writers_wal() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let mut creator = Worker::start(root.path(), "sealed-wal");
    wait_file(&root.path().join("ready"));
    creator.kill_and_wait();
    let stage = fs::read_dir(root.path().join("data/sessions/.staging"))?
        .next()
        .unwrap()?
        .path();
    assert!(stage.join("session.db-wal").metadata()?.len() > 32);
    global(root.path()).execute("DELETE FROM lifecycle", [])?;
    drop(stores);
    let reopened = Stores::open(root.path())?;
    assert!(reopened.open_session(ID).is_err());
    assert!(!stage.exists());
    assert_eq!(count(root.path(), "data/sessions/.quarantine"), 1);
    Ok(())
}

#[test]
fn export_requires_an_existing_root_and_host_startup_creates_it() -> Result<()> {
    let parent = tempdir()?;
    let missing = parent.path().join("missing/nested");
    assert!(Stores::export_session(&missing, ID, std::io::sink()).is_err());
    assert!(!missing.exists());
    Stores::open(&missing)?;
    assert!(missing.join("global.db").is_file());
    Ok(())
}

#[test]
fn partial_trash_removal_replays_only_after_cleanup_is_authorized() -> Result<()> {
    for authorized in [false, true] {
        let root = tempdir()?;
        let s = Stores::open(root.path())?;
        let at = if authorized {
            Phase::BeforeTrashRemoval
        } else {
            Phase::Trashed
        };
        let h = stop_at(&s, at).create(ID, first())?;
        assert!(h.delete().is_err());
        fs::remove_file(
            root.path()
                .join("data/sessions/.trash")
                .join(h.incarnation())
                .join("session.db"),
        )?;
        drop((h, s));
        let reopened = Stores::open(root.path())?;
        assert!(reopened.open_session(ID).is_err());
        assert_eq!(pending(root.path()), 0);
        assert_eq!(count(root.path(), "data/sessions/.trash"), 0);
        assert_eq!(
            count(root.path(), "data/sessions/.quarantine"),
            usize::from(!authorized)
        );
        assert_eq!(reopened.create(ID, first()).is_ok(), authorized);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn fix_round_rename_recovery_retries_both_parent_barriers() -> Result<()> {
    let fixture = tempdir()?;
    let library = fixture.path().join("fault.so");
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&library)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session_db_fsync.c"))
        .arg("-ldl")
        .status()?
        .success());
    let reject = |root: &Path, sync: &Path, after: &Path| -> Result<()> {
        let child = Command::new(std::env::current_exe()?)
            .args(["--exact", "subprocess", "--ignored", "--nocapture"])
            .env(CHILD_ENV, root)
            .env("I46_LIFECYCLE_ACTION", "reject-open")
            .env("LD_PRELOAD", &library)
            .env("I46_SYNC_DIRECTORY", sync)
            .env("I46_SYNC_AFTER", after)
            .spawn()?;
        Worker(child).expect_success();
        Ok(())
    };
    let tmp = tempdir()?;
    let root = tmp.path().canonicalize()?;
    let global_db = root.join("global.db");
    for sync in [&root, &root, &root.join(".global-staging")] {
        reject(&root, sync, &global_db)?;
        assert!(global_db.is_file());
        assert!(!root.join("state/locks/session/.initialized").exists());
    }
    let stores = Stores::open(&root)?;
    assert!(stop_at(&stores, Phase::Sealed).create(ID, first()).is_err());
    let stage_parent = root.join("data/sessions/.staging");
    let op: String = query(&global(&root), "SELECT operation FROM lifecycle");
    Connection::open(stage_parent.join(&op).join("session.db"))?
        .execute("UPDATE manifest SET sealed_operation='broken'", [])?;
    drop(stores);
    let quarantine = root.join("data/sessions/.quarantine");
    for sync in [&quarantine, &quarantine, &stage_parent] {
        reject(&root, sync, &quarantine.join(&op))?;
        assert_eq!(pending(&root), 1);
    }
    let stores = Stores::open(&root)?;
    assert_eq!(pending(&root), 0);
    assert!(stores.open_session(ID).is_err());
    assert!(
        query::<String>(&global(&root), "SELECT quarantine_reason FROM lifecycle").contains("seal")
    );

    let orphan_root = tempdir()?;
    let root = orphan_root.path().canonicalize()?;
    Stores::open(&root)?.create(ID, first())?;
    global(&root).execute("DELETE FROM conversations", [])?;
    let quarantine = root.join("data/sessions/.quarantine");
    for parent in [&quarantine, &quarantine, &root.join("data/sessions")] {
        reject(&root, parent, &root.join("global.db"))?;
        assert_eq!(count(&root, "data/sessions/.quarantine"), 1);
        assert!(!shard(&root).exists());
    }
    assert!(Stores::open(&root)?.open_session(ID).is_err());
    Ok(())
}

#[test]
fn round2_disjoint_registration_finishes_during_source_snapshot() -> Result<()> {
    let root = tempdir()?;
    Stores::open(root.path())?.create(ID, first())?;
    Worker::start(root.path(), "disjoint-registration").expect_success();
    Ok(())
}

#[test]
fn round2_typed_history_must_be_readable_before_publish_and_on_open() -> Result<()> {
    for change in [
        "UPDATE message_user_content SET content_json=CAST(content_json AS BLOB)",
        "UPDATE message_user_content SET message_id=NULL",
        "PRAGMA foreign_keys=OFF; UPDATE message_user_content SET message_id=CAST(message_id AS BLOB)",
        "UPDATE message_user_content SET content_json='1e400'",
        "UPDATE messages SET thinking=x'80'",
        "UPDATE manifest SET updated_at=x'80'",
    ] {
        for staged in [true, false] {
            let root = tempdir()?;
            let s = Stores::open(root.path())?;
            let path = if staged {
                assert!(stop_at(&s, Phase::Sealed).create(ID, first()).is_err());
                fs::read_dir(root.path().join("data/sessions/.staging"))?
                    .next()
                    .unwrap()?
                    .path()
                    .join("session.db")
            } else {
                s.create(ID, first())?;
                shard(root.path())
            };
            Connection::open(path)?.execute_batch(change)?;
            if staged {
                drop(s);
                let reopened = Stores::open(root.path())?;
                assert!(reopened.global().conversation(ID)?.is_none(), "{change}");
                assert_eq!(pending(root.path()), 0);
                assert_eq!(count(root.path(), "data/sessions/.quarantine"), 1);
            } else {
                assert!(s.open_session(ID).is_err(), "{change}");
            }
        }
    }
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let deep = (0..150).fold(serde_json::Value::Null, |v, _| {
        serde_json::Value::Array(vec![v])
    });
    let mut options = first();
    options.first_message.as_mut().unwrap().typed = Some(deep);
    assert!(s.create(ID, options).is_err());
    assert!(s.global().conversation(ID)?.is_none());
    assert_eq!(count(root.path(), "data/sessions/.quarantine"), 1);
    Ok(())
}

#[test]
fn round2_pending_shard_path_damage_is_local_to_create_and_delete() -> Result<()> {
    for directory in [true, false] {
        if !directory && !cfg!(unix) {
            continue;
        }
        for deleting in [false, true] {
            let root = tempdir()?;
            let s = Stores::open(root.path())?;
            s.create("healthy", first())?;
            let path = if deleting {
                let handle = stop_at(&s, Phase::DeleteIntent).create(ID, first())?;
                assert!(handle.delete().is_err());
                shard(root.path())
            } else {
                assert!(stop_at(&s, Phase::Sealed).create(ID, first()).is_err());
                fs::read_dir(root.path().join("data/sessions/.staging"))?
                    .next()
                    .unwrap()?
                    .path()
                    .join("session.db")
            };
            if directory {
                fs::rename(&path, path.with_extension("saved"))?;
                fs::create_dir(path)?;
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0))?;
                    // Privileged test runners may bypass mode bits.
                    if fs::File::open(&path).is_ok() {
                        continue;
                    }
                }
            }
            drop(s);
            for _ in 0..2 {
                let reopened = Stores::open(root.path())?;
                assert_eq!(
                    reopened
                        .open_session("healthy")?
                        .read(|r| r.initial_message())?,
                    Some(first_message())
                );
                assert!(reopened.open_session(ID).is_err());
                assert_eq!(pending(root.path()), 0);
                let reason: String = query(
                    &global(root.path()),
                    "SELECT quarantine_reason FROM lifecycle",
                );
                assert!(
                    reason.contains(if directory {
                        "regular file"
                    } else {
                        "opening "
                    }),
                    "{reason}"
                );
                let evidence = fs::read_dir(root.path().join("data/sessions/.quarantine"))?
                    .next()
                    .unwrap()?
                    .path();
                if directory {
                    assert!(evidence.join("session.db").is_dir());
                    assert!(evidence.join("session.saved").is_file());
                } else {
                    assert!(evidence.join("session.db").is_file());
                }
                if deleting {
                    assert!(reopened.create(ID, first()).is_err());
                }
            }
        }
    }
    Ok(())
}
