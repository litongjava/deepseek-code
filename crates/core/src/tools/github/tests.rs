//! Fake-`gh` probes: every test runs the real module against an executable
//! script that records its argv and stdin, and emits the real child stdout /
//! stderr / exit codes. Nothing here reaches GitHub.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// One step of a multi-invocation fake `gh`.
struct Step {
    stdout: String,
    stderr: String,
    exit: i32,
}

/// An executable fake `gh` plus the files it records. Every invocation records
/// its own argv and stdin (`argv_1`, `stdin_1`, ...), because one tool call can
/// run gh twice (a create followed by its detail read).
struct Fake {
    dir: tempfile::TempDir,
    program: PathBuf,
}

impl Fake {
    fn new() -> Self {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("gh");
        Self { dir, program }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A script that emits the same result for every invocation.
    fn emit(&self, stdout: &str, stderr: &str, exit: i32) -> &Self {
        self.emit_sleeping(stdout, stderr, exit, 0)
    }

    fn emit_sleeping(&self, stdout: &str, stderr: &str, exit: i32, sleep: u64) -> &Self {
        self.script(
            &[Step {
                stdout: stdout.to_owned(),
                stderr: stderr.to_owned(),
                exit,
            }],
            sleep,
        )
    }

    /// A script whose behaviour changes per invocation, so a successful write
    /// can be followed by a failing (or differently shaped) read.
    fn sequence(&self, steps: &[Step]) -> &Self {
        self.script(steps, 0)
    }

    fn script(&self, steps: &[Step], sleep: u64) -> &Self {
        let mut arms = String::new();
        for (index, step) in steps.iter().enumerate() {
            let out = self.path(&format!("out{}", index + 1));
            let err = self.path(&format!("err{}", index + 1));
            std::fs::write(&out, &step.stdout).unwrap();
            std::fs::write(&err, &step.stderr).unwrap();
            arms.push_str(&format!(
                "{}) OUT='{out}'; ERR='{err}'; EX={exit};;\n",
                index + 1,
                out = out.display(),
                err = err.display(),
                exit = step.exit,
            ));
        }
        // Later invocations repeat the last step, so `emit` needs only one.
        let last = steps.len();
        let script = format!(
            "#!/bin/sh\necho $$ > '{self_pid}'\ncount=$(cat '{counter}' 2>/dev/null || echo 0)\ncount=$((count + 1))\necho \"$count\" > '{counter}'\nprintf '%s\\n' \"$@\" > '{prefix}'_$count\ncat > '{stdin}'_$count\nif [ {sleep} -gt 0 ]; then sleep {sleep}; fi\nOUT=''; ERR=''; EX=1\ncase \"$count\" in\n{arms}*) OUT='{lastout}'; ERR='{lasterr}'; EX={lastexit};;\nesac\n[ -n \"$OUT\" ] && cat \"$OUT\"\n[ -n \"$ERR\" ] && cat \"$ERR\" >&2\nexit \"$EX\"\n",
            self_pid = self.path("gh-self.pid").display(),
            counter = self.path("count").display(),
            prefix = self.path("argv").display(),
            stdin = self.path("stdin").display(),
            arms = arms,
            lastout = self.path(&format!("out{last}")).display(),
            lasterr = self.path(&format!("err{last}")).display(),
            lastexit = steps[last - 1].exit,
        );
        self.write(script);
        self
    }

    fn write(&self, script: String) {
        std::fs::write(&self.program, script).unwrap();
        let mut permissions = std::fs::metadata(&self.program).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&self.program, permissions).unwrap();
    }

    fn invocations(&self) -> usize {
        std::fs::read_to_string(self.path("count"))
            .ok()
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0)
    }

    fn argv(&self) -> Vec<String> {
        self.argv_at(1)
    }

    fn argv_at(&self, invocation: usize) -> Vec<String> {
        std::fs::read_to_string(self.path(&format!("argv_{invocation}")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn stdin_text(&self) -> String {
        self.stdin_at(1)
    }

    fn stdin_at(&self, invocation: usize) -> String {
        std::fs::read_to_string(self.path(&format!("stdin_{invocation}"))).unwrap_or_default()
    }

    fn cwd(&self) -> &Path {
        self.dir.path()
    }

    async fn run(&self, input: Value) -> Value {
        super::execute_with(
            Some(&self.program),
            input,
            self.dir.path().to_str().unwrap(),
        )
        .await
        .expect("the tool returns a value")
    }
}

/// The PID a fixture process recorded for itself.
fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a PID still names a live (non-zombie) process.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32) -> bool {
    true
}

fn link_page(page: u32, has_next: bool) -> String {
    if has_next {
        format!("Link: <https://api.github.com/repositories/1/issues?page={}&per_page=2>; rel=\"next\", <https://api.github.com/repositories/1/issues?page=9>; rel=\"last\"", page + 1)
    } else {
        String::new()
    }
}

#[tokio::test]
async fn repo_view_returns_normalized_json() {
    let fake = Fake::new();
    fake.emit(
        r#"{"nameWithOwner":"octo/demo","name":"demo","owner":{"login":"octo"},"description":"A demo","url":"https://github.com/octo/demo","defaultBranchRef":{"name":"main"},"isPrivate":true,"isFork":false,"stargazerCount":7,"visibility":"PRIVATE","pushedAt":"2026-01-02T03:04:05Z","hasIssuesEnabled":true}"#,
        "",
        0,
    );
    let result = fake
        .run(json!({"operation":"repo_view","repo":"octo/demo"}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result["operation"], "repo_view");
    let repo = &result["repository"];
    assert_eq!(repo["name_with_owner"], "octo/demo");
    assert_eq!(repo["owner"], "octo");
    assert_eq!(repo["description"], "A demo");
    assert_eq!(repo["default_branch"], "main");
    assert_eq!(repo["private"], true);
    assert_eq!(repo["stars"], 7);
    assert_eq!(repo["pushed_at"], "2026-01-02T03:04:05Z");
    let argv = fake.argv();
    assert_eq!(argv[0], "repo");
    assert_eq!(argv[1], "view");
    assert_eq!(argv[2], "octo/demo");
    assert_eq!(argv[3], "--json");
}

#[tokio::test]
async fn issue_list_paginates_from_the_link_header() {
    let fake = Fake::new();
    let body = r#"[{"number":1,"title":"First","state":"open","html_url":"https://github.com/octo/demo/issues/1","body":"one","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","user":{"login":"ann"},"labels":[{"name":"bug"}],"comments":3},{"number":2,"title":"A PR","state":"open","pull_request":{"url":"x"}}]"#;
    fake.emit(
        &format!(
            "HTTP/2.0 200 OK\n{}\nContent-Type: application/json\n\n{}",
            link_page(1, true),
            body
        ),
        "",
        0,
    );
    let result = fake
        .run(json!({"operation":"issue_list","repo":"octo/demo","page":1,"per_page":2}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result["count"], 1, "{result}");
    assert_eq!(result["filtered_pull_requests"], 1, "{result}");
    assert_eq!(result["has_more"], true);
    assert_eq!(result["next_page"], 2);
    let item = &result["items"][0];
    assert_eq!(item["number"], 1);
    assert_eq!(item["title"], "First");
    assert_eq!(item["author"], "ann");
    assert_eq!(item["labels"], json!(["bug"]));
    assert_eq!(item["created_at"], "2026-01-01T00:00:00Z");
    assert_eq!(item["url"], "https://github.com/octo/demo/issues/1");
    let argv = fake.argv();
    assert_eq!(argv[0], "api");
    assert_eq!(argv[1], "--include");
    assert_eq!(argv[2], "-X");
    assert_eq!(argv[3], "GET");
    assert_eq!(
        argv[4],
        "repos/octo/demo/issues?state=open&per_page=2&page=1"
    );
}

#[tokio::test]
async fn pr_list_reports_the_next_page_and_normalizes_rest_fields() {
    let fake = Fake::new();
    let body = r#"[{"number":9,"title":"Feature","state":"open","html_url":"https://github.com/octo/demo/pull/9","body":"body","draft":true,"head":{"ref":"feat","sha":"abc"},"base":{"ref":"main"},"user":{"login":"bob"},"created_at":"2026-02-01T00:00:00Z","updated_at":"2026-02-02T00:00:00Z","merged_at":null}]"#;
    fake.emit(
        &format!("HTTP/2.0 200 OK\n{}\n\n{}", link_page(2, false), body),
        "",
        0,
    );
    let result = fake
        .run(json!({"operation":"pr_list","repo":"octo/demo","page":2,"per_page":1,"state":"all"}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result["has_more"], false);
    assert_eq!(result["next_page"], Value::Null);
    let item = &result["items"][0];
    assert_eq!(item["number"], 9);
    assert_eq!(item["author"], "bob");
    assert_eq!(item["draft"], true);
    assert_eq!(item["head"], "feat");
    assert_eq!(item["head_sha"], "abc");
    assert_eq!(item["base"], "main");
    assert_eq!(item["merged_at"], "");
    assert_eq!(
        fake.argv()[4],
        "repos/octo/demo/pulls?state=all&per_page=1&page=2"
    );
}

#[tokio::test]
async fn pr_list_refuses_a_label_filter_without_running_gh() {
    let fake = Fake::new();
    fake.emit("", "", 0);
    let error = super::execute_with(
        Some(&fake.program),
        json!({"operation":"pr_list","repo":"octo/demo","labels":["bug"]}),
        fake.cwd().to_str().unwrap(),
    )
    .await
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("pr_list does not support labels"),
        "{message}"
    );
    assert_eq!(fake.invocations(), 0, "gh must not run: {message}");
    assert!(
        !fake.path("argv_1").exists() && !fake.path("gh-self.pid").exists(),
        "gh must not run"
    );
}

#[tokio::test]
async fn issue_list_still_encodes_its_label_filter() {
    let fake = Fake::new();
    fake.emit("HTTP/2.0 200 OK\n\n[]", "", 0);
    let result = fake
        .run(json!({"operation":"issue_list","repo":"octo/demo","labels":["help wanted","bug"]}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(
        fake.argv()[4],
        "repos/octo/demo/issues?state=open&per_page=30&page=1&labels=help%20wanted,bug"
    );
}

#[tokio::test]
async fn issue_create_passes_the_body_literally_on_stdin() {
    let fake = Fake::new();
    fake.emit("https://github.com/octo/demo/issues/42\n", "", 0);
    // Unicode, blank lines, quotes, a leading dash and a shell metacharacter:
    // all of it must reach gh unchanged.
    let body = "First line — ünïcode ✓\n\n  --not-a-flag\n'; rm -rf /tmp/nope; echo '\nlast line";
    let result = fake
        .run(json!({
            "operation":"issue_create",
            "repo":"octo/demo",
            "title":"Title with 'quotes' and spaces",
            "body":body
        }))
        .await;
    assert_eq!(result["created"], true, "{result}");
    assert_eq!(result["number"], 42, "{result}");
    assert_eq!(fake.stdin_text(), body, "the body was altered");
    let argv = fake.argv();
    assert!(argv.contains(&"--body-file".to_owned()), "{argv:?}");
    assert!(
        argv.iter().filter(|arg| *arg == "-").count() == 1,
        "the body must travel on stdin: {argv:?}"
    );
    // The body is not present in any argument.
    assert!(
        !argv.iter().any(|arg| arg.contains("last line")),
        "{argv:?}"
    );
    assert_eq!(argv[2], "--repo");
    assert_eq!(argv[3], "octo/demo");
    assert_eq!(argv[4], "--title");
    assert_eq!(argv[5], "Title with 'quotes' and spaces");
}

#[tokio::test]
async fn arguments_are_never_joined_into_a_shell_command() {
    let fake = Fake::new();
    fake.emit("https://github.com/octo/demo/issues 1\n", "", 0);
    let title = "title; touch pwned && echo $(id)";
    let _ = fake
        .run(json!({"operation":"issue_create","title":title,"body":"x"}))
        .await;
    let argv = fake.argv();
    // Exactly one element carries the whole title; nothing was split or
    // interpreted, and no shell was involved.
    assert_eq!(
        argv.iter().filter(|arg| *arg == title).count(),
        1,
        "{argv:?}"
    );
    assert!(!argv.iter().any(|arg| arg == "-c"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg.ends_with("sh")), "{argv:?}");
    assert_eq!(argv[0], "issue");
    assert_eq!(argv[1], "create");
    assert!(!fake.cwd().join("pwned").exists());
}

#[tokio::test]
async fn create_without_a_body_still_supplies_one_so_gh_never_prompts() {
    let fake = Fake::new();
    fake.emit("https://github.com/octo/demo/issues/7\n", "", 0);
    let _ = fake
        .run(json!({"operation":"issue_create","title":"No body"}))
        .await;
    let argv = fake.argv();
    assert!(argv.contains(&"--body-file".to_owned()), "{argv:?}");
    assert_eq!(fake.stdin_text(), "");
}

#[tokio::test]
async fn missing_gh_is_a_structured_error() {
    let dir = tempfile::tempdir().unwrap();
    let result = super::execute_with(
        None,
        json!({"operation":"repo_view"}),
        dir.path().to_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(result["is_error"], true);
    assert_eq!(result["error_kind"], "gh_missing");
    assert_eq!(result["outcome"], "not_applied");
    assert!(result["error"].as_str().unwrap().contains("gh auth login"));
}

#[tokio::test]
async fn nonzero_exit_reports_the_code_and_redacts_tokens() {
    let fake = Fake::new();
    fake.emit(
        "",
        "HTTP 404: Not Found (token gho_SECRETTOKEN123 used)\n",
        1,
    );
    let result = fake
        .run(json!({"operation":"issue_view","repo":"octo/demo","number":5}))
        .await;
    assert_eq!(result["is_error"], true);
    assert_eq!(result["error_kind"], "failed");
    assert_eq!(result["exit_code"], 1);
    assert_eq!(result["outcome"], "not_applied");
    let stderr = result["stderr"].as_str().unwrap();
    assert!(stderr.contains("404"), "{stderr}");
    assert!(!stderr.contains("SECRETTOKEN"), "{stderr}");
    assert!(stderr.contains("[redacted token]"), "{stderr}");
}

#[tokio::test]
async fn auth_exit_code_is_reported_as_such() {
    let fake = Fake::new();
    fake.emit(
        "",
        "To get started with GitHub CLI, please run: gh auth login\n",
        4,
    );
    let result = fake
        .run(json!({"operation":"repo_view","repo":"octo/demo"}))
        .await;
    assert_eq!(result["error_kind"], "auth_required", "{result}");
}

#[tokio::test]
async fn invalid_json_is_reported() {
    let fake = Fake::new();
    fake.emit("this is not json\n", "", 0);
    let result = fake
        .run(json!({"operation":"repo_view","repo":"octo/demo"}))
        .await;
    assert_eq!(result["error_kind"], "invalid_json", "{result}");
    assert!(result["stdout_excerpt"]
        .as_str()
        .unwrap()
        .contains("this is not json"));
}

#[tokio::test]
async fn valid_json_of_the_wrong_shape_is_rejected() {
    let fake = Fake::new();
    fake.emit("[1,2,3]\n", "", 0);
    let single = fake
        .run(json!({"operation":"pr_view","repo":"octo/demo","number":1}))
        .await;
    assert_eq!(single["error_kind"], "invalid_json", "{single}");
    assert!(single["error"].as_str().unwrap().contains("not an object"));

    fake.emit("HTTP/2.0 200 OK\n\n{\"message\":\"Not Found\"}", "", 0);
    let list = fake
        .run(json!({"operation":"pr_list","repo":"octo/demo"}))
        .await;
    assert_eq!(list["error_kind"], "invalid_json", "{list}");
    assert!(list["error"].as_str().unwrap().contains("not a list"));
}

#[tokio::test]
async fn a_response_over_the_raw_cap_is_reported_as_an_output_limit() {
    let fake = Fake::new();
    // More than the explicit raw cap, so no truncation of the document is
    // mistaken for a JSON error.
    let script = format!(
        "#!/bin/sh\ncount=$(cat '{counter}' 2>/dev/null || echo 0)\ncount=$((count + 1))\necho \"$count\" > '{counter}'\nprintf '%s\\n' \"$@\" > '{argv}'_$count\ncat > '{stdin}'_$count\nprintf 'HTTP/2.0 200 OK\\n\\n'\nyes x | head -c 2000000\nexit 0\n",
        counter = fake.path("count").display(),
        argv = fake.path("argv").display(),
        stdin = fake.path("stdin").display(),
    );
    fake.write(script);
    let result = fake
        .run(json!({"operation":"issue_list","repo":"octo/demo","per_page":100}))
        .await;
    assert_eq!(result["error_kind"], "output_limit", "{result}");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["output_limit"], super::MAX_RESPONSE_BYTES);
    assert_eq!(result["is_error"], true);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("smaller per_page"),
        "{result}"
    );
}

/// A valid page larger than the tool's presentation limit (100 KiB) must still
/// be parsed completely: the body is clipped *after* parsing, not by the capture.
#[tokio::test]
async fn a_page_larger_than_the_presentation_limit_is_parsed_and_clipped() {
    let fake = Fake::new();
    let body = "ünïcode body ✓ ".repeat(10_000);
    assert!(body.len() > 150_000, "{}", body.len());
    let page = format!(r#"[{{"number":7,"title":"Big page","state":"open","body":"{body}"}}]"#);
    assert!(page.len() > 150_000, "{}", page.len());
    fake.emit(&format!("HTTP/2.0 200 OK\n\n{page}"), "", 0);
    let result = fake
        .run(json!({"operation":"issue_list","repo":"octo/demo","per_page":1}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result["count"], 1, "{result}");
    let item = &result["items"][0];
    assert_eq!(item["title"], "Big page");
    assert_eq!(item["number"], 7);
    assert_eq!(item["body_truncated"], true, "{item}");
    let clipped = item["body"].as_str().unwrap();
    assert!(clipped.starts_with("ünïcode body ✓"), "{clipped:.40}");
    assert!(clipped.contains("[body truncated:"), "{clipped:.200}");
}

#[tokio::test]
async fn a_large_single_object_is_parsed_before_its_body_is_clipped() {
    let fake = Fake::new();
    let body = "正文 ✓ ".repeat(40_000);
    let object = format!(r#"{{"number":3,"title":"Long","state":"OPEN","body":"{body}"}}"#);
    assert!(object.len() > 150_000, "{}", object.len());
    fake.emit(&object, "", 0);
    let result = fake
        .run(json!({"operation":"issue_view","repo":"octo/demo","number":3}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result["issue"]["title"], "Long");
    assert_eq!(result["issue"]["state"], "open");
    assert_eq!(result["issue"]["body_truncated"], true);
}

#[tokio::test]
async fn long_bodies_are_clipped_and_flagged() {
    let fake = Fake::new();
    let long = "y".repeat(20_000);
    let body = format!(r#"[{{"number":1,"title":"Long","state":"open","body":"{long}"}}]"#);
    fake.emit(&format!("HTTP/2.0 200 OK\n\n{body}"), "", 0);
    let result = fake
        .run(json!({"operation":"issue_list","repo":"octo/demo"}))
        .await;
    assert_eq!(result["is_error"], false, "{result}");
    let item = &result["items"][0];
    assert_eq!(item["body_truncated"], true);
    let text = item["body"].as_str().unwrap();
    assert!(text.len() < 9_000, "{}", text.len());
    assert!(text.contains("[body truncated:"), "{text}");
}

#[tokio::test]
async fn timeout_reports_an_unknown_outcome_for_a_write_and_stops_the_child() {
    let fake = Fake::new();
    fake.emit_sleeping("https://github.com/octo/demo/issues/1\n", "", 0, 30);
    let started = std::time::Instant::now();
    let result = fake
        .run(json!({"operation":"issue_create","title":"Slow","body":"x","timeout_secs":1}))
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call did not stop at its timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(result["is_error"], true, "{result}");
    assert_eq!(result["error_kind"], "timeout");
    assert_eq!(result["outcome"], "unknown");
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("never retried automatically"));
    // The owned child really is gone: the fake `gh` records its own pid, and
    // the process tree is stopped rather than left sleeping.
    let pid = read_pid(&fake.path("gh-self.pid")).expect("the fake gh recorded its pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while process_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the timed-out gh process (PID {pid}) is still running"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[tokio::test]
async fn transport_failure_marks_the_write_outcome_unknown() {
    let fake = Fake::new();
    fake.emit(
        "",
        "Post \"https://api.github.com\": connection reset by peer\n",
        1,
    );
    let result = fake
        .run(json!({"operation":"issue_comment","repo":"octo/demo","number":3,"body":"hi"}))
        .await;
    assert_eq!(result["outcome"], "unknown", "{result}");
    assert_eq!(result["maybe_applied"], true);
    assert_eq!(result["exit_code"], 1);
}

#[tokio::test]
async fn create_success_with_a_failed_detail_read_is_still_created() {
    let fake = Fake::new();
    fake.sequence(&[
        Step {
            stdout: "https://github.com/octo/demo/issues/51\n".into(),
            stderr: String::new(),
            exit: 0,
        },
        Step {
            stdout: String::new(),
            stderr: "HTTP 500: server error\n".into(),
            exit: 1,
        },
    ]);
    let result = fake
        .run(json!({"operation":"issue_create","repo":"octo/demo","title":"Keep me","body":"b"}))
        .await;
    assert_eq!(result["created"], true, "{result}");
    assert_eq!(result["outcome"], "succeeded", "{result}");
    assert_eq!(result["number"], 51);
    assert_eq!(result["is_error"], false);
    assert!(result.get("detail").is_none());
    assert!(result["detail_error"].as_str().unwrap().contains("500"));
}

#[tokio::test]
async fn merge_names_a_method_and_guards_the_head_commit() {
    let fake = Fake::new();
    fake.sequence(&[
        Step {
            stdout: "https://github.com/octo/demo/pull/12\n".into(),
            stderr: String::new(),
            exit: 0,
        },
        // A follow-up read is what decides whether the merge really happened.
        Step {
            stdout: r#"{"number":12,"state":"OPEN","mergedAt":null,"title":"t","url":"u"}"#.into(),
            stderr: String::new(),
            exit: 0,
        },
    ]);
    let result = fake
        .run(json!({
            "operation":"pr_merge",
            "repo":"octo/demo",
            "number":12,
            "merge_method":"squash",
            "match_head_commit":"abc1234"
        }))
        .await;
    let argv = fake.argv_at(1);
    assert!(argv.contains(&"--squash".to_owned()), "{argv:?}");
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--match-head-commit" && pair[1] == "abc1234"),
        "{argv:?}"
    );
    for forbidden in ["--admin", "--auto", "--delete-branch", "--disable-auto"] {
        assert!(!argv.contains(&forbidden.to_owned()), "{argv:?}");
    }
    assert_eq!(result["is_error"], false, "{result}");
    // gh exits 0 for a queued merge, so this must not claim completion.
    assert_eq!(result["outcome"], "accepted", "{result}");
    assert_eq!(result["merged"], false, "{result}");
    assert!(result["error"].as_str().unwrap().contains("not merged yet"));
    assert_eq!(result["head_guard"], "abc1234");
    assert_eq!(result["merge_method"], "squash");
}

#[tokio::test]
async fn a_verified_merge_is_reported_as_merged() {
    let fake = Fake::new();
    fake.sequence(&[
        Step {
            stdout: "https://github.com/octo/demo/pull/12\n".into(),
            stderr: String::new(),
            exit: 0,
        },
        Step {
            stdout: r#"{"number":12,"state":"MERGED","mergedAt":"2026-03-01T00:00:00Z","title":"t","url":"u"}"#.into(),
            stderr: String::new(),
            exit: 0,
        },
    ]);
    let result = fake
        .run(json!({"operation":"pr_merge","repo":"octo/demo","number":12}))
        .await;
    assert_eq!(result["outcome"], "merged", "{result}");
    assert_eq!(result["merged"], true);
    assert!(fake.argv_at(1).contains(&"--merge".to_owned()));
}

#[tokio::test]
async fn merge_refuses_a_body() {
    let fake = Fake::new();
    fake.emit("", "", 0);
    let error = super::execute_with(
        Some(&fake.program),
        json!({"operation":"pr_merge","number":1,"body":"commit text"}),
        fake.cwd().to_str().unwrap(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("does not take a body"),
        "{error:#}"
    );
}

#[tokio::test]
async fn unknown_operation_is_refused() {
    let fake = Fake::new();
    fake.emit("", "", 0);
    let error = super::execute_with(
        Some(&fake.program),
        json!({"operation":"repo_delete"}),
        fake.cwd().to_str().unwrap(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("Unsupported GitHub operation"));
    assert!(!fake.path("argv").exists(), "gh must not run");
}

#[tokio::test]
async fn repo_must_be_owner_slash_name() {
    let fake = Fake::new();
    fake.emit("", "", 0);
    for repo in [
        "../../etc",
        "octo",
        "octo/de mo",
        "octo/demo/extra",
        "-R/--repo",
        "octo/-x",
        "./demo",
    ] {
        let error = super::execute_with(
            Some(&fake.program),
            json!({"operation":"repo_view","repo":repo}),
            fake.cwd().to_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("repo must be owner/name"),
            "{repo}: {error:#}"
        );
    }
    assert!(!fake.path("argv").exists(), "gh must not run");
}
