//! User-selected plan acceptance, independent of model arguments and permission parsing.
use super::ServiceContext;
use anyhow::{bail, Context, Result};

pub const OPTIONS: [&str; 4] = [
    "Yes, clear context and auto-accept edits",
    "Yes, auto-accept edits (keep context)",
    "Yes, manually approve edits (keep context)",
    "No, keep planning (or enter feedback)",
];
#[derive(Debug, PartialEq)]
pub enum Choice {
    ClearEdits,
    KeepEdits,
    ClearBypass,
    KeepBypass,
    Manual,
    ClearManual,
    ClearAuto,
    KeepAuto,
    Revise(String),
}
pub const BYPASS_CLEAR: &str = "Yes, clear context and return to bypass mode";
pub const BYPASS_KEEP: &str = "Yes, return to bypass mode (keep context)";

pub fn options(bypass: bool) -> [&'static str; 4] {
    if bypass {
        [BYPASS_CLEAR, BYPASS_KEEP, OPTIONS[2], OPTIONS[3]]
    } else {
        OPTIONS
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub session_id: String,
    pub request_id: String,
    pub tool_use_id: String,
    pub plan_version: String,
    pub plan: String,
    pub plan_file_path: String,
    pub status: String,
    pub choices: Vec<ApprovalOption>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalOption {
    pub mode: String,
    pub context: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

impl ApprovalRequest {
    pub fn new(session: &str, request: &str, tool: &str, plan: &str, path: &std::path::Path, bypass: bool) -> Self {
        Self {
            kind: "plan_approval".into(),
            version: 1,
            session_id: session.into(),
            request_id: request.into(),
            tool_use_id: tool.into(),
            // An opaque review version binds this request to this exact plan snapshot.
            plan_version: uuid::Uuid::new_v4().to_string(),
            plan: plan.into(),
            plan_file_path: path.to_string_lossy().into_owned(),
            status: "pending".into(),
            choices: ["ask", "auto", "bypass"].into_iter().flat_map(|mode| {
                ["keep", "clear"].into_iter().map(move |context| ApprovalOption {
                    mode: mode.into(), context: context.into(),
                    enabled: mode != "bypass" || bypass,
                    disabled_reason: (mode == "bypass" && !bypass).then(|| "Bypass is unavailable for this execution host".into()),
                })
            }).collect(),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApprovalResponse {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    session_id: String,
    request_id: String,
    tool_use_id: String,
    plan_version: String,
    action: String,
    mode: Option<String>,
    context: Option<String>,
    feedback: Option<String>,
}

/// Validate against the server-owned pending request, never a client-supplied request.
/// Legacy CLI answers retain their original accept-edits and bypass semantics.
pub fn parse_response(answer: &str, request: &ApprovalRequest, bypass: bool, bypass_available: bool) -> Result<Choice> {
    let value = if answer.trim_start().starts_with('{') {
        Some(serde_json::from_str::<serde_json::Value>(answer)?)
    } else {
        None
    };
    let Some(value) = value.filter(|value| value.get("type").is_some() || value.get("version").is_some()) else {
        return parse_for_mode(answer, bypass);
    };
    let response: ApprovalResponse = serde_json::from_value(value)?;
    anyhow::ensure!(request.kind == "plan_approval" && request.version == 1 && request.status == "pending", "Plan approval request is not pending or supported");
    anyhow::ensure!(response.kind == "plan_approval_response" && response.version == 1, "Unsupported plan approval response version or type");
    anyhow::ensure!(response.session_id == request.session_id && response.request_id == request.request_id && response.tool_use_id == request.tool_use_id, "Plan approval request ownership mismatch");
    anyhow::ensure!(response.plan_version == request.plan_version, "Plan review version is stale; request approval again");
    if response.action == "revise" {
        anyhow::ensure!(response.mode.is_none() && response.context.is_none(), "Feedback cannot also approve a permission mode");
        return Ok(Choice::Revise(response.feedback.unwrap_or_default()));
    }
    anyhow::ensure!(response.action == "approve" && response.feedback.is_none(), "Invalid plan approval action");
    let mode = response.mode.context("Approval mode is required")?;
    let context = response.context.context("Approval context policy is required")?;
    anyhow::ensure!(request.choices.iter().any(|choice| choice.enabled && choice.mode == mode && choice.context == context), "Plan approval choice is unavailable");
    anyhow::ensure!(mode != "bypass" || bypass_available, "Bypass launch authority is unavailable");
    Ok(match (mode.as_str(), context.as_str()) {
        ("ask", "keep") => Choice::Manual,
        ("ask", "clear") => Choice::ClearManual,
        ("auto", "keep") => Choice::KeepAuto,
        ("auto", "clear") => Choice::ClearAuto,
        ("bypass", "keep") => Choice::KeepBypass,
        ("bypass", "clear") => Choice::ClearBypass,
        _ => bail!("Unknown plan approval choice"),
    })
}

pub fn parse(answer: &str) -> Result<Choice> {
    parse_for_mode(answer, false)
}

pub fn parse_for_mode(answer: &str, bypass: bool) -> Result<Choice> {
    let answer = answer.trim();
    // Desktop's existing AskUser route returns one question→answer mapping.
    if answer.starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(answer)?;
        let entries = value.as_object().context("Plan answer must be an object")?;
        anyhow::ensure!(
            entries.len() == 1,
            "Plan approval requires exactly one answer"
        );
        let selected = entries
            .values()
            .next()
            .and_then(serde_json::Value::as_str)
            .context("Plan answer must be text")?;
        anyhow::ensure!(
            !selected.trim().starts_with('{'),
            "Nested plan answers are unsupported"
        );
        return parse_for_mode(selected, bypass);
    }
    Ok(match answer {
        "1" | "bypass_clear" | BYPASS_CLEAR if bypass => Choice::ClearBypass,
        "2" | "bypass_keep" | BYPASS_KEEP if bypass => Choice::KeepBypass,
        "1" | "accept_edits_clear" => Choice::ClearEdits,
        "2" | "accept_edits_keep" => Choice::KeepEdits,
        "3" | "manual_keep" | "yes" | "y" | "allow" | "approved" | "允许" | "是" => {
            Choice::Manual
        }
        "4" | "no" | "n" => Choice::Revise(String::new()),
        value if value == OPTIONS[0] => Choice::ClearEdits,
        value if value == OPTIONS[1] => Choice::KeepEdits,
        value if value == OPTIONS[2] => Choice::Manual,
        value if value == OPTIONS[3] => Choice::Revise(String::new()),
        value if value.starts_with("feedback:") => Choice::Revise(value[9..].trim().to_owned()),
        _ => bail!("Unknown plan approval choice; use 1–4 or feedback: <changes>"),
    })
}

pub fn commit_boundary(context: &ServiceContext, session: &str, content: &str) -> Result<String> {
    commit_boundary_inner(context, session, content, None)
}
pub fn commit_approved_boundary(
    context: &ServiceContext,
    session: &str,
    content: &str,
    manager: &crate::permissions::PermissionManager,
    expected: crate::permissions::manager::ModeSnapshot,
) -> Result<String> {
    commit_approved_boundary_mode(
        context,
        session,
        content,
        manager,
        expected,
        crate::permissions::PermissionMode::AcceptEdits,
    )
}
pub fn commit_approved_boundary_mode(
    context: &ServiceContext,
    session: &str,
    content: &str,
    manager: &crate::permissions::PermissionManager,
    expected: crate::permissions::manager::ModeSnapshot,
    mode: crate::permissions::PermissionMode,
) -> Result<String> {
    commit_boundary_inner(context, session, content, Some((manager, expected, mode)))
}
pub fn commit_approved_mode(
    context: &ServiceContext,
    session: &str,
    manager: &crate::permissions::PermissionManager,
    expected: crate::permissions::manager::ModeSnapshot,
    mode: crate::permissions::PermissionMode,
    approved_plan: &str,
) -> Result<()> {
    context.db.with_conn(|conn| -> Result<()> {
        let tx=crate::db::write_transaction(&conn)?;
        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",rusqlite::params![format!("cli.permission-mode.{session}"),super::session_permission_mode::name(mode)])?;
        super::permission_authority::record_conn(&tx,session,super::permission_authority::Source::ApprovedPlan,&serde_json::json!(approved_plan))?;
        manager.commit_mode_transition(expected,mode,||{tx.commit()?;Ok(())})
    })?
}
fn commit_boundary_inner(
    context: &ServiceContext,
    session: &str,
    content: &str,
    transition: Option<(
        &crate::permissions::PermissionManager,
        crate::permissions::manager::ModeSnapshot,
        crate::permissions::PermissionMode,
    )>,
) -> Result<String> {
    context.db.with_conn(|conn| -> Result<String> {
        let tx = crate::db::write_transaction(&conn)?;
        crate::db::conversation_repo::get_conversation(&tx, session)?
            .context("Plan session not found")?;
        let order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",
            [session],
            |r| r.get(0),
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        crate::db::message_repo::insert_message(
            &tx,
            &id,
            session,
            "user",
            content,
            None,
            &chrono::Utc::now().to_rfc3339(),
            true,
            order,
        )?;
        crate::db::conversation_repo::increment_message_count(&tx, session)?;
        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",rusqlite::params![format!("plan.context_boundary.{session}"),serde_json::json!({"boundary_id":id}).to_string()])?;
        if let Some((manager, expected, mode)) = transition {
            super::permission_authority::record_conn(&tx,session,super::permission_authority::Source::ApprovedPlan,&serde_json::json!(content))?;
            tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[format!("cli.permission-mode.{session}"), super::session_permission_mode::name(mode).into()])?;
            manager.commit_mode_transition(expected, mode, || { tx.commit()?; Ok(()) })?;
        } else { tx.commit()?; }
        Ok(id)
    })?
}

pub fn latest_boundary(db: &crate::db::DbManager, session: &str) -> Result<Option<String>> {
    db.with_conn(|conn| latest_boundary_conn(conn, session))?
}
pub fn latest_boundary_conn(conn: &rusqlite::Connection, session: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("plan.context_boundary.{session}")],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let metadata: serde_json::Value = serde_json::from_str(&raw)?;
    let id = metadata["boundary_id"]
        .as_str()
        .context("Invalid plan boundary metadata")?;
    Ok(conn
        .query_row(
            "SELECT id FROM messages WHERE id=?1 AND conversation_id=?2 AND is_compact_boundary=1",
            rusqlite::params![id, session],
            |r| r.get(0),
        )
        .optional()?)
}

/// Call inside the response's write transaction. Either a boundary already
/// exists and this stale segment sorts before it, or a later boundary sorts
/// after the saved response. There is no check/save race across processes.
pub fn final_response_order(
    conn: &rusqlite::Connection,
    session: &str,
    observed: Option<&str>,
) -> Result<(i64, bool)> {
    let latest = latest_boundary_conn(conn, session)?;
    if latest.as_deref() != observed {
        if let Some(id) = latest {
            let order: i64 = conn.query_row(
                "SELECT sort_order FROM messages WHERE id=?1 AND conversation_id=?2",
                rusqlite::params![id, session],
                |r| r.get(0),
            )?;
            return Ok((order - 1, true));
        }
    }
    Ok((
        conn.query_row(
            "SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",
            [session],
            |r| r.get(0),
        )?,
        false,
    ))
}

/// Archive a host-buffered pre-reset response before the already durable boundary.
pub fn archive_segment(
    db: &crate::db::DbManager,
    session: &str,
    boundary: &str,
    existing: Option<&str>,
    text: &str,
    thinking: &str,
    tools: &[crate::db::message_repo::ToolCallRow],
    snapshots: &[serde_json::Value],
) -> Result<()> {
    db.with_conn(|conn| -> Result<()> {
        // Read-then-write: take the write lock up front. A deferred transaction
        // fails with SQLITE_BUSY at once (no busy wait) when another connection
        // commits between the read and the first write.
        let tx = crate::db::write_transaction(conn)?;
        let order: i64 = tx.query_row("SELECT sort_order FROM messages WHERE id=?1 AND conversation_id=?2 AND is_compact_boundary=1", rusqlite::params![boundary,session], |r|r.get(0))?;
        let id = existing.map(str::to_owned).unwrap_or_else(||uuid::Uuid::new_v4().to_string());
        if let Some(message) = crate::db::message_repo::get_message(&tx,&id)? {
            anyhow::ensure!(message.conversation_id == session && message.role == "assistant", "Archive response belongs to another session or role");
            tx.execute("UPDATE messages SET content=?2,thinking=?3,sort_order=?4 WHERE id=?1",rusqlite::params![id,text,thinking,order-1])?;
        } else {
            crate::db::message_repo::insert_message(&tx,&id,session,"assistant",text,Some(thinking),&chrono::Utc::now().to_rfc3339(),false,order-1)?;
            crate::db::conversation_repo::increment_message_count(&tx,session)?;
        }
        for tool in tools { crate::db::message_repo::insert_tool_call(&tx,&format!("{id}:{}",tool.id),&id,&tool.name,tool.input.as_deref(),tool.output.as_deref(),tool.is_error,tool.sort_order)?; }
        crate::db::message_repo::save_request_snapshots(&tx,&id,&serde_json::json!(snapshots))?;
        tx.commit()?;
        Ok(())
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnected_desktop_save_is_safe_on_either_side_of_boundary_commit() {
        for boundary_first in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let context =
                crate::services::session_messages::context(&temp.path().join("data")).unwrap();
            let sessions = crate::runtime::SessionStore::new(context.db.clone());
            let id = sessions.create(temp.path(), "model", "provider").unwrap();
            sessions.append(&id, "user", "old request").unwrap();
            if boundary_first {
                commit_boundary(&context, &id, "approved plan").unwrap();
            }
            // Same transaction/order helper as Desktop final save, without a reset event.
            context
                .db
                .with_conn(|conn| -> Result<()> {
                    let tx = crate::db::write_transaction(conn)?;
                    let (order, archive) = final_response_order(&tx, &id, None)?;
                    assert_eq!(archive, boundary_first);
                    crate::db::message_repo::insert_message(
                        &tx,
                        "old-response",
                        &id,
                        "assistant",
                        "OLD_DESKTOP_SECRET",
                        None,
                        "now",
                        false,
                        order,
                    )?;
                    tx.commit()?;
                    Ok(())
                })
                .unwrap()
                .unwrap();
            if !boundary_first {
                commit_boundary(&context, &id, "approved plan").unwrap();
            }
            let history = sessions
                .model_history(
                    &id,
                    &crate::native_engine::provider_manager::ApiFormat::OpenAI,
                )
                .unwrap();
            assert!(!serde_json::to_string(&history)
                .unwrap()
                .contains("OLD_DESKTOP_SECRET"));
            context
                .db
                .with_conn(|conn| {
                    conn.execute(
                        "UPDATE peripheral_settings SET value='invalid metadata' WHERE key=?1",
                        [format!("plan.context_boundary.{id}")],
                    )
                })
                .unwrap()
                .unwrap();
            assert!(context
                .db
                .with_conn(|conn| final_response_order(conn, &id, None))
                .unwrap()
                .is_err());
        }
    }
    #[test]
    fn mode_revision_and_commit_failures_leave_no_approved_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let context =
            crate::services::session_messages::context(&temp.path().join("data")).unwrap();
        let sessions = crate::runtime::SessionStore::new(context.db.clone());
        let id = sessions.create(temp.path(), "model", "provider").unwrap();
        let manager = crate::runtime::default_permissions();
        manager.set_mode(crate::permissions::PermissionMode::PlanMode);
        let stale = manager.mode_snapshot();
        manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
        manager.set_mode(crate::permissions::PermissionMode::PlanMode);
        assert!(commit_approved_boundary(&context, &id, "plan", &manager, stale).is_err());
        assert!(latest_boundary(&context.db, &id).unwrap().is_none());
        context.db.with_conn(|conn|conn.execute_batch("CREATE TABLE plan_parent(id INTEGER PRIMARY KEY); CREATE TABLE plan_guard(id INTEGER REFERENCES plan_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_plan_commit AFTER INSERT ON messages WHEN NEW.is_compact_boundary=1 BEGIN INSERT INTO plan_guard VALUES(99); END;")).unwrap().unwrap();
        assert!(
            commit_approved_boundary(&context, &id, "plan", &manager, manager.mode_snapshot())
                .is_err()
        );
        assert_eq!(
            manager.get_mode(),
            crate::permissions::PermissionMode::PlanMode
        );
        assert!(latest_boundary(&context.db, &id).unwrap().is_none());
        assert!(sessions.history(&id).unwrap().is_empty());
    }
    #[test]
    fn choices_accept_desktop_answers_and_never_invent_bypass() {
        for (answer, expected) in [
            ("1", Choice::ClearEdits),
            ("2", Choice::KeepEdits),
            ("3", Choice::Manual),
            ("yes", Choice::Manual),
            ("4", Choice::Revise(String::new())),
        ] {
            assert_eq!(parse(answer).unwrap(), expected);
            assert_eq!(
                parse(&serde_json::json!({"Review plan":answer}).to_string()).unwrap(),
                expected
            );
        }
        for answer in ["bypassPermissions", "auto", "", r#"{"a":"1","b":"2"}"#] {
            assert!(parse(answer).is_err());
        }
    }
    #[test]
    fn late_host_archive_stays_before_boundary_and_replay_excludes_old_tools() {
        let temp = tempfile::tempdir().unwrap();
        let context =
            crate::services::session_messages::context(&temp.path().join("data")).unwrap();
        let sessions = crate::runtime::SessionStore::new(context.db.clone());
        let id = sessions.create(temp.path(), "model", "provider").unwrap();
        sessions.append(&id, "user", "old history").unwrap();
        let boundary = commit_boundary(
            &context,
            &id,
            "Implement the following approved plan: new work",
        )
        .unwrap();
        let tool = crate::db::message_repo::ToolCallRow {
            id: "old-call".into(),
            message_id: String::new(),
            name: "Write".into(),
            input: Some("OLD_SECRET".into()),
            output: Some("skipped".into()),
            is_error: true,
            sort_order: 0,
        };
        archive_segment(
            &context.db,
            &id,
            &boundary,
            None,
            "OLD_SECRET",
            "OLD_THINKING",
            &[tool],
            &[],
        )
        .unwrap();
        sessions.append(&id, "assistant", "new answer").unwrap();
        let history = sessions
            .model_history(
                &id,
                &crate::native_engine::provider_manager::ApiFormat::OpenAI,
            )
            .unwrap();
        assert_eq!(history.len(), 2);
        assert!(!serde_json::to_string(&history).unwrap().contains("OLD"));
        assert!(serde_json::to_string(&sessions.history(&id).unwrap())
            .unwrap()
            .contains("OLD_SECRET"));
    }
}
