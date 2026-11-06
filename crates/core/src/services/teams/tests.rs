use super::*;
use std::sync::Mutex;

static ENV: Mutex<()> = Mutex::new(());

fn enabled_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    std::env::set_var("DEEPSEEK_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
    guard
}

fn root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    (temp, data)
}

#[test]
fn create_persists_lead_and_allocates_unique_names() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let output = create(
        &root,
        "leader",
        "My Team!",
        Some("purpose"),
        None,
        Some("m"),
        &cwd,
    )
    .unwrap();
    assert_eq!(output.team.name, "my-team");
    assert_eq!(output.team.lead_agent_id, "team-lead@my-team");
    assert_eq!(output.team.lead_session_id, "leader");
    assert_eq!(output.team.members.len(), 1);
    assert_eq!(output.team.members[0].name, LEAD_NAME);
    assert!(!output.team.members[0].is_active);
    assert_eq!(output.team_file_path, team_path(&root, "my-team"));
    assert!(output.team_file_path.exists());

    let other = create(&root, "other", "My Team!", None, None, None, &cwd).unwrap();
    assert_eq!(other.team.name, "my-team-2");
    // A different leader may create another team with the same requested name.
    assert!(create(&root, "leader", "another", None, None, None, &cwd).is_err());
}

#[test]
fn members_bind_finish_and_recover_stale_sessions() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "alpha", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root,
        &team.name,
        "leader",
        "researcher",
        Some("general"),
        Some("m"),
        "investigate",
        &cwd,
    )
    .unwrap();
    assert_eq!(member.agent_id, "researcher@alpha");
    bind_member_session(&root, &team.name, &member.agent_id, "child-1").unwrap();
    register_live("child-1");
    let read = read_recovered(&root, &team.name).unwrap().unwrap();
    let entry = read
        .members
        .iter()
        .find(|m| m.name == "researcher")
        .unwrap();
    assert!(entry.is_active);
    assert_eq!(entry.session_id.as_deref(), Some("child-1"));

    // A restart loses the live registry: persisted active members become stale.
    unregister_live("child-1");
    let recovered = read_recovered(&root, &team.name).unwrap().unwrap();
    let entry = recovered
        .members
        .iter()
        .find(|m| m.name == "researcher")
        .unwrap();
    assert!(!entry.is_active);
    assert_eq!(entry.status.as_deref(), Some("stale"));

    // Other leaders cannot add members.
    assert!(register_member(&root, &team.name, "intruder", "x", None, None, "p", &cwd).is_err());
    assert!(
        register_member(&root, &team.name, "leader", LEAD_NAME, None, None, "p", &cwd).is_err()
    );
    assert!(register_member(
        &root,
        &team.name,
        "leader",
        "Researcher",
        None,
        None,
        "p",
        &cwd
    )
    .is_err());
}

#[test]
fn delete_refuses_active_members_then_removes_the_team() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "beta", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root, &team.name, "leader", "tester", None, None, "run", &cwd,
    )
    .unwrap();
    bind_member_session(&root, &team.name, &member.agent_id, "child-2").unwrap();
    register_live("child-2");

    let refused = delete(&root, "leader").unwrap();
    assert!(!refused.success);
    assert_eq!(refused.team_name.as_deref(), Some("beta"));
    assert!(refused.message.contains("tester"));
    assert!(team_dir(&root, "beta").exists());

    finish_member(&root, &team.name, &member.agent_id, "idle", Some("done")).unwrap();
    let removed = delete(&root, "leader").unwrap();
    assert!(removed.success);
    assert!(removed.message.contains("beta"));
    assert!(!team_dir(&root, "beta").exists());

    let again = delete(&root, "leader").unwrap();
    assert!(again.success);
    assert_eq!(again.team_name, None);
}

#[test]
fn gate_requires_opt_in() {
    assert!(!enabled_with(None));
    for value in ["0", "false", "off", ""] {
        assert!(!enabled_with(Some(value)));
    }
    for value in ["1", "true", "YES", " on "] {
        assert!(enabled_with(Some(value)));
    }
}

#[test]
fn finishes_are_idempotent_for_removed_teams_and_results_are_bounded() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "delta", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root, &team.name, "leader", "writer", None, None, "write", &cwd,
    )
    .unwrap();
    finish_member(
        &root,
        &team.name,
        &member.agent_id,
        "idle",
        Some(&"x".repeat(9000)),
    )
    .unwrap();
    let read = read_recovered(&root, &team.name).unwrap().unwrap();
    let entry = read.members.iter().find(|m| m.name == "writer").unwrap();
    assert_eq!(entry.result.as_deref().unwrap().chars().count(), 4097);
    delete(&root, "leader").unwrap();
    finish_member(&root, &team.name, &member.agent_id, "idle", None).unwrap();
}

#[test]
fn requests_correlate_reject_timeout_and_plan_decisions() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "req", None, None, None, &cwd)
        .unwrap()
        .team;
    let member =
        register_member(&root, &team.name, "leader", "tester", None, None, "p", &cwd).unwrap();
    bind_member_session(&root, &team.name, &member.agent_id, "child").unwrap();

    let shutdown = create_request(
        &root,
        &team.name,
        "shutdown",
        "leader",
        "child",
        60_000,
        Some("wrap up"),
        None,
    )
    .unwrap();
    assert_eq!(shutdown.state, "pending");
    let error =
        respond_request(&root, &team.name, &shutdown.id, "intruder", true, None).unwrap_err();
    assert!(error.to_string().contains("another session"));
    let answered = respond_request(&root, &team.name, &shutdown.id, "child", true, None).unwrap();
    assert_eq!(answered.state, "approved");
    let read = read_team(&root, &team.name).unwrap().unwrap();
    let entry = read.members.iter().find(|m| m.name == "tester").unwrap();
    assert!(!entry.is_active);
    assert_eq!(entry.status.as_deref(), Some("stopped"));
    assert!(respond_request(&root, &team.name, &shutdown.id, "child", false, None).is_err());

    let slow = create_request(
        &root, &team.name, "shutdown", "leader", "child", 1, None, None,
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let error = respond_request(&root, &team.name, &slow.id, "child", true, None).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(
        request(&root, &team.name, &slow.id).unwrap().unwrap().state,
        "timeout"
    );

    let plan = create_request(
        &root,
        &team.name,
        "plan_approval",
        "child",
        "leader",
        60_000,
        Some("tester"),
        Some("# Plan"),
    )
    .unwrap();
    let pending = pending_plan_request(&root, &team.name, "child").unwrap();
    assert!(
        pending.is_some(),
        "{}",
        std::fs::read_to_string(requests_path(&root, &team.name)).unwrap_or_default()
    );
    assert!(matches!(
        plan_decision(&root, &team.name, "child").unwrap(),
        PlanDecision::Pending
    ));
    respond_request(
        &root,
        &team.name,
        &plan.id,
        "leader",
        false,
        Some("needs tests"),
    )
    .unwrap();
    match plan_decision(&root, &team.name, "child").unwrap() {
        PlanDecision::Rejected { feedback } => assert_eq!(feedback.as_deref(), Some("needs tests")),
        _ => panic!("expected rejection"),
    }
    // A consumed decision is not replayed.
    assert!(matches!(
        plan_decision(&root, &team.name, "child").unwrap(),
        PlanDecision::Pending
    ));
}

#[test]
fn session_identity_resolves_leader_and_members() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "who", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root,
        &team.name,
        "leader",
        "scout",
        Some("general"),
        None,
        "p",
        &cwd,
    )
    .unwrap();
    bind_member_session(&root, &team.name, &member.agent_id, "child").unwrap();
    assert_eq!(
        identity(&root, "leader").unwrap(),
        Some(("who".into(), LEAD_NAME.into()))
    );
    assert_eq!(
        identity(&root, "child").unwrap(),
        Some(("who".into(), "scout".into()))
    );
    assert_eq!(identity(&root, "other").unwrap(), None);
    let refreshed = read_team(&root, &team.name).unwrap().unwrap();
    assert!(member_by_name(&refreshed, "SCOUT").is_some());
}

#[test]
fn classify_peers_distinguishes_team_subagent_and_session() {
    use serde_json::json;
    let _env = enabled_guard();
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let context = crate::services::session_messages::context(&data).unwrap();
    let cwd = temp.path().join("project");
    let team = create(&data, "lead", "view", None, None, None, &cwd)
        .unwrap()
        .team;
    let member =
        register_member(&data, &team.name, "lead", "alpha", None, None, "p", &cwd).unwrap();
    bind_member_session(&data, &team.name, &member.agent_id, "child").unwrap();
    register_live("child");
    context
        .save_setting(
            "delegation.sub",
            &json!({"parent_session_id":"lead","status":"completed"}),
        )
        .unwrap();
    let mut peers = vec![
        json!({"id":"lead"}),
        json!({"id":"child"}),
        json!({"id":"sub"}),
        json!({"id":"independent"}),
    ];
    classify_peers(&data, &context, &mut peers).unwrap();
    assert_eq!(peers[0]["kind"], "team-lead");
    assert_eq!(peers[0]["teammate_name"], LEAD_NAME);
    assert_eq!(peers[1]["kind"], "teammate");
    assert_eq!(peers[1]["team"], "view");
    assert_eq!(peers[1]["teammate_name"], "alpha");
    assert_eq!(peers[1]["member_status"], "active");
    assert_eq!(peers[2]["kind"], "subagent");
    assert_eq!(peers[3]["kind"], "session");
    unregister_live("child");
}
