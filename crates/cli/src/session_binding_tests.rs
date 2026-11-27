#[cfg(test)]
use crate::fixture_sql::FixtureSql;
use super::{Cli, RuntimePaths, Selection, Session};
use serde_json::json;
use std::ffi::OsString;

fn selection(limit: u32) -> Selection {
    Selection {
        provider_id: "test-provider".into(),
        model: "test-model".into(),
        providers: vec![serde_json::from_value(json!({
            "id":"test-provider","name":"Display name","enabled":true,
            "api_format":"openai","api_key":"fixture","base_url":"http://127.0.0.1:1",
            "models":[{"id":"test-model","name":"Test","enabled":true,
                "supports_web_search":false,"max_tokens":limit}]
        }))
        .unwrap()],
    }
}

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_with_mode_source(std::iter::once("dcl").chain(args.iter().copied())).unwrap()
}

#[tokio::test]
async fn restart_handoff_and_plain_observation_never_publish() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let startup = cli(&["--model", "test-model", "--max-tokens", "137"]);
    let owner = Session::open(&startup, &paths, selection(137), None)
        .await
        .unwrap();
    owner
        .store
        .db
        .directory_sql(|db| {
            db.execute_batch(
                "CREATE TRIGGER reject_write BEFORE UPDATE OF provider,model ON conversations
         BEGIN SELECT RAISE(ABORT,'observer attempted binding write'); END;",
            )
        })
        .unwrap()
        .unwrap();
    let observer = cli(&["--resume", &owner.id]);
    let launch = crate::restart::resume(&startup, &owner).unwrap();
    let replacement = Cli::try_parse_with_mode_source(
        std::iter::once(OsString::from("dcl")).chain(launch.arguments),
    )
    .unwrap();
    // A previous-version originator may still send explicit flags alongside its generation.
    let old_handoff = cli(&[
        "--resume",
        &owner.id,
        "--binding-generation",
        "1",
        "--model",
        "test-model",
    ]);
    for args in [observer, replacement, old_handoff] {
        assert!(!args.changes_binding());
        let session = Session::open(&args, &paths, selection(4096), Some(&owner.id))
            .await
            .unwrap();
        assert_eq!(session.binding_state().generation, 1);
        assert_eq!(
            session.launch_provider().await.unwrap().model.max_tokens,
            Some(137)
        );
    }
    assert_eq!(
        owner.store.db.observed_binding(&owner.id)
            .unwrap()
            .generation,
        1
    );
}

#[tokio::test]
async fn rejected_relaunch_keeps_only_the_winning_token_cap() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let startup = cli(&["--model", "test-model", "--max-tokens", "137"]);
    let mut owner = Session::open(&startup, &paths, selection(137), None)
        .await
        .unwrap();
    let stale = cli(&[
        "--resume",
        &owner.id,
        "--binding-generation",
        "1",
        "--max-tokens",
        "137",
    ]);
    owner.switch(&paths, selection(4096)).await.unwrap();
    let session = Session::open(&stale, &paths, selection(137), Some(&owner.id))
        .await
        .unwrap();
    let active = session.launch_provider().await.unwrap();
    assert_eq!(active.model.max_tokens, Some(4096));
    let turn = crate::background_commands::turn_arguments(
        &stale,
        &paths,
        &session,
        &session.id,
        "probe",
        crate::session::TurnOrigin::User("probe"),
        &active,
    )
    .unwrap();
    assert!(!turn.iter().any(|arg| arg.contains("137")));
    let launch = crate::restart::resume(&stale, &session).unwrap();
    let next = Cli::try_parse_with_mode_source(
        std::iter::once(OsString::from("dcl")).chain(launch.arguments),
    )
    .unwrap();
    assert_eq!(next.max_tokens, None);
    assert_eq!(next.binding_generation, Some(2));
}

#[tokio::test]
async fn rejected_clear_rolls_back_the_conversation_and_binding() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let mut session = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let original = session.id.clone();
    session
        .store
        .db
        .directory_sql(|db| {
            db.execute_batch(
                "CREATE TRIGGER reject_clear BEFORE INSERT ON cli_binding_revisions
         BEGIN SELECT RAISE(ABORT,'clear binding rejected'); END;",
            )
        })
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        assert!(
            format!("{:#}", session.clear().await.unwrap_err()).contains("clear binding rejected")
        );
        assert_eq!(session.id, original);
        for table in [
            "conversations",
            "cli_binding_generations",
            "cli_binding_revisions",
        ] {
            let count: u64 = session
                .store
                .db
                .directory_sql(|db| {
                    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                })
                .unwrap()
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
    }
}

#[tokio::test]
async fn resume_race_returns_an_error_and_keeps_the_source_handle() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let source = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let mut destination = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let observed = (*destination.binding_state()).clone();
    destination.switch(&paths, selection(4096)).await.unwrap();
    let error = source
        .prepare_resumed_binding(
            &destination.store.db.open_session(&destination.id).unwrap(),
            observed,
        )
        .unwrap_err();
    assert!(error.to_string().contains("binding changed during refresh"));
    assert_eq!(source.binding_state().generation, 1);
    assert_eq!(
        source.launch_provider().await.unwrap().model.max_tokens,
        Some(137)
    );
}

#[tokio::test]
async fn rejected_launch_drops_its_prompt_before_dispatch_in_both_modes() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let mut winner = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let mut launch = cli(&[
        "--resume",
        &winner.id,
        "--model",
        "test-model",
        "for the rejected selection",
    ]);
    launch.observed_generation = Some(winner.binding_state().generation);
    winner.switch(&paths, selection(4096)).await.unwrap();
    let loser = Session::open(&launch, &paths, selection(256), Some(&winner.id))
        .await
        .unwrap();
    for single in [false, true] {
        let mut prompt = launch.prompt.clone().unwrap();
        let admission = loser.admit_launch_prompt(&mut prompt, single);
        assert!(prompt.is_empty());
        if single {
            assert!(admission
                .unwrap_err()
                .to_string()
                .contains("prompt was not sent"));
        } else {
            admission.unwrap();
            assert_eq!(
                loser.launch_provider().await.unwrap().model.max_tokens,
                Some(4096)
            );
        }
    }
}

#[tokio::test]
async fn rejected_picker_preserves_winner_settings_and_saves_effort_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let mut winner = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let mut picker = Session::open(&cli(&[]), &paths, selection(137), Some(&winner.id))
        .await
        .unwrap();
    winner.switch(&paths, selection(4096)).await.unwrap();
    let key = "cli.session";
    let settings = r#"{"effort":"high","autocompact_percent":70,"extra":true}"#;
    winner
        .store
        .db
        .shard_sql(&winner.id, |c| {
            c.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
                (&key, settings),
            )
        })
        .unwrap()
        .unwrap();
    let value = || {
        winner
            .store
            .db
            .shard_sql(&winner.id, |c| {
                c.query_row(
                    "SELECT value FROM peripheral_settings WHERE key=?1",
                    [&key],
                    |r| r.get::<_, String>(0),
                )
            })
            .unwrap()
            .unwrap()
    };
    assert!(picker
        .switch_with_effort(&paths, Some(selection(256)), Some(Some("low")))
        .await
        .is_err());
    assert_eq!(value(), settings);
    let version = picker.binding_state().generation;
    winner
        .store
        .db
        .directory_sql(|c| {
            c.execute_batch("CREATE TRIGGER reject_effort BEFORE INSERT ON cli_binding_efforts BEGIN SELECT RAISE(ABORT,'effort write rejected'); END;")
        })
        .unwrap()
        .unwrap();
    assert!(picker
        .switch_with_effort(&paths, Some(selection(256)), Some(Some("low")))
        .await
        .is_err());
    assert_eq!(
        winner.store.db.observed_binding(&winner.id)
            .unwrap()
            .generation,
        version
    );
    assert_eq!(value(), settings);
    winner
        .store
        .db
        .directory_sql(|c| c.execute_batch("DROP TRIGGER reject_effort"))
        .unwrap()
        .unwrap();
    picker
        .switch_with_effort(&paths, Some(selection(256)), Some(Some("low")))
        .await
        .unwrap();
    // A fresh reader applies the effort committed with the binding generation.
    use deepseek_code_core::db::session_store::settings::{Effort, PREFERENCES};
    let reopened = paths.open_database().unwrap().open_session(&winner.id).unwrap();
    let preferences = reopened.read(|r| r.setting(PREFERENCES)).unwrap().unwrap();
    assert_eq!(preferences.effort, Some(Effort::Low));
    let updated: serde_json::Value = serde_json::from_str(&value()).unwrap();
    assert_eq!(
        updated,
        json!({"effort":"low","autocompact_percent":70,"extra":true})
    );
}

#[tokio::test]
async fn unstamped_worker_adopts_the_authoritative_revision_without_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let mut owner = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    owner.switch(&paths, selection(4096)).await.unwrap();
    let worker = Session::open_with_registration(
        &cli(&["--resume", &owner.id]),
        &paths,
        selection(128),
        Some(&owner.id),
        crate::local_coordination::RegistrationMode::HostOwned,
        None,
    )
    .await
    .unwrap();
    let private = crate::worker_binding::observe_worker(
        &paths,
        &owner.store.db,
        &owner.id,
        Some(&owner.binding),
        || anyhow::bail!("No launch credentials available"),
    )
    .await
    .unwrap();
    assert!(!private.observation_only);
    assert_eq!(private.active().unwrap().model.max_tokens, Some(4096));
    assert_eq!(owner.binding_state().generation, 2);
    assert_eq!(worker.binding_state().generation, 2);
    assert_eq!(
        worker.launch_provider().await.unwrap().model.max_tokens,
        Some(4096)
    );
    assert_eq!(
        owner.store.db.observed_binding(&owner.id)
            .unwrap()
            .generation,
        2
    );
}
