//! Search reads metadata without visiting transcript overflow pages. Large
//! searchable values have independently keyed fragments so a cold continuation
//! never walks the prefix of a SQLite overflow chain.
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, DatabaseName};

pub(super) const BLOCK: usize = 64 * 1024;
pub(super) const PROJECTED_TOOLS: [&str; 6] =
    ["Bash", "Read", "Write", "Edit", "MultiEdit", "ApplyPatch"];

/// DbManager holds BEGIN IMMEDIATE through schema creation, backfill and trigger installation.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for (table, columns) in [
        ("mcp_tool_outputs", &["model_content_json"][..]),
        ("messages", &["model_content", "model_recovery"][..]),
        (
            "tool_calls",
            &[
                "model_output",
                "model_preview",
                "model_recovery",
                "model_search",
                "request_id",
                "provider_call_id",
            ][..],
        ),
    ] {
        for column in columns {
            if !conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
                params![table, column],
                |r| r.get::<_, bool>(0),
            )? {
                let ddl = if table == "tool_calls" && *column == "request_id" {
                    "ALTER TABLE tool_calls ADD COLUMN request_id TEXT REFERENCES model_requests(id) ON DELETE SET NULL".to_owned()
                } else {
                    format!("ALTER TABLE {table} ADD COLUMN {column} TEXT")
                };
                conn.execute_batch(&ddl)?;
            }
        }
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_tool_calls_request_id ON tool_calls(request_id);")?;
    let installed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('context_search_sources') WHERE name='materialized')",[],|r|r.get(0))?;
    let checked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tool_calls') WHERE name='model_checked')",
        [],
        |r| r.get(0),
    )?;
    if installed && checked {
        return Ok(());
    }
    if !checked {
        conn.execute_batch(
            "ALTER TABLE tool_calls ADD COLUMN model_checked INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    let upgrading: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='context_search_sources')",
        [],
        |r| r.get(0),
    )?;
    if upgrading {
        if !installed {
            conn.execute_batch(
            "ALTER TABLE context_search_sources ADD COLUMN materialized INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE context_search_sources ADD COLUMN recovery TEXT;",
        )?;
        } else {
            conn.execute_batch(
                "DROP TRIGGER invalidate_model_tool; DROP TRIGGER invalidate_model_message;",
            )?;
        }
        for table in ["messages", "tool_calls"] {
            for op in ["insert", "update", "delete"] {
                conn.execute_batch(&format!("DROP TRIGGER context_search_{table}_{op}"))?;
            }
        }
    } else {
        conn.execute_batch(
        "DROP INDEX IF EXISTS idx_context_search_messages;
         DROP INDEX IF EXISTS idx_context_search_tools;
         DROP INDEX IF EXISTS idx_context_search_boundaries;
         CREATE INDEX IF NOT EXISTS idx_message_history ON messages(conversation_id,sort_order);
         CREATE INDEX IF NOT EXISTS idx_message_boundary_order ON messages(conversation_id,sort_order)
             WHERE is_compact_boundary=1;
         CREATE TABLE context_search_sources(
             kind TEXT NOT NULL, id TEXT NOT NULL, owner TEXT NOT NULL, sort_order INTEGER NOT NULL,
             sequence INTEGER NOT NULL, label TEXT NOT NULL, stamp TEXT, boundary INTEGER,
             size INTEGER NOT NULL, chunked INTEGER NOT NULL, materialized INTEGER NOT NULL DEFAULT 0,recovery TEXT, PRIMARY KEY(kind,id)
         ) WITHOUT ROWID;
         CREATE INDEX context_search_order ON context_search_sources(kind,owner,sort_order,sequence);
         CREATE INDEX context_search_boundary_order ON context_search_sources(owner,sort_order,sequence)
             WHERE kind='message' AND boundary=1;
         -- Keep fragment bytes out of the search key, including interior B-tree pages.
         -- A rowid table puts the composite key in a separate, compact index.
         CREATE TABLE context_search_chunks(
             kind TEXT NOT NULL, id TEXT NOT NULL, part INTEGER NOT NULL, body BLOB NOT NULL,
             PRIMARY KEY(kind,id,part),
             FOREIGN KEY(kind,id) REFERENCES context_search_sources(kind,id) ON DELETE CASCADE
         );"
    )?;
    }
    let projected = PROJECTED_TOOLS.map(|name| format!("'{name}'")).join(",");
    let max = super::context_search_repo::MAX_CHUNK_BYTES;
    for (table, column, model, ready_column, kind, fields, metadata, chunked) in [
        ("messages", "content", "model_content","model_content","message", "id,conversation_id,sort_order,role,created_at,is_compact_boundary,content",
         "conversation_id,sort_order,rowid,role,created_at,is_compact_boundary",
         format!("role IN ('user','assistant') AND NOT coalesce(is_compact_boundary,0) AND length(CAST(content AS BLOB))>{BLOCK}")),
        ("tool_calls", "output","model_search","model_output", "tool", "id,message_id,sort_order,name,input,output",
         "message_id,sort_order,rowid,name,NULL,0",
         format!("name!='ContextSearch' AND typeof(output) IN ('text','blob') AND (
                    (name COLLATE NOCASE NOT IN ({projected}) AND length(CAST(output AS BLOB))>{BLOCK})
                    OR (length(CAST(input AS BLOB))>4096 AND
                        (name COLLATE NOCASE NOT IN ({projected}) OR length(CAST(output AS BLOB))<={max}))
                  )")),
    ] {
        let ready=format!("{ready_column} IS NOT NULL");
        let value=format!("CASE WHEN {ready} THEN {model} ELSE {column} END");
        let size=format!("CASE WHEN {ready} AND {model} IS NULL THEN -2
            WHEN typeof({value}) IN ('text','blob') THEN length(CAST(({value}) AS BLOB)) ELSE -1 END");
        let chunks=format!("CASE WHEN {ready} THEN {model} IS NOT NULL AND
            (length(CAST({model} AS BLOB))>{BLOCK} OR length(CAST({column} AS BLOB))>{BLOCK} OR length(CAST({input} AS BLOB))>4096)
            ELSE ({chunked}) END",input=if kind=="tool" {"input"}else{"NULL"});
        // 0 legacy/unclassified, 1 projected bytes, 2 classified original bytes.
        let materialization = if kind == "tool" { "CASE WHEN model_output IS NOT NULL THEN 1 WHEN model_checked THEN 2 ELSE 0 END" } else { &ready };
        let insert = format!("INSERT INTO context_search_sources
            SELECT '{kind}',id,{metadata},{size},coalesce(({chunks}),0),({materialization}),model_recovery FROM {table}");
        if !upgrading {
        conn.execute_batch(&insert)?;
        // Open each old value once and advance sequentially. Backfill occurs once
        // on upgrade; searches only open the small indexed fragments afterward.
        let mut old = conn.prepare("SELECT id,sequence,materialized FROM context_search_sources WHERE kind=? AND chunked=1")?;
        let mut rows = old.query([kind])?;
        let mut fragment = conn.prepare("INSERT INTO context_search_chunks VALUES (?1,?2,?3,?4)")?;
        let mut buf = vec![0; BLOCK];
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let source = if row.get::<_, i64>(2)? == 1 { model } else { column };
            let value = conn.blob_open(DatabaseName::Main, table, source, row.get(1)?, true)?;
            for (part, offset) in (0..value.len()).step_by(BLOCK).enumerate() {
                let len = BLOCK.min(value.len() - offset);
                value.read_at_exact(&mut buf[..len], offset)?;
                fragment.execute(params![kind, id, part, &buf[..len]])?;
            }
        }
        }
        let remove = |key: &str| format!(
            "DELETE FROM context_search_chunks WHERE kind='{kind}' AND id={key}.id;
             DELETE FROM context_search_sources WHERE kind='{kind}' AND id={key}.id;"
        );
        let save = format!("{insert} WHERE rowid=NEW.rowid;
            INSERT INTO context_search_chunks
            WITH RECURSIVE offsets(pos,size) AS (
                SELECT 0,size FROM context_search_sources WHERE kind='{kind}' AND id=NEW.id AND chunked=1 AND size>0
                UNION ALL SELECT pos+{BLOCK},size FROM offsets WHERE pos+{BLOCK}<size
            ) SELECT '{kind}',NEW.id,pos/{BLOCK},substr(CAST(({value}) AS BLOB),pos+1,{BLOCK}) FROM offsets JOIN {table} ON {table}.rowid=NEW.rowid;");
        let fields=format!("{fields},{model},model_recovery{}", if kind=="tool" {",model_checked,model_output"} else {""});
        let old = remove("OLD");
        let new = remove("NEW");
        conn.execute_batch(&format!(
            "CREATE TRIGGER context_search_{table}_insert AFTER INSERT ON {table} BEGIN {new} {save} END;
             CREATE TRIGGER context_search_{table}_update AFTER UPDATE OF {fields},rowid,_rowid_,oid ON {table} BEGIN {old} {save} END;
             CREATE TRIGGER context_search_{table}_delete AFTER DELETE ON {table} BEGIN {old} END;"
        ))?;
    }
    conn.execute_batch("CREATE TRIGGER invalidate_model_tool AFTER UPDATE OF output,name ON tool_calls
        WHEN (OLD.output IS NOT NEW.output OR OLD.name IS NOT NEW.name) AND NEW.model_checked IS OLD.model_checked AND NEW.model_output IS OLD.model_output AND NEW.model_search IS OLD.model_search AND NEW.model_recovery IS OLD.model_recovery
        BEGIN UPDATE tool_calls SET model_checked=0,model_output=NULL,model_preview=NULL,model_search=NULL,model_recovery=NULL WHERE id=NEW.id; END;
        CREATE TRIGGER invalidate_model_message AFTER UPDATE OF content ON messages WHEN OLD.content IS NOT NEW.content AND NEW.model_content IS OLD.model_content AND NEW.model_content IS NOT NULL
        BEGIN UPDATE messages SET model_content=NULL,model_recovery=NULL WHERE id=NEW.id; END;")?;
    if installed {
        // Previous builds materialized ordinary previews too. Recovery metadata
        // identifies scalar overrides, even when saving the recovery file failed.
        conn.execute_batch("UPDATE tool_calls SET model_checked=1,
            model_output=CASE WHEN model_recovery IS NOT NULL THEN model_output END,
            model_search=CASE WHEN model_recovery IS NOT NULL THEN model_search END, model_preview=NULL
            WHERE model_output IS NOT NULL;
            UPDATE messages SET model_content=NULL WHERE model_recovery IS NULL AND model_content IS NOT NULL;")?;
        crate::native_engine::mcp_output::discard_ordinary_sidecars(conn)?;
    }
    Ok(())
}

/// Read only the intersecting fragments. Charge a whole fragment when just its
/// suffix or prefix is returned. Unfragmented values have a bounded starting
/// position in the source record (message content or a short tool input).
pub(super) fn read_bytes(
    conn: &Connection,
    kind: &str,
    id: &str,
    chunked: bool,
    materialized: bool,
    start: usize,
    count: usize,
) -> Result<(Vec<u8>, usize)> {
    let mut charged = 0;
    let mut bytes = Vec::with_capacity(count);
    if count > 0 && chunked {
        let last = (start + count - 1) / BLOCK;
        let mut stmt = conn.prepare_cached(
            "SELECT part,body FROM context_search_chunks
                WHERE kind=?1 AND id=?2 AND part BETWEEN ?3 AND ?4 ORDER BY part",
        )?;
        let mut rows = stmt.query(params![kind, id, start / BLOCK, last])?;
        while let Some(row) = rows.next()? {
            let part: usize = row.get(0)?;
            let block = row.get_ref(1)?.as_blob()?;
            charged += block.len();
            let base = part * BLOCK;
            let lo = start.saturating_sub(base);
            let hi = (start + count - base).min(block.len());
            ensure!(lo <= hi, "Invalid ContextSearch fragment");
            bytes.extend_from_slice(&block[lo..hi]);
        }
        ensure!(bytes.len() == count, "Incomplete ContextSearch source");
    } else if count > 0 {
        let (table, column, lookup) = if kind == "tool" {
            (
                "tool_calls",
                if materialized {
                    "model_search"
                } else {
                    "output"
                },
                "SELECT rowid FROM tool_calls WHERE id=?",
            )
        } else {
            (
                "messages",
                if materialized {
                    "model_content"
                } else {
                    "content"
                },
                "SELECT rowid FROM messages WHERE id=?",
            )
        };
        let rowid = conn
            .prepare_cached(lookup)?
            .query_row([id], |row| row.get(0))?;
        let blob = conn.blob_open(DatabaseName::Main, table, column, rowid, true)?;
        bytes.resize(count, 0);
        blob.read_at_exact(&mut bytes, start)?;
        charged += count;
    }
    Ok((bytes, charged))
}

#[cfg(test)]
mod override_tests {
    use super::*;

    #[test]
    fn base64_model_boundary_upgrade_reindexes_ordinary_results_and_keeps_recovery() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        migrate(&db).unwrap();
        // Simulate the preceding schema: all completed results had materialized
        // text, and the search index and its triggers were already installed.
        db.execute_batch("ALTER TABLE tool_calls RENAME COLUMN model_checked TO old_fixture_field;
            INSERT INTO conversations(id,created_at,updated_at) VALUES('s','now','now');
            INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order) VALUES('m','s','assistant','','now',0);
            INSERT INTO tool_calls(id,message_id,name,output,sort_order,model_output,model_preview,model_search)
              VALUES('plain','m','Bash','ordinary',0,'ordinary','old preview','ordinary');
            INSERT INTO tool_calls(id,message_id,name,output,sort_order,model_output,model_search,model_recovery)
              VALUES('projected','m','Bash','original',1,'saved text','saved text','saved recovery');
            INSERT INTO mcp_tool_outputs(conversation_id,tool_use_id,content_json,model_content_json,metadata_json,display_text,is_error)
              VALUES('s','plain','\"ordinary\"','\"old preview\"','{}','ordinary',0);").unwrap();
        migrate(&db).unwrap();
        migrate(&db).unwrap();
        let ordinary: bool = db.query_row("SELECT model_checked AND output='ordinary' AND model_output IS NULL AND model_preview IS NULL
            AND model_search IS NULL AND model_recovery IS NULL FROM tool_calls WHERE id='plain'", [], |r| r.get(0)).unwrap();
        assert!(ordinary);
        let indexes: Vec<(String, i64)> = db
            .prepare(
                "SELECT id,materialized FROM context_search_sources WHERE kind='tool' ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(indexes, vec![("plain".into(), 2), ("projected".into(), 1)]);
        assert!(db
            .query_row(
                "SELECT model_content_json IS NULL FROM mcp_tool_outputs WHERE tool_use_id='plain'",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap());
        assert!(db.query_row("SELECT model_output='saved text' AND model_recovery='saved recovery' FROM tool_calls WHERE id='projected'", [], |r|r.get::<_,bool>(0)).unwrap());
    }
}
