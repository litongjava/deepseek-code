//! Durable project-local agent mailboxes; no network messages are sent.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

pub(super) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {name:"SendMessage".into(),description:"Send durable team or same-project messages. With a team, address a teammate by name or use \"*\" to broadcast; without a team use a session UUID from /list-agents. Active recipients consume queued context at their next safe model boundary. Operations: send, shutdown_request (leader only), shutdown_response, plan_approval_response (leader only), request_status, receive, sent, pending_count, mark_read, peers. No external network transmission.".into(),input_schema:json!({"type":"object","properties":{"operation":{"type":"string","enum":["send","receive","pending_count","mark_read","sent","peers","shutdown_request","shutdown_response","plan_approval_response","request_status"]},"to":{"type":"string","description":"Teammate name, \"*\" for broadcast, or a session UUID"},"from":{"type":"string"},"content":{"type":"string"},"message":{"description":"Plain text or a structured shutdown/plan response"},"recipient":{"type":"string"},"unread_only":{"type":"boolean"},"message_id":{"type":"string"},"request_id":{"type":"string"},"approve":{"description":"true/false or yes/no"},"reason":{"type":"string"},"feedback":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["operation"]})}
}

fn message_content(input: &Value) -> Result<String> {
    if let Some(text) = input["content"].as_str() {
        return Ok(text.to_owned());
    }
    match &input["message"] {
        Value::String(text) => Ok(text.clone()),
        Value::Object(_) => Ok(input["message"].to_string()),
        _ => bail!("content or message is required"),
    }
}

fn approve(input: &Value) -> Result<bool> {
    match &input["approve"] {
        Value::Bool(value) => Ok(*value),
        Value::String(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Ok(true),
            "false" | "no" | "0" => Ok(false),
            _ => bail!("approve must be true or false"),
        },
        _ => bail!("approve is required"),
    }
}

fn teammate<'a>(
    team: &'a crate::services::teams::TeamFile,
    to: &str,
) -> Result<&'a crate::services::teams::TeamMember> {
    crate::services::teams::member_by_name(team, to)
        .or_else(|| {
            team.members
                .iter()
                .find(|member| member.session_id.as_deref() == Some(to))
        })
        .with_context(|| {
            format!(
                "Unknown recipient: {to}. Teammates: {}",
                team.members
                    .iter()
                    .filter(|member| member.name != crate::services::teams::LEAD_NAME)
                    .map(|member| member.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn delivered(name: Option<&str>, session: &str) -> Value {
    let mut value = json!({"session_id":session});
    if let Some(name) = name {
        value["name"] = json!(name);
    }
    value
}

/// Describes when a queued message reaches its recipient: inside a running
/// turn, on the next turn of a session open in another frontend, or when a
/// closed session is resumed.
fn delivery(result: &mut Value, active: bool, open: bool) {
    result["receiver_active"] = json!(active);
    result["receiver_open"] = json!(active || open);
    if active {
        result["delivery"] = json!("next_safe_model_boundary");
    } else if open {
        result["delivery"] = json!("next_recipient_turn");
        result["note"] = json!("The recipient's session is open but idle: its user is notified and the message enters context on its next turn. Do not resume it from here.");
    } else {
        result["delivery"] = json!("next_resume");
        result["note"] = json!("The recipient's session is closed; it reads the message when it is resumed.");
    }
}

/// Live routing uses the private runtime DB; legacy cwd mail is explicitly separate.
pub fn execute_live(
    context: &crate::services::ServiceContext,
    session: &str,
    input: Value,
) -> Result<Value> {
    use crate::services::session_messages as live;
    use crate::services::teams;
    let root = &context.paths.data_dir;
    match input["operation"]
        .as_str()
        .context("operation is required")?
    {
        "send" => {
            let to = input["to"].as_str().context("to is required")?;
            let content = message_content(&input)?;
            let Some(team) = teams::team_for_session(root, session)? else {
                // Without a team the recipient must already be a session UUID.
                let message = live::send(context, session, to, &content)?;
                let active = live::receiver_active(root, to)?;
                let open = live::session_open(root, to)?;
                let mut result = json!({"message":message,"state":"queued","processed":false,
                    "to":to,"delivered":[delivered(None,to)]});
                delivery(&mut result, active, open);
                return Ok(result);
            };
            if to == "*" {
                let recipients: Vec<_> = team
                    .members
                    .iter()
                    .filter(|member| {
                        member.session_id.as_deref().is_some_and(|id| id != session)
                            && member.status.as_deref() != Some("stopped")
                    })
                    .collect();
                ensure!(
                    !recipients.is_empty(),
                    "No active teammates to broadcast to"
                );
                let mut messages = Vec::new();
                let mut targets = Vec::new();
                for member in recipients {
                    let recipient = member.session_id.as_deref().unwrap_or_default();
                    messages.push(live::send(context, session, recipient, &content)?);
                    targets.push(delivered(Some(&member.name), recipient));
                }
                return Ok(json!({
                    "success":true,
                    "broadcast":true,
                    "message":format!("Broadcast to {} teammate(s)", targets.len()),
                    "recipients":targets,
                    "messages":messages,
                    "delivered":targets
                }));
            }
            let recipient = if let Some(member) = teams::member_by_name(&team, to) {
                ensure!(
                    member.status.as_deref() != Some("stopped"),
                    "Teammate has stopped: {to}"
                );
                member
                    .session_id
                    .clone()
                    .with_context(|| format!("Teammate {to} has no session yet"))?
            } else if uuid::Uuid::parse_str(to).is_ok() {
                to.to_owned()
            } else {
                teammate(&team, to)?;
                bail!("Unknown recipient: {to}");
            };
            let message = live::send(context, session, &recipient, &content)?;
            let active = live::receiver_active(root, &recipient)?;
            let open = live::session_open(root, &recipient)?;
            let mut result = json!({"message":message,"state":"queued","processed":false,
                "to":recipient,"delivered":[delivered(Some(to),&recipient)]});
            delivery(&mut result, active, open);
            Ok(result)
        }
        "shutdown_request" => {
            let team = teams::team_for_session(root, session)?
                .context("Shutdown requests require a team")?;
            ensure!(
                team.lead_session_id == session,
                "Only the team leader can request shutdown"
            );
            let to = input["to"]
                .as_str()
                .context("to teammate name is required")?;
            let member = teammate(&team, to)?;
            ensure!(
                member.name != teams::LEAD_NAME,
                "The team leader cannot shut itself down"
            );
            ensure!(
                member.status.as_deref() != Some("stopped"),
                "Teammate has already stopped: {to}"
            );
            let recipient = member
                .session_id
                .clone()
                .with_context(|| format!("Teammate {to} has no session yet"))?;
            let timeout = input["timeout_ms"].as_u64().unwrap_or(60_000);
            let request = teams::create_request(
                root,
                &team.name,
                "shutdown",
                session,
                &recipient,
                timeout,
                input["reason"].as_str(),
                None,
            )?;
            let content =
                json!({"type":"shutdown_request","request_id":request.id,"reason":request.reason})
                    .to_string();
            let message = live::send(context, session, &recipient, &content)?;
            Ok(json!({
                "success":true,
                "message":format!("Shutdown requested from {to}"),
                "request_id":request.id,
                "target":member.name,
                "to":recipient,
                "delivered":[delivered(Some(&member.name),&recipient)],
                "mailbox":message
            }))
        }
        "shutdown_response" | "plan_approval_response" => {
            let team =
                teams::team_for_session(root, session)?.context("Team responses require a team")?;
            let id = input["request_id"]
                .as_str()
                .context("request_id is required")?;
            let approve = approve(&input)?;
            let feedback = input["reason"]
                .as_str()
                .or_else(|| input["feedback"].as_str());
            if input["operation"] == "plan_approval_response" {
                ensure!(
                    team.lead_session_id == session,
                    "Only the team leader can answer plan approval requests"
                );
            }
            let request = teams::respond_request(root, &team.name, id, session, approve, feedback)?;
            if input["operation"] == "plan_approval_response" {
                ensure!(
                    request.kind == "plan_approval",
                    "Request is not a plan approval"
                );
            } else {
                ensure!(
                    request.kind == "shutdown",
                    "Request is not a shutdown request"
                );
            }
            let content = json!({
                "type":input["operation"],
                "request_id":request.id,
                "approve":approve,
                "reason":request.feedback
            })
            .to_string();
            let message = live::send(context, session, &request.from, &content)?;
            Ok(json!({
                "success":true,
                "message":if approve {"Request approved"} else {"Request rejected"},
                "request_id":request.id,
                "state":request.state,
                "delivered":[delivered(None,&request.from)],
                "mailbox":message
            }))
        }
        "request_status" => {
            let team =
                teams::team_for_session(root, session)?.context("Team requests require a team")?;
            let id = input["request_id"]
                .as_str()
                .context("request_id is required")?;
            let request = teams::request(root, &team.name, id)?
                .with_context(|| format!("Request not found: {id}"))?;
            Ok(json!({"request":request}))
        }
        "sent" => Ok(json!({"messages":live::sent(context,session)?})),
        "peers" => Ok(json!({"sessions":live::peers(context,session)?})),
        "receive" => Ok(
            json!({"messages":live::inbox(context,session,input["unread_only"].as_bool().unwrap_or(true))?}),
        ),
        "pending_count" => Ok(json!({"pending_count":live::pending_count(context,session)?})),
        "mark_read" => {
            let id = input["message_id"]
                .as_str()
                .context("message_id is required")?;
            live::mark_read(context, session, id)?;
            Ok(json!({"message_id":id,"read":true}))
        }
        _ => bail!("Unknown live message operation"),
    }
}

pub fn execute_for_session(mut input: Value, cwd: &str, session_id: &str) -> Result<Value> {
    input["from"] = json!(session_id);
    input["recipient"] = json!(session_id);
    execute(input, cwd)
}

pub(super) fn execute(input: Value, cwd: &str) -> Result<Value> {
    let operation = input["operation"]
        .as_str()
        .context("operation is required")?;
    if !matches!(
        operation,
        "send" | "receive" | "pending_count" | "mark_read"
    ) {
        bail!("Unknown message operation");
    }
    let root = std::path::Path::new(cwd).canonicalize()?.join(".claude");
    let path = root.join("agent-messages.sqlite3");
    super::resolve_path(&path.to_string_lossy(), cwd)?;
    let readonly = matches!(operation, "receive" | "pending_count");
    if readonly && !path.exists() {
        return Ok(if operation == "receive" {
            json!({"messages":[]})
        } else {
            json!({"pending_count":0})
        });
    }
    if !readonly {
        std::fs::create_dir_all(&root)?;
    }
    let mut db = if readonly {
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        rusqlite::Connection::open(path)?
    };
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    if !readonly {
        db.execute_batch("CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY,sender TEXT NOT NULL,recipient TEXT NOT NULL,content TEXT NOT NULL,created_at TEXT NOT NULL,read INTEGER NOT NULL DEFAULT 0); CREATE INDEX IF NOT EXISTS messages_recipient ON messages(recipient,read);")?;
    }
    let tx = db.transaction_with_behavior(if readonly {
        rusqlite::TransactionBehavior::Deferred
    } else {
        rusqlite::TransactionBehavior::Immediate
    })?;
    let recipient = input["recipient"].as_str().unwrap_or("agent");
    let result = match operation {
        "send" => {
            let to = input["to"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .context("to is required")?;
            let from = input["from"].as_str().unwrap_or("agent");
            let content = input["content"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("content is required")?;
            if to.len() > 256 || from.len() > 256 || content.len() > 64 * 1024 {
                bail!("Message exceeds recipient/sender/content limits");
            }
            let id = uuid::Uuid::new_v4().to_string();
            let created = chrono::Utc::now().to_rfc3339();
            tx.execute("INSERT INTO messages(id,sender,recipient,content,created_at) VALUES(?1,?2,?3,?4,?5)",params![id,from,to,content,created])?;
            json!({"id":id,"from":from,"to":to,"content":content,"created_at":created,"read":false})
        }
        "receive" => {
            let unread = input["unread_only"].as_bool().unwrap_or(true);
            let mut statement=tx.prepare("SELECT id,sender,recipient,content,created_at,read FROM messages WHERE recipient=?1 AND (?2=0 OR read=0) ORDER BY rowid LIMIT 1000")?;
            let rows=statement.query_map(params![recipient,unread],|row|Ok(json!({"id":row.get::<_,String>(0)?,"from":row.get::<_,String>(1)?,"to":row.get::<_,String>(2)?,"content":row.get::<_,String>(3)?,"created_at":row.get::<_,String>(4)?,"read":row.get::<_,bool>(5)?})))?;
            json!({"messages":rows.collect::<rusqlite::Result<Vec<_>>>()?})
        }
        "pending_count" => {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM messages WHERE recipient=?1 AND read=0",
                [recipient],
                |row| row.get(0),
            )?;
            json!({"pending_count":count})
        }
        "mark_read" => {
            let id = input["message_id"]
                .as_str()
                .context("message_id is required")?;
            let exists: Option<String> = tx
                .query_row(
                    "SELECT id FROM messages WHERE id=?1 AND recipient=?2",
                    params![id, recipient],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_none() {
                bail!("Message not found in recipient mailbox");
            }
            tx.execute(
                "UPDATE messages SET read=1 WHERE id=?1 AND recipient=?2",
                params![id, recipient],
            )?;
            json!({"message_id":id,"read":true})
        }
        _ => unreachable!(),
    };
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn team_names_broadcast_requests_and_timeouts_use_live_inbox() {
        use crate::{runtime::SessionStore, services::session_messages as live, services::teams};
        std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let context = live::context(&data).unwrap();
        let store = SessionStore::new(context.db.clone());
        let lead = store.create(&workspace, "fixture", "fixture").unwrap();
        let alpha = store.fork(&lead).unwrap();
        let beta = store.fork(&lead).unwrap();
        let team = teams::create(&data, &lead, "squad", None, None, None, &workspace)
            .unwrap()
            .team;
        for (name, session) in [("alpha", &alpha), ("beta", &beta)] {
            let member = teams::register_member(
                &data, &team.name, &lead, name, None, None, "work", &workspace,
            )
            .unwrap();
            teams::bind_member_session(&data, &team.name, &member.agent_id, session).unwrap();
        }

        let sent = execute_live(
            &context,
            &lead,
            json!({"operation":"send","to":"alpha","content":"hello"}),
        )
        .unwrap();
        assert_eq!(sent["to"], alpha);
        assert!(live::inbox(&context, &alpha, false)
            .unwrap()
            .iter()
            .any(|message| message.content == "hello"));

        let broadcast = execute_live(
            &context,
            &lead,
            json!({"operation":"send","to":"*","content":"all hands"}),
        )
        .unwrap();
        assert_eq!(broadcast["recipients"].as_array().unwrap().len(), 2);
        assert!(live::inbox(&context, &alpha, false)
            .unwrap()
            .iter()
            .any(|message| message.content == "all hands"));
        assert!(live::inbox(&context, &beta, false)
            .unwrap()
            .iter()
            .any(|message| message.content == "all hands"));
        assert!(execute_live(
            &context,
            &lead,
            json!({"operation":"send","to":"ghost","content":"hello"})
        )
        .is_err());

        let request = execute_live(
            &context,
            &lead,
            json!({"operation":"shutdown_request","to":"alpha","reason":"wrap up"}),
        )
        .unwrap();
        let id = request["request_id"].as_str().unwrap().to_owned();
        assert!(execute_live(
            &context,
            &beta,
            json!({"operation":"shutdown_response","request_id":id,"approve":true})
        )
        .is_err());
        let rejected = execute_live(
            &context,
            &alpha,
            json!({"operation":"shutdown_response","request_id":id,"approve":"no","reason":"busy"}),
        )
        .unwrap();
        assert_eq!(rejected["state"], "rejected");
        assert!(live::inbox(&context, &lead, false)
            .unwrap()
            .iter()
            .any(|message| message.content.contains("shutdown_response")));

        let plan = teams::create_request(
            &data,
            &team.name,
            "plan_approval",
            &alpha,
            &lead,
            60_000,
            Some("alpha"),
            Some("# plan"),
        )
        .unwrap();
        let approved = execute_live(
            &context,
            &lead,
            json!({"operation":"plan_approval_response","request_id":plan.id,"approve":true,"feedback":"go"}),
        )
        .unwrap();
        assert_eq!(approved["state"], "approved");
        assert!(live::inbox(&context, &alpha, false)
            .unwrap()
            .iter()
            .any(|message| message.content.contains("plan_approval_response")));

        let slow = execute_live(
            &context,
            &lead,
            json!({"operation":"shutdown_request","to":"beta","timeout_ms":1}),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let timed_out = execute_live(
            &context,
            &beta,
            json!({"operation":"shutdown_response","request_id":slow["request_id"],"approve":true}),
        )
        .unwrap_err();
        assert!(timed_out.to_string().contains("timed out"));
        let status = execute_live(
            &context,
            &lead,
            json!({"operation":"request_status","request_id":slow["request_id"]}),
        )
        .unwrap();
        assert_eq!(status["request"]["state"], "timeout");
    }

    #[test]
    fn two_sessions_exchange_persistent_messages_and_ack_only_their_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let message = execute_for_session(
            json!({"operation":"send","from":"spoof","to":"child","content":"Inspect file"}),
            cwd,
            "parent",
        )
        .unwrap();
        assert_eq!(message["from"], "parent");
        let received = execute_for_session(
            json!({"operation":"receive","recipient":"parent"}),
            cwd,
            "child",
        )
        .unwrap();
        assert_eq!(received["messages"][0]["id"], message["id"]);
        assert!(execute_for_session(
            json!({"operation":"mark_read","message_id":message["id"]}),
            cwd,
            "parent"
        )
        .is_err());
        execute_for_session(
            json!({"operation":"mark_read","message_id":message["id"]}),
            cwd,
            "child",
        )
        .unwrap();
        assert_eq!(
            execute_for_session(json!({"operation":"pending_count"}), cwd, "child").unwrap()
                ["pending_count"],
            0
        );
        assert_eq!(
            execute_for_session(
                json!({"operation":"receive","unread_only":false}),
                cwd,
                "child"
            )
            .unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
