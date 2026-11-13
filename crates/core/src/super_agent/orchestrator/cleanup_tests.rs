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

async fn cleanup_pending(f: &TwoRepoDeliveryFixture, reason: &str) {
    let report = f.orchestrator().tick(&f.router).await.unwrap();
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
        .context
        .setting(&format!("super-agent.cleanup-pending.{}", f.task_id))
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
                    .join("super-agent")
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
        .join("super-agent")
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
        .join("super-agent")
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
        .join("super-agent")
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
                .with_conn(|conn| {
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
        f.context
            .save_setting(&format!("child-owner.{child}"), &owner)
            .unwrap();
        if status != "missing" {
            let directory = f.data.join("background-tasks").join(&job_id);
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
                .with_conn(|conn| {
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
                    .with_conn(|conn| {
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
            let directory = f.data.join("background-tasks").join(&id);
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
                let report = f.orchestrator().tick(&f.router).await.unwrap();
                assert!(report.warnings.is_empty(), "{report:?}");
                assert_eq!(cleanup_events(&f), 1);
                assert!(!f.repos[0].worktree.exists());
            } else {
                cleanup_pending(&f, &id).await;
                assert!(f.repos[0].worktree.is_dir());
                let reason: String = f
                    .context
                    .setting(&format!("super-agent.cleanup-pending.{}", f.task_id))
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
