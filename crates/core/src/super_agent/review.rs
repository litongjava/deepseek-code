//! Reviewer-only task evidence: the review request and scoped session queries.
//!
//! A Reviewer may inspect the current task's Coder session tree and the task's
//! own artifacts. Authorization is checked from the durable task record at call
//! time; a session ID supplied by the model never widens its own scope.
use crate::db::{conversation_repo, message_repo};
use crate::services::{feedback, subagents};
use crate::super_agent::isolation::{SnapshotKind, SnapshotManifest};
use crate::super_agent::model::{
    NonGitSubmission, ReportKind, ReviewStage, TaskRecord, TaskReviewReport,
};
use crate::super_agent::role::{self, MemberRole};
use crate::super_agent::store::{StoreError, TaskStore};
use crate::tools::ToolDefinition;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024;

pub fn definitions() -> Vec<ToolDefinition> {
    let page = |description: &str, extra: Value, required: &[&str]| {
        let mut properties = json!({
            "session_id":{"type":"string"},
            "cursor":{"type":"string","description":"Opaque cursor from the previous page"},
            "limit":{"type":"integer","minimum":1,"maximum":MAX_LIMIT}
        });
        if let (Some(properties), Some(extra)) =
            (properties.as_object_mut(), extra.as_object())
        {
            for (key, value) in extra {
                properties.insert(key.clone(), value.clone());
            }
        }
        ToolDefinition {
            name: description.into(),
            input_schema: json!({
                "type":"object",
                "properties":properties,
                "required":required,
                "additionalProperties":false
            }),
            description: format!("{description}: bounded, redacted task evidence for this Reviewer"),
        }
    };
    vec![
        ToolDefinition {
            name: "SessionMetadata".into(),
            description: "Read metadata for a Coder session in this task's session tree. Returns no transcript.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{"session_id":{"type":"string"}},
                "required":["session_id"],
                "additionalProperties":false
            }),
        },
        page("SessionMessages", json!({}), &["session_id"]),
        page("SessionEvents", json!({"type":{"type":"string"}}), &["session_id"]),
        ToolDefinition {
            name: "SessionChildren".into(),
            description: "List the current and previous Coder sessions plus recorded child sessions descending from them.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{"session_id":{"type":"string"}},
                "required":["session_id"],
                "additionalProperties":false
            }),
        },
        ToolDefinition {
            name: "ArtifactRead".into(),
            description: "Read one report body or one stored artifact of the current task (maximum 64 KiB). Paths and cross-task artifacts are rejected.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{"artifact_id":{"type":"string"}},
                "required":["artifact_id"],
                "additionalProperties":false
            }),
        },
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "SessionMetadata" | "SessionMessages" | "SessionEvents" | "SessionChildren" | "ArtifactRead"
    )
}

fn store_error(error: StoreError) -> anyhow::Error {
    match error {
        StoreError::Invalid(message) => anyhow::anyhow!(message),
        other => anyhow::Error::new(other),
    }
}

/// The task this Reviewer currently belongs to, verified from durable records.
pub fn reviewer_task(runtime: &crate::super_agent::crp::ToolRuntime) -> Result<TaskRecord> {
    let (task_id, member) = role::membership(&runtime.db, &runtime.session_id)?
        .context("This session is not a current Coder or Reviewer of any task")?;
    ensure!(
        member == MemberRole::Reviewer,
        "Session evidence tools are only available to the task Reviewer"
    );
    runtime
        .store()
        .get_task(&task_id)
        .map_err(store_error)?
        .with_context(|| format!("Task not found: {task_id}"))
}

fn coder_sessions(store: &TaskStore, task: &TaskRecord) -> Result<BTreeSet<String>> {
    let mut sessions = BTreeSet::new();
    if let Some(session) = &task.coder.session_id {
        sessions.insert(session.clone());
    }
    sessions.extend(task.coder.previous_session_ids.iter().cloned());
    let db = store.db();
    let mut queue: VecDeque<String> = sessions.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    while let Some(parent) = queue.pop_front() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        let invocations =
            db.with_conn(|conn| -> Result<Vec<subagents::Invocation>> {
                subagents::list_invocations(conn, &parent, None)
            })??;
        for invocation in invocations {
            if sessions.insert(invocation.child_session_id.clone()) {
                queue.push_back(invocation.child_session_id);
            }
        }
    }
    Ok(sessions)
}

fn authorize_session(store: &TaskStore, task: &TaskRecord, session: &str) -> Result<()> {
    ensure!(
        session.len() <= 128 && !session.is_empty(),
        "A session ID is required"
    );
    ensure!(
        coder_sessions(store, task)?.contains(session),
        "Session {session} is not part of this task's Coder session tree"
    );
    Ok(())
}

fn limit(input: &Value) -> Result<usize> {
    let limit = input["limit"].as_u64().unwrap_or(DEFAULT_LIMIT as u64) as usize;
    ensure!(
        (1..=MAX_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_LIMIT}"
    );
    Ok(limit)
}

fn cursor(input: &Value) -> Result<u64> {
    match input.get("cursor") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(text)) if text.is_empty() => Ok(0),
        Some(Value::String(text)) => text
            .parse::<u64>()
            .with_context(|| format!("Invalid cursor: {text}")),
        Some(_) => anyhow::bail!("cursor must be a string"),
    }
}

fn redact_text(text: &str) -> String {
    let secrets: Vec<String> = [
        "DEEPSEEK_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_CODE_CHILD_API_KEY",
    ]
    .iter()
    .filter_map(|name| std::env::var(name).ok())
    .filter(|value| !value.is_empty())
    .collect();
    feedback::redact(text, &secrets)
}

fn redact_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_text(text),
        Value::Array(values) => values.iter_mut().for_each(redact_value),
        Value::Object(values) => values.values_mut().for_each(redact_value),
        _ => {}
    }
}

fn session_metadata(store: &TaskStore, task: &TaskRecord, session: &str) -> Result<Value> {
    authorize_session(store, task, session)?;
    let row = store
        .db()
        .with_conn(|conn| -> Result<Option<conversation_repo::ConversationRow>> {
            conversation_repo::get_conversation(conn, session)
        })??
        .with_context(|| format!("Session {session} not found"))?;
    let counts = store.db().with_conn(|conn| -> Result<(i64, i64)> {
        let messages = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            [session],
            |row| row.get(0),
        )?;
        let calls = conn.query_row(
            "SELECT COUNT(*) FROM tool_calls c JOIN messages m ON m.id=c.message_id WHERE m.conversation_id=?1",
            [session],
            |row| row.get(0),
        )?;
        Ok((messages, calls))
    })??;
    Ok(json!({
        "session_id": session,
        "title": row.title,
        "model": row.model,
        "provider": row.provider,
        "workspace_path": row.workspace_path,
        "archived": row.archived,
        "workspace_mode": row.workspace_mode,
        "message_count": counts.0,
        "tool_call_count": counts.1,
        "created_at": row.created_at,
        "updated_at": row.updated_at
    }))
}

fn session_messages(
    store: &TaskStore,
    task: &TaskRecord,
    input: &Value,
) -> Result<Value> {
    let session = input["session_id"]
        .as_str()
        .context("session_id is required")?;
    authorize_session(store, task, session)?;
    let cursor = cursor(input)?;
    let limit = limit(input)?;
    let messages = store.db().with_conn(|conn| -> Result<Vec<message_repo::MessageRow>> {
        let mut statement = conn.prepare(
            "SELECT id,conversation_id,role,content,thinking,created_at,is_compact_boundary,sort_order \
             FROM messages WHERE conversation_id=?1 AND sort_order>=?2 ORDER BY sort_order LIMIT ?3",
        )?;
        let rows = statement.query_map(
            rusqlite::params![session, cursor as i64, (limit + 1) as i64],
            |row| {
                Ok(message_repo::MessageRow {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    thinking: row.get(4)?,
                    created_at: row.get(5)?,
                    is_compact_boundary: row.get::<_, i64>(6)? != 0,
                    sort_order: row.get(7)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })??;
    let has_more = messages.len() > limit;
    let selected: Vec<Value> = messages
        .into_iter()
        .take(limit)
        .map(|message| {
            let calls = store
                .db()
                .with_conn(|conn| message_repo::get_tool_calls_for_message(conn, &message.id))
                .ok()
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            json!({
                "id": message.id,
                "role": message.role,
                "content": redact_text(&message.content),
                "thinking": message.thinking.as_deref().map(redact_text),
                "created_at": message.created_at,
                "compact_boundary": message.is_compact_boundary,
                "sort_order": message.sort_order,
                "tool_calls": calls.into_iter().map(|call| json!({
                    "id": call.id,
                    "name": call.name,
                    "input": call.input.as_deref().map(redact_text),
                    "output": call.output.as_deref().map(redact_text),
                    "is_error": call.is_error
                })).collect::<Vec<_>>()
            })
        })
        .collect();
    let next = selected
        .last()
        .and_then(|value| value["sort_order"].as_i64())
        .map(|value| (value + 1).to_string());
    Ok(json!({
        "session_id": session,
        "messages": selected,
        "next_cursor": if has_more { next } else { None }
    }))
}

fn session_events(store: &TaskStore, task: &TaskRecord, input: &Value) -> Result<Value> {
    let session = input["session_id"]
        .as_str()
        .context("session_id is required")?;
    authorize_session(store, task, session)?;
    let cursor = cursor(input)?;
    let limit = limit(input)?;
    let filter = match input["type"].as_str() {
        Some(value) => Some(
            crate::super_agent::EventType::parse(&value.to_ascii_uppercase())
                .with_context(|| format!("Unknown event type: {value}"))?,
        ),
        None => None,
    };
    let events = store.events(&task.task_id).map_err(store_error)?;
    let mut selected = Vec::new();
    for event in events {
        if event.sequence <= cursor || filter.is_some_and(|kind| kind != event.event_type) {
            continue;
        }
        if !event_belongs_to(&event, session) {
            continue;
        }
        selected.push(event);
        if selected.len() > limit {
            break;
        }
    }
    let has_more = selected.len() > limit;
    selected.truncate(limit);
    let next = selected.last().map(|event| event.sequence.to_string());
    let events: Vec<Value> = selected
        .into_iter()
        .map(|event| {
            let mut payload = event.payload.clone();
            redact_value(&mut payload);
            let mut encoded = payload.to_string();
            if encoded.len() > MAX_EVENT_BYTES {
                encoded.truncate(MAX_EVENT_BYTES);
                encoded.push_str("…[truncated]");
            }
            json!({
                "event_id": event.event_id,
                "sequence": event.sequence,
                "event_type": event.event_type.as_str(),
                "session_id": event.session_id,
                "causation_id": event.causation_id,
                "created_at": event.timestamp,
                "payload": encoded
            })
        })
        .collect();
    Ok(json!({
        "session_id": session,
        "events": events,
        "next_cursor": if has_more { next } else { None }
    }))
}

fn event_belongs_to(event: &crate::super_agent::Event, session: &str) -> bool {
    event.session_id.as_deref() == Some(session)
}

fn session_children(store: &TaskStore, task: &TaskRecord, input: &Value) -> Result<Value> {
    let session = input["session_id"]
        .as_str()
        .context("session_id is required")?;
    authorize_session(store, task, session)?;
    let invocations = store
        .db()
        .with_conn(|conn| -> Result<Vec<subagents::Invocation>> {
            subagents::list_invocations(conn, session, None)
        })??;
    let children: Vec<Value> = invocations
        .into_iter()
        .map(|invocation| {
            json!({
                "session_id": invocation.child_session_id,
                "parent_session_id": invocation.parent_session_id,
                "mode": invocation.mode,
                "status": invocation.status,
                "description": invocation.description,
                "attempt_id": invocation.attempt_id
            })
        })
        .collect();
    Ok(json!({
        "session_id": session,
        "current_coder": task.coder.session_id,
        "previous_coder_sessions": task.coder.previous_session_ids,
        "children": children
    }))
}

fn artifact_name(value: &str) -> Result<&str> {
    ensure!(
        !value.is_empty()
            && value.len() <= 200
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
        "Artifact IDs may only use letters, digits, dot, dash and underscore"
    );
    ensure!(
        value != "." && value != ".." && !value.contains(".."),
        "Artifact ID cannot traverse directories"
    );
    Ok(value)
}

fn artifact_read(store: &TaskStore, task: &TaskRecord, input: &Value) -> Result<Value> {
    let artifact_id = input["artifact_id"]
        .as_str()
        .context("artifact_id is required")?;
    // A report may be read by its opaque ID.
    if let Some(report) = store
        .reports(&task.task_id)
        .map_err(store_error)?
        .into_iter()
        .find(|report| report.report_id == artifact_id && !report.invalidated)
    {
        let mut body = report.body;
        redact_value(&mut body);
        return Ok(json!({
            "artifact_id": artifact_id,
            "kind": "report",
            "report_kind": report.kind.as_str(),
            "spec_version": report.spec_version,
            "round": report.round,
            "body": body
        }));
    }
    let name = artifact_name(artifact_id)?;
    let directory = store
        .data_dir()
        .join("super-agent")
        .join(&task.task_id)
        .join("artifacts");
    let path = directory.join(name);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("Artifact not found: {artifact_id}"))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "Artifact is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_ARTIFACT_BYTES,
        "Artifact exceeds the read limit"
    );
    let canonical_directory = directory.canonicalize()?;
    let canonical = path.canonicalize()?;
    ensure!(
        canonical.parent() == Some(canonical_directory.as_path()),
        "Artifact escaped the task directory"
    );
    let bytes = std::fs::read(&canonical)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(json!({
        "artifact_id": artifact_id,
        "kind": "file",
        "path": canonical,
        "content": redact_text(&text)
    }))
}

pub fn execute(
    runtime: &crate::super_agent::crp::ToolRuntime,
    name: &str,
    input: Value,
) -> Result<Value> {
    ensure!(input.is_object(), "Tool input must be an object");
    let store = runtime.store();
    let task = reviewer_task(runtime)?;
    match name {
        "SessionMetadata" => {
            let session = input["session_id"]
                .as_str()
                .context("session_id is required")?;
            session_metadata(&store, &task, session)
        }
        "SessionMessages" => session_messages(&store, &task, &input),
        "SessionEvents" => session_events(&store, &task, &input),
        "SessionChildren" => session_children(&store, &task, &input),
        "ArtifactRead" => artifact_read(&store, &task, &input),
        other => anyhow::bail!("Unknown Reviewer evidence tool: {other}"),
    }
}

/// Builds the documented review request from durable task facts. The Reviewer
/// receives actual session IDs and event bounds, never only a summary.
pub fn build_review_request(store: &TaskStore, task: &TaskRecord) -> Result<Value> {
    let events = store.events(&task.task_id).map_err(store_error)?;
    let reports = store.reports(&task.task_id).map_err(store_error)?;
    let current = reports.iter().filter(|report| {
        !report.invalidated
            && report.spec_version == task.spec_version
            && report.round == task.review_round
    });
    let plan_ref = current
        .clone()
        .find(|report| report.kind == ReportKind::Plan)
        .map(|report| report.report_id.clone());
    let implementation_ref = current
        .clone()
        .find(|report| report.kind == ReportKind::Implementation)
        .map(|report| report.report_id.clone());
    let changes_ref = task
        .non_git_targets
        .iter()
        .filter_map(|target| target.diff_ref.clone())
        .next();
    let repository_versions: Vec<Value> = task
        .repositories
        .iter()
        .map(|repo| {
            json!({
                "repository_id": repo.repository_id,
                "base_commit": repo.base_commit,
                "submitted_commit": repo.submitted_commit
            })
        })
        .collect();
    let non_git_snapshots: Vec<Value> = task
        .non_git_targets
        .iter()
        .map(|target| {
            let submission = target.submission.as_ref();
            json!({
                "target": target.path,
                "snapshot_dir": target.snapshot_dir,
                "diff_ref": target.diff_ref,
                "status": target.status.as_str(),
                // The Reviewer must echo this immutable content identity in
                // `reviewed_versions[].snapshot_ref`. It changes whenever the
                // submitted bytes or the review round change.
                "snapshot_ref": submission.map(|submission| submission.artifact_id.clone()),
                "submission": submission.map(|submission| json!({
                    "artifact_id": submission.artifact_id,
                    "source_path": submission.source_path,
                    "spec_version": submission.spec_version,
                    "review_round": submission.review_round,
                    "content_hash": submission.content_hash,
                    "complete": submission.complete,
                    "skipped": submission.skipped,
                    "skipped_unmapped": submission.skipped_unmapped,
                    "preserves_mode": submission.preserves_mode,
                    "platform": submission.platform
                }))
            })
        })
        .collect();
    let child_session_ids: Vec<String> = coder_sessions(store, task)?
        .into_iter()
        .filter(|session| Some(session) != task.coder.session_id.as_ref())
        .collect();
    let start_sequence = events.first().map(|event| event.sequence).unwrap_or(0);
    let end_sequence = events.last().map(|event| event.sequence).unwrap_or(0);
    Ok(json!({
        "review_request": {
            "task_id": task.task_id,
            "pair_id": task.pair_id,
            "spec_version": task.spec_version,
            "review_round": task.review_round,
            "task_spec_ref": format!("task-spec-{}", task.task_id),
            "coder": {
                "session_id": task.coder.session_id,
                "previous_session_ids": task.coder.previous_session_ids,
                "child_session_ids": child_session_ids
            },
            "trace": {
                "start_sequence": start_sequence,
                "end_sequence": end_sequence
            },
            "artifacts": {
                "plan_ref": plan_ref,
                "implementation_report_ref": implementation_ref,
                "changes_ref": changes_ref
            },
            "repository_versions": repository_versions,
            "non_git_snapshots": non_git_snapshots,
            "runtime": {
                "harness_version": env!("CARGO_PKG_VERSION"),
                "permission_policy_version": task.permission_policy_version,
                "review_stage": if task.phase == crate::super_agent::Phase::FinalReview {
                    ReviewStage::FinalDelivery.as_str()
                } else {
                    ReviewStage::Implementation.as_str()
                }
            }
        }
    }))
}

/// Final-delivery review request. It binds the immutable integrated versions
/// the host verified, so the Reviewer cannot be pointed at an earlier
/// implementation commit.
pub fn build_final_review_request(store: &TaskStore, task: &TaskRecord) -> Result<Value> {
    let mut request = build_review_request(store, task)?;
    let versions: Vec<Value> = task
        .repositories
        .iter()
        .map(|repo| {
            json!({
                "repository_id": repo.repository_id,
                "integrated_commit": repo.integrated_commit,
                "reviewed_commit": repo.reviewed_commit,
                "target_branch": repo.target_branch
            })
        })
        .collect();
    let integration: Vec<Value> = task
        .repositories
        .iter()
        .map(|repo| {
            json!({
                "repository_id": repo.repository_id,
                "integrated_commit": repo.integrated_commit,
                "reviewed_commit": repo.reviewed_commit,
                "target_branch": repo.target_branch,
                "integration_status": repo.integration_status.as_str()
            })
        })
        .collect();
    let inner = request
        .get_mut("review_request")
        .and_then(Value::as_object_mut)
        .context("Review request shape is invalid")?;
    inner.insert("repository_versions".into(), json!(versions));
    inner.insert("integration".into(), json!(integration));
    inner.insert(
        "review_stage".into(),
        json!(ReviewStage::FinalDelivery.as_str()),
    );
    if let Some(runtime) = inner.get_mut("runtime").and_then(Value::as_object_mut) {
        runtime.insert(
            "review_stage".into(),
            json!(ReviewStage::FinalDelivery.as_str()),
        );
    }
    Ok(request)
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn path_identity(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_identity(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

/// Content identity over the sorted manifest, so enumeration order and the
/// original target never influence it.
fn manifest_content_hash(manifest: &SnapshotManifest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deepseek-code.non-git.content.v1\0");
    let mut entries: Vec<_> = manifest.entries.iter().collect();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        let kind: &[u8] = match entry.kind {
            SnapshotKind::File => b"file",
            SnapshotKind::Directory => b"directory",
            SnapshotKind::Symlink => b"symlink",
        };
        hasher.update(kind);
        hasher.update([0]);
        hasher.update(entry.hash.as_bytes());
        hasher.update([0]);
        hasher.update(entry.size.to_le_bytes());
        hasher.update(entry.mode.unwrap_or(u32::MAX).to_le_bytes());
        if let Some(target) = &entry.link_target {
            hasher.update(target.as_bytes());
        }
        hasher.update([0xff]);
    }
    for skipped in &manifest.skipped {
        hasher.update(b"skipped\0");
        hasher.update(skipped.as_bytes());
    }
    hasher.update(if manifest.truncated {
        b"truncated".as_slice()
    } else {
        b"complete".as_slice()
    });
    hex(hasher.finalize())
}

/// Stable, collision-resistant identity: task, canonical target path, spec
/// version, review round and submitted content. It never uses the basename or
/// enumeration order, so equal final components stay separate.
fn submission_artifact_id(
    task_id: &str,
    source: &Path,
    spec_version: u32,
    review_round: u32,
    content_hash: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deepseek-code.non-git.identity.v1\0");
    hasher.update(task_id.as_bytes());
    hasher.update([0]);
    hasher.update(path_identity(source));
    hasher.update([0]);
    hasher.update(spec_version.to_le_bytes());
    hasher.update(review_round.to_le_bytes());
    hasher.update(content_hash.as_bytes());
    let identity = hex(hasher.finalize());
    format!(
        "non-git-{}-s{spec_version}-r{review_round}-{}",
        &identity[..24],
        &content_hash[..24]
    )
}

/// Compares the protected snapshot content and metadata, including explicitly
/// skipped paths. Used to decide whether the current target still matches an
/// already pinned submission; it does not perform integrity verification.
fn same_submission_content(left: &SnapshotManifest, right: &SnapshotManifest) -> bool {
    left.entries == right.entries
        && left.skipped == right.skipped
        && left.skipped_raw == right.skipped_raw
        && left.skipped_unmapped == right.skipped_unmapped
        && left.truncated == right.truncated
}

fn submission_scope(data_dir: &Path, task_id: &str) -> PathBuf {
    data_dir
        .join("super-agent")
        .join(task_id)
        .join("submissions")
}

fn validate_submission_artifact_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value != "."
            && value != ".."
            && !value.starts_with('.')
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "Invalid non-Git submission artifact ID"
    );
    Ok(())
}

/// Verifies a pinned artifact against its durable identity: scope, metadata,
/// manifest and payload. It never reads the mutable working target.
fn verify_submission_dir(scope: &Path, submission: &NonGitSubmission) -> Result<SnapshotManifest> {
    validate_submission_artifact_id(&submission.artifact_id)?;
    ensure!(
        submission.source_path.is_absolute(),
        "Pinned non-Git source path must be absolute"
    );
    let scope = scope.canonicalize().with_context(|| {
        format!(
            "Task submission directory is unavailable: {}",
            scope.display()
        )
    })?;
    let directory = submission.directory.canonicalize().with_context(|| {
        format!(
            "Pinned non-Git submission is missing: {}",
            submission.directory.display()
        )
    })?;
    ensure!(
        directory.parent() == Some(scope.as_path()),
        "Pinned non-Git submission is outside the task's artifact scope"
    );
    ensure!(
        directory.file_name().and_then(|name| name.to_str())
            == Some(submission.artifact_id.as_str()),
        "Pinned non-Git submission directory does not match its artifact ID"
    );
    let metadata = std::fs::read(directory.join("submission.json")).with_context(|| {
        format!(
            "Pinned non-Git submission metadata is missing for {}",
            submission.artifact_id
        )
    })?;
    let stored: NonGitSubmission =
        serde_json::from_slice(&metadata).context("Parsing pinned non-Git submission metadata")?;
    ensure!(
        stored == *submission,
        "Pinned non-Git submission metadata differs from the durable record"
    );
    let manifest = crate::super_agent::isolation::verify_snapshot_payload(&directory)?;
    ensure!(
        manifest.source == submission.source_path,
        "Pinned non-Git submission describes a different target"
    );
    ensure!(
        !manifest.skipped_unmapped,
        "Pinned non-Git submission contains paths that cannot be represented"
    );
    let content_hash = manifest_content_hash(&manifest);
    ensure!(
        content_hash == submission.content_hash,
        "Pinned non-Git submission content no longer matches its identity"
    );
    let expected = submission_artifact_id(
        &submission.task_id,
        &submission.source_path,
        submission.spec_version,
        submission.review_round,
        &content_hash,
    );
    ensure!(
        expected == submission.artifact_id,
        "Pinned non-Git submission identity is inconsistent"
    );
    Ok(manifest)
}

/// Verifies the current round's pinned non-Git submission without reading the
/// mutable target. The returned manifest is the exact reviewed content.
pub fn verify_pinned_submission(
    data_dir: &Path,
    task: &TaskRecord,
    target: &crate::super_agent::NonGitTarget,
) -> Result<SnapshotManifest> {
    let submission = target
        .submission
        .as_ref()
        .context("The non-Git target has no pinned review submission")?;
    ensure!(
        submission.task_id == task.task_id,
        "Pinned non-Git submission belongs to another task"
    );
    ensure!(
        submission.spec_version == task.spec_version
            && submission.review_round == task.review_round,
        "Pinned non-Git submission belongs to spec version {} round {}, current is {} round {}",
        submission.spec_version,
        submission.review_round,
        task.spec_version,
        task.review_round
    );
    let source = target
        .path
        .canonicalize()
        .unwrap_or_else(|_| target.path.clone());
    ensure!(
        submission.source_path == source,
        "Pinned non-Git submission belongs to a different target"
    );
    let scope = submission_scope(data_dir, &task.task_id);
    verify_submission_dir(&scope, submission)
}

/// Delivery freshness: the working target must still equal the pinned content.
/// A mismatch is reported to the gate; the target is never rewritten.
pub fn submission_matches_target(
    target: &crate::super_agent::NonGitTarget,
    manifest: &SnapshotManifest,
) -> Result<bool> {
    let current = crate::super_agent::isolation::current_manifest(&target.path)?;
    Ok(same_submission_content(&current, manifest))
}

fn read_submission_metadata(directory: &Path) -> Result<NonGitSubmission> {
    let bytes = std::fs::read(directory.join("submission.json")).with_context(|| {
        format!(
            "Pinned non-Git submission metadata is missing in {}",
            directory.display()
        )
    })?;
    serde_json::from_slice(&bytes).context("Parsing pinned non-Git submission metadata")
}

/// Copies the current target into a new pinned artifact. The copy is staged
/// under a hidden directory and published by rename only after the payload,
/// manifest and metadata are complete.
fn pin_submission_snapshot(
    data_dir: &Path,
    task: &TaskRecord,
    target: &crate::super_agent::NonGitTarget,
) -> Result<NonGitSubmission> {
    let source = target
        .path
        .canonicalize()
        .with_context(|| format!("Non-Git target is unavailable: {}", target.path.display()))?;
    ensure!(source.is_dir(), "Non-Git target must be a directory");
    let scope = submission_scope(data_dir, &task.task_id);
    std::fs::create_dir_all(&scope)?;
    let staging = scope.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<NonGitSubmission> {
        crate::super_agent::isolation::create_snapshot(&source, &staging)?;
        // Full payload verification before anything is published; this also
        // rejects absolute or escaping symlinks that would resolve to the
        // mutable original after copying.
        let manifest = crate::super_agent::isolation::verify_snapshot_payload(&staging)?;
        ensure!(
            !manifest.truncated,
            "Non-Git target exceeds snapshot limits; the submission is incomplete"
        );
        ensure!(
            !manifest.skipped_unmapped,
            "Non-Git target contains paths that cannot be represented on this platform"
        );
        let content_hash = manifest_content_hash(&manifest);
        let artifact_id = submission_artifact_id(
            &task.task_id,
            &source,
            task.spec_version,
            task.review_round,
            &content_hash,
        );
        let directory = scope.join(&artifact_id);
        if directory.exists() {
            // Same content identity: reuse the published artifact and its
            // original immutable metadata. A retry after publishing but before
            // the durable record must not manufacture a new timestamp.
            let existing = read_submission_metadata(&directory)?;
            ensure!(
                existing.artifact_id == artifact_id
                    && existing.task_id == task.task_id
                    && existing.source_path == source
                    && existing.spec_version == task.spec_version
                    && existing.review_round == task.review_round
                    && existing.content_hash == content_hash,
                "An existing non-Git submission with the same identity has different metadata"
            );
            ensure!(
                existing.directory.canonicalize().ok() == directory.canonicalize().ok(),
                "An existing non-Git submission points at a different directory"
            );
            let canonical_scope = scope.canonicalize()?;
            verify_submission_dir(&canonical_scope, &existing)?;
            let _ = std::fs::remove_dir_all(&staging);
            return Ok(existing);
        }
        let submission = NonGitSubmission {
            artifact_id,
            task_id: task.task_id.clone(),
            source_path: source.clone(),
            directory: directory.clone(),
            spec_version: task.spec_version,
            review_round: task.review_round,
            content_hash,
            created_at: chrono::Utc::now().to_rfc3339(),
            platform: std::env::consts::OS.into(),
            preserves_mode: cfg!(unix),
            complete: manifest.is_complete(),
            skipped: manifest.skipped.clone(),
            skipped_unmapped: manifest.skipped_unmapped,
        };
        std::fs::write(
            staging.join("submission.json"),
            serde_json::to_vec_pretty(&submission)?,
        )?;
        match std::fs::rename(&staging, &directory) {
            Ok(()) => Ok(submission),
            // A concurrent writer may have published the identical artifact
            // between the existence check and the rename. Reuse it.
            Err(_) if directory.exists() => {
                let existing = read_submission_metadata(&directory)?;
                ensure!(
                    existing.artifact_id == submission.artifact_id
                        && existing.task_id == submission.task_id
                        && existing.source_path == submission.source_path
                        && existing.spec_version == submission.spec_version
                        && existing.review_round == submission.review_round
                        && existing.content_hash == submission.content_hash,
                    "A concurrent non-Git submission has different metadata"
                );
                let canonical_scope = scope.canonicalize()?;
                verify_submission_dir(&canonical_scope, &existing)?;
                let _ = std::fs::remove_dir_all(&staging);
                Ok(existing)
            }
            Err(error) => Err(error.into()),
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Computes and records the non-Git changes artifact and pins the submitted
/// content. Repeating the call with unchanged content reuses the same artifact.
pub fn record_non_git_changes(
    store: &TaskStore,
    task: &TaskRecord,
    target: &crate::super_agent::NonGitTarget,
) -> Result<crate::super_agent::NonGitTarget> {
    let before: SnapshotManifest = serde_json::from_str(&target.snapshot_manifest_json)
        .context("Stored non-Git snapshot manifest is invalid")?;
    let current = crate::super_agent::isolation::current_manifest(&target.path)?;
    let current_round = target.submission.as_ref().is_some_and(|submission| {
        submission.task_id == task.task_id
            && submission.spec_version == task.spec_version
            && submission.review_round == task.review_round
    });
    let (submission, pinned) = if current_round {
        // A corrupt existing artifact is a hard error; never silently replace
        // evidence the Reviewer may already have read.
        let manifest = verify_pinned_submission(store.data_dir(), task, target)
            .context("The existing pinned non-Git review submission is invalid")?;
        if same_submission_content(&current, &manifest) {
            (
                target
                    .submission
                    .clone()
                    .context("Pinned submission disappeared")?,
                manifest,
            )
        } else {
            let submission = pin_submission_snapshot(store.data_dir(), task, target)?;
            let manifest = crate::super_agent::isolation::load_snapshot(&submission.directory)?;
            (submission, manifest)
        }
    } else {
        let submission = pin_submission_snapshot(store.data_dir(), task, target)?;
        let manifest = crate::super_agent::isolation::load_snapshot(&submission.directory)?;
        (submission, manifest)
    };
    let diff = crate::super_agent::isolation::diff_snapshot(&before, &pinned);
    let name = format!("changes-{}.json", submission.artifact_id);
    let bytes = serde_json::to_vec_pretty(&diff)?;
    let path = store
        .write_artifact(&task.task_id, &name, &bytes)
        .map_err(store_error)?;
    let mut updated = target.clone();
    updated.diff_ref = Some(
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(name),
    );
    updated.status = crate::super_agent::NonGitStatus::InProgress;
    updated.submission = Some(submission);
    store.save_non_git_target(&updated).map_err(store_error)?;
    Ok(updated)
}

/// The Reviewer's read-only copy of the pinned submission. It never reopens
/// the mutable target, so edits made after submission cannot change the
/// reviewed bytes.
pub fn prepare_non_git_review(
    data_dir: &Path,
    task_id: &str,
    round: u32,
    target: &crate::super_agent::NonGitTarget,
) -> Result<PathBuf> {
    let submission = target.submission.as_ref().context(
        "The non-Git target has no pinned review submission; submit the implementation first",
    )?;
    ensure!(
        submission.task_id == task_id,
        "Pinned non-Git submission belongs to another task"
    );
    ensure!(
        submission.review_round == round,
        "The pinned non-Git submission belongs to review round {}, not round {round}",
        submission.review_round
    );
    let scope = submission_scope(data_dir, task_id);
    let manifest = verify_submission_dir(&scope, submission).with_context(|| {
        format!(
            "Pinned non-Git review snapshot for {} is unavailable",
            target.path.display()
        )
    })?;
    ensure!(
        !manifest.truncated,
        "Pinned non-Git review snapshot is incomplete"
    );
    let directory = submission.directory.canonicalize()?;
    let payload = directory
        .join("payload")
        .canonicalize()
        .context("Pinned non-Git review payload is missing")?;
    ensure!(
        payload.starts_with(&directory),
        "Pinned non-Git review payload escaped its artifact"
    );
    Ok(payload)
}

pub fn verify_submitted_versions(task: &TaskRecord) -> Result<()> {
    for repo in &task.repositories {
        let head = crate::worktree::session::command(&repo.working_path, &["rev-parse", "HEAD"])
            .with_context(|| format!("Reading HEAD of {}", repo.working_path.display()))?;
        ensure!(
            head.status.success(),
            "Cannot read the submitted commit for {}: {}",
            repo.repository_id,
            String::from_utf8_lossy(&head.stderr).trim()
        );
        let commit = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        ensure!(
            repo.submitted_commit.as_deref() == Some(commit.as_str()),
            "Repository {} has no matching submitted commit",
            repo.repository_id
        );
        let status = crate::worktree::session::command(
            &repo.working_path,
            &["status", "--porcelain"],
        )
        .with_context(|| format!("Reading status of {}", repo.working_path.display()))?;
        ensure!(
            status.status.success() && status.stdout.is_empty(),
            "The task worktree {} has uncommitted changes; commit the delivery before submitting",
            repo.working_path.display()
        );
    }
    Ok(())
}

/// Validated review body for the current round, if present.
pub fn current_review(store: &TaskStore, task: &TaskRecord) -> Result<Option<TaskReviewReport>> {
    let reports = store.reports(&task.task_id).map_err(store_error)?;
    for report in reports.iter().rev() {
        if report.kind == ReportKind::Review
            && !report.invalidated
            && report.spec_version == task.spec_version
            && report.round == task.review_round
        {
            return Ok(Some(
                serde_json::from_value(report.body.clone()).context("Invalid review report")?,
            ));
        }
    }
    Ok(None)
}

/// Helper used by tests and the orchestrator to expose a redacted report body.
pub fn redacted_body(body: &Value) -> Value {
    let mut body = body.clone();
    redact_value(&mut body);
    body
}

#[cfg(test)]
mod tests;
