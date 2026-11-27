// Included in the real-Git delivery fixture module; no production repository is used.
async fn cleanup_fixture(count: usize) -> TwoRepoDeliveryFixture {
    let fixture = TwoRepoDeliveryFixture::with_count(count);
    let plan = fixture.prepare_plan();
    fixture.deliver_all(&plan).await;
    let host = fixture.orchestrator();
    host.tick(&fixture.router).await.unwrap();
    fixture.wait_settled().await;
    fixture.final_reports(&plan, true);
    host.tick(&fixture.router).await.unwrap();
    host.tick(&fixture.router).await.unwrap();
    assert_eq!(fixture.task().status, Status::Completed);
    assert_eq!(fixture.task_completed_count(), 1);
    fixture
}

fn cleanup_events(f: &TwoRepoDeliveryFixture) -> usize {
    f.store()
        .events(&f.task_id)
        .unwrap()
        .iter()
        .filter(|e| e.event_type == EventType::CleanupCompleted)
        .count()
}

// The host retries a busy evidence lock on its next tick. Let transient
// contention clear before asserting which durable job blocks cleanup.
async fn cleanup_observation(
    f: &TwoRepoDeliveryFixture,
) -> crate::super_agent::orchestrator::TickReport {
    let host = f.orchestrator();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let report = host.tick(&f.router).await.unwrap();
        let busy = matches!(report.warnings.as_slice(), [warning] if warning.contains("is busy; retry the decision"));
        if !busy || tokio::time::Instant::now() >= deadline {
            return report;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn cleanup_pending(f: &TwoRepoDeliveryFixture, reason: &str) {
    let report = cleanup_observation(f).await;
    assert!(
        report
            .warnings
            .iter()
            .any(|message| message.contains(reason)),
        "{report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|message| message.contains(reason))
            .count(),
        1
    );
    assert_eq!(f.task().status, Status::Completed);
    assert_eq!(
        f.task().cleanup_status,
        crate::super_agent::CleanupStatus::Pending
    );
    assert_eq!(cleanup_events(f), 0);
    let saved: String = f
        .context.db.task_state(&format!("super-agent.cleanup-pending.{}", f.task_id))
        .unwrap()
        .unwrap();
    assert!(saved.contains(reason), "{saved}");
}

fn reopened_cleanup(f: &TwoRepoDeliveryFixture) -> Orchestrator {
    let context = ServiceContext::open(
        RuntimePaths::new(f.data.clone()).unwrap(),
        Arc::new(crate::services::EnvironmentCredentials),
    )
    .unwrap();
    Orchestrator::new(
        context,
        TwoRepoDeliveryFixture::config(),
        TwoRepoDeliveryFixture::provider(),
        "bypass".into(),
        Some(f.supervisor.clone()),
    )
}

#[tokio::test]
async fn cleanup_archives_before_removal_and_reopen_is_idempotent() {
    use sha2::{Digest, Sha256};
    let f = cleanup_fixture(1).await;
    let before = f.task();
    let repo = &before.repositories[0];
    let source = std::fs::read(repo.repo_root.join("base.txt")).unwrap();
    let source_head = git(&repo.repo_root, &["rev-parse", "HEAD"]);
    let target_checkout = f.checkout_target(0);
    let target_index = PathBuf::from(git(
        &target_checkout,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let index_before = std::fs::read(&target_index).unwrap();
    // Same content, changed stat cache; read-only verification must not refresh
    // the newly discovered target checkout's index.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(target_checkout.join("base.txt"), &source).unwrap();
    let host = f.orchestrator();
    let mut archived = false;
    crate::super_agent::cleanup::run(&host.launcher(), &f.task_id, &host.policy(), &mut |stage| {
        if stage == "archived" {
            archived = true;
            assert!(repo.worktree_path.is_dir());
            assert_eq!(
                git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
                f.repos[0].reviewed
            );
            let paths: Vec<_> = std::fs::read_dir(
                f.data
                    .join("tasks/super-agent")
                    .join(&f.task_id)
                    .join("artifacts"),
            )
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
            let index: Value = serde_json::from_slice(
                &std::fs::read(
                    paths
                        .iter()
                        .find(|p| p.to_string_lossy().ends_with("reports.json"))
                        .unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(index["delivered_oid"], f.repos[0].reviewed);
            assert_eq!(index["base_oid"], f.repos[0].base);
            assert!(index["reports"].as_array().unwrap().len() >= 4);
            let diff = std::fs::read(
                paths
                    .iter()
                    .find(|p| p.extension().is_some_and(|e| e == "diff"))
                    .unwrap(),
            )
            .unwrap();
            assert!(String::from_utf8_lossy(&diff).contains("+value=1"));
        }
        Ok(())
    })
    .unwrap();
    assert!(archived);
    assert!(!repo.worktree_path.exists());
    assert!(git(
        &repo.repo_root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/heads/{}", repo.task_branch)
        ]
    )
    .is_empty());
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.target_branch]),
        f.repos[0].reviewed
    );
    assert_eq!(git(&repo.repo_root, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(std::fs::read(&target_index).unwrap(), index_before);
    assert_eq!(
        git(&target_checkout, &["rev-parse", "HEAD"]),
        f.repos[0].reviewed
    );
    assert_eq!(
        std::fs::read(repo.repo_root.join("base.txt")).unwrap(),
        source
    );
    let event = f
        .store()
        .events(&f.task_id)
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == EventType::CleanupCompleted)
        .unwrap();
    for key in ["reports", "diff"] {
        let bytes = std::fs::read(event.payload[key]["path"].as_str().unwrap()).unwrap();
        assert_eq!(
            event.payload[key]["sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
    }
    assert!(f.store().locks(&f.task_id).unwrap().is_empty());
    host.tick(&f.router).await.unwrap();
    reopened_cleanup(&f).tick(&f.router).await.unwrap();
    assert_eq!(cleanup_events(&f), 1);
    assert_eq!(f.task_completed_count(), 1);
    // A delayed failure from another host cannot downgrade successful cleanup.
    f.store()
        .set_cleanup_status(&f.task_id, crate::super_agent::CleanupStatus::Pending)
        .unwrap();
    assert_eq!(f.task().repositories, before.repositories);
    assert_eq!(
        f.task().cleanup_status,
        crate::super_agent::CleanupStatus::Completed
    );
}

#[tokio::test]
async fn cleanup_dirty_untracked_and_ignored_content_is_preserved() {
    for mode in ["dirty", "untracked", "ignored"] {
        let f = cleanup_fixture(1).await;
        let repo = f.task().repositories.remove(0);
        let path = repo.worktree_path.join(if mode == "dirty" {
            "reviewed.txt"
        } else {
            "user.txt"
        });
        if mode == "ignored" {
            std::fs::write(repo.git_common_dir.join("info/exclude"), b"user.txt\n").unwrap();
        }
        std::fs::write(&path, b"user data\n").unwrap();
        cleanup_pending(&f, "dirty, untracked or ignored").await;
        assert_eq!(std::fs::read(&path).unwrap(), b"user data\n");
        assert_eq!(
            git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
            f.repos[0].reviewed
        );
    }
}

#[tokio::test]
async fn cleanup_external_branch_advance_is_preserved() {
    let f = cleanup_fixture(1).await;
    let repo = f.task().repositories.remove(0);
    std::fs::write(repo.worktree_path.join("user.txt"), b"user commit").unwrap();
    git(&repo.worktree_path, &["add", "."]);
    git(&repo.worktree_path, &["commit", "-qm", "external advance"]);
    let moved = git(&repo.worktree_path, &["rev-parse", "HEAD"]);
    cleanup_pending(&f, "HEAD moved").await;
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
        moved
    );
    assert_eq!(
        std::fs::read(repo.worktree_path.join("user.txt")).unwrap(),
        b"user commit"
    );
}

#[tokio::test]
async fn cleanup_wrong_path_branch_and_foreign_worktree_are_refused() {
    for mode in ["source", "branch", "foreign", "common"] {
        let f = cleanup_fixture(1).await;
        let original = f.task().repositories.remove(0);
        let mut repo = original.clone();
        match mode {
            "source" => repo.worktree_path = repo.repo_root.clone(),
            "branch" => repo.task_branch = repo.target_branch.clone(),
            "common" => repo.git_common_dir = f._temp.path().to_path_buf(),
            _ => {
                git(&repo.worktree_path, &["switch", "-qc", "user-branch"]);
            }
        }
        f.store().save_repo(&repo).unwrap();
        cleanup_pending(
            &f,
            match mode {
                "source" => "path is not owned",
                "branch" => "branch is not owned",
                "common" => "identity mismatch",
                _ => "registration/branch",
            },
        )
        .await;
        assert!(original.worktree_path.is_dir());
        assert_eq!(
            std::fs::read(original.worktree_path.join("reviewed.txt")).unwrap(),
            b"value=1\n"
        );
        assert_eq!(
            git(&original.repo_root, &["rev-parse", &original.task_branch]),
            f.repos[0].reviewed
        );
    }
}

#[tokio::test]
async fn cleanup_artifact_write_failure_deletes_nothing() {
    let f = cleanup_fixture(1).await;
    let repo = f.task().repositories.remove(0);
    let artifacts = f
        .data
        .join("tasks/super-agent")
        .join(&f.task_id)
        .join("artifacts");
    std::fs::create_dir_all(artifacts.parent().unwrap()).unwrap();
    std::fs::write(&artifacts, b"fixture obstruction").unwrap();
    cleanup_pending(&f, "File exists").await;
    assert!(repo.worktree_path.is_dir());
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
        f.repos[0].reviewed
    );
    assert_eq!(std::fs::read(&artifacts).unwrap(), b"fixture obstruction");
}

#[tokio::test]
async fn cleanup_reopen_retries_only_branch_after_worktree_success() {
    let f = cleanup_fixture(1).await;
    let repo = f.task().repositories.remove(0);
    let host = f.orchestrator();
    let branch_lock = repo
        .git_common_dir
        .join("refs/heads")
        .join(format!("{}.lock", repo.task_branch));
    let error = crate::super_agent::cleanup::run(
        &host.launcher(),
        &f.task_id,
        &host.policy(),
        &mut |stage| {
            if stage == "after-worktree-remove" {
                std::fs::write(&branch_lock, b"fixture lock")?;
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("lock"), "{error:#}");
    assert!(!repo.worktree_path.exists());
    cleanup_pending(&f, "lock").await;
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
        f.repos[0].reviewed
    );
    let artifacts = f
        .data
        .join("tasks/super-agent")
        .join(&f.task_id)
        .join("artifacts");
    let snapshot = || {
        let mut files: Vec<_> = std::fs::read_dir(&artifacts)
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                (path.clone(), std::fs::read(path).unwrap())
            })
            .collect();
        files.sort();
        files
    };
    let before = snapshot();
    std::fs::remove_file(branch_lock).unwrap(); // only this test's obstruction
    let reopened = reopened_cleanup(&f);
    crate::super_agent::cleanup::run(
        &reopened.launcher(),
        &f.task_id,
        &reopened.policy(),
        &mut |stage| {
            assert_ne!(
                stage, "before-worktree-remove",
                "removed worktree must not be deleted twice"
            );
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(snapshot(), before);
    assert_eq!(cleanup_events(&f), 1);
    assert_eq!(f.task().status, Status::Completed);
}

#[tokio::test]
async fn cleanup_crash_after_git_success_uses_durable_intent() {
    for boundary in ["after-worktree-remove", "after-branch-delete"] {
        let f = cleanup_fixture(1).await;
        let host = f.orchestrator();
        let error = crate::super_agent::cleanup::run(
            &host.launcher(),
            &f.task_id,
            &host.policy(),
            &mut |stage| {
                anyhow::ensure!(stage != boundary, "injected host crash");
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected host crash"));
        let report = reopened_cleanup(&f).tick(&f.router).await.unwrap();
        assert!(report.warnings.is_empty(), "{report:?}");
        assert_eq!(cleanup_events(&f), 1);
    }
}

#[tokio::test]
async fn cleanup_missing_assets_without_intent_and_recreated_assets_are_refused() {
    for recreated in [false, true] {
        let f = cleanup_fixture(1).await;
        let repo = f.task().repositories.remove(0);
        if recreated {
            let host = f.orchestrator();
            crate::super_agent::cleanup::run(
                &host.launcher(),
                &f.task_id,
                &host.policy(),
                &mut |stage| {
                    anyhow::ensure!(
                        stage != "before-branch-delete",
                        "stop after worktree receipt"
                    );
                    Ok(())
                },
            )
            .unwrap_err();
            std::fs::create_dir(&repo.worktree_path).unwrap();
            std::fs::write(repo.worktree_path.join("user.txt"), b"replacement").unwrap();
            cleanup_pending(&f, "recreated").await;
            assert_eq!(
                std::fs::read(repo.worktree_path.join("user.txt")).unwrap(),
                b"replacement"
            );
        } else {
            git(
                &repo.repo_root,
                &["worktree", "remove", repo.worktree_path.to_str().unwrap()],
            );
            cleanup_pending(&f, "No such file").await;
        }
        assert_eq!(
            git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
            f.repos[0].reviewed
        );
    }
}

#[tokio::test]
async fn cleanup_corrupt_archive_blocks_remaining_deletion() {
    let f = cleanup_fixture(1).await;
    let host = f.orchestrator();
    crate::super_agent::cleanup::run(&host.launcher(), &f.task_id, &host.policy(), &mut |stage| {
        anyhow::ensure!(stage != "before-branch-delete", "stop");
        Ok(())
    })
    .unwrap_err();
    let artifacts = f
        .data
        .join("tasks/super-agent")
        .join(&f.task_id)
        .join("artifacts");
    let diff = std::fs::read_dir(artifacts)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "diff"))
        .unwrap();
    std::fs::write(diff, b"tampered").unwrap();
    cleanup_pending(&f, "hash mismatch").await;
    let repo = f.task().repositories.remove(0);
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
        f.repos[0].reviewed
    );
}

#[tokio::test]
async fn cleanup_preserves_other_operation_lock() {
    let f = cleanup_fixture(1).await;
    let plan = f.store().delivery_plans(&f.task_id).unwrap().remove(0);
    let target = &plan.repositories[0];
    f.store()
        .unlock(&target.lock_key, &f.task_id, &target.lock_operation_id)
        .unwrap();
    f.store()
        .try_lock("foreign-cleanup-test", &target.lock_key, "another-task")
        .unwrap();
    let report = f.orchestrator().tick(&f.router).await.unwrap();
    assert!(report.warnings.is_empty(), "{report:?}");
    assert_eq!(cleanup_events(&f), 1);
    assert_eq!(
        f.store().lock_holder(&target.lock_key).unwrap(),
        Some(("another-task".into(), "foreign-cleanup-test".into()))
    );
}

#[tokio::test]
async fn cleanup_multi_repository_remains_pending() {
    let f = cleanup_fixture(2).await;
    cleanup_pending(&f, "multi-repository/non-Git").await;
    assert!(f
        .orchestrator()
        .tick(&f.router)
        .await
        .unwrap()
        .warnings
        .is_empty());
    assert!(reopened_cleanup(&f)
        .tick(&f.router)
        .await
        .unwrap()
        .warnings
        .is_empty());
    assert_eq!(f.task().status, Status::Completed);
    assert_eq!(cleanup_events(&f), 0);
    for repo in &f.task().repositories {
        assert!(repo.worktree_path.is_dir());
    }
}

#[tokio::test]
async fn cleanup_missing_member_state_and_missing_report_preserve_assets() {
    for missing in ["state", "report"] {
        let f = cleanup_fixture(1).await;
        if missing == "state" {
            let marker = f.final_completion_marker();
            std::fs::remove_file(
                f.job_directory(marker["job_id"].as_str().unwrap())
                    .join("state.json"),
            )
            .unwrap();
        } else {
            f.context
                .db
                .fixture_directory(|conn| {
                    conn.execute(
                        "DELETE FROM sa_reports WHERE task_id=?1 AND kind='HARNESS'",
                        [&f.task_id],
                    )
                })
                .unwrap()
                .unwrap();
        }
        cleanup_pending(
            &f,
            if missing == "state" {
                "cannot prove member stopped"
            } else {
                "completion gate"
            },
        )
        .await;
        let repo = f.task().repositories.remove(0);
        assert!(repo.worktree_path.is_dir());
        assert_eq!(
            git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
            f.repos[0].reviewed
        );
    }
}

#[tokio::test]
async fn cleanup_active_and_unknown_descendant_jobs_preserve_worktree() {
    use crate::native_engine::child_session::ChildOwnership;
    for status in ["running", "unknown", "missing"] {
        let f = cleanup_fixture(1).await;
        let parent = f.task().coder.session_id.unwrap();
        let child = SessionStore::new(f.context.db.clone())
            .create(&f.repos[0].worktree, "router-model", "fixture")
            .unwrap();
        let job_id = uuid::Uuid::new_v4().to_string();
        let owner = ChildOwnership {
            parent_session_id: parent.clone(),
            agent_type: "general".into(),
            description: "descendant fixture".into(),
            mode: "background".into(),
            status: status.into(),
            job_id: Some(job_id.clone()),
            created_at: "fixture".into(),
            updated_at: "fixture".into(),
            last_error: None,
        };
        use crate::db::session_store::WorkerDirectory as _;
        f.context.db.save_child_owner(&f.context.db.worker_pair(&parent, &child).unwrap(), &owner)
            .unwrap();
        if status != "missing" {
            let directory = f.data.join("tasks/background").join(&job_id);
            std::fs::create_dir(&directory).unwrap();
            let job = crate::services::background_sessions::Job {
                id: job_id.clone(),
                foreground: false,
                parent_session_id: parent,
                session_id: child,
                title: "fixture".into(),
                status: status.into(),
                supervisor_pid: None,
                child_pid: None,
                started_ms: 0,
                heartbeat_ms: 0,
                finished_ms: None,
                exit_code: None,
                error: None,
                launched: false,
            };
            std::fs::write(
                directory.join("state.json"),
                serde_json::to_vec(&job).unwrap(),
            )
            .unwrap();
        }
        cleanup_pending(&f, &job_id).await;
        assert!(f.repos[0].worktree.is_dir());
    }
}

#[tokio::test]
async fn cleanup_active_child_task_and_non_git_scope_stay_pending() {
    // The completed parent is real; only the post-completion out-of-scope
    // relationship/target is injected, without inventing successful evidence.
    for scenario in ["child", "non-git", "mixed-git-and-non-git"] {
        let f = cleanup_fixture(1).await;
        if scenario == "child" {
            let child = f
                .store()
                .create_task(&f.router, f.task().spec, "child-task")
                .unwrap();
            f.context
                .db
                .fixture_directory(|conn| {
                    conn.execute(
                        "UPDATE sa_tasks SET parent_task_id=?1 WHERE task_id=?2",
                        [&f.task_id, &child.task_id],
                    )
                })
                .unwrap()
                .unwrap();
            // Call only cleanup: advancing this intentionally incomplete child
            // would launch unrelated fixture work in the same tick.
            let error = crate::super_agent::cleanup::cleanup_completed(
                &f.orchestrator().launcher(),
                &f.task_id,
                &f.orchestrator().policy(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("Child task"), "{error:#}");
        } else {
            let plain = f.data.join("plain-target");
            let snapshot = f.data.join("plain-snapshot");
            std::fs::create_dir_all(&plain).unwrap();
            std::fs::create_dir_all(&snapshot).unwrap();
            std::fs::write(plain.join("user.txt"), b"user content").unwrap();
            std::fs::write(snapshot.join("user.txt"), b"saved snapshot").unwrap();
            let lock_key = format!("non-git:{}", plain.display());
            f.store()
                .try_lock("plain-scope-operation", &lock_key, &f.task_id)
                .unwrap();
            f.store()
                .save_non_git_target(&crate::super_agent::NonGitTarget {
                    task_id: f.task_id.clone(),
                    path: plain.clone(),
                    snapshot_dir: snapshot.clone(),
                    snapshot_manifest_json: "{}".into(),
                    diff_ref: None,
                    status: crate::super_agent::NonGitStatus::Completed,
                    lock_key: lock_key.clone(),
                    submission: None,
                })
                .unwrap();
            if scenario == "non-git" {
                f.context
                    .db
                    .fixture_directory(|conn| {
                        conn.execute("DELETE FROM sa_task_repos WHERE task_id=?1", [&f.task_id])
                    })
                    .unwrap()
                    .unwrap();
            }
            cleanup_pending(&f, "multi-repository/non-Git").await;
            assert!(f
                .orchestrator()
                .tick(&f.router)
                .await
                .unwrap()
                .warnings
                .is_empty());
            assert!(reopened_cleanup(&f)
                .tick(&f.router)
                .await
                .unwrap()
                .warnings
                .is_empty());
            assert_eq!(
                std::fs::read(plain.join("user.txt")).unwrap(),
                b"user content"
            );
            assert_eq!(
                std::fs::read(snapshot.join("user.txt")).unwrap(),
                b"saved snapshot"
            );
            assert_eq!(f.store().non_git_targets(&f.task_id).unwrap().len(), 1);
            assert!(f.store().lock_holder(&lock_key).unwrap().is_some());
        }
        assert!(f.repos[0].worktree.is_dir());
        assert_eq!(cleanup_events(&f), 0);
    }
}

#[tokio::test]
async fn cleanup_index_flags_cannot_hide_user_edits() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let f = cleanup_fixture(1).await;
        let repo = f.task().repositories.remove(0);
        git(&repo.worktree_path, &["update-index", flag, "reviewed.txt"]);
        std::fs::write(repo.worktree_path.join("reviewed.txt"), b"hidden user edit").unwrap();
        cleanup_pending(&f, "cannot prove it clean").await;
        assert_eq!(
            std::fs::read(repo.worktree_path.join("reviewed.txt")).unwrap(),
            b"hidden user edit"
        );
    }
}

#[tokio::test]
async fn cleanup_archive_base_must_be_an_oid_not_a_revision_or_option() {
    for base in ["HEAD", "--output=unexpected-user-file"] {
        let f = cleanup_fixture(1).await;
        let mut repo = f.task().repositories.remove(0);
        repo.base_commit = base.into();
        f.store().save_repo(&repo).unwrap();
        cleanup_pending(&f, "Base commit is not an immutable Git OID").await;
        assert!(repo.worktree_path.is_dir());
        assert!(!repo.repo_root.join("unexpected-user-file").exists());
        assert_eq!(
            git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
            f.repos[0].reviewed
        );
    }
}

#[tokio::test]
async fn cleanup_symbolic_task_ref_cannot_delete_target_after_partial_removal() {
    let f = cleanup_fixture(1).await;
    let host = f.orchestrator();
    crate::super_agent::cleanup::run(&host.launcher(), &f.task_id, &host.policy(), &mut |stage| {
        anyhow::ensure!(
            stage != "before-branch-delete",
            "stop before branch deletion"
        );
        Ok(())
    })
    .unwrap_err();
    let repo = f.task().repositories.remove(0);
    git(
        &repo.repo_root,
        &[
            "symbolic-ref",
            &format!("refs/heads/{}", repo.task_branch),
            &format!("refs/heads/{}", repo.target_branch),
        ],
    );
    cleanup_pending(&f, "Task branch is symbolic").await;
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.target_branch]),
        f.repos[0].reviewed
    );
    assert_eq!(
        git(
            &repo.repo_root,
            &["symbolic-ref", &format!("refs/heads/{}", repo.task_branch)]
        ),
        format!("refs/heads/{}", repo.target_branch)
    );
}

#[tokio::test]
async fn cleanup_allows_target_to_advance_after_completion() {
    for checked_out in [false, true] {
        let f = cleanup_fixture(1).await;
        let before = f.task();
        let repo = &before.repositories[0];
        let checkout = f.checkout_target(0);
        std::fs::write(checkout.join("later.txt"), b"later user commit").unwrap();
        git(&checkout, &["add", "later.txt"]);
        git(
            &checkout,
            &["commit", "-qm", "advance target after delivery"],
        );
        let advanced = git(&checkout, &["rev-parse", "HEAD"]);
        if !checked_out {
            git(
                &repo.repo_root,
                &["worktree", "remove", checkout.to_str().unwrap()],
            );
        }
        let plan = f.store().delivery_plans(&f.task_id).unwrap().remove(0);
        // The completion gate stays strict; only post-completion cleanup may
        // accept descendant commits. The archive must retain the delivered OID.
        assert!(
            !crate::super_agent::delivery::verify_delivery(&before, &plan)
                .unwrap()
                .complete
        );
        let report = f.orchestrator().tick(&f.router).await.unwrap();
        assert!(report.warnings.is_empty(), "{report:?}");
        assert_eq!(cleanup_events(&f), 1);
        assert!(!repo.worktree_path.exists());
        assert!(
            crate::worktree::session::command(
                &repo.repo_root,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", repo.task_branch)
                ]
            )
            .unwrap()
            .status
            .code()
                == Some(1)
        );
        assert_eq!(
            git(&repo.repo_root, &["rev-parse", &repo.target_branch]),
            advanced
        );
        assert_eq!(f.task().repositories, before.repositories);
        let event = f
            .store()
            .events(&f.task_id)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == EventType::CleanupCompleted)
            .unwrap();
        let index: Value = serde_json::from_slice(
            &std::fs::read(event.payload["reports"]["path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            index["delivered_oid"],
            repo.integrated_commit.as_deref().unwrap()
        );
        if checked_out {
            assert_eq!(
                std::fs::read(checkout.join("later.txt")).unwrap(),
                b"later user commit"
            );
        }
        reopened_cleanup(&f).tick(&f.router).await.unwrap();
        assert_eq!(cleanup_events(&f), 1);
    }
}

#[tokio::test]
async fn cleanup_rejects_target_that_no_longer_contains_delivery() {
    let f = cleanup_fixture(1).await;
    let repo = f.task().repositories.remove(0);
    git(
        &repo.repo_root,
        &[
            "update-ref",
            &format!("refs/heads/{}", repo.target_branch),
            &repo.base_commit,
        ],
    );
    cleanup_pending(&f, "Delivered targets changed").await;
    assert!(repo.worktree_path.is_dir());
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.task_branch]),
        repo.integrated_commit.unwrap()
    );
    assert_eq!(
        git(&repo.repo_root, &["rev-parse", &repo.target_branch]),
        repo.base_commit
    );
}

#[tokio::test]
async fn cleanup_unknown_jobs_are_filtered_by_readable_identity() {
    for relation in [
        "unrelated",
        "session",
        "parent",
        "unreadable",
        "conflicting",
    ] {
        for damage in ["unknown", "lifecycle", "publication"] {
            let f = cleanup_fixture(1).await;
            let id = uuid::Uuid::new_v4().to_string();
            let directory = f.data.join("tasks/background").join(&id);
            std::fs::create_dir(&directory).unwrap();
            let coder = f.task().coder.session_id.unwrap();
            let job = crate::services::background_sessions::Job {
                id: if relation == "conflicting" {
                    "another-job".into()
                } else {
                    id.clone()
                },
                foreground: false,
                session_id: if relation == "session" {
                    coder.clone()
                } else {
                    "unrelated-session".into()
                },
                parent_session_id: if relation == "parent" {
                    coder
                } else {
                    "unrelated-parent".into()
                },
                title: "unknown identity fixture".into(),
                status: "unknown".into(),
                supervisor_pid: None,
                child_pid: None,
                started_ms: 0,
                heartbeat_ms: 0,
                finished_ms: None,
                exit_code: None,
                error: None,
                launched: false,
            };
            let mut value = serde_json::to_value(job).unwrap();
            if damage == "lifecycle" {
                value["status"] = json!({"broken": true});
            }
            if damage == "publication" {
                std::fs::write(directory.join("worker.claim"), [0xff]).unwrap();
            }
            let bytes = if relation == "unreadable" {
                b"{not json".to_vec()
            } else {
                serde_json::to_vec(&value).unwrap()
            };
            std::fs::write(directory.join("state.json"), &bytes).unwrap();
            if relation == "unrelated" {
                let report = cleanup_observation(&f).await;
                assert!(report.warnings.is_empty(), "{report:?}");
                assert_eq!(cleanup_events(&f), 1);
                assert!(!f.repos[0].worktree.exists());
            } else {
                cleanup_pending(&f, &id).await;
                assert!(f.repos[0].worktree.is_dir());
                let reason: String = f
                    .context.db.task_state(&format!("super-agent.cleanup-pending.{}", f.task_id))
                    .unwrap()
                    .unwrap();
                if relation == "unreadable" {
                    assert!(reason.contains("cannot read identity"), "{reason}");
                }
            }
            assert_eq!(std::fs::read(directory.join("state.json")).unwrap(), bytes);
        }
    }
}

#[tokio::test]
async fn cleanup_pending_warning_is_deduplicated_and_changes_are_reported() {
    let f = cleanup_fixture(1).await;
    let repo = f.task().repositories.remove(0);
    std::fs::write(repo.worktree_path.join("user.txt"), b"keep me").unwrap();
    cleanup_pending(&f, "dirty, untracked or ignored").await;
    assert!(f
        .orchestrator()
        .tick(&f.router)
        .await
        .unwrap()
        .warnings
        .is_empty());
    assert!(reopened_cleanup(&f)
        .tick(&f.router)
        .await
        .unwrap()
        .warnings
        .is_empty());
    git(&repo.worktree_path, &["add", "user.txt"]);
    git(
        &repo.worktree_path,
        &["commit", "-qm", "external task advance"],
    );
    cleanup_pending(&f, "HEAD moved").await;
    assert!(f
        .orchestrator()
        .tick(&f.router)
        .await
        .unwrap()
        .warnings
        .is_empty());
    assert_eq!(f.task().status, Status::Completed);
    assert_eq!(cleanup_events(&f), 0);
    assert_eq!(
        std::fs::read(repo.worktree_path.join("user.txt")).unwrap(),
        b"keep me"
    );
}

#[tokio::test]
async fn cleanup_finishes_after_retirement_of_a_previous_coder() {
    let f = cleanup_fixture(1).await;
    let current = f.task().coder.session_id.unwrap();
    let former = uuid::Uuid::new_v4().to_string();
    let old = f.context.db.create(&former, Default::default()).unwrap();
    f.store()
        .set_sessions(&f.task_id, Some(&former), None)
        .unwrap();
    f.store()
        .replace_coder_session(&f.task_id, &current)
        .unwrap();
    let host = f.orchestrator();
    let shard = f.data.join("data/sessions").join(&former).join("session.db");
    let backup = shard.with_extension("fixture");
    std::fs::rename(&shard, &backup).unwrap();
    assert!(crate::super_agent::cleanup::cleanup_completed(
        &host.launcher(),
        &f.task_id,
        &host.policy()
    )
    .is_err());
    assert!(f.task().repositories[0].worktree_path.is_dir());
    std::fs::rename(backup, shard).unwrap();
    use crate::db::session_store::WorkerDirectory;
    let child = uuid::Uuid::new_v4().to_string();
    f.context.db.create(&child, Default::default()).unwrap();
    let job_id = uuid::Uuid::new_v4().to_string();
    let owner = crate::native_engine::child_session::ChildOwnership {
        parent_session_id: former.clone(),
        agent_type: "general".into(),
        description: "historical child".into(),
        mode: "background".into(),
        status: "completed".into(),
        job_id: Some(job_id.clone()),
        created_at: "fixture".into(),
        updated_at: "fixture".into(),
        last_error: None,
    };
    f.context
        .db
        .save_child_owner(&f.context.db.worker_pair(&former, &child).unwrap(), &owner)
        .unwrap();
    let job_dir = f.data.join("tasks/background").join(&job_id);
    std::fs::create_dir(&job_dir).unwrap();
    let state = json!({"id":job_id,"session_id":child,"parent_session_id":former,
        "title":"descendant","status":"completed","started_ms":1,"heartbeat_ms":1,"finished_ms":2,"exit_code":0});
    std::fs::write(job_dir.join("state.json"), state.to_string()).unwrap();
    old.delete().unwrap();
    let replacement = f.context.db.create(&former, Default::default()).unwrap();
    let _replacement_lease = replacement.acquire_execution().unwrap();
    // A live job of the replacement is unrelated. An unfinished job of the
    // surviving child still blocks this cleanup even after its parent retires.
    let other_job = uuid::Uuid::new_v4().to_string();
    let directory = f.data.join("tasks/background").join(&other_job);
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(
        directory.join("session-owners.json"),
        json!({
            "parent":replacement.incarnation(), "execution":replacement.incarnation()
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        directory.join("state.json"),
        json!({
            "id":other_job, "session_id":former, "parent_session_id":former, "status":"running",
            "title":"unrelated", "started_ms":1, "heartbeat_ms":1
        })
        .to_string(),
    )
    .unwrap();
    let mut live_child = state.clone();
    live_child["status"] = json!("running");
    live_child["finished_ms"] = Value::Null;
    std::fs::write(job_dir.join("state.json"), live_child.to_string()).unwrap();
    let error = crate::super_agent::cleanup::cleanup_completed(
        &host.launcher(),
        &f.task_id,
        &host.policy(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains(&job_id), "{error:#}");
    assert!(f.task().repositories[0].worktree_path.is_dir());
    std::fs::write(job_dir.join("state.json"), state.to_string()).unwrap();
    crate::super_agent::cleanup::cleanup_completed(&host.launcher(), &f.task_id, &host.policy())
        .unwrap();
    assert_eq!(cleanup_events(&f), 1);
}

#[tokio::test]
async fn cleanup_checks_grandchild_after_intermediate_retirement() {
    use crate::db::session_store::WorkerDirectory;
    let f = cleanup_fixture(1).await;
    let coder = f.task().coder.session_id.unwrap();
    let middle = uuid::Uuid::new_v4().to_string();
    let leaf = uuid::Uuid::new_v4().to_string();
    let middle_db = f.context.db.create(&middle, Default::default()).unwrap();
    let leaf_db = f.context.db.create(&leaf, Default::default()).unwrap();
    let coder_db = f.context.db.open_session(&coder).unwrap();
    let mut leaf_job = None;
    for (parent, child, terminal) in [(&coder_db, &middle_db, true), (&middle_db, &leaf_db, false)]
    {
        let job = uuid::Uuid::new_v4().to_string();
        if !terminal {
            leaf_job = Some(job.clone());
        }
        let status = if terminal { "completed" } else { "running" };
        let owner = crate::native_engine::child_session::ChildOwnership {
            parent_session_id: parent.id().into(),
            agent_type: "general".into(),
            description: "owned descendant".into(),
            mode: "background".into(),
            status: status.into(),
            job_id: Some(job.clone()),
            created_at: "fixture".into(),
            updated_at: "fixture".into(),
            last_error: None,
        };
        let pair = f.context.db.worker_pair(parent.id(), child.id()).unwrap();
        f.context.db.save_child_owner(&pair, &owner).unwrap();
        let path = f.data.join("tasks/background").join(&job);
        std::fs::create_dir(&path).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let state = json!({"id":job,"session_id":child.id(),"parent_session_id":parent.id(),
            "title":"owned descendant","status":status,"started_ms":now,"heartbeat_ms":now,
            "finished_ms":terminal.then_some(now),"exit_code":terminal.then_some(0)});
        std::fs::write(path.join("state.json"), state.to_string()).unwrap();
        std::fs::write(
            path.join("session-owners.json"),
            json!({"parent":parent.incarnation(),"execution":child.incarnation()}).to_string(),
        )
        .unwrap();
    }
    let lease = leaf_db.acquire_execution().unwrap();
    let host = f.orchestrator();
    let wt = f.task().repositories[0].worktree_path.clone();
    let check = || {
        crate::super_agent::cleanup::cleanup_completed(&host.launcher(), &f.task_id, &host.policy())
    };
    let control = check();
    println!(
        "Before intermediate retirement: {control:?}, worktree exists={}",
        wt.is_dir()
    );
    assert!(control.is_err() && wt.is_dir());
    middle_db.delete().unwrap();
    let after = check();
    println!(
        "After intermediate retirement with same active grandchild: {after:?}, worktree exists={}",
        wt.is_dir()
    );
    assert!(
        after.is_err() && wt.is_dir(),
        "Cleanup must wait for the active descendant"
    );
    let path = f
        .data
        .join("tasks/background")
        .join(leaf_job.unwrap())
        .join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["status"] = json!("completed");
    state["finished_ms"] = json!(chrono::Utc::now().timestamp_millis());
    std::fs::write(path, state.to_string()).unwrap();
    assert!(
        check().is_err() && wt.is_dir(),
        "The execution lease alone must retain the worktree"
    );
    drop(lease);
    let report = cleanup_observation(&f).await;
    assert!(!wt.exists(), "{report:?}");
    assert_eq!(cleanup_events(&f), 1);
}

#[tokio::test]
async fn cleanup_excludes_foreground_workspace_admission() {
    let f = cleanup_fixture(1).await;
    let host = f.orchestrator();
    let path = f.task().repositories.remove(0).worktree_path;
    let mut active = None;
    let mut attempted = false;
    let outcome = crate::super_agent::cleanup::run(
        &host.launcher(),
        &f.task_id,
        &host.policy(),
        &mut |stage| {
            if stage == "before-worktree-remove" {
                attempted = true;
                let other = f.context.db.create_code_session(
                    &path,
                    "model",
                    "provider",
                    &uuid::Uuid::new_v4().to_string(),
                )?;
                assert_eq!(other.workspace()?.canonicalize()?, path.canonicalize()?);
                active = Some(other.acquire_execution()?);
            }
            Ok(())
        },
    );
    assert!(attempted);
    assert!(
        path.is_dir(),
        "active foreground session lost its workspace"
    );
    assert!(outcome.is_err());
    assert!(active.is_none());

    let other = f
        .context
        .db
        .create_code_session(
            &path,
            "model",
            "provider",
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
    let lease = other.acquire_execution().unwrap();
    let outcome =
        crate::super_agent::cleanup::run(&host.launcher(), &f.task_id, &host.policy(), &mut |_| {
            Ok(())
        });
    assert!(outcome.is_err());
    assert!(path.is_dir(), "cleanup removed the leased workspace");
    drop(lease);
    crate::super_agent::cleanup::run(
        &host.launcher(),
        &f.task_id,
        &host.policy(),
        &mut |_| Ok(()),
    )
    .unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn cleanup_releases_fences_when_participant_is_busy() -> anyhow::Result<()> {
    use std::{fs::File, sync::mpsc, time::Duration};
    let f = cleanup_fixture(1).await;
    let wt = f.task().repositories.remove(0).worktree_path;
    let first = "00000000-0000-4000-8000-000000000001";
    let last = "ffffffff-ffff-4fff-bfff-ffffffffffff";
    f.context.db.create_code_session(&wt, "m", "p", first)?;
    let outside = f.checkout_target(0);
    let executing = f.context.db.create_code_session(&outside, "m", "p", last)?;
    let lease = executing.acquire_execution()?;

    // Pause a two-key publisher after taking its first key. The running second
    // session must still be able to publish work while the first key is held.
    let key = File::options()
        .read(true)
        .write(true)
        .open(f.data.join("state/locks/session").join(first))?;
    key.try_lock()?;
    let host = f.orchestrator();
    let task = f.task_id.clone();
    let (tx, rx) = mpsc::channel();
    let cleaner = std::thread::spawn(move || {
        let result =
            crate::super_agent::cleanup::run(&host.launcher(), &task, &host.policy(), &mut |_| {
                Ok(())
            });
        let _ = tx.send(result);
    });
    let result = rx.recv_timeout(Duration::from_secs(3));
    // Break the old wait cycle even on failure, before joining the worker.
    drop(key);
    cleaner.join().unwrap();
    let error = result
        .expect("cleanup waited for a busy lifecycle lock")
        .unwrap_err();
    assert!(format!("{error:#}").contains("busy"), "{error:#}");
    assert!(wt.join("base.txt").exists());
    drop(crate::services::background_sessions::job_publication_fence(
        &f.data, true,
    )?);
    drop(f.context.db.hold_worktree_removal(&wt)?);

    let archives = tempfile::tempdir()?;
    for id in [first, last] {
        std::fs::write(
            archives.path().join(format!("{id}.json")),
            serde_json::to_vec(
                &json!({"id":id,"name":"Import","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","messages":[]}),
            )?,
        )?;
    }
    let db = f.context.db.clone();
    let (tx, rx) = mpsc::channel();
    let importer = std::thread::spawn(move || {
        let _ = tx.send(db.import_json(archives.path(), &outside, false));
    });
    let result = rx.recv_timeout(Duration::from_secs(4));
    drop(lease);
    importer.join().unwrap();
    let error = result
        .expect("batch import waited indefinitely")
        .unwrap_err();
    assert!(format!("{error:#}").contains("busy"), "{error:#}");
    let host = f.orchestrator();
    crate::super_agent::cleanup::run(
        &host.launcher(),
        &f.task_id,
        &host.policy(),
        &mut |_| Ok(()),
    )?;
    assert!(!wt.exists());
    Ok(())
}
#[tokio::test]
async fn cleanup_preserves_all_engine_paths_until_lease_ends() -> anyhow::Result<()> {
    use crate::services::session_workspace::change;
    for (secondary, before) in [(false, false), (true, true), (true, false)] {
        let f = cleanup_fixture(1).await;
        let wt = f.task().repositories[0].worktree_path.clone();
        let a = f.checkout_target(0);
        let db = &f.context.db;
        let s =
            db.create_code_session(&a, "model", "provider", &uuid::Uuid::new_v4().to_string())?;
        if before {
            change(db.as_ref(), s.id(), &a, wt.to_str().unwrap(), true)?;
        }
        let lease = s.acquire_execution()?;
        if !before {
            change(db.as_ref(), s.id(), &a, wt.to_str().unwrap(), secondary)?;
        }
        let (cwd, project, _) = db.engine_workspace(&f.data.join("data/workspaces"), s.id(), None)?;
        let project = project.unwrap();
        assert!(if secondary {
            project
                .folders
                .iter()
                .skip(1)
                .any(|f| std::path::Path::new(&f.path) == wt)
        } else {
            cwd == wt
        });
        let path = wt.join("base.txt");
        let file = std::fs::File::open(&path)?;
        change(db.as_ref(), s.id(), &wt, a.to_str().unwrap(), false)?;
        let host = f.orchestrator();
        let outcome = crate::super_agent::cleanup::run(
            &host.launcher(),
            &f.task_id,
            &host.policy(),
            &mut |_| Ok(()),
        );
        let error = outcome.unwrap_err();
        assert!(
            format!("{error:#}").contains("Worktree has active workspace users"),
            "{error:#}"
        );
        assert!(path.exists() && file.metadata()?.len() > 0);
        drop((file, lease));
        crate::super_agent::cleanup::run(
            &host.launcher(),
            &f.task_id,
            &host.policy(),
            &mut |_| Ok(()),
        )?;
        assert!(!path.exists());
    }
    Ok(())
}

#[tokio::test]
async fn cleanup_excludes_import_publication() -> anyhow::Result<()> {
    let f = cleanup_fixture(1).await;
    let wt = f.task().repositories[0].worktree_path.clone();
    let host = f.orchestrator();
    let archive = f.data.join("import.json");
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        &archive,
        serde_json::to_vec(
            &serde_json::json!({"id":id,"name":"Imported","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","messages":[]}),
        )?,
    )?;
    let mut attempted = false;
    crate::super_agent::cleanup::run(&host.launcher(), &f.task_id, &host.policy(), &mut |stage| {
        if stage == "before-worktree-remove" {
            let err = f.context.db.import_json(&archive, &wt, false).unwrap_err();
            assert!(
                format!("{err:#}").contains("Worktree removal in progress"),
                "{err:#}"
            );
            assert!(!f
                .context
                .db
                .global()
                .list_sessions(Default::default())?
                .iter()
                .any(|row| row.conversation.id == id));
            attempted = true;
        }
        Ok(())
    })?;
    assert!(attempted && !wt.exists());
    Ok(())
}
