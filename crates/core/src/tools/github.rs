//! Thin, typed wrapper around the `gh` CLI that is already installed and signed
//! in on this machine.
//!
//! This is deliberately not a general shell: it runs `gh` as an argument vector
//! (never through a shell), accepts only the operations below, passes written
//! bodies on stdin instead of in arguments, and returns normalized JSON instead
//! of raw stdout. Authentication stays with `gh` itself - no token, host or URL
//! is ever accepted as input.
//!
//! Mutating operations are never retried. A timeout, a cancellation or a
//! transport-level failure after submission can leave the outcome unknown, and
//! repeating the call could create a second issue, comment or merge, so the
//! result reports the ambiguity instead. A merge that gh accepts is reported as
//! accepted, and only a follow-up read that *shows* the merge is reported as
//! merged: gh exits 0 for an enqueued or auto-merge too.

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

/// The operations this tool exposes. Anything else is refused before running.
const OPERATIONS: &[&str] = &[
    "repo_view",
    "issue_list",
    "issue_view",
    "issue_create",
    "issue_comment",
    "pr_list",
    "pr_view",
    "pr_create",
    "pr_comment",
    "pr_merge",
];

const READ_OPERATIONS: &[&str] = &[
    "repo_view",
    "issue_list",
    "issue_view",
    "pr_list",
    "pr_view",
];

const WRITE_OPERATIONS: &[&str] = &[
    "issue_create",
    "issue_comment",
    "pr_create",
    "pr_comment",
    "pr_merge",
];

/// Whether an operation only reads. The permission layer treats a read here like
/// any other read-only tool and a write like any other mutation.
pub(crate) fn is_read_operation(operation: Option<&str>) -> bool {
    operation.is_some_and(|operation| READ_OPERATIONS.contains(&operation))
}

pub(crate) fn is_known_operation(operation: &str) -> bool {
    OPERATIONS.contains(&operation)
}

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 300;
/// Largest written body accepted in one call.
const MAX_WRITE_BODY: usize = 128 * 1024;
/// Largest single body field returned to the model; longer ones are clipped and
/// flagged instead of being silently shortened.
const MAX_ITEM_BODY: usize = 8 * 1024;
/// Bytes of one complete gh response that are kept for parsing. This is a raw
/// capture, not the presentation preview, so a page larger than the tool output
/// limit is still parsed completely inside this cap; beyond it the response is
/// refused instead of being read from a mangled buffer.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_STDERR: usize = 4 * 1024;
const DEFAULT_PER_PAGE: u32 = 30;
const MAX_PER_PAGE: u32 = 100;
const MAX_PAGE: u32 = 10_000;

pub(super) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "GitHub".into(),
        description: "Read and write GitHub issues, pull requests and repository details through the `gh` CLI already installed and signed in on this machine. Supported operations: repo_view; issue_list, issue_view, issue_create, issue_comment; pr_list, pr_view, pr_create, pr_comment, pr_merge. Reads return normalized JSON; issue_list and pr_list are paginated with page/per_page and report next_page when GitHub has more; only issue_list accepts a labels filter. Writes pass the body literally on stdin, report a URL and identifier, and are never retried: a timeout or transport failure after submission returns outcome:\"unknown\" because repeating it could create a duplicate. Use repo:\"owner/name\" for another repository; omitting it uses the session's current directory. merge always names a method, accepts match_head_commit as a safety guard, and reports \"accepted\" unless a follow-up read shows the merge completed. Every mutation needs the normal approval; loading this tool does not authorize a write.".into(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "operation":{"type":"string","enum":OPERATIONS,"description":"Which GitHub operation to run."},
                "repo":{"type":"string","pattern":"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$","description":"Repository as owner/name. Omit to use the repository of the current directory."},
                "number":{"type":"integer","minimum":1,"description":"Issue or pull request number. Required by issue_view, issue_comment, pr_view, pr_comment and pr_merge."},
                "title":{"type":"string","description":"Title for issue_create or pr_create."},
                "body":{"type":"string","description":format!("Body text for create or comment. Sent literally on stdin; at most {MAX_WRITE_BODY} bytes. pr_merge does not take a body.")},
                "labels":{"type":"array","items":{"type":"string"},"maxItems":20,"description":"Labels to add on create, or to filter issue_list by. pr_list does not accept labels: the GitHub REST pull request list has no label parameter, so passing labels with pr_list is refused before gh runs."},
                "state":{"type":"string","enum":["open","closed","all"],"default":"open","description":"State filter for issue_list and pr_list."},
                "base":{"type":"string","description":"Base branch for pr_create."},
                "head":{"type":"string","description":"Head branch for pr_create."},
                "merge_method":{"type":"string","enum":["merge","squash","rebase"],"default":"merge","description":"How pr_merge combines the commits. There is no force, admin or branch-deletion option."},
                "match_head_commit":{"type":"string","description":"Commit SHA the pull request head must still match for pr_merge to proceed."},
                "page":{"type":"integer","minimum":1,"maximum":MAX_PAGE,"default":1,"description":"Page number for issue_list and pr_list."},
                "per_page":{"type":"integer","minimum":1,"maximum":MAX_PER_PAGE,"default":DEFAULT_PER_PAGE,"description":format!("Items per page for list operations, 1..={MAX_PER_PAGE}.")},
                "timeout_secs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_SECS,"default":DEFAULT_TIMEOUT_SECS,"description":"How long gh may run before it is stopped."}
            },
            "required":["operation"],
            "additionalProperties":false
        }),
    }
}

/// Runs one operation with the `gh` found on PATH.
pub(super) async fn execute(input: Value, cwd: &str) -> Result<Value> {
    let program = super::lsp_servers::find_on_path("gh");
    execute_with(program.as_deref(), input, cwd).await
}

/// The body of `execute`, with program resolution separated so the
/// missing-binary path is reachable without mutating PATH.
pub(super) async fn execute_with(program: Option<&Path>, input: Value, cwd: &str) -> Result<Value> {
    let operation = input["operation"]
        .as_str()
        .context("operation is required")?
        .to_owned();
    ensure!(
        is_known_operation(&operation),
        "Unsupported GitHub operation {operation}; supported operations are {}",
        OPERATIONS.join(", ")
    );
    let program = match program {
        Some(program) => program.to_path_buf(),
        None => return Ok(missing_gh(&operation)),
    };
    let repo = repository(&input)?;
    let timeout = Duration::from_secs(
        input["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    );
    if READ_OPERATIONS.contains(&operation.as_str()) {
        return read(&program, cwd, &operation, repo.as_deref(), &input, timeout).await;
    }
    debug_assert!(WRITE_OPERATIONS.contains(&operation.as_str()));
    write(&program, cwd, &operation, repo.as_deref(), &input, timeout).await
}

/// A structured, actionable result instead of a panic when `gh` is absent.
fn missing_gh(operation: &str) -> Value {
    failure(
        operation,
        "gh_missing",
        "The GitHub tool needs the `gh` CLI on PATH. Install it (https://cli.github.com) and run `gh auth login` once; this tool does not manage credentials itself.",
        json!({"outcome":"not_applied"}),
    )
}

/// `repo` is `owner/name` or absent. Validation keeps it from ever being read as
/// an option or a path, and no other host or URL form is accepted.
fn repository(input: &Value) -> Result<Option<String>> {
    let Some(repo) = input.get("repo").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let repo = repo.as_str().context("repo must be a string")?.trim();
    let (owner, name) = repo
        .split_once('/')
        .with_context(|| format!("repo must be owner/name, not {repo}"))?;
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
            // A leading dash could read as an option and a leading dot as a
            // relative path, so neither can start a component.
            && part.starts_with(|byte: char| byte.is_ascii_alphanumeric())
    };
    ensure!(
        valid(owner) && valid(name) && !name.contains('/'),
        "repo must be owner/name using letters, digits, '.', '-' or '_', not {repo}"
    );
    Ok(Some(format!("{owner}/{name}")))
}

fn number(input: &Value) -> Result<u64> {
    input["number"]
        .as_u64()
        .filter(|number| (1..=u32::MAX as u64).contains(number))
        .context("number is required and must be a positive issue or pull request number")
}

/// An optional string field with a size cap.
fn optional_text(input: &Value, key: &str) -> Result<Option<String>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            ensure!(
                value.len() <= MAX_WRITE_BODY,
                "{key} is {} bytes; one call accepts at most {MAX_WRITE_BODY}",
                value.len()
            );
            Ok(Some(value.clone()))
        }
        Some(_) => bail!("{key} must be a string"),
    }
}

/// A required, non-empty string field.
fn required_text(input: &Value, key: &str) -> Result<String> {
    let value = optional_text(input, key)?.with_context(|| format!("{key} is required"))?;
    ensure!(!value.trim().is_empty(), "{key} must not be empty");
    Ok(value)
}

fn labels(input: &Value) -> Result<Vec<String>> {
    let Some(values) = input.get("labels").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let values = values.as_array().context("labels must be an array")?;
    ensure!(values.len() <= 20, "at most 20 labels per call");
    values
        .iter()
        .map(|label| {
            let label = label.as_str().context("each label must be a string")?;
            ensure!(
                !label.trim().is_empty() && label.len() <= 100,
                "a label must be non-empty and at most 100 bytes"
            );
            Ok(label.to_owned())
        })
        .collect()
}

/// `labels` is a valid filter only for the issue list. The REST pull request
/// list (`GET /repos/{owner}/{repo}/pulls`) accepts state, head, base, sort,
/// direction, per_page and page but no label parameter, so forwarding one would
/// silently return unfiltered pull requests. Refuse it here, before any process
/// starts, instead of pretending the filter was applied.
fn ensure_labels_supported(operation: &str, labels: &[String]) -> Result<()> {
    ensure!(
        operation != "pr_list" || labels.is_empty(),
        "pr_list does not support labels: the GitHub REST pull request list has no label filter, so the query would return unfiltered results. This tool cannot filter pull requests by label; issue_list labels apply only to issues."
    );
    Ok(())
}

fn page(input: &Value) -> u32 {
    input["page"]
        .as_u64()
        .unwrap_or(1)
        .clamp(1, MAX_PAGE as u64) as u32
}

fn per_page(input: &Value) -> u32 {
    input["per_page"]
        .as_u64()
        .unwrap_or(DEFAULT_PER_PAGE as u64)
        .clamp(1, MAX_PER_PAGE as u64) as u32
}

/// Starts `gh` with the shared bounded-output, process-tree and timeout helper.
async fn run_gh(
    program: &Path,
    cwd: &str,
    args: &[String],
    stdin: Option<std::fs::File>,
    timeout: Duration,
) -> Result<Value> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        // Never wait for an editor, a pager or a host prompt: every operation
        // below supplies its inputs explicitly.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0");
    super::bash_process::run_raw(&mut command, timeout, MAX_RESPONSE_BYTES, stdin).await
}

/// A `run_logged` result with the accessors this module needs.
struct Run {
    value: Value,
}

impl Run {
    fn timed_out(&self) -> bool {
        self.value["timed_out"] == true
    }

    fn code(&self) -> Option<i64> {
        self.value["exit_code"].as_i64()
    }

    fn success(&self) -> bool {
        self.value["success"] == true && !self.timed_out()
    }

    fn stdout(&self) -> &str {
        self.value["stdout"].as_str().unwrap_or_default()
    }

    fn stderr(&self) -> String {
        let text = self.value["stderr"].as_str().unwrap_or_default();
        clip(&redact(text), MAX_STDERR)
    }

    /// Whether the capture was cut short, which makes the stdout unusable as a
    /// complete JSON document.
    fn truncated(&self) -> bool {
        self.value["stdout_truncated"] == true || self.value["stderr_truncated"] == true
    }
}

async fn read(
    program: &Path,
    cwd: &str,
    operation: &str,
    repo: Option<&str>,
    input: &Value,
    timeout: Duration,
) -> Result<Value> {
    match operation {
        "repo_view" => {
            let mut args = vec!["repo".to_owned(), "view".to_owned()];
            if let Some(repo) = repo {
                args.push(repo.to_owned());
            }
            args.extend(
                [
                    "--json",
                    "nameWithOwner,name,owner,description,url,defaultBranchRef,isPrivate,isFork,stargazerCount,visibility,pushedAt,hasIssuesEnabled",
                ]
                .map(str::to_owned),
            );
            let run = Run {
                value: run_gh(program, cwd, &args, None, timeout).await?,
            };
            finish_read(
                operation,
                run,
                |value| json!({"repository": project_repository(value)}),
            )
        }
        "issue_view" | "pr_view" => {
            let view = if operation == "issue_view" {
                "issue"
            } else {
                "pr"
            };
            let number = number(input)?;
            let mut args = vec![
                view.to_owned(),
                "view".to_owned(),
                number.to_string(),
                "--json".to_owned(),
            ];
            args.push(
                if operation == "issue_view" {
                    "number,title,state,url,body,author,labels,assignees,createdAt,updatedAt,comments"
                } else {
                    "number,title,state,url,body,author,labels,isDraft,headRefName,baseRefName,createdAt,updatedAt,mergedAt,comments"
                }
                .to_owned(),
            );
            if let Some(repo) = repo {
                args.extend(["--repo".to_owned(), repo.to_owned()]);
            }
            let run = Run {
                value: run_gh(program, cwd, &args, None, timeout).await?,
            };
            finish_read(operation, run, |value| {
                if operation == "issue_view" {
                    json!({"issue": project_issue(value)})
                } else {
                    json!({"pull_request": project_pull_request(value)})
                }
            })
        }
        _ => list(program, cwd, operation, repo, input, timeout).await,
    }
}

/// Parses a complete JSON read and adds the normalized projection.
fn finish_read(operation: &str, run: Run, project: impl FnOnce(&Value) -> Value) -> Result<Value> {
    if run.timed_out() {
        return Ok(failure(
            operation,
            "timeout",
            "gh did not finish before the timeout and was stopped",
            json!({"outcome":"not_applied"}),
        ));
    }
    if !run.success() {
        return Ok(exit_failure(operation, &run));
    }
    let stdout = run.stdout().trim();
    if stdout.is_empty() {
        return Ok(failure(
            operation,
            "empty_response",
            "gh returned no output",
            json!({}),
        ));
    }
    if run.truncated() {
        return Ok(truncation_failure(operation));
    }
    let value: Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(error) => {
            let mut result = failure(
                operation,
                "invalid_json",
                format!("gh returned output that is not valid JSON: {error}"),
                json!({}),
            );
            if let Some(object) = result.as_object_mut() {
                object.insert("stdout_excerpt".into(), json!(excerpt(stdout)));
            }
            return Ok(result);
        }
    };
    // A syntactically valid document of the wrong shape is still unusable: this
    // must not silently become an object of empty fields.
    if !value.is_object() {
        return Ok(failure(
            operation,
            "invalid_json",
            "gh returned JSON that is not an object",
            json!({"stdout_excerpt": excerpt(stdout)}),
        ));
    }
    let mut result = project(&value);
    if let Some(object) = result.as_object_mut() {
        object.insert("is_error".into(), json!(false));
        object.insert("operation".into(), json!(operation));
    }
    Ok(result)
}

async fn list(
    program: &Path,
    cwd: &str,
    operation: &str,
    repo: Option<&str>,
    input: &Value,
    timeout: Duration,
) -> Result<Value> {
    let collection = if operation == "issue_list" {
        "issues"
    } else {
        "pulls"
    };
    let mut query = vec![
        format!("state={}", state(input)?),
        format!("per_page={}", per_page(input)),
        format!("page={}", page(input)),
    ];
    let labels = labels(input)?;
    ensure_labels_supported(operation, &labels)?;
    if !labels.is_empty() {
        query.push(format!(
            "labels={}",
            labels
                .iter()
                .map(|label| percent_encode(label))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    // The REST issue list also returns pull requests. Filtering them here keeps
    // `issue_list` meaning issues, which is what `gh issue list` does too.
    let path = match repo {
        Some(repo) => format!("repos/{repo}/{collection}?{}", query.join("&")),
        None => format!("repos/{{owner}}/{{repo}}/{collection}?{}", query.join("&")),
    };
    let args: Vec<String> = ["api", "--include", "-X", "GET"]
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(path))
        .collect();
    let run = Run {
        value: run_gh(program, cwd, &args, None, timeout).await?,
    };
    if run.timed_out() {
        return Ok(failure(
            operation,
            "timeout",
            "gh did not finish before the timeout and was stopped",
            json!({"outcome":"not_applied"}),
        ));
    }
    if !run.success() {
        return Ok(exit_failure(operation, &run));
    }
    if run.truncated() {
        return Ok(truncation_failure(operation));
    }
    let Some((headers, body)) = split_include(run.stdout()) else {
        return Ok(failure(
            operation,
            "invalid_response",
            "gh did not include HTTP headers in its output, so the page cannot be parsed",
            json!({}),
        ));
    };
    let body = body.trim();
    if body.is_empty() {
        return Ok(failure(
            operation,
            "empty_response",
            "gh returned no items",
            json!({}),
        ));
    }
    let items: Vec<Value> = match serde_json::from_str(body) {
        Ok(Value::Array(items)) => items,
        Ok(_) => {
            return Ok(failure(
                operation,
                "invalid_json",
                "gh returned a JSON value that is not a list",
                json!({"stdout_excerpt": excerpt(body)}),
            ))
        }
        Err(error) => {
            let mut result = failure(
                operation,
                "invalid_json",
                format!("gh returned output that is not valid JSON: {error}"),
                json!({}),
            );
            if let Some(object) = result.as_object_mut() {
                object.insert("stdout_excerpt".into(), json!(excerpt(body)));
            }
            return Ok(result);
        }
    };
    let mut filtered_pull_requests = 0_u64;
    let projected: Vec<Value> = items
        .iter()
        .filter(|item| {
            let is_pull_request = operation == "issue_list" && !item["pull_request"].is_null();
            if is_pull_request {
                filtered_pull_requests += 1;
            }
            !is_pull_request
        })
        .map(|item| {
            if operation == "issue_list" {
                project_issue(item)
            } else {
                project_pull_request(item)
            }
        })
        .collect();
    let next = next_page(&headers, page(input));
    Ok(json!({
        "is_error": false,
        "operation": operation,
        "page": page(input),
        "per_page": per_page(input),
        "count": projected.len(),
        "items": projected,
        "has_more": next.is_some(),
        "next_page": next,
        "filtered_pull_requests": filtered_pull_requests,
    }))
}

fn state(input: &Value) -> Result<String> {
    match input.get("state").and_then(Value::as_str) {
        None | Some("open") => Ok("open".into()),
        Some(value @ ("closed" | "all")) => Ok(value.into()),
        Some(other) => bail!("state must be open, closed or all, not {other}"),
    }
}

/// Percent-encodes a query value so a label can never change the query shape.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Splits `gh api --include` output into its headers and body. The first blank
/// line ends the headers; everything after it is the body.
fn split_include(text: &str) -> Option<(String, String)> {
    for separator in ["\r\n\r\n", "\n\n"] {
        if let Some(index) = text.find(separator) {
            return Some((
                text[..index].to_owned(),
                text[index + separator.len()..].to_owned(),
            ));
        }
    }
    None
}

/// The `page` of the `rel="next"` link, when GitHub sent one.
fn next_page(headers: &str, current: u32) -> Option<u32> {
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("link") {
            continue;
        }
        for part in value.split(',') {
            if !part.contains("rel=\"next\"") {
                continue;
            }
            let Some(start) = part.find('<').map(|index| index + 1) else {
                return Some(current + 1);
            };
            let Some(end) = part[start..].find('>').map(|index| index + start) else {
                return Some(current + 1);
            };
            let url = &part[start..end];
            if let Some(query) = url.split_once('?').map(|(_, query)| query) {
                for pair in query.split('&') {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    if key == "page" {
                        if let Ok(page) = value.parse::<u32>() {
                            return Some(page.max(current + 1));
                        }
                    }
                }
            }
            // A next link without an explicit page still means there is more.
            return Some(current + 1);
        }
    }
    None
}

// ---- projection ----

/// `gh ... --json` uses camelCase and REST uses snake_case, so each field is
/// looked up through the names both shapes actually use.
fn field_any(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn pointer_any(value: &Value, paths: &[&str]) -> String {
    paths
        .iter()
        .find_map(|path| value.pointer(path).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn project_repository(value: &Value) -> Value {
    json!({
        "name_with_owner": field_any(value, &["nameWithOwner", "full_name"]),
        "name": field_any(value, &["name"]),
        "owner": pointer_any(value, &["/owner/login"]),
        "description": field_any(value, &["description"]),
        "url": field_any(value, &["url", "html_url"]),
        "default_branch": pointer_any(value, &["/defaultBranchRef/name", "/default_branch"]),
        "private": value["isPrivate"] == true || value["private"] == true,
        "fork": value["isFork"] == true || value["fork"] == true,
        "stars": value["stargazerCount"].as_u64().unwrap_or(0),
        "visibility": field_any(value, &["visibility"]),
        "pushed_at": field_any(value, &["pushedAt", "pushed_at"]),
        "issues_enabled": value["hasIssuesEnabled"] != false,
    })
}

fn project_issue(value: &Value) -> Value {
    let (body, body_truncated) = clip_body(value);
    json!({
        "number": value["number"].as_u64().unwrap_or(0),
        "title": field_any(value, &["title"]),
        "state": field_any(value, &["state"]).to_ascii_lowercase(),
        "url": field_any(value, &["html_url", "url"]),
        "body": body,
        "body_truncated": body_truncated,
        "author": pointer_any(value, &["/author/login", "/user/login"]),
        "labels": label_names(value),
        "assignees": login_names(value, "assignees"),
        "created_at": field_any(value, &["createdAt", "created_at"]),
        "updated_at": field_any(value, &["updatedAt", "updated_at"]),
        "comments": value["comments"].as_u64().unwrap_or(0),
    })
}

fn project_pull_request(value: &Value) -> Value {
    let (body, body_truncated) = clip_body(value);
    json!({
        "number": value["number"].as_u64().unwrap_or(0),
        "title": field_any(value, &["title"]),
        "state": field_any(value, &["state"]).to_ascii_lowercase(),
        "url": field_any(value, &["html_url", "url"]),
        "body": body,
        "body_truncated": body_truncated,
        "author": pointer_any(value, &["/author/login", "/user/login"]),
        "labels": label_names(value),
        "draft": value["draft"] == true || value["isDraft"] == true,
        "head": pointer_any(value, &["/head/ref"]),
        "head_sha": pointer_any(value, &["/head/sha", "/headRefOid"]),
        "base": pointer_any(value, &["/base/ref"]),
        "created_at": field_any(value, &["createdAt", "created_at"]),
        "updated_at": field_any(value, &["updatedAt", "updated_at"]),
        "merged_at": field_any(value, &["mergedAt", "merged_at"]),
        "comments": value["comments"].as_u64().unwrap_or(0),
    })
}

/// `gh` emits labels as objects with `name` (and REST also allows strings).
fn label_names(value: &Value) -> Vec<String> {
    value["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .map(|label| {
                    label
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| label.get("name").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn login_names(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .map(|users| {
            users
                .iter()
                .map(|user| {
                    user.as_str()
                        .map(str::to_owned)
                        .or_else(|| user.get("login").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A body field, clipped to a known size and flagged when it was shortened.
fn clip_body(value: &Value) -> (String, bool) {
    let body = field_any(value, &["body"]);
    if body.len() <= MAX_ITEM_BODY {
        return (body, false);
    }
    let mut end = MAX_ITEM_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!(
            "{}\n[body truncated: {} of {} bytes shown]",
            &body[..end],
            end,
            body.len()
        ),
        true,
    )
}

/// Keeps at most `limit` bytes on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn excerpt(text: &str) -> String {
    let limit = 400;
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// ---- writes ----

async fn write(
    program: &Path,
    cwd: &str,
    operation: &str,
    repo: Option<&str>,
    input: &Value,
    timeout: Duration,
) -> Result<Value> {
    let (args, body) = write_arguments(operation, repo, input)?;
    // The body is handed to gh on stdin from a temp file, so newlines, quotes
    // and leading dashes are data and never options.
    let body_file = match &body {
        Some(text) => {
            let file = tempfile::tempfile().context("Creating the body file")?;
            {
                use std::io::Write;
                let mut writer = &file;
                writer
                    .write_all(text.as_bytes())
                    .context("Writing the body file")?;
            }
            // Writing advanced the offset, and a duplicated descriptor shares
            // it, so rewind before gh reads: otherwise gh starts at EOF.
            {
                use std::io::{Seek, SeekFrom};
                (&file)
                    .seek(SeekFrom::Start(0))
                    .context("Rewinding the body file")?;
            }
            Some(file)
        }
        None => None,
    };
    let stdin = match &body_file {
        Some(file) => Some(file.try_clone().context("Duplicating the body file")?),
        None => None,
    };
    let run = Run {
        value: run_gh(program, cwd, &args, stdin, timeout).await?,
    };
    if run.timed_out() {
        return Ok(unknown_outcome(
            operation,
            "gh was stopped at the timeout, so the request may or may not have been applied",
        ));
    }
    if !run.success() {
        return Ok(write_failure(operation, &run));
    }
    let stdout = run.stdout().trim().to_owned();
    let url = first_url(&stdout);
    let created_number = url.as_deref().and_then(trailing_number);
    match operation {
        "issue_create" | "pr_create" => {
            let mut result = json!({
                "is_error": false,
                "operation": operation,
                "outcome": "succeeded",
                "created": true,
                "url": url,
                "number": created_number,
            });
            // A follow-up read that fails must not turn a completed create into
            // a failure: the issue exists either way.
            if let Some(number) = created_number {
                let view = if operation.starts_with("issue") {
                    "issue_view"
                } else {
                    "pr_view"
                };
                match detail_read(program, cwd, view, number, repo, timeout).await {
                    Ok(detail) => {
                        if let Some(object) = result.as_object_mut() {
                            object.insert("detail".into(), detail);
                        }
                    }
                    Err(error) => {
                        if let Some(object) = result.as_object_mut() {
                            object.insert("detail_error".into(), json!(format!("{error:#}")));
                        }
                    }
                }
            }
            Ok(result)
        }
        "issue_comment" | "pr_comment" => Ok(json!({
            "is_error": false,
            "operation": operation,
            "outcome": "succeeded",
            "commented": true,
            "url": url,
            "number": input["number"].as_u64(),
        })),
        _ => {
            // gh exits 0 when it *enqueues* a merge or enables auto-merge too,
            // so a follow-up read decides whether the merge actually happened.
            let number = number(input)?;
            let method = merge_method(input)?;
            let mut result = json!({
                "is_error": false,
                "operation": operation,
                "outcome": "accepted",
                "merged": Value::Null,
                "number": number,
                "merge_method": method,
                "head_guard": input["match_head_commit"].as_str(),
                "url": url,
            });
            match detail_read(program, cwd, "pr_view", number, repo, timeout).await {
                Ok(detail) => {
                    let merged_now = detail["pull_request"]["merged_at"]
                        .as_str()
                        .is_some_and(|merged_at| !merged_at.is_empty());
                    let state = detail["pull_request"]["state"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if let Some(object) = result.as_object_mut() {
                        if merged_now || state == "merged" {
                            object.insert("merged".into(), json!(true));
                            object.insert("outcome".into(), json!("merged"));
                        } else {
                            object.insert("merged".into(), json!(false));
                            object.insert(
                                "error".into(),
                                json!("gh accepted the merge request but the pull request is not merged yet; it may be queued or waiting for required checks, so this is not reported as a completed merge"),
                            );
                        }
                        object.insert("detail".into(), detail);
                    }
                }
                Err(error) => {
                    if let Some(object) = result.as_object_mut() {
                        object.insert(
                            "verify_error".into(),
                            json!(format!("{error:#}; the merge may still be queued, so it is not reported as completed")),
                        );
                    }
                }
            }
            Ok(result)
        }
    }
}

/// Arguments for one write, plus the body that goes on stdin.
fn write_arguments(
    operation: &str,
    repo: Option<&str>,
    input: &Value,
) -> Result<(Vec<String>, Option<String>)> {
    let mut args: Vec<String> = Vec::new();
    let (command, subcommand) = match operation {
        "issue_create" => ("issue", "create"),
        "issue_comment" => ("issue", "comment"),
        "pr_create" => ("pr", "create"),
        "pr_comment" => ("pr", "comment"),
        "pr_merge" => ("pr", "merge"),
        other => bail!("Unsupported GitHub write operation {other}"),
    };
    args.extend([command.to_owned(), subcommand.to_owned()]);
    let create = matches!(operation, "issue_create" | "pr_create");
    if !create {
        args.push(number(input)?.to_string());
    }
    if let Some(repo) = repo {
        args.extend(["--repo".to_owned(), repo.to_owned()]);
    }
    let body = match operation {
        // Comments need their text; an empty comment is almost always a mistake.
        "issue_comment" | "pr_comment" => Some(required_text(input, "body")?),
        // A create always sends `--body-file -`, even when empty, so gh never
        // opens an editor or prompts for one.
        "issue_create" | "pr_create" => Some(optional_text(input, "body")?.unwrap_or_default()),
        // `pr_merge` has no body in this wrapper: its commit text is not ours.
        _ => {
            ensure!(
                input.get("body").is_none_or(Value::is_null),
                "pr_merge does not take a body"
            );
            None
        }
    };
    if create {
        args.extend(["--title".to_owned(), required_text(input, "title")?]);
    }
    if body.is_some() {
        // `-` means stdin: the text never becomes an argument.
        args.extend(["--body-file".to_owned(), "-".to_owned()]);
    }
    if let Some(base) = optional_text(input, "base")? {
        args.extend(["--base".to_owned(), base]);
    }
    if let Some(head) = optional_text(input, "head")? {
        args.extend(["--head".to_owned(), head]);
    }
    for label in labels(input)? {
        args.extend(["--label".to_owned(), label]);
    }
    if operation == "pr_merge" {
        let method = merge_method(input)?;
        match method.as_str() {
            "merge" => args.push("--merge".into()),
            "squash" => args.push("--squash".into()),
            "rebase" => args.push("--rebase".into()),
            other => bail!("merge_method must be merge, squash or rebase, not {other}"),
        }
        if let Some(commit) = input["match_head_commit"]
            .as_str()
            .map(str::trim)
            .filter(|commit| !commit.is_empty())
        {
            ensure!(
                (4..=64).contains(&commit.len())
                    && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "match_head_commit must be a hexadecimal commit SHA"
            );
            args.extend(["--match-head-commit".to_owned(), commit.to_owned()]);
        }
    }
    // The flags that could force a merge, delete a branch or use admin rights are
    // never constructed, and any other input was already refused by the schema.
    Ok((args, body))
}

fn merge_method(input: &Value) -> Result<String> {
    match input.get("merge_method").and_then(Value::as_str) {
        None => Ok("merge".into()),
        Some(method @ ("merge" | "squash" | "rebase")) => Ok(method.into()),
        Some(other) => bail!("merge_method must be merge, squash or rebase, not {other}"),
    }
}

/// Reads an object back through the same typed read path.
async fn detail_read(
    program: &Path,
    cwd: &str,
    view: &str,
    number: u64,
    repo: Option<&str>,
    timeout: Duration,
) -> Result<Value> {
    let input = json!({"operation": view, "number": number, "repo": repo});
    let value = Box::pin(read(program, cwd, view, repo, &input, timeout)).await?;
    if value["is_error"] != false {
        let error = value["error"].as_str().unwrap_or("unknown error");
        let stderr = value["stderr"].as_str().unwrap_or_default();
        bail!(
            "detail read failed: {error}{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(" ({})", clip(stderr, MAX_STDERR))
            }
        );
    }
    Ok(value)
}

fn first_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.starts_with("https://"))
        .map(|word| word.trim_end_matches(['.', ',']).to_owned())
}

fn trailing_number(url: &str) -> Option<u64> {
    let last = url.rsplit('/').next()?;
    last.parse::<u64>().ok().filter(|number| *number > 0)
}

// ---- results ----

fn failure(operation: &str, kind: &str, message: impl Into<String>, extra: Value) -> Value {
    let mut value = json!({
        "is_error": true,
        "operation": operation,
        "error_kind": kind,
        "error": message.into(),
    });
    if let (Some(object), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        object.extend(extra.clone());
    }
    value
}

fn exit_failure(operation: &str, run: &Run) -> Value {
    let stderr = run.stderr();
    let kind = match run.code() {
        Some(4) => "auth_required",
        Some(2) => "cancelled",
        _ => "failed",
    };
    let mut value = failure(
        operation,
        kind,
        match kind {
            "auth_required" => {
                "gh is not signed in for this host: run `gh auth login` in a terminal".to_owned()
            }
            "cancelled" => "gh was cancelled before it finished".to_owned(),
            _ => "gh reported a failure".to_owned(),
        },
        json!({"exit_code": run.code(), "stderr": stderr}),
    );
    if let Some(object) = value.as_object_mut() {
        object.insert("outcome".into(), json!("not_applied"));
    }
    value
}

/// A write that failed after the request may already have reached GitHub.
fn write_failure(operation: &str, run: &Run) -> Value {
    let stderr = run.stderr();
    let transport = looks_like_transport_failure(&stderr);
    let mut value = exit_failure(operation, run);
    let original = value["error"].as_str().unwrap_or_default().to_owned();
    if let Some(object) = value.as_object_mut() {
        if transport {
            object.insert("outcome".into(), json!("unknown"));
            object.insert(
                "error".into(),
                json!(format!(
                    "gh failed with what looks like a transport error, so the request may or may not have been applied. Do not repeat it blindly; check GitHub first. {original}"
                )),
            );
        }
        object.insert("maybe_applied".into(), json!(transport));
    }
    value
}

fn unknown_outcome(operation: &str, message: &str) -> Value {
    failure(
        operation,
        "timeout",
        format!(
            "{message}. This write is never retried automatically because a second attempt could create a duplicate; check GitHub before trying again."
        ),
        json!({"outcome":"unknown"}),
    )
}

/// A response larger than the explicit raw cap is not a JSON document, so it is
/// refused rather than parsed from a shortened buffer.
fn truncation_failure(operation: &str) -> Value {
    failure(
        operation,
        "output_limit",
        format!(
            "gh returned more than {MAX_RESPONSE_BYTES} bytes in one response, so the JSON cannot be read completely. Use a smaller per_page or a later page."
        ),
        json!({"truncated": true, "output_limit": MAX_RESPONSE_BYTES}),
    )
}

/// Markers gh prints when the request itself did not complete. They are only
/// used to label a failure as "unknown", never to claim success.
fn looks_like_transport_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "connection refused",
        "connection reset",
        "i/o timeout",
        "tls handshake",
        "unexpected eof",
        "broken pipe",
        "no such host",
        "http 502",
        "http 503",
        "http 504",
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Removes credential-shaped tokens from anything returned to the model.
fn redact(text: &str) -> String {
    const PREFIXES: &[&str] = &["gho_", "ghp_", "ghu_", "ghs_", "ghr_", "github_pat_"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = PREFIXES.iter().filter_map(|prefix| rest.find(prefix)).min() {
        out.push_str(&rest[..index]);
        let token = &rest[index..];
        let end = token
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ')'))
            .unwrap_or(token.len());
        out.push_str("[redacted token]");
        rest = &token[end..];
    }
    out.push_str(rest);
    out
}

// The fake-`gh` probes need a POSIX shell and an executable script, so they are
// Unix-only; the platform-neutral contract checks run everywhere.
#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
mod platform_tests;
