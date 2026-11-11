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

async fn reviewer_response(state: &MockState, request: &Value, stage: &str, step: usize) -> String {
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
            reviewer_response(&state, request.as_ref().unwrap_or(&json!({})), &stage, step).await
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

/// Rewrites the delivery launch operation under its real key. The key is not
/// derived from the edited contents, so the stored record disagrees with the
/// key it was found under.
fn tamper_delivery_operation(
    context: &ServiceContext,
    task: &deepseek_code_core::super_agent::TaskRecord,
    edit: impl FnOnce(&mut Value),
) {
    let key = format!(
        "super-agent.launch-op.{}.coder.resume.{}.{}.delivery",
        task.task_id, task.spec_version, task.review_round
    );
    let mut operation = context
        .setting::<Value>(&key)
        .unwrap()
        .expect("delivery launch operation");
    edit(&mut operation);
    context.save_setting(&key, &operation).unwrap();
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
    let orchestrator = Orchestrator::new(
        context.clone(),
        Fixture::config_with_delivery(delivery.enabled()),
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
    let orchestrator = Orchestrator::new(
        context.clone(),
        Fixture::config_with_delivery(delivery.enabled()),
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
    let develop_before = move_target(&fixture.repo, "other.txt", "target work\n");
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
    assert_ne!(
        integrated, second_commit,
        "integration did not merge the target"
    );
    assert_eq!(
        task.repositories[0].integration_status,
        deepseek_code_core::super_agent::IntegrationStatus::Prepared,
        "the host recorded a prepared integration"
    );
    assert_eq!(
        task.repositories[0].reviewed_commit.as_deref(),
        Some(second_commit.as_str())
    );
    assert_eq!(
        task.phase,
        Phase::FinalReview,
        "changed integrated content requires a final review"
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
    assert_eq!(prepared.payload["tree_changed"], json!(true));
    assert_eq!(
        prepared.payload["repositories"][0]["integrated_commit"],
        json!(integrated)
    );

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
    let orchestrator = Orchestrator::new(
        context.clone(),
        Fixture::config_with_delivery(delivery.enabled()),
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
        EventType::IntegrationCompleted | EventType::TaskCompleted | EventType::CleanupCompleted
    )));
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "develop"]),
        develop_before
    );

    // Repeated ticks after the final decision must not duplicate work.
    let settled_jobs = jobs(&fixture.data, &router).len();
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
            // A previous host persisted the receipt but never launched, then
            // the target moved. The fresh launch must not happen.
            let prepared =
                match deepseek_code_core::super_agent::delivery::prepare_delivery(&store, &task)
                    .unwrap()
                {
                    deepseek_code_core::super_agent::delivery::DeliveryPreflight::Planned(plan) => {
                        plan
                    }
                    other => panic!("expected a prepared plan, got {other:?}"),
                };
            store
                .begin_repository_dispatch(
                    &deepseek_code_core::super_agent::delivery::DeliveryDispatch {
                        task_id: task_id.clone(),
                        spec_version: 1,
                        review_round: task.review_round,
                        plan_operation_id: prepared.operation_id.clone(),
                        coder_session_id: coder.clone(),
                        purpose: "delivery".into(),
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
            store
                .begin_repository_dispatch(
                    &deepseek_code_core::super_agent::delivery::DeliveryDispatch {
                        task_id: task_id.clone(),
                        spec_version: 1,
                        review_round: task.review_round,
                        plan_operation_id: plan.operation_id.clone(),
                        coder_session_id: coder.clone(),
                        purpose: "delivery".into(),
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
                .resume_coder(
                    &task,
                    &deepseek_code_core::super_agent::launcher::coder_delivery_prompt(&task, &plan),
                    "delivery",
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
                    operation["role"] == "coder" && operation["purpose"] == "delivery"
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
            assert!(
                report.blocked.iter().any(|reason| reason.contains("moved")),
                "a moved target must block: {report:?}"
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
                    operation["role"] == "coder" && operation["purpose"] == "delivery"
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
                    tamper_delivery_operation(&reopened, &task, |operation| {
                        operation["task_id"] = json!("task-999");
                    });
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
                    let key = format!(
                        "super-agent.launch-op.{}.coder.resume.{}.{}.delivery",
                        task_id, task.spec_version, task.review_round
                    );
                    let preserved = reopened.setting::<Value>(&key).unwrap().unwrap();
                    assert_eq!(preserved["task_id"], json!("task-999"));
                    assert_eq!(preserved["handle"], Value::Null);
                    // Release the exact hostile child and let it exit.
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

            if delivery != DeliveryMode::RecoveryMismatch {
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
                let jobs_before_reopen = jobs(&fixture.data, &router).len();
                let reopen_context = open(&fixture.data);
                let reopen_store = TaskStore::new(reopen_context.db.clone(), fixture.data.clone());
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
        "integration_status": final_task.repositories[0].integration_status.as_str(),
        "integration_report": integration_report.body,
        "final_review_stage": final_review.body["stage"],
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
    assert_eq!(evidence["status"], json!("RUNNING"));
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
    assert_eq!(evidence["status"], json!("RUNNING"));
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
    assert_eq!(evidence["status"], json!("RUNNING"));
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
