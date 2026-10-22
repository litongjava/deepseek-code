use super::*;

fn command(root: &Path) -> String {
    let script = root.join("capture.cjs");
    std::fs::write(&script, r#"const fs=require('fs');let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>{fs.appendFileSync('events.jsonl',JSON.stringify(JSON.parse(s))+'\n');console.log('{}')});"#).unwrap();
    format!("node \"{}\"", script.display())
}

#[tokio::test]
async fn six_events_exact_payload_matcher_once_resume_and_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = command(dir.path());
    let cases = [
        (
            HookEvent::Notification,
            json!({"notification_type":"permission_prompt","message":"Permission needed"}),
            "permission_prompt",
        ),
        (
            HookEvent::StopFailure,
            json!({"error":"server_error","error_details":"HTTP 500"}),
            "server_error",
        ),
        (HookEvent::Setup, json!({"trigger":"init"}), "init"),
        (
            HookEvent::ConfigChange,
            json!({"source":"project_settings","file_path":"project/settings.json"}),
            "project_settings",
        ),
        (
            HookEvent::InstructionsLoaded,
            json!({"file_path":"project/CLAUDE.md","memory_type":"Project","load_reason":"session_start"}),
            "session_start",
        ),
        (
            HookEvent::CwdChanged,
            json!({"old_cwd":"old","new_cwd":dir.path()}),
            "[",
        ),
    ];
    let mut definitions = json!({});
    for (event, _, matcher) in &cases {
        definitions[event.name()] = json!([{"matcher":format!("^{matcher}$"),"hooks":[{"type":"command","command":cmd,"once":true,"statusMessage":"capture lifecycle"}]}]);
        definitions[event.name()]
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks":[{"type":"command","command":cmd,"if":"Read(*)"}]}));
        if *event != HookEvent::CwdChanged {
            definitions[event.name()]
                .as_array_mut()
                .unwrap()
                .push(json!({"matcher":"^wrong$","hooks":[{"type":"command","command":cmd}]}));
        }
    }
    std::fs::write(
        dir.path().join("settings.json"),
        json!({"hooks":definitions}).to_string(),
    )
    .unwrap();
    for session in ["first", "first", "second"] {
        let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
        assert_eq!(runner.list().len(), 17);
        for (event, payload, _) in &cases {
            let outcome = runner.run(*event, session, payload.clone()).await.unwrap();
            assert!(!outcome.blocked);
        }
    }
    let rows: Vec<Value> = std::fs::read_to_string(dir.path().join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 12);
    for (index, row) in rows.iter().enumerate() {
        let (event, mut expected, _) = cases[index % 6].clone();
        let session = if index < 6 { "first" } else { "second" };
        expected["session_id"] = json!(session);
        expected["cwd"] = json!(dir.path());
        expected["hook_event_name"] = json!(event.name());
        expected["transcript_path"] =
            json!(super::super::lifecycle_events::transcript(dir.path(), session).unwrap());
        assert_eq!(*row, expected);
        assert!(Path::new(row["transcript_path"].as_str().unwrap()).is_file());
    }
}

#[test]
fn lifecycle_output_semantics_and_type_limits() {
    for event in [
        HookEvent::Notification,
        HookEvent::StopFailure,
        HookEvent::Setup,
        HookEvent::ConfigChange,
        HookEvent::InstructionsLoaded,
        HookEvent::CwdChanged,
    ] {
        let outcome = parse_output(event, 2, "", "no").unwrap();
        assert_eq!(outcome.blocked, event == HookEvent::ConfigChange);
        assert!(outcome.additional_context.is_empty());
        let outcome = parse_json_output(event, json!({"continue":false,"decision":"block","reason":"policy","systemMessage":"visible","hookSpecificOutput":{"hookEventName":event.name(),"additionalContext":"context","permissionDecision":"allow","updatedInput":{},"retry":true}})).unwrap();
        assert_eq!(outcome.blocked, event == HookEvent::ConfigChange);
        assert!(!outcome.prevent_continuation && !outcome.allow_permission && !outcome.retry);
        assert!(outcome.updated_input.is_none());
        assert_eq!(
            outcome.additional_context,
            if event == HookEvent::Setup {
                vec!["context"]
            } else {
                vec![]
            }
        );
        assert_eq!(outcome.warnings, vec!["visible"]);
        assert_eq!(
            parse_output(event, 0, "plain", "")
                .unwrap()
                .additional_context
                .is_empty(),
            event != HookEvent::Setup
        );
        let dir = tempfile::tempdir().unwrap();
        for kind in ["prompt", "agent"] {
            std::fs::write(
                dir.path().join("settings.json"),
                json!({"hooks":{event.name():[{"hooks":[{"type":kind,"prompt":"check"}]}]}})
                    .to_string(),
            )
            .unwrap();
            assert!(HookRunner::load(dir.path(), dir.path())
                .err()
                .unwrap()
                .to_string()
                .contains("not supported"));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        json!({"hooks":{"Setup":[{"hooks":[{"type":"http","url":"http://127.0.0.1/"}]}]}})
            .to_string(),
    )
    .unwrap();
    assert!(HookRunner::load(dir.path(), dir.path())
        .err()
        .unwrap()
        .to_string()
        .contains("not supported"));
}

#[tokio::test]
async fn policy_settings_cannot_be_blocked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), json!({"hooks":{"ConfigChange":[{"hooks":[{"type":"command","command":"node -e \"process.exit(2)\""}]}]}}).to_string()).unwrap();
    let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
    assert!(
        !runner
            .run(
                HookEvent::ConfigChange,
                "test",
                json!({"source":"policy_settings"})
            )
            .await
            .unwrap()
            .blocked
    );
    assert!(
        runner
            .run(
                HookEvent::ConfigChange,
                "test",
                json!({"source":"user_settings"})
            )
            .await
            .unwrap()
            .blocked
    );
}
#[tokio::test]
async fn sdk_callbacks_share_event_semantics_and_ignore_cwd_matcher() {
    use crate::services::hook_callbacks::{self, Host};
    use std::sync::{Arc, Mutex};
    struct Capture(Mutex<Vec<Value>>);
    impl Host for Capture {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            input: Value,
            _: Option<String>,
        ) -> futures::future::BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                self.0.lock().unwrap().push(input.clone());
                Ok(
                    json!({"decision":"block","hookSpecificOutput":{"hookEventName":input["hook_event_name"],"additionalContext":"sdk-context"}}),
                )
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let capture = Arc::new(Capture(Mutex::new(Vec::new())));
    let host: Arc<dyn Host> = capture.clone();
    let config = json!({"CwdChanged":[{"matcher":"[","hookCallbackIds":["cwd"]}],"Setup":[{"matcher":"init","hookCallbackIds":["setup"]}],"Notification":[{"matcher":"idle_prompt","hookCallbackIds":["notification"]}]});
    let _guard = hook_callbacks::install(
        "sdk-lifecycle",
        hook_callbacks::validate(&config).unwrap(),
        host.clone(),
    );
    let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
    let cwd = runner
        .run(
            HookEvent::CwdChanged,
            "sdk-lifecycle",
            json!({"old_cwd":"a","new_cwd":"b"}),
        )
        .await
        .unwrap();
    assert!(!cwd.blocked && cwd.additional_context.is_empty());
    let setup = runner
        .run(HookEvent::Setup, "sdk-lifecycle", json!({"trigger":"init"}))
        .await
        .unwrap();
    assert!(!setup.blocked);
    assert_eq!(setup.additional_context, vec!["sdk-context"]);
    runner
        .run(
            HookEvent::Notification,
            "sdk-lifecycle",
            json!({"notification_type":"permission_prompt","message":"skip"}),
        )
        .await
        .unwrap();
    assert_eq!(capture.0.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn lifecycle_cancellation_timeout_and_status_leave_once_retryable() {
    use crate::services::hook_progress;
    for event in [
        HookEvent::Notification,
        HookEvent::StopFailure,
        HookEvent::Setup,
        HookEvent::ConfigChange,
        HookEvent::InstructionsLoaded,
        HookEvent::CwdChanged,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow.cjs");
        std::fs::write(&script, "const fs=require('fs'); if(fs.existsSync('ready')) { fs.appendFileSync('success','x'); } else { setTimeout(()=>{},30000); }").unwrap();
        std::fs::write(dir.path().join("settings.json"),json!({"hooks":{event.name():[{"hooks":[{"type":"command","command":format!("node \"{}\"",script.display()),"once":true,"timeout":1,"statusMessage":"lifecycle pending"}]}]}}).to_string()).unwrap();
        let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut status = hook_progress::subscribe(&session);
        let other = hook_progress::subscribe("unrelated-lifecycle-session");
        {
            let run = runner.run(event, &session, json!({}));
            tokio::pin!(run);
            tokio::select! {
                _ = &mut run => panic!("slow hook finished early"),
                progress = status.changed() => assert_eq!(progress[0].1,"lifecycle pending"),
                _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("missing status"),
            }
            assert!(other.snapshot().is_empty());
        }
        assert!(status.snapshot().is_empty());
        let timed = runner.run(event, &session, json!({})).await;
        assert!(
            timed.is_err()
                || timed
                    .unwrap()
                    .warnings
                    .iter()
                    .any(|w| w.contains("timed out")),
            "timeout was not reported"
        );
        assert!(status.snapshot().is_empty());
        std::fs::write(dir.path().join("ready"), "").unwrap();
        for _ in 0..2 {
            runner.run(event, &session, json!({})).await.unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("success")).unwrap(),
            "x"
        );
    }
}
#[tokio::test]
async fn http_lifecycle_events_use_the_same_payload_and_output_rules() {
    use std::sync::{Arc, Mutex};
    use axum::{extract::State, routing::post, Json, Router};
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let app = Router::new().route("/hook",post(|State(received): State<Arc<Mutex<Vec<Value>>>>, Json(input): Json<Value>| async move {
        received.lock().unwrap().push(input.clone());
        Json(json!({"decision":"block","reason":"http policy","hookSpecificOutput":{"hookEventName":input["hook_event_name"],"additionalContext":"must not inject"}}))
    })).with_state(received.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let events = [
        (
            HookEvent::Notification,
            json!({"notification_type":"idle_prompt","message":"idle"}),
        ),
        (HookEvent::StopFailure, json!({"error":"unknown"})),
        (HookEvent::ConfigChange, json!({"source":"user_settings"})),
        (
            HookEvent::InstructionsLoaded,
            json!({"file_path":"CLAUDE.md","memory_type":"Project","load_reason":"session_start"}),
        ),
        (HookEvent::CwdChanged, json!({"old_cwd":"a","new_cwd":"b"})),
    ];
    let mut config = json!({});
    for (event, _) in &events {
        config[event.name()] = json!([{"hooks":[{"type":"http","url":url,"timeout":5}]}]);
    }
    std::fs::write(
        dir.path().join("settings.json"),
        json!({"hooks":config}).to_string(),
    )
    .unwrap();
    let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
    for (event, payload) in &events {
        let outcome = runner
            .run(*event, "http-lifecycle", payload.clone())
            .await
            .unwrap();
        assert_eq!(outcome.blocked, *event == HookEvent::ConfigChange);
        assert!(outcome.additional_context.is_empty());
    }
    let rows = received.lock().unwrap();
    assert_eq!(rows.len(), 5);
    for (row, (event, payload)) in rows.iter().zip(events) {
        let mut expected = payload;
        expected["hook_event_name"] = json!(event.name());
        expected["session_id"] = json!("http-lifecycle");
        expected["cwd"] = json!(dir.path());
        expected["transcript_path"] =
            json!(
                super::super::lifecycle_events::transcript(dir.path(), "http-lifecycle").unwrap()
            );
        assert_eq!(*row, expected);
    }
    server.abort();
}

#[tokio::test]
async fn unmatched_hooks_and_callbacks_never_materialize_transcripts() {
    use crate::services::hook_callbacks::{self, Host};
    use std::sync::Arc;
    struct Never;
    impl Host for Never {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            _: Value,
            _: Option<String>,
        ) -> futures::future::BoxFuture<'a, Result<Value>> {
            Box::pin(async { panic!("unmatched callback executed") })
        }
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("settings.json"), json!({"hooks":{
        "PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"must-not-run"}]},{"matcher":"Read","hooks":[{"type":"command","command":"must-not-run","if":"Read(*.rs)"}]}],
        "PermissionRequest":[{"matcher":"Write","hooks":[{"type":"command","command":"must-not-run"}]}]
    }}).to_string()).unwrap();
    // Any attempt to query this database would fail. Matching only uses memory.
    std::fs::write(root.path().join(crate::runtime::DATABASE_FILE), "not a database").unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Never);
    let callbacks = hook_callbacks::validate(&json!({"PreToolUse":[{"matcher":"Write","hookCallbackIds":["skip"]}],"PermissionRequest":[{"matcher":"Write","hookCallbackIds":["skip"]}]})).unwrap();
    let _guard = hook_callbacks::install(&session, callbacks, host.clone());
    let runner = HookRunner::load(root.path(), root.path()).unwrap();
    for event in [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PermissionRequest,
        HookEvent::Notification,
    ] {
        let outcome = runner
            .run(
                event,
                &session,
                json!({"tool_name":"Read","tool_input":{"file_path":"note.txt"}}),
            )
            .await
            .unwrap();
        assert!(!outcome.blocked);
        assert!(outcome.warnings.is_empty());
    }
    assert!(!root.path().join("hook-transcripts").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join(crate::runtime::DATABASE_FILE)).unwrap(),
        "not a database"
    );
}

#[tokio::test]
async fn callback_transcripts_are_lazy_shared_and_failure_is_nonfatal() {
    use crate::services::hook_callbacks::{self, Host};
    use std::sync::{Arc, Mutex};
    struct Capture {
        inputs: Mutex<Vec<Value>>,
        fail: bool,
    }
    impl Host for Capture {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            input: Value,
            _: Option<String>,
        ) -> futures::future::BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                let mut inputs = self.inputs.lock().unwrap();
                let path = Path::new(input["transcript_path"].as_str().unwrap());
                if !self.fail {
                    // A second snapshot within this run would erase this sentinel.
                    assert_eq!(
                        std::fs::read_to_string(path).unwrap(),
                        if inputs.is_empty() {
                            ""
                        } else {
                            "callback sentinel"
                        }
                    );
                    std::fs::write(path, "callback sentinel").unwrap();
                }
                inputs.push(input);
                Ok(json!({}))
            })
        }
    }
    for event in [HookEvent::PreToolUse, HookEvent::PermissionRequest] {
        for fail in [false, true] {
            let root = tempfile::tempdir().unwrap();
            if fail {
                std::fs::write(root.path().join("hook-transcripts"), "obstruction").unwrap();
            }
            let session = uuid::Uuid::new_v4().to_string();
            let capture = Arc::new(Capture {
                inputs: Mutex::new(vec![]),
                fail,
            });
            let host: Arc<dyn Host> = capture.clone();
            let callbacks = hook_callbacks::validate(
                &json!({event.name():[{"matcher":"Read","hookCallbackIds":["one","two"]}]}),
            )
            .unwrap();
            let _guard = hook_callbacks::install(&session, callbacks, host.clone());
            let runner = HookRunner::load(root.path(), root.path()).unwrap();
            let result = runner
                .run(event, &session, json!({"tool_name":"Read"}))
                .await
                .unwrap();
            assert!(!result.blocked);
            let inputs = capture.inputs.lock().unwrap();
            assert_eq!(inputs.len(), 2);
            for input in inputs.iter() {
                assert_eq!(
                    input["transcript_path"],
                    json!(crate::services::lifecycle_events::transcript_path(
                        root.path(),
                        &session
                    ))
                );
            }
        }
    }
}

#[tokio::test]
async fn completed_once_hook_does_not_regenerate_transcript() {
    let root = tempfile::tempdir().unwrap();
    let cmd = command(root.path());
    std::fs::write(
        root.path().join("settings.json"),
        json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":cmd,"once":true}]}]}})
            .to_string(),
    )
    .unwrap();
    let runner = HookRunner::load(root.path(), root.path()).unwrap();
    runner
        .run(
            HookEvent::PreToolUse,
            "once-lazy",
            json!({"tool_name":"Read"}),
        )
        .await
        .unwrap();
    std::fs::rename(
        root.path().join("hook-transcripts"),
        root.path().join("retained-transcripts"),
    )
    .unwrap();
    runner
        .run(
            HookEvent::PreToolUse,
            "once-lazy",
            json!({"tool_name":"Read"}),
        )
        .await
        .unwrap();
    assert!(!root.path().join("hook-transcripts").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("events.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
