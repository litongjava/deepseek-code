use super::*;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn note(name: &str, body: &str) -> Vec<u8> {
    format!("---\nname: {name}\ndescription: Relevant when reviewing tests\nmetadata:\n  type: user\n  custom: keep-this\n---\n{body}\n").into_bytes()
}

fn store(root: &Path) -> FileMemoryStore {
    FileMemoryStore::open(root, &MemoryScope::private("fixture-owner").unwrap()).unwrap()
}

fn state(root: &Path) -> std::path::PathBuf {
    root.join("data/memory/state")
        .join(MemoryScope::private("fixture-owner").unwrap().key())
}

// Every private child is reaped even if an assertion fails before the barrier.
struct OwnedChild(Child);
impl std::ops::Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
trait SpawnOwned {
    fn spawn_owned(&mut self) -> std::io::Result<OwnedChild>;
}
impl SpawnOwned for Command {
    fn spawn_owned(&mut self) -> std::io::Result<OwnedChild> {
        self.spawn().map(OwnedChild)
    }
}

fn bounded_wait(child: &mut Child) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("private memory test child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child(root: &Path, mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "memory::files::tests::process_worker",
            "--nocapture",
        ])
        .env("DCL_MEMORY_TEST_ROOT", root)
        .env("DCL_MEMORY_TEST_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Subprocess harness entry, a no-op in the ordinary parent test run.
#[test]
fn process_worker() {
    let Some(root) = std::env::var_os("DCL_MEMORY_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    match std::env::var("DCL_MEMORY_TEST_MODE").unwrap().as_str() {
        "cas" => {
            let id = std::env::var("DCL_MEMORY_TEST_ID").unwrap();
            let memory = store(&root);
            let before = memory.read("one.md").unwrap().unwrap();
            fs::write(root.join(format!("ready-{id}")), &before.hash).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !root.join("go").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            let result = memory.put(
                "one.md",
                &note("One", &format!("writer {id}")),
                ExpectedVersion::Hash(before.hash),
            );
            let status = match result {
                Ok(_) => "success",
                Err(MemoryFileError::Conflict(_)) => "conflict",
                Err(e) => panic!("{e}"),
            };
            fs::write(root.join(format!("result-{id}")), status).unwrap();
        }
        "crash-repair" => {
            let memory = store(&root);
            let (_, token) = memory
                .snapshot()
                .unwrap()
                .repair_preview("fixture")
                .unwrap();
            memory.confirm_repair("fixture", &token).unwrap();
            panic!("configured repair crash checkpoint was not reached");
        }
        "crash" | "crash-delete" => {
            let memory = store(&root);
            let before = memory.read("one.md").unwrap().unwrap();
            if std::env::var("DCL_MEMORY_TEST_MODE").unwrap() == "crash-delete" {
                memory
                    .delete("one.md", ExpectedVersion::Hash(before.hash))
                    .unwrap();
            } else {
                memory
                    .put(
                        "one.md",
                        &note("Changed", "new fact 中文"),
                        ExpectedVersion::Hash(before.hash),
                    )
                    .unwrap();
            }
            panic!("configured crash checkpoint was not reached");
        }
        "lock" => {
            let memory = store(&root);
            let path = state(&root).join("lock");
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            crate::fs::lock_file(&file).unwrap();
            fs::write(root.join("lock-ready"), b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !root.join("release").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            crate::fs::unlock_file(&file).unwrap();
            drop(memory);
        }
        other => panic!("unknown private test mode {other}"),
    }
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn scope_git_common_directory_shares_worktrees_and_subdirectories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    // Minimal isolated Git metadata; no git init/add/commit/worktree mutation.
    fs::create_dir_all(root.join(".git/objects")).unwrap();
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    let tree = temp.path().join("linked");
    fs::create_dir_all(&tree).unwrap();
    let metadata = root.join(".git/worktrees/linked");
    fs::create_dir_all(&metadata).unwrap();
    fs::write(metadata.join("HEAD"), b"ref: refs/heads/linked\n").unwrap();
    fs::write(metadata.join("commondir"), b"../..\n").unwrap();
    fs::write(
        metadata.join("gitdir"),
        format!("{}\n", tree.join(".git").display()),
    )
    .unwrap();
    fs::write(
        tree.join(".git"),
        format!("gitdir: {}\n", metadata.display()),
    )
    .unwrap();
    let scope = MemoryScope::project(&[root.clone()], true).unwrap();
    assert_eq!(
        scope,
        MemoryScope::project(&[root.join("src/nested")], true).unwrap()
    );
    assert_eq!(scope, MemoryScope::project(&[tree], true).unwrap());
    assert_eq!(scope.key().len(), 67);
    let clone = temp.path().join("other-clone");
    fs::create_dir_all(clone.join(".git/objects")).unwrap();
    fs::create_dir_all(clone.join(".git/refs/heads")).unwrap();
    fs::write(clone.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    assert_ne!(scope, MemoryScope::project(&[clone], true).unwrap());
}

#[test]
fn scope_non_git_multi_root_private_and_trust_are_explicit() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let a = a.path().to_owned();
    let b = b.path().to_owned();
    let single = MemoryScope::project(&[a.clone()], true).unwrap();
    assert_eq!(
        single,
        MemoryScope::project(&[a.clone(), a.clone()], true).unwrap()
    );
    assert_ne!(single, MemoryScope::project(&[b.clone()], true).unwrap());
    let multi = MemoryScope::project(&[a.clone(), b.clone()], true).unwrap();
    assert_eq!(
        multi,
        MemoryScope::project(&[b.clone(), a.clone(), b], true).unwrap()
    );
    assert_ne!(multi, single);
    assert_ne!(
        MemoryScope::private("owner").unwrap(),
        MemoryScope::private("other").unwrap()
    );
    assert_eq!(
        MemoryScope::private("owner").unwrap(),
        MemoryScope::private("owner").unwrap()
    );
    assert!(MemoryScope::private("../other").is_err());
    assert!(MemoryScope::project(&[a], false).is_err());
    assert!(MemoryScope::project(&[], true).is_err());
    assert!(MemoryScope::project(&["relative".into()], true).is_err());
}

#[test]
fn corrupt_git_boundary_is_not_reclassified_as_non_git() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(".git"), "gitdir: does-not-exist\n").unwrap();
    assert!(MemoryScope::project(&[root.path().to_owned()], true).is_err());
}

#[cfg(unix)]
#[test]
fn scope_preserves_non_utf8_identity_without_lossy_collisions() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let a = root
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
    let b = root
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'a', 0xfe]));
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    assert_ne!(
        MemoryScope::project(&[a], true).unwrap(),
        MemoryScope::project(&[b], true).unwrap()
    );
}

#[test]
fn format_accepts_chinese_english_and_required_feedback_sections() {
    let punctuation = note("'Review & apply * carefully!'", "Ordinary punctuation.");
    assert_eq!(
        MemoryDocument::parse(&punctuation).unwrap().name,
        "Review & apply * carefully!"
    );
    let chinese = note("验证范围", "事实：分别报告各层验证结果。");
    assert_eq!(MemoryDocument::parse(&chinese).unwrap().name, "验证范围");
    let mut feedback = String::from_utf8(note("Testing", "A durable fact.\n\n## Why\nUser corrected the report.\n\n## How to apply\nReport each layer.")).unwrap().replace("type: user", "type: feedback");
    assert_eq!(
        MemoryDocument::parse(feedback.as_bytes()).unwrap().kind,
        MemoryKind::Feedback
    );
    feedback = feedback.replace("## Why", "## Reason");
    assert!(MemoryDocument::parse(feedback.as_bytes()).is_err());
    assert!(MemoryDocument::parse(b"---\nname: x\ndescription: x\nmetadata:\n  type: project\n---\nfact\n## Why\n\n## How to apply\n\n").is_err());
}

#[test]
fn default_home_and_empty_workspace_use_persistent_private_owner() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let private = MemoryScope::private("parent-session").unwrap();
    assert_eq!(
        private,
        MemoryScope::for_workspace(&[], false, home.path(), "parent-session").unwrap()
    );
    assert_eq!(
        private,
        MemoryScope::for_workspace(
            &[home.path().to_owned()],
            true,
            home.path(),
            "parent-session"
        )
        .unwrap()
    );
    assert_ne!(
        private,
        MemoryScope::for_workspace(
            &[home.path().to_owned()],
            true,
            home.path(),
            "other-session"
        )
        .unwrap()
    );
    assert_eq!(
        MemoryScope::project(&[project.path().to_owned()], true).unwrap(),
        MemoryScope::for_workspace(
            &[project.path().to_owned()],
            true,
            home.path(),
            "parent-session"
        )
        .unwrap()
    );
    assert!(MemoryScope::for_workspace(
        &[project.path().to_owned()],
        false,
        home.path(),
        "parent-session"
    )
    .is_err());
}

#[test]
fn format_rejects_missing_duplicate_tag_alias_depth_limits_and_non_utf8() {
    let good = String::from_utf8(note("Good", "fact")).unwrap();
    for bad in [
        good.replace("name: Good\n", ""),
        good.replace("description: Relevant when reviewing tests\n", ""),
        good.replace("type: user", "type: dream"),
        good.replace("name: Good", "name: Good\nname: Duplicate"),
        good.replace("name: Good", "name: &anchor Good"),
        good.replace("name: Good", "name: *anchor"),
        good.replace("name: Good", "name: !tag Good"),
        good.replace("type: user", "type: user\n  created_at: tomorrow"),
        good.replace("name: Good", &format!("name: {}", "长".repeat(81))),
        good.replace(
            "name: Good",
            &format!(
                "name: Good\nextra: [{}]",
                "[".repeat(12) + "0" + &"]".repeat(12)
            ),
        ),
        good.replace(
            "name: Good",
            &format!("name: Good\nextra: {}", "x".repeat(MAX_FRONTMATTER_BYTES)),
        ),
    ] {
        assert!(MemoryDocument::parse(bad.as_bytes()).is_err());
    }
    assert!(MemoryDocument::parse(&[0xff, 0xfe]).is_err());
    assert!(MemoryDocument::parse(&note("Too big", &"中".repeat(MAX_FILE_BYTES))).is_err());
}

#[test]
fn format_rejects_known_credentials_without_exposing_them_in_error() {
    let raw = note("Credential", "password=synthetic-do-not-use");
    let error = MemoryDocument::parse(&raw).unwrap_err().to_string();
    assert!(!error.contains("synthetic-do-not-use"));
    assert!(error.contains("credential"));
}

#[test]
fn unicode_names_have_canonical_full_casefold_keys() {
    assert_eq!(format::name_key("École"), format::name_key("E\u{301}COLE"));
    assert_eq!(format::name_key("Straße"), format::name_key("STRASSE"));
    assert_eq!(format::name_key("Σςσ"), format::name_key("σσσ"));
    assert_ne!(format::name_key("ı"), format::name_key("i"));
}

#[test]
fn store_crud_persists_ids_extensions_generation_and_index() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let first = memory
        .put(
            "one.md",
            &note("中文标题", "事实一。 [[Missing]]"),
            ExpectedVersion::Absent,
        )
        .unwrap();
    assert!(first.document.id().is_some());
    let initial = memory.snapshot().unwrap();
    assert!(initial.index.is_empty());
    assert!(!memory.index_path().exists());
    assert_eq!(initial.entries.len(), 1);
    assert!(initial
        .diagnostics
        .iter()
        .any(|d| d.message.contains("unresolved")));
    let next = memory
        .put(
            "one.md",
            &note("English", "A revised fact."),
            ExpectedVersion::Hash(first.hash.clone()),
        )
        .unwrap();
    assert_eq!(first.document.id(), next.document.id());
    assert_eq!(
        next.document.frontmatter["metadata"]["custom"].as_str(),
        Some("keep-this")
    );
    assert_eq!(
        first.document.frontmatter["metadata"]["created_at"],
        next.document.frontmatter["metadata"]["created_at"]
    );
    assert!(matches!(
        memory.put(
            "one.md",
            &note("Bad", "stale"),
            ExpectedVersion::Hash(first.hash)
        ),
        Err(MemoryFileError::Conflict(_))
    ));
    drop(memory);
    let memory = store(root.path());
    assert_eq!(memory.read("one.md").unwrap().unwrap().hash, next.hash);
    assert!(memory.snapshot().unwrap().generation > initial.generation);
    memory
        .delete("one.md", ExpectedVersion::Hash(next.hash))
        .unwrap();
    assert!(memory.snapshot().unwrap().index.is_empty());
    assert!(memory.read("one.md").unwrap().is_none());
    assert!(!fs::read_dir(
        root.path()
            .join("data/memory/trash")
            .join(MemoryScope::private("fixture-owner").unwrap().key())
    )
    .unwrap()
    .collect::<Vec<_>>()
    .is_empty());
}

#[test]
fn stores_never_cross_private_scope_and_reject_rebound_identity() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    memory
        .put("one.md", &note("One", "private"), ExpectedVersion::Absent)
        .unwrap();
    let other =
        FileMemoryStore::open(root.path(), &MemoryScope::private("other").unwrap()).unwrap();
    assert!(other.read("one.md").unwrap().is_none());
    fs::write(state(root.path()).join("scope.json"), b"{}").unwrap();
    assert!(
        FileMemoryStore::open(root.path(), &MemoryScope::private("fixture-owner").unwrap())
            .is_err()
    );
}

#[test]
fn invalid_writes_and_duplicate_names_leave_existing_bytes_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let first = memory
        .put("one.md", &note("Straße", "keep"), ExpectedVersion::Absent)
        .unwrap();
    let before = memory.snapshot().unwrap();
    assert!(memory
        .put(
            "two.md",
            &note("STRASSE", "duplicate"),
            ExpectedVersion::Absent
        )
        .is_err());
    assert!(memory
        .put(
            "one.md",
            b"broken",
            ExpectedVersion::Hash(first.hash.clone())
        )
        .is_err());
    assert!(memory
        .put("one.md", &note("Overwrite", "bad"), ExpectedVersion::Absent)
        .is_err());
    assert_eq!(memory.read("one.md").unwrap().unwrap().bytes, first.bytes);
    assert_eq!(memory.snapshot().unwrap().generation, before.generation);
    assert!(memory.read("two.md").unwrap().is_none());
}

#[test]
fn external_bad_files_are_retained_diagnosed_and_not_indexed() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    fs::write(memory.directory().join("valid.md"), note("Valid", "fact")).unwrap();
    let invalids = [
        ("missing.md", b"no frontmatter".to_vec()),
        ("encoding.md", vec![0xff]),
        ("huge.md", vec![b'x'; MAX_FILE_BYTES + 1]),
    ];
    for (name, bytes) in &invalids {
        fs::write(memory.directory().join(name), bytes).unwrap();
    }
    let snapshot = memory.snapshot().unwrap();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.diagnostics.len(), 3);
    let (proposal, token) = snapshot.repair_preview("fixture").unwrap();
    memory.confirm_repair("fixture", &token).unwrap();
    assert_eq!(memory.snapshot().unwrap().index, proposal);
    assert!(proposal.contains("valid.md"));
    for (name, bytes) in &invalids {
        assert!(!proposal.contains(name));
        assert_eq!(fs::read(memory.directory().join(name)).unwrap(), *bytes);
    }
}

#[test]
fn index_repairs_missing_stale_and_deleted_pointers_preserving_valid_order() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    memory
        .put(
            "one.md",
            &note("One ](aside", "fact"),
            ExpectedVersion::Absent,
        )
        .unwrap();
    memory
        .put("two.md", &note("Two", "fact"), ExpectedVersion::Absent)
        .unwrap();
    fs::write(
        memory.index_path(),
        "arbitrary old index text\r\n- [stale](two.md) — old\r\n- [invented](ghost.md) — missing\n",
    )
    .unwrap();
    let snapshot = memory.snapshot().unwrap();
    let (proposal, token) = snapshot.repair_preview("fixture").unwrap();
    assert_eq!(
        fs::read_to_string(memory.index_path()).unwrap(),
        snapshot.index
    );
    assert!(proposal.starts_with("arbitrary old index text\r\n- [Two](two.md)"));
    assert!(proposal.contains("one.md"));
    assert!(!proposal.contains("ghost.md"));
    memory.confirm_repair("fixture", &token).unwrap();
    assert_eq!(memory.snapshot().unwrap().index, proposal);
    fs::remove_file(memory.directory().join("two.md")).unwrap();
    let stale = memory.snapshot().unwrap();
    assert_eq!(stale.index, proposal);
    let (_, token) = stale.repair_preview("fixture").unwrap();
    memory.confirm_repair("fixture", &token).unwrap();
    assert!(!memory.snapshot().unwrap().index.contains("two.md"));
    let clean = memory.snapshot().unwrap();
    let (_, token) = clean.repair_preview("fixture").unwrap();
    memory.confirm_repair("fixture", &token).unwrap();
    assert_eq!(memory.snapshot().unwrap().generation, clean.generation);
}

#[test]
fn index_repair_refuses_changed_preimage_or_proposal() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    memory
        .put("one.md", &note("One", "fact"), ExpectedVersion::Absent)
        .unwrap();
    let before = memory.snapshot().unwrap();
    let (_, token) = before.repair_preview("fixture").unwrap();
    fs::write(memory.index_path(), b"authored\r\n").unwrap();
    assert!(matches!(
        memory.confirm_repair("fixture", &token),
        Err(MemoryFileError::Conflict(_))
    ));
    let (_, token) = memory
        .snapshot()
        .unwrap()
        .repair_preview("fixture")
        .unwrap();
    memory
        .put("two.md", &note("Two", "fact"), ExpectedVersion::Absent)
        .unwrap();
    assert!(matches!(
        memory.confirm_repair("fixture", &token),
        Err(MemoryFileError::Conflict(_))
    ));
    assert_eq!(fs::read(memory.index_path()).unwrap(), b"authored\r\n");
}

#[test]
fn index_context_obeys_line_byte_token_limits_with_complete_lines_and_counts() {
    let index = (0..300)
        .map(|i| format!("- [中文 {i}](item-{i}.md) — {}\n", "说明".repeat(40)))
        .collect::<String>();
    for limits in [
        IndexLimits {
            lines: 200,
            bytes: usize::MAX,
            tokens: usize::MAX,
        },
        IndexLimits {
            lines: usize::MAX,
            bytes: 25 * 1024,
            tokens: usize::MAX,
        },
        IndexLimits {
            lines: usize::MAX,
            bytes: usize::MAX,
            tokens: 4096,
        },
    ] {
        let count = |s: &str| s.chars().count();
        let result = IndexContext::from_index(&index, "/test/MEMORY.md", limits, count);
        assert!(result.truncated && result.marker_included);
        assert_eq!(result.included + result.omitted, 300);
        assert!(result.text.lines().count() <= limits.lines);
        assert!(result.text.len() <= limits.bytes);
        assert!(count(&result.text) <= limits.tokens);
        for row in result.text.lines().take(result.included) {
            assert!(index.lines().any(|whole| whole == row));
        }
        assert!(result
            .text
            .contains(&format!("{} entries omitted", result.omitted)));
        assert!(result.text.contains("/test/MEMORY.md"));
    }
    let small = "- [One](one.md) — A fact\n";
    assert_eq!(
        IndexContext::from_index(small, "MEMORY.md", IndexLimits::default(), str::len).text,
        small
    );
    let zero = IndexContext::from_index(
        small,
        "MEMORY.md",
        IndexLimits {
            lines: 0,
            bytes: 0,
            tokens: 0,
        },
        str::len,
    );
    assert!(zero.text.is_empty() && zero.truncated && !zero.marker_included);
    assert_eq!(zero.omitted, 1);
}

#[test]
fn traversal_absolute_devices_and_reserved_index_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    for path in [
        "../escape.md",
        "/tmp/escape.md",
        "a/b.md",
        "a\\b.md",
        "CON.md",
        "nul.md",
        "com1.md",
        "file:ads.md",
        "MEMORY.md",
        "memory.md",
        "bad..md",
        "ends.md.",
        "a.md ",
    ] {
        assert!(
            memory
                .put(path, &note("One", "fact"), ExpectedVersion::Absent)
                .is_err(),
            "{path}"
        );
        assert!(
            memory.delete(path, ExpectedVersion::Absent).is_err(),
            "{path}"
        );
        assert!(memory.read(path).is_err(), "{path}");
    }
}

#[test]
fn hardlinks_are_never_read_modified_or_deleted_by_the_store() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let outside = root.path().join("outside.md");
    let bytes = note("Outside", "keep");
    fs::write(&outside, &bytes).unwrap();
    fs::hard_link(&outside, memory.directory().join("linked.md")).unwrap();
    assert!(memory.read("linked.md").is_err());
    assert!(memory
        .put(
            "linked.md",
            &note("Overwrite", "bad"),
            ExpectedVersion::Hash(hash(&bytes))
        )
        .is_err());
    assert!(memory
        .delete("linked.md", ExpectedVersion::Hash(hash(&bytes)))
        .is_err());
    assert_eq!(fs::read(outside).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn symlink_files_and_directory_swaps_never_follow_outside_targets() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let outside = tempfile::tempdir().unwrap();
    let bytes = note("Outside", "keep");
    fs::write(outside.path().join("one.md"), &bytes).unwrap();
    symlink(
        outside.path().join("one.md"),
        memory.directory().join("one.md"),
    )
    .unwrap();
    assert!(memory.read("one.md").is_err());
    assert!(memory
        .put("one.md", &note("Overwrite", "bad"), ExpectedVersion::Absent)
        .is_err());
    assert!(memory
        .delete("one.md", ExpectedVersion::Hash(hash(&bytes)))
        .is_err());
    fs::rename(
        memory.directory(),
        memory.directory().with_file_name("moved"),
    )
    .unwrap();
    symlink(outside.path(), memory.directory()).unwrap();
    assert!(memory
        .put("other.md", &note("Other", "bad"), ExpectedVersion::Absent)
        .is_err());
    assert_eq!(fs::read(outside.path().join("one.md")).unwrap(), bytes);
    assert!(!outside.path().join("other.md").exists());
}

#[cfg(unix)]
#[test]
fn symlink_in_state_or_project_parent_is_rejected_before_creation() {
    use std::os::unix::fs::symlink;
    for component in ["state", "projects", "trash"] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("data/memory")).unwrap();
        symlink(
            outside.path(),
            root.path().join("data/memory").join(component),
        )
        .unwrap();
        assert!(FileMemoryStore::open(
            root.path(),
            &MemoryScope::private("fixture-owner").unwrap()
        )
        .is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[cfg(windows)]
#[test]
fn windows_reparse_points_are_rejected_and_pinned_directories_cannot_be_swapped() {
    use std::os::windows::fs::symlink_file;
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let outside = root.path().join("outside.md");
    let bytes = note("Outside", "keep");
    fs::write(&outside, &bytes).unwrap();
    // Requires Windows Developer Mode or symlink privilege; skip when the
    // host cannot create the reparse point at all.
    if symlink_file(&outside, memory.directory().join("one.md")).is_err() {
        eprintln!("skipping: symlink privilege unavailable");
        return;
    }
    assert!(memory.read("one.md").is_err());
    assert!(memory
        .delete("one.md", ExpectedVersion::Hash(hash(&bytes)))
        .is_err());
    assert!(memory
        .put("one.md", &note("Bad", "bad"), ExpectedVersion::Absent)
        .is_err());
    assert!(fs::rename(
        memory.directory(),
        memory.directory().with_file_name("moved")
    )
    .is_err());
    assert_eq!(fs::read(outside).unwrap(), bytes);
}

#[cfg(windows)]
#[test]
fn windows_directory_reparse_point_is_not_a_managed_project_root() {
    use std::os::windows::fs::symlink_dir;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("data/memory")).unwrap();
    if symlink_dir(outside.path(), root.path().join("data/memory/projects")).is_err() {
        eprintln!("skipping: directory symlink privilege unavailable");
        return;
    }
    assert!(
        FileMemoryStore::open(root.path(), &MemoryScope::private("fixture-owner").unwrap())
            .is_err()
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn windows_junction_is_rejected_before_any_external_file_is_created() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("data/memory")).unwrap();
    let status = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(root.path().join("data/memory/projects"))
        .arg(outside.path())
        .status()
        .unwrap();
    if !status.success() {
        eprintln!("skipping: junction privilege unavailable");
        return;
    }
    assert!(
        FileMemoryStore::open(root.path(), &MemoryScope::private("fixture-owner").unwrap())
            .is_err()
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn two_processes_racing_one_hash_yield_one_success_and_one_conflict() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let original = memory
        .put("one.md", &note("One", "original"), ExpectedVersion::Absent)
        .unwrap();
    let mut a = child(root.path(), "cas")
        .env("DCL_MEMORY_TEST_ID", "a")
        .spawn_owned()
        .unwrap();
    let mut b = child(root.path(), "cas")
        .env("DCL_MEMORY_TEST_ID", "b")
        .spawn_owned()
        .unwrap();
    wait_file(&root.path().join("ready-a"));
    wait_file(&root.path().join("ready-b"));
    assert_eq!(
        fs::read_to_string(root.path().join("ready-a")).unwrap(),
        original.hash
    );
    assert_eq!(
        fs::read_to_string(root.path().join("ready-b")).unwrap(),
        original.hash
    );
    fs::write(root.path().join("go"), b"go").unwrap();
    assert!(bounded_wait(&mut a).success());
    assert!(bounded_wait(&mut b).success());
    let mut results = [
        fs::read_to_string(root.path().join("result-a")).unwrap(),
        fs::read_to_string(root.path().join("result-b")).unwrap(),
    ];
    results.sort();
    assert_eq!(results, ["conflict", "success"]);
    let snapshot = memory.snapshot().unwrap();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.generation, 2);
}

#[test]
fn cross_process_lock_times_out_and_cancellation_does_not_write() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let mut holder = child(root.path(), "lock").spawn_owned().unwrap();
    wait_file(&root.path().join("lock-ready"));
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(40),
        ..Default::default()
    };
    assert!(matches!(
        FileMemoryStore::open_with_options(
            root.path(),
            &MemoryScope::private("fixture-owner").unwrap(),
            options
        ),
        Err(MemoryFileError::Busy)
    ));
    fs::write(root.path().join("release"), b"release").unwrap();
    assert!(bounded_wait(&mut holder).success());
    let cancelled = Arc::new(AtomicBool::new(false));
    let memory2 = FileMemoryStore::open_with_options(
        root.path(),
        &MemoryScope::private("fixture-owner").unwrap(),
        StoreOptions {
            cancelled: cancelled.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    cancelled.store(true, Ordering::Release);
    assert!(matches!(
        memory2.put("one.md", &note("One", "fact"), ExpectedVersion::Absent),
        Err(MemoryFileError::Cancelled)
    ));
    assert!(memory.read("one.md").unwrap().is_none());
}

#[test]
fn process_crashes_at_each_commit_boundary_recover_consistent_index_and_generation() {
    for phase in [
        "staged",
        "journal",
        "entry",
        "index",
        "catalog",
        "generation",
        "cleared",
    ] {
        let root = tempfile::tempdir().unwrap();
        let memory = store(root.path());
        memory
            .put(
                "one.md",
                &note("Original", "old fact"),
                ExpectedVersion::Absent,
            )
            .unwrap();
        fs::write(memory.index_path(), "marker unchanged\n").unwrap();
        drop(memory);
        let mut worker = child(
            root.path(),
            if phase == "index" {
                "crash-repair"
            } else {
                "crash"
            },
        )
        .env("DCL_MEMORY_TEST_CRASH", phase)
        .spawn_owned()
        .unwrap();
        assert_eq!(bounded_wait(&mut worker).code(), Some(87), "{phase}");
        if phase == "journal" {
            let journal = state(root.path()).join("journal.json");
            let before = fs::read(&journal).unwrap();
            let scope = MemoryScope::private("fixture-owner").unwrap();
            assert!(FileMemoryStore::project_snapshot(root.path(), &scope).is_err());
            assert_eq!(fs::read(&journal).unwrap(), before);
            assert_eq!(
                fs::read(state(root.path()).join("generation")).unwrap(),
                b"1"
            );
        }
        let recovered = store(root.path()).snapshot().unwrap();
        let expected = if matches!(phase, "staged" | "index") {
            "Original"
        } else {
            "Changed"
        };
        assert_eq!(recovered.entries[0].document.name, expected, "{phase}");
        if phase == "index" {
            assert!(recovered
                .index
                .starts_with("marker unchanged\n- [Original](one.md)"));
        } else {
            assert_eq!(recovered.index, "marker unchanged\n", "{phase}");
        }
        assert_eq!(
            recovered.generation,
            if phase == "staged" { 1 } else { 2 },
            "{phase}"
        );
        assert!(!state(root.path()).join("journal.json").exists());
        assert_eq!(
            store(root.path()).snapshot().unwrap().generation,
            recovered.generation
        );
    }
}

#[test]
fn recovery_preserves_external_conflict_and_journal_instead_of_overwriting() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    memory
        .put(
            "one.md",
            &note("Original", "old fact"),
            ExpectedVersion::Absent,
        )
        .unwrap();
    let path = memory.directory().join("one.md");
    drop(memory);
    let mut worker = child(root.path(), "crash")
        .env("DCL_MEMORY_TEST_CRASH", "entry")
        .spawn_owned()
        .unwrap();
    assert_eq!(bounded_wait(&mut worker).code(), Some(87));
    let external = note("External", "must survive");
    fs::write(&path, &external).unwrap();
    assert!(matches!(
        FileMemoryStore::open(root.path(), &MemoryScope::private("fixture-owner").unwrap()),
        Err(MemoryFileError::RecoveryConflict(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), external);
    assert!(state(root.path()).join("journal.json").exists());
}

#[test]
fn snapshot_observes_external_edits_without_advancing_generation() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let entry = memory
        .put("one.md", &note("One", "old body"), ExpectedVersion::Absent)
        .unwrap();
    let before = memory.snapshot().unwrap();
    let edited = String::from_utf8(entry.bytes)
        .unwrap()
        .replace("old body", "new body");
    fs::write(memory.directory().join("one.md"), edited).unwrap();
    let after = memory.snapshot().unwrap();
    assert_ne!(before.entries[0].hash, after.entries[0].hash);
    assert_eq!(before.index, after.index);
    assert_eq!(after.generation, before.generation);
    assert_eq!(memory.snapshot().unwrap().generation, after.generation);
}

#[test]
fn deletion_crash_recovers_without_resurrecting_the_entry() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    memory
        .put(
            "one.md",
            &note("Original", "old fact"),
            ExpectedVersion::Absent,
        )
        .unwrap();
    drop(memory);
    let mut worker = child(root.path(), "crash-delete")
        .env("DCL_MEMORY_TEST_CRASH", "entry")
        .spawn_owned()
        .unwrap();
    assert_eq!(bounded_wait(&mut worker).code(), Some(87));
    let recovered = store(root.path()).snapshot().unwrap();
    assert!(recovered.entries.is_empty());
    assert!(recovered.index.is_empty());
    assert_eq!(recovered.generation, 2);
}

#[test]
fn corrupt_journal_checksum_never_publishes_modified_payload() {
    let root = tempfile::tempdir().unwrap();
    let memory = store(root.path());
    let original = memory
        .put(
            "one.md",
            &note("Original", "old fact"),
            ExpectedVersion::Absent,
        )
        .unwrap();
    let path = memory.directory().join("one.md");
    drop(memory);
    let mut worker = child(root.path(), "crash")
        .env("DCL_MEMORY_TEST_CRASH", "journal")
        .spawn_owned()
        .unwrap();
    assert_eq!(bounded_wait(&mut worker).code(), Some(87));
    let journal_path = state(root.path()).join("journal.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    journal["payload"]["changes"][0]["after"][0] = serde_json::json!(0);
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let result =
        FileMemoryStore::open(root.path(), &MemoryScope::private("fixture-owner").unwrap());
    assert!(
        matches!(result, Err(MemoryFileError::Invalid(message)) if message.contains("checksum"))
    );
    assert_eq!(fs::read(path).unwrap(), original.bytes);
    assert!(journal_path.exists());
}

#[cfg(windows)]
#[test]
fn windows_canonical_path_case_aliases_have_one_scope() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("MemoryScope");
    fs::create_dir(&path).unwrap();
    assert_eq!(
        MemoryScope::project(&[path], true).unwrap(),
        MemoryScope::project(&[root.path().join("memoryscope")], true).unwrap()
    );
}
