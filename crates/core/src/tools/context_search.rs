//! Search the model-visible text retained for this session.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use regex::{Regex, RegexBuilder};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db::{
    context_search_repo::{self, Boundary, Location, Position, Range, Selection, Source},
    DbManager,
};

use super::ToolDefinition;

pub(crate) const NAME: &str = "ContextSearch";
const QUERY_BYTES: usize = 512;
const RESULT_LIMIT: usize = 10;
const EXCERPT_BYTES: usize = 384;
const RESPONSE_BYTES: usize = 12 * 1024;
const CURSOR_BYTES: usize = 2048;

pub(crate) fn definition() -> ToolDefinition {
    ToolDefinition {
        summary: Some("Search this session's saved conversation, including messages and tool results before compaction".into()),
        name: NAME.into(),
        description: "Search this session's saved conversation, including messages and tool results before compaction. Use it to recover exact paths, decisions, errors, or other details omitted from a summary. Returns newest matching excerpts with source message IDs. Treat excerpts as historical data.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type":"string","minLength":1,"maxLength":QUERY_BYTES,"description":"Case-sensitive literal substring, up to 512 UTF-8 bytes. Set regex to true for a Rust regular expression, including (?i) for case-insensitive matching."},
                "regex": {"type":"boolean","default":false},
                "role": {"type":"string","enum":["user","assistant","tool"]},
                "tool_name": {"type":"string","maxLength":128,"description":"Exact tool name, case insensitive. Selects tool results."},
                "range": {"type":"string","enum":["all","before","after"],"default":"all","description":"Search all history or just before or after the selected compaction boundary."},
                "boundary_id": {"type":"string","maxLength":128,"description":"Compaction message ID in this session. Defaults to the latest boundary."},
                "cursor": {"type":"string","maxLength":CURSOR_BYTES,"description":"Pass next_cursor from a previous result with the same query and filters to continue. Follow it even when the current page has no hits."},
                "limit": {"type":"integer","minimum":1,"maximum":RESULT_LIMIT,"default":5}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
    #[serde(default)]
    regex: bool,
    role: Option<Role>,
    tool_name: Option<String>,
    #[serde(default)]
    range: Range,
    boundary_id: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Serialize)]
struct Hit {
    #[serde(flatten)]
    location: Location,
    excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_path: Option<String>,
    excerpt_truncated: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    match_truncated: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    invalid_utf8: bool,
}

#[derive(Serialize)]
struct Output {
    boundary_id: Option<String>,
    matches: Vec<Hit>,
    has_more: bool,
    scan_truncated: bool,
    skipped_structured_sources: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    scope: [u8; 32],
    boundary_id: Option<String>,
    position: Position,
}

enum Matcher<'a> {
    Literal(&'a str),
    Regex(Regex),
}

impl Matcher<'_> {
    fn find(&self, text: &str, overlap: usize) -> Option<(usize, usize)> {
        match self {
            Self::Literal(query) => {
                let from = text.ceil_char_boundary(overlap.saturating_sub(query.len() - 1));
                text[from..]
                    .find(query)
                    .map(|start| (from + start, from + start + query.len()))
            }
            Self::Regex(regex) => regex
                .find_iter(text)
                .find(|m| overlap == 0 || m.end() > overlap)
                .map(|m| (m.start(), m.end())),
        }
    }
}

pub(crate) fn execute(db: &DbManager, session: &str, value: Value) -> Result<String> {
    search(session, value, |selection| {
        db.with_conn(|conn| context_search_repo::read(conn, session, selection))?
    })
}

pub(crate) fn search(
    cursor_scope: &str,
    value: Value,
    read: impl FnOnce(Selection<'_>) -> Result<context_search_repo::Transcript>,
) -> Result<String> {
    let input: Input = serde_json::from_value(value).context("Invalid ContextSearch input")?;
    let limit = input.limit.unwrap_or(5);
    ensure!(
        !input.query.trim().is_empty()
            && input.query.len() <= QUERY_BYTES
            && !input.query.contains('\0'),
        "ContextSearch requires a non-blank query of at most {QUERY_BYTES} bytes, without NULs"
    );
    ensure!(
        (1..=RESULT_LIMIT).contains(&limit),
        "ContextSearch limit must be 1–{RESULT_LIMIT}"
    );
    for (name, value) in [
        ("boundary_id", input.boundary_id.as_deref()),
        ("tool_name", input.tool_name.as_deref()),
    ] {
        ensure!(
            value.is_none_or(|s| !s.trim().is_empty() && s.len() <= 128),
            "Invalid ContextSearch {name}"
        );
    }
    ensure!(
        input.tool_name.is_none() || matches!(input.role, None | Some(Role::Tool)),
        "A tool_name filter requires role tool or an omitted role"
    );
    let matcher = if input.regex {
        Matcher::Regex(
            RegexBuilder::new(&input.query)
                .size_limit(256 * 1024)
                .dfa_size_limit(512 * 1024)
                .build()
                .map_err(|_| anyhow::anyhow!("Invalid or oversized ContextSearch regex"))?,
        )
    } else {
        Matcher::Literal(&input.query)
    };
    let role = input.role.map(|role| match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    });
    let scope: [u8; 32] = Sha256::digest(serde_json::to_vec(&(
        cursor_scope,
        &input.query,
        input.regex,
        role,
        &input.tool_name,
        input.range,
        &input.boundary_id,
    ))?)
    .into();
    let previous = input
        .cursor
        .as_deref()
        .map(|token| -> Result<Cursor> {
            ensure!(token.len() <= CURSOR_BYTES, "Invalid ContextSearch cursor");
            let encoded = token.strip_prefix("cs2.").context(
                "Invalid or outdated ContextSearch cursor. Repeat the query without a cursor.",
            )?;
            let payload = URL_SAFE_NO_PAD
                .decode(encoded)
                .context("Invalid ContextSearch cursor")?;
            let cursor: Cursor =
                serde_json::from_slice(&payload).context("Invalid ContextSearch cursor")?;
            ensure!(
                cursor.scope == scope,
                "ContextSearch cursor belongs to a different session or search"
            );
            Ok(cursor)
        })
        .transpose()?;
    let boundary = match (&previous, input.boundary_id.as_deref()) {
        (Some(cursor), _) => Boundary::Pinned(cursor.boundary_id.as_deref()),
        (None, Some(id)) => Boundary::Pinned(Some(id)),
        (None, None) => Boundary::Latest,
    };
    let transcript = read(Selection {
        range: input.range,
        boundary,
        role,
        tool_name: input.tool_name.as_deref(),
        position: previous.as_ref().map(|cursor| &cursor.position),
    })?;

    let candidates = match &matcher {
        Matcher::Literal(query) if query.chars().take(3).count() == 3 => {
            literal_candidates(&transcript.sources, query)?
        }
        _ => None,
    };
    let boundary_id = transcript.boundary_id;
    let cursor_for = |position| -> Result<String> {
        let cursor = Cursor {
            scope,
            boundary_id: boundary_id.clone(),
            position,
        };
        Ok(format!(
            "cs2.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?)
        ))
    };
    let mut output = Output {
        boundary_id: boundary_id.clone(),
        matches: Vec::new(),
        has_more: false,
        scan_truncated: transcript.next.is_some() || transcript.skipped_structured_sources > 0,
        skipped_structured_sources: transcript.skipped_structured_sources,
        next_cursor: transcript.next.map(cursor_for).transpose()?,
    };
    let mut positions = Vec::new();
    for (idx, source) in transcript.sources.into_iter().enumerate() {
        if candidates.as_ref().is_some_and(|rows| !rows[idx]) {
            continue;
        }
        let Some((start, end)) = matcher.find(&source.text, source.overlap) else {
            continue;
        };
        if output.matches.len() == limit {
            output.has_more = true;
            output.next_cursor = Some(cursor_for(source.position)?);
            break;
        }
        let (excerpt, excerpt_truncated) = excerpt(
            &source.text,
            start,
            end,
            source.preceded,
            source.truncated,
            source.recovery.as_ref(),
        );
        positions.push(source.position);
        output.matches.push(Hit {
            location: source.location,
            recovery_path: source.recovery.map(|r| r.path).filter(|p| !p.is_empty()),
            excerpt,
            excerpt_truncated,
            match_truncated: end - start > EXCERPT_BYTES,
            invalid_utf8: source.invalid_utf8,
        });
    }
    loop {
        let result = serde_json::to_string(&output)?;
        if result.len() <= RESPONSE_BYTES {
            return Ok(result);
        }
        ensure!(
            output.matches.len() > 1,
            "ContextSearch response metadata exceeded its byte limit"
        );
        output.matches.pop();
        output.next_cursor = positions.pop().map(cursor_for).transpose()?;
        output.has_more = true;
    }
}

/// A contentless, private index keeps neither a second copy of text nor a
/// database on disk. The original text verifies each candidate and supplies the
/// exact excerpt. FTS cannot reliably index a string containing NUL, so those
/// uncommon rows are sent directly to the substring check.
fn literal_candidates(sources: &[Source], query: &str) -> Result<Option<Vec<bool>>> {
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA temp_store=MEMORY;")?;
    let available: bool =
        conn.query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })?;
    if !available {
        return Ok(None);
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE transcript USING fts5(text, content='', tokenize='trigram case_sensitive 1');",
    )?;
    let mut candidates = vec![false; sources.len()];
    let tx = conn.transaction()?;
    {
        let mut insert = tx.prepare("INSERT INTO transcript(rowid,text) VALUES(?1,?2)")?;
        for (idx, source) in sources.iter().enumerate() {
            if source.text.contains('\0') {
                candidates[idx] = true;
            } else {
                insert.execute(params![idx + 1, source.text])?;
            }
        }
    }
    tx.commit()?;
    let phrase = format!("\"{}\"", query.replace('"', "\"\""));
    let mut stmt = conn.prepare("SELECT rowid FROM transcript WHERE transcript MATCH ?1")?;
    let mut rows = stmt.query([phrase])?;
    while let Some(row) = rows.next()? {
        candidates[row.get::<_, usize>(0)? - 1] = true;
    }
    Ok(Some(candidates))
}

fn excerpt(
    text: &str,
    start: usize,
    end: usize,
    preceded: bool,
    source_truncated: bool,
    recovery: Option<&crate::native_engine::tool_loop::Recovery>,
) -> (String, bool) {
    let before = EXCERPT_BYTES.saturating_sub(end - start).min(96);
    let left = text.ceil_char_boundary(start.saturating_sub(before));
    let right = text.floor_char_boundary(
        end.saturating_add(144)
            .min(left + EXCERPT_BYTES)
            .min(text.len()),
    );
    // A search excerpt may clip context, but a recovery reference must remain usable.
    let (mut left, mut right) = (left, right);
    if let Some(recovery) = recovery {
        for span in &recovery.spans {
            if span.start < right && span.end > left {
                left = left.min(span.start);
                right = right.max(span.end);
            }
        }
    }
    let prefix = if left > 0 || preceded { "…" } else { "" };
    let suffix = if right < text.len() || source_truncated {
        "…"
    } else {
        ""
    };
    (
        format!("{prefix}{}{suffix}", &text[left..right]),
        !prefix.is_empty() || !suffix.is_empty(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::message_repo,
        runtime::{RuntimePaths, SessionStore},
        services::compaction,
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        db: std::sync::Arc<crate::db::session_store::Stores>,
        store: SessionStore,
        session: String,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let db = RuntimePaths::new(temp.path().join("data"))
                .unwrap()
                .open_database()
                .unwrap();
            let store = SessionStore::new(db.clone());
            let session = store.create(temp.path(), "fixture", "fixture").unwrap();
            Self {
                _temp: temp,
                db,
                store,
                session,
            }
        }

        fn append(&self, role: &str, text: &str) -> String {
            self.store.append(&self.session, role, text).unwrap()
        }

        fn compact(&self) -> String {
            let prepared = compaction::prepare(&self.db, &self.session).unwrap();
            compaction::commit(&self.db, prepared, "Summary mentions needle")
                .unwrap()
                .message_id
        }

        fn tool(&self, message: &str, id: &str, name: &str, text: &str) {
            self.db
                .fixture_shard(&self.session, |c| {
                    message_repo::insert_tool_call(
                        c,
                        id,
                        message,
                        name,
                        Some(r#"{"password":"hidden-hook-input"}"#),
                        Some(text),
                        false,
                        0,
                    )
                })
                .unwrap()
                .unwrap();
        }

        fn search(&self, input: Value) -> Value {
            serde_json::from_str(&self.db.open_session(&self.session).unwrap().context_search(input).unwrap()).unwrap()
        }
    }

    #[test]
    fn base64_model_boundary_ordinary_persistence_matrix() {
        use crate::native_engine::provider_manager::ApiFormat;
        let f = Fixture::new();
        f.append("user", "inspect");
        let message = f.append("assistant", "");
        let raw = r#"{"stdout":"ordinary-marker!","stderr":"","exit_code":0}"#;
        for name in ["Bash", "TaskGet", "Monitor", "Other"] {
            f.tool(&message, name, name, raw);
        }
        let expected = f.db.fixture_shard(&f.session, |c| {
            let rows = message_repo::get_tool_calls_for_message(c, &message).unwrap();
            let nulls: usize = c.query_row("SELECT count(*) FROM tool_calls
                WHERE model_output IS NULL AND model_preview IS NULL AND model_recovery IS NULL AND model_search IS NULL",
                [], |r| r.get(0)).unwrap();
            assert_eq!(nulls, 4);
            rows.iter().map(|row| json!({"role":"tool","tool_call_id":row.id,"content":row.output})).collect::<Vec<_>>()
        }).unwrap();
        for legacy in [false, true] {
            if legacy {
                f.db.fixture_shard(&f.session, |c| {
                    c.execute("UPDATE tool_calls SET model_checked=0", [])
                })
                .unwrap()
                .unwrap();
            }
            for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
                let history = f.store.model_history(&f.session, &format).unwrap();
                let outputs: Vec<_> = match format {
                    ApiFormat::OpenAI => history
                        .iter()
                        .filter(|m| m["role"] == "tool")
                        .map(|m| &m["content"])
                        .collect(),
                    ApiFormat::Anthropic => history
                        .iter()
                        .flat_map(|m| m["content"].as_array().into_iter().flatten())
                        .filter(|b| b["type"] == "tool_result")
                        .map(|b| &b["content"])
                        .collect(),
                };
                assert_eq!(outputs, vec![&json!(raw); 4]);
            }
            let full = compaction::prepare(&f.db, &f.session).unwrap().transcript;
            let part =
                compaction::prepare_part(&f.db, &f.session, &message, compaction::Part::From)
                    .unwrap()
                    .transcript;
            for transcript in [&full, &part] {
                let entries: Vec<Value> = serde_json::from_str(transcript).unwrap();
                let results: Vec<_> = entries
                    .into_iter()
                    .filter(|m| m["role"] == "tool")
                    .collect();
                assert_eq!(results, expected);
            }
            let results = f.search(json!({"query":"ordinary-marker","role":"tool"}));
            let matches = results["matches"].as_array().unwrap();
            assert_eq!(matches.len(), 4);
            for hit in matches {
                assert_eq!(
                    hit["excerpt"],
                    if hit["tool_name"] == "Bash" {
                        "ordinary-marker!"
                    } else {
                        raw
                    }
                );
                assert!(hit.get("recovery_path").is_none());
            }
        }
    }

    #[test]
    fn base64_model_boundary_persists_typed_content_in_either_arrival_order() {
        let f = Fixture::new();
        let message = f.append("assistant", "");
        let payload = "QUJD".repeat(256);
        let transport = json!({"content":[{"type":"text","text":format!("header\n{payload}\n")},{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]});
        let shard = f.db.open_session(&f.session).unwrap();
        for (index, sidecar_first) in [true, false].into_iter().enumerate() {
            let occurrence = format!("typed-{index}");
            let id = format!("{message}:{occurrence}");
            let original =
                crate::native_engine::mcp_output::McpOutput::from_transport(&transport).unwrap();
            if !sidecar_first {
                f.tool(&message, &id, "Bash", &original.display());
            }
            let display = shard
                .save_mcp(&message, &occurrence, &transport, false)
                .unwrap();
            if sidecar_first {
                f.tool(&message, &id, "Bash", &display);
            }
            f.db.fixture_shard(&f.session,|conn|{
                let (raw,model):(String,String)=conn.query_row("SELECT content_json,model_content_json FROM mcp_tool_outputs WHERE tool_use_id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                let blocks:Value=serde_json::from_str(&model).unwrap();
                assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(),original.content);
                assert!(blocks[0]["text"].as_str().unwrap().contains("base64 data omitted"));
                assert!(!blocks[0]["text"].as_str().unwrap().contains(&payload));
                assert_eq!(blocks[1],original.content[1]);
                assert!(matches!(crate::db::model_results::provider_content(conn,&f.session,&id,"Bash",false).unwrap(),crate::db::model_results::StoredContent::Override(view) if view == blocks));
            }).unwrap();
            f.db.fixture_shard(&f.session, |conn| {
                conn.execute("UPDATE tool_calls SET model_checked=0,model_output=NULL,model_search=NULL,model_recovery=NULL WHERE id=?1", [&id]).unwrap();
                conn.execute("UPDATE mcp_tool_outputs SET model_content_json=NULL WHERE tool_use_id=?1", [&id]).unwrap();
            }).unwrap();
            use crate::native_engine::provider_manager::ApiFormat;
            for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
                let history = f.store.model_history(&f.session, &format).unwrap();
                let json = serde_json::to_string(&history).unwrap();
                assert!(
                    json.contains("iVBORw0KGgo=")
                        && json.contains("base64 data omitted")
                        && !json.contains(&payload),
                    "legacy {format:?} sidecar"
                );
            }
        }
    }

    #[test]
    fn base64_model_boundary_atomic_storage_and_legacy_reads() {
        let f = Fixture::new();
        f.append("user", "look up the result");
        let message = f.append("assistant", "");
        let payload = "QUJD".repeat(256);
        let raw = json!({"result":json!({"stdout":format!("header\n{payload}\n")}).to_string()})
            .to_string();
        f.tool(&message, "stored", "TaskGet", &raw);
        let read = || {
            f.store
                .model_history(
                    &f.session,
                    &crate::native_engine::provider_manager::ApiFormat::OpenAI,
                )
                .unwrap()
        };
        let view = read().into_iter().find(|m| m["role"] == "tool").unwrap()["content"].clone();
        assert!(view.as_str().unwrap().contains("base64: 1 block collapsed"));
        assert!(!view.as_str().unwrap().contains(&payload));
        let stored = || {
            f.db.fixture_shard(&f.session, |c| {
                c.query_row(
                    "SELECT output,model_output FROM tool_calls WHERE id='stored'",
                    [],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
            })
            .unwrap()
            .unwrap()
        };
        assert_eq!(stored(), (raw.clone(), Some(view.as_str().unwrap().into())));
        // Abort a replacement at the same SQL write. Both representations survive.
        f.db.fixture_shard(&f.session,|c|{
            c.execute_batch("CREATE TRIGGER reject_replacement BEFORE UPDATE OF output ON tool_calls BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
            assert!(message_repo::insert_tool_call(c,"stored",&message,"TaskGet",None,Some("replacement"),false,0).is_err());
            c.execute_batch("DROP TRIGGER reject_replacement").unwrap();
        }).unwrap();
        assert_eq!(stored().1.as_deref(), view.as_str());
        // A stored model field must win even if the original would produce different bytes.
        f.db.fixture_shard(&f.session,|c|c.execute("UPDATE tool_calls SET model_output='stored-model-sentinel',model_search='stored-model-sentinel' WHERE id='stored'",[])).unwrap().unwrap();
        assert!(read()
            .iter()
            .any(|m| m["content"] == "stored-model-sentinel"));
        assert!(compaction::prepare(&f.db, &f.session)
            .unwrap()
            .transcript
            .contains("stored-model-sentinel"));
        assert_eq!(
            f.search(json!({"query":"stored-model-sentinel"}))["matches"][0]["excerpt"],
            "stored-model-sentinel"
        );
        f.db.fixture_shard(&f.session,|c|c.execute("UPDATE tool_calls SET model_checked=0,model_output=NULL,model_preview=NULL,model_search=NULL,model_recovery=NULL WHERE id='stored'",[])).unwrap().unwrap();
        assert_eq!(
            read().into_iter().find(|m| m["role"] == "tool").unwrap()["content"],
            view
        );
        assert_eq!(
            f.search(json!({"query":"base64 data omitted"}))["matches"][0]["tool_call_id"],
            "stored"
        );
        assert_eq!(stored(), (raw, None));
    }

    #[test]
    fn base64_review_history_projects_results_and_hashes_original_rows() {
        let f = Fixture::new();
        f.append("user", "run both commands");
        let msg = f.append("assistant", "");
        let payload = "QUJD".repeat(64);
        for name in ["Bash", "TaskGet"] {
            let raw = json!({"metadata":{"managed_shell":true},"stdout":format!("data:text/plain;base64,{payload}")}).to_string();
            f.tool(&msg, name, name, &raw);
        }
        let prepared = compaction::prepare(&f.db, &f.session).unwrap();
        assert!(!prepared.transcript.contains(&payload));
        assert!(prepared.transcript.contains("hidden-hook-input"));
        assert_eq!(
            prepared
                .transcript
                .matches("base64: 1 block collapsed")
                .count(),
            2
        );
        for name in ["Bash", "TaskGet"] {
            let results = f.search(json!({"query":"data:text/plain;base64,", "tool_name":name}));
            assert!(results["matches"][0]["excerpt"]
                .as_str()
                .unwrap()
                .contains("base64 data omitted"));
            assert!(!results.to_string().contains("QUJD"));
            let exact = f.search(json!({"query":payload,"tool_name":name}));
            assert_eq!(exact["matches"], json!([]));
        }
        f.db.fixture_shard(&f.session, |c| {
            let raw: String = c
                .query_row("SELECT output FROM tool_calls WHERE id='Bash'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(raw.contains(&payload));
            c.execute(
                "UPDATE tool_calls SET output=? WHERE id='Bash'",
                [raw.replace("QUJD", "QUJE")],
            )
            .unwrap();
        })
        .unwrap();
        assert_ne!(
            prepared.fingerprint,
            compaction::prepare(&f.db, &f.session).unwrap().fingerprint
        );
    }

    #[test]
    fn base64_model_boundary_stored_page_compaction_and_search() {
        let f = Fixture::new();
        f.append("user", "retrieve an old page");
        let message = f.append("assistant", "");
        let payload = "QUJD".repeat(256);
        let page =
            json!({"output_mode":"page","stdout":format!("page-marker!{payload}!")}).to_string();
        f.tool(&message, "old-page", "TaskGet", &page);
        let compact = || compaction::prepare(&f.db, &f.session).unwrap().transcript;
        let transcript = compact();
        assert_eq!(transcript, compact());
        let entries: Vec<Value> = serde_json::from_str(&transcript).unwrap();
        let result = entries
            .iter()
            .find(|m| m["tool_call_id"] == "old-page")
            .unwrap();
        let view = result["content"].as_str().unwrap();
        assert!(!view.contains(&payload));
        let reference = view
            .split_once("; full text: ")
            .unwrap()
            .1
            .split_once(']')
            .unwrap()
            .0;
        let (file, line) = reference.rsplit_once(", line ").unwrap();
        assert_eq!(line, "1");
        assert_eq!(std::fs::read_to_string(file).unwrap(), page);
        let part =
            compaction::prepare_part(&f.db, &f.session, &message, compaction::Part::From).unwrap();
        let entries: Vec<Value> = serde_json::from_str(&part.transcript).unwrap();
        let result = entries
            .iter()
            .find(|m| m["tool_call_id"] == "old-page")
            .unwrap();
        assert_eq!(result["content"], view);
        let result = f.search(json!({"query":"page-marker","tool_name":"TaskGet"}));
        assert!(result["matches"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains(reference));
        assert!(
            f.search(json!({"query":"QUJD","tool_name":"TaskGet"}))["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let notice = crate::tools::monitor::Event {
            task_id: "old-monitor".into(),
            description: "legacy".into(),
            kind: "output".into(),
            text: format!("notification-marker!{payload}!"),
        }
        .notification();
        f.append("user", &notice);
        let entries: Vec<Value> = serde_json::from_str(&compact()).unwrap();
        let projected = entries.last().unwrap()["content"].as_str().unwrap();
        assert!(!projected.contains(&payload));
        let reference = projected
            .split_once("; full text: ")
            .unwrap()
            .1
            .split_once(']')
            .unwrap()
            .0;
        let (file, line) = reference.rsplit_once(", line ").unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), notice);
        assert!(notice
            .lines()
            .nth(line.parse::<usize>().unwrap() - 1)
            .unwrap()
            .contains(&payload));
        let hit = f.search(json!({"query":"<event>notification-marker","role":"user"}));
        assert!(hit["matches"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains(reference));
        let large = format!(
            "large-task-begin!{}!large-task-end",
            ".".repeat(8 * 1024 * 1024)
        );
        f.tool(&message, "ordinary", "TaskGet", &large);
        let hit = f.search(json!({"query":"large-task-begin","tool_name":"TaskGet"}));
        assert_eq!(hit["skipped_structured_sources"], 0);
        assert_eq!(hit["matches"][0]["tool_call_id"], "ordinary");
    }

    #[test]
    fn context_search_compaction_ranges_roles_and_location() {
        let f = Fixture::new();
        let old = f.append("user", "exact path /old/needle.txt");
        let assistant = f.append("assistant", "investigating needle");
        f.tool(
            &assistant,
            "old-tool",
            "Bash",
            r#"{"stderr":"needle: file missing","exit_code":1}"#,
        );
        let turn = f.db.start_turn_timer(&f.session).unwrap();
        f.db.fixture_shard(&f.session, |c| {
            crate::db::turn_repo::link_message(c, &turn.record().turn_id, &assistant)
        })
        .unwrap()
        .unwrap();

        let first_boundary = f.compact();
        let middle = f.append("user", "needle middle");
        f.append("assistant", "middle reply");
        let latest = f.compact();
        let recent = f.append("user", "needle recent");

        let all = f.search(json!({"query":"needle","limit":10}));
        let ids: Vec<_> = all["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["message_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [&recent, &middle, &assistant, &assistant, &old]);
        assert_eq!(all["boundary_id"], latest);
        assert_eq!(f.search(json!({"query":"needle","range":"before","boundary_id":first_boundary,"role":"user"}))["matches"][0]["message_id"], old);
        let after = f.search(json!({"query":"needle","range":"after"}));
        assert_eq!(after["matches"].as_array().unwrap().len(), 1);
        assert_eq!(after["matches"][0]["message_id"], recent);
        let tool = f.search(
            json!({"query":"needle: [a-z ]+","regex":true,"tool_name":"bash","range":"before"}),
        );
        let hit = &tool["matches"][0];
        assert_eq!(hit["message_id"], assistant);
        assert_eq!(hit["tool_call_id"], "old-tool");
        assert_eq!(hit["tool_name"], "Bash");
        assert_eq!(hit["role"], "tool");
        assert_eq!(hit["turn_id"], turn.record().turn_id);
        assert!(chrono::DateTime::parse_from_rfc3339(hit["timestamp"].as_str().unwrap()).is_ok());
        assert!(hit["excerpt"]
            .as_str()
            .unwrap()
            .contains("needle: file missing"));

        let first = f.search(json!({"query":"needle","range":"before","role":"user","limit":1}));
        assert_eq!(first["matches"][0]["message_id"], middle);
        f.append("assistant", "recent reply");
        assert_ne!(f.compact(), latest);
        let continued = f.search(json!({
            "query":"needle","range":"before","role":"user","cursor":first["next_cursor"]
        }));
        assert_eq!(continued["boundary_id"], latest);
        assert_eq!(continued["matches"][0]["message_id"], old);
    }

    #[test]
    fn context_search_isolates_sessions_and_omits_private_sidecars() {
        let f = Fixture::new();
        let other = f
            .store
            .create(f._temp.path(), "fixture", "fixture")
            .unwrap();
        f.store
            .append(&other, "user", "scope-token OTHER SESSION")
            .unwrap();
        f.store
            .append(&other, "assistant", "other assistant")
            .unwrap();
        let foreign_boundary = compaction::commit(
            &f.db,
            compaction::prepare(&f.db, &other).unwrap(),
            "scope-token foreign summary",
        )
        .unwrap()
        .message_id;

        let own = f.append("user", "scope-token OWN SESSION");
        let assistant = f.append("assistant", "visible text");
        f.tool(&assistant, "nested", "Bash", "scope-token NESTED");
        f.tool(
            &assistant,
            "image",
            "Read",
            r#"{"content":"image display","image":{"data":"scope-token IMAGE"}}"#,
        );
        f.tool(
            &assistant,
            "edit",
            "Edit",
            r#"{"result":"edited","file_change":"scope-token DIFF"}"#,
        );
        f.tool(&assistant, "repeat", NAME, "scope-token REPEATED SEARCH");
        f.db.fixture_shard(&f.session, |c| {
            c.execute(
                "UPDATE messages SET thinking='scope-token REASONING' WHERE id=?1",
                [&assistant],
            )
            .unwrap();
            message_repo::save_request_snapshots(
                c,
                &assistant,
                &json!([{"systemPrompt":"scope-token TRAJECTORY"}]),
            )
            .unwrap();
            message_repo::save_tool_origin(c, "nested", "script", 0).unwrap();
        })
        .unwrap();
        let hit = f.search(json!({"query":"scope-token"}));
        assert_eq!(hit["matches"].as_array().unwrap().len(), 1);
        assert_eq!(hit["matches"][0]["message_id"], own);
        assert_eq!(
            f.search(json!({"query":"hidden-hook-input"}))["matches"],
            json!([])
        );
        assert!(f.db.open_session(&f.session).unwrap().context_search(json!({"query":"scope-token","session_id":other})
        )
        .is_err());
        assert!(f.db.open_session(&f.session).unwrap().context_search(json!({"query":"scope-token","boundary_id":foreign_boundary,"range":"before"})
        )
        .is_err());
        assert_eq!(
            f.search(json!({"query":"scope-token","range":"before"}))["matches"],
            json!([])
        );
    }

    #[test]
    fn context_search_handles_large_and_invalid_utf8_tool_output() {
        let f = Fixture::new();
        let assistant = f.append("assistant", "tool result follows");
        f.tool(&assistant, "large", "FixtureTool", "initial");
        let mut bytes = vec![b'a'; 1_100_000];
        bytes.extend_from_slice(b"\xff needle \xfe");
        bytes.extend(std::iter::repeat_n(b'z', 1_100_000));
        f.db.fixture_shard(&f.session, |c| {
            c.execute("UPDATE tool_calls SET output=?1 WHERE id='large'", [bytes])
                .unwrap();
        })
        .unwrap();

        let value = f.search(json!({"query":"needle","role":"tool"}));
        let hit = &value["matches"][0];
        let text = hit["excerpt"].as_str().unwrap();
        assert!(text.starts_with('…') && text.ends_with('…'));
        assert!(text.contains("� needle �"), "{text}");
        assert!(text.len() <= EXCERPT_BYTES + "……".len());
        assert_eq!(hit["excerpt_truncated"], true);
        assert_eq!(hit["invalid_utf8"], true);
        assert_eq!(value["scan_truncated"], false);

        let mut oversized = "prefix-token ".to_owned();
        oversized.push_str(&"é".repeat(4 * 1024 * 1024));
        oversized.push_str(" beyond-cap-token");
        f.tool(&assistant, "oversized", "FixtureTool", &oversized);
        let capped = f.search(json!({"query":"prefix-token","regex":true,"role":"tool"}));
        assert_eq!(capped["scan_truncated"], true);
        assert_eq!(capped["matches"][0]["excerpt_truncated"], true);
        assert!(capped["matches"][0].get("invalid_utf8").is_none());
        let beyond = f.search(json!({"query":"beyond-cap-token","regex":true,"role":"tool"}));
        assert_eq!(beyond["matches"], json!([]));
        assert_eq!(beyond["scan_truncated"], true);
        let resumed = f.search(json!({
            "query":"beyond-cap-token","regex":true,"role":"tool","cursor":beyond["next_cursor"]
        }));
        assert!(resumed["matches"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("beyond-cap-token"));
        assert!(resumed.get("next_cursor").is_none());
    }

    #[test]
    fn context_search_resumes_split_literal_without_reading_raw_structured_chunks() {
        let f = Fixture::new();
        let assistant = f.append("assistant", "tool results follow");
        let marker = "xyz中-needle";
        let large = "a".repeat(context_search_repo::MAX_CHUNK_BYTES - 5) + marker;
        f.tool(&assistant, "large", "FixtureTool", &large);
        let image =
            json!({"image":{"data":"z".repeat(context_search_repo::MAX_CHUNK_BYTES) + marker}});
        f.tool(&assistant, "oversized-image", "Read", &image.to_string());
        let first = f.search(json!({"query":marker,"role":"tool"}));
        assert_eq!(first["matches"], json!([]));
        assert_eq!(first["skipped_structured_sources"], 1);
        assert_eq!(first["scan_truncated"], true);
        let resumed = f.search(json!({"query":marker,"role":"tool","cursor":first["next_cursor"]}));
        let hits = resumed["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["tool_call_id"], "large");
        assert!(hits[0]["excerpt"].as_str().unwrap().contains(marker));
        assert!(hits[0].get("invalid_utf8").is_none());
        assert!(resumed.get("next_cursor").is_none());
    }

    #[test]
    fn context_search_resumes_past_the_metadata_limit_and_binds_the_cursor() {
        let f = Fixture::new();
        let old = f.append("user", "older needle");
        f.db.fixture_shard(&f.session, |c| {
            c.execute(
                "WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n<?2)
             INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order)
             SELECT 'filler-' || n, ?1, 'user', 'filler', 'now', 100 FROM rows",
                params![f.session, context_search_repo::MAX_ROWS + 1],
            )
        })
        .unwrap()
        .unwrap();
        let first = f.search(json!({"query":"needle","role":"user"}));
        assert_eq!(first["matches"], json!([]));
        assert_eq!(first["scan_truncated"], true);
        let cursor = &first["next_cursor"];
        let found = f.search(json!({"query":"needle","role":"user","cursor":cursor}));
        assert_eq!(found["matches"][0]["message_id"], old);
        assert!(found.get("next_cursor").is_none());
        assert!(f.db.open_session(&f.session).unwrap().context_search(json!({"query":"changed","role":"user","cursor":cursor})
        )
        .is_err());
        let other = f
            .store
            .create(f._temp.path(), "fixture", "fixture")
            .unwrap();
        assert!(f.db.open_session(&other).unwrap().context_search(json!({"query":"needle","role":"user","cursor":cursor})
        )
        .is_err());
    }

    #[test]
    fn context_search_stops_blob_reads_at_the_total_budget_and_resumes() {
        let f = Fixture::new();
        let old = f.append("user", "after-budget-needle");
        let text = "a".repeat(context_search_repo::MAX_SCAN_BYTES / 4);
        for _ in 0..4 {
            f.append("user", &text);
        }
        let page =
            f.db.fixture_shard(&f.session, |conn| {
                context_search_repo::read(
                    conn,
                    &f.session,
                    Selection {
                        range: Range::All,
                        boundary: Boundary::Latest,
                        role: Some("user"),
                        tool_name: None,
                        position: None,
                    },
                )
            })
            .unwrap()
            .unwrap();
        assert!(page.bytes_read > 0 && page.bytes_read <= context_search_repo::MAX_SCAN_BYTES);
        assert!(!page.sources.is_empty());
        assert!(page.next.is_some());
        drop(page);
        let first = f.search(json!({"query":"after-budget-needle","regex":true,"role":"user"}));
        assert_eq!(first["matches"], json!([]));
        let mut page = first;
        for _ in 0..10 {
            let cursor = page
                .get("next_cursor")
                .expect("older fact must remain reachable");
            page = f.search(json!({
                "query":"after-budget-needle","regex":true,"role":"user","cursor":cursor
            }));
            if !page["matches"].as_array().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(page["matches"][0]["message_id"], old);
    }

    #[test]
    fn context_search_keeps_a_fitting_utf8_match_before_surrounding_text() {
        let f = Fixture::new();
        let query = "长".repeat(100) + ".txt";
        let text = "😀".repeat(40) + &query + &"界".repeat(100);
        f.append("user", &text);
        let result = f.search(json!({"query":query}));
        let hit = &result["matches"][0];
        let excerpt = hit["excerpt"].as_str().unwrap();
        assert!(excerpt.contains(&query), "{excerpt}");
        assert!(excerpt.len() <= EXCERPT_BYTES + "……".len());
        assert!(!excerpt.contains('�'));
        assert!(hit.get("match_truncated").is_none());
        let long = "字".repeat(150);
        f.append("user", &long);
        let result = f.search(json!({"query":long}));
        assert_eq!(result["matches"][0]["match_truncated"], true);
        assert_eq!(result["matches"][0]["excerpt"], "字".repeat(128) + "…");
    }

    #[test]
    fn context_search_validates_inputs_and_caps_results() {
        let f = Fixture::new();
        for input in [
            json!({"query":" "}),
            json!({"query":"x".repeat(513)}),
            json!({"query":"x\u{0}y"}),
            json!({"query":"[" ,"regex":true}),
            json!({"query":"x","limit":0}),
            json!({"query":"x","limit":11}),
            json!({"query":"x","role":"user","tool_name":"Bash"}),
        ] {
            assert!(
                f.db.open_session(&f.session).unwrap().context_search(input.clone()).is_err(),
                "{input}"
            );
        }
        for _ in 0..12 {
            f.append(
                "user",
                &format!("{}xx{}", "\u{1}".repeat(100), "\u{1}".repeat(150)),
            );
        }
        let found = f.search(json!({"query":"xx"}));
        assert_eq!(found["matches"].as_array().unwrap().len(), 5);
        assert_eq!(found["has_more"], true);
        let mut page = found;
        let mut delivered = std::collections::HashSet::new();
        loop {
            for hit in page["matches"].as_array().unwrap() {
                assert!(delivered.insert(hit["message_id"].as_str().unwrap().to_owned()));
            }
            let Some(cursor) = page.get("next_cursor") else {
                break;
            };
            page = f.search(json!({"query":"xx","cursor":cursor}));
        }
        assert_eq!(delivered.len(), 12);
        let output = f.db.open_session(&f.session).unwrap().context_search(json!({"query":"xx","limit":10})).unwrap();
        assert!(output.len() <= RESPONSE_BYTES);
        let output: Value = serde_json::from_str(&output).unwrap();
        assert!(output["matches"].as_array().unwrap().len() < 10);
        assert_eq!(output["has_more"], true);
    }

    #[test]
    fn context_search_uses_bundled_fts_for_literal_paths_and_unicode() {
        let f = Fixture::new();
        let first = f.append("user", "literal /src/\"你好.rs:14 and \0tail");
        f.append("user", "literal /src/\"你好.RS:14");
        // Prove the bundled build actually exercises the FTS candidate path.
        let transcript =
            f.db.fixture_shard(&f.session, |conn| {
                context_search_repo::read(
                    conn,
                    &f.session,
                    Selection {
                        range: Range::All,
                        boundary: Boundary::Latest,
                        role: None,
                        tool_name: None,
                        position: None,
                    },
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            literal_candidates(&transcript.sources, "/src/\"你好.rs:14").unwrap(),
            Some(vec![false, true])
        );
        assert_eq!(
            f.search(json!({"query":"/src/\"你好.rs:14"}))["matches"][0]["message_id"],
            first
        );
        assert_eq!(
            f.search(json!({"query":"tail"}))["matches"][0]["message_id"],
            first
        );
        assert_eq!(
            f.search(json!({"query":"(?i)你好\\.rs","regex":true}))["matches"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn context_search_finds_oldest_fact_in_million_token_fixture() {
        let f = Fixture::new();
        let oldest = f.append("user", "Underflow sentinel: /archive/x-rare.txt");
        // One million space-delimited words, about 6 MiB of transcript.
        let chunk = "alpha bravo charlie delta echo foxtrot golf hotel ".repeat(1024);
        for n in 0..128 {
            f.append(if n % 2 == 0 { "user" } else { "assistant" }, &chunk);
        }
        let started = std::time::Instant::now();
        let value = f.search(json!({"query":"/archive/x-rare.txt"}));
        println!(
            "ContextSearch literal across 1,048,576 words ({} bytes): {:?}",
            128 * chunk.len(),
            started.elapsed()
        );
        assert_eq!(value["matches"][0]["message_id"], oldest);
        assert_eq!(value["scan_truncated"], false);
    }
}
