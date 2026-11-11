//! Explicit, host-initiated import of legacy `plan_mode` plans into the
//! session-scoped task records used by `TaskCreate`/`TaskGet`/`TaskUpdate`/
//! `TaskList`.
//!
//! This is deliberately **not** a model-facing tool: nothing in the tool
//! definitions, ToolSearch catalog, prompts or permission rules changes, and
//! no import happens on startup, listing, reading or resume. A caller (today
//! only tests; later an explicit maintenance command) passes the data
//! directory, the canonical workspace, the target session and exactly one
//! selector.
//!
//! The legacy database is opened read-only and is never created, rewritten or
//! deleted. All target rows (root task, step tasks, dependency edges and the
//! legacy-ID mapping) commit in one `BEGIN IMMEDIATE` transaction.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Maximum number of steps accepted from one legacy plan.
const MAX_STEPS: usize = 1000;
/// Maximum size of a legacy text field (task, description, result, timestamp).
const MAX_FIELD_BYTES: usize = 1024 * 1024;
const IMPORT_VERSION: u32 = 1;
const SOURCE_RELATIVE: &str = ".claude/coordination.sqlite3";
/// Version of the source snapshot recorded in `import_ids.source_hash`.
///
/// The snapshot covers the record body *and* whether the selected plan was the
/// workspace's `current_plan`, because both are persisted into the root task.
/// Older mappings have no prefix and are reported as an explicit conflict.
const SNAPSHOT_FORMAT: &str = "v2";

/// Exactly one plan is selected per explicit import.
#[derive(Debug, Clone)]
pub enum ImportSelector {
    /// A specific legacy plan record ID (a UUID).
    Plan(String),
    /// The workspace's `current_plan` row.
    Current,
}

/// Arguments for one explicit import.
#[derive(Debug)]
pub struct ImportRequest<'a> {
    pub data_dir: &'a Path,
    pub workspace: &'a Path,
    pub session: &'a str,
    pub selector: ImportSelector,
    pub dry_run: bool,
}

/// One imported step as reported back to the caller.
#[derive(Debug, Serialize, PartialEq)]
pub struct ImportedStep {
    pub legacy_step_id: String,
    /// `None` on a dry run that would create tasks: IDs are assigned by the
    /// real import and are not predicted here.
    pub task_id: Option<String>,
    pub status: String,
}

/// Structured result of one import attempt.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    /// `created`, `unchanged` or `conflict`.
    pub status: String,
    pub dry_run: bool,
    pub plan_id: String,
    pub source_workspace: String,
    pub current: bool,
    /// `metadata.plan.current` of the mapped root task, when one is known.
    pub stored_current: Option<bool>,
    pub root_task_id: Option<String>,
    pub steps: Vec<ImportedStep>,
    /// Tasks/edges this call created (`0` unless `status == "created"`).
    pub created_tasks: usize,
    pub created_edges: usize,
    /// Already-mapped tasks observed for an unchanged import.
    pub unchanged_tasks: usize,
    pub conflicts: Vec<String>,
    pub message: String,
}

struct ParsedStep {
    step_id: String,
    description: String,
    dependencies: Vec<String>,
    status: &'static str,
    legacy_status: String,
    result: Value,
}

struct ParsedPlan {
    plan_id: String,
    task: String,
    status: &'static str,
    legacy_status: String,
    steps: Vec<ParsedStep>,
    created_at: String,
    updated_at: String,
    current: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct MappingRow {
    kind: String,
    legacy_id: String,
    task_id: String,
    source_hash: String,
}

enum Decision {
    Create,
    Unchanged,
    Conflict(Vec<String>),
}

struct Created {
    root_id: String,
    step_ids: Vec<String>,
    tasks: usize,
    edges: usize,
}

/// Imports one legacy plan into `<data_dir>/tasks/<session>.sqlite3`.
///
/// The caller must pass a canonical workspace, directly or via a path this
/// function canonicalizes. A dry run validates and reports without creating
/// the target database, its directory or any task rows.
pub fn import(request: ImportRequest<'_>) -> Result<ImportReport> {
    let session = uuid::Uuid::parse_str(request.session)
        .context("Invalid target session ID")?
        .to_string();
    let workspace = request
        .workspace
        .canonicalize()
        .context("Workspace does not exist")?;
    ensure!(workspace.is_dir(), "Workspace must be a directory");
    let source = source_database(&workspace)?;
    let (record, plan_id, current) = read_source(&source, &request.selector)?;
    let plan = parse_plan(&record, &plan_id, current)?;
    let source_hash = snapshot_hash(&record, current);
    let workspace_key = workspace.to_string_lossy().into_owned();
    let target = request
        .data_dir
        .join("tasks")
        .join(format!("{session}.sqlite3"));
    if request.dry_run {
        return dry_run(&target, &workspace_key, &plan, &source_hash);
    }
    apply(&target, &workspace_key, &plan, &source_hash)
}

/// Canonical workspace; the legacy database must live inside it.
fn source_database(workspace: &Path) -> Result<PathBuf> {
    let claude = workspace.join(".claude");
    match std::fs::symlink_metadata(&claude) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "The .claude directory must not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let path = workspace.join(SOURCE_RELATIVE);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("No legacy plan database in {}", workspace.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Refusing a symlinked legacy plan database"
    );
    ensure!(metadata.is_file(), "Legacy plan database is not a file");
    let canonical = path.canonicalize()?;
    ensure!(
        canonical.starts_with(workspace),
        "Legacy plan database is outside the workspace"
    );
    Ok(canonical)
}

/// Reads the plan record and `current_plan` in one read-only snapshot.
fn read_source(source: &Path, selector: &ImportSelector) -> Result<(Value, String, bool)> {
    let mut db = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
    let id = match selector {
        ImportSelector::Plan(id) => uuid::Uuid::parse_str(id)
            .context("Invalid legacy plan ID")?
            .to_string(),
        ImportSelector::Current => tx
            .query_row("SELECT id FROM current_plan WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .optional()?
            .context("The workspace has no current plan")?,
    };
    let body: Option<String> = tx
        .query_row(
            "SELECT body FROM records WHERE kind='plan' AND id=?1",
            [&id],
            |row| row.get(0),
        )
        .optional()?;
    let body = body.with_context(|| format!("Legacy plan not found: {id}"))?;
    let record: Value =
        serde_json::from_str(&body).context("Legacy plan record is not valid JSON")?;
    let current: Option<String> = tx
        .query_row("SELECT id FROM current_plan WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let is_current = current.as_deref() == Some(id.as_str());
    Ok((record, id, is_current))
}

fn parse_plan(record: &Value, selected_id: &str, current: bool) -> Result<ParsedPlan> {
    ensure!(
        record["plan_id"].as_str() == Some(selected_id),
        "Legacy plan record ID does not match the selected plan"
    );
    let task = bounded_string(record, "task", "plan task")?;
    ensure!(!task.trim().is_empty(), "Legacy plan task is empty");
    let legacy_status = record["status"]
        .as_str()
        .context("Legacy plan status is missing")?
        .to_owned();
    let status = match legacy_status.as_str() {
        "draft" => "pending",
        "active" => "in_progress",
        "abandoned" => "cancelled",
        other => bail!("Unsupported legacy plan status: {other}"),
    };
    let raw_steps = record["steps"]
        .as_array()
        .context("Legacy plan steps must be an array")?;
    ensure!(
        raw_steps.len() <= MAX_STEPS,
        "Legacy plan has more than {MAX_STEPS} steps"
    );
    let mut steps = Vec::with_capacity(raw_steps.len());
    let mut seen = HashSet::new();
    for step in raw_steps {
        let step_id = step["step_id"]
            .as_str()
            .context("Legacy step ID is missing")?;
        ensure!(!step_id.trim().is_empty(), "Legacy step ID is empty");
        ensure!(
            seen.insert(step_id.to_owned()),
            "Duplicate legacy step ID: {step_id}"
        );
        let description = bounded_string(step, "description", "step description")?;
        ensure!(
            !description.trim().is_empty(),
            "Legacy step {step_id} has no description"
        );
        let legacy_status = step["status"]
            .as_str()
            .context("Legacy step status is missing")?
            .to_owned();
        let status = match legacy_status.as_str() {
            "pending" | "blocked" | "ready" => "pending",
            "in_progress" => "in_progress",
            "completed" => "completed",
            "skipped" => "cancelled",
            other => bail!("Unsupported legacy step status: {other}"),
        };
        let dependencies = match step.get("dependencies") {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .context("Legacy dependency must be a step ID")
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("Legacy step dependencies must be an array"),
        };
        let result = match step.get("result") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::String(text)) => {
                ensure!(
                    text.len() <= MAX_FIELD_BYTES,
                    "Legacy step result exceeds 1 MiB"
                );
                Value::String(text.clone())
            }
            Some(_) => bail!("Legacy step result must be a string or null"),
        };
        steps.push(ParsedStep {
            step_id: step_id.to_owned(),
            description,
            dependencies,
            status,
            legacy_status,
            result,
        });
    }
    validate_graph(&steps)?;
    Ok(ParsedPlan {
        plan_id: selected_id.to_owned(),
        task,
        status,
        legacy_status,
        steps,
        created_at: timestamp(record, "created_at"),
        updated_at: timestamp(record, "updated_at"),
        current,
    })
}

fn bounded_string(value: &Value, key: &str, label: &str) -> Result<String> {
    let text = value[key]
        .as_str()
        .with_context(|| format!("Legacy {label} is missing"))?;
    ensure!(
        text.len() <= MAX_FIELD_BYTES,
        "Legacy {label} exceeds 1 MiB"
    );
    Ok(text.to_owned())
}

/// A usable RFC 3339 timestamp, else the import time.
fn timestamp(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .filter(|text| chrono::DateTime::parse_from_rfc3339(text).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339())
}

/// Validates every dependency reference and rejects cycles over the whole plan.
fn validate_graph(steps: &[ParsedStep]) -> Result<()> {
    let index: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(position, step)| (step.step_id.as_str(), position))
        .collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); steps.len()];
    let mut indegree = vec![0usize; steps.len()];
    for (position, step) in steps.iter().enumerate() {
        let mut unique = HashSet::new();
        for dependency in &step.dependencies {
            let target = *index.get(dependency.as_str()).with_context(|| {
                format!(
                    "Legacy step {} depends on missing step {dependency}",
                    step.step_id
                )
            })?;
            ensure!(
                target != position,
                "Legacy step {} depends on itself",
                step.step_id
            );
            if unique.insert(target) {
                indegree[position] += 1;
                dependents[target].push(position);
            }
        }
    }
    let mut queue: VecDeque<usize> = (0..steps.len())
        .filter(|step| indegree[*step] == 0)
        .collect();
    let mut removed = 0usize;
    while let Some(step) = queue.pop_front() {
        removed += 1;
        for dependent in &dependents[step] {
            indegree[*dependent] -= 1;
            if indegree[*dependent] == 0 {
                queue.push_back(*dependent);
            }
        }
    }
    ensure!(
        removed == steps.len(),
        "Legacy plan dependencies contain a cycle"
    );
    Ok(())
}

/// Identifies the exact source state this import persists: the record body and
/// the selected plan's current-plan membership. A mapping is only `unchanged`
/// when both still match.
fn snapshot_hash(record: &Value, current: bool) -> String {
    let snapshot = json!({"format": SNAPSHOT_FORMAT, "record": record, "current": current});
    let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
    let digest: String = ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{SNAPSHOT_FORMAT}:{digest}")
}

fn read_existing(path: &Path) -> Result<Option<Connection>> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(Duration::from_secs(5))?;
            Ok(Some(db))
        }
        Ok(_) => bail!("Task storage path is not a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn read_mapping(conn: &Connection, workspace: &str, plan_id: &str) -> Result<Vec<MappingRow>> {
    if !table_exists(conn, "import_ids")? {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "SELECT kind, legacy_id, task_id, source_hash FROM import_ids \
         WHERE source_workspace=?1 AND plan_key=?2 ORDER BY kind, legacy_id",
    )?;
    let rows = statement.query_map(params![workspace, plan_id], |row| {
        Ok(MappingRow {
            kind: row.get(0)?,
            legacy_id: row.get(1)?,
            task_id: row.get(2)?,
            source_hash: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

struct Classification {
    decision: Decision,
    rows: Vec<MappingRow>,
    stored_current: Option<bool>,
}

fn stored_root_current(conn: &Connection, root: Option<&str>) -> Result<Option<bool>> {
    let Some(root) = root else {
        return Ok(None);
    };
    if !table_exists(conn, "tasks")? {
        return Ok(None);
    }
    let body: Option<String> = conn
        .query_row("SELECT body FROM tasks WHERE id=?1", [root], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(body
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .and_then(|task| task["metadata"]["plan"]["current"].as_bool()))
}

/// Decides whether this import creates rows, is an unchanged repeat, or
/// conflicts. It never writes.
fn classify(
    conn: &Connection,
    workspace: &str,
    plan: &ParsedPlan,
    source_hash: &str,
) -> Result<Classification> {
    let rows = read_mapping(conn, workspace, &plan.plan_id)?;
    if rows.is_empty() {
        return Ok(Classification {
            decision: Decision::Create,
            rows,
            stored_current: None,
        });
    }
    let expected: HashSet<(String, String)> = std::iter::once(("plan", plan.plan_id.clone()))
        .chain(plan.steps.iter().map(|step| ("step", step.step_id.clone())))
        .map(|(kind, legacy)| (kind.to_owned(), legacy))
        .collect();
    let actual: HashSet<(String, String)> = rows
        .iter()
        .map(|row| (row.kind.clone(), row.legacy_id.clone()))
        .collect();
    let mut conflicts = Vec::new();
    if expected != actual {
        conflicts.push(
            "The legacy import mapping is incomplete or references an unexpected record".into(),
        );
    }
    let stored_current = stored_root_current(conn, mapped_root(&rows).as_deref())?;
    let hashes: HashSet<&str> = rows.iter().map(|row| row.source_hash.as_str()).collect();
    if hashes.len() != 1 {
        conflicts.push("The legacy import mapping has inconsistent source hashes".into());
    } else {
        let stored = *hashes.iter().next().unwrap();
        if !stored.starts_with(&format!("{SNAPSHOT_FORMAT}:")) {
            conflicts.push(format!(
                "This plan was imported by an older importer snapshot format ({stored}); \
                 re-import is required to record current-plan membership. The existing tasks \
                 were not changed."
            ));
        } else if stored != source_hash {
            conflicts.push(format!(
                "The legacy plan or its current-plan membership changed since it was imported \
                 (source current={}, stored current={}, stored hash {stored}, current hash {source_hash})",
                plan.current,
                stored_current
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
    }
    if table_exists(conn, "tasks")? {
        let mut missing = Vec::new();
        for row in &rows {
            let exists: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
                [&row.task_id],
                |value| value.get(0),
            )?;
            if exists == 0 {
                missing.push(row.task_id.clone());
            }
        }
        if !missing.is_empty() {
            missing.sort();
            conflicts.push(format!(
                "Mapped task records are missing: {}",
                missing.join(", ")
            ));
        }
    } else {
        conflicts.push("Task storage is missing its tasks table".into());
    }
    let decision = if conflicts.is_empty() {
        Decision::Unchanged
    } else {
        Decision::Conflict(conflicts)
    };
    Ok(Classification {
        decision,
        rows,
        stored_current,
    })
}

fn mapped_root(rows: &[MappingRow]) -> Option<String> {
    rows.iter()
        .find(|row| row.kind == "plan")
        .map(|row| row.task_id.clone())
}

fn mapped_steps(plan: &ParsedPlan, rows: &[MappingRow]) -> Vec<ImportedStep> {
    plan.steps
        .iter()
        .map(|step| ImportedStep {
            legacy_step_id: step.step_id.clone(),
            task_id: rows
                .iter()
                .find(|row| row.kind == "step" && row.legacy_id == step.step_id)
                .map(|row| row.task_id.clone()),
            status: step.status.to_owned(),
        })
        .collect()
}

fn report(
    plan: &ParsedPlan,
    workspace: &str,
    status: &str,
    stored_current: Option<bool>,
    root_task_id: Option<String>,
    steps: Vec<ImportedStep>,
    created_tasks: usize,
    created_edges: usize,
    unchanged_tasks: usize,
    conflicts: Vec<String>,
    message: String,
) -> ImportReport {
    ImportReport {
        status: status.to_owned(),
        dry_run: false,
        plan_id: plan.plan_id.clone(),
        source_workspace: workspace.to_owned(),
        current: plan.current,
        stored_current,
        root_task_id,
        steps,
        created_tasks,
        created_edges,
        unchanged_tasks,
        conflicts,
        message,
    }
}

fn dry_run(
    target: &Path,
    workspace: &str,
    plan: &ParsedPlan,
    source_hash: &str,
) -> Result<ImportReport> {
    let edges = edge_count(plan);
    let Some(db) = read_existing(target)? else {
        let mut report = report(
            plan,
            workspace,
            "created",
            None,
            None,
            plan.steps
                .iter()
                .map(|step| ImportedStep {
                    legacy_step_id: step.step_id.clone(),
                    task_id: None,
                    status: step.status.to_owned(),
                })
                .collect(),
            0,
            0,
            0,
            Vec::new(),
            format!(
                "Dry run: the plan is not imported in this session; a real import would create {} task(s) and {edges} dependency edge(s). Task IDs are assigned then.",
                plan.steps.len() + 1
            ),
        );
        report.dry_run = true;
        return Ok(report);
    };
    let classification = classify(&db, workspace, plan, source_hash)?;
    let Classification {
        decision,
        rows,
        stored_current,
    } = classification;
    let (status, conflicts, message, unchanged) = match decision {
        Decision::Create => (
            "created",
            Vec::new(),
            format!(
                "Dry run: a real import would create {} task(s) and {edges} dependency edge(s). Task IDs are assigned then.",
                plan.steps.len() + 1
            ),
            0,
        ),
        Decision::Unchanged => (
            "unchanged",
            Vec::new(),
            "Dry run: the plan is already imported with an identical source; a real import would write nothing.".to_owned(),
            rows.len(),
        ),
        Decision::Conflict(conflicts) => (
            "conflict",
            conflicts,
            "Dry run: the source or its mapping conflicts; a real import would write nothing.".to_owned(),
            0,
        ),
    };
    let mut value = report(
        plan,
        workspace,
        status,
        stored_current,
        mapped_root(&rows),
        mapped_steps(plan, &rows),
        0,
        0,
        unchanged,
        conflicts,
        message,
    );
    value.dry_run = true;
    Ok(value)
}

fn edge_count(plan: &ParsedPlan) -> usize {
    plan.steps
        .iter()
        .map(|step| step.dependencies.iter().collect::<HashSet<_>>().len())
        .sum()
}

fn apply(
    target: &Path,
    workspace: &str,
    plan: &ParsedPlan,
    source_hash: &str,
) -> Result<ImportReport> {
    let directory = target.parent().context("Task storage parent missing")?;
    std::fs::create_dir_all(directory)?;
    let mut db = Connection::open(target)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Re-check under the write lock: a concurrent import may have completed
    // between validation and this transaction.
    let classification = classify(&tx, workspace, plan, source_hash)?;
    let Classification {
        decision,
        rows,
        stored_current,
    } = classification;
    match decision {
        Decision::Unchanged => {
            let value = report(
                plan,
                workspace,
                "unchanged",
                stored_current,
                mapped_root(&rows),
                mapped_steps(plan, &rows),
                0,
                0,
                rows.len(),
                Vec::new(),
                "The source plan is unchanged since the previous import; nothing was written."
                    .to_owned(),
            );
            tx.commit()?;
            return Ok(value);
        }
        Decision::Conflict(conflicts) => {
            let value = report(
                plan,
                workspace,
                "conflict",
                stored_current,
                mapped_root(&rows),
                mapped_steps(plan, &rows),
                0,
                0,
                0,
                conflicts,
                "The source plan or its mapping conflicts with the imported tasks; nothing was written."
                    .to_owned(),
            );
            tx.commit()?;
            return Ok(value);
        }
        Decision::Create => {}
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, body TEXT NOT NULL); \
         CREATE TABLE IF NOT EXISTS todo(singleton INTEGER PRIMARY KEY CHECK(singleton=1), body TEXT NOT NULL); \
         CREATE TABLE IF NOT EXISTS import_ids(source_workspace TEXT NOT NULL, plan_key TEXT NOT NULL, \
           kind TEXT NOT NULL, legacy_id TEXT NOT NULL, task_id TEXT NOT NULL, imported_at TEXT NOT NULL, \
           source_hash TEXT NOT NULL, PRIMARY KEY(source_workspace, plan_key, kind, legacy_id));",
    )?;
    let created = insert_plan(&tx, workspace, plan, source_hash)?;
    tx.commit()?;
    let steps = plan
        .steps
        .iter()
        .zip(&created.step_ids)
        .map(|(step, task_id)| ImportedStep {
            legacy_step_id: step.step_id.clone(),
            task_id: Some(task_id.clone()),
            status: step.status.to_owned(),
        })
        .collect();
    Ok(report(
        plan,
        workspace,
        "created",
        Some(plan.current),
        Some(created.root_id),
        steps,
        created.tasks,
        created.edges,
        0,
        Vec::new(),
        format!(
            "Imported {} task(s) and {} dependency edge(s) from the workspace plan.",
            created.tasks, created.edges
        ),
    ))
}

fn insert_plan(
    tx: &Transaction<'_>,
    workspace: &str,
    plan: &ParsedPlan,
    source_hash: &str,
) -> Result<Created> {
    let now = chrono::Utc::now().to_rfc3339();
    let root_id = uuid::Uuid::new_v4().to_string();
    let step_ids: Vec<String> = plan
        .steps
        .iter()
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let index: HashMap<&str, usize> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(position, step)| (step.step_id.as_str(), position))
        .collect();
    let mut blocked_by: Vec<Vec<String>> = vec![Vec::new(); plan.steps.len()];
    let mut blocks: Vec<Vec<String>> = vec![Vec::new(); plan.steps.len()];
    let mut edges = 0usize;
    for (position, step) in plan.steps.iter().enumerate() {
        let mut unique = HashSet::new();
        for dependency in &step.dependencies {
            let target = index[dependency.as_str()];
            if unique.insert(target) {
                blocked_by[position].push(step_ids[target].clone());
                blocks[target].push(step_ids[position].clone());
                edges += 1;
            }
        }
    }
    let root_metadata = json!({"plan": {
        "origin": "coordination",
        "kind": "plan",
        "plan_id": plan.plan_id,
        "source_workspace": workspace,
        "legacy_status": plan.legacy_status,
        "current": plan.current,
        "created_at": plan.created_at,
        "updated_at": plan.updated_at,
        "source": SOURCE_RELATIVE,
        "import_version": IMPORT_VERSION
    }});
    let root = task_body(
        &root_id,
        &summary(&plan.task),
        &plan.task,
        plan.status,
        Vec::new(),
        Vec::new(),
        Value::Null,
        root_metadata,
        &plan.created_at,
        &plan.updated_at,
    );
    insert_task(tx, &root)?;
    for (position, step) in plan.steps.iter().enumerate() {
        let metadata = json!({"plan": {
            "origin": "coordination",
            "kind": "step",
            "plan_id": plan.plan_id,
            "source_workspace": workspace,
            "step_id": step.step_id,
            "order": position,
            "legacy_status": step.legacy_status,
            "legacy_result": step.result,
            "source": SOURCE_RELATIVE,
            "import_version": IMPORT_VERSION
        }});
        let body = task_body(
            &step_ids[position],
            &summary(&step.description),
            &step.description,
            step.status,
            blocked_by[position].clone(),
            blocks[position].clone(),
            step.result.clone(),
            metadata,
            &plan.created_at,
            &plan.updated_at,
        );
        insert_task(tx, &body)?;
    }
    tx.execute(
        "INSERT INTO import_ids(source_workspace, plan_key, kind, legacy_id, task_id, imported_at, source_hash) \
         VALUES(?1, ?2, 'plan', ?2, ?3, ?4, ?5)",
        params![workspace, plan.plan_id, root_id, now, source_hash],
    )?;
    for (position, step) in plan.steps.iter().enumerate() {
        tx.execute(
            "INSERT INTO import_ids(source_workspace, plan_key, kind, legacy_id, task_id, imported_at, source_hash) \
             VALUES(?1, ?2, 'step', ?3, ?4, ?5, ?6)",
            params![
                workspace,
                plan.plan_id,
                step.step_id,
                step_ids[position],
                now,
                source_hash
            ],
        )?;
    }
    if plan.current {
        // Only this source workspace and this session's imported roots change;
        // unrelated tasks and other workspaces are untouched.
        let mut statement = tx.prepare("SELECT id, body FROM tasks WHERE id!=?1")?;
        let others = statement
            .query_map([&root_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (id, body) in others {
            let mut task: Value = serde_json::from_str(&body)?;
            let matched = {
                let meta = &task["metadata"]["plan"];
                meta["origin"] == "coordination"
                    && meta["kind"] == "plan"
                    && meta["source_workspace"].as_str() == Some(workspace)
                    && meta["current"] == true
            };
            if matched {
                task["metadata"]["plan"]["current"] = json!(false);
                tx.execute(
                    "UPDATE tasks SET body=?2 WHERE id=?1",
                    params![id, task.to_string()],
                )?;
            }
        }
    }
    Ok(Created {
        root_id,
        step_ids,
        tasks: plan.steps.len() + 1,
        edges,
    })
}

fn insert_task(tx: &Transaction<'_>, task: &Value) -> Result<()> {
    tx.execute(
        "INSERT INTO tasks(id, body) VALUES(?1, ?2)",
        params![task["id"].as_str(), task.to_string()],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn task_body(
    id: &str,
    subject: &str,
    description: &str,
    status: &str,
    blocked_by: Vec<String>,
    blocks: Vec<String>,
    output: Value,
    metadata: Value,
    created_at: &str,
    updated_at: &str,
) -> Value {
    json!({
        "id": id,
        "subject": subject,
        "description": description,
        "status": status,
        "blockedBy": blocked_by,
        "blocks": blocks,
        "createdAt": created_at,
        "updatedAt": updated_at,
        "name": subject,
        "priority": "medium",
        "output": output,
        "metadata": metadata
    })
}

/// First non-empty line, trimmed and truncated for a task subject.
fn summary(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn plan_value(id: &str, task: &str, steps: Vec<Value>, status: &str) -> Value {
        json!({
            "plan_id": id,
            "task": task,
            "steps": steps,
            "status": status,
            "created_at": "2026-01-01T00:00:00+00:00",
            "updated_at": "2026-01-02T00:00:00+00:00"
        })
    }

    fn step_value(
        id: &str,
        description: &str,
        dependencies: &[&str],
        status: &str,
        result: Value,
    ) -> Value {
        json!({
            "step_id": id,
            "description": description,
            "dependencies": dependencies,
            "status": status,
            "result": result
        })
    }

    fn workspace_with_plans(root: &Path, plans: &[Value], current: Option<&str>) -> PathBuf {
        let workspace = root.join("workspace");
        let claude = workspace.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let db = Connection::open(claude.join("coordination.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE records(kind TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(kind,id)); \
             CREATE TABLE current_plan(singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL);",
        )
        .unwrap();
        for plan in plans {
            db.execute(
                "INSERT INTO records(kind,id,body) VALUES('plan',?1,?2)",
                params![plan["plan_id"].as_str().unwrap(), plan.to_string()],
            )
            .unwrap();
        }
        if let Some(id) = current {
            db.execute("INSERT INTO current_plan(singleton,id) VALUES(1,?1)", [id])
                .unwrap();
        }
        drop(db);
        workspace
    }

    fn write_plan(workspace: &Path, plan: &Value) {
        let db = Connection::open(workspace.join(".claude/coordination.sqlite3")).unwrap();
        db.execute(
            "INSERT INTO records(kind,id,body) VALUES('plan',?1,?2) \
             ON CONFLICT(kind,id) DO UPDATE SET body=excluded.body",
            params![plan["plan_id"].as_str().unwrap(), plan.to_string()],
        )
        .unwrap();
    }

    fn set_current(workspace: &Path, id: Option<&str>) {
        let db = Connection::open(workspace.join(".claude/coordination.sqlite3")).unwrap();
        db.execute("DELETE FROM current_plan", []).unwrap();
        if let Some(id) = id {
            db.execute("INSERT INTO current_plan(singleton,id) VALUES(1,?1)", [id])
                .unwrap();
        }
    }

    fn source_bytes(workspace: &Path) -> Vec<u8> {
        std::fs::read(workspace.join(".claude/coordination.sqlite3")).unwrap()
    }

    /// The stored legacy record body; `current_plan` edits never change it.
    fn record_body(workspace: &Path, plan_id: &str) -> String {
        let db = Connection::open(workspace.join(".claude/coordination.sqlite3")).unwrap();
        db.query_row(
            "SELECT body FROM records WHERE kind='plan' AND id=?1",
            [plan_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Every mapping row for exact before/after comparison.
    fn mapping_snapshot(data: &Path, session: &str) -> Vec<(String, String, String, String)> {
        let path = target_path(data, session);
        if !path.exists() {
            return Vec::new();
        }
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        if !table_exists(&db, "import_ids").unwrap() {
            return Vec::new();
        }
        let mut statement = db
            .prepare(
                "SELECT kind, legacy_id, task_id, source_hash FROM import_ids \
                 ORDER BY source_workspace, plan_key, kind, legacy_id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn stored_root_current(data: &Path, session: &str, root: &str) -> bool {
        get_task(data, session, root)["metadata"]["plan"]["current"]
            .as_bool()
            .unwrap()
    }

    fn target_path(data: &Path, session: &str) -> PathBuf {
        data.join("tasks").join(format!("{session}.sqlite3"))
    }

    fn list_tasks(data: &Path, session: &str) -> Vec<Value> {
        crate::tools::tasks::execute_for_session("TaskList", json!({}), data, session).unwrap()
            ["tasks"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn get_task(data: &Path, session: &str, id: &str) -> Value {
        crate::tools::tasks::execute_for_session("TaskGet", json!({"id":id}), data, session)
            .unwrap()
    }

    fn mapping_count(data: &Path, session: &str, workspace: &Path, plan_id: &str) -> usize {
        let path = target_path(data, session);
        if !path.exists() {
            return 0;
        }
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        read_mapping(
            &db,
            workspace.canonicalize().unwrap().to_string_lossy().as_ref(),
            plan_id,
        )
        .unwrap()
        .len()
    }

    fn import_plan(
        data: &Path,
        workspace: &Path,
        session: &str,
        plan_id: &str,
    ) -> Result<ImportReport> {
        import(ImportRequest {
            data_dir: data,
            workspace,
            session,
            selector: ImportSelector::Plan(plan_id.to_owned()),
            dry_run: false,
        })
    }

    #[test]
    fn imports_legacy_plan_and_preserves_status_results_and_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let plan = plan_value(
            &plan_id,
            "Ship the migration",
            vec![
                step_value("step-0", "Write code", &[], "completed", json!("done")),
                step_value("step-1", "Review code", &["step-0"], "pending", Value::Null),
                step_value("step-2", "Write docs", &[], "skipped", json!("not needed")),
                step_value(
                    "step-3",
                    "Release",
                    &["step-1", "step-2"],
                    "blocked",
                    Value::Null,
                ),
            ],
            "active",
        );
        let workspace = workspace_with_plans(temp.path(), &[plan], Some(&plan_id));
        let source = source_bytes(&workspace);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.status, "created");
        assert_eq!(report.created_tasks, 5);
        assert_eq!(report.created_edges, 3);
        assert!(report.current);
        assert_eq!(report.steps.len(), 4);
        let root_id = report.root_task_id.clone().expect("root task");
        let tasks = list_tasks(&data, &session);
        assert_eq!(tasks.len(), 5);

        let root = get_task(&data, &session, &root_id);
        assert_eq!(root["status"], "in_progress");
        assert_eq!(root["description"], "Ship the migration");
        assert_eq!(root["metadata"]["plan"]["legacy_status"], "active");
        assert_eq!(root["metadata"]["plan"]["current"], true);
        assert_eq!(
            root["metadata"]["plan"]["source_workspace"],
            workspace.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(root["metadata"]["plan"]["import_version"], 1);
        assert_eq!(root["blockedBy"], json!([]));

        let step_id = |legacy: &str| {
            report
                .steps
                .iter()
                .find(|step| step.legacy_step_id == legacy)
                .unwrap()
                .task_id
                .clone()
                .unwrap()
        };
        let step_0 = get_task(&data, &session, &step_id("step-0"));
        assert_eq!(step_0["status"], "completed");
        assert_eq!(step_0["output"], "done");
        assert_eq!(step_0["metadata"]["plan"]["legacy_result"], "done");
        assert_eq!(step_0["blocks"], json!([step_id("step-1")]));

        let step_1 = get_task(&data, &session, &step_id("step-1"));
        assert_eq!(step_1["status"], "pending");
        assert_eq!(step_1["blockedBy"], json!([step_id("step-0")]));
        assert_eq!(step_1["metadata"]["plan"]["order"], 1);
        assert_eq!(step_1["metadata"]["plan"]["legacy_status"], "pending");

        let step_2 = get_task(&data, &session, &step_id("step-2"));
        assert_eq!(step_2["status"], "cancelled");
        assert_eq!(step_2["output"], "not needed");
        assert_eq!(step_2["metadata"]["plan"]["legacy_status"], "skipped");

        let step_3 = get_task(&data, &session, &step_id("step-3"));
        assert_eq!(step_3["status"], "pending");
        assert_eq!(
            step_3["blockedBy"],
            json!([step_id("step-1"), step_id("step-2")])
        );
        assert_eq!(step_3["metadata"]["plan"]["legacy_status"], "blocked");

        // Ordinary reads never touch the legacy store.
        assert_eq!(source_bytes(&workspace), source);
        let _ = list_tasks(&data, &session);
        let _ = get_task(&data, &session, &root_id);
        assert_eq!(source_bytes(&workspace), source);

        // Rows persist without the legacy source present.
        std::fs::remove_file(workspace.join(".claude/coordination.sqlite3")).unwrap();
        let reopened = get_task(&data, &session, &root_id);
        assert_eq!(reopened["status"], "in_progress");
        assert_eq!(reopened["steps"], Value::Null);
        assert_eq!(list_tasks(&data, &session).len(), 5);
    }

    #[test]
    fn dry_run_missing_source_and_invalid_arguments_create_no_target_storage() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let plan = plan_value(
            &plan_id,
            "Plan",
            vec![step_value("step-0", "One", &[], "pending", Value::Null)],
            "draft",
        );
        let workspace = workspace_with_plans(temp.path(), &[plan], None);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let source = source_bytes(&workspace);

        let report = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Plan(plan_id.clone()),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(report.status, "created");
        assert!(report.dry_run);
        assert!(report.root_task_id.is_none());
        assert!(report.steps.iter().all(|step| step.task_id.is_none()));
        assert!(
            !data.exists(),
            "a dry run created the target data directory"
        );
        assert_eq!(source_bytes(&workspace), source);

        // A missing legacy database is an error, not an empty import.
        let empty_workspace = temp.path().join("empty-workspace");
        std::fs::create_dir_all(&empty_workspace).unwrap();
        let error = import_plan(&data, &empty_workspace, &session, &plan_id).unwrap_err();
        assert!(
            error.to_string().contains("No legacy plan database"),
            "{error}"
        );
        assert!(!data.exists(), "a missing source created target storage");

        // Invalid session and selector IDs are rejected before any path work.
        let error = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: "not-a-uuid",
            selector: ImportSelector::Plan(plan_id.clone()),
            dry_run: false,
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("Invalid target session ID"),
            "{error}"
        );
        assert!(!data.exists());

        let error = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Plan("not-a-uuid".into()),
            dry_run: false,
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("Invalid legacy plan ID"),
            "{error}"
        );
        assert!(!data.exists());

        // The current selector without a current plan fails without writing.
        let error = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Current,
            dry_run: false,
        })
        .unwrap_err();
        assert!(error.to_string().contains("no current plan"), "{error}");
        assert!(!data.exists());

        #[cfg(unix)]
        {
            let outside = temp.path().join("outside-workspace");
            workspace_with_plans(&outside, &[], None);
            std::fs::remove_dir_all(outside.join("workspace")).unwrap();
            let outside = outside.join("workspace");
            std::fs::create_dir_all(&outside).unwrap();
            let claude = outside.join(".claude");
            std::fs::create_dir_all(&claude).unwrap();
            let db = Connection::open(claude.join("coordination.sqlite3")).unwrap();
            db.execute_batch(
                "CREATE TABLE records(kind TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(kind,id)); \
                 CREATE TABLE current_plan(singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL);",
            )
            .unwrap();
            drop(db);
            let linked = temp.path().join("linked-workspace");
            std::fs::create_dir_all(&linked).unwrap();
            std::os::unix::fs::symlink(&claude, linked.join(".claude")).unwrap();
            let error = import_plan(&data, &linked, &session, &plan_id).unwrap_err();
            assert!(error.to_string().contains("symlink"), "{error}");
            assert!(!data.exists());
        }
    }

    #[test]
    fn invalid_plans_and_midway_failures_leave_target_rows_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(
                &plan_id,
                "Plan",
                vec![step_value("step-0", "One", &[], "pending", Value::Null)],
                "active",
            )],
            None,
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        crate::tools::tasks::execute_for_session(
            "TaskCreate",
            json!({"subject":"unrelated"}),
            &data,
            &session,
        )
        .unwrap();
        let before = list_tasks(&data, &session);

        let invalid = [
            (
                "unknown status",
                plan_value(
                    &plan_id,
                    "Plan",
                    vec![step_value("step-0", "One", &[], "weird", Value::Null)],
                    "active",
                ),
            ),
            (
                "duplicate step",
                plan_value(
                    &plan_id,
                    "Plan",
                    vec![
                        step_value("step-0", "One", &[], "pending", Value::Null),
                        step_value("step-0", "Two", &[], "pending", Value::Null),
                    ],
                    "active",
                ),
            ),
            (
                "dangling dependency",
                plan_value(
                    &plan_id,
                    "Plan",
                    vec![step_value(
                        "step-0",
                        "One",
                        &["step-9"],
                        "pending",
                        Value::Null,
                    )],
                    "active",
                ),
            ),
            (
                "self dependency",
                plan_value(
                    &plan_id,
                    "Plan",
                    vec![step_value(
                        "step-0",
                        "One",
                        &["step-0"],
                        "pending",
                        Value::Null,
                    )],
                    "active",
                ),
            ),
            (
                "cycle",
                plan_value(
                    &plan_id,
                    "Plan",
                    vec![
                        step_value("step-0", "One", &["step-1"], "pending", Value::Null),
                        step_value("step-1", "Two", &["step-0"], "pending", Value::Null),
                    ],
                    "active",
                ),
            ),
            (
                "unknown plan status",
                plan_value(
                    &plan_id,
                    "Plan",
                    vec![step_value("step-0", "One", &[], "pending", Value::Null)],
                    "archived",
                ),
            ),
        ];
        for (name, plan) in invalid {
            write_plan(&workspace, &plan);
            let source = source_bytes(&workspace);
            let error = import_plan(&data, &workspace, &session, &plan_id).unwrap_err();
            assert!(!error.to_string().is_empty(), "{name} was accepted");
            assert_eq!(
                source_bytes(&workspace),
                source,
                "{name} changed the source"
            );
            assert_eq!(list_tasks(&data, &session), before, "{name} changed tasks");
            assert_eq!(
                mapping_count(&data, &session, &workspace, &plan_id),
                0,
                "{name} wrote a mapping"
            );
        }

        // Oversized fields and step counts are rejected.
        let oversized = plan_value(
            &plan_id,
            "Plan",
            vec![step_value(
                "step-0",
                &"x".repeat(MAX_FIELD_BYTES + 1),
                &[],
                "pending",
                Value::Null,
            )],
            "active",
        );
        write_plan(&workspace, &oversized);
        assert!(import_plan(&data, &workspace, &session, &plan_id).is_err());
        assert_eq!(list_tasks(&data, &session), before);

        let many: Vec<Value> = (0..=MAX_STEPS)
            .map(|index| {
                step_value(
                    &format!("step-{index}"),
                    "step",
                    &[],
                    "pending",
                    Value::Null,
                )
            })
            .collect();
        write_plan(&workspace, &plan_value(&plan_id, "Plan", many, "active"));
        assert!(import_plan(&data, &workspace, &session, &plan_id).is_err());
        assert_eq!(list_tasks(&data, &session), before);

        // A failure after some row inserts rolls every target row back.
        write_plan(
            &workspace,
            &plan_value(
                &plan_id,
                "Plan",
                vec![
                    step_value("step-0", "One", &[], "pending", Value::Null),
                    step_value("step-1", "Two", &["step-0"], "pending", Value::Null),
                ],
                "active",
            ),
        );
        {
            let db = Connection::open(target_path(&data, &session)).unwrap();
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS import_ids(source_workspace TEXT NOT NULL, plan_key TEXT NOT NULL, \
                   kind TEXT NOT NULL, legacy_id TEXT NOT NULL, task_id TEXT NOT NULL, imported_at TEXT NOT NULL, \
                   source_hash TEXT NOT NULL, PRIMARY KEY(source_workspace, plan_key, kind, legacy_id)); \
                 CREATE TRIGGER fail_import BEFORE INSERT ON import_ids WHEN NEW.legacy_id='step-1' \
                   BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        }
        let error = import_plan(&data, &workspace, &session, &plan_id).unwrap_err();
        assert!(error.to_string().contains("injected failure"), "{error}");
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(mapping_count(&data, &session, &workspace, &plan_id), 0);
        {
            let db = Connection::open(target_path(&data, &session)).unwrap();
            db.execute_batch("DROP TRIGGER fail_import").unwrap();
        }
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.status, "created");
        assert_eq!(list_tasks(&data, &session).len(), before.len() + 3);
    }

    #[test]
    fn unchanged_reimport_is_idempotent_and_keeps_manual_edits() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let plan = plan_value(
            &plan_id,
            "Plan",
            vec![
                step_value("step-0", "One", &[], "pending", Value::Null),
                step_value("step-1", "Two", &["step-0"], "pending", Value::Null),
            ],
            "draft",
        );
        let workspace = workspace_with_plans(temp.path(), &[plan], None);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let first = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(first.status, "created");
        let step = first.steps[0].task_id.clone().unwrap();
        crate::tools::tasks::execute_for_session(
            "TaskUpdate",
            json!({"id":step,"subject":"edited by user","metadata":{"note":"kept"}}),
            &data,
            &session,
        )
        .unwrap();
        let before = list_tasks(&data, &session);
        let source = source_bytes(&workspace);

        let again = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(again.status, "unchanged");
        assert_eq!(again.created_tasks, 0);
        assert_eq!(again.unchanged_tasks, 3);
        assert_eq!(again.root_task_id, first.root_task_id);
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(source_bytes(&workspace), source);
        let edited = get_task(&data, &session, &step);
        assert_eq!(edited["subject"], "edited by user");
        assert_eq!(edited["metadata"]["note"], "kept");

        // A dry run of the same repeat also writes nothing.
        let dry = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Plan(plan_id.clone()),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(dry.status, "unchanged");
        assert_eq!(dry.unchanged_tasks, 3);
        assert_eq!(list_tasks(&data, &session), before);
    }

    /// Legacy `exit` deletes only the `current_plan` row, leaving the record
    /// body untouched. That still changes what the importer persists
    /// (`metadata.plan.current`), so it must not read as `unchanged`.
    #[test]
    fn current_plan_only_change_conflicts_without_writes_in_both_directions() {
        let temp = tempfile::tempdir().unwrap();

        // Imported while current, then the workspace clears current_plan.
        let plan_id = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(
                &plan_id,
                "Current plan",
                vec![step_value("step-0", "One", &[], "pending", Value::Null)],
                "active",
            )],
            Some(&plan_id),
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let first = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(first.status, "created");
        assert!(first.current);
        let root = first.root_task_id.clone().unwrap();
        assert!(stored_root_current(&data, &session, &root));
        let before = list_tasks(&data, &session);
        let mappings = mapping_snapshot(&data, &session);

        set_current(&workspace, None);
        let body_before = record_body(&workspace, &plan_id);
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.status, "conflict", "{report:?}");
        assert!(!report.current);
        assert_eq!(report.created_tasks, 0);
        assert!(
            report
                .conflicts
                .iter()
                .any(|conflict| conflict.to_lowercase().contains("current")),
            "{:?}",
            report.conflicts
        );
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(mapping_snapshot(&data, &session), mappings);
        assert!(stored_root_current(&data, &session, &root));
        assert_eq!(record_body(&workspace, &plan_id), body_before);

        // A dry run reports the same conflict and writes nothing.
        let dry = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Plan(plan_id.clone()),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(dry.status, "conflict");
        assert!(!dry.current);
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(mapping_snapshot(&data, &session), mappings);

        // Reopening the store still shows the originally imported state.
        drop(Connection::open(target_path(&data, &session)).unwrap());
        assert!(stored_root_current(&data, &session, &root));
        assert_eq!(list_tasks(&data, &session), before);

        // The reverse direction: imported while not current, then the
        // workspace makes it current without touching the record body.
        let plan_b = uuid::Uuid::new_v4().to_string();
        let second_root = temp.path().join("second");
        let workspace_b = workspace_with_plans(
            &second_root,
            &[plan_value(
                &plan_b,
                "Later plan",
                vec![step_value("step-0", "One", &[], "pending", Value::Null)],
                "draft",
            )],
            None,
        );
        let data_b = temp.path().join("data-b");
        let session_b = uuid::Uuid::new_v4().to_string();
        let first_b = import_plan(&data_b, &workspace_b, &session_b, &plan_b).unwrap();
        assert_eq!(first_b.status, "created");
        assert!(!first_b.current);
        let root_b = first_b.root_task_id.clone().unwrap();
        assert!(!stored_root_current(&data_b, &session_b, &root_b));
        let before_b = list_tasks(&data_b, &session_b);
        let mappings_b = mapping_snapshot(&data_b, &session_b);

        set_current(&workspace_b, Some(&plan_b));
        let body_b = record_body(&workspace_b, &plan_b);
        let report_b = import_plan(&data_b, &workspace_b, &session_b, &plan_b).unwrap();
        assert_eq!(report_b.status, "conflict", "{report_b:?}");
        assert!(report_b.current);
        assert!(
            report_b
                .conflicts
                .iter()
                .any(|conflict| conflict.to_lowercase().contains("current")),
            "{:?}",
            report_b.conflicts
        );
        assert_eq!(list_tasks(&data_b, &session_b), before_b);
        assert_eq!(mapping_snapshot(&data_b, &session_b), mappings_b);
        assert!(!stored_root_current(&data_b, &session_b, &root_b));
        assert_eq!(record_body(&workspace_b, &plan_b), body_b);

        let dry_b = import(ImportRequest {
            data_dir: &data_b,
            workspace: &workspace_b,
            session: &session_b,
            selector: ImportSelector::Plan(plan_b.clone()),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(dry_b.status, "conflict");
        assert!(dry_b.current);
        assert_eq!(list_tasks(&data_b, &session_b), before_b);
        assert_eq!(mapping_snapshot(&data_b, &session_b), mappings_b);
    }

    /// A current-plan change that does not affect the selected plan's
    /// membership is not a source change for that plan.
    #[test]
    fn unrelated_current_plan_change_keeps_the_selected_plan_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let plan_a = uuid::Uuid::new_v4().to_string();
        let plan_b = uuid::Uuid::new_v4().to_string();
        let plan_c = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[
                plan_value(&plan_a, "Plan A", vec![], "active"),
                plan_value(&plan_b, "Plan B", vec![], "active"),
                plan_value(&plan_c, "Plan C", vec![], "active"),
            ],
            Some(&plan_a),
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let first = import_plan(&data, &workspace, &session, &plan_b).unwrap();
        assert_eq!(first.status, "created");
        assert!(!first.current);
        let before = list_tasks(&data, &session);
        let mappings = mapping_snapshot(&data, &session);

        set_current(&workspace, Some(&plan_c));
        let report = import_plan(&data, &workspace, &session, &plan_b).unwrap();
        assert_eq!(report.status, "unchanged", "{report:?}");
        assert!(!report.current);
        assert!(report.conflicts.is_empty());
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(mapping_snapshot(&data, &session), mappings);
    }

    /// A mapping written by the earlier hash format must be reported, never
    /// treated as a match and never rebuilt in place.
    #[test]
    fn older_snapshot_hash_is_an_explicit_conflict_and_keeps_rows() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(
                &plan_id,
                "Plan",
                vec![step_value("step-0", "One", &[], "pending", Value::Null)],
                "active",
            )],
            None,
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        import_plan(&data, &workspace, &session, &plan_id).unwrap();
        {
            // Simulate the round-04 importer's body-only hash.
            let db = Connection::open(target_path(&data, &session)).unwrap();
            db.execute("UPDATE import_ids SET source_hash=?1", ["a".repeat(64)])
                .unwrap();
        }
        let before = list_tasks(&data, &session);
        let mappings = mapping_snapshot(&data, &session);

        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.status, "conflict", "{report:?}");
        assert_eq!(report.created_tasks, 0);
        assert!(
            report.conflicts.iter().any(|conflict| {
                let conflict = conflict.to_lowercase();
                conflict.contains("older") || conflict.contains("snapshot")
            }),
            "{:?}",
            report.conflicts
        );
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(mapping_snapshot(&data, &session), mappings);

        let dry = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Plan(plan_id.clone()),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(dry.status, "conflict");
        assert_eq!(mapping_snapshot(&data, &session), mappings);
    }

    #[test]
    fn changed_source_conflicts_without_writes() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let original = plan_value(
            &plan_id,
            "Plan",
            vec![step_value("step-0", "One", &[], "pending", Value::Null)],
            "active",
        );
        let workspace = workspace_with_plans(temp.path(), &[original.clone()], None);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        import_plan(&data, &workspace, &session, &plan_id).unwrap();
        let before = list_tasks(&data, &session);
        let mappings = mapping_count(&data, &session, &workspace, &plan_id);

        let mut changed = original;
        changed["steps"].as_array_mut().unwrap().push(step_value(
            "step-1",
            "Two",
            &["step-0"],
            "pending",
            Value::Null,
        ));
        write_plan(&workspace, &changed);
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.status, "conflict");
        assert_eq!(report.created_tasks, 0);
        assert!(!report.conflicts.is_empty());
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(
            mapping_count(&data, &session, &workspace, &plan_id),
            mappings
        );

        // A dry run reports the same conflict and writes nothing.
        let dry = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Plan(plan_id.clone()),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(dry.status, "conflict");
        assert!(dry.dry_run);
        assert_eq!(list_tasks(&data, &session), before);
    }

    #[test]
    fn missing_mapped_target_reports_conflict_without_recreating_it() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let plan = plan_value(
            &plan_id,
            "Plan",
            vec![
                step_value("step-0", "One", &[], "pending", Value::Null),
                step_value("step-1", "Two", &["step-0"], "pending", Value::Null),
            ],
            "active",
        );
        let workspace = workspace_with_plans(temp.path(), &[plan], None);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let first = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        let step = first.steps[1].task_id.clone().unwrap();
        crate::tools::tasks::execute_for_session(
            "TaskUpdate",
            json!({"id":step,"status":"deleted"}),
            &data,
            &session,
        )
        .unwrap();
        let before = list_tasks(&data, &session);
        assert_eq!(before.len(), 2);

        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.status, "conflict");
        assert!(
            report
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("missing")),
            "{:?}",
            report.conflicts
        );
        assert_eq!(list_tasks(&data, &session), before);
        assert_eq!(
            mapping_count(&data, &session, &workspace, &plan_id),
            3,
            "the mapping must not be silently rebuilt"
        );
    }

    #[test]
    fn identical_legacy_ids_in_different_workspaces_and_sessions_stay_separate() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let alpha = plan_value(
            &plan_id,
            "Alpha",
            vec![step_value(
                "step-0",
                "Alpha step",
                &[],
                "pending",
                Value::Null,
            )],
            "active",
        );
        let beta = plan_value(
            &plan_id,
            "Beta",
            vec![step_value(
                "step-0",
                "Beta step",
                &[],
                "pending",
                Value::Null,
            )],
            "active",
        );
        let first_workspace = workspace_with_plans(temp.path(), &[alpha], None);
        let second_root = temp.path().join("second");
        let second_workspace = workspace_with_plans(&second_root, &[beta], None);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let first = import_plan(&data, &first_workspace, &session, &plan_id).unwrap();
        let second = import_plan(&data, &second_workspace, &session, &plan_id).unwrap();
        assert_eq!(first.status, "created");
        assert_eq!(second.status, "created");
        assert_ne!(first.root_task_id, second.root_task_id);
        let tasks = list_tasks(&data, &session);
        assert_eq!(tasks.len(), 4);
        let roots: HashSet<String> = tasks
            .iter()
            .filter(|task| task["metadata"]["plan"]["kind"] == "plan")
            .map(|task| task["description"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            roots,
            HashSet::from(["Alpha".to_owned(), "Beta".to_owned()])
        );
        assert_eq!(
            mapping_count(&data, &session, &first_workspace, &plan_id),
            2
        );
        assert_eq!(
            mapping_count(&data, &session, &second_workspace, &plan_id),
            2
        );

        // A second session gets its own task rows and mapping.
        let other_session = uuid::Uuid::new_v4().to_string();
        import_plan(&data, &first_workspace, &other_session, &plan_id).unwrap();
        assert_eq!(list_tasks(&data, &session).len(), 4);
        assert_eq!(list_tasks(&data, &other_session).len(), 2);
    }

    #[test]
    fn concurrent_duplicate_imports_create_one_mapping_set() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let plan = plan_value(
            &plan_id,
            "Plan",
            vec![
                step_value("step-0", "One", &[], "pending", Value::Null),
                step_value("step-1", "Two", &["step-0"], "pending", Value::Null),
            ],
            "active",
        );
        let workspace = workspace_with_plans(temp.path(), &[plan], None);
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let barrier = std::sync::Barrier::new(2);
        let reports = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                import_plan(&data, &workspace, &session, &plan_id).unwrap()
            });
            let second = scope.spawn(|| {
                barrier.wait();
                import_plan(&data, &workspace, &session, &plan_id).unwrap()
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        let created = [&reports.0, &reports.1]
            .iter()
            .filter(|report| report.status == "created")
            .count();
        assert_eq!(created, 1, "{:?}", (reports.0.status, reports.1.status));
        assert_eq!(list_tasks(&data, &session).len(), 3);
        assert_eq!(mapping_count(&data, &session, &workspace, &plan_id), 3);
    }

    #[test]
    fn current_flag_updates_stay_scoped_to_workspace_and_session() {
        let temp = tempfile::tempdir().unwrap();
        let plan_a = uuid::Uuid::new_v4().to_string();
        let plan_b = uuid::Uuid::new_v4().to_string();
        let first_workspace = workspace_with_plans(
            temp.path(),
            &[
                plan_value(&plan_a, "Plan A", vec![], "active"),
                plan_value(&plan_b, "Plan B", vec![], "draft"),
            ],
            Some(&plan_a),
        );
        let plan_c = uuid::Uuid::new_v4().to_string();
        let second_root = temp.path().join("second");
        let second_workspace = workspace_with_plans(
            &second_root,
            &[plan_value(&plan_c, "Plan C", vec![], "active")],
            Some(&plan_c),
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let a = import_plan(&data, &first_workspace, &session, &plan_a).unwrap();
        import_plan(&data, &second_workspace, &session, &plan_c).unwrap();

        set_current(&first_workspace, Some(&plan_b));
        let b = import_plan(&data, &first_workspace, &session, &plan_b).unwrap();
        assert!(b.current);
        let root_a = get_task(&data, &session, &a.root_task_id.unwrap());
        assert_eq!(root_a["metadata"]["plan"]["current"], false);
        let root_b = get_task(&data, &session, &b.root_task_id.unwrap());
        assert_eq!(root_b["metadata"]["plan"]["current"], true);
        // The other workspace's imported current root is untouched.
        let current_c: Vec<Value> = list_tasks(&data, &session)
            .into_iter()
            .filter(|task| {
                task["metadata"]["plan"]["current"] == true
                    && task["metadata"]["plan"]["source_workspace"]
                        == second_workspace
                            .canonicalize()
                            .unwrap()
                            .to_string_lossy()
                            .as_ref()
            })
            .collect();
        assert_eq!(current_c.len(), 1);
        assert_eq!(current_c[0]["description"], "Plan C");
    }

    #[test]
    fn current_selector_imports_the_current_plan() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(&plan_id, "Current plan", vec![], "active")],
            Some(&plan_id),
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let report = import(ImportRequest {
            data_dir: &data,
            workspace: &workspace,
            session: &session,
            selector: ImportSelector::Current,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(report.status, "created");
        assert!(report.current);
        assert_eq!(
            get_task(&data, &session, report.root_task_id.as_ref().unwrap())["status"],
            "in_progress"
        );
    }

    #[test]
    fn abandoned_plan_is_preserved_as_cancelled() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(&plan_id, "Old plan", vec![], "abandoned")],
            None,
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        let root = get_task(&data, &session, report.root_task_id.as_ref().unwrap());
        assert_eq!(root["status"], "cancelled");
        assert_eq!(root["metadata"]["plan"]["legacy_status"], "abandoned");
        assert_eq!(root["metadata"]["plan"]["current"], false);
    }

    #[test]
    fn duplicate_dependencies_collapse_to_one_edge() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(
                &plan_id,
                "Plan",
                vec![
                    step_value("step-0", "One", &[], "pending", Value::Null),
                    step_value(
                        "step-1",
                        "Two",
                        &["step-0", "step-0"],
                        "pending",
                        Value::Null,
                    ),
                ],
                "draft",
            )],
            None,
        );
        let data = temp.path().join("data");
        let session = uuid::Uuid::new_v4().to_string();
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert_eq!(report.created_edges, 1);
        let step_0 = get_task(&data, &session, report.steps[0].task_id.as_ref().unwrap());
        assert_eq!(step_0["blocks"].as_array().unwrap().len(), 1);
        let step_1 = get_task(&data, &session, report.steps[1].task_id.as_ref().unwrap());
        assert_eq!(step_1["blockedBy"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_large_valid_plan_imports_every_step() {
        let temp = tempfile::tempdir().unwrap();
        let plan_id = uuid::Uuid::new_v4().to_string();
        let steps: Vec<Value> = (0..500)
            .map(|index| {
                step_value(
                    &format!("step-{index}"),
                    "step",
                    &[],
                    "pending",
                    Value::Null,
                )
            })
            .collect();
        let workspace = workspace_with_plans(
            temp.path(),
            &[plan_value(&plan_id, "Plan", steps, "active")],
            None,
        );
        let session = uuid::Uuid::new_v4().to_string();
        let data = temp.path().join("data");
        let started = std::time::Instant::now();
        let report = import_plan(&data, &workspace, &session, &plan_id).unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(report.created_tasks, 501);
    }
}
