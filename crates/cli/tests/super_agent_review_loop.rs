//! A real child-process Reviewer rejection/fix/re-review loop.
//!
//! Production code only: the Orchestrator prepares a real Git worktree, the
//! Launcher starts real `dcl` Coder and Reviewer children against a local mock
//! model, and the members submit typed reports through the real CrpSubmit tool.
//! The test asserts the host resumes the original Coder with the Reviewer's
//! exact issues and then the same Reviewer session for the new submitted
//! version. It stops before delivery integration.
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{EnvironmentCredentials, ServiceContext},
    super_agent::{
        config::{ApprovalsConfig, DeliveryConfig, PlanApprovalPolicy, SuperAgentConfig},
        orchestrator::Orchestrator,
        role::{self, Role},
        store::TaskStore,
        ApprovalType, EventType, Phase, Status, TaskSpec,
    },
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

const ISSUE: &str = "MISSING_CORRECTION: file.txt must contain exactly value=2";
const FIRST_CONTENT: &str = "value=1";
const CORRECTED_CONTENT: &str = "value=2";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Moves the target branch forward in an isolated worktree so the user's
/// source checkout is never touched.
fn move_target(repo: &Path, file: &str, content: &str) -> String {
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    let path = PathBuf::from(common).join("target-update");
    let _ = std::fs::remove_dir_all(&path);
    git(
        repo,
        &["worktree", "add", "-q", &path.to_string_lossy(), "develop"],
    );
    std::fs::write(path.join(file), content).unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "target work"]);
    let head = git(&path, &["rev-parse", "HEAD"]);
    git(
        repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    );
    head
}

fn stream(content: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":content,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    )
}

fn tool_call(id: &str, name: &str, input: Value) -> Value {
    json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]})
}

fn submit(id: &str, kind: &str, payload: Value) -> String {
    stream(
        tool_call(id, "CrpSubmit", json!({"kind": kind, "payload": payload})),
        "tool_calls",
    )
}

fn system_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Removes host-injected `<system-reminder>` blocks. The git snapshot and the
/// recalled-memory excerpts are both reminder blocks; the current instruction
/// is what remains.
fn strip_reminders(content: &str) -> String {
    let mut out = String::new();
    let mut rest = content;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<system-reminder>".len()..];
        match after.find("</system-reminder>") {
            Some(end) => rest = &after[end + "</system-reminder>".len()..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The current host instruction: the latest user message with reminder blocks
/// removed. A reminder-only message is skipped.
fn current_instruction(body: &Value) -> String {
    let users: Vec<&str> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .collect();
    for content in users.iter().rev() {
        let instruction = strip_reminders(content);
        let instruction = instruction.trim();
        if !instruction.is_empty() {
            return instruction.to_owned();
        }
    }
    String::new()
}

fn last_tool_result(body: &Value) -> Option<String> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .map(str::to_owned)
}

/// Every embedded durable review request, in order of appearance.
fn review_requests(text: &str) -> Vec<Value> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```json") {
        let after = &rest[start + "```json".len()..];
        let Some(end) = after.find("```") else {
            break;
        };
        if let Ok(value) = serde_json::from_str::<Value>(after[..end].trim()) {
            if let Some(request) = value.get("review_request") {
                found.push(request.clone());
            }
        }
        rest = &after[end + 3..];
    }
    found
}

/// The request for the current turn, parsed from the current instruction only.
/// Recalled memory is outside the instruction, so an older request cannot win.
fn current_review_request(body: &Value) -> Option<Value> {
    review_requests(&current_instruction(body))
        .into_iter()
        .next()
}

/// The stage a review request declares, from its structured field only.
fn request_stage(request: &Value) -> &str {
    request["review_stage"]
        .as_str()
        .or_else(|| request["runtime"]["review_stage"].as_str())
        .unwrap_or("IMPLEMENTATION")
}

#[test]
fn current_review_request_ignores_older_requests_inside_recalled_memory() {
    let current = json!({"review_request": {
        "task_id": "task-001", "spec_version": 1, "review_round": 1,
        "runtime": {"review_stage": "IMPLEMENTATION"},
        "repository_versions": [{"repository_id": "repo", "submitted_commit": "current-commit"}]
    }});
    // Recalled memory carries a higher round and a final-delivery request; the
    // current instruction is still the round-1 implementation review.
    let stale_final = json!({"review_request": {
        "task_id": "task-001", "spec_version": 1, "review_round": 7,
        "review_stage": "FINAL_DELIVERY",
        "runtime": {"review_stage": "FINAL_DELIVERY"},
        "repository_versions": [{"repository_id": "repo", "integrated_commit": "stale-integrated"}]
    }});
    let body = json!({"messages": [
        {"role": "system", "content": "## Super Agent Task task-001 / Reviewer\nstatic rules mention FINAL_DELIVERY"},
        {"role": "user", "content": format!(
            "<system-reminder>\n# gitStatus\nsnapshot\n</system-reminder>\n\nTask task-001: Real review loop\n\nreview_request:\n```json\n{current}\n```\n\n<system-reminder>\nRetrieved for possible relevance — use only if it actually applies.\n\n[Memory 1]\n```json\n{stale_final}\n```\n</system-reminder>"
        )}
    ]});
    let instruction = current_instruction(&body);
    assert!(instruction.starts_with("Task task-001"), "{instruction}");
    assert!(!instruction.contains("FINAL_DELIVERY"), "{instruction}");
    let request = current_review_request(&body).expect("request");
    assert_eq!(request_stage(&request), "IMPLEMENTATION");
    assert_eq!(
        request["repository_versions"][0]["submitted_commit"],
        json!("current-commit")
    );

    // The inverse: the current turn is the final review, and memory quotes the
    // earlier implementation request.
    let current_final = json!({"review_request": {
        "task_id": "task-001", "spec_version": 1, "review_round": 1,
        "review_stage": "FINAL_DELIVERY",
        "runtime": {"review_stage": "FINAL_DELIVERY"},
        "repository_versions": [{"repository_id": "repo", "integrated_commit": "integrated-commit"}]
    }});
    let stale_impl = json!({"review_request": {
        "task_id": "task-001", "spec_version": 1, "review_round": 1,
        "runtime": {"review_stage": "IMPLEMENTATION"},
        "repository_versions": [{"repository_id": "repo", "submitted_commit": "old-commit"}]
    }});
    let body = json!({"messages": [
        {"role": "system", "content": "static rules"},
        {"role": "user", "content": format!(
            "Task task-001: Real review loop\n\nreview_request:\n```json\n{current_final}\n```\n\n<system-reminder>\nRetrieved for possible relevance.\n[Memory 2]\n```json\n{stale_impl}\n```\n</system-reminder>"
        )}
    ]});
    let request = current_review_request(&body).expect("request");
    assert_eq!(request_stage(&request), "FINAL_DELIVERY");
    assert_eq!(
        request["repository_versions"][0]["integrated_commit"],
        json!("integrated-commit")
    );
}

fn instruction_round(prompt: &str) -> u32 {
    prompt
        .split("review round ")
        .nth(1)
        .and_then(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryMode {
    /// Stage 5B2a is opt-in; the accepted flow stops at DELIVERY_PENDING.
    Pending,
    /// The target branch is unchecked and delivered with a CAS update-ref.
    Cas,
    /// The target branch is checked out in a linked worktree with spaces.
    CheckedOut,
    /// The Coder's delivery command fails and it falsely claims success.
    Failed,
    /// A recorded plan was explicitly released and another owner holds it.
    Released,
    /// The parent permission policy denies the delivery Bash call.
    Denied,
    /// The live Router parent mode denies the delivery Bash call.
    ParentDenied,
    /// The host stops between the Coder's Git write and its observation.
    Interrupted,
    /// A receipt exists with no job, and the target moved before launch.
    StaleReceipt,
    /// The host crashes after the child spawns but before its handle is saved.
    SpawnCrash,
    /// A crashed delivery's saved operation contradicts this task/evidence.
    RecoveryMismatch,
    /// A bound receipt is redirected to another real job of the same Coder.
    BoundJobSwap,
    /// Delivered content equals the reviewed content; completion needs a real
    /// post-delivery FINAL_DELIVERY review of the delivered OIDs.
    CompleteIdentical,
    /// Changed content was reviewed FINAL_DELIVERY before delivery.
    CompleteChanged,
    /// Both final-delivery reports exist while the real Reviewer is still
    /// running; its durable state is removed before the completion decision.
    GateMissingState,
    /// Both final-delivery reports exist while the real Reviewer is still
    /// running; completion waits for its actual process to stop.
    GateWaitsForExit,
}

impl DeliveryMode {
    fn enabled(self) -> bool {
        self != Self::Pending
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Cas => "cas",
            Self::CheckedOut => "checked-out",
            Self::Failed => "failed",
            Self::Released => "released",
            Self::Denied => "denied",
            Self::ParentDenied => "parent-denied",
            Self::Interrupted => "interrupted",
            Self::StaleReceipt => "stale-receipt",
            Self::SpawnCrash => "spawn-crash",
            Self::RecoveryMismatch => "recovery-mismatch",
            Self::BoundJobSwap => "bound-job-swap",
            Self::CompleteIdentical => "complete-identical",
            Self::CompleteChanged => "complete-changed",
            Self::GateMissingState => "gate-missing-state",
            Self::GateWaitsForExit => "gate-waits-for-exit",
        }
    }
}

#[derive(Default)]
struct MockState {
    requests: Mutex<Vec<Value>>,
    released: Mutex<HashSet<String>>,
    /// Per-turn step counter keyed by the role and the turn's instruction text.
    turns: Mutex<std::collections::HashMap<String, usize>>,
    router_session: String,
    blocked_review: bool,
    /// The delivery command fails and the member fabricates a success report.
    fail_delivery: bool,
    /// Hold a successful delivery turn so a test can observe the active child.
    hold_delivery: bool,
    /// Tool results that contradict what the fixture asked for. A real member
    /// must not submit after a failed command, so a violation fails the test
    /// with the exact output instead of hiding it behind a later assertion.
    violations: Mutex<Vec<String>>,
    /// Hold the post-submission completion-review turn so the real Reviewer
    /// child stays alive after both reports are durable. Only the two
    /// completion-gate scenarios enable it.
    hold_finalization_exit: bool,
}

impl MockState {
    fn record(&self, entry: Value) {
        self.requests.lock().unwrap().push(entry);
    }
    fn release(&self, key: &str) {
        self.released.lock().unwrap().insert(key.to_owned());
    }
    fn released(&self, key: &str) -> bool {
        self.released.lock().unwrap().contains(key)
    }
    fn violation(&self, text: String) {
        self.violations.lock().unwrap().push(text);
    }

    fn assert_clean(&self) {
        let violations = self.violations.lock().unwrap();
        assert!(
            violations.is_empty(),
            "the mock member was asked to continue after a failed tool call: {violations:#?}"
        );
    }

    fn next_step(&self, key: String) -> usize {
        let mut turns = self.turns.lock().unwrap();
        let step = turns.entry(key).or_insert(0);
        let current = *step;
        *step += 1;
        current
    }
}

async fn wait_for_release(state: &MockState, key: &str) {
    for _ in 0..2400 {
        if state.released(key) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("mock model waited too long for release {key}");
}

/// How many times the delivery command actually ran, counted from the tool
/// results the mock saw rather than from job records.
fn delivery_tool_calls(state: &MockState) -> usize {
    state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request["role"] == "coder"
                && request["prompt"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains("deliver the host-verified integration"))
                && request["last_tool_result"]
                    .as_str()
                    .is_some_and(|result| result.starts_with("Exit code:"))
        })
        .count()
}

/// Waits until the delivery Bash tool actually ran, while the mock still holds
/// the child's next response.
async fn wait_for_delivery_tool(state: &MockState) {
    for _ in 0..2400 {
        let ran = state.requests.lock().unwrap().iter().any(|request| {
            request["role"] == "coder"
                && request["prompt"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains("deliver the host-verified integration"))
                && request["last_tool_result"]
                    .as_str()
                    .is_some_and(|result| result.contains("Exit code: 0"))
        });
        if ran {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the delivery command did not run");
}

/// Waits until the integration Bash tool actually ran, while the mock still
/// holds the child's report.
async fn wait_for_integration_tool(state: &MockState) {
    for _ in 0..2400 {
        let ran = state.requests.lock().unwrap().iter().any(|request| {
            request["role"] == "coder"
                && request["prompt"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains("Prepare integration"))
                && request["last_tool_result"]
                    .as_str()
                    .is_some_and(|result| result.starts_with("Exit code:"))
        });
        if ran {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the integration command did not run");
}

/// How many times the integration merge command actually ran.
fn integration_tool_calls(state: &MockState) -> usize {
    state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request["role"] == "coder"
                && request["prompt"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains("Prepare integration"))
                && request["last_tool_result"]
                    .as_str()
                    .is_some_and(|result| result.starts_with("Exit code:"))
        })
        .count()
}

fn tool_output(result: &str) -> String {
    result
        .split_once("Output:\n")
        .map(|(_, output)| output.trim().to_owned())
        .unwrap_or_else(|| result.trim().to_owned())
}

fn integration_repository(prompt: &str) -> String {
    prompt
        .split("repository ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("repo-1")
        .to_owned()
}

/// The bounded delivery command the host put in a fenced `sh` block.
fn delivery_command_from_prompt(prompt: &str) -> String {
    let block = prompt
        .split_once("```sh\n")
        .and_then(|(_, rest)| rest.split_once("\n```"))
        .map(|(command, _)| command.trim().to_owned())
        .unwrap_or_default();
    assert!(!block.is_empty(), "no delivery command in {prompt}");
    block
}

async fn coder_response(
    prompt: &str,
    step: usize,
    last_result: Option<&str>,
    fail: bool,
    state: &MockState,
) -> String {
    if prompt.contains("deliver the host-verified integration") {
        return if fail {
            match step {
                0 => stream(
                    tool_call(
                        "delivery-denied",
                        "Bash",
                        json!({
                            "shell":"sh",
                            "command":"printf 'delivery permission denied\\n' >&2; false",
                            "timeout":30000
                        }),
                    ),
                    "tool_calls",
                ),
                // A fabricated success claim must never be accepted as
                // delivery; the host reads the real target instead. A typed
                // CrpSubmit at this phase is rejected by the state machine, so
                // the claim is a plain message naming a wrong object.
                1 => stream(
                    json!({"content":"Delivery succeeded: refs/heads/develop is now 1111111111111111111111111111111111111111"}),
                    "stop",
                ),
                _ => stream(json!({"content":"delivery reported"}), "stop"),
            }
        } else {
            match step {
                0 => stream(
                    tool_call(
                        "delivery-bash",
                        "Bash",
                        json!({
                            "shell":"sh",
                            "command": format!("pwd && {}", delivery_command_from_prompt(prompt)),
                            "timeout":60000
                        }),
                    ),
                    "tool_calls",
                ),
                _ => stream(json!({"content":"delivery complete"}), "stop"),
            }
        };
    }
    if prompt.contains("Prepare integration") {
        match step {
            0 => stream(
                tool_call(
                    "merge-target",
                    "Bash",
                    json!({
                        "shell":"sh",
                        "command":"git rev-parse develop && git merge --no-edit develop >/dev/null && git rev-parse HEAD",
                        "timeout":60000
                    }),
                ),
                "tool_calls",
            ),
            1 => {
                let result = last_result.unwrap_or_default();
                let output = tool_output(result);
                let mut lines = output
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty());
                let before = lines.next().unwrap_or_default().to_owned();
                let after = lines.next().unwrap_or_default().to_owned();
                let oid = |value: &str| {
                    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                };
                if !result.contains("Exit code: 0") || !oid(&before) || !oid(&after) {
                    state.violation(format!(
                        "the integration merge did not yield two commits: {result}"
                    ));
                    return stream(json!({"content": "the integration merge failed"}), "stop");
                }
                // Hold the real child after its Git command but before its typed
                // report. Repeated host ticks must not duplicate its operation
                // while the task is still Integrating.
                wait_for_release(state, "integration-hold").await;
                submit(
                    "integration-submit",
                    "integration",
                    json!({
                        "spec_version": 1,
                        "round": instruction_round(prompt),
                        "targets": [{
                            "repository_id": integration_repository(prompt),
                            "target_branch": "develop",
                            "before_commit": before,
                            "after_commit": after,
                            "conflict": false,
                            "tests_passed": true,
                            "issues": []
                        }]
                    }),
                )
            }
            _ => stream(json!({"content":"integration submitted"}), "stop"),
        }
    } else if prompt.contains("requested changes") {
        match step {
            0 => stream(
                tool_call(
                    "fix-bash",
                    "Bash",
                    json!({"shell":"sh","command":format!(
                        "printf '{CORRECTED_CONTENT}\\n' > file.txt && git add file.txt && git commit -q -m 'apply reviewed correction'"
                    ),"timeout":30000}),
                ),
                "tool_calls",
            ),
            1 => {
                let result = last_result.unwrap_or_default();
                if !result.contains("Exit code: 0") {
                    state.violation(format!(
                        "the reviewed correction command failed instead of committing: {result}"
                    ));
                    return stream(
                        json!({"content": format!("the correction command failed: {result}")}),
                        "stop",
                    );
                }
                let round = instruction_round(prompt);
                submit(
                    "fix-implementation",
                    "implementation",
                    json!({
                        "spec_version": 1,
                        "round": round,
                        "summary": "applied the reviewed correction",
                        "commits": ["HEAD"]
                    }),
                )
            }
            _ => stream(json!({"content":"correction submitted"}), "stop"),
        }
    } else if prompt.contains("was approved") {
        match step {
            0 => stream(
                tool_call(
                    "impl-bash",
                    "Bash",
                    json!({"shell":"sh","command":format!(
                        "printf '{FIRST_CONTENT}\\n' > file.txt && git add file.txt && git commit -q -m 'first implementation'"
                    ),"timeout":30000}),
                ),
                "tool_calls",
            ),
            1 => submit(
                "impl-submit",
                "implementation",
                json!({
                    "spec_version": 1,
                    "round": instruction_round(prompt),
                    "summary": "first implementation",
                    "commits": ["HEAD"]
                }),
            ),
            _ => stream(json!({"content":"implementation submitted"}), "stop"),
        }
    } else if prompt.contains("implementation plan") {
        match step {
            0 => submit(
                "plan-submit",
                "plan",
                json!({
                    "spec_version": 1,
                    "round": 0,
                    "understanding": "write the value into file.txt",
                    "scope": ["file.txt"],
                    "steps": ["write the file", "commit"],
                    "verification": ["git diff"],
                    "risks": []
                }),
            ),
            _ => stream(json!({"content":"plan submitted"}), "stop"),
        }
    } else {
        stream(json!({"content":"coder idle"}), "stop")
    }
}

async fn reviewer_response(
    state: &MockState,
    request: &Value,
    stage: &str,
    step: usize,
    finalizing: bool,
) -> String {
    let round = request["review_round"].as_u64().unwrap_or(0);
    let coder = request["coder"]["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let repository = request["repository_versions"][0]["repository_id"]
        .as_str()
        .unwrap_or("repo-1")
        .to_owned();
    let commit = request["repository_versions"][0]["submitted_commit"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if finalizing {
        // The post-delivery completion review of the already delivered content.
        // A different prompt and purpose; the delivered OIDs are the reviewed
        // ones. The harness is held so the host must not decide on one report.
        let integrated = request["repository_versions"][0]["integrated_commit"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        return match step {
            0 => stream(
                tool_call(
                    "finalizing-read",
                    "Bash",
                    json!({
                        "shell":"sh",
                        "command":"pwd && cat file.txt && git rev-parse HEAD",
                        "timeout":30000
                    }),
                ),
                "tool_calls",
            ),
            1 => submit(
                "finalizing-review",
                "review",
                json!({
                    "verdict": "APPROVED",
                    "spec_version": 1,
                    "round": round,
                    "stage": "FINAL_DELIVERY",
                    "reviewed_versions": [{"repository_id": repository, "commit": integrated}],
                    "acceptance_results": [{
                        "criterion": "AC-001",
                        "passed": true,
                        "evidence": [CORRECTED_CONTENT]
                    }],
                    "issues": [],
                    "verification": ["pwd; cat file.txt; git rev-parse HEAD"],
                    "limitations": []
                }),
            ),
            2 => {
                wait_for_release(state, "finalization-harness").await;
                submit(
                    "finalizing-harness",
                    "harness",
                    json!({
                        "spec_version": 1,
                        "round": round,
                        "stage": "FINAL_DELIVERY",
                        "findings": []
                    }),
                )
            }
            3 => {
                // The harness submission already returned "Report recorded".
                // The real child is alive and waiting for this response, which
                // makes "both reports durable, process still running" a
                // protocol barrier instead of a sleep.
                if state.hold_finalization_exit {
                    wait_for_release(state, "finalization-exit").await;
                }
                stream(json!({"content": "completion review submitted"}), "stop")
            }
            _ => stream(json!({"content": "completion review submitted"}), "stop"),
        };
    }
    if stage == "FINAL_DELIVERY" {
        // The final-delivery review of the host-verified integrated content.
        let integrated = request["repository_versions"][0]["integrated_commit"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        return match step {
            0 => stream(
                tool_call(
                    "final-read",
                    "Bash",
                    json!({
                        "shell":"sh",
                        "command":"pwd && cat file.txt && cat other.txt && git rev-parse HEAD",
                        "timeout":30000
                    }),
                ),
                "tool_calls",
            ),
            1 => submit(
                "final-review",
                "review",
                json!({
                    "verdict": "APPROVED",
                    "spec_version": 1,
                    "round": round,
                    "stage": "FINAL_DELIVERY",
                    "reviewed_versions": [{"repository_id": repository, "commit": integrated}],
                    "acceptance_results": [{
                        "criterion": "AC-001",
                        "passed": true,
                        "evidence": [CORRECTED_CONTENT, "other.txt"]
                    }],
                    "issues": [],
                    "verification": ["pwd; cat file.txt; cat other.txt; git rev-parse HEAD"],
                    "limitations": []
                }),
            ),
            2 => {
                // Hold the harness until the host has seen the review-only
                // state, then complete the final evidence.
                wait_for_release(state, "final-harness-r1").await;
                submit(
                    "final-harness",
                    "harness",
                    json!({
                        "spec_version": 1,
                        "round": round,
                        "stage": "FINAL_DELIVERY",
                        "findings": []
                    }),
                )
            }
            _ => stream(json!({"content":"final review submitted"}), "stop"),
        };
    }
    if round == 0 {
        match step {
            0 => stream(
                tool_call(
                    "reviewer-messages",
                    "SessionMessages",
                    json!({"session_id": coder}),
                ),
                "tool_calls",
            ),
            1 => stream(
                tool_call(
                    "reviewer-denied",
                    "SessionMetadata",
                    json!({"session_id": state.router_session}),
                ),
                "tool_calls",
            ),
            2 => {
                let verdict = if state.blocked_review {
                    "BLOCKED"
                } else {
                    "CHANGES_REQUESTED"
                };
                let issues = if state.blocked_review {
                    json!([{
                        "severity": "CRITICAL",
                        "location": "file.txt",
                        "description": "REVIEWER_CANNOT_VERIFY: the workspace is not trustworthy",
                        "expected": "a verifiable workspace"
                    }])
                } else {
                    json!([{
                        "severity": "HIGH",
                        "location": "file.txt",
                        "description": ISSUE,
                        "expected": "file.txt contains value=2"
                    }])
                };
                submit(
                    "review-0",
                    "review",
                    json!({
                        "verdict": verdict,
                        "spec_version": 1,
                        "round": 0,
                        "stage": "IMPLEMENTATION",
                        "reviewed_versions": [{"repository_id": repository, "commit": commit}],
                        "acceptance_results": [{
                            "criterion": "AC-001",
                            "passed": false,
                            "evidence": [FIRST_CONTENT]
                        }],
                        "issues": issues,
                        "verification": ["read file.txt"],
                        "limitations": []
                    }),
                )
            }
            3 => {
                // Both outcomes hold the harness until the host has observed
                // the review-only state.
                wait_for_release(state, "review-r0").await;
                submit(
                    "harness-0",
                    "harness",
                    json!({"spec_version": 1, "round": 0, "findings": []}),
                )
            }
            _ => stream(json!({"content":"review submitted"}), "stop"),
        }
    } else {
        match step {
            0 => stream(
                tool_call(
                    "reviewer-read",
                    "Bash",
                    json!({
                        "shell":"sh",
                        "command":"pwd && cat file.txt && git rev-parse HEAD",
                        "timeout":30000
                    }),
                ),
                "tool_calls",
            ),
            1 => submit(
                "harness-1",
                "harness",
                json!({"spec_version": 1, "round": round, "findings": []}),
            ),
            2 => {
                // Harness first: the host must not decide on a single report.
                wait_for_release(state, "harness-r1").await;
                submit(
                    "review-1",
                    "review",
                    json!({
                        "verdict": "APPROVED",
                        "spec_version": 1,
                        "round": round,
                        "stage": "IMPLEMENTATION",
                        "reviewed_versions": [{"repository_id": repository, "commit": commit}],
                        "acceptance_results": [{
                            "criterion": "AC-001",
                            "passed": true,
                            "evidence": [CORRECTED_CONTENT]
                        }],
                        "issues": [],
                        "verification": ["cat file.txt"],
                        "limitations": []
                    }),
                )
            }
            _ => stream(json!({"content":"round one reviewed"}), "stop"),
        }
    }
}

async fn mock_handler(
    state: Arc<MockState>,
    body: Value,
) -> ([(&'static str, &'static str); 1], String) {
    let system = system_text(&body);
    let prompt = current_instruction(&body);
    let role = if system.contains("/ Reviewer") {
        "reviewer"
    } else if system.contains("/ Coder") {
        "coder"
    } else {
        "other"
    };
    if let Ok(path) = std::env::var("MOCK_DUMP") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{}", serde_json::to_string(&body).unwrap());
        }
    }
    // Only the Reviewer receives a durable review_request. A Coder instruction
    // may quote earlier conversation text, which must not be mistaken for one.
    let request = if role == "reviewer" {
        current_review_request(&body)
    } else {
        None
    };
    let stage = request
        .as_ref()
        .map(request_stage)
        .unwrap_or("IMPLEMENTATION")
        .to_owned();
    // A stable per-turn key: the member, the requested evidence stage and the
    // version the host asked about. Tool steps advance per turn.
    let turn_key = match &request {
        Some(request) => format!(
            "reviewer|{}|{}|{}",
            request["review_round"],
            stage,
            request["repository_versions"][0]["integrated_commit"]
                .as_str()
                .or_else(|| request["repository_versions"][0]["submitted_commit"].as_str())
                .unwrap_or("")
        ),
        None => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&prompt, &mut hasher);
            format!(
                "coder|{}|{:x}",
                instruction_round(&prompt),
                std::hash::Hasher::finish(&hasher)
            )
        }
    };
    let step = state.next_step(turn_key);
    let last_result = last_tool_result(&body);
    let tool_results: Vec<String> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect();
    state.record(json!({
        "role": role,
        "step": step,
        "stage": stage,
        "prompt": prompt,
        "last_tool_result": last_result,
        "tool_results": tool_results
    }));
    // Hold the delivery turn after the command ran so a test can observe an
    // active child whose target already matches the approved commit.
    if role == "coder"
        && prompt.contains("deliver the host-verified integration")
        && step >= 1
        && state.hold_delivery
    {
        wait_for_release(&state, "delivery-hold").await;
    }
    let response = match role {
        "coder" => {
            coder_response(
                &prompt,
                step,
                last_result.as_deref(),
                state.fail_delivery,
                &state,
            )
            .await
        }
        "reviewer" => {
            let finalizing = role == "reviewer" && prompt.contains("final completion review");
            reviewer_response(
                &state,
                request.as_ref().unwrap_or(&json!({})),
                &stage,
                step,
                finalizing,
            )
            .await
        }
        _ => stream(json!({"content":"unused"}), "stop"),
    };
    ([("content-type", "text/event-stream")], response)
}

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    repo: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("file.txt"), "initial\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        git(&repo, &["switch", "-q", "-c", "develop"]);
        std::fs::write(repo.join("file.txt"), "develop\n").unwrap();
        git(&repo, &["commit", "-q", "-am", "develop"]);
        git(&repo, &["switch", "-q", "main"]);
        let repo = repo.canonicalize().unwrap();
        let project = temp.path().canonicalize().unwrap();
        Self {
            _temp: temp,
            data,
            repo,
            project,
        }
    }

    fn spec(&self) -> TaskSpec {
        TaskSpec {
            title: "Real review loop".into(),
            workspace: self.project.clone(),
            targets: vec![self.repo.to_string_lossy().into_owned()],
            goal: "Make file.txt contain the reviewed value".into(),
            requirements: vec!["Keep the repository valid".into()],
            acceptance_criteria: vec![deepseek_code_core::super_agent::AcceptanceCriterion {
                id: "AC-001".into(),
                description: "file.txt contains value=2".into(),
            }],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: Some("develop".into()),
            repository_target_branches: Default::default(),
        }
    }

    fn config_with_delivery(enabled: bool) -> SuperAgentConfig {
        SuperAgentConfig {
            approvals: ApprovalsConfig {
                plan: PlanApprovalPolicy::Auto,
            },
            delivery: DeliveryConfig { enabled },
            ..Default::default()
        }
    }
}

async fn wait_idle(data: &Path, router: &str, expected: usize) {
    for _ in 0..800 {
        if let Ok(jobs) =
            deepseek_code_core::services::background_sessions::list(data, Some(router))
        {
            let running = jobs
                .iter()
                .filter(|job| {
                    matches!(job.status.as_str(), "starting" | "running" | "unresponsive")
                })
                .count();
            if jobs.len() >= expected && running == 0 {
                let failed: Vec<_> = jobs
                    .iter()
                    .filter(|job| job.status != "completed")
                    .collect();
                assert!(
                    failed.is_empty(),
                    "child job did not complete: {:#?}",
                    failed
                        .iter()
                        .map(|job| (job.id.clone(), job.status.clone(), job.error.clone()))
                        .collect::<Vec<_>>()
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let jobs = deepseek_code_core::services::background_sessions::list(data, Some(router))
        .unwrap_or_default();
    panic!(
        "child jobs did not settle: {:#?}",
        jobs.iter()
            .map(|job| (job.id.clone(), job.status.clone(), job.error.clone()))
            .collect::<Vec<_>>()
    );
}

fn jobs(data: &Path, router: &str) -> Vec<deepseek_code_core::services::background_sessions::Job> {
    deepseek_code_core::services::background_sessions::list(data, Some(router)).unwrap_or_default()
}

fn reports(store: &TaskStore, task_id: &str) -> Vec<deepseek_code_core::super_agent::Report> {
    store.reports(task_id).unwrap()
}

fn events(store: &TaskStore, task_id: &str) -> Vec<deepseek_code_core::super_agent::Event> {
    store.events(task_id).unwrap()
}

/// Runs Git without panicking so a failing assertion can report the real state
/// instead of losing it with the temporary directory.
fn git_text(cwd: &Path, args: &[&str]) -> String {
    match Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
    {
        Ok(output) => format!(
            "exit={} stdout={:?} stderr={:?}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("spawn failed: {error}"),
    }
}

/// The exact task-worktree facts around a blocked tick: status, diff, HEAD and
/// the files present, plus the events and reports that led there.
fn worktree_diagnostics(store: &TaskStore, task_id: &str) -> Value {
    let task = store.get_task(task_id).ok().flatten();
    let (phase, status, repositories) = match &task {
        Some(task) => (
            task.phase.as_str(),
            task.status.as_str(),
            task.repositories
                .iter()
                .map(|repo| {
                    let path = &repo.working_path;
                    let mut files: Vec<String> = std::fs::read_dir(path)
                        .map(|entries| {
                            entries
                                .filter_map(|entry| entry.ok())
                                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                                .filter(|name| name != ".git")
                                .collect()
                        })
                        .unwrap_or_default();
                    files.sort();
                    json!({
                        "repository_id": repo.repository_id,
                        "working_path": path,
                        "head": git_text(path, &["rev-parse", "HEAD"]),
                        "submitted_commit": repo.submitted_commit,
                        "status": git_text(path, &["status", "--porcelain=v1", "--untracked-files=all"]),
                        "diff": git_text(path, &["diff", "HEAD"]),
                        "files": files,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        None => ("missing", "missing", Vec::new()),
    };
    json!({
        "phase": phase,
        "status": status,
        "repositories": repositories,
        "events": store.events(task_id).unwrap_or_default().iter().map(|event| json!({
            "sequence": event.sequence,
            "type": event.event_type.as_str(),
            "round": event.review_round,
            "processed": event.processed,
        })).collect::<Vec<_>>(),
        "reports": store.reports(task_id).unwrap_or_default().iter().map(|report| json!({
            "kind": format!("{:?}", report.kind),
            "round": report.round,
            "spec_version": report.spec_version,
        })).collect::<Vec<_>>(),
    })
}

/// Fails with the real worktree state when a tick blocks unexpectedly, so the
/// failure names the dirty files instead of only the message.
fn assert_unblocked(
    report: &deepseek_code_core::super_agent::orchestrator::TickReport,
    store: &TaskStore,
    task_id: &str,
    state: &MockState,
) {
    if report.blocked.is_empty() {
        return;
    }
    let mut diagnostics = worktree_diagnostics(store, task_id);
    diagnostics["requests"] = json!(state
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| json!({
            "role": request["role"],
            "step": request["step"],
            "stage": request["stage"],
            "last_tool_result": request["last_tool_result"],
        }))
        .collect::<Vec<_>>());
    panic!(
        "unexpected blocked tick: {report:?}\n{}",
        serde_json::to_string_pretty(&diagnostics).unwrap_or_default()
    );
}

/// Waits for a member report without waiting for the child process: the mock
/// model intentionally holds its next response until the host has observed the
/// one-report state, so waiting for process exit here would deadlock.
async fn wait_for_report<F>(store: &TaskStore, task_id: &str, mut ready: F)
where
    F: FnMut(&[deepseek_code_core::super_agent::Report]) -> bool,
{
    for _ in 0..800 {
        if ready(&store.reports(task_id).unwrap_or_default()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "expected member report did not appear: {:#?}",
        store.reports(task_id).unwrap_or_default()
    );
}

/// Waits for a member report, but fails immediately when every child has
/// stopped and the report is still missing. A silent deadline is not evidence:
/// the real job status and error are reported instead.
async fn wait_for_report_fail_fast<F>(
    store: &TaskStore,
    data: &Path,
    router: &str,
    task_id: &str,
    mut ready: F,
) where
    F: FnMut(&[deepseek_code_core::super_agent::Report]) -> bool,
{
    for _ in 0..800 {
        if ready(&store.reports(task_id).unwrap_or_default()) {
            return;
        }
        let jobs = jobs(data, router);
        let active = jobs
            .iter()
            .any(|job| matches!(job.status.as_str(), "starting" | "running" | "unresponsive"));
        if !jobs.is_empty() && !active {
            let states: Vec<_> = jobs
                .iter()
                .map(|job| (job.id.clone(), job.status.clone(), job.error.clone()))
                .collect();
            panic!(
                "the member child stopped without the required report: {states:#?}\nreports: {:#?}",
                store.reports(task_id).unwrap_or_default()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "expected member report did not appear: {:#?}",
        store.reports(task_id).unwrap_or_default()
    );
}

/// Waits until the real finalizing Reviewer child has received the recorded
/// harness result and is blocked on the held model response. The mock records
/// the step-3 request before it waits, so this is a protocol barrier rather
/// than a fixed sleep.
async fn wait_for_finalization_exit_request(state: &MockState) {
    for _ in 0..2400 {
        let waiting = {
            let requests = state.requests.lock().unwrap();
            requests.iter().any(|request| {
                request["role"] == "reviewer"
                    && request["step"] == json!(3)
                    && request["prompt"]
                        .as_str()
                        .is_some_and(|prompt| prompt.contains("final completion review"))
            })
        };
        if waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let requests: Vec<Value> = state.requests.lock().unwrap().clone();
    panic!("the finalizing Reviewer never reached the held exit barrier: {requests:#?}");
}

/// The Linux start time of one PID, so a signal is bound to the exact process
/// that was observed and never to a later process that reused the PID.
#[cfg(target_os = "linux")]
fn proc_start_ticks(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    // Field 22 (`starttime`); the first token after `comm` is field 3.
    rest.split_whitespace().nth(19).map(str::to_owned)
}

#[cfg(target_os = "linux")]
fn signal_exact(pid: u32, start: &str, signal: &str) {
    assert_eq!(
        proc_start_ticks(pid).as_deref(),
        Some(start),
        "PID {pid} no longer has the recorded start identity; refusing to signal it"
    );
    let status = Command::new("kill")
        .args([format!("-{signal}"), pid.to_string()])
        .status()
        .expect("running kill");
    assert!(status.success(), "kill -{signal} {pid} failed: {status}");
}

/// Freezes only the recorded supervisor so its heartbeat cannot recreate the
/// removed state file while the real Reviewer child stays alive. The identity
/// is re-verified for every signal, including the drop-path resumption.
#[cfg(target_os = "linux")]
struct FrozenSupervisor {
    pid: u32,
    start: String,
    frozen: bool,
}

#[cfg(target_os = "linux")]
impl FrozenSupervisor {
    fn freeze(pid: u32) -> Self {
        let start = proc_start_ticks(pid)
            .unwrap_or_else(|| panic!("no start identity for supervisor PID {pid}"));
        // Hold the exact identity as a guard *before* signalling. If the bounded
        // wait for the stopped state or an identity assertion panics, `Drop`
        // still resumes this same process instead of leaving it stopped. `Drop`
        // re-checks the start time, so a reused PID is never signalled.
        let guard = Self {
            pid,
            start,
            frozen: true,
        };
        signal_exact(guard.pid, &guard.start, "STOP");
        // Signal success is not proof that the heartbeat writer stopped.
        for _ in 0..400 {
            if proc_state(guard.pid).as_deref() == Some("T") {
                assert_eq!(
                    proc_start_ticks(guard.pid).as_deref(),
                    Some(guard.start.as_str()),
                    "supervisor PID {pid} changed identity while stopping"
                );
                return guard;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("supervisor PID {pid} did not reach the stopped state");
    }

    fn thaw(&mut self) {
        if self.frozen {
            signal_exact(self.pid, &self.start, "CONT");
            self.frozen = false;
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for FrozenSupervisor {
    fn drop(&mut self) {
        if self.frozen {
            if proc_start_ticks(self.pid).as_deref() == Some(self.start.as_str()) {
                let _ = Command::new("kill")
                    .args(["-CONT", &self.pid.to_string()])
                    .status();
            }
            self.frozen = false;
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().next().map(str::to_owned)
}

/// The resumed supervisor rewrites the `state.json` the fixture deleted. Wait
/// for that durable publication before releasing the held response, so the
/// child's final control-channel read cannot race a missing file.
#[cfg(target_os = "linux")]
fn wait_for_republished_state(data: &Path, job_id: &str) {
    let path = data
        .join("background-tasks")
        .join(job_id)
        .join("state.json");
    for _ in 0..400 {
        if std::fs::read(&path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "the resumed supervisor did not republish {}",
        path.display()
    );
}

/// Waits for the deliberately damaged job to record a real terminal status.
/// A missing `state.json` is not exit proof: only the exact job's terminal
/// record counts. The damaged child may legitimately fail because its control
/// directory lost state, so success is not required.
#[cfg(target_os = "linux")]
fn wait_damaged_job_terminal(data: &Path, job_id: &str) -> String {
    for _ in 0..1200 {
        if let Ok(deepseek_code_core::services::background_sessions::JobSnapshot::Present(job)) =
            deepseek_code_core::services::background_sessions::inspect(data, job_id)
        {
            if matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
                return job.status;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the damaged job {job_id} did not reach a terminal state");
}

/// True once the exact recorded process is gone, or its PID now carries a
/// different start time (the recorded instance exited). Only `/proc` identity
/// is accepted; a missing job file proves nothing about the process.
#[cfg(target_os = "linux")]
fn wait_instance_stopped(pid: u32, start: &str) -> bool {
    for _ in 0..1200 {
        match proc_start_ticks(pid) {
            None => return true,
            Some(current) if current != start => return true,
            Some(_) => {}
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn proc_start_ticks(_: u32) -> Option<String> {
    None
}

/// A process the completion-gate cleanup must never touch.
struct Sentinel(std::process::Child);

impl Sentinel {
    fn start(&self) -> String {
        proc_start_ticks(self.0.id()).unwrap_or_else(|| panic!("no sentinel start identity"))
    }

    fn assert_alive(&mut self) {
        assert!(
            self.0.try_wait().unwrap().is_none(),
            "the sentinel process was terminated by cleanup"
        );
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(not(target_os = "linux"))]
struct FrozenSupervisor;

#[cfg(not(target_os = "linux"))]
impl FrozenSupervisor {
    fn freeze(_: u32) -> Self {
        panic!("the completion-gate state-damage scenario is Linux-only");
    }
    fn thaw(&mut self) {}
}

/// The exact reserved job directory names, used to prove that a reopened host
/// started no replacement without trusting the discovery list (which ignores
/// damaged or missing state).
fn job_directory_ids(data: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(data.join("background-tasks"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

/// Durable settings rows under one prefix, keyed and ordered, so adding or
/// rewriting a launch operation or resume marker is visible.
fn setting_entries(context: &ServiceContext, prefix: &str) -> Vec<(String, String)> {
    let pattern = format!("{prefix}%");
    context
        .db
        .with_conn(|conn| -> anyhow::Result<Vec<(String, String)>> {
            let mut statement = conn.prepare(
                "SELECT key,value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key",
            )?;
            let rows = statement.query_map([pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let rows: Vec<(String, String)> = rows.collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .unwrap()
        .unwrap()
}

fn operation_prompts(context: &ServiceContext, task_id: &str) -> Vec<Value> {
    let prefix = format!("super-agent.launch-op.{task_id}.%");
    context
        .db
        .with_conn(|conn| -> anyhow::Result<Vec<Value>> {
            let mut statement = conn
                .prepare("SELECT value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key")?;
            let rows = statement.query_map([prefix], |row| row.get::<_, String>(0))?;
            Ok(rows
                .filter_map(|row| row.ok())
                .filter_map(|value| serde_json::from_str::<Value>(&value).ok())
                .collect())
        })
        .unwrap()
        .unwrap()
}

/// The durable storage key of one repository's typed delivery operation.
fn delivery_operation_key(
    task: &deepseek_code_core::super_agent::TaskRecord,
    plan: &deepseek_code_core::super_agent::delivery::DeliveryPlan,
    coder: &str,
) -> String {
    let identity = deepseek_code_core::super_agent::delivery::DeliveryOperationIdentity::for_target(
        plan,
        &plan.repositories[0],
        coder,
    );
    format!(
        "super-agent.launch-op.{}.coder.resume.{}.{}.{}",
        task.task_id,
        task.spec_version,
        task.review_round,
        identity.purpose_key()
    )
}

/// Rewrites the delivery launch operation under its real key. The key is not
/// derived from the edited contents, so the stored record disagrees with the
/// key it was found under.
fn tamper_delivery_operation(context: &ServiceContext, key: &str, edit: impl FnOnce(&mut Value)) {
    let mut operation = context
        .setting::<Value>(key)
        .unwrap()
        .expect("delivery launch operation");
    edit(&mut operation);
    context.save_setting(key, &operation).unwrap();
}

/// Bytes of the worktree's real index file. A denied delivery must not rewrite
/// it, so the comparison is byte-exact rather than mtime-based.
fn index_bytes(worktree: &Path) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "--git-path", "index"])
        .env("LC_ALL", "C")
        .output()
        .expect("resolving the worktree index");
    assert!(
        output.status.success(),
        "cannot resolve the worktree index: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(!raw.is_empty(), "cannot resolve the worktree index");
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    };
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!("reading index {}: {error}", path.display());
    })
}

fn open(data: &Path) -> ServiceContext {
    ServiceContext::open(
        RuntimePaths::new(data.to_path_buf()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap()
}

async fn run_review_loop(
    blocked: bool,
) -> (Fixture, ServiceContext, String, Arc<MockState>, Value) {
    run_review_loop_with(blocked, DeliveryMode::Pending).await
}

async fn run_review_loop_with(
    blocked: bool,
    delivery: DeliveryMode,
) -> (Fixture, ServiceContext, String, Arc<MockState>, Value) {
    let content_changed = !matches!(
        delivery,
        DeliveryMode::CompleteIdentical
            | DeliveryMode::GateMissingState
            | DeliveryMode::GateWaitsForExit
    );
    let fixture = Fixture::new();
    let context = open(&fixture.data);
    let sessions = SessionStore::new(context.db.clone());
    let router = sessions
        .create(&fixture.project, "router-model", "fixture")
        .unwrap();
    role::set(&context.db, &router, Role::Router).unwrap();

    let state = Arc::new(MockState {
        router_session: router.clone(),
        blocked_review: blocked,
        fail_delivery: delivery == DeliveryMode::Failed,
        hold_finalization_exit: matches!(
            delivery,
            DeliveryMode::GateMissingState | DeliveryMode::GateWaitsForExit
        ),
        hold_delivery: !matches!(
            delivery,
            DeliveryMode::Failed
                | DeliveryMode::Denied
                | DeliveryMode::ParentDenied
                | DeliveryMode::StaleReceipt
        ),
        ..Default::default()
    });
    let handler_state = state.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let state = handler_state.clone();
            async move { mock_handler(state, body).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    std::fs::write(
        fixture.data.join("config.json"),
        serde_json::to_vec(&json!({
            "providers":[{
                "id":"fixture","name":"Fixture","provider_type":"openai","api_key":"fixture-key",
                "base_url":format!("http://{address}"),
                "models":[{
                    "id":"router-model","name":"router-model","enabled":true,"max_tokens":8192,
                    "supports_tools":true,"supports_streaming":true,
                    "context_window":128000,"cost_per_1k_input":null,"cost_per_1k_output":null
                }],
                "enabled":true,"is_default":true,"settings":{},
                "supports_web_search":false,"web_search_strategy":null,
                "web_search_tested_at":null,"web_search_test_reason":null
            }],
            "default_provider_id":"fixture"
        }))
        .unwrap(),
    )
    .unwrap();

    let provider = deepseek_code_core::native_engine::provider_manager::ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id":"fixture","name":"Fixture","base_url":format!("http://{address}"),
            "api_key":"fixture-key","api_format":"openai","models":[],"enabled":true
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id":"router-model","name":"router-model","enabled":true,
            "supports_vision":false,"supports_web_search":false
        }))
        .unwrap(),
    };
    let store = TaskStore::new(context.db.clone(), fixture.data.clone());
    let created = store
        .create_task(&router, fixture.spec(), "create")
        .unwrap();
    let task_id = created.task_id.clone();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
    // Completion scenarios keep delivery disabled until the pre-delivery
    // assertions are done, then enable it with a fresh Orchestrator. Older
    // scenarios keep the previous behavior exactly.
    let completion_mode = matches!(
        delivery,
        DeliveryMode::CompleteIdentical
            | DeliveryMode::CompleteChanged
            | DeliveryMode::GateMissingState
            | DeliveryMode::GateWaitsForExit
    );
    let early_delivery = delivery.enabled() && !completion_mode;
    // Reassigned whenever the host is reopened during the loop.
    let mut orchestrator = Orchestrator::new(
        context.clone(),
        Fixture::config_with_delivery(early_delivery),
        provider.clone(),
        "bypass".into(),
        Some(executable.clone()),
    );

    // 1. Prepare the real worktree and launch the real Coder, which submits a
    // plan through CrpSubmit.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    wait_idle(&fixture.data, &router, 1).await;

    // 2. The auto approval policy releases implementation; the same Coder is
    // resumed and commits the first version.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    wait_idle(&fixture.data, &router, 2).await;
    let task = store.get_task(&task_id).unwrap().unwrap();
    // `tick` processes pending events again after `advance`, so a fast child may
    // have submitted its report within this tick. Both are legitimate; only the
    // next tick is required to launch the Reviewer.
    assert!(
        matches!(task.phase, Phase::Implementing | Phase::Reviewing),
        "unexpected phase {} after the first implementation",
        task.phase.as_str()
    );
    let coder = task.coder.session_id.clone().expect("coder session");

    // 3. The host applies the submitted implementation, pins the commit and
    // launches the real Reviewer.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(
        task.phase,
        Phase::Reviewing,
        "the submitted implementation did not move the task to review"
    );
    let reviewer = task.reviewer.session_id.clone().expect("reviewer session");
    assert_ne!(
        coder, reviewer,
        "Coder and Reviewer must be distinct sessions"
    );
    let first_commit = task.repositories[0]
        .submitted_commit
        .clone()
        .expect("first pinned commit");
    assert_ne!(first_commit, task.repositories[0].base_commit);

    // The Reviewer submitted only its review (the mock holds the harness
    // response until the host has observed the single-report state).
    wait_for_report(&store, &task_id, |reports| {
        reports.iter().any(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Review && report.round == 0
        })
    })
    .await;
    let first_reports = reports(&store, &task_id);
    let round0_review = first_reports
        .iter()
        .find(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Review && report.round == 0
        })
        .expect("round 0 review report")
        .clone();
    assert_eq!(
        round0_review.body["reviewed_versions"][0]["commit"],
        json!(first_commit),
        "the Reviewer must review the pinned commit"
    );
    assert!(!first_reports.iter().any(|report| report.kind
        == deepseek_code_core::super_agent::ReportKind::Harness
        && report.round == 0));
    // One report must not advance the task.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.review_round, 0, "a single report advanced the round");
    assert_eq!(task.phase, Phase::Reviewing);
    assert!(
        !events(&store, &task_id)
            .iter()
            .any(|event| event.event_type == EventType::ReviewCompleted && event.review_round == 0),
        "the host decided with only one report"
    );
    state.release("review-r0");
    wait_idle(&fixture.data, &router, 3).await;

    if blocked {
        // A BLOCKED review keeps pending evidence and never resumes the Coder.
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        let task = store.get_task(&task_id).unwrap().unwrap();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::Reviewing);
        let approvals = store.approvals(&task_id).unwrap();
        assert!(
            approvals.iter().any(|approval| {
                approval.request_type == ApprovalType::DeliveryBlocker
                    && approval.status == deepseek_code_core::super_agent::ApprovalStatus::Pending
                    && approval.request["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty())
            }),
            "a blocked review needs a concrete blocker approval: {approvals:?}"
        );
        assert!(
            reports(&store, &task_id)
                .iter()
                .any(|report| report.kind == deepseek_code_core::super_agent::ReportKind::Harness),
            "blocked evidence must be preserved"
        );
        assert_eq!(
            jobs(&fixture.data, &router).len(),
            3,
            "a blocked review must not resume the Coder"
        );
        assert_eq!(
            store
                .get_task(&task_id)
                .unwrap()
                .unwrap()
                .coder
                .session_id
                .as_deref(),
            Some(coder.as_str())
        );
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        assert_eq!(jobs(&fixture.data, &router).len(), 3, "no duplicate work");
        let evidence = json!({
            "blocked": true,
            "task_id": task_id,
            "coder_session": coder,
            "reviewer_session": reviewer,
            "jobs": jobs(&fixture.data, &router),
            "approvals": approvals,
        });
        server.abort();
        return (fixture, context, task_id, state, evidence);
    }

    // 4. The host resumes the original Coder exactly once with the exact issue.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_unblocked(&report, &store, &task_id, &state);
    wait_idle(&fixture.data, &router, 4).await;
    state.assert_clean();
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.review_round, 1, "review round did not advance");
    assert!(
        matches!(task.phase, Phase::Implementing | Phase::Reviewing),
        "unexpected phase {} after the corrected implementation",
        task.phase.as_str()
    );
    assert_eq!(
        task.coder.session_id.as_deref(),
        Some(coder.as_str()),
        "the original Coder session was replaced"
    );
    let changes_prompt = operation_prompts(&context, &task_id)
        .into_iter()
        .find(|operation| {
            operation["role"] == "coder"
                && operation["kind"] == "resume"
                && operation["review_round"] == 1
        })
        .and_then(|operation| operation["prompt"].as_str().map(str::to_owned))
        .expect("the round 1 Coder resume operation");
    assert!(
        changes_prompt.contains(ISSUE),
        "the Coder did not receive the Reviewer issue: {changes_prompt}"
    );
    let changes_jobs = jobs(&fixture.data, &router)
        .iter()
        .filter(|job| job.session_id == coder)
        .count();
    assert_eq!(
        changes_jobs, 3,
        "the Coder must be resumed exactly once: {changes_jobs}"
    );

    // The earlier round's evidence is still intact.
    assert!(reports(&store, &task_id)
        .iter()
        .any(|report| report.report_id == round0_review.report_id
            && report.created_at == round0_review.created_at));

    // Reopen the task storage before the next round: durable operations and
    // reports must survive a fresh context.
    drop(orchestrator);
    let context = open(&fixture.data);
    let store = TaskStore::new(context.db.clone(), fixture.data.clone());
    orchestrator = Orchestrator::new(
        context.clone(),
        Fixture::config_with_delivery(early_delivery),
        provider.clone(),
        "bypass".into(),
        Some(executable.clone()),
    );

    // 5. The same Reviewer session is resumed for round 1.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_unblocked(&report, &store, &task_id, &state);
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Reviewing);
    assert_eq!(task.review_round, 1);
    assert_eq!(task.reviewer.session_id.as_deref(), Some(reviewer.as_str()));
    let second_commit = task.repositories[0]
        .submitted_commit
        .clone()
        .expect("second pinned commit");
    assert_ne!(second_commit, first_commit, "the fix was not committed");
    let coder_file = task.repositories[0].working_path.join("file.txt");
    let coder_bytes = std::fs::read_to_string(&coder_file).unwrap_or_default();
    let round1_workspace = operation_prompts(&context, &task_id)
        .into_iter()
        .find(|operation| {
            operation["role"] == "reviewer"
                && operation["kind"] == "resume"
                && operation["review_round"] == 1
        })
        .and_then(|operation| operation["workspace"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let reviewer_bytes =
        std::fs::read_to_string(Path::new(&round1_workspace).join("file.txt")).unwrap_or_default();
    let coder_prompts: Vec<String> = state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["role"] == "coder")
        .map(|request| {
            request["prompt"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(180)
                .collect()
        })
        .collect();
    assert_eq!(
        coder_bytes,
        "value=2\n",
        "the Coder worktree {} was not fixed; prompts={coder_prompts:#?}",
        coder_file.display()
    );
    assert_eq!(
        reviewer_bytes, "value=2\n",
        "the Reviewer checkout {round1_workspace} is stale; coder prompts={coder_prompts:#?}"
    );
    wait_for_report(&store, &task_id, |reports| {
        reports.iter().any(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Harness && report.round == 1
        })
    })
    .await;
    let round1_prompt = operation_prompts(&context, &task_id)
        .into_iter()
        .find(|operation| {
            operation["role"] == "reviewer"
                && operation["kind"] == "resume"
                && operation["review_round"] == 1
        })
        .and_then(|operation| {
            operation["prompt"]
                .as_str()
                .and_then(|prompt| review_requests(&strip_reminders(prompt)).into_iter().next())
        })
        .expect("round 1 review request");
    assert_eq!(round1_prompt["review_round"], 1);
    assert_eq!(
        round1_prompt["repository_versions"][0]["submitted_commit"],
        json!(second_commit),
        "the Reviewer was not given the corrected version"
    );
    // The exact Reviewer Bash result must show the process working directory,
    // the pinned bytes and the pinned commit. SessionMessages/transcript text is
    // not evidence of execution against the reviewed checkout.
    let bash_outputs: HashSet<String> = state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["role"] == "reviewer")
        .flat_map(|request| {
            request["tool_results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|result| result.as_str().map(str::to_owned))
        .filter(|result| result.starts_with("Exit code:"))
        .filter_map(|result| {
            result
                .split_once("Output:\n")
                .map(|(_, output)| output.trim().to_owned())
        })
        .collect();
    assert_eq!(
        bash_outputs.len(),
        1,
        "expected exactly one Reviewer Bash execution: {bash_outputs:#?}"
    );
    let bash_output = bash_outputs.iter().next().unwrap();
    let lines: Vec<&str> = bash_output.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "the Reviewer Bash must report cwd, content and HEAD: {bash_output:?}"
    );
    assert_eq!(
        std::fs::canonicalize(lines[0]).unwrap(),
        std::fs::canonicalize(&round1_workspace).unwrap(),
        "the Reviewer tool ran in {} instead of the pinned round 1 checkout {round1_workspace}",
        lines[0]
    );
    assert_eq!(lines[1], CORRECTED_CONTENT, "Reviewer read stale bytes");
    assert_eq!(lines[2], second_commit, "Reviewer read a different commit");
    let reviewer_row = context
        .db
        .with_conn(|conn| {
            deepseek_code_core::db::conversation_repo::get_conversation(conn, &reviewer)
        })
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(reviewer_row.workspace_path.as_deref().unwrap()).unwrap(),
        std::fs::canonicalize(&round1_workspace).unwrap(),
        "the Reviewer session metadata was not rebound to the round 1 checkout"
    );
    let reviewer_job = operation_prompts(&context, &task_id)
        .into_iter()
        .find(|operation| {
            operation["role"] == "reviewer"
                && operation["kind"] == "resume"
                && operation["review_round"] == 1
        })
        .and_then(|operation| operation["job_id"].as_str().map(str::to_owned))
        .expect("round 1 reviewer job");
    let launch =
        deepseek_code_core::services::background_sessions::launch_of(&fixture.data, &reviewer_job)
            .unwrap()
            .expect("round 1 reviewer launch record");
    assert_eq!(
        std::fs::canonicalize(&launch.cwd).unwrap(),
        std::fs::canonicalize(&round1_workspace).unwrap(),
        "the round 1 reviewer launch cwd is not the pinned checkout"
    );
    assert_eq!(git(&launch.cwd, &["rev-parse", "HEAD"]), second_commit);
    // Keep the diagnostic snapshot even if a later assertion fails.
    write_evidence(
        "round-10-real-review-loop-round1-diagnostics",
        &json!({
            "task_id": task_id,
            "coder_session": coder,
            "reviewer_session": reviewer,
            "first_commit": first_commit,
            "second_commit": second_commit,
            "coder_worktree": task.repositories[0].working_path,
            "coder_bytes": coder_bytes,
            "reviewer_workspace": round1_workspace,
            "reviewer_row": reviewer_row.workspace_path,
            "reviewer_launch_cwd": launch.cwd,
            "reviewer_launch_head": git(&launch.cwd, &["rev-parse", "HEAD"]),
            "reviewer_bash_output": bash_output,
            "jobs": jobs(&fixture.data, &router),
            "operations": operation_prompts(&context, &task_id),
        }),
    );

    // Harness first: the host must still not decide the new round.
    let round1_harness = reports(&store, &task_id)
        .into_iter()
        .find(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Harness && report.round == 1
        })
        .expect("round 1 harness report");
    assert_eq!(round1_harness.round, 1);
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Reviewing);
    assert_eq!(task.review_round, 1);
    assert!(
        !events(&store, &task_id)
            .iter()
            .any(|event| event.event_type == EventType::ReviewCompleted && event.review_round == 1),
        "the host decided the new round without the round 1 review"
    );
    state.release("harness-r1");
    wait_idle(&fixture.data, &router, 5).await;

    // 6. Both round 1 reports approve the corrected version. The target branch
    // moves independently before integration starts, so the integration
    // baseline captures the newer target content.
    let develop_before = if content_changed {
        move_target(&fixture.repo, "other.txt", "target work\n")
    } else {
        // Content-identical: the target does not move, so integration is a
        // fast-forward that keeps the reviewed commit and its tree.
        git(&fixture.repo, &["rev-parse", "develop"])
    };
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = store.get_task(&task_id).unwrap().unwrap();
    // A fast child can submit and be verified inside this same tick, so either
    // state is legitimate; the integration assertions below require the
    // verified preparation regardless.
    assert!(
        matches!(task.phase, Phase::Integrating | Phase::FinalReview),
        "unexpected phase {} after the round 1 approval",
        task.phase.as_str()
    );
    assert_eq!(task.review_round, 1);
    let round1_review = reports(&store, &task_id)
        .into_iter()
        .find(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Review
                && report.round == 1
                && report.body["verdict"] == "APPROVED"
        })
        .expect("approved round 1 review");
    assert_eq!(
        round1_review.body["reviewed_versions"][0]["commit"],
        json!(second_commit)
    );
    let decided = events(&store, &task_id)
        .into_iter()
        .find(|event| event.event_type == EventType::ReviewCompleted && event.review_round == 1)
        .expect("round 1 review decision");
    assert_eq!(
        decided.payload["review_report_id"],
        json!(round1_review.report_id)
    );
    assert_ne!(round1_review.report_id, round0_review.report_id);
    assert!(!events(&store, &task_id).iter().any(|event| matches!(
        event.event_type,
        EventType::IntegrationCompleted | EventType::TaskCompleted | EventType::CleanupCompleted
    )));

    // The integration child is real and is held after its Git command but
    // before its report. Repeated ticks must reuse the exact integration
    // operation without launching another child or rerunning the command.
    wait_for_integration_tool(&state).await;
    let integration_op = operation_prompts(&context, &task_id)
        .into_iter()
        .find(|operation| {
            operation["role"] == "coder"
                && operation["purpose"] == "integration"
                && operation["review_round"] == 1
        })
        .expect("integration operation");
    let integration_job = integration_op["job_id"]
        .as_str()
        .expect("integration job id")
        .to_owned();
    let integration_jobs = jobs(&fixture.data, &router).len();
    let integration_commands = integration_tool_calls(&state);
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    assert_eq!(
        jobs(&fixture.data, &router).len(),
        integration_jobs,
        "a repeated tick launched another child while integration was in progress"
    );
    let repeated_op = operation_prompts(&context, &task_id)
        .into_iter()
        .find(|operation| {
            operation["role"] == "coder"
                && operation["purpose"] == "integration"
                && operation["review_round"] == 1
        })
        .expect("integration operation after a repeated tick");
    assert_eq!(
        repeated_op["job_id"],
        json!(integration_job),
        "a repeated tick replaced the integration operation"
    );
    assert_eq!(
        integration_tool_calls(&state),
        integration_commands,
        "a repeated tick reran the integration command"
    );
    let held = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(held.phase, Phase::Integrating);
    assert_eq!(held.status, Status::Running);
    assert_eq!(held.coder.session_id.as_deref(), Some(coder.as_str()));
    assert_eq!(held.reviewer.session_id.as_deref(), Some(reviewer.as_str()));
    // Release the exact integration child and let it submit its report. No new
    // job starts until the host processes that report.
    state.release("integration-hold");
    wait_for_report(&store, &task_id, |reports| {
        reports.iter().any(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Integration
                && report.round == 1
        })
    })
    .await;
    wait_idle(&fixture.data, &router, integration_jobs).await;

    // 7. Stage5A: the same Coder prepares integration inside its own task
    // worktree, and the host verifies real Git state.
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    wait_for_report(&store, &task_id, |reports| {
        reports.iter().any(|report| {
            report.kind == deepseek_code_core::super_agent::ReportKind::Integration
                && report.round == 1
        })
    })
    .await;
    let integration_report = reports(&store, &task_id)
        .into_iter()
        .find(|report| report.kind == deepseek_code_core::super_agent::ReportKind::Integration)
        .expect("integration claim");
    let claimed = integration_report.body["targets"][0].clone();
    let report = orchestrator.tick(&router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = store.get_task(&task_id).unwrap().unwrap();
    let integrated = task.repositories[0]
        .integrated_commit
        .clone()
        .expect("verified integrated commit");
    if content_changed {
        assert_ne!(
            integrated, second_commit,
            "integration did not merge the target"
        );
        assert_eq!(
            task.phase,
            Phase::FinalReview,
            "changed integrated content requires a final review"
        );
    } else {
        assert_eq!(
            integrated, second_commit,
            "an unchanged integration keeps the reviewed commit"
        );
        assert_eq!(
            task.phase,
            Phase::DeliveryPending,
            "unchanged content reuses the implementation review for delivery"
        );
    }
    assert_eq!(
        task.repositories[0].integration_status,
        deepseek_code_core::super_agent::IntegrationStatus::Prepared,
        "the host recorded a prepared integration"
    );
    assert_eq!(
        task.repositories[0].reviewed_commit.as_deref(),
        Some(second_commit.as_str())
    );
    // The claim was verified against the real commit, not trusted verbatim.
    let actual_head = git(&task.repositories[0].working_path, &["rev-parse", "HEAD"]);
    assert_eq!(claimed["after_commit"], json!(actual_head));
    assert_eq!(claimed["before_commit"], json!(develop_before));
    assert_eq!(integrated, actual_head);
    // The user's target branch was not changed.
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "develop"]),
        develop_before
    );
    // No delivery or completion fact exists.
    assert!(!events(&store, &task_id).iter().any(|event| matches!(
        event.event_type,
        EventType::IntegrationCompleted | EventType::TaskCompleted | EventType::CleanupCompleted
    )));
    let prepared = events(&store, &task_id)
        .into_iter()
        .find(|event| event.event_type == EventType::IntegrationPrepared && event.review_round == 1)
        .expect("integration prepared event");
    assert_eq!(prepared.payload["tree_changed"], json!(content_changed));
    assert_eq!(
        prepared.payload["repositories"][0]["integrated_commit"],
        json!(integrated)
    );

    if content_changed {
        // 8. The same Reviewer session reviews the immutable integrated checkout.
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        wait_for_report(&store, &task_id, |reports| {
            reports.iter().any(|report| {
                report.kind == deepseek_code_core::super_agent::ReportKind::Review
                    && report.round == 1
                    && report.body["stage"] == "FINAL_DELIVERY"
            })
        })
        .await;
        let final_review = reports(&store, &task_id)
            .into_iter()
            .find(|report| {
                report.kind == deepseek_code_core::super_agent::ReportKind::Review
                    && report.round == 1
                    && report.body["stage"] == "FINAL_DELIVERY"
            })
            .expect("final delivery review");
        assert_eq!(
            final_review.body["reviewed_versions"][0]["commit"],
            json!(integrated)
        );
        // The final Reviewer ran in the immutable integrated checkout. The exact
        // cwd, bytes and HEAD come from its real Bash tool output, and its durable
        // resume operation is stage-specific.
        let final_op = operation_prompts(&context, &task_id)
            .into_iter()
            .find(|operation| {
                operation["role"] == "reviewer"
                    && operation["purpose"] == "final-delivery"
                    && operation["review_round"] == 1
            })
            .expect("final delivery reviewer operation");
        let implementation_op = operation_prompts(&context, &task_id)
            .into_iter()
            .find(|operation| {
                operation["role"] == "reviewer"
                    && operation["purpose"] == "implementation"
                    && operation["review_round"] == 1
            })
            .expect("implementation reviewer operation");
        assert_ne!(
            final_op["job_id"], implementation_op["job_id"],
            "the final review reused the implementation review job"
        );
        assert_ne!(final_op["prompt"], implementation_op["prompt"]);
        let final_workspace = final_op["workspace"]
            .as_str()
            .expect("final review workspace")
            .to_owned();
        let final_job = final_op["job_id"]
            .as_str()
            .expect("final job id")
            .to_owned();
        let final_launch =
            deepseek_code_core::services::background_sessions::launch_of(&fixture.data, &final_job)
                .unwrap()
                .expect("final reviewer launch record");
        assert_eq!(
            std::fs::canonicalize(&final_launch.cwd).unwrap(),
            std::fs::canonicalize(&final_workspace).unwrap(),
            "the final reviewer launch cwd is not the integrated checkout"
        );
        assert_eq!(git(&final_launch.cwd, &["rev-parse", "HEAD"]), integrated);
        let reviewer_outputs: Vec<String> = state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["role"] == "reviewer")
            .flat_map(|request| {
                request["tool_results"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|result| result.as_str().map(str::to_owned))
            .filter(|result| result.starts_with("Exit code:"))
            .filter_map(|result| {
                result
                    .split_once("Output:\n")
                    .map(|(_, output)| output.trim().to_owned())
            })
            .collect();
        let final_output = reviewer_outputs
        .iter()
        .find(|output| output.contains("target work"))
        .cloned()
        .unwrap_or_else(|| {
            let requests: Vec<Value> = state
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["role"] == "reviewer")
                .map(|request| {
                    json!({
                        "step": request["step"],
                        "prompt": request["prompt"].as_str().unwrap_or_default()
                            .chars().take(220).collect::<String>(),
                        "results": request["tool_results"]
                    })
                })
                .collect();
            panic!(
                "the final reviewer did not read the integrated checkout: {reviewer_outputs:#?}\nrequests: {requests:#?}"
            )
        });
        let lines: Vec<&str> = final_output.lines().collect();
        assert_eq!(lines.len(), 4, "{final_output:?}");
        assert_eq!(
            std::fs::canonicalize(lines[0]).unwrap(),
            std::fs::canonicalize(&final_workspace).unwrap(),
            "the final reviewer tool ran in the wrong checkout"
        );
        assert_eq!(lines[1], CORRECTED_CONTENT);
        assert_eq!(lines[2], "target work");
        assert_eq!(lines[3], integrated);

        // Reopening the task storage must not relaunch the Reviewer or lose the
        // final-review evidence.
        let jobs_before_reopen = jobs(&fixture.data, &router).len();
        drop(orchestrator);
        let context = open(&fixture.data);
        let store = TaskStore::new(context.db.clone(), fixture.data.clone());
        orchestrator = Orchestrator::new(
            context.clone(),
            Fixture::config_with_delivery(early_delivery),
            provider.clone(),
            "bypass".into(),
            Some(executable.clone()),
        );
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        assert_eq!(
            jobs(&fixture.data, &router).len(),
            jobs_before_reopen,
            "reopening duplicated the final review launch"
        );

        // The review alone must not decide the final round: the implementation-stage
        // harness is still present but belongs to the earlier review.
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        let task = store.get_task(&task_id).unwrap().unwrap();
        assert_eq!(task.phase, Phase::FinalReview);
        assert!(
            !events(&store, &task_id).iter().any(|event| {
                event.event_type == EventType::ReviewCompleted
                    && event.payload["stage"].as_str() == Some("FINAL_DELIVERY")
            }),
            "the final review was decided without its own harness"
        );
        state.release("final-harness-r1");
        wait_for_report(&store, &task_id, |reports| {
            reports.iter().any(|report| {
                report.kind == deepseek_code_core::super_agent::ReportKind::Harness
                    && report.round == 1
                    && report.body["stage"] == "FINAL_DELIVERY"
            })
        })
        .await;
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        let task = store.get_task(&task_id).unwrap().unwrap();
        assert_eq!(
            task.phase,
            Phase::DeliveryPending,
            "approved integrated content must wait for delivery"
        );
        assert_eq!(task.status, Status::Running);
        assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
        assert_eq!(task.reviewer.session_id.as_deref(), Some(reviewer.as_str()));
        let final_decision = events(&store, &task_id)
            .into_iter()
            .find(|event| {
                event.event_type == EventType::ReviewCompleted
                    && event.payload["stage"].as_str() == Some("FINAL_DELIVERY")
            })
            .expect("final delivery decision");
        assert_eq!(
            final_decision.payload["review_report_id"],
            json!(final_review.report_id)
        );
        // Still nothing delivered and no completion.
        assert!(!events(&store, &task_id).iter().any(|event| matches!(
            event.event_type,
            EventType::IntegrationCompleted
                | EventType::TaskCompleted
                | EventType::CleanupCompleted
        )));
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "develop"]),
            develop_before
        );
    }

    // Repeated ticks after the final decision must not duplicate work.
    let settled_jobs = jobs(&fixture.data, &router).len();
    // Facts for the two real completion-gate scenarios; null otherwise.
    let mut completion_gate = Value::Null;
    if delivery == DeliveryMode::Pending {
        // Still nothing delivered and no completion.
        assert!(!events(&store, &task_id).iter().any(|event| matches!(
            event.event_type,
            EventType::IntegrationCompleted
                | EventType::TaskCompleted
                | EventType::CleanupCompleted
        )));
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "develop"]),
            develop_before
        );

        // Repeated ticks after the final decision must not duplicate work.
        let report = orchestrator.tick(&router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        assert_eq!(jobs(&fixture.data, &router).len(), settled_jobs);
        // Bounded cleanup: every real child process exits normally after the
        // host decision; this is evidence, not a requirement for the phase
        // transition.
        wait_idle(&fixture.data, &router, settled_jobs).await;
    } else if delivery == DeliveryMode::Released {
        // A plan recorded earlier no longer owns the branch. The delivery must
        // not dispatch, write Git, or take the other owner's lock.
        let plan = match deepseek_code_core::super_agent::delivery::prepare_delivery(&store, &task)
            .unwrap()
        {
            deepseek_code_core::super_agent::delivery::DeliveryPreflight::Planned(plan) => plan,
            other => panic!("expected a prepared plan, got {other:?}"),
        };
        let release = deepseek_code_core::super_agent::delivery::release_delivery(&store, &plan);
        assert!(release.complete(), "{release:?}");
        let lock_key = plan.repositories[0].lock_key.clone();
        store
            .try_lock("competing:lock", &lock_key, "competing-task")
            .unwrap();

        let report = orchestrator.tick(&router).await.unwrap();
        assert!(
            !report.blocked.is_empty(),
            "delivery without ownership must block: {report:?}"
        );
        assert_eq!(
            jobs(&fixture.data, &router).len(),
            settled_jobs,
            "delivery without ownership dispatched a child"
        );
        let task = store.get_task(&task_id).unwrap().unwrap();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::DeliveryPending);
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "develop"]),
            develop_before
        );
        assert_eq!(store.locks("competing-task").unwrap(), vec![lock_key]);
        assert!(!events(&store, &task_id)
            .iter()
            .any(|event| event.event_type == EventType::IntegrationCompleted));
    } else {
        // Stage5B2a: exactly one delivery dispatch through the SAME Coder, then
        // a host verification of the real target and its checkout.
        let mut checkout = None;
        if delivery == DeliveryMode::CheckedOut {
            let common = git(
                &fixture.repo,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
            let path = PathBuf::from(common).join("target checkout");
            git(
                &fixture.repo,
                &["worktree", "add", "-q", &path.to_string_lossy(), "develop"],
            );
            checkout = Some(path.canonicalize().unwrap());
        }
        if delivery == DeliveryMode::Denied {
            // The current policy denies shell execution for this session. The
            // resumed delivery child must receive the denial; it must not keep
            // the authority of the mode it was first launched with.
            context
                .save_setting(
                    &format!("cli.permissions.{coder}"),
                    &json!({"Bash": "Deny"}),
                )
                .unwrap();
        }
        if delivery == DeliveryMode::ParentDenied {
            // Change the live Router policy only. The Orchestrator remains
            // constructed with bypass and no Coder-specific deny exists, so a
            // denial proves the resumed child read the parent's persisted mode
            // instead of keeping its cached launch authority.
            deepseek_code_core::services::session_permission_mode::set(
                &context.db,
                &router,
                &deepseek_code_core::runtime::default_permissions(),
                deepseek_code_core::permissions::PermissionMode::DontAsk,
            )
            .unwrap();
        }
        let mut moved_target = None;
        if delivery == DeliveryMode::StaleReceipt {
            // A previous host persisted a valid typed receipt but never
            // launched, then the target moved. The fresh launch must not
            // happen: the live drift check has to reject the stale plan.
            let prepared =
                match deepseek_code_core::super_agent::delivery::prepare_delivery(&store, &task)
                    .unwrap()
                {
                    deepseek_code_core::super_agent::delivery::DeliveryPreflight::Planned(plan) => {
                        plan
                    }
                    other => panic!("expected a prepared plan, got {other:?}"),
                };
            let identity =
                deepseek_code_core::super_agent::delivery::DeliveryOperationIdentity::for_target(
                    &prepared,
                    &prepared.repositories[0],
                    &coder,
                );
            store
                .begin_repository_dispatch(
                    &deepseek_code_core::super_agent::delivery::DeliveryDispatch {
                        task_id: task_id.clone(),
                        spec_version: 1,
                        review_round: task.review_round,
                        plan_operation_id: prepared.operation_id.clone(),
                        coder_session_id: coder.clone(),
                        purpose: identity.purpose_key(),
                        repository_id: Some(prepared.repositories[0].repository_id.clone()),
                        old_oid: Some(prepared.repositories[0].old_oid.clone()),
                        new_oid: Some(prepared.repositories[0].new_oid.clone()),
                        attempt: prepared.repositories[0].attempt,
                        job_id: None,
                        created_at: "2026-01-01T00:00:00Z".into(),
                    },
                )
                .unwrap();
            moved_target = Some(move_target(&fixture.repo, "base.txt", "external move\n"));
        }
        let delivery_index_before = index_bytes(&task.repositories[0].working_path);
        if completion_mode {
            // All pre-delivery assertions above are complete. Enable delivery
            // now, so no earlier tick could have dispatched it.
            orchestrator = Orchestrator::new(
                context.clone(),
                Fixture::config_with_delivery(true),
                provider.clone(),
                "bypass".into(),
                Some(executable.clone()),
            );
        }
        let delivery_orchestrator = orchestrator;
        let report = if matches!(
            delivery,
            DeliveryMode::SpawnCrash | DeliveryMode::RecoveryMismatch
        ) {
            // Use the launcher's own crash boundary rather than the host tick:
            // the child is spawned, then the process stops before persisting
            // its handle. The task is never blocked, exactly as in a crash.
            let launcher = deepseek_code_core::super_agent::launcher::Launcher {
                context: context.clone(),
                config: Fixture::config_with_delivery(true),
                provider: provider.clone(),
                permission_mode: "bypass".into(),
                executable_override: Some(executable.clone()),
                gate: Some(std::sync::Arc::new(|boundary: &'static str| {
                    if boundary == "after-spawn" {
                        anyhow::bail!("injected host crash after spawn");
                    }
                    Ok(())
                })),
            };
            let plan =
                match deepseek_code_core::super_agent::delivery::prepare_delivery(&store, &task)
                    .unwrap()
                {
                    deepseek_code_core::super_agent::delivery::DeliveryPreflight::Planned(plan) => {
                        plan
                    }
                    other => panic!("expected a prepared plan, got {other:?}"),
                };
            let identity =
                deepseek_code_core::super_agent::delivery::DeliveryOperationIdentity::for_target(
                    &plan,
                    &plan.repositories[0],
                    &coder,
                );
            store
                .begin_repository_dispatch(
                    &deepseek_code_core::super_agent::delivery::DeliveryDispatch {
                        task_id: task_id.clone(),
                        spec_version: 1,
                        review_round: task.review_round,
                        plan_operation_id: plan.operation_id.clone(),
                        coder_session_id: coder.clone(),
                        purpose: identity.purpose_key(),
                        repository_id: Some(plan.repositories[0].repository_id.clone()),
                        old_oid: Some(plan.repositories[0].old_oid.clone()),
                        new_oid: Some(plan.repositories[0].new_oid.clone()),
                        attempt: plan.repositories[0].attempt,
                        job_id: None,
                        created_at: "2026-01-01T00:00:00Z".into(),
                    },
                )
                .unwrap();
            let error = launcher
                .resume_coder_for_delivery(
                    &task,
                    &deepseek_code_core::super_agent::launcher::coder_delivery_prompt(
                        &task,
                        &plan.repositories[0],
                    ),
                    &identity,
                )
                .await
                .expect_err("the injected crash must stop the launcher");
            assert!(
                format!("{error:#}").contains("injected host crash"),
                "{error:#}"
            );
            deepseek_code_core::super_agent::orchestrator::TickReport::default()
        } else {
            delivery_orchestrator.tick(&router).await.unwrap()
        };
        if !matches!(
            delivery,
            DeliveryMode::Failed
                | DeliveryMode::Denied
                | DeliveryMode::ParentDenied
                | DeliveryMode::StaleReceipt
        ) {
            assert_unblocked(&report, &store, &task_id, &state);
        }
        let mut dispatched_job = None;
        if delivery != DeliveryMode::StaleReceipt {
            assert_eq!(
                jobs(&fixture.data, &router).len(),
                settled_jobs + 1,
                "delivery did not dispatch exactly one child"
            );
            let delivery_op = operation_prompts(&context, &task_id)
                .into_iter()
                .find(|operation| {
                    operation["role"] == "coder"
                        && operation["kind"] == "resume"
                        && operation["purpose"].as_str().is_some_and(|purpose| {
                            purpose == "delivery" || purpose.starts_with("delivery.")
                        })
                })
                .expect("delivery launch operation");
            assert_eq!(delivery_op["resume_session"], json!(coder));
            assert_eq!(delivery_op["spec_version"], json!(1));
            assert!(delivery_op["prompt"].as_str().is_some_and(|prompt| {
                prompt.contains("deliver the host-verified integration")
            }));
            let plan = store
                .delivery_plans(&task_id)
                .unwrap()
                .pop()
                .expect("delivery plan");
            let receipt = store
                .repository_dispatch(
                    &task_id,
                    &plan.operation_id,
                    &plan.repositories[0].repository_id,
                )
                .unwrap()
                .expect("dispatch receipt");
            assert_eq!(receipt.coder_session_id, coder);
            if !matches!(
                delivery,
                DeliveryMode::SpawnCrash | DeliveryMode::RecoveryMismatch
            ) {
                // The spawn-crash boundary leaves the receipt unbound on
                // purpose; recovery must adopt the reserved child instead.
                assert_eq!(receipt.job_id.as_deref(), delivery_op["job_id"].as_str());
            } else {
                assert_eq!(receipt.job_id, None);
            }
            dispatched_job = delivery_op["job_id"].as_str().map(str::to_owned);
        }

        let worktree_path = store.get_task(&task_id).unwrap().unwrap().repositories[0]
            .working_path
            .clone();
        let worktree_before = (
            git_text(&worktree_path, &["rev-parse", "HEAD"]),
            git_text(
                &worktree_path,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            ),
            std::fs::read(worktree_path.join("file.txt")).ok(),
        );
        // From here the branch may replace the host with a reopened one.
        let mut active_store = store.clone();
        let mut active_orchestrator = delivery_orchestrator;

        if delivery == DeliveryMode::StaleReceipt {
            let moved = moved_target.expect("the target was moved externally");
            // The typed receipt and plan are valid, so recovery reaches the
            // live drift check and refuses the stale plan before any launch.
            assert!(
                report.blocked.iter().any(|reason| {
                    reason.contains("moved") && reason.contains("the approved plan is stale")
                }),
                "a moved target must block dispatch: {report:?}"
            );
            assert_eq!(
                jobs(&fixture.data, &router).len(),
                settled_jobs,
                "a stale receipt launched a child"
            );
            let task = store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(task.status, Status::Blocked);
            assert_eq!(task.phase, Phase::DeliveryPending);
            assert_eq!(
                task.repositories[0].integration_status,
                deepseek_code_core::super_agent::IntegrationStatus::Prepared
            );
            assert_eq!(git(&fixture.repo, &["rev-parse", "develop"]), moved);
            assert!(!events(&store, &task_id)
                .iter()
                .any(|event| event.event_type == EventType::IntegrationCompleted));
        } else if delivery == DeliveryMode::Denied {
            wait_idle(&fixture.data, &router, settled_jobs + 1).await;
            let _ = active_orchestrator.tick(&router).await.unwrap();
            let task = active_store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(task.status, Status::Blocked, "a denied delivery must block");
            assert_eq!(task.phase, Phase::DeliveryPending);
            assert_eq!(
                task.repositories[0].integration_status,
                deepseek_code_core::super_agent::IntegrationStatus::Prepared
            );
            assert_eq!(
                git(&fixture.repo, &["rev-parse", "develop"]),
                develop_before,
                "a denied delivery must not move the target"
            );
            assert_eq!(
                (
                    git_text(&worktree_path, &["rev-parse", "HEAD"]),
                    git_text(
                        &worktree_path,
                        &["status", "--porcelain=v1", "--untracked-files=all"]
                    ),
                    std::fs::read(worktree_path.join("file.txt")).ok(),
                ),
                worktree_before,
                "a denied delivery must not touch the task worktree"
            );
            assert!(
                !events(&active_store, &task_id).iter().any(|event| matches!(
                    event.event_type,
                    EventType::IntegrationCompleted
                        | EventType::TaskCompleted
                        | EventType::CleanupCompleted
                ))
            );
            // The child received a real permission denial from the engine,
            // not a shell failure produced by the fixture.
            let denial = state
                .requests
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|request| {
                    request["role"] == "coder"
                        && request["prompt"].as_str().is_some_and(|prompt| {
                            prompt.contains("deliver the host-verified integration")
                        })
                        && request["last_tool_result"]
                            .as_str()
                            .is_some_and(|result| result.contains("Permission denied"))
                })
                .and_then(|request| request["last_tool_result"].as_str().map(str::to_owned))
                .expect("the delivery Bash call must be denied by the permission engine");
            assert!(
                denial.contains("Permission denied: Explicit tool deny rule"),
                "{denial}"
            );
        } else if delivery == DeliveryMode::ParentDenied {
            wait_idle(&fixture.data, &router, settled_jobs + 1).await;
            let _ = active_orchestrator.tick(&router).await.unwrap();
            let task = active_store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(
                task.status,
                Status::Blocked,
                "a live-parent denied delivery must block"
            );
            assert_eq!(task.phase, Phase::DeliveryPending);
            assert_eq!(
                task.repositories[0].integration_status,
                deepseek_code_core::super_agent::IntegrationStatus::Prepared
            );
            // The live Router mode is the only denial source.
            assert!(
                context
                    .setting::<Value>(&format!("cli.permissions.{coder}"))
                    .unwrap()
                    .is_none(),
                "this scenario must not seed a Coder-specific deny rule"
            );
            assert_eq!(
                deepseek_code_core::services::session_permission_mode::load(&context.db, &coder)
                    .unwrap(),
                Some(deepseek_code_core::permissions::PermissionMode::DontAsk),
                "the resumed child did not adopt the live parent mode"
            );
            // Exact same Coder identity: the original session is resumed and the
            // delivery job is bound to it under the Router. The operation still
            // records the launch mode (`bypass`), so the denial can only come
            // from the live parent mode read before the tool call.
            let delivery_op = operation_prompts(&context, &task_id)
                .into_iter()
                .find(|operation| {
                    operation["role"] == "coder"
                        && operation["purpose"].as_str().is_some_and(
                            deepseek_code_core::super_agent::delivery::is_delivery_purpose,
                        )
                })
                .expect("delivery operation");
            assert_eq!(delivery_op["permission_mode"], json!("bypass"));
            assert_eq!(delivery_op["resume_session"], json!(coder));
            assert_eq!(delivery_op["session_id"], json!(coder));
            assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
            let delivery_job = dispatched_job
                .as_deref()
                .and_then(|id| {
                    jobs(&fixture.data, &router)
                        .into_iter()
                        .find(|job| job.id == id)
                })
                .expect("delivery job");
            assert_eq!(delivery_job.session_id, coder);
            assert_eq!(delivery_job.parent_session_id, router);
            assert_eq!(
                git(&fixture.repo, &["rev-parse", "develop"]),
                develop_before,
                "a live-parent denied delivery must not move the target"
            );
            assert_eq!(
                (
                    git_text(&worktree_path, &["rev-parse", "HEAD"]),
                    git_text(
                        &worktree_path,
                        &["status", "--porcelain=v1", "--untracked-files=all"]
                    ),
                    std::fs::read(worktree_path.join("file.txt")).ok(),
                ),
                worktree_before,
                "a live-parent denied delivery must not touch the task worktree or index"
            );
            assert_eq!(
                index_bytes(&worktree_path),
                delivery_index_before,
                "a live-parent denied delivery must not rewrite the Git index"
            );
            assert!(
                !events(&active_store, &task_id).iter().any(|event| matches!(
                    event.event_type,
                    EventType::IntegrationCompleted
                        | EventType::TaskCompleted
                        | EventType::CleanupCompleted
                ))
            );
            let denial = state
                .requests
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|request| {
                    request["role"] == "coder"
                        && request["prompt"].as_str().is_some_and(|prompt| {
                            prompt.contains("deliver the host-verified integration")
                        })
                        && request["last_tool_result"]
                            .as_str()
                            .is_some_and(|result| result.contains("Permission denied"))
                })
                .and_then(|request| request["last_tool_result"].as_str().map(str::to_owned))
                .expect("the delivery Bash call must be denied by the live parent mode");
            assert!(
                denial.contains(
                    "Permission denied: dontAsk mode: this action requires permission and was not pre-approved"
                ),
                "{denial}"
            );
        } else if delivery == DeliveryMode::Failed {
            wait_idle(&fixture.data, &router, settled_jobs + 1).await;
            let _ = active_orchestrator.tick(&router).await.unwrap();
            let task = active_store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(task.status, Status::Blocked, "a failed delivery must block");
            assert_eq!(task.phase, Phase::DeliveryPending);
            assert_eq!(
                git(&fixture.repo, &["rev-parse", "develop"]),
                develop_before,
                "a failed or falsely claimed delivery must not move the target"
            );
            assert!(
                !events(&active_store, &task_id).iter().any(|event| matches!(
                    event.event_type,
                    EventType::IntegrationCompleted
                        | EventType::TaskCompleted
                        | EventType::CleanupCompleted
                ))
            );
            // The fabricated member message exists but is never delivery proof,
            // and no typed integration report was recorded for this phase.
            assert!(!reports(&active_store, &task_id).iter().any(|report| {
                report.kind == deepseek_code_core::super_agent::ReportKind::Integration
                    && report.round == 2
            }));
        } else {
            // The mock holds after running the command so the target moves
            // while the child is still active. The host must not declare
            // delivery from that transient match.
            wait_for_delivery_tool(&state).await;
            assert_eq!(
                git(&fixture.repo, &["rev-parse", "develop"]),
                integrated,
                "the delivery command did not move the target"
            );
            let midflight = active_store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(
                midflight.phase,
                Phase::DeliveryPending,
                "an active delivery child must not finalize the task"
            );
            assert!(!events(&active_store, &task_id)
                .iter()
                .any(|event| event.event_type == EventType::IntegrationCompleted));

            if matches!(
                delivery,
                DeliveryMode::Interrupted
                    | DeliveryMode::SpawnCrash
                    | DeliveryMode::RecoveryMismatch
                    | DeliveryMode::BoundJobSwap
            ) {
                // The host is replaced after the Coder moved Git but before any
                // completion observation. `Interrupted` also drops the recorded
                // job binding, as if the previous host stopped after launching;
                // `SpawnCrash` keeps the real crash boundary, where the
                // operation exists without a handle but its job is running.
                // `RecoveryMismatch` starts from that same real boundary and
                // then corrupts the saved operation under its real key.
                let jobs_before_recovery = jobs(&fixture.data, &router).len();
                let calls_before_recovery = delivery_tool_calls(&state);
                drop(active_orchestrator);
                let reopened = open(&fixture.data);
                let reopened_store = TaskStore::new(reopened.db.clone(), fixture.data.clone());
                if delivery == DeliveryMode::Interrupted {
                    reopened
                        .db
                        .with_conn(|conn| {
                            conn.execute(
                                "UPDATE sa_delivery_dispatches SET job_id=NULL WHERE task_id=?1",
                                [&task_id],
                            )
                        })
                        .unwrap()
                        .unwrap();
                }
                if delivery == DeliveryMode::RecoveryMismatch {
                    // The record is found under its correct storage key, but its
                    // contents belong to another task. Contents, not the key,
                    // are the immutable identity.
                    let plan = reopened_store
                        .delivery_plans(&task_id)
                        .unwrap()
                        .pop()
                        .expect("delivery plan");
                    let key = delivery_operation_key(&task, &plan, &coder);
                    tamper_delivery_operation(&reopened, &key, |operation| {
                        operation["task_id"] = json!("task-999");
                    });
                }
                let mut swapped_from = None;
                if delivery == DeliveryMode::BoundJobSwap {
                    // Redirect the already-bound receipt to a different real
                    // job of the same Coder and Router: the plan-mode launch.
                    // A job of the same session is not proof that it belongs to
                    // this delivery.
                    let plan = reopened_store
                        .delivery_plans(&task_id)
                        .unwrap()
                        .pop()
                        .expect("delivery plan");
                    let receipt = reopened_store
                        .repository_dispatch(
                            &task_id,
                            &plan.operation_id,
                            &plan.repositories[0].repository_id,
                        )
                        .unwrap()
                        .expect("bound dispatch receipt");
                    let delivery_job = receipt.job_id.clone().expect("bound delivery job");
                    let other_job = jobs(&fixture.data, &router)
                        .into_iter()
                        .find(|job| job.session_id == coder && job.id != delivery_job)
                        .map(|job| job.id)
                        .expect("another real job of the same Coder");
                    reopened
                        .db
                        .with_conn(|conn| {
                            conn.execute(
                                "UPDATE sa_repository_dispatches SET job_id=?3 WHERE task_id=?1 AND plan_operation_id=?2",
                                [
                                    task_id.as_str(),
                                    plan.operation_id.as_str(),
                                    other_job.as_str(),
                                ],
                            )
                        })
                        .unwrap()
                        .unwrap();
                    swapped_from = Some((delivery_job, other_job, plan));
                }
                let reopened_orchestrator = Orchestrator::new(
                    reopened.clone(),
                    Fixture::config_with_delivery(true),
                    provider.clone(),
                    "bypass".into(),
                    Some(executable.clone()),
                );
                let report = reopened_orchestrator.tick(&router).await.unwrap();
                if delivery == DeliveryMode::RecoveryMismatch {
                    assert!(
                        report.blocked.iter().any(|reason| {
                            reason.contains("recorded delivery launch")
                                && reason.contains("task-999")
                        }),
                        "a contradictory operation must block explicitly: {report:?}"
                    );
                    assert_eq!(
                        jobs(&fixture.data, &router).len(),
                        jobs_before_recovery,
                        "recovery launched a second child"
                    );
                    assert_eq!(
                        delivery_tool_calls(&state),
                        calls_before_recovery,
                        "recovery reran the delivery command"
                    );
                    let blocked = reopened_store.get_task(&task_id).unwrap().unwrap();
                    assert_eq!(blocked.status, Status::Blocked);
                    assert_eq!(blocked.phase, Phase::DeliveryPending);
                    assert_eq!(
                        blocked.repositories[0].integration_status,
                        deepseek_code_core::super_agent::IntegrationStatus::Prepared,
                        "the host must not observe the borrowed job as delivery"
                    );
                    let plan = reopened_store
                        .delivery_plans(&task_id)
                        .unwrap()
                        .pop()
                        .expect("delivery plan");
                    let receipt = reopened_store
                        .repository_dispatch(
                            &task_id,
                            &plan.operation_id,
                            &plan.repositories[0].repository_id,
                        )
                        .unwrap()
                        .expect("dispatch receipt");
                    assert_eq!(
                        receipt.job_id, None,
                        "a contradictory operation must not bind the receipt"
                    );
                    assert!(!events(&reopened_store, &task_id)
                        .iter()
                        .any(|event| matches!(
                            event.event_type,
                            EventType::IntegrationCompleted
                                | EventType::TaskCompleted
                                | EventType::CleanupCompleted
                        )));
                    // The child's real write already moved the target; the host
                    // must not undo it or claim it as delivery.
                    assert_eq!(
                        git(&fixture.repo, &["rev-parse", "develop"]),
                        integrated,
                        "rejection must not rewind a real successful write"
                    );
                    // The contradictory record is preserved byte-for-byte for
                    // inspection; recovery never rewrites it to bypass the check.
                    let plan = reopened_store
                        .delivery_plans(&task_id)
                        .unwrap()
                        .pop()
                        .expect("delivery plan");
                    let key = delivery_operation_key(&task, &plan, &coder);
                    let preserved = reopened.setting::<Value>(&key).unwrap().unwrap();
                    assert_eq!(preserved["task_id"], json!("task-999"));
                    assert_eq!(preserved["handle"], Value::Null);
                    // Release the exact hostile child and let it exit.
                    state.release("delivery-hold");
                    wait_idle(&fixture.data, &router, jobs_before_recovery).await;
                    active_store = reopened_store;
                    active_orchestrator = reopened_orchestrator;
                } else if delivery == DeliveryMode::BoundJobSwap {
                    let (delivery_job, other_job, plan) =
                        swapped_from.expect("the swapped receipt identity");
                    // The receipt points at a real job of the same Coder, but
                    // not at the job this delivery's typed operation reserved.
                    // A same-session job is never proof of this delivery.
                    assert!(
                        report.blocked.iter().any(|reason| {
                            reason.contains(&other_job)
                                && reason.contains("refusing to observe a different child")
                        }),
                        "a swapped bound job must block: {report:?}"
                    );
                    assert_eq!(
                        jobs(&fixture.data, &router).len(),
                        jobs_before_recovery,
                        "the swapped receipt launched a child"
                    );
                    assert_eq!(
                        delivery_tool_calls(&state),
                        calls_before_recovery,
                        "the swapped receipt reran the delivery command"
                    );
                    let blocked = reopened_store.get_task(&task_id).unwrap().unwrap();
                    assert_eq!(blocked.status, Status::Blocked);
                    assert_eq!(blocked.phase, Phase::DeliveryPending);
                    assert_eq!(
                        blocked.repositories[0].integration_status,
                        deepseek_code_core::super_agent::IntegrationStatus::Prepared,
                        "the borrowed job must not be observed as delivery"
                    );
                    // The rejected observation rewrites neither record: the
                    // receipt still names the swapped job, and the reserved
                    // operation still names its own child.
                    let receipt = reopened_store
                        .repository_dispatch(
                            &task_id,
                            &plan.operation_id,
                            &plan.repositories[0].repository_id,
                        )
                        .unwrap()
                        .expect("dispatch receipt");
                    assert_eq!(
                        receipt.job_id.as_deref(),
                        Some(other_job.as_str()),
                        "the rejected observation rewrote the receipt"
                    );
                    let key = delivery_operation_key(&task, &plan, &coder);
                    let operation = reopened
                        .setting::<Value>(&key)
                        .unwrap()
                        .expect("delivery launch operation");
                    assert_eq!(
                        operation["job_id"],
                        json!(delivery_job),
                        "the rejected observation rewrote the reserved job"
                    );
                    assert_eq!(operation["handle"]["job_id"], json!(delivery_job));
                    assert_eq!(
                        git(&fixture.repo, &["rev-parse", "develop"]),
                        integrated,
                        "the rejected observation changed the target"
                    );
                    assert!(!events(&reopened_store, &task_id)
                        .iter()
                        .any(|event| matches!(
                            event.event_type,
                            EventType::IntegrationCompleted
                                | EventType::TaskCompleted
                                | EventType::CleanupCompleted
                        )));
                    state.release("delivery-hold");
                    wait_idle(&fixture.data, &router, jobs_before_recovery).await;
                    active_store = reopened_store;
                    active_orchestrator = reopened_orchestrator;
                } else {
                    assert_unblocked(&report, &reopened_store, &task_id, &state);
                    assert_eq!(
                        jobs(&fixture.data, &router).len(),
                        jobs_before_recovery,
                        "recovery launched a second child"
                    );
                    assert_eq!(
                        delivery_tool_calls(&state),
                        calls_before_recovery,
                        "recovery reran the delivery command"
                    );
                    assert_eq!(
                        reopened_store.get_task(&task_id).unwrap().unwrap().phase,
                        Phase::DeliveryPending,
                        "a running delivery must not be finalized on recovery"
                    );
                    assert!(!events(&reopened_store, &task_id)
                        .iter()
                        .any(|event| event.event_type == EventType::IntegrationCompleted));
                    let plan = reopened_store
                        .delivery_plans(&task_id)
                        .unwrap()
                        .pop()
                        .expect("delivery plan");
                    let rebound = reopened_store
                        .repository_dispatch(
                            &task_id,
                            &plan.operation_id,
                            &plan.repositories[0].repository_id,
                        )
                        .unwrap()
                        .expect("rebound dispatch receipt");
                    assert_eq!(
                        rebound.job_id.as_deref(),
                        dispatched_job.as_deref(),
                        "recovery did not adopt the original child"
                    );
                    active_store = reopened_store;
                    active_orchestrator = reopened_orchestrator;
                }
            }

            if delivery != DeliveryMode::RecoveryMismatch && delivery != DeliveryMode::BoundJobSwap
            {
                let report = active_orchestrator.tick(&router).await.unwrap();
                assert_unblocked(&report, &active_store, &task_id, &state);
                assert_eq!(jobs(&fixture.data, &router).len(), settled_jobs + 1);
                let still = active_store.get_task(&task_id).unwrap().unwrap();
                assert_eq!(still.phase, Phase::DeliveryPending);

                state.release("delivery-hold");
                wait_idle(&fixture.data, &router, settled_jobs + 1).await;
                let report = active_orchestrator.tick(&router).await.unwrap();
                assert_unblocked(&report, &active_store, &task_id, &state);
                assert_eq!(
                    delivery_tool_calls(&state),
                    1,
                    "the delivery command must run exactly once"
                );
                let task = active_store.get_task(&task_id).unwrap().unwrap();
                assert_eq!(
                    task.phase,
                    Phase::Finalizing,
                    "verified delivery finalizes the prepared content"
                );
                assert_eq!(task.status, Status::Running);
                assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
                assert_eq!(
                    task.repositories[0].integration_status,
                    deepseek_code_core::super_agent::IntegrationStatus::Completed
                );
                assert_eq!(git(&fixture.repo, &["rev-parse", "develop"]), integrated);
                if let Some(checkout) = &checkout {
                    assert_eq!(git(checkout, &["rev-parse", "HEAD"]), integrated);
                    assert_eq!(git(checkout, &["status", "--porcelain"]), "");
                    assert_eq!(
                        std::fs::read_to_string(checkout.join("file.txt"))
                            .unwrap()
                            .trim(),
                        CORRECTED_CONTENT
                    );
                    assert_eq!(
                        std::fs::read_to_string(checkout.join("other.txt"))
                            .unwrap()
                            .trim(),
                        "target work"
                    );
                }
                let delivered = events(&active_store, &task_id)
                    .into_iter()
                    .find(|event| event.event_type == EventType::IntegrationCompleted)
                    .expect("delivery observation");
                assert_eq!(delivered.payload["complete"], json!(true));
                assert_eq!(
                    delivered.payload["repositories"][0]["new_oid"],
                    json!(integrated)
                );
                assert_eq!(
                    delivered.payload["repositories"][0]["target_ref"],
                    json!("refs/heads/develop")
                );
                assert_eq!(delivered.payload["observed_without_dispatch"], json!(false));
                // Task completion and cleanup remain for later increments.
                assert!(
                    !events(&active_store, &task_id).iter().any(|event| matches!(
                        event.event_type,
                        EventType::TaskCompleted | EventType::CleanupCompleted
                    ))
                );

                // The real Bash call ran in the Coder's task worktree. The last
                // request of the delivery turn carries the delivery tool result.
                let coder_worktree = task.repositories[0].working_path.clone();
                let delivery_result = state
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|request| {
                        request["role"] == "coder"
                            && request["prompt"].as_str().is_some_and(|prompt| {
                                prompt.contains("deliver the host-verified integration")
                            })
                            && request["last_tool_result"]
                                .as_str()
                                .is_some_and(|result| result.starts_with("Exit code:"))
                    })
                    .and_then(|request| request["last_tool_result"].as_str().map(str::to_owned))
                    .expect("delivery Bash result");
                assert!(
                    delivery_result.contains("Exit code: 0"),
                    "{delivery_result}"
                );
                let output = delivery_result
                    .split_once("Output:\n")
                    .map(|(_, output)| output.trim())
                    .unwrap_or_default();
                assert_eq!(
                    std::fs::canonicalize(output.lines().next().unwrap_or_default()).unwrap(),
                    std::fs::canonicalize(&coder_worktree).unwrap(),
                    "the delivery command ran outside the Coder task worktree"
                );

                // Reopening the storage re-reads the recorded facts and does not
                // dispatch or write Git again.
                if matches!(
                    delivery,
                    DeliveryMode::CompleteIdentical
                        | DeliveryMode::CompleteChanged
                        | DeliveryMode::GateMissingState
                        | DeliveryMode::GateWaitsForExit
                ) {
                    // Completion adds one more history-aware step; its own
                    // reopen check below covers this.
                } else {
                    let jobs_before_reopen = jobs(&fixture.data, &router).len();
                    let reopen_context = open(&fixture.data);
                    let reopen_store =
                        TaskStore::new(reopen_context.db.clone(), fixture.data.clone());
                    let reopen = Orchestrator::new(
                        reopen_context.clone(),
                        Fixture::config_with_delivery(true),
                        provider.clone(),
                        "bypass".into(),
                        Some(executable.clone()),
                    );
                    let report = reopen.tick(&router).await.unwrap();
                    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
                    assert_eq!(
                        jobs(&fixture.data, &router).len(),
                        jobs_before_reopen,
                        "reopening replayed the delivery"
                    );
                    assert_eq!(
                        reopen_store.get_task(&task_id).unwrap().unwrap().phase,
                        Phase::Finalizing
                    );
                    assert_eq!(git(&fixture.repo, &["rev-parse", "develop"]), integrated);
                }

                if completion_mode {
                    let jobs_before = jobs(&fixture.data, &router).len();
                    let first = active_orchestrator.tick(&router).await.unwrap();
                    assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
                    let gate_mode = matches!(
                        delivery,
                        DeliveryMode::GateMissingState | DeliveryMode::GateWaitsForExit
                    );
                    if delivery == DeliveryMode::CompleteIdentical || gate_mode {
                        // The delivered content reused the implementation
                        // approval, so the same Reviewer is resumed once for a
                        // real final review of the delivered OIDs.
                        let task = active_store.get_task(&task_id).unwrap().unwrap();
                        assert_eq!(task.status, Status::Running);
                        assert_eq!(task.phase, Phase::Finalizing);
                        assert_eq!(
                            jobs(&fixture.data, &router).len(),
                            jobs_before + 1,
                            "the completion review must resume the existing Reviewer once"
                        );
                        let final_ops = operation_prompts(&context, &task_id)
                            .into_iter()
                            .filter(|operation| operation["purpose"] == "final-completion")
                            .collect::<Vec<_>>();
                        assert_eq!(final_ops.len(), 1, "{final_ops:?}");
                        assert_eq!(final_ops[0]["resume_session"], json!(reviewer));
                        wait_for_report_fail_fast(
                            &active_store,
                            &fixture.data,
                            &router,
                            &task_id,
                            |reports| {
                                reports.iter().any(|report| {
                                    report.kind
                                        == deepseek_code_core::super_agent::ReportKind::Review
                                        && report.round == 1
                                        && report.body["stage"] == "FINAL_DELIVERY"
                                })
                            },
                        )
                        .await;
                        // One report alone must not decide the completion.
                        let deciding = active_orchestrator.tick(&router).await.unwrap();
                        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");
                        assert!(
                            !events(&active_store, &task_id).iter().any(|event| {
                                event.event_type == EventType::ReviewCompleted
                                    && event.payload["stage"].as_str() == Some("FINAL_DELIVERY")
                            }),
                            "the completion review was decided without its harness"
                        );
                        state.release("finalization-harness");
                        wait_for_report_fail_fast(
                            &active_store,
                            &fixture.data,
                            &router,
                            &task_id,
                            |reports| {
                                reports.iter().any(|report| {
                                    report.kind
                                        == deepseek_code_core::super_agent::ReportKind::Harness
                                        && report.round == 1
                                        && report.body["stage"] == "FINAL_DELIVERY"
                                })
                            },
                        )
                        .await;
                        // The real CrpSubmit admitted both reports in the
                        // FINALIZING phase; a rejected submission would have
                        // shown up in the Reviewer's tool results.
                        let finalization_results: Vec<String> = state
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|request| {
                                request["role"] == "reviewer"
                                    && request["prompt"].as_str().is_some_and(|prompt| {
                                        prompt.contains("final completion review")
                                    })
                            })
                            .flat_map(|request| {
                                request["tool_results"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default()
                            })
                            .filter_map(|result| result.as_str().map(str::to_owned))
                            .collect();
                        assert!(
                            finalization_results
                                .iter()
                                .filter(|result| result.contains("Report recorded"))
                                .count()
                                >= 2,
                            "the real CrpSubmit must record the completion review and harness: {finalization_results:#?}"
                        );
                        assert!(
                            !finalization_results
                                .iter()
                                .any(|result| result.contains("not accepted")
                                    || result.contains("does not match")),
                            "the completion submission was rejected: {finalization_results:#?}"
                        );
                        if gate_mode {
                            // The mock holds step 3 after both reports were
                            // recorded, so the real Reviewer child is alive and
                            // waiting on its held model response.
                            wait_for_finalization_exit_request(&state).await;
                            let round = active_store
                                .get_task(&task_id)
                                .unwrap()
                                .unwrap()
                                .review_round;
                            let marker_key = format!(
                                "super-agent.reviewer-resume.{task_id}.1.{round}.final-completion"
                            );
                            let marker = context
                                .setting::<Value>(&marker_key)
                                .unwrap()
                                .expect("final completion Reviewer marker");
                            let job_id = marker["job_id"]
                                .as_str()
                                .filter(|id| !id.is_empty())
                                .expect("final completion job id")
                                .to_owned();
                            assert_eq!(
                                marker["session_id"].as_str(),
                                Some(reviewer.as_str()),
                                "the recorded completion Reviewer is not this task's Reviewer"
                            );
                            let plan = active_store
                                .delivery_plans(&task_id)
                                .unwrap()
                                .into_iter()
                                .filter(|plan| plan.spec_version == 1 && plan.review_round == round)
                                .max_by(|left, right| {
                                    left.created_at
                                        .cmp(&right.created_at)
                                        .then(left.operation_id.cmp(&right.operation_id))
                                })
                                .expect("persisted delivery plan");
                            assert_eq!(
                                marker["plan_operation_id"],
                                json!(plan.operation_id),
                                "the completion receipt does not name the persisted plan"
                            );
                            let job = match deepseek_code_core::services::background_sessions::inspect(
                                &fixture.data,
                                &job_id,
                            )
                            .unwrap()
                            {
                                deepseek_code_core::services::background_sessions::JobSnapshot::Present(job) => job,
                                other => panic!(
                                    "the final completion job {job_id} is not a readable running job: {other:?}"
                                ),
                            };
                            assert_eq!(job.id, job_id);
                            assert_eq!(job.session_id, reviewer);
                            assert_eq!(job.parent_session_id, router);
                            assert_eq!(
                                job.status, "running",
                                "the completion Reviewer child is not running"
                            );
                            let live = active_store.get_task(&task_id).unwrap().unwrap();
                            assert_eq!(live.phase, Phase::Finalizing);
                            assert_eq!(live.status, Status::Running);
                            let durable_reports = reports(&active_store, &task_id);
                            assert!(
                                durable_reports.iter().any(|report| {
                                    report.kind
                                        == deepseek_code_core::super_agent::ReportKind::Review
                                        && report.round == round
                                        && report.body["stage"] == "FINAL_DELIVERY"
                                }) && durable_reports.iter().any(|report| {
                                    report.kind
                                        == deepseek_code_core::super_agent::ReportKind::Harness
                                        && report.round == round
                                        && report.body["stage"] == "FINAL_DELIVERY"
                                }),
                                "both current final-delivery reports must be durable: {durable_reports:#?}"
                            );
                            let job_dir = fixture.data.join("background-tasks").join(&job_id);
                            let launch_bytes = std::fs::read(job_dir.join("launch.json")).unwrap();
                            // Reports prove submission, not exit. A tick with
                            // both reports and a live child only waits.
                            let settling = active_orchestrator.tick(&router).await.unwrap();
                            assert_eq!(settling.blocked, Vec::<String>::new(), "{settling:?}");
                            let waiting = active_orchestrator.tick(&router).await.unwrap();
                            assert_eq!(waiting.blocked, Vec::<String>::new(), "{waiting:?}");
                            let waiting_task = active_store.get_task(&task_id).unwrap().unwrap();
                            assert_eq!(waiting_task.phase, Phase::Finalizing);
                            assert_eq!(waiting_task.status, Status::Running);
                            assert!(
                                !events(&active_store, &task_id)
                                    .iter()
                                    .any(|event| event.event_type == EventType::TaskCompleted),
                                "an active completion Reviewer must never complete the task"
                            );

                            if delivery == DeliveryMode::GateWaitsForExit {
                                state.release("finalization-exit");
                                wait_idle(&fixture.data, &router, jobs_before + 1).await;
                                let stopped =
                                    match deepseek_code_core::services::background_sessions::inspect(
                                        &fixture.data,
                                        &job_id,
                                    )
                                    .unwrap()
                                    {
                                        deepseek_code_core::services::background_sessions::JobSnapshot::Present(job) => job,
                                        other => panic!(
                                            "the stopped completion job {job_id} is unreadable: {other:?}"
                                        ),
                                    };
                                assert_eq!(stopped.status, "completed");
                                completion_gate = json!({
                                    "mode": delivery.label(),
                                    "job_id": job_id,
                                    "session_id": reviewer,
                                    "parent_session_id": router,
                                    "job_status_at_reports": "running",
                                    "reports_durable_before_wait": true,
                                    "uncompleted_while_active": true,
                                    "released": true,
                                    "job_status_after_exit": stopped.status,
                                });
                            } else {
                                // Keep the real child alive and freeze only its
                                // supervisor so the heartbeat cannot recreate
                                // the state file; then damage exactly that job.
                                let supervisor_pid =
                                    job.supervisor_pid.expect("running job supervisor PID");
                                // Capture the exact observed instances before the
                                // fixture stops anything: after cleanup, only these
                                // PID+start identities may be reported as stopped.
                                let supervisor_start = proc_start_ticks(supervisor_pid)
                                    .expect("supervisor start identity");
                                let child_pid = job.child_pid.expect("running job child PID");
                                let child_start =
                                    proc_start_ticks(child_pid).expect("child start identity");
                                let job_dirs_before = job_directory_ids(&fixture.data);
                                let launch_ops_before = setting_entries(
                                    &context,
                                    &format!("super-agent.launch-op.{task_id}."),
                                );
                                let resume_markers_before = setting_entries(
                                    &context,
                                    &format!("super-agent.reviewer-resume.{task_id}."),
                                );
                                let mut supervisor = FrozenSupervisor::freeze(supervisor_pid);
                                std::fs::remove_file(job_dir.join("state.json")).unwrap();
                                assert!(!job_dir.join("state.json").exists());
                                let mut sentinel = Sentinel(
                                    Command::new("sleep")
                                        .arg("90")
                                        .spawn()
                                        .expect("sentinel process"),
                                );
                                let sentinel_start = sentinel.start();
                                let blocked = active_orchestrator.tick(&router).await.unwrap();
                                let blocked_reason = blocked
                                    .blocked
                                    .iter()
                                    .find(|reason| reason.contains(&job_id))
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        panic!(
                                            "the damaged completion job {job_id} must block with its exact identity: {blocked:?}"
                                        )
                                    });
                                let blocked_task =
                                    active_store.get_task(&task_id).unwrap().unwrap();
                                assert_eq!(blocked_task.status, Status::Blocked);
                                assert_eq!(blocked_task.phase, Phase::Finalizing);
                                assert!(
                                    !events(&active_store, &task_id)
                                        .iter()
                                        .any(|event| event.event_type == EventType::TaskCompleted),
                                    "a damaged completion job must never complete the task"
                                );
                                assert!(
                                    !job_dir.join("state.json").exists(),
                                    "the host recreated the damaged job state"
                                );
                                assert_eq!(
                                    std::fs::read(job_dir.join("launch.json")).unwrap(),
                                    launch_bytes,
                                    "the host rewrote another job file"
                                );
                                // A new host must stay blocked, preserve the
                                // damaged evidence and start no replacement.
                                let reopened_context = open(&fixture.data);
                                let reopened_store = TaskStore::new(
                                    reopened_context.db.clone(),
                                    fixture.data.clone(),
                                );
                                let reopened = Orchestrator::new(
                                    reopened_context.clone(),
                                    Fixture::config_with_delivery(true),
                                    provider.clone(),
                                    "bypass".into(),
                                    Some(executable.clone()),
                                );
                                let reopened_report = reopened.tick(&router).await.unwrap();
                                assert_eq!(
                                    reopened_report.blocked,
                                    Vec::<String>::new(),
                                    "a blocked task must not be re-run by a fresh host"
                                );
                                let reopened_task =
                                    reopened_store.get_task(&task_id).unwrap().unwrap();
                                assert_eq!(reopened_task.status, Status::Blocked);
                                assert_eq!(reopened_task.phase, Phase::Finalizing);
                                assert!(
                                    !events(&reopened_store, &task_id).iter().any(|event| {
                                        event.event_type == EventType::TaskCompleted
                                    }),
                                    "a reopened host completed from a damaged job"
                                );
                                assert!(
                                    !job_dir.join("state.json").exists(),
                                    "the reopened host recreated the damaged job state"
                                );
                                // The discovery list ignores a damaged job, so
                                // it cannot prove that no replacement started.
                                // Compare the exact durable sets instead.
                                assert_eq!(
                                    job_directory_ids(&fixture.data),
                                    job_dirs_before,
                                    "a reopened host created another job directory"
                                );
                                assert_eq!(
                                    setting_entries(
                                        &reopened_context,
                                        &format!("super-agent.launch-op.{task_id}.")
                                    ),
                                    launch_ops_before,
                                    "a reopened host added or rewrote a launch operation"
                                );
                                assert_eq!(
                                    setting_entries(
                                        &reopened_context,
                                        &format!("super-agent.reviewer-resume.{task_id}.")
                                    ),
                                    resume_markers_before,
                                    "a reopened host added or rewrote a resume marker"
                                );
                                let reopened_marker = reopened_context
                                    .setting::<Value>(&marker_key)
                                    .unwrap()
                                    .expect("final completion Reviewer marker");
                                assert_eq!(reopened_marker["job_id"], json!(job_id));
                                assert_eq!(reopened_marker["session_id"], json!(reviewer));

                                // Cleanup resumes only the instance recorded
                                // above. The damaged job is allowed to end
                                // `failed`/`cancelled`: deleting its state is a
                                // real control-directory loss, and success is
                                // not an exit proof. Every other job keeps the
                                // normal `completed` requirement.
                                supervisor.thaw();
                                wait_for_republished_state(&fixture.data, &job_id);
                                state.release("finalization-exit");
                                let damaged_status =
                                    wait_damaged_job_terminal(&fixture.data, &job_id);
                                assert!(
                                    wait_instance_stopped(child_pid, &child_start),
                                    "the damaged Reviewer child {child_pid} is still running"
                                );
                                assert!(
                                    wait_instance_stopped(supervisor_pid, &supervisor_start),
                                    "the frozen supervisor {supervisor_pid} is still running"
                                );
                                let others: Vec<_> = jobs(&fixture.data, &router)
                                    .into_iter()
                                    .filter(|job| job.id != job_id)
                                    .collect();
                                assert!(
                                    others.iter().all(|job| job.status == "completed"),
                                    "unrelated jobs did not complete: {others:#?}"
                                );
                                assert_eq!(
                                    proc_start_ticks(sentinel.0.id()).as_deref(),
                                    Some(sentinel_start.as_str()),
                                    "the sentinel process was disturbed by cleanup"
                                );
                                sentinel.assert_alive();
                                let _ = reopened.tick(&router).await.unwrap();
                                let final_events = events(&reopened_store, &task_id);
                                assert!(
                                    !final_events
                                        .iter()
                                        .any(|event| event.event_type == EventType::TaskCompleted),
                                    "the stopped damaged job completed the task"
                                );
                                assert_eq!(
                                    reopened_store.get_task(&task_id).unwrap().unwrap().status,
                                    Status::Blocked
                                );
                                completion_gate = json!({
                                    "mode": delivery.label(),
                                    "job_id": job_id,
                                    "session_id": reviewer,
                                    "parent_session_id": router,
                                    "job_status_at_reports": "running",
                                    "reports_durable_before_damage": true,
                                    "supervisor_frozen_pid": supervisor_pid,
                                    "state_removed": true,
                                    "state_still_absent_after_tick": true,
                                    "launch_bytes_unchanged": true,
                                    "blocked_reason": blocked_reason,
                                    "reopened_status": "BLOCKED",
                                    "reopened_phase": "FINALIZING",
                                    "job_directories_unchanged": true,
                                    "launch_operations_unchanged": true,
                                    "resume_markers_unchanged": true,
                                    "job_status_after_cleanup": damaged_status,
                                    "child_stopped": true,
                                    "supervisor_stopped": true,
                                    "other_jobs_completed": true,
                                    "task_completed_events": 0,
                                    "sentinel_alive_after_cleanup": true,
                                });
                            }
                        } else {
                            wait_idle(&fixture.data, &router, jobs_before + 1).await;
                        }
                    } else {
                        // The pre-delivery FINAL_DELIVERY review of the changed
                        // content already covers the delivered OIDs; a second
                        // review would be a duplicate.
                        assert_eq!(
                            jobs(&fixture.data, &router).len(),
                            jobs_before,
                            "changed content must reuse its final-delivery review"
                        );
                    }
                    // The damaged-state scenario ends blocked; its shared
                    // assertions already ran above.
                    if delivery != DeliveryMode::GateMissingState {
                        let _ = active_orchestrator.tick(&router).await.unwrap();
                        let report = active_orchestrator.tick(&router).await.unwrap();
                        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
                        let task = active_store.get_task(&task_id).unwrap().unwrap();
                        assert_eq!(task.phase, Phase::Finalizing);
                        assert_eq!(task.status, Status::Completed);
                        assert_eq!(
                            events(&active_store, &task_id)
                                .iter()
                                .filter(|event| event.event_type == EventType::TaskCompleted)
                                .count(),
                            1,
                            "task completion must be recorded exactly once"
                        );
                        assert!(!events(&active_store, &task_id)
                            .iter()
                            .any(|event| event.event_type == EventType::CleanupCompleted));
                        assert_eq!(git(&fixture.repo, &["rev-parse", "develop"]), integrated);
                        // The delivered commit keeps its implementation-review
                        // lineage; nothing was rewritten to satisfy the gate.
                        assert_eq!(
                            task.repositories[0].reviewed_commit.as_deref(),
                            Some(second_commit.as_str())
                        );

                        // Extra ticks and a reopened store do not duplicate the
                        // completion or replay the delivery.
                        let _ = active_orchestrator.tick(&router).await.unwrap();
                        let reopened_context = open(&fixture.data);
                        let reopened_store =
                            TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
                        let reopened = Orchestrator::new(
                            reopened_context,
                            Fixture::config_with_delivery(true),
                            provider.clone(),
                            "bypass".into(),
                            Some(executable.clone()),
                        );
                        let reopened_report = reopened.tick(&router).await.unwrap();
                        assert_eq!(
                            reopened_report.blocked,
                            Vec::<String>::new(),
                            "{reopened_report:?}"
                        );
                        assert_eq!(
                            reopened_store.get_task(&task_id).unwrap().unwrap().status,
                            Status::Completed
                        );
                        assert_eq!(
                            events(&reopened_store, &task_id)
                                .iter()
                                .filter(|event| event.event_type == EventType::TaskCompleted)
                                .count(),
                            1
                        );
                    }
                }
            }
        }
    }

    // The real Reviewer evidence tools ran through the child tool pipeline:
    // the coder transcript was readable and the unrelated session was denied.
    let all_results: Vec<String> = state
        .requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| {
            request["tool_results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|result| result.as_str().map(str::to_owned))
        .collect();
    assert!(
        all_results
            .iter()
            .any(|result| result.contains("not part of this task's Coder session tree")),
        "the Reviewer was not denied the unrelated session: {all_results:#?}"
    );
    assert!(
        all_results
            .iter()
            .any(|result| result.contains(&coder) && result.contains("\"messages\"")),
        "the Reviewer could not read the Coder transcript: {all_results:#?}"
    );

    let conversation = |session: &str| -> Value {
        context
            .db
            .with_conn(|conn| {
                deepseek_code_core::db::conversation_repo::get_conversation(conn, session)
            })
            .unwrap()
            .unwrap()
            .map(|row| {
                json!({
                    "workspace_path": row.workspace_path,
                    "model": row.model,
                    "provider": row.provider
                })
            })
            .unwrap_or(Value::Null)
    };
    let final_task = store.get_task(&task_id).unwrap().unwrap();
    let evidence = json!({
        "blocked": false,
        "delivery": delivery.label(),
        "task_id": task_id,
        "router_session": router,
        "coder_session": coder,
        "reviewer_session": reviewer,
        "first_commit": first_commit,
        "second_commit": second_commit,
        "develop_before": develop_before,
        "integrated_commit": integrated,
        "completion_gate": completion_gate,
        "integration_status": final_task.repositories[0].integration_status.as_str(),
        "integration_report": integration_report.body,
        "final_review_stage": reports(&store, &task_id)
            .into_iter()
            .filter(|report| {
                report.kind == deepseek_code_core::super_agent::ReportKind::Review
                    && report.body["stage"] == "FINAL_DELIVERY"
            })
            .last()
            .map(|report| report.body["stage"].clone())
            .unwrap_or(Value::Null),
        "coder": conversation(&coder),
        "reviewer": conversation(&reviewer),
        "jobs": jobs(&fixture.data, &router),
        "operations": operation_prompts(&context, &task_id),
        "reports": reports(&store, &task_id).into_iter().map(|report| json!({
            "report_id": report.report_id,
            "kind": report.kind.as_str(),
            "round": report.round,
            "created_at": report.created_at
        })).collect::<Vec<_>>(),
        "events": events(&store, &task_id).into_iter().map(|event| json!({
            "sequence": event.sequence,
            "type": event.event_type.as_str(),
            "round": event.review_round,
            "payload": event.payload
        })).collect::<Vec<_>>(),
        "phase": final_task.phase.as_str(),
        "status": final_task.status.as_str(),
        "reviewer_requests": state.requests.lock().unwrap().clone()
    });
    server.abort();
    (fixture, context, task_id, state, evidence)
}

fn write_evidence(name: &str, evidence: &Value) {
    let Ok(directory) = std::env::var("SUPER_AGENT_EVIDENCE_DIR") else {
        println!(
            "{name}: {}",
            serde_json::to_string_pretty(evidence).unwrap()
        );
        return;
    };
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        Path::new(&directory).join(format!("{name}.json")),
        serde_json::to_vec_pretty(evidence).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_reviewer_rejects_then_the_same_sessions_complete_a_new_round() {
    let (_fixture, _context, task_id, _state, evidence) = run_review_loop(false).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["blocked"], false);
    write_evidence("round-10-real-review-loop", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_reviewer_leaves_a_blocker_and_never_resumes_the_coder() {
    let (_fixture, _context, task_id, _state, evidence) = run_review_loop(true).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["blocked"], true);
    write_evidence("round-10-blocked-review", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_repository_delivery_moves_the_target_through_the_same_coder() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::Cas).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("cas"));
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    // All final-delivery evidence exists for changed content, so the final
    // reopen observes completion; the flow asserted RUNNING before that tick.
    assert_eq!(evidence["status"], json!("COMPLETED"));
    assert_eq!(evidence["integration_status"], json!("COMPLETED"));
    write_evidence("round-16-delivery-cas", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checked_out_target_is_delivered_in_place_through_the_same_coder() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::CheckedOut).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("checked-out"));
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    assert_eq!(evidence["integration_status"], json!("COMPLETED"));
    write_evidence("round-16-delivery-checked-out", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_delivery_command_and_a_false_claim_never_mark_delivered() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::Failed).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("failed"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-16-delivery-failed", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_released_plan_cannot_dispatch_under_another_owner() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::Released).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("released"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-16-delivery-released", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_delivery_bash_leaves_the_target_untouched() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::Denied).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("denied"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-17-delivery-denied", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reopened_host_reconciles_a_delivered_target_without_replaying() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::Interrupted).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("interrupted"));
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    // The recovered delivery has all final-delivery evidence, so the final
    // reopen tick completes the task.
    assert_eq!(evidence["status"], json!("COMPLETED"));
    assert_eq!(evidence["integration_status"], json!("COMPLETED"));
    write_evidence("round-17-delivery-interrupted", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_receipt_does_not_launch_after_the_target_moved() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::StaleReceipt).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("stale-receipt"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-17-delivery-stale-receipt", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawn_crash_adopts_the_running_delivery_child() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::SpawnCrash).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("spawn-crash"));
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    // The adopted child's delivery has all final-delivery evidence, so the
    // final reopen tick completes the task.
    assert_eq!(evidence["status"], json!("COMPLETED"));
    assert_eq!(evidence["integration_status"], json!("COMPLETED"));
    write_evidence("round-17-delivery-spawn-crash", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_parent_mode_denies_the_delivery_bash() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::ParentDenied).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("parent-denied"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-19-delivery-parent-denied", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_contradictory_delivery_operation_is_rejected_before_adoption() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::RecoveryMismatch).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("recovery-mismatch"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-19-delivery-recovery-mismatch", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bound_receipt_swapped_to_another_job_is_rejected() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::BoundJobSwap).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("bound-job-swap"));
    assert_eq!(evidence["phase"], json!("DELIVERY_PENDING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    assert_eq!(evidence["integration_status"], json!("PREPARED"));
    write_evidence("round-34-delivery-bound-job-swap", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_content_identical_delivery_reaches_task_completion() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::CompleteIdentical).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("complete-identical"));
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    assert_eq!(evidence["status"], json!("COMPLETED"));
    write_evidence("round-37-content-identical-completion", &evidence);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_content_delivery_reaches_task_completion() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::CompleteChanged).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("complete-changed"));
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    assert_eq!(evidence["status"], json!("COMPLETED"));
    write_evidence("round-37-changed-content-completion", &evidence);
}

/// Real CLI negative: both `FINAL_DELIVERY` reports are durable and the real
/// Reviewer child is still running when only its job `state.json` is removed.
/// No `TASK_COMPLETED` may be recorded, the block reason must name that exact
/// job, and a reopened host must stay blocked without a replacement Reviewer.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completion_review_with_destroyed_state_never_completes() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::GateMissingState).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("gate-missing-state"));
    assert!(
        !evidence["completion_gate"].is_null(),
        "the held completion barrier never ran: {evidence:#?}"
    );
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    assert_eq!(evidence["status"], json!("BLOCKED"));
    let job_id = evidence["completion_gate"]["job_id"]
        .as_str()
        .expect("the damaged job identity");
    let reason = evidence["completion_gate"]["blocked_reason"]
        .as_str()
        .expect("the blocked reason");
    assert!(reason.contains(job_id), "{reason}");
    assert_eq!(evidence["completion_gate"]["state_removed"], json!(true));
    assert_eq!(
        evidence["completion_gate"]["job_directories_unchanged"],
        json!(true)
    );
    assert_eq!(
        evidence["completion_gate"]["launch_operations_unchanged"],
        json!(true)
    );
    assert_eq!(
        evidence["completion_gate"]["resume_markers_unchanged"],
        json!(true)
    );
    assert_eq!(
        evidence["completion_gate"]["sentinel_alive_after_cleanup"],
        json!(true)
    );
    let completed = evidence["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "TASK_COMPLETED")
        .count();
    assert_eq!(completed, 0, "a damaged job completed the task");
    write_evidence("round-45-completion-destroyed-state", &evidence);
}

/// Real CLI positive control: both reports are durable while the real Reviewer
/// child is still running. Completion waits for its actual exit, and the
/// `TASK_COMPLETED` event is recorded exactly once after it stops.
///
/// Both scenarios share one held-gate fixture whose damage/sentinel half is
/// Linux-only (`/proc` start identity and `kill -STOP/CONT`). The pair is
/// therefore limited to the same platform; macOS and Windows are not claimed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_completion_review_waits_then_completes_after_it_stops() {
    let (_fixture, _context, task_id, _state, evidence) =
        run_review_loop_with(false, DeliveryMode::GateWaitsForExit).await;
    assert!(!task_id.is_empty());
    assert_eq!(evidence["delivery"], json!("gate-waits-for-exit"));
    assert!(
        !evidence["completion_gate"].is_null(),
        "the held completion barrier never ran: {evidence:#?}"
    );
    assert_eq!(evidence["phase"], json!("FINALIZING"));
    assert_eq!(evidence["status"], json!("COMPLETED"));
    assert_eq!(
        evidence["completion_gate"]["uncompleted_while_active"],
        json!(true)
    );
    assert_eq!(evidence["completion_gate"]["released"], json!(true));
    assert_eq!(
        evidence["completion_gate"]["job_status_after_exit"],
        json!("completed")
    );
    let completed = evidence["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "TASK_COMPLETED")
        .count();
    assert_eq!(completed, 1, "completion must be recorded exactly once");
    write_evidence("round-45-completion-live-review", &evidence);
}

/// Actual delivery to two independent real Git repositories through real
/// `dcl` children against the local mock model. Both repositories are seeded
/// at the host-approved delivery boundary; every delivery is a real child Bash
/// command, and the two target branches are delivered one after the other.
#[cfg(unix)]
mod two_repository_cli {
    use super::*;

    struct CliRepo {
        repository_id: String,
        root: PathBuf,
        worktree: PathBuf,
        checkout: Option<PathBuf>,
        branch: String,
        base: String,
        reviewed: String,
    }

    struct TwoRepoCliFixture {
        _temp: tempfile::TempDir,
        data: PathBuf,
        context: ServiceContext,
        router: String,
        task_id: String,
        coder: String,
        repos: Vec<CliRepo>,
        /// Fixture-owned file each real delivery Bash command appends to.
        marker: PathBuf,
    }

    #[derive(Default)]
    struct TwoRepoMockState {
        requests: Mutex<Vec<Value>>,
        released: Mutex<HashSet<String>>,
        turns: Mutex<std::collections::HashMap<String, usize>>,
        /// Delivery turns whose tool result came back, one entry per marker.
        runs: Mutex<Vec<String>>,
        /// Fixture-owned execution log appended by the real Bash command.
        marker: PathBuf,
        /// Hold the second repository's turn after its command ran.
        hold_b: bool,
        /// Make the second repository's real Bash command fail.
        fail_b: bool,
    }

    impl TwoRepoMockState {
        fn release(&self, key: &str) {
            self.released.lock().unwrap().insert(key.to_owned());
        }

        fn released(&self, key: &str) -> bool {
            self.released.lock().unwrap().contains(key)
        }

        fn next_step(&self, key: String) -> usize {
            let mut turns = self.turns.lock().unwrap();
            let step = turns.entry(key).or_insert(0);
            let current = *step;
            *step += 1;
            current
        }

        fn note_run(&self, marker: &str) {
            self.runs.lock().unwrap().push(marker.to_owned());
        }

        fn record(&self, prompt: &str, last: Option<&str>) {
            self.requests.lock().unwrap().push(json!({
                "role": "coder",
                "prompt": prompt,
                "last_tool_result": last,
            }));
        }
    }

    async fn two_repo_handler(
        state: Arc<TwoRepoMockState>,
        body: Value,
    ) -> ([(&'static str, &'static str); 1], String) {
        let prompt = current_instruction(&body);
        let last = last_tool_result(&body);
        state.record(&prompt, last.as_deref());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&prompt, &mut hasher);
        let step = state.next_step(format!("{:x}", std::hash::Hasher::finish(&hasher)));
        let second = prompt.contains("repository repo-2");
        let response = if prompt.contains("deliver the host-verified integration") {
            if step >= 1 {
                // The Bash tool result came back for this delivery turn.
                state.note_run(if second { "repo-2" } else { "repo-1" });
            }
            match step {
                0 => {
                    // The real child Bash command itself records its execution in
                    // fixture-owned storage, so a replay cannot hide behind the
                    // model request count.
                    let marker = state.marker.display();
                    let command = if second && state.fail_b {
                        format!(
                            "printf '%s\\n' 'repo-2' >> '{marker}'\nprintf 'delivery failed for repo-2\\n' >&2; false"
                        )
                    } else if second {
                        format!(
                            "printf '%s\\n' 'repo-2' >> '{marker}'\npwd && {}",
                            delivery_command_from_prompt(&prompt)
                        )
                    } else {
                        format!(
                            "printf '%s\\n' 'repo-1' >> '{marker}'\npwd && {}",
                            delivery_command_from_prompt(&prompt)
                        )
                    };
                    stream(
                        tool_call(
                            "delivery-bash",
                            "Bash",
                            json!({"shell":"sh","command":command,"timeout":60000}),
                        ),
                        "tool_calls",
                    )
                }
                _ => {
                    if second && state.hold_b && !state.fail_b {
                        for _ in 0..2400 {
                            if state.released("delivery-hold-b") {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        assert!(
                            state.released("delivery-hold-b"),
                            "the mock waited too long to release the second delivery"
                        );
                    }
                    stream(json!({"content":"delivery complete"}), "stop")
                }
            }
        } else {
            stream(json!({"content":"unused"}), "stop")
        };
        ([("content-type", "text/event-stream")], response)
    }

    impl TwoRepoCliFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let marker = temp.path().join("delivery-executions.log");
            let data = temp.path().join("data");
            std::fs::create_dir_all(&data).unwrap();
            let context = ServiceContext::open(
                RuntimePaths::new(data.clone()).unwrap(),
                Arc::new(EnvironmentCredentials),
            )
            .unwrap();
            let router = SessionStore::new(context.db.clone())
                .create(temp.path(), "router-model", "fixture")
                .unwrap();
            role::set(&context.db, &router, Role::Router).unwrap();
            let repos = vec![
                Self::repo(temp.path(), "repo one", "develop", "repo-1", 0, false),
                // The second target is checked out in a linked worktree whose
                // path contains spaces, and uses a different branch.
                Self::repo(temp.path(), "repo two", "integration", "repo-2", 1, true),
            ];
            let spec = TaskSpec {
                title: "Two repository real delivery".into(),
                workspace: temp.path().to_path_buf(),
                targets: repos
                    .iter()
                    .map(|repo| repo.root.to_string_lossy().into_owned())
                    .collect(),
                goal: "Deliver both repositories through the same Coder".into(),
                requirements: vec!["Keep both repositories valid".into()],
                acceptance_criteria: vec![deepseek_code_core::super_agent::AcceptanceCriterion {
                    id: "AC-001".into(),
                    description: "Both targets advance to the approved commit".into(),
                }],
                constraints: vec![],
                out_of_scope: vec![],
                target_branch: Some("develop".into()),
                repository_target_branches: repos
                    .iter()
                    .map(|repo| {
                        (
                            repo.root.to_string_lossy().into_owned(),
                            repo.branch.clone(),
                        )
                    })
                    .collect(),
            };
            let store = TaskStore::new(context.db.clone(), data.clone());
            let task = store.create_task(&router, spec, "create").unwrap();
            for (index, repo) in repos.iter().enumerate() {
                let common = PathBuf::from(git(
                    &repo.root,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                ))
                .canonicalize()
                .unwrap();
                store
                    .save_repo(&deepseek_code_core::super_agent::RepoTarget {
                        task_id: task.task_id.clone(),
                        repository_id: repo.repository_id.clone(),
                        repo_root: repo.root.clone(),
                        git_common_dir: common,
                        target_path: repo.root.clone(),
                        target_branch: repo.branch.clone(),
                        task_branch: format!("agent/{index}"),
                        worktree_path: repo.worktree.clone(),
                        working_path: repo.worktree.clone(),
                        base_commit: repo.base.clone(),
                        submitted_commit: Some(repo.reviewed.clone()),
                        reviewed_commit: Some(repo.reviewed.clone()),
                        integrated_commit: Some(repo.reviewed.clone()),
                        integration_status:
                            deepseek_code_core::super_agent::IntegrationStatus::Prepared,
                        verification_status:
                            deepseek_code_core::super_agent::VerificationStatus::Passed,
                        source_state: vec![],
                    })
                    .unwrap();
            }
            Self::record_evidence(&store, &task, &repos);
            let workspace = data
                .join("super-agent")
                .join(&task.task_id)
                .join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            for repo in &repos {
                let link = workspace.join(&repo.repository_id);
                if !link.exists() {
                    std::os::unix::fs::symlink(&repo.worktree, &link).unwrap();
                }
            }
            let coder = SessionStore::new(context.db.clone())
                .create(&workspace, "router-model", "fixture")
                .unwrap();
            // The Coder is a Router child, as a real launch would have made it.
            let now = chrono::Utc::now().to_rfc3339();
            context
                .save_setting(
                    &format!(
                        "{}{}",
                        deepseek_code_core::native_engine::child_session::OWNER_KEY_PREFIX,
                        coder
                    ),
                    &deepseek_code_core::native_engine::child_session::ChildOwnership {
                        parent_session_id: router.clone(),
                        agent_type: "general".into(),
                        description: "super agent member".into(),
                        mode: "background".into(),
                        status: "completed".into(),
                        job_id: Some("fixture-coder-job".into()),
                        created_at: now.clone(),
                        updated_at: now,
                        last_error: None,
                    },
                )
                .unwrap();
            // The runtime plan gate requires a durable approved plan for this
            // exact spec version and permission policy before any Coder Bash.
            let approval = store
                .request_approval(
                    "fixture-plan-approval",
                    &task.task_id,
                    deepseek_code_core::super_agent::store::ApprovalDraft {
                        source_session_id: coder.clone(),
                        parent_session_id: router.clone(),
                        request_type: deepseek_code_core::super_agent::ApprovalType::Plan,
                        request: json!({
                            "task_id": task.task_id,
                            "spec_version": task.spec_version,
                            "round": task.review_round,
                        }),
                        task_spec_version: task.spec_version,
                        permission_policy_version: task.permission_policy_version,
                    },
                )
                .unwrap();
            store
                .resolve_approval(
                    "fixture-plan-approval-resolved",
                    &approval.approval_id,
                    deepseek_code_core::super_agent::ApprovalStatus::Approved,
                    Some("fixture approved the plan"),
                )
                .unwrap();
            store
                .set_sessions(&task.task_id, Some(&coder), None)
                .unwrap();
            assert_eq!(
                store.get_task(&task.task_id).unwrap().unwrap().phase,
                Phase::DeliveryPending
            );
            Self {
                _temp: temp,
                data,
                context,
                router,
                task_id: task.task_id,
                coder,
                repos,
                marker: marker.clone(),
            }
        }

        fn repo(
            project: &Path,
            name: &str,
            branch: &str,
            repository_id: &str,
            index: usize,
            checked_out: bool,
        ) -> CliRepo {
            let root = project.join(name);
            std::fs::create_dir_all(&root).unwrap();
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.email", "test@example.invalid"]);
            git(&root, &["config", "user.name", "Test"]);
            git(&root, &["config", "commit.gpgsign", "false"]);
            std::fs::write(root.join("base.txt"), "base\n").unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "base"]);
            let base = git(&root, &["rev-parse", "HEAD"]);
            git(&root, &["branch", branch, &base]);
            let common = PathBuf::from(git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ));
            let worktree = common
                .join("desktop-worktrees")
                .join(format!("task-{index}"));
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("agent/{index}"),
                    &worktree.to_string_lossy(),
                    "main",
                ],
            );
            std::fs::write(worktree.join("reviewed.txt"), "value=1\n").unwrap();
            git(&worktree, &["add", "."]);
            git(&worktree, &["commit", "-q", "-m", "reviewed"]);
            let reviewed = git(&worktree, &["rev-parse", "HEAD"]);
            let checkout = if checked_out {
                let path = common.join(format!("target checkout {index}"));
                git(
                    &root,
                    &["worktree", "add", "-q", &path.to_string_lossy(), branch],
                );
                Some(path.canonicalize().unwrap())
            } else {
                None
            };
            CliRepo {
                repository_id: repository_id.into(),
                root: root.canonicalize().unwrap(),
                worktree: worktree.canonicalize().unwrap(),
                checkout,
                branch: branch.into(),
                base,
                reviewed,
            }
        }

        fn record_evidence(
            store: &TaskStore,
            task: &deepseek_code_core::super_agent::TaskRecord,
            repos: &[CliRepo],
        ) {
            let reviewed_versions: Vec<Value> = repos
                .iter()
                .map(|repo| {
                    json!({
                        "repository_id": repo.repository_id,
                        "commit": repo.reviewed,
                    })
                })
                .collect();
            let review = store
                .add_report(
                    &format!("{}-review", task.task_id),
                    &task.task_id,
                    deepseek_code_core::super_agent::ReportKind::Review,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "verdict": "APPROVED",
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "stage": "IMPLEMENTATION",
                        "reviewed_versions": reviewed_versions,
                        "acceptance_results": [],
                    }),
                )
                .unwrap();
            let harness = store
                .add_report(
                    &format!("{}-harness", task.task_id),
                    &task.task_id,
                    deepseek_code_core::super_agent::ReportKind::Harness,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "stage": "IMPLEMENTATION",
                        "findings": [],
                    }),
                )
                .unwrap();
            let entries: Vec<Value> = repos
                .iter()
                .map(|repo| {
                    let tree = git(
                        &repo.root,
                        &["rev-parse", &format!("{}^{{tree}}", repo.reviewed)],
                    );
                    json!({
                        "repository_id": repo.repository_id,
                        "target_branch": repo.branch,
                        "target_ref": format!("refs/heads/{}", repo.branch),
                        "baseline_commit": repo.base,
                        "reviewed_commit": repo.reviewed,
                        "integrated_commit": repo.reviewed,
                        "integrated_tree": tree,
                        "tree_changed": false,
                        "content_review_reused": true,
                        "reason": "fixture preparation",
                    })
                })
                .collect();
            store
                .db()
                .with_conn(|conn| {
                    conn.execute(
                        "UPDATE sa_tasks SET phase='REVIEWING', status='RUNNING' WHERE task_id=?1",
                        [&task.task_id],
                    )
                })
                .unwrap()
                .unwrap();
            let policy = deepseek_code_core::super_agent::state::StatePolicy::default();
            store
                .apply_event(
                    &task.task_id,
                    deepseek_code_core::super_agent::NewEvent {
                        session_id: None,
                        run_id: None,
                        event_type: EventType::ReviewCompleted,
                        causation_id: None,
                        operation_id: format!("{}-decision", task.task_id),
                        payload: json!({
                            "verdict": "APPROVED",
                            "stage": "IMPLEMENTATION",
                            "review_report_id": review.report_id,
                            "harness_report_id": harness.report_id,
                            "harness_blocked": false,
                        }),
                    },
                    &policy,
                )
                .unwrap();
            store
                .apply_event(
                    &task.task_id,
                    deepseek_code_core::super_agent::NewEvent {
                        session_id: None,
                        run_id: None,
                        event_type: EventType::IntegrationPrepared,
                        causation_id: None,
                        operation_id: format!("{}-prepared", task.task_id),
                        payload: json!({"repositories": entries, "tree_changed": false}),
                    },
                    &policy,
                )
                .unwrap();
        }

        fn store(&self) -> TaskStore {
            TaskStore::new(self.context.db.clone(), self.data.clone())
        }

        fn task(&self) -> deepseek_code_core::super_agent::TaskRecord {
            self.store().get_task(&self.task_id).unwrap().unwrap()
        }

        fn prepare_plan(&self) -> deepseek_code_core::super_agent::delivery::DeliveryPlan {
            match deepseek_code_core::super_agent::delivery::prepare_delivery(
                &self.store(),
                &self.task(),
            )
            .unwrap()
            {
                deepseek_code_core::super_agent::delivery::DeliveryPreflight::Planned(plan) => plan,
                other => panic!("expected a prepared delivery plan, got {other:?}"),
            }
        }

        fn config() -> deepseek_code_core::super_agent::config::SuperAgentConfig {
            let mut config =
                deepseek_code_core::super_agent::config::parse(&json!({"agents":{}})).unwrap();
            config.delivery.enabled = true;
            config
        }

        fn provider(
            address: std::net::SocketAddr,
        ) -> deepseek_code_core::native_engine::provider_manager::ResolvedProvider {
            deepseek_code_core::native_engine::provider_manager::ResolvedProvider {
                provider: serde_json::from_value(json!({
                    "id":"fixture","name":"Fixture","base_url":format!("http://{address}"),
                    "api_key":"fixture-key","api_format":"openai","models":[],"enabled":true
                }))
                .unwrap(),
                model: serde_json::from_value(json!({
                    "id":"router-model","name":"router-model","enabled":true,
                    "supports_vision":false,"supports_web_search":false
                }))
                .unwrap(),
            }
        }

        async fn serve(
            &self,
            hold_b: bool,
            fail_b: bool,
        ) -> (Arc<TwoRepoMockState>, Orchestrator, std::net::SocketAddr) {
            let state = Arc::new(TwoRepoMockState {
                hold_b,
                fail_b,
                marker: self.marker.clone(),
                ..Default::default()
            });
            let handler_state = state.clone();
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                    let state = handler_state.clone();
                    async move { two_repo_handler(state, body).await }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            std::fs::write(
                self.data.join("config.json"),
                serde_json::to_vec(&json!({
                    "providers":[{
                        "id":"fixture","name":"Fixture","provider_type":"openai","api_key":"fixture-key",
                        "base_url":format!("http://{address}"),
                        "models":[{
                            "id":"router-model","name":"router-model","enabled":true,"max_tokens":8192,
                            "supports_tools":true,"supports_streaming":true,
                            "context_window":128000,"cost_per_1k_input":null,"cost_per_1k_output":null
                        }],
                        "enabled":true,"is_default":true,"settings":{},
                        "supports_web_search":false,"web_search_strategy":null,
                        "web_search_tested_at":null,"web_search_test_reason":null
                    }],
                    "default_provider_id":"fixture"
                }))
                .unwrap(),
            )
            .unwrap();
            let orchestrator = self.reopen_with(&self.context, address);
            (state, orchestrator, address)
        }

        fn reopen_with(
            &self,
            context: &ServiceContext,
            address: std::net::SocketAddr,
        ) -> Orchestrator {
            Orchestrator::new(
                context.clone(),
                Self::config(),
                Self::provider(address),
                "bypass".into(),
                Some(PathBuf::from(env!("CARGO_BIN_EXE_dcl"))),
            )
        }

        fn target(&self, index: usize) -> String {
            let repo = &self.repos[index];
            git(
                &repo.root,
                &["rev-parse", &format!("refs/heads/{}", repo.branch)],
            )
        }

        fn observation(
            &self,
            store: &TaskStore,
            plan: &deepseek_code_core::super_agent::delivery::DeliveryPlan,
            index: usize,
        ) -> bool {
            store
                .repository_delivery_observation(
                    &self.task_id,
                    &plan.operation_id,
                    &self.repos[index].repository_id,
                )
                .unwrap()
                .is_some()
        }

        fn completed(&self, store: &TaskStore) -> bool {
            store
                .events(&self.task_id)
                .unwrap()
                .iter()
                .any(|event| event.event_type == EventType::IntegrationCompleted)
        }

        fn integration_completed_count(&self, store: &TaskStore) -> usize {
            store
                .events(&self.task_id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == EventType::IntegrationCompleted)
                .count()
        }

        fn executions(&self) -> Vec<String> {
            std::fs::read_to_string(&self.marker)
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        }

        fn assert_executed_once(&self, label: &str) {
            let lines = self.executions();
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.as_str() == "repo-1")
                    .count(),
                1,
                "{label}: repository repo-1 ran {} times: {lines:?}",
                lines
                    .iter()
                    .filter(|line| line.as_str() == "repo-1")
                    .count()
            );
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.as_str() == "repo-2")
                    .count(),
                1,
                "{label}: repository repo-2 ran {} times: {lines:?}",
                lines
                    .iter()
                    .filter(|line| line.as_str() == "repo-2")
                    .count()
            );
            assert_eq!(lines.len(), 2, "{label}: unexpected executions: {lines:?}");
        }

        /// Durable tool-call rows for the delivery Bash invocations. The
        /// provider reuses the raw `delivery-bash` id for both turns of the
        /// same Coder session, so the stored row IDs must still be distinct.
        fn delivery_tool_call_ids(&self) -> Vec<String> {
            let marker = self.marker.to_string_lossy().into_owned();
            self.context
                .db
                .with_conn(|conn| -> anyhow::Result<Vec<String>> {
                    let messages =
                        deepseek_code_core::db::message_repo::get_messages_by_conversation(
                            conn,
                            &self.coder,
                        )?;
                    let mut ids = Vec::new();
                    for message in messages {
                        for call in
                            deepseek_code_core::db::message_repo::get_tool_calls_for_message(
                                conn,
                                &message.id,
                            )?
                        {
                            if call.name == "Bash"
                                && call
                                    .input
                                    .as_deref()
                                    .is_some_and(|input| input.contains(&marker))
                            {
                                ids.push(call.id);
                            }
                        }
                    }
                    Ok(ids)
                })
                .unwrap()
                .unwrap()
        }

        fn assert_distinct_delivery_invocations(&self) {
            let ids = self.delivery_tool_call_ids();
            assert_eq!(
                ids.len(),
                2,
                "expected one durable invocation per repository: {ids:?}"
            );
            let unique: HashSet<&String> = ids.iter().collect();
            assert_eq!(
                unique.len(),
                2,
                "the durable invocation IDs were reused: {ids:?}"
            );
        }

        fn delivery_runs(&self, state: &TwoRepoMockState, marker: &str) -> usize {
            state
                .runs
                .lock()
                .unwrap()
                .iter()
                .filter(|run| run.as_str() == marker)
                .count()
        }

        async fn wait_for_delivery_run(&self, state: &TwoRepoMockState, marker: &str) {
            for _ in 0..2400 {
                if self.delivery_runs(state, marker) > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the delivery command for {marker} did not run");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_git_repositories_are_delivered_sequentially_by_the_same_coder() {
        let fixture = TwoRepoCliFixture::new();
        let plan = fixture.prepare_plan();
        let (state, orchestrator, _address) = fixture.serve(true, false).await;

        // Repository A: real child, real command, then observation only.
        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        wait_idle(&fixture.data, &fixture.router, 1).await;
        let second = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(second.blocked, Vec::<String>::new(), "{second:?}");
        assert_eq!(fixture.target(0), fixture.repos[0].reviewed);
        assert_eq!(
            fixture
                .executions()
                .iter()
                .filter(|line| line.as_str() == "repo-1")
                .count(),
            1,
            "repository A's real command did not run exactly once: {:?}",
            fixture.executions()
        );
        assert!(
            !fixture.executions().iter().any(|line| line == "repo-2"),
            "repository B ran before its turn"
        );
        assert!(
            fixture.observation(&fixture.store(), &plan, 0),
            "repository A was not observed after its real delivery"
        );
        assert!(
            !fixture.observation(&fixture.store(), &plan, 1),
            "repository B was observed before its child ran"
        );
        assert!(!fixture.completed(&fixture.store()));
        assert_eq!(
            fixture.task().repositories[1].integration_status,
            deepseek_code_core::super_agent::IntegrationStatus::Prepared
        );

        // Repository B: the child moves its target, then the model is held.
        let third = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(third.blocked, Vec::<String>::new(), "{third:?}");
        fixture.wait_for_delivery_run(&state, "repo-2").await;
        assert_eq!(fixture.target(1), fixture.repos[1].reviewed);
        let while_held = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(while_held.blocked, Vec::<String>::new(), "{while_held:?}");
        assert!(
            !fixture.completed(&fixture.store()),
            "a running second child must not complete the task"
        );
        assert_eq!(jobs(&fixture.data, &fixture.router).len(), 2);

        state.release("delivery-hold-b");
        wait_idle(&fixture.data, &fixture.router, 2).await;
        let final_report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            final_report.blocked,
            Vec::<String>::new(),
            "{final_report:?}"
        );

        let task = fixture.task();
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(task.status, Status::Running);
        assert!(fixture.completed(&fixture.store()));
        for repo in &task.repositories {
            assert_eq!(
                repo.integration_status,
                deepseek_code_core::super_agent::IntegrationStatus::Completed
            );
        }
        assert_eq!(fixture.target(0), fixture.repos[0].reviewed);
        assert_eq!(fixture.target(1), fixture.repos[1].reviewed);
        let checkout = fixture.repos[1].checkout.as_ref().unwrap();
        assert_eq!(
            git(checkout, &["rev-parse", "HEAD"]),
            fixture.repos[1].reviewed
        );
        assert_eq!(git(checkout, &["status", "--porcelain"]), "");
        assert_eq!(fixture.delivery_runs(&state, "repo-1"), 1);
        assert_eq!(fixture.delivery_runs(&state, "repo-2"), 1);
        fixture.assert_executed_once("successful two-repository delivery");
        fixture.assert_distinct_delivery_invocations();
        let jobs = jobs(&fixture.data, &fixture.router);
        assert_eq!(jobs.len(), 2, "each repository must have its own job");
        assert!(jobs.iter().all(|job| job.session_id == fixture.coder));
        assert_ne!(jobs[0].id, jobs[1].id);
        assert!(!fixture
            .store()
            .events(&fixture.task_id)
            .unwrap()
            .iter()
            .any(|event| event.event_type == EventType::TaskCompleted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_second_repository_is_adopted_after_the_host_reopens() {
        let fixture = TwoRepoCliFixture::new();
        let plan = fixture.prepare_plan();
        let (state, orchestrator, address) = fixture.serve(true, false).await;

        // Repository A is delivered by its own real child.
        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        wait_idle(&fixture.data, &fixture.router, 1).await;
        let second = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(second.blocked, Vec::<String>::new(), "{second:?}");
        assert!(fixture.observation(&fixture.store(), &plan, 0));
        assert_eq!(fixture.target(0), fixture.repos[0].reviewed);

        // B's real child executes its Git command, then is held at the model
        // completion, so a durable write exists before any observation.
        let third = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(third.blocked, Vec::<String>::new(), "{third:?}");
        fixture.wait_for_delivery_run(&state, "repo-2").await;
        assert_eq!(fixture.target(1), fixture.repos[1].reviewed);
        let jobs_before = jobs(&fixture.data, &fixture.router);
        assert_eq!(jobs_before.len(), 2);
        let dispatch_before = fixture
            .store()
            .repository_dispatch(&fixture.task_id, &plan.operation_id, "repo-2")
            .unwrap()
            .expect("B dispatch receipt");
        let job_id_b = dispatch_before.job_id.clone().expect("B job id");
        assert_eq!(
            jobs_before.iter().filter(|job| job.id == job_id_b).count(),
            1,
            "the receipt did not name the running B job"
        );
        assert!(fixture.observation(&fixture.store(), &plan, 0));
        assert!(!fixture.observation(&fixture.store(), &plan, 1));
        assert!(!fixture.completed(&fixture.store()));
        assert_eq!(fixture.delivery_runs(&state, "repo-2"), 1);

        // Drop the host and reopen from the same persisted database while B is
        // still running. The new host must adopt the exact typed job instead of
        // launching anything or trusting a member report.
        drop(orchestrator);
        let reopened_context = ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = fixture.reopen_with(&reopened_context, address);
        let adopting = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(adopting.blocked, Vec::<String>::new(), "{adopting:?}");
        assert_eq!(
            jobs(&fixture.data, &fixture.router).len(),
            jobs_before.len(),
            "the reopened host started another child"
        );
        assert!(
            !fixture.completed(&reopened_store),
            "a running adopted child completed the task"
        );
        assert!(!fixture.observation(&reopened_store, &plan, 1));
        assert_eq!(
            reopened_store
                .repository_dispatch(&fixture.task_id, &plan.operation_id, "repo-2")
                .unwrap(),
            Some(dispatch_before.clone()),
            "the reopened host rewrote B's dispatch receipt"
        );
        assert_eq!(
            fixture.target(1),
            fixture.repos[1].reviewed,
            "the real write disappeared across the reopen"
        );

        // Release the held child: the reopened host observes the real success.
        state.release("delivery-hold-b");
        wait_idle(&fixture.data, &fixture.router, 2).await;
        let finished = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(finished.blocked, Vec::<String>::new(), "{finished:?}");

        let task = reopened_store.get_task(&fixture.task_id).unwrap().unwrap();
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(task.status, Status::Running);
        for repo in &task.repositories {
            assert_eq!(
                repo.integration_status,
                deepseek_code_core::super_agent::IntegrationStatus::Completed
            );
        }
        assert_eq!(fixture.target(0), fixture.repos[0].reviewed);
        assert_eq!(fixture.target(1), fixture.repos[1].reviewed);
        assert_eq!(fixture.integration_completed_count(&reopened_store), 1);
        assert_eq!(
            reopened_store
                .repository_dispatch(&fixture.task_id, &plan.operation_id, "repo-2")
                .unwrap(),
            Some(dispatch_before.clone()),
            "finalizing rewrote B's dispatch receipt"
        );

        // Extra ticks and another reopen emit exactly one global completion and
        // never task completion or cleanup in this increment.
        let extra = reopened.tick(&fixture.router).await.unwrap();
        // This seeded fixture has no Reviewer session, so the completion
        // finalizer blocks with its diagnosis instead of inventing evidence.
        // Delivery completion is unaffected and stays exactly one event.
        assert!(
            extra.blocked.iter().any(|reason| reason
                .contains("No Reviewer session exists to record the final completion review")),
            "{extra:?}"
        );
        assert_eq!(fixture.integration_completed_count(&reopened_store), 1);
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Blocked
        );
        let second_context = ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let second_store = TaskStore::new(second_context.db.clone(), fixture.data.clone());
        let second = fixture.reopen_with(&second_context, address);
        let second_tick = second.tick(&fixture.router).await.unwrap();
        assert_eq!(second_tick.blocked, Vec::<String>::new(), "{second_tick:?}");
        assert_eq!(
            fixture.integration_completed_count(&second_store),
            1,
            "the global completion event was duplicated"
        );
        assert!(
            !second_store
                .events(&fixture.task_id)
                .unwrap()
                .iter()
                .any(|event| matches!(
                    event.event_type,
                    EventType::TaskCompleted | EventType::CleanupCompleted
                )),
            "this increment must not complete or clean the task"
        );

        fixture.assert_executed_once("running second repository adopted after reopen");
        fixture.assert_distinct_delivery_invocations();
        assert_eq!(fixture.delivery_runs(&state, "repo-1"), 1);
        assert_eq!(fixture.delivery_runs(&state, "repo-2"), 1);
        let jobs_after = jobs(&fixture.data, &fixture.router);
        assert_eq!(jobs_after.len(), 2, "each repository must have its own job");
        assert!(jobs_after.iter().all(|job| job.session_id == fixture.coder));
        assert_ne!(jobs_after[0].id, jobs_after[1].id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_second_repository_keeps_the_first_delivery_after_reopen() {
        let fixture = TwoRepoCliFixture::new();
        let plan = fixture.prepare_plan();
        let (state, orchestrator, _address) = fixture.serve(false, true).await;

        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        wait_idle(&fixture.data, &fixture.router, 1).await;
        let second = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(second.blocked, Vec::<String>::new(), "{second:?}");
        assert!(fixture.observation(&fixture.store(), &plan, 0));

        let third = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(third.blocked, Vec::<String>::new(), "{third:?}");
        wait_idle(&fixture.data, &fixture.router, 2).await;
        let failed = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            failed.blocked.iter().any(|reason| {
                reason.contains("repo-2") && reason.contains("approved integrated commit")
            }),
            "the failing repository was not named: {failed:?}"
        );
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::DeliveryPending);
        assert_eq!(
            task.repositories[0].integration_status,
            deepseek_code_core::super_agent::IntegrationStatus::Completed
        );
        assert_eq!(
            task.repositories[1].integration_status,
            deepseek_code_core::super_agent::IntegrationStatus::Prepared
        );
        assert!(fixture.observation(&fixture.store(), &plan, 0));
        assert!(!fixture.observation(&fixture.store(), &plan, 1));
        assert!(!fixture.completed(&fixture.store()));
        assert_eq!(fixture.target(0), fixture.repos[0].reviewed);
        assert_eq!(fixture.target(1), fixture.repos[1].base);
        assert_eq!(fixture.delivery_runs(&state, "repo-1"), 1);
        fixture.assert_executed_once("failed second repository");
        fixture.assert_distinct_delivery_invocations();

        // A reopened host performs no delivery work for the blocked task: no
        // completed repository is replayed and no target is touched.
        let jobs_before = jobs(&fixture.data, &fixture.router).len();
        let reopened_context = ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = fixture.reopen_with(&reopened_context, "127.0.0.1:1".parse().unwrap());
        let after = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(after.blocked, Vec::<String>::new(), "{after:?}");
        assert_eq!(jobs(&fixture.data, &fixture.router).len(), jobs_before);
        assert!(fixture.observation(&reopened_store, &plan, 0));
        assert_eq!(fixture.target(0), fixture.repos[0].reviewed);
        assert_eq!(fixture.target(1), fixture.repos[1].base);
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Blocked
        );
        assert_eq!(fixture.delivery_runs(&state, "repo-1"), 1);
    }
}
