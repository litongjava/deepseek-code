use super::*;
use crate::db::message_repo;
use crate::services::lifecycle_events::NewSessionConfiguration;
use rusqlite::{params_from_iter, types::Value};
use serde_json::Value as Json;

#[derive(Clone, Copy, Debug)]
pub enum Branch<'a> {
    Full,
    Summary { message: &'a str, part: crate::services::compaction::Part, fingerprint: &'a str, text: &'a str },
    AgentFork {
        title: &'a str,
        directive: &'a str,
    },
    Through {
        message: Option<&'a str>,
        index: Option<usize>,
    },
    Clear,
    Child,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Source {
    pub id: String,
    pub incarnation: String,
    pub revision: i64,
    pub child: bool,
    pub expected_cwd: Option<PathBuf>,
    pub binding: String,
    #[serde(default)]
    pub provider_selection: Option<super::super::binding::ClearBinding>,
}

fn private_owner(c: &Connection, id: &str) -> Result<(String, String)> {
    // Child bindings point directly to the root owner. Retain the binding when
    // that owner disappears so a reused logical ID cannot grant its workspace.
    c.query_row(
        "SELECT o.id,o.incarnation FROM conversations s
        LEFT JOIN conversation_private_workspace_bindings b ON b.child_id=s.id
        JOIN conversations o ON o.id=coalesce(b.parent_id,s.id)
          AND o.incarnation=coalesce(b.parent_incarnation,s.incarnation)
        WHERE s.id=?1 AND s.project_id IS NULL AND o.project_id IS NULL
          AND o.workspace_mode=s.workspace_mode AND (b.child_id IS NULL OR o.archived=0)
          AND NOT EXISTS (SELECT 1 FROM conversation_private_workspace_bindings WHERE child_id=o.id)",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .context("Private workspace owner unavailable")
}

pub(in crate::db::session_store) fn workspace(
    c: &Connection,
    root: &Path,
    id: &str,
) -> Result<PathBuf> {
    let row = crate::db::conversation_repo::conversation_metadata(c, id)?
        .context("Session unavailable")?;
    ensure!(!row.archived, "Session is archived");
    if row.project_id.is_some() {
        return Ok(crate::native_engine::workspace::resolve_workspace(c, root, id, None)?.0);
    }
    let (_, inc) = private_owner(c, id)?;
    ensure!(
        Uuid::parse_str(&inc)?.to_string() == inc,
        "Invalid workspace incarnation"
    );
    fs::create_dir_all(root)?;
    let path = root.canonicalize()?.join(inc);
    fs::create_dir_all(&path)?;
    ensure!(
        path.canonicalize()? == path,
        "Private workspace is redirected"
    );
    Ok(path)
}

pub(super) fn binding(c: &Connection, id: &str) -> Result<String> {
    // Worktree and private-parent changes can be made independently of title
    // and other directory metadata. Fence their actual values at publication.

    Ok(c.query_row(
        "SELECT json_array(c.workspace_mode,c.project_id,c.archived, w.project_id,
        w.source_path,w.repo_path,w.path,w.branch,b.backend,b.metadata,p.parent_id,p.parent_incarnation,
        (SELECT incarnation FROM conversations WHERE id=p.parent_id))
        FROM conversations c LEFT JOIN conversation_worktrees w ON w.conversation_id=c.id
        LEFT JOIN conversation_worktree_backends b ON b.conversation_id=c.id
        LEFT JOIN conversation_private_workspace_bindings p ON p.child_id=c.id WHERE c.id=?1",
        [id],
        |r| r.get(0),
    )?)
}

impl SessionDb {
    pub fn workspace(&self) -> Result<PathBuf> {
        let _guards = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores.global().read(|c| {
            let snapshot = c.unchecked_transaction()?;
            workspace(&snapshot, &self.stores.layout().workspaces(), &self.id)
        })
    }
    pub fn copy_preferences_to(&self, destination: &SessionDb) -> Result<()> {
        ensure!(self.stores.data_dir() == destination.stores.data_dir() && self.key != destination.key,
            "Preference copy requires distinct sessions in one store");
        let _locks = self.stores.group_access(vec![(&self.key,false,false),(&destination.key,false,false)])?;
        self.check_directory()?;
        destination.check_directory()?;
        let values = self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |conn| {
            let tx = conn.unchecked_transaction()?;
            preference_snapshot(&tx, self.id(), destination.id())
        })?;
        destination.write_locked("copy session preferences", |w| {
            for (key, value) in &values { super::super::state::set_local(w.0, key, value)?; }
            Ok(())
        })
    }

    pub fn prepare_summary_part(
        &self,
        message: &str,
        part: crate::services::compaction::Part,
    ) -> Result<crate::services::compaction::PreparedPart> {
        self.prepare_summary_part_for(
            message,
            part,
            &crate::native_engine::provider_manager::ApiFormat::OpenAI,
        )
    }

    pub(crate) fn prepare_summary_part_for(
        &self,
        message: &str,
        part: crate::services::compaction::Part,
        format: &crate::native_engine::provider_manager::ApiFormat,
    ) -> Result<crate::services::compaction::PreparedPart> {
        use crate::services::compaction::{Part, PreparedPart};
        self.read(|r| {
            let rows = crate::db::model_results::read_messages(r.0, self.id(), |row, collapsed| (row, collapsed))?;
            let index = rows.iter().position(|(m, _)| m.id == message)
                .context("Message not found in this conversation")?;
            let range = match part {
                Part::UpTo => rows[..index].iter().rposition(|(m, _)| m.is_compact_boundary).unwrap_or(0)..index,
                Part::From => index..rows.len(),
            };
            let selected = &rows[range];
            ensure!(!selected.is_empty(), "Nothing to summarize there");
            for (row, _) in selected {
                let native: bool = r.0.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_boundaries WHERE message_id=?1)",
                    [&row.id], |r| r.get(0),
                )?;
                ensure!(!native, "Partial text summaries cannot include an opaque Responses boundary; select messages after it or branch before it");
            }
            let (messages, boundary) = crate::runtime::replay_rows(r.0, self.id(), selected, format, |_| Ok(None), true)?;
            Ok(PreparedPart {
                boundary,
                incarnation: self.incarnation().into(),
                fingerprint: crate::services::compaction::fingerprint(r.0, self.id())?.0,
                transcript: serde_json::to_string(&messages)?,
                messages,
                count: selected.len(),
            })
        })
    }

    pub fn branch(&self, destination: &str, boundary: Branch<'_>) -> Result<SessionDb> {
        self.branch_at(destination, boundary, None, None, None)
    }
    pub fn derive_owned_child(&self, destination: &str, expected_cwd: &Path) -> Result<SessionDb> {
        self.branch_at(
            destination,
            Branch::Child,
            Some(expected_cwd.canonicalize()?),
            None,
            None,
        )
    }
    pub fn clear_with_provider(
        &self,
        destination: &str,
        snapshot: &crate::native_engine::session_binding::BindingSnapshot,
        configuration: Option<&NewSessionConfiguration>,
    ) -> Result<SessionDb> {
        self.branch_at(
            destination,
            Branch::Clear,
            None,
            Some(super::super::binding::ClearBinding::from_snapshot(
                snapshot,
            )?),
            configuration,
        )
    }
    fn branch_at(
        &self,
        destination: &str,
        boundary: Branch<'_>,
        expected_cwd: Option<PathBuf>,
        provider_selection: Option<super::super::binding::ClearBinding>,
        configuration: Option<&NewSessionConfiguration>,
    ) -> Result<SessionDb> {
        let dest = storage_key(destination)?;
        ensure!(dest != self.key, "Source and destination must differ");
        let s = &self.stores;
        let _locks = s.group_access(vec![(&self.key, false, false), (&dest, true, true)])?;
        self.binding_control()?;
        let (row, revision, binding) = s.global().read(|conn| {
            let snapshot = conn.unchecked_transaction()?;
            let c = &snapshot;
            let row = super::super::directory::one(c, &self.id)?
                .context("Source unavailable")?
                .conversation;
            let revision = c.query_row(
                "SELECT metadata_revision FROM conversations WHERE id=?1",
                [&self.id],
                |r| r.get(0),
            )?;
            if matches!(
                boundary,
                Branch::Child | Branch::Clear | Branch::AgentFork { .. }
            ) {
                ensure!(!row.archived, "Archived execution source");
                if row.project_id.is_none() {
                    private_owner(c, &self.id)?;
                } else {
                    crate::native_engine::workspace::resolve_workspace(
                        c,
                        &s.layout().workspaces(),
                        &self.id,
                        None,
                    )?;
                }
            }
            Ok((row, revision, binding(c, &self.id)?))
        })?;
        let options = CreateOptions {
            title: Some(if let Branch::AgentFork { title, .. } = boundary {
                title.into()
            } else if matches!(boundary, Branch::Child | Branch::Clear) {
                "CLI session".into()
            } else {
                format!(
                    "{} (branch)",
                    row.title.as_deref().unwrap_or("Conversation")
                )
            }),
            model: row.model,
            provider: row.provider,
            workspace_mode: Some(row.workspace_mode),
            research: row.research_mode,
            project: row.project_id.map(ProjectSelection::Existing),
            ..Default::default()
        };
        s.create_locked(
            destination,
            options,
            Seed::Fork(self, boundary, configuration),
            Some(Source {
                id: self.id.clone(),
                incarnation: self.incarnation.clone(),
                revision,
                child: matches!(boundary, Branch::Child | Branch::AgentFork { .. }),
                expected_cwd,
                binding,
                provider_selection,
            }),
        )
    }
}

struct Copier<'a> {
    messages: crate::db::transcript::MessageIds,
    src: &'a Connection,
    dst: &'a Connection,
    id: &'a str,
    session_dir: PathBuf,
    keep_unresolved_origins: bool,
}
impl Copier<'_> {
    fn mapped(&self, kind: &str, old: &str) -> Result<Option<String>> {
        Ok(self
            .dst
            .query_row(
                "SELECT new FROM copied_ids WHERE kind=?1 AND old=?2",
                [kind, old],
                |r| r.get(0),
            )
            .optional()?)
    }
    fn assign(&self, kind: &str, old: &str) -> Result<String> {
        if let Some(id) = self.mapped(kind, old)? {
            return Ok(id);
        }
        let compaction = kind == "request" && self.src.prepare_cached(
            "SELECT 1 FROM request_metadata WHERE request_id=?1 AND json_extract(value,'$.kind')='compaction'"
        )?.exists([old])?;
        let id = if compaction {
            // Compaction has no assistant trajectory. Its ID names the HTTP
            // request in every descendant and in the recording gateway.
            old.to_owned()
        } else if kind == "message" {
            self.messages.issue()
        } else {
            Uuid::new_v4().to_string()
        };
        self.dst
            .execute("INSERT INTO copied_ids VALUES(?1,?2,?3)", [kind, old, &id])?;
        Ok(id)
    }
    fn remap_blocks(&self, message: &str, blocks: &mut Json) -> Result<()> {
        if let Some(items) = blocks.as_array_mut() {
            for block in items {
                for key in ["id", "tool_use_id", "toolCallId", "parentToolCallId"] {
                    if let Some(old) = block[key].as_str() {
                        // Request traces carry the provider ID. Streamed history qualifies
                        // it with the assistant message, and providers may reuse it.
                        let occurrence: Option<String> = self
                            .src
                            .query_row(
                                "SELECT id FROM tool_calls WHERE message_id=?1 AND id IN (?2,?3)
                             ORDER BY id=?3 DESC LIMIT 1",
                                params![message, old, format!("{message}:{old}")],
                                |r| r.get(0),
                            )
                            .optional()?;
                        if let Some(new) = occurrence
                            .map(|id| self.mapped("tool", &id))
                            .transpose()?
                            .flatten()
                        {
                            block[key] = Json::String(new)
                        }
                    }
                }
            }
        }
        Ok(())
    }
    // Names and column rules below are fixed schema identifiers. Each cursor
    // holds one row; the potentially large ID map belongs to the private stage.
    fn rows(
        &self,
        table: &str,
        owner: Option<(&str, &str)>,
        identity: Option<(&str, &str)>,
        links: &[(&str, &str)],
    ) -> Result<()> {
        let mut q = self
            .src
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
        let columns: Vec<String> = q.column_names().into_iter().map(str::to_owned).collect();
        let idx = |name: &str| {
            columns
                .iter()
                .position(|c| c == name)
                .context("Missing copy column")
        };
        let marks = vec!["?"; columns.len()].join(",");
        let mut insert = self.dst.prepare(&format!(
            "INSERT OR REPLACE INTO {table}({}) VALUES({marks})",
            columns.join(",")
        ))?;
        let mut rows = q.query([])?;
        'source_rows: while let Some(row) = rows.next()? {
            let mut values = (0..columns.len())
                .map(|i| row.get::<_, Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some((col, kind)) = owner {
                let old: String = row.get(idx(col)?)?;
                let Some(new) = self.mapped(kind, &old)? else {
                    continue;
                };
                values[idx(col)?] = Value::Text(new);
            }
            if let Some((col, kind)) = identity {
                let i = idx(col)?;
                // The public snapshot writer also normalizes anonymous request entries.
                if table != "model_requests" || values[i] != Value::Null {
                    values[i] = Value::Text(self.assign(kind, &row.get::<_, String>(i)?)?);
                }
            }
            for &(col, kind) in links {
                let old: String = row.get(idx(col)?)?;
                let new = match self.mapped(kind, &old)? {
                    Some(id) => id,
                    // A full in-flight snapshot may precede the parent stream event.
                    None if table == "tool_call_origins" && self.keep_unresolved_origins => {
                        self.assign("unresolved_parent", &old)?
                    }
                    None if table == "tool_call_origins" => continue 'source_rows,
                    None => anyhow::bail!("Unselected copy reference"),
                };
                values[idx(col)?] = Value::Text(new);
            }
            if let Ok(i) = idx("conversation_id") {
                values[i] = Value::Text(self.id.into())
            }
            if table == "tool_queued_inputs" {
                let i = idx("inputs_json")?;
                let mut inputs: Vec<crate::services::input_queue::DeliveredInput> =
                    serde_json::from_str(&row.get::<_, String>(i)?)?;
                for input in &mut inputs {
                    if let Some(old) = &input.message_id {
                        let new = self.assign("message", old)?;
                        crate::services::memory_files::prompt::copy_message(
                            self.src,
                            self.dst,
                            old,
                            &new,
                            &self.session_dir,
                        )?;
                        input.message_id = Some(new);
                    }
                }
                values[i] = Value::Text(serde_json::to_string(&inputs)?);
            }
            if table == "model_requests" {
                let i = idx("response_blocks_json")?;
                let mut blocks: Json = serde_json::from_str(&row.get::<_, String>(i)?)?;
                self.remap_blocks(&row.get::<_, String>(idx("message_id")?)?, &mut blocks)?;
                values[i] = Value::Text(blocks.to_string());
            }
            if table == "request_metadata" {
                let i = idx("value")?;
                let mut metadata: Json = serde_json::from_str(row.get_ref(i)?.as_str()?)?;
                if metadata["kind"] == "compaction" && metadata.get("originSession").is_none() {
                    let request: String = row.get(idx("request_id")?)?;
                    metadata["originSession"] = Json::String(self.src.query_row(
                        "SELECT conversation_id FROM model_requests WHERE id=?1",
                        [request],
                        |r| r.get(0),
                    )?);
                    values[i] = Value::Text(metadata.to_string());
                }
            }
            if table == "session_permission_authority" {
                values[idx("id")?] = Value::Null
            }
            insert.execute(params_from_iter(values))?;
        }
        Ok(())
    }
    fn run(&self, boundary: Branch<'_>) -> Result<()> {
        self.dst.execute_batch(
            "CREATE TEMP TABLE copied_ids(kind TEXT,old TEXT,new TEXT,PRIMARY KEY(kind,old))",
        )?;
        self.rows("session_agents", None, None, &[])?;
        if matches!(
            boundary,
            Branch::Child | Branch::Full | Branch::AgentFork { .. }
        ) {
            self.rows("session_permission_authority", None, None, &[])?;
        }
        if matches!(boundary, Branch::Clear | Branch::Child) {
            return Ok(());
        }
        crate::services::memory_files::prompt::copy_prefix(self.src, self.dst, &self.session_dir)?;
        let limit = match boundary {
            Branch::Through { message, index } => {
                let by_id = message
                    .map(|id| {
                        self.src
                            .query_row("SELECT n FROM (SELECT id,row_number() OVER(ORDER BY sort_order,rowid)-1 n FROM messages) WHERE id=?1", [id], |r| r.get::<_, usize>(0))
                            .optional()
                    })
                    .transpose()?
                    .flatten();
                Some(by_id.or(index).context("Missing branch boundary")?)
            }
            _ => None,
        };
        let mut q = self.src.prepare("SELECT id,role,content,thinking,created_at,is_compact_boundary,sort_order,model_content,model_recovery FROM messages ORDER BY sort_order,rowid")?;
        let mut insert=self.dst.prepare_cached("INSERT INTO messages(id,conversation_id,role,content,thinking,created_at,is_compact_boundary,sort_order,model_content,model_recovery) VALUES(?,?,?,?,?,?,?,?,?,?)")?;
        let mut count = 0;
        let mut rows = q.query([])?;
        while let Some(row) = rows.next()? {
            let old_id: String = row.get(0)?;
            let id = self.assign("message", &old_id)?;
            insert.execute(params![
                id,
                self.id,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
                row.get::<_, Value>(3)?,
                row.get::<_, Value>(4)?,
                row.get::<_, Value>(5)?,
                row.get::<_, Value>(6)?,
                row.get::<_, Value>(7)?,
                row.get::<_, Value>(8)?
            ])?;
            if row.get::<_, String>(1)? == "user" {
                crate::services::memory_files::prompt::copy_message(
                    self.src,
                    self.dst,
                    &old_id,
                    &id,
                    &self.session_dir,
                )?;
            }
            count += 1;
            if limit == Some(count - 1) {
                break;
            }
        }
        ensure!(
            limit.is_none_or(|i| i < count),
            "Branch boundary outside history"
        );
        let mut turns = self.src.prepare(
            "SELECT id,started_at,ended_at,elapsed_ms,status FROM execution_turns ORDER BY rowid",
        )?;
        let mut rows = turns.query([])?;
        while let Some(r) = rows.next()? {
            let old: String = r.get(0)?;
            if limit.is_some() {
                let mut selected = false;
                let mut q = self
                    .src
                    .prepare_cached("SELECT message_id FROM message_turns WHERE turn_id=?1")?;
                let mut links = q.query([&old])?;
                while let Some(link) = links.next()? {
                    if self
                        .mapped("message", &link.get::<_, String>(0)?)?
                        .is_some()
                    {
                        selected = true;
                        break;
                    }
                }
                if !selected {
                    continue;
                }
            }
            self.dst.execute(
                "INSERT INTO execution_turns(id,conversation_id,source_turn_id,started_at,ended_at,elapsed_ms,status) VALUES(?1,?2,?3,?4,?5,?6,CASE ?7 WHEN 'running' THEN 'cancelled' ELSE ?7 END)",
                params![
                    self.assign("turn", &old)?,
                    self.id,
                    old,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, String>(4)?
                ],
            )?;
        }
        let message = Some(("message_id", "message"));
        self.rows("message_turns", message, None, &[("turn_id", "turn")])?;
        self.rows("tool_calls", message, Some(("id", "tool")), &[])?;
        for table in ["message_metadata", "approved_plan_boundaries"] {
            self.rows(table, message, None, &[])?;
        }
        let tool = Some(("tool_call_id", "tool"));
        self.rows(
            "tool_call_origins",
            tool,
            None,
            &[("parent_tool_call_id", "tool")],
        )?;
        for table in ["tool_queued_inputs", "tool_call_notes", "tool_call_timings"] {
            self.rows(table, tool, None, &[])?
        }
        self.rows("mcp_tool_outputs", Some(("tool_use_id", "tool")), None, &[])?;
        for table in ["message_user_content", "native_boundaries"] {
            self.rows(table, message, None, &[])?
        }
        for table in ["permission_approvals", "code_diffs", "attachments"] {
            self.rows(table, message, Some(("id", table)), &[])?
        }
        self.rows("model_requests", message, Some(("id", "request")), &[])?;
        self.rows(
            "request_metadata",
            Some(("request_id", "request")),
            None,
            &[],
        )?;
        // Forward retry references may point into a later selected trajectory.
        // Inspect the JSON only after checking its owning message.
        let mut source = self
            .src
            .prepare("SELECT message_id,requests_json FROM message_trajectory")?;
        let mut rows = source.query([])?;
        while let Some(row) = rows.next()? {
            if self.mapped("message", &row.get::<_, String>(0)?)?.is_none() {
                continue;
            }
            let snapshots: Json = serde_json::from_str(row.get_ref(1)?.as_str()?)?;
            let items = match &snapshots {
                Json::Array(items) => items.as_slice(),
                item => std::slice::from_ref(item),
            };
            for item in items {
                if let Some(id) = item["id"].as_str() {
                    self.assign("request", id)?;
                }
            }
        }
        let mut trajectories = self
            .src
            .prepare("SELECT message_id FROM message_trajectory ORDER BY rowid")?;
        for old in trajectories.query_map([], |r| r.get::<_, String>(0))? {
            let old = old?;
            let Some(new) = self.mapped("message", &old)? else {
                continue;
            };
            let mut requests = message_repo::get_request_snapshots(self.src, &old)?;
            let items = match &mut requests {
                Json::Array(items) => items.as_mut_slice(),
                item => std::slice::from_mut(item),
            };
            for request in items {
                if let Some(id) = request["id"].as_str() {
                    request["id"] = Json::String(self.assign("request", id)?);
                }
                for (field, kind) in [
                    ("contextBoundary", "message"),
                    ("replacesRequestId", "request"),
                ] {
                    if let Some(old) = request[field].as_str() {
                        request[field] = self.mapped(kind, old)?.map_or(Json::Null, Json::String);
                    }
                }
                if let Some(blocks) = request.get_mut("responseBlocks") {
                    self.remap_blocks(&old, blocks)?;
                }
            }
            message_repo::save_request_trajectory(self.dst, &new, &requests)?;
        }

        Ok(())
    }
}
pub(super) fn copy_snapshot(
    source: &SessionDb,
    dst: &Connection,
    id: &str,
    boundary: Branch<'_>,
    configuration: Option<&NewSessionConfiguration>,
    memory: &crate::services::memory_files::prompt::Admission,
    copy_preferences: bool,
) -> Result<()> {
    let path = source
        .stores
        .sessions()
        .join(&source.key)
        .join("session.db");
    let mut src = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tx = src.transaction()?;
    layout::verify(&tx, SESSION)?;
    verify_manifest(
        &tx,
        source.stores.global().store_uuid(),
        &source.id,
        &source.incarnation,
    )?;
    if let Some(configuration) = configuration {
        let key = super::super::settings::accepted_configuration(&configuration.workspace);
        super::super::state::set_local(dst, &key.name, &serde_json::to_string(&configuration.files)?)?;
    }
    if copy_preferences {
        for (key, value) in preference_snapshot(&tx, source.id(), id)? {
            super::super::state::set_local(dst, &key, &value)?;
        }
    }
    if let Branch::AgentFork { directive, .. } = boundary {
        ensure!(
            super::super::state::local(&tx, "fork-of")?.is_none(),
            "A fork cannot start another fork"
        );
        super::super::state::set_local(dst, "fork-of", &serde_json::to_string(source.id())?)?;
        crate::services::permission_authority::record_conn(
            dst,
            id,
            crate::services::permission_authority::Source::User,
            &Json::String(directive.into()),
        )?;
    }
    let mut addition=None;
    let boundary=if let Branch::Summary{message,part,fingerprint,text}=boundary {
        use crate::services::compaction::Part;
        ensure!(crate::services::compaction::fingerprint(&tx,source.id())?.0==fingerprint,"Session changed during compaction");
        let (index,order):(usize,i64)=tx.query_row("SELECT position,sort_order FROM (SELECT id,sort_order,row_number() OVER(ORDER BY sort_order,rowid)-1 AS position FROM messages) WHERE id=?1",[message],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let (copy,order,boundary)=match part {
            Part::UpTo => (Branch::Full,order,true),
            Part::From if index==0 => (Branch::Full,tx.query_row("SELECT max(sort_order)+1 FROM messages",[],|r| r.get(0))?,true),
            Part::From => (Branch::Through{message:None,index:Some(index-1)},order,false),
        };
        addition=Some((part,order,boundary,text));
        copy
    } else { boundary };
    source.stores.step(Phase::ForkSnapshot)?;
    Copier {
        messages: crate::db::transcript::MessageIds::for_shard(dst)?,
        src: &tx,
        dst,
        id,
        session_dir: source
            .stores
            .sessions()
            .join(storage_key(id)?)
            .join("memory"),
        keep_unresolved_origins: matches!(boundary, Branch::Full | Branch::AgentFork { .. }),
    }
    .run(boundary)?;
    if let Some((part,order,boundary,text))=addition {
        if part==crate::services::compaction::Part::UpTo {
            dst.execute("UPDATE messages SET sort_order=sort_order+1 WHERE sort_order>=?1",[order])?;
        }
        let heading = if boundary {
            "Conversation summary — earlier transcript retained in session history"
        } else {
            "Summary of the later conversation — full transcript retained in session history"
        };
        let content = format!("[{heading}]\n{text}");
        if boundary {
            use super::super::boundary;
            boundary::create(
                dst,
                id,
                order,
                &content,
                boundary::Kind::Compaction,
                boundary::Storage::Shard(memory),
            )?;
        } else {
            super::super::boundary::freeze_retained_prefix(dst, memory)?;
            let message = crate::db::transcript::MessageIds::for_shard(dst)?.issue();
            message_repo::insert_message(
                dst,
                &message,
                id,
                "user",
                &content,
                None,
                &chrono::Utc::now().to_rfc3339(),
                false,
                order,
            )?;
            dst.execute(
                "INSERT INTO message_metadata VALUES(?1,?2)",
                [&message, r#"{"promptOrigin":"summary"}"#],
            )?;
        }
    }
    super::super::history::advance(dst)
}

fn preference_snapshot(c: &Connection, source: &str, destination: &str) -> Result<Vec<(String,String)>> {
    let mut values = Vec::new();
    for key in ["cli.session","cli.permissions","cli.display","cli.ide"] {
        if let Some(value) = super::super::state::local(c,key)? { values.push((key.into(),value)); }
    }
    for key in ["cli.permission-mode","plan.return-bypass"] {
        if let Some(value)=super::super::state::local(c,&format!("{key}.{source}"))? {
            values.push((format!("{key}.{destination}"),value));
        }
    }
    Ok(values)
}
