//! Router and CRP member tools.
//!
//! These are the only model-facing tools of the Super Agent. Every call is
//! authorized from the durable records at execution time: a forged router tool
//! name, a stale member session or a task owned by another Router is rejected
//! even if it reaches the dispatcher.
use crate::db::DbManager;
use crate::services::{EnvironmentCredentials, ServiceContext};
use crate::super_agent::role::{self, MemberRole, Role};
use crate::super_agent::store::{EventAppend, StoreError, TaskStore};
use crate::super_agent::{
    ApprovalStatus, ApprovalType, EventType, NewEvent, NonGitStatus, Phase, RepoTarget, ReportKind,
    TaskRecord, TaskSpec,
};
use crate::tools::ToolDefinition;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub const ROUTER_TOOL_NAMES: [&str; 6] = [
    "CrpCreate",
    "CrpList",
    "CrpShow",
    "CrpSend",
    "CrpUpdateSpec",
    "CrpAnswer",
];
pub const MEMBER_TOOL_NAMES: [&str; 2] = ["CrpSubmit", "CrpProposeTask"];

/// Tools the Router must never use to change business code.
pub const ROUTER_FORBIDDEN_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

#[derive(Clone)]
pub struct ToolRuntime {
    pub db: Arc<DbManager>,
    pub data_dir: PathBuf,
    pub session_id: String,
}

impl ToolRuntime {
    pub fn context(&self) -> Result<ServiceContext> {
        ServiceContext::open(
            crate::runtime::RuntimePaths::new(self.data_dir.clone())?,
            Arc::new(EnvironmentCredentials),
        )
    }

    pub fn store(&self) -> TaskStore {
        TaskStore::new(self.db.clone(), self.data_dir.clone())
    }
}

pub fn handles(name: &str) -> bool {
    ROUTER_TOOL_NAMES.contains(&name)
        || MEMBER_TOOL_NAMES.contains(&name)
        || crate::super_agent::review::handles(name)
}

pub fn is_forbidden_router_tool(name: &str) -> bool {
    ROUTER_FORBIDDEN_EDIT_TOOLS.contains(&name)
}

fn text(description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false,"description":description})
}

fn report_payload_schema(kind: ReportKind) -> Value {
    let common = json!({
        "spec_version":{"type":"integer","minimum":1},
        "round":{"type":"integer","minimum":0}
    });
    let mut properties = common.as_object().cloned().unwrap_or_default();
    match kind {
        ReportKind::Plan => {
            for (name, schema) in [
                ("understanding", json!({"type":"string"})),
                ("scope", json!({"type":"array","items":{"type":"string"}})),
                ("steps", json!({"type":"array","items":{"type":"string"}})),
                (
                    "verification",
                    json!({"type":"array","items":{"type":"string"}}),
                ),
                ("risks", json!({"type":"array","items":{"type":"string"}})),
            ] {
                properties.insert(name.into(), schema);
            }
            json!({
                "type":"object",
                "properties":properties,
                "required":["spec_version","understanding"],
                "additionalProperties":false
            })
        }
        ReportKind::Implementation => {
            properties.insert("summary".into(), json!({"type":"string"}));
            properties.insert(
                "tests".into(),
                json!({"type":"array","items":{"type":"object","properties":{
                    "command":{"type":"string"},
                    "passed":{"type":"boolean"},
                    "evidence_ref":{"type":"string"}
                },"required":["command","passed"],"additionalProperties":false}}),
            );
            properties.insert(
                "commits".into(),
                json!({"type":"array","items":{"type":"string"}}),
            );
            json!({
                "type":"object",
                "properties":properties,
                "required":["spec_version","summary"],
                "additionalProperties":false
            })
        }
        ReportKind::Review => {
            properties.insert(
                "verdict".into(),
                json!({"type":"string","enum":["APPROVED","CHANGES_REQUESTED","BLOCKED"]}),
            );
            properties.insert(
                "stage".into(),
                json!({"type":"string","enum":["IMPLEMENTATION","FINAL_DELIVERY"]}),
            );
            properties.insert(
                "reviewed_versions".into(),
                json!({"type":"array","items":{"type":"object","properties":{
                    "repository_id":{"type":"string"},
                    "commit":{"type":"string"},
                    "snapshot_ref":{"type":"string"}
                },"required":["repository_id"],"additionalProperties":false}}),
            );
            properties.insert(
                "acceptance_results".into(),
                json!({"type":"array","items":{"type":"object","properties":{
                    "criterion":{"type":"string"},
                    "passed":{"type":"boolean"},
                    "evidence":{"type":"array","items":{"type":"string"}}
                },"required":["criterion","passed"],"additionalProperties":false}}),
            );
            properties.insert(
                "issues".into(),
                json!({"type":"array","items":{"type":"object","properties":{
                    "severity":{"type":"string","enum":["LOW","MEDIUM","HIGH","CRITICAL"]},
                    "location":{"type":"string"},
                    "description":{"type":"string"},
                    "expected":{"type":"string"}
                },"required":["severity","location","description"],"additionalProperties":false}}),
            );
            properties.insert(
                "verification".into(),
                json!({"type":"array","items":{"type":"string"}}),
            );
            properties.insert(
                "limitations".into(),
                json!({"type":"array","items":{"type":"string"}}),
            );
            json!({
                "type":"object",
                "properties":properties,
                "required":["verdict","spec_version","stage","reviewed_versions","acceptance_results"],
                "additionalProperties":false
            })
        }
        ReportKind::Harness => {
            properties.insert(
                "findings".into(),
                json!({"type":"array","items":{"type":"object","properties":{
                    "category":{"type":"string","enum":["MODEL","HARNESS","TOOL","ENVIRONMENT","REQUIREMENT_AMBIGUITY"]},
                    "severity":{"type":"string","enum":["LOW","MEDIUM","HIGH","CRITICAL"]},
                    "phenomenon":{"type":"string"},
                    "evidence":{"type":"array","items":{"type":"string"}},
                    "impact":{"type":"string"},
                    "probable_cause":{"type":"string"},
                    "confidence":{"type":"number","minimum":0,"maximum":1},
                    "suggestion":{"type":"string"},
                    "validation":{"type":"string"},
                    "create_task":{"type":"boolean"}
                },"required":["category","severity","phenomenon"],"additionalProperties":false}}),
            );
            properties.insert(
                "notes".into(),
                json!({"type":"array","items":{"type":"string"}}),
            );
            json!({
                "type":"object",
                "properties":properties,
                "required":["spec_version"],
                "additionalProperties":false
            })
        }
        ReportKind::Integration => {
            properties.insert(
                "targets".into(),
                json!({"type":"array","items":{"type":"object","properties":{
                    "repository_id":{"type":"string"},
                    "target_branch":{"type":"string"},
                    "before_commit":{"type":"string"},
                    "after_commit":{"type":"string"},
                    "conflict":{"type":"boolean"},
                    "tests_passed":{"type":"boolean"},
                    "issues":{"type":"array","items":{"type":"string"}}
                },"required":["repository_id","target_branch"],"additionalProperties":false}}),
            );
            json!({
                "type":"object",
                "properties":properties,
                "required":["spec_version","targets"],
                "additionalProperties":false
            })
        }
    }
}

fn submit_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "kind":{"type":"string","enum":["plan","implementation","review","harness","integration"]},
            "payload":{"oneOf":[
                report_payload_schema(ReportKind::Plan),
                report_payload_schema(ReportKind::Implementation),
                report_payload_schema(ReportKind::Review),
                report_payload_schema(ReportKind::Harness),
                report_payload_schema(ReportKind::Integration)
            ]}
        },
        "required":["kind","payload"],
        "additionalProperties":false
    })
}

pub fn definitions(role: Role, member: Option<MemberRole>) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();
    if role.is_router() {
        definitions.push(ToolDefinition {
            name: "CrpCreate".into(),
            description: "Create one Super Agent task from a complete specification. Returns task and pair IDs. The host prepares the isolated workspace, launches the Coder and requests plan approval; the task is not complete until the host verifies its evidence.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "spec":{
                        "type":"object",
                        "description":"Task specification: title, workspace, targets, goal, requirements, acceptance_criteria (id and description), constraints, out_of_scope, optional target_branch",
                        "properties":{
                            "title":{"type":"string"},
                            "workspace":{"type":"string"},
                            "targets":{"type":"array","items":{"type":"string"}},
                            "goal":{"type":"string"},
                            "requirements":{"type":"array","items":{"type":"string"}},
                            "acceptance_criteria":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"description":{"type":"string"}},"required":["id","description"]}},
                            "constraints":{"type":"array","items":{"type":"string"}},
                            "out_of_scope":{"type":"array","items":{"type":"string"}},
                            "target_branch":{"type":"string"},
                            "repository_target_branches":{"type":"object","additionalProperties":{"type":"string"}}
                        },
                        "required":["title","workspace","goal"]
                    }
                },
                "required":["spec"],
                "additionalProperties":false
            }),
        });
        definitions.push(ToolDefinition {
            name: "CrpList".into(),
            description: "List this Router's tasks with phase, status and review round. Optionally filter by status (QUEUED, RUNNING, WAITING_USER, BLOCKED, RECOVERING, PAUSED, COMPLETED, FAILED, CANCELLED).".into(),
            input_schema: text(
                "List tasks",
                json!({"status":{"type":"string"}}),
                &[],
            ),
        });
        definitions.push(ToolDefinition {
            name: "CrpShow".into(),
            description: "Show one task's summary, sessions, pending approvals and report/evidence references. It does not return full transcripts.".into(),
            input_schema: text(
                "Show a task",
                json!({"task_id":{"type":"string"}}),
                &["task_id"],
            ),
        });
        definitions.push(ToolDefinition {
            name: "CrpSend".into(),
            description: "Send a message to this task's current Coder or Reviewer through the session message queue. The member must already have a session.".into(),
            input_schema: text(
                "Send a task message",
                json!({
                    "task_id":{"type":"string"},
                    "to":{"type":"string","enum":["coder","reviewer"]},
                    "content":{"type":"string"}
                }),
                &["task_id","to","content"],
            ),
        });
        definitions.push(ToolDefinition {
            name: "CrpUpdateSpec".into(),
            description: "Replace the task specification. The host increments the spec version, invalidates earlier reports and returns the task to planning and approval.".into(),
            input_schema: text(
                "Update the task specification",
                json!({"task_id":{"type":"string"},"spec":{"type":"object"}}),
                &["task_id","spec"],
            ),
        });
        definitions.push(ToolDefinition {
            name: "CrpAnswer".into(),
            description: "Resolve an existing pending approval for this Router's task. It cannot create authorization: the approval must exist, be pending and match the current spec and permission policy version.".into(),
            input_schema: text(
                "Answer an approval",
                json!({
                    "approval_id":{"type":"string"},
                    "decision":{"type":"string","enum":["approved","rejected"]},
                    "note":{"type":"string"}
                }),
                &["approval_id","decision"],
            ),
        });
    }
    if member.is_some() {
        definitions.push(ToolDefinition {
            name: "CrpSubmit".into(),
            description: "Submit the task report for the current role. Coder: plan, implementation, integration. Reviewer: review, harness. The payload must match the typed schema for that kind; the host validates it and records the event without applying state.".into(),
            input_schema: submit_schema(),
        });
        definitions.push(ToolDefinition {
            name: "CrpProposeTask".into(),
            description: "Propose unrelated work discovered during this task. The Router decides whether it becomes a task.".into(),
            input_schema: text(
                "Propose a task",
                json!({"proposal":{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"},"reason":{"type":"string"},"severity":{"type":"string"}}}}),
                &["proposal"],
            ),
        });
    }
    if member == Some(MemberRole::Reviewer) {
        definitions.extend(crate::super_agent::review::definitions());
    }
    definitions
}

fn operation(tool_use_id: &str, prefix: &str) -> Result<String> {
    ensure!(!tool_use_id.trim().is_empty(), "Tool use ID is required");
    Ok(format!("{prefix}:{tool_use_id}"))
}

/// Executes a CRP tool. The caller has already checked the tool name; this
/// function re-checks authority from the database.
pub fn execute(
    runtime: &ToolRuntime,
    name: &str,
    input: Value,
    tool_use_id: &str,
) -> Result<Value> {
    ensure!(input.is_object(), "Tool input must be an object");
    if crate::super_agent::review::handles(name) {
        return crate::super_agent::review::execute(runtime, name, input);
    }
    let context = runtime.context()?;
    match name {
        "CrpCreate" => create(runtime, &context, input, tool_use_id),
        "CrpList" => list(runtime, input),
        "CrpShow" => show(runtime, input),
        "CrpSend" => send(runtime, &context, input, tool_use_id),
        "CrpUpdateSpec" => update_spec(runtime, input, tool_use_id),
        "CrpAnswer" => answer(runtime, input, tool_use_id),
        "CrpSubmit" => submit(runtime, input, tool_use_id),
        "CrpProposeTask" => propose(runtime, input, tool_use_id),
        other => anyhow::bail!("Unknown Super Agent tool: {other}"),
    }
}

fn require_router(runtime: &ToolRuntime) -> Result<TaskStore> {
    let role = role::load(&runtime.db, &runtime.session_id)?;
    ensure!(
        role.is_router(),
        "Super Agent tool {name} requires a Router session",
        name = "Crp"
    );
    Ok(runtime.store())
}

fn owned_task(store: &TaskStore, router: &str, task_id: &str) -> Result<TaskRecord> {
    let task = store
        .get_task(task_id)
        .map_err(store_error)?
        .with_context(|| format!("Task not found: {task_id}"))?;
    ensure!(
        task.router_session_id == router,
        "Task {task_id} belongs to another Router"
    );
    Ok(task)
}

fn store_error(error: StoreError) -> anyhow::Error {
    match error {
        StoreError::Invalid(message) => anyhow::anyhow!(message),
        other => anyhow::Error::new(other),
    }
}

fn create(
    runtime: &ToolRuntime,
    _context: &ServiceContext,
    input: Value,
    tool_use_id: &str,
) -> Result<Value> {
    let store = require_router(runtime)?;
    let spec: TaskSpec =
        serde_json::from_value(input["spec"].clone()).context("Invalid task specification")?;
    let task = store
        .create_task(
            &runtime.session_id,
            spec,
            &operation(tool_use_id, "crp-create")?,
        )
        .map_err(store_error)?;
    Ok(json!({
        "task_id": task.task_id,
        "pair_id": task.pair_id,
        "phase": task.phase.as_str(),
        "status": task.status.as_str(),
        "message": "Task created. The host prepares the workspace and launches the Coder; approval is handled separately."
    }))
}

fn list(runtime: &ToolRuntime, input: Value) -> Result<Value> {
    let store = require_router(runtime)?;
    let status = match input["status"].as_str() {
        Some(value) => Some(crate::super_agent::Status::parse(
            &value.to_ascii_uppercase(),
        )?),
        None => None,
    };
    let tasks = store
        .list_tasks(&runtime.session_id, status)
        .map_err(store_error)?;
    let tasks: Vec<Value> = tasks
        .into_iter()
        .map(|task| {
            json!({
                "task_id": task.task_id,
                "pair_id": task.pair_id,
                "title": task.title,
                "phase": task.phase.as_str(),
                "status": task.status.as_str(),
                "review_round": task.review_round,
                "updated_at": task.updated_at
            })
        })
        .collect();
    Ok(json!({"tasks": tasks, "count": tasks.len()}))
}

fn show(runtime: &ToolRuntime, input: Value) -> Result<Value> {
    let store = require_router(runtime)?;
    let task_id = input["task_id"].as_str().context("task_id is required")?;
    let task = owned_task(&store, &runtime.session_id, task_id)?;
    let approvals = store.approvals(task_id).map_err(store_error)?;
    let pending: Vec<Value> = approvals
        .iter()
        .filter(|approval| approval.status == ApprovalStatus::Pending)
        .map(|approval| {
            json!({
                "approval_id": approval.approval_id,
                "request_type": approval.request_type.as_str(),
                "task_spec_version": approval.task_spec_version,
                "created_at": approval.created_at
            })
        })
        .collect();
    let reports = store.reports(task_id).map_err(store_error)?;
    let reports: Vec<Value> = reports
        .iter()
        .map(|report| {
            json!({
                "report_id": report.report_id,
                "kind": report.kind.as_str(),
                "spec_version": report.spec_version,
                "round": report.round,
                "invalidated": report.invalidated,
                "created_at": report.created_at
            })
        })
        .collect();
    let repos: Vec<Value> = task
        .repositories
        .iter()
        .map(|repo| {
            json!({
                "repository_id": repo.repository_id,
                "target_branch": repo.target_branch,
                "task_branch": repo.task_branch,
                "submitted_commit": repo.submitted_commit,
                "reviewed_commit": repo.reviewed_commit,
                "integrated_commit": repo.integrated_commit
            })
        })
        .collect();
    Ok(json!({
        "task_id": task.task_id,
        "pair_id": task.pair_id,
        "title": task.title,
        "phase": task.phase.as_str(),
        "status": task.status.as_str(),
        "spec_version": task.spec_version,
        "review_round": task.review_round,
        "goal": task.spec.goal,
        "acceptance_criteria": task.spec.acceptance_criteria,
        "coder_session_id": task.coder.session_id,
        "reviewer_session_id": task.reviewer.session_id,
        "cleanup_status": task.cleanup_status.as_str(),
        "pending_approvals": pending,
        "reports": reports,
        "repositories": repos,
        "evidence_dir": runtime.data_dir.join("super-agent").join(&task.task_id).display().to_string()
    }))
}

fn send(
    runtime: &ToolRuntime,
    context: &ServiceContext,
    input: Value,
    tool_use_id: &str,
) -> Result<Value> {
    let store = require_router(runtime)?;
    let task_id = input["task_id"].as_str().context("task_id is required")?;
    let task = owned_task(&store, &runtime.session_id, task_id)?;
    let to = input["to"].as_str().context("to is required")?;
    let recipient = match to {
        "coder" => task
            .coder
            .session_id
            .clone()
            .context("Task has no Coder session yet"),
        "reviewer" => task
            .reviewer
            .session_id
            .clone()
            .context("Task has no Reviewer session yet"),
        other => anyhow::bail!("Unknown member {other}; use coder or reviewer"),
    }?;
    let content = input["content"].as_str().context("content is required")?;
    let operation_id = operation(tool_use_id, "crp-send")?;
    let message = crate::services::session_messages::send_once(
        context,
        &operation_id,
        &runtime.session_id,
        &recipient,
        content,
    )?;
    let append: EventAppend = store
        .ingest_event(
            task_id,
            NewEvent {
                session_id: Some(runtime.session_id.clone()),
                run_id: None,
                event_type: EventType::MessageSent,
                causation_id: None,
                operation_id,
                payload: json!({"to":to,"message_id":message.id,"bytes":content.len()}),
            },
        )
        .map_err(store_error)?;
    Ok(json!({
        "message_id": message.id,
        "recipient": recipient,
        "queued": true,
        "event_id": append.event.event_id
    }))
}

fn parse_spec(value: &Value) -> Result<TaskSpec> {
    let spec: TaskSpec =
        serde_json::from_value(value.clone()).context("Invalid task specification")?;
    spec.validate()?;
    Ok(spec)
}

fn update_spec(runtime: &ToolRuntime, input: Value, tool_use_id: &str) -> Result<Value> {
    let store = require_router(runtime)?;
    let task_id = input["task_id"].as_str().context("task_id is required")?;
    let task = owned_task(&store, &runtime.session_id, task_id)?;
    let spec = parse_spec(&input["spec"])?;
    let append = store
        .ingest_event(
            task_id,
            NewEvent {
                session_id: Some(runtime.session_id.clone()),
                run_id: None,
                event_type: EventType::SpecUpdated,
                causation_id: None,
                operation_id: operation(tool_use_id, "crp-update-spec")?,
                payload: json!({"spec":spec}),
            },
        )
        .map_err(store_error)?;
    Ok(json!({
        "task_id": task.task_id,
        "current_spec_version": task.spec_version,
        "event_id": append.event.event_id,
        "message": "Specification update recorded. The host applies it, increments the version and returns the task to planning."
    }))
}

fn answer(runtime: &ToolRuntime, input: Value, tool_use_id: &str) -> Result<Value> {
    let store = require_router(runtime)?;
    let approval_id = input["approval_id"]
        .as_str()
        .context("approval_id is required")?;
    let approval = store
        .approval(approval_id)
        .map_err(store_error)?
        .with_context(|| format!("Approval not found: {approval_id}"))?;
    let task = owned_task(&store, &runtime.session_id, &approval.task_id)?;
    if approval.status == ApprovalStatus::Pending {
        // A current approval may only be answered while it still describes the
        // active contract. A replay of an earlier answer is handled by
        // `resolve_approval` below and keeps its original result.
        ensure!(
            approval.task_spec_version == task.spec_version,
            "Approval {approval_id} belongs to spec version {}, current version is {}",
            approval.task_spec_version,
            task.spec_version
        );
        ensure!(
            approval.permission_policy_version == task.permission_policy_version,
            "Approval {approval_id} belongs to permission policy version {}, current policy is {}",
            approval.permission_policy_version,
            task.permission_policy_version
        );
    }
    let decision = match input["decision"]
        .as_str()
        .context("decision is required")?
        .to_ascii_lowercase()
        .as_str()
    {
        "approved" | "approve" => ApprovalStatus::Approved,
        "rejected" | "reject" => ApprovalStatus::Rejected,
        other => anyhow::bail!("Unknown decision {other}; use approved or rejected"),
    };
    let resolved = store
        .resolve_approval(
            &operation(tool_use_id, "crp-answer")?,
            approval_id,
            decision,
            input["note"].as_str(),
        )
        .map_err(store_error)?;
    let append = store
        .ingest_event(
            &approval.task_id,
            NewEvent {
                session_id: Some(runtime.session_id.clone()),
                run_id: None,
                event_type: EventType::ApprovalResolved,
                causation_id: Some(approval.approval_id.clone()),
                operation_id: operation(tool_use_id, "crp-answer-event")?,
                payload: json!({
                    "approval_id": resolved.approval_id,
                    "request_type": resolved.request_type.as_str(),
                    "decision": resolved.status.as_str()
                }),
            },
        )
        .map_err(store_error)?;
    Ok(json!({
        "approval_id": resolved.approval_id,
        "status": resolved.status.as_str(),
        "event_id": append.event.event_id
    }))
}

fn submit(runtime: &ToolRuntime, input: Value, tool_use_id: &str) -> Result<Value> {
    let membership = role::membership(&runtime.db, &runtime.session_id)?
        .context("This session is not a current Coder or Reviewer of any task")?;
    let (task_id, member) = membership;
    let store = runtime.store();
    let task = store
        .get_task(&task_id)
        .map_err(store_error)?
        .with_context(|| format!("Task not found: {task_id}"))?;
    let kind = input["kind"]
        .as_str()
        .context("kind is required")?
        .to_ascii_lowercase();
    let report_kind = match (member, kind.as_str()) {
        (MemberRole::Coder, "plan") => ReportKind::Plan,
        (MemberRole::Coder, "implementation") => ReportKind::Implementation,
        (MemberRole::Coder, "integration") => ReportKind::Integration,
        (MemberRole::Reviewer, "review") => ReportKind::Review,
        (MemberRole::Reviewer, "harness") => ReportKind::Harness,
        (member, other) => anyhow::bail!("The {} cannot submit a {other} report", member.as_str()),
    };
    let payload = input["payload"].clone();
    ensure!(payload.is_object(), "payload must be an object");
    if report_kind == ReportKind::Review {
        let expected = match task.phase {
            Phase::Reviewing => crate::super_agent::ReviewStage::Implementation,
            Phase::FinalReview => crate::super_agent::ReviewStage::FinalDelivery,
            other => anyhow::bail!(
                "A review report is not accepted while the task is {}",
                other.as_str()
            ),
        };
        let body: crate::super_agent::TaskReviewReport =
            serde_json::from_value(payload.clone()).context("Invalid review report body")?;
        ensure!(
            body.stage == expected,
            "Review stage {} does not match the current review stage {}",
            body.stage.as_str(),
            expected.as_str()
        );
    }
    let report = store
        .add_report(
            &operation(tool_use_id, "crp-report")?,
            &task_id,
            report_kind,
            task.spec_version,
            task.review_round,
            payload.clone(),
        )
        .map_err(store_error)?;
    let event_type = match report_kind {
        ReportKind::Plan => EventType::PlanSubmitted,
        ReportKind::Implementation => EventType::ImplementationSubmitted,
        ReportKind::Integration => EventType::IntegrationCompleted,
        ReportKind::Review => {
            ensure!(
                payload["verdict"].is_string(),
                "Review reports must include a verdict"
            );
            // The host decides the outcome only after both the review and the
            // harness reports are present; this event is a durable submission.
            EventType::ReviewSubmitted
        }
        ReportKind::Harness => EventType::HarnessSubmitted,
    };
    let mut payload = json!({"report_id":report.report_id,"to":member.as_str()});
    if let Some(verdict) = payload_value(&report.body)["verdict"].as_str() {
        payload["verdict"] = json!(verdict);
    }
    let append = store
        .ingest_event(
            &task_id,
            NewEvent {
                session_id: Some(runtime.session_id.clone()),
                run_id: None,
                event_type,
                causation_id: Some(report.report_id.clone()),
                operation_id: operation(tool_use_id, "crp-submit")?,
                payload,
            },
        )
        .map_err(store_error)?;
    Ok(json!({
        "task_id": task_id,
        "report_id": report.report_id,
        "kind": report_kind.as_str(),
        "spec_version": report.spec_version,
        "round": report.round,
        "event_id": append.event.event_id,
        "message": "Report recorded. The host processes it; this does not change task state directly."
    }))
}

fn payload_value(body: &Value) -> Value {
    body.clone()
}

fn propose(runtime: &ToolRuntime, input: Value, tool_use_id: &str) -> Result<Value> {
    let membership = role::membership(&runtime.db, &runtime.session_id)?
        .context("This session is not a current Coder or Reviewer of any task")?;
    let (task_id, member) = membership;
    let proposal = input["proposal"].clone();
    ensure!(
        proposal["summary"]
            .as_str()
            .is_some_and(|summary| !summary.trim().is_empty()),
        "A proposal needs a nonempty summary"
    );
    let store = runtime.store();
    store
        .get_task(&task_id)
        .map_err(store_error)?
        .with_context(|| format!("Task not found: {task_id}"))?;
    let append = store
        .ingest_event(
            &task_id,
            NewEvent {
                session_id: Some(runtime.session_id.clone()),
                run_id: None,
                event_type: EventType::NewTaskProposed,
                causation_id: None,
                operation_id: operation(tool_use_id, "crp-propose")?,
                payload: json!({"from":member.as_str(),"proposal":proposal}),
            },
        )
        .map_err(store_error)?;
    Ok(json!({"task_id": task_id, "event_id": append.event.event_id, "recorded": true}))
}

/// Repository summary for a Coder prompt.
pub fn repository_lines(repositories: &[RepoTarget]) -> Vec<String> {
    repositories
        .iter()
        .map(|repo| {
            format!(
                "- {} (target branch {}, task branch {}, working directory {})",
                repo.repository_id,
                repo.target_branch,
                repo.task_branch,
                repo.working_path.display()
            )
        })
        .collect()
}

/// Human-readable summary of a task for prompts and tool output.
pub fn task_summary(task: &TaskRecord) -> Value {
    let non_git: Vec<String> = task
        .non_git_targets
        .iter()
        .filter(|target| target.status != NonGitStatus::Completed)
        .map(|target| target.path.display().to_string())
        .collect();
    json!({
        "task_id": task.task_id,
        "pair_id": task.pair_id,
        "title": task.title,
        "phase": task.phase.as_str(),
        "status": task.status.as_str(),
        "goal": task.spec.goal,
        "unfinished_non_git_targets": non_git
    })
}

/// Phases in which a Coder is expected to be working in the task workspace.
pub fn coder_active(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Planning | Phase::Implementing | Phase::Integrating
    )
}

#[cfg(test)]
mod tests;
