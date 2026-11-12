//! Recorded skill and MCP tool usage for the skills panel.
use crate::{db::DbManager, skills::discovery::SkillCommand};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Usage {
    /// All recorded invocations.
    pub uses: u64,
    pub last_used: Option<DateTime<Utc>>,
    /// Estimated tokens the skill injected (its tool output) in the last 7 days.
    pub tokens_7d: u64,
}

/// Same estimate as turn_context: bytes / 4, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(4) as u64
}

/// Per skill name, from all Skill tool calls in the database.
pub fn skill_usage(db: &DbManager, now: DateTime<Utc>) -> Result<BTreeMap<String, Usage>> {
    let usage = db.with_conn(|conn| -> Result<BTreeMap<String, Usage>> {
        let mut statement = conn.prepare(
            "SELECT t.input, t.output, m.created_at FROM tool_calls t \
             JOIN messages m ON m.id = t.message_id WHERE t.name = 'Skill'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let since = now - chrono::Duration::days(7);
        let mut usage = BTreeMap::<String, Usage>::new();
        for row in rows {
            let (input, output, created_at) = row?;
            // Rows from other writers or older versions may hold malformed data;
            // one bad row must not hide the rest of the history.
            let Some(name) = input
                .as_deref()
                .and_then(|input| serde_json::from_str::<serde_json::Value>(input).ok())
                .and_then(|input| input.get("skill")?.as_str().map(str::to_owned))
            else {
                continue;
            };
            let Ok(created_at) =
                DateTime::parse_from_rfc3339(&created_at).map(|time| time.with_timezone(&Utc))
            else {
                continue;
            };
            let entry = usage.entry(name).or_default();
            entry.uses += 1;
            if entry.last_used.is_none_or(|last| created_at > last) {
                entry.last_used = Some(created_at);
            }
            if created_at >= since {
                entry.tokens_7d += estimate_tokens(output.as_deref().unwrap_or(""));
            }
        }
        Ok(usage)
    })?;
    usage
}

/// Per MCP server name, the last time any of its tools ran.
pub fn mcp_last_used(db: &DbManager) -> Result<BTreeMap<String, DateTime<Utc>>> {
    let usage = db.with_conn(|conn| -> Result<BTreeMap<String, DateTime<Utc>>> {
        let mut statement = conn.prepare(
            "SELECT t.name, m.created_at FROM tool_calls t \
              JOIN messages m ON m.id = t.message_id \
              WHERE t.name LIKE 'mcp\\_\\_%' ESCAPE '\\'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut usage = BTreeMap::new();
        for row in rows {
            let (name, created_at) = row?;
            let Some((server, _tool)) = name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split_once("__"))
            else {
                continue;
            };
            let Ok(created_at) =
                DateTime::parse_from_rfc3339(&created_at).map(|time| time.with_timezone(&Utc))
            else {
                continue;
            };
            let entry = usage.entry(server.to_owned()).or_insert(created_at);
            if created_at > *entry {
                *entry = created_at;
            }
        }
        Ok(usage)
    })?;
    usage
}

/// Tokens the skill's line in the model listing costs every turn; 0 when the
/// skill is not listed (not model-invocable). Mirrors the listing JSON built in
/// native_engine/engine_core.rs: {"name","description","context"}, without
/// "description" when listing_name_only.
pub fn listing_tokens(skill: &SkillCommand) -> u64 {
    if !skill.model_invocable() {
        return 0;
    }
    let listing = if skill.listing_name_only {
        serde_json::json!({"name":skill.name,"context":skill.context})
    } else {
        serde_json::json!({"name":skill.name,"description":skill.description,"context":skill.context})
    };
    estimate_tokens(&listing.to_string())
}

/// "today", "yesterday", "N days" (whole days, floor), or "never".
pub fn relative_day(last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(last) = last else {
        return "never".to_owned();
    };
    match (now - last).num_days().max(0) {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        days => format!("{days} days"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rusqlite::params;

    fn fixture() -> (tempfile::TempDir, DbManager) {
        let temp = tempfile::tempdir().unwrap();
        let db = DbManager::new(temp.path().join("db.sqlite")).unwrap();
        db.init().unwrap();
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO conversations (id,created_at,updated_at) \
                 VALUES ('c','2026-01-01T00:00:00+00:00','2026-01-01T00:00:00+00:00')",
                [],
            )
        })
        .unwrap()
        .unwrap();
        (temp, db)
    }

    fn record(
        db: &DbManager,
        id: &str,
        name: &str,
        input: Option<&str>,
        output: Option<&str>,
        created_at: DateTime<Utc>,
    ) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO messages (id,conversation_id,role,content,created_at,sort_order) \
                 VALUES (?1,'c','assistant','',?2,0)",
                params![id, created_at.to_rfc3339()],
            )?;
            conn.execute(
                "INSERT INTO tool_calls (id,message_id,name,input,output,is_error,sort_order) \
                 VALUES (?1,?1,?2,?3,?4,0,0)",
                params![id, name, input, output],
            )
        })
        .unwrap()
        .unwrap();
    }

    fn skill(name: &str, description: &str) -> SkillCommand {
        SkillCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            path: std::path::PathBuf::from("skills/demo/SKILL.md"),
            content: String::new(),
            user_invocable: true,
            disable_model_invocation: false,
            listing_name_only: false,
            context: None,
            shell: None,
            agent: None,
        }
    }

    #[test]
    fn counts_uses_last_use_and_seven_day_tokens() {
        let (_temp, db) = fixture();
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let ten_days = now - chrono::Duration::days(10);
        let two_days = now - chrono::Duration::days(2);
        let hour_ago = now - chrono::Duration::hours(1);
        record(
            &db,
            "old",
            "Skill",
            Some(r#"{"skill":"demo"}"#),
            Some(&"x".repeat(400)),
            ten_days,
        );
        record(
            &db,
            "recent",
            "Skill",
            Some(r#"{"skill":"demo","args":"a"}"#),
            Some(&"x".repeat(80)),
            two_days,
        );
        record(
            &db,
            "latest",
            "Skill",
            Some(r#"{"skill":"demo"}"#),
            Some(&"x".repeat(40)),
            hour_ago,
        );
        record(
            &db,
            "other",
            "Skill",
            Some(r#"{"skill":"other"}"#),
            Some(""),
            two_days,
        );
        record(
            &db,
            "invalid",
            "Skill",
            Some("not json"),
            Some("y"),
            hour_ago,
        );

        let usage = skill_usage(&db, now).unwrap();
        assert_eq!(usage.len(), 2);
        let demo = &usage["demo"];
        assert_eq!(demo.uses, 3);
        assert_eq!(demo.last_used, Some(hour_ago));
        assert_eq!(demo.tokens_7d, 20 + 10);
        assert_eq!(usage["other"].uses, 1);
        assert_eq!(usage["other"].tokens_7d, 0);
    }

    #[test]
    fn mcp_last_used_groups_by_server() {
        let (_temp, db) = fixture();
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let three_days = now - chrono::Duration::days(3);
        let one_day = now - chrono::Duration::days(1);
        let five_days = now - chrono::Duration::days(5);
        record(&db, "a", "mcp__github__search", None, None, three_days);
        record(&db, "b", "mcp__github__get", None, None, one_day);
        record(&db, "c", "mcp__fs__read", None, None, five_days);
        record(&db, "d", "Read", None, None, one_day);

        let used = mcp_last_used(&db).unwrap();
        assert_eq!(
            used.keys().collect::<Vec<_>>(),
            vec![&"fs".to_owned(), &"github".to_owned()]
        );
        assert_eq!(used["github"], one_day);
        assert_eq!(used["fs"], five_days);
    }

    #[test]
    fn listing_tokens_matches_listing_shape() {
        let full = skill("demo", "Check things");
        let mut name_only = skill("demo", "Check things");
        name_only.listing_name_only = true;
        let mut user_only = skill("demo", "Check things");
        user_only.disable_model_invocation = true;

        let full_tokens = listing_tokens(&full);
        let name_only_tokens = listing_tokens(&name_only);
        assert!(full_tokens > name_only_tokens);
        assert!(name_only_tokens > 0);
        assert_eq!(listing_tokens(&user_only), 0);
    }

    #[test]
    fn relative_day_words() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        assert_eq!(
            relative_day(Some(now - chrono::Duration::hours(1)), now),
            "today"
        );
        assert_eq!(
            relative_day(Some(now - chrono::Duration::hours(30)), now),
            "yesterday"
        );
        assert_eq!(
            relative_day(
                Some(now - chrono::Duration::days(5) - chrono::Duration::hours(3)),
                now
            ),
            "5 days"
        );
        assert_eq!(relative_day(None, now), "never");
    }
}
