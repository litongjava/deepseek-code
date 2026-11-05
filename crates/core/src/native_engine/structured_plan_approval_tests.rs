use super::*;
use crate::services::plan_approval::{parse_response, ApprovalRequest, Choice};

fn response(request: &ApprovalRequest, mode: &str, context: &str) -> Value {
    json!({"type":"plan_approval_response","version":1,
        "sessionId":request.session_id,"requestId":request.request_id,
        "toolUseId":request.tool_use_id,"planVersion":request.plan_version,
        "action":"approve","mode":mode,"context":context})
}

#[test]
fn structured_choices_versions_ownership_feedback_and_legacy() {
    let request = ApprovalRequest::new(
        "session",
        "request",
        "tool",
        "# Plan",
        std::path::Path::new("plan.md"),
        true,
    );
    for (mode, context, expected) in [
        ("ask", "keep", Choice::Manual),
        ("ask", "clear", Choice::ClearManual),
        ("auto", "keep", Choice::KeepAuto),
        ("auto", "clear", Choice::ClearAuto),
        ("bypass", "keep", Choice::KeepBypass),
        ("bypass", "clear", Choice::ClearBypass),
    ] {
        assert_eq!(
            parse_response(
                &response(&request, mode, context).to_string(),
                &request,
                true
            )
            .unwrap(),
            expected
        );
    }
    for (field, invalid) in [
        ("version", json!(2)),
        ("planVersion", json!("stale")),
        ("sessionId", json!("other")),
        ("requestId", json!("other")),
        ("toolUseId", json!("other")),
        ("mode", json!("accept_edits")),
        ("context", json!("erase")),
    ] {
        let mut answer = response(&request, "ask", "keep");
        answer[field] = invalid;
        assert!(
            parse_response(&answer.to_string(), &request, true).is_err(),
            "{field}"
        );
    }
    assert!(parse_response(
        &response(&request, "bypass", "keep").to_string(),
        &request,
        false
    )
    .is_err());
    let unavailable = ApprovalRequest::new(
        "session",
        "request",
        "tool",
        "# Plan",
        std::path::Path::new("plan.md"),
        false,
    );
    assert_eq!(unavailable.choices.len(), 6);
    assert!(parse_response(
        &response(&unavailable, "bypass", "clear").to_string(),
        &unavailable,
        true
    )
    .is_err());
    let mut feedback = response(&request, "ask", "keep");
    feedback["action"] = json!("revise");
    feedback["feedback"] = json!("Add tests");
    assert!(parse_response(&feedback.to_string(), &request, true).is_err());
    feedback.as_object_mut().unwrap().remove("mode");
    feedback.as_object_mut().unwrap().remove("context");
    assert_eq!(
        parse_response(&feedback.to_string(), &request, true).unwrap(),
        Choice::Revise("Add tests".into())
    );
    for bypass in [false, true] {
        for answer in [
            "1",
            "2",
            "3",
            "4",
            "yes",
            "feedback: revise",
            "accept_edits_clear",
            "accept_edits_keep",
            "manual_keep",
            "{\"Review\":\"3\"}",
        ] {
            assert_eq!(
                parse_response(answer, &request, bypass).unwrap(),
                crate::services::plan_approval::parse_for_mode(answer, bypass).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn structured_approval_commits_six_modes_and_rejects_stale_reviews() {
    use crate::services::session_permission_mode as modes;
    for mode in ["ask", "auto", "bypass"] {
        for policy in ["keep", "clear"] {
            for failure in ["", "version", "plan", "mode", "revoked", "feedback"] {
                let (_temp, context, _sessions, mut executor, mut events, manager) =
                    super::approval_tests::fixture();
                let session = executor.conv_id.clone().unwrap();
                manager.set_plan_launch_permissions(true, false);
                modes::set(
                    &context.db,
                    &session,
                    &manager,
                    PermissionMode::BypassPermissions,
                )
                .unwrap();
                modes::set(&context.db, &session, &manager, PermissionMode::PlanMode).unwrap();
                let waiters = executor.answer_waiters.clone();
                let task = tokio::spawn(async move {
                    let result = executor
                        .plan_transition(
                            "ExitPlanMode",
                            &json!({"plan":"# Reviewed plan"}),
                            "review-tool",
                        )
                        .await;
                    (executor, result)
                });
                let EngineEvent::AskUser {
                    request_id,
                    tool_input,
                    ..
                } = timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                else {
                    panic!("Expected approval");
                };
                let request: ApprovalRequest =
                    serde_json::from_value(tool_input["planApprovalRequest"].clone()).unwrap();
                assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
                let mut answer = response(&request, mode, policy);
                match failure {
                    "version" => answer["planVersion"] = json!("stale"),
                    "plan" => {
                        plans::save(&context.paths.data_dir, &session, "# Changed").unwrap();
                    }
                    "mode" => {
                        manager.set_mode(PermissionMode::AskPermissions);
                        manager.set_mode(PermissionMode::PlanMode);
                    }
                    "revoked" => manager.set_plan_launch_permissions(false, false),
                    "feedback" => {
                        answer["action"] = json!("revise");
                        answer["feedback"] = json!("Revise tests");
                        answer.as_object_mut().unwrap().remove("mode");
                        answer.as_object_mut().unwrap().remove("context");
                    }
                    _ => {}
                }
                waiters
                    .lock()
                    .await
                    .remove(&request_id)
                    .unwrap()
                    .send(answer.to_string())
                    .unwrap();
                let (executor, result) = timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap();
                let failed = matches!(failure, "version" | "plan" | "mode")
                    || (failure == "revoked" && mode == "bypass");
                assert_eq!(
                    result.is_err(),
                    failed,
                    "{mode}/{policy}/{failure}: {result:?}"
                );
                let approved = !failed && failure != "feedback";
                let expected = if !approved {
                    PermissionMode::PlanMode
                } else {
                    match mode {
                        "ask" => PermissionMode::AskPermissions,
                        "auto" => PermissionMode::Auto,
                        _ => PermissionMode::BypassPermissions,
                    }
                };
                assert_eq!(manager.get_mode(), expected);
                assert_eq!(
                    executor.pending_plan_reset.is_some(),
                    approved && policy == "clear"
                );
                if approved {
                    assert_eq!(result.unwrap()["mode"], mode);
                    assert_eq!(modes::load(&context.db, &session).unwrap(), Some(expected));
                }
                assert!(waiters.lock().await.is_empty());
            }
        }
    }
}
