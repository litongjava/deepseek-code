//! Freeze resident indexes at the start of a window. Store later observations
//! only when the index set changes. Replay uses transcript order and never reads disk.
use super::{xml, FileMemory};
use crate::db::session_store::{settings::Setting, SessionDb};
use crate::services::turn_context::Entry;
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Index {
    scope: String,
    path: PathBuf,
    hash: String,
    excerpt: String,
}

const SCOPES: [&str; 5] = ["global", "team", "project", "subagent", "session"];
const INDEX: Setting<IndexState> = Setting::new("memory.prompt.index");
const NEXT_INPUT: Setting<String> = Setting::new("memory.prompt.next-input");
const EPOCH: Setting<u64> = Setting::new("memory.prompt.epoch");
const CHANGE: &str = "MEMORY.md changed. The current index follows.\n";
const TRUNCATED: &str = "[MEMORY.md truncated; read the full index at the path above.]\n";

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct IndexState {
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    indexes: Vec<Index>,
    // Frozen prefixes written by the single-project format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first: Option<String>,
}

fn index_excerpt(index: impl AsRef<[u8]>) -> String {
    let index = index.as_ref();
    let limits = crate::memory::files::IndexLimits::default();
    let lines = index.iter().filter(|&&b| b == b'\n').count()
        + usize::from(!index.is_empty() && !index.ends_with(b"\n"));
    if index.len() <= limits.bytes && lines <= limits.lines {
        return String::from_utf8_lossy(index).into_owned();
    }
    // Reserve one line for the note, including a newline when the byte cap
    // falls in the middle of a long (possibly multibyte) line.
    let line_end = index
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(limits.lines - 2)
        .map_or(index.len(), |(offset, _)| offset + 1);
    let byte_end = limits.bytes - TRUNCATED.len();
    let mut end = line_end.min(byte_end);
    if end == byte_end && index[end - 1] != b'\n' {
        end -= 1;
    }
    while end > 0 && index.get(end).is_some_and(|b| b & 0xc0 == 0x80) {
        end -= 1;
    }
    let mut text = String::from_utf8_lossy(&index[..end]).into_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(TRUNCATED);
    text
}

fn index_key(boundary: Option<&str>) -> Setting<IndexState> {
    boundary.map_or(INDEX, |id| {
        Setting::memory_receipt(format!("prompt.index.boundary.{id}"))
    })
}

fn change(id: &str) -> Setting<IndexState> {
    Setting::memory_receipt(format!("prompt.change.{id}"))
}

impl Index {
    fn block(&self) -> String {
        let path = xml(&self.path.to_string_lossy());
        let excerpt = xml(&self.excerpt);
        format!(
            "<{scope}_memory>\nIndex: {path}\n{excerpt}\n</{scope}_memory>",
            scope = self.scope
        )
    }
}

impl IndexState {
    fn with_session_dir(mut self, directory: &std::path::Path) -> Self {
        if let Some(index) = self
            .indexes
            .iter_mut()
            .find(|index| index.scope == "session")
        {
            index.path = directory.join("MEMORY.md");
        }
        self
    }

    pub(crate) fn seal(&self, c: &Connection, id: Option<&str>) -> Result<()> {
        c.execute(
            "DELETE FROM peripheral_settings WHERE key='memory.prompt.next-input'",
            [],
        )?;
        let mut frozen = self.clone();
        frozen.epoch = EPOCH.read(c)?.unwrap_or_default() + 1;
        EPOCH.write(c, &frozen.epoch)?;
        index_key(id).write(c, &frozen)
    }

    pub(crate) fn render(&self) -> Vec<String> {
        if self.indexes.is_empty() {
            return self.first.iter().cloned().collect();
        }
        self.indexes
            .iter()
            .map(|index| format!("<system-reminder>\n{}\n</system-reminder>", index.block()))
            .collect()
    }

    fn changed(&self, previous: &Self) -> Option<String> {
        let mut blocks = Vec::new();
        for scope in SCOPES {
            let old = previous.indexes.iter().find(|i| i.scope == scope);
            let new = self.indexes.iter().find(|i| i.scope == scope);
            if new == old {
                continue;
            }
            if let Some(index) = new {
                blocks.push(index.block());
            } else if let Some(index) = old {
                blocks.push(format!(
                    "{scope} index removed: {}",
                    xml(&index.path.to_string_lossy())
                ));
            }
        }
        (!blocks.is_empty()).then(|| {
            format!(
                "<system-reminder>\n{CHANGE}{}\n</system-reminder>",
                blocks.join("\n")
            )
        })
    }
}

pub(crate) fn freeze_empty_boundary(tx: &Connection, id: &str) -> Result<()> {
    index_key(Some(id)).write(tx, &IndexState::default())
}

pub(crate) const INDEX_HISTORY: &str = "WITH history(id,pos,seq,tool,tool_seq,part,boundary) AS (
    SELECT m.id,m.sort_order,m.rowid,-1,0,0,m.is_compact_boundary FROM messages m
    WHERE m.role='user'
    UNION ALL
    SELECT json_extract(j.value,'$.message_id'),m.sort_order,m.rowid,t.sort_order,t.rowid,j.key,0
    FROM messages m JOIN tool_calls t ON t.message_id=m.id
    JOIN tool_queued_inputs q ON q.tool_call_id=t.id JOIN json_each(q.inputs_json) j
    WHERE m.role='assistant' AND json_extract(j.value,'$.prompt_origin')='typed'
    AND NOT EXISTS(SELECT 1 FROM tool_call_origins n WHERE n.tool_call_id=t.id)
)";

fn latest(tx: &Connection) -> Result<Option<IndexState>> {
    use rusqlite::OptionalExtension;
    // Retained inputs can sort after a newer partial boundary. Its epoch
    // supersedes all earlier observations even in that retained suffix.
    let sql = format!("{INDEX_HISTORY} SELECT p.value FROM history h JOIN peripheral_settings p ON
        p.key=CASE WHEN h.boundary THEN 'memory.prompt.index.boundary.' ELSE 'memory.prompt.change.' END || h.id
        ORDER BY coalesce(json_extract(p.value,'$.epoch'),0) DESC,h.pos DESC,h.seq DESC,h.tool DESC,h.tool_seq DESC,h.part DESC LIMIT 1");
    let value: Option<String> = tx.query_row(&sql, [], |r| r.get(0)).optional()?;
    let prefix = INDEX.read(tx)?;
    match value {
        Some(raw) => {
            let state: IndexState = serde_json::from_str(&raw)?;
            Ok(Some(
                prefix.filter(|p| p.epoch > state.epoch).unwrap_or(state),
            ))
        }
        None => Ok(prefix),
    }
}

pub(crate) fn copy_prefix(
    src: &Connection,
    dst: &Connection,
    session: &std::path::Path,
) -> Result<()> {
    if let Some(epoch) = EPOCH.read(src)? {
        EPOCH.write(dst, &epoch)?;
    }
    if let Some(state) = INDEX.read(src)? {
        INDEX.write(dst, &state.with_session_dir(session))?;
    }
    Ok(())
}

pub(crate) fn copy_message(
    src: &Connection,
    dst: &Connection,
    old: &str,
    new: &str,
    session: &std::path::Path,
) -> Result<()> {
    dst.execute("INSERT OR REPLACE INTO peripheral_settings(key,value) SELECT 'prompt.pending.'||?1,'true' WHERE ?2",rusqlite::params![new,src.prepare_cached("SELECT 1 FROM peripheral_settings WHERE key='prompt.pending.'||?1 OR (key='memory.prompt.message.'||?1 AND json_extract(value,'$.pending')=1)")?.exists([old])?])?;
    for (from, to) in [
        (change(old), change(new)),
        (index_key(Some(old)), index_key(Some(new))),
    ] {
        if let Some(state) = from.read(src)? {
            to.write(dst, &state.with_session_dir(session))?;
        }
    }
    Ok(())
}

pub(crate) struct Admission {
    memory: Option<FileMemory>,
    team: Option<PathBuf>,
    session_dir: PathBuf,
    stage: Option<PathBuf>,
    agent: Option<String>,
}

impl Admission {
    fn context(stores: crate::db::session_store::Stores) -> Result<super::ServiceContext> {
        Ok(super::ServiceContext {
            paths: crate::runtime::RuntimePaths::new(stores.data_dir().into())?,
            credentials: std::sync::Arc::new(crate::services::EnvironmentCredentials),
            db: std::sync::Arc::new(stores),
        })
    }

    pub(crate) fn session(shard: &SessionDb) -> Result<Self> {
        let context = Self::context(shard.scoped_stores())?;
        let config = super::config(&context)?;
        let memory = config
            .enabled
            .then(|| FileMemory::from_session(&context, shard.clone(), None))
            .transpose()?;
        Ok(Self {
            memory,
            stage: None,
            team: super::team_directory(&context.paths.data_dir)?,
            session_dir: shard.memory_directory(),
            agent: shard.read(|r| r.setting(super::settings::MEMORY_AGENT))?,
        })
    }

    pub(crate) fn starting(
        stores: &crate::db::session_store::Stores,
        roots: &[std::path::PathBuf],
        owner: &str,
        session_dir: PathBuf,
        agent: Option<String>,
    ) -> Result<Self> {
        let context = Self::context(stores.clone())?;
        let config = super::config(&context)?;
        let memory = if config.enabled {
            let scope = if super::private_roots(roots) {
                super::MemoryScope::private(owner)?
            } else {
                super::MemoryScope::project(roots, true)?
            };
            Some(FileMemory::new(&context, scope, None)?)
        } else {
            None
        };
        Ok(Self {
            memory,
            stage: None,
            team: super::team_directory(&context.paths.data_dir)?,
            session_dir,
            agent,
        })
    }

    pub(crate) fn staged_session(mut self, stage: PathBuf) -> Self {
        self.stage = Some(stage);
        self
    }

    pub(crate) fn frozen_index(&self) -> Result<IndexState> {
        let mut current = IndexState::default();
        let Some(memory) = &self.memory else {
            return Ok(current);
        };
        let data = memory.context.paths.data_dir.canonicalize()?;
        for (scope, directory) in memory.directories(
            self.team.as_deref(),
            self.agent.as_deref(),
            &self.session_dir,
        )? {
            let root = if scope == "team" {
                directory.as_path()
            } else {
                data.as_path()
            };
            if let Some((raw, hash)) = crate::memory::files::resident_index(
                root,
                if scope == "session" {
                    self.stage.as_ref().unwrap_or(&directory)
                } else {
                    &directory
                },
            )? {
                if raw.is_empty() {
                    continue;
                }
                current.indexes.push(Index {
                    scope: scope.into(),
                    path: directory.join("MEMORY.md"),
                    hash,
                    excerpt: index_excerpt(&raw),
                });
            }
        }
        Ok(current)
    }

    pub(crate) fn freeze(&self, tx: &Connection, boundary: Option<&str>) -> Result<()> {
        let current = self.frozen_index()?;
        match boundary {
            Some(id) => current.seal(tx, Some(id)),
            None => INDEX.write(tx, &current),
        }
    }

    /// Only the first prompt takes a snapshot on admission. Later admissions
    /// identify where to put the next request's single change reminder.
    pub(crate) fn admit(&self, tx: &Connection, id: &str) -> Result<()> {
        if latest(tx)?.is_none() {
            return self.freeze(tx, None);
        }
        NEXT_INPUT.write(tx, &id.to_owned())
    }

    pub(crate) fn refresh(shard: &SessionDb) -> Result<bool> {
        let indexes = Self::session(shard)?;
        shard.write(|w| w.refresh_resident_indexes(&indexes))
    }

    pub(crate) fn refresh_request(
        &self,
        tx: &Connection,
        mut append: impl FnMut() -> Result<String>,
    ) -> Result<bool> {
        let current = self.frozen_index()?;
        let anchor = NEXT_INPUT.read(tx)?;
        if anchor.is_some() {
            tx.execute(
                "DELETE FROM peripheral_settings WHERE key='memory.prompt.next-input'",
                [],
            )?;
        }
        let Some(previous) = latest(tx)? else {
            INDEX.write(tx, &current)?;
            return Ok(!current.indexes.is_empty());
        };
        if current.indexes == previous.indexes && previous.first.is_none() {
            return Ok(false);
        }
        // Sync or rewind may have removed the latest admitted input.
        let mut visible = tx.prepare_cached(&format!(
            "{INDEX_HISTORY} SELECT 1 FROM history WHERE id=?1 AND NOT boundary"
        ))?;
        let id = match anchor {
            Some(id) if visible.exists([&id])? => id,
            _ => append()?,
        };
        let mut current = current;
        current.epoch = EPOCH.read(tx)?.unwrap_or_default();
        change(&id).write(tx, &current)?;
        Ok(true)
    }
}

#[cfg(test)]
pub(crate) fn replay(
    shard: &SessionDb,
    boundary: Option<&str>,
    messages: &[Value],
) -> Result<(Vec<String>, Vec<Entry>)> {
    shard.read(|r| replay_at(r, boundary, messages))
}

/// The last observed set in this precise view, including a cut used by a
/// summary request. Repacking a saved request must never observe newer files.
pub(crate) fn visible_indexes(
    r: &crate::db::session_store::SessionRead<'_>,
    boundary: Option<&str>,
    messages: &[Value],
) -> Result<Vec<String>> {
    let prefix = r.setting(index_key(boundary))?.unwrap_or_default();
    for id in messages.iter().rev().filter_map(|m| m["id"].as_str()) {
        if let Some(state) = r.setting(change(id))?.filter(|s| s.epoch >= prefix.epoch) {
            return Ok(state.render());
        }
    }
    Ok(prefix.render())
}

pub(crate) fn replay_at(
    r: &crate::db::session_store::SessionRead<'_>,
    boundary: Option<&str>,
    messages: &[Value],
) -> Result<(Vec<String>, Vec<Entry>)> {
    let mut state = r.setting(index_key(boundary))?.unwrap_or_default();
    let prefix = state.render();
    let mut entries = Vec::new();
    for row in messages.iter().filter(|m| m["role"] == "user") {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        if let Some(next) = r
            .setting(change(id))?
            .filter(|next| next.epoch >= state.epoch)
        {
            if let Some(block) = next.changed(&state) {
                entries.push(Entry {
                    key: format!("message:{id}"),
                    occurrence: 0,
                    blocks: vec![block],
                    skill: None,
                });
            }
            state = next;
        }
    }
    Ok((prefix, entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_index_keeps_exact_limits_and_marks_both_overflows() {
        let lines = "line\n".repeat(200);
        assert_eq!(index_excerpt(&lines), lines);
        let overflow = index_excerpt(&(lines + "hidden\n"));
        assert_eq!(overflow.lines().count(), 200);
        assert!(overflow.ends_with(TRUNCATED));
        assert!(!overflow.contains("hidden"));

        let bytes = "é".repeat(12799) + "a\n";
        assert_eq!(bytes.len(), 25 * 1024);
        assert_eq!(index_excerpt(&bytes), bytes);
        let overflow = index_excerpt(&("é\n".to_owned() + &bytes));
        assert!(overflow.starts_with("é\n") && overflow.ends_with(TRUNCATED));
        assert!((25 * 1024 - 4..=25 * 1024).contains(&overflow.len()));
        let invalid = vec![0xff; 25 * 1024];
        assert!(!index_excerpt(&invalid).contains(TRUNCATED));
    }
}
