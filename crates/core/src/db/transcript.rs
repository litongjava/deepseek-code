//! The conversation detail and snapshot replacement contracts shared by hosts.

/// UUIDv8 message references carry a shard-specific check on their random nonce.
/// Snapshot writers can reject a reference from another shard without consulting it.
pub(crate) struct MessageIds(String);

impl MessageIds {
    pub(crate) fn for_shard(c: &rusqlite::Connection) -> anyhow::Result<Self> {
        Ok(Self(c.query_row(
            "SELECT incarnation FROM manifest WHERE singleton=1",
            [],
            |r| r.get(0),
        )?))
    }

    pub(crate) fn new(incarnation: &str) -> Self {
        Self(incarnation.to_owned())
    }

    fn check_bytes(&self, nonce: &[u8]) -> [u8; 6] {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(self.0.as_bytes());
        hash.update(nonce);
        hash.finalize()[..6]
            .try_into()
            .expect("six-byte message check")
    }

    pub(crate) fn issue(&self) -> String {
        let mut bytes = *uuid::Uuid::new_v4().as_bytes();
        bytes[0] = 0xdc;
        let mut bytes = uuid::Builder::from_custom_bytes(bytes)
            .into_uuid()
            .into_bytes();
        let check = self.check_bytes(&bytes[..10]);
        bytes[10..].copy_from_slice(&check);
        uuid::Uuid::from_bytes(bytes).to_string()
    }

    fn foreign_owner_key(&self, id: &str) -> Option<String> {
        let (prefix, raw) = id
            .strip_prefix("notice-")
            .map_or(("", id), |raw| ("notice-", raw));
        let uuid = uuid::Uuid::parse_str(raw).ok()?;
        let bytes = uuid.as_bytes();
        (uuid.get_version_num() == 8
            && bytes[0] == 0xdc
            && bytes[10..] != self.check_bytes(&bytes[..10]))
        .then(|| format!("{prefix}{}", uuid.simple()))
    }
}

pub fn enriched_messages(
    conn: &rusqlite::Connection,
    id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let messages = super::message_repo::get_messages_by_conversation(conn, id)?;
    messages
        .iter()
        .map(|msg| {
            let mut value = serde_json::to_value(msg)?;
            value["content"] = super::message_repo::user_content_for_display(conn, msg)?;
            value["requestSnapshots"] =
                super::message_repo::get_request_snapshots(conn, &msg.id)
                    .unwrap_or_else(|_| serde_json::json!([]));
            if let Some(turn) = crate::db::turn_repo::get_message_turn(conn, &msg.id)?
            {
                value["turn"] = serde_json::to_value(turn)?;
            }
            let tool_calls = super::message_repo::get_tool_calls_for_message(conn, &msg.id)?;
            if !tool_calls.is_empty() {
                value["tool_calls"] = serde_json::json!(tool_calls
                    .iter()
                    .map(|tc| {
                        let input = tc
                            .input
                            .as_ref()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .unwrap_or(serde_json::json!({}));
                        let mcp_result = crate::services::subagents::typed_mcp_result(
                            conn, id, &tc.id, tc.output.as_deref(), tc.is_error,
                        )?;
                        Ok::<_, anyhow::Error>(serde_json::json!({
                            "id": tc.id.strip_prefix(&format!("{}:", msg.id)).unwrap_or(&tc.id),
                            "name": tc.name,
                            "input": input,
                            "result": tc.output,
                            "mcp_result": mcp_result,
                            "status": if tc.is_error { "error" } else if tc.output.is_none() { "running" } else { "done" },
                        }))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?);
            }
            Ok(value)
        })
        .collect()
}

/// Apply a client snapshot inside the caller's write transaction. Retained rows
/// keep their trajectory, turn links and tool occurrences.
pub fn replace_messages(
    tx: &rusqlite::Connection,
    id: &str,
    messages: &[serde_json::Value],
) -> anyhow::Result<()> {
    let owner = MessageIds::for_shard(tx)?;
    let existing = super::message_repo::get_messages_by_conversation(tx, id)?;
    let mut retained = std::collections::HashSet::new();
    for (idx, msg) in messages.iter().enumerate() {
        let msg_id = msg
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| owner.issue());
        anyhow::ensure!(retained.insert(msg_id.clone()), "Duplicate message ID");
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let previous = super::message_repo::get_message(tx, &msg_id)?;
        let foreign_key = owner.foreign_owner_key(&msg_id);
        anyhow::ensure!(
            ["conversation_id", "conversationId"].into_iter().all(|key| {
                msg.get(key).and_then(|v| v.as_str()).is_none_or(|source| source == id)
            })
                && (previous.is_some() || foreign_key.is_none() || tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM removed_message_owners WHERE id=?1 AND role=?2)",
                    rusqlite::params![foreign_key,role], |r|r.get::<_,bool>(0))?),
            "Message ID belongs to a different conversation"
        );
        if let Some(previous) = &previous {
            anyhow::ensure!(
                previous.conversation_id == id && previous.role == role,
                "Message ID belongs to a different conversation or role"
            );
        }
        let now = chrono::Utc::now().to_rfc3339();
        let content = msg.get("content").cloned().unwrap_or(serde_json::json!(""));
        if role == "user" {
            let prior_content = previous
                .as_ref()
                .map(|row| super::message_repo::user_content_for_display(tx, row))
                .transpose()?;
            let (_, normalized) = super::message_repo::prepare_user_message(&content)?;
            super::message_repo::save_user_message(tx, &msg_id, id, &content, &now, idx as i64)?;
            if prior_content.as_ref() != Some(&normalized) {
                tx.execute(
                    "INSERT INTO message_metadata VALUES(?1,?2) ON CONFLICT(message_id)
                     DO UPDATE SET value=json_patch(message_metadata.value,excluded.value)",
                    [&msg_id, r#"{"promptOrigin":"transcript"}"#],
                )?;
            }
        } else {
            let content = content
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| content.to_string());
            if previous.is_some() {
                tx.execute(
                    "UPDATE messages SET content = ?1, sort_order = ?2 WHERE id = ?3",
                    rusqlite::params![content, idx as i64, msg_id],
                )?;
            } else {
                super::message_repo::insert_message(
                    tx, &msg_id, id, role, &content, None, &now, false, idx as i64,
                )?;
            }
        }
    }
    for message in existing {
        if !retained.contains(&message.id) {
            super::message_repo::delete_message(tx, &message.id)?;
        }
    }
    Ok(())
}
