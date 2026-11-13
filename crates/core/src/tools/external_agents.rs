//! Delegate a task to another coding agent installed on this machine:
//! deepseek-code (`dcl`), Claude Code (`claude`) or OpenAI Codex (`codex`). A
//! tool is offered only when its executable is on PATH; signing in to an agent
//! or configuring its provider is left to the user.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MAX_TIMEOUT_SECS: u64 = 3600;
/// Bytes of an agent's answer returned to the model.
const MAX_OUTPUT: usize = 60_000;

struct Agent {
    tool: &'static str,
    command: &'static str,
    name: &'static str,
    login: &'static str,
}

/// Set for a `dcl` started by this tool, so it does not offer the tool again.
const NESTED_ENV: &str = "DCL_EXTERNAL_AGENT";

const AGENTS: &[Agent] = &[
    Agent { tool: "DeepSeekCode", command: "dcl", name: "deepseek-code", login: "run `dcl` once in a terminal and configure a model provider, or pass its API key settings" },
    Agent { tool: "ClaudeCode", command: "claude", name: "Claude Code", login: "run `claude` once in a terminal and sign in, or set ANTHROPIC_API_KEY" },
    Agent { tool: "Codex", command: "codex", name: "OpenAI Codex", login: "run `codex login`, or configure a model provider in ~/.codex/config.toml" },
];

impl Agent {
    /// `dcl` and `claude` print one JSON result object. Codex prints JSONL
    /// lifecycle events instead.
    fn json_result(&self) -> bool {
        matches!(self.command, "dcl" | "claude")
    }
}

fn agent(tool: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|agent| agent.tool == tool)
}

pub fn is_external_agent(tool: &str) -> bool {
    agent(tool).is_some()
}

fn schema(agent: &Agent) -> Value {
    let name = agent.name;
    json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": format!("The complete task for {name}. It does not see this conversation, so include every file, constraint and expected result it needs.")},
            "mode": {"type": "string", "enum": ["read-only", "edit"], "default": "read-only", "description": "read-only: inspect and answer only. edit: may modify files in the project."},
            "model": {"type": "string", "description": "Optional model for the agent; its own default otherwise."},
            "timeout_secs": {"type": "integer", "minimum": 10, "maximum": MAX_TIMEOUT_SECS, "default": DEFAULT_TIMEOUT_SECS},
            "session_id": {"type": "string", "description": format!("Continue the {name} session with this session_id from an earlier result, keeping its context. Omit to start a new session.")}
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

/// Definitions for the agents installed on this machine.
pub(super) fn definitions() -> Vec<super::ToolDefinition> {
    AGENTS
        .iter()
        .filter(|agent| !(agent.command == "dcl" && std::env::var_os(NESTED_ENV).is_some()))
        .filter(|agent| super::lsp_servers::find_on_path(agent.command).is_some())
        .map(|agent| super::ToolDefinition {
            name: agent.tool.into(),
            description: format!(
                "Delegate a self-contained task to {} (the `{}` CLI installed on this machine) and return its final answer. Use it for a second opinion, a review, or work the user explicitly wants {} to do. It runs non-interactively in the project directory: read-only by default, mode=edit lets it change files. It cannot ask questions, so give it the full context. Each call starts a new {} session unless session_id continues an earlier one from a previous result. Requires approval.",
                agent.name, agent.command, agent.name, agent.name
            ),
            input_schema: schema(agent),
        })
        .collect()
}

/// The session id the caller asked to continue, normalized to the canonical
/// lowercase hyphenated UUID. The same canonical value reaches the agent and
/// is compared afterwards, so an equivalent spelling (uppercase, no hyphens,
/// braces or a `urn:uuid:` prefix) can never run a turn and then be reported
/// as a mismatch. Only a UUID is accepted, so an id can never be read as a
/// command-line option.
fn requested_session(agent: &Agent, input: &Value) -> Result<Option<String>> {
    let Some(session) = input["session_id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    let id = uuid::Uuid::parse_str(session).with_context(|| {
        format!(
            "session_id must be a {} session UUID, not {session}",
            agent.name
        )
    })?;
    Ok(Some(id.hyphenated().to_string()))
}

fn prompt(input: &Value) -> Result<&str> {
    input["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .context("prompt is required")
}

fn model(input: &Value) -> Option<String> {
    input["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
}

fn arguments(
    agent: &Agent,
    input: &Value,
    prompt: &str,
    cwd: &str,
    answer_file: &Path,
) -> Result<Vec<String>> {
    let edit = match input["mode"].as_str().unwrap_or("read-only") {
        "read-only" => false,
        "edit" => true,
        other => bail!("mode must be read-only or edit, not {other}"),
    };
    let session = requested_session(agent, input)?;
    let mut args: Vec<String> = Vec::new();
    match agent.command {
        "claude" | "dcl" => {
            // A headless agent denies anything that would need approval;
            // edit mode lets it change files but still not run commands.
            args.extend(["-p", "--output-format", "json", "--permission-mode"].map(String::from));
            args.push(if edit { "acceptEdits" } else { "default" }.into());
            if let Some(session) = &session {
                args.extend(["--resume".into(), session.clone()]);
            }
            if let Some(model) = model(input) {
                args.extend(["--model".into(), model]);
            }
            args.extend(["--".into(), prompt.into()]);
        }
        _ => {
            // Codex options belong to `exec` before its `resume` subcommand;
            // the prompt follows `--`, so a leading dash is never an option.
            args.extend(
                ["exec", "--json", "--skip-git-repo-check", "--sandbox"].map(String::from),
            );
            args.push(if edit { "workspace-write" } else { "read-only" }.into());
            args.extend([
                "--cd".into(),
                cwd.into(),
                "--output-last-message".into(),
                answer_file.to_string_lossy().into_owned(),
            ]);
            if let Some(model) = model(input) {
                args.extend(["--model".into(), model]);
            }
            if let Some(session) = &session {
                args.push("resume".into());
                args.push(session.clone());
            }
            args.extend(["--".into(), prompt.into()]);
        }
    }
    Ok(args)
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_owned();
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}

fn tail(text: &str, lines: usize) -> String {
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    rows[rows.len().saturating_sub(lines)..].join("\n")
}

/// The lifecycle events Codex `exec --json` emits that matter to the caller.
#[derive(Default, Debug)]
struct CodexEvents {
    thread_id: Option<String>,
    answer: Option<String>,
    completed: bool,
    failure: Option<String>,
    error: Option<String>,
}

/// Reads Codex's authoritative JSONL stream. Unknown event types and stdout
/// noise are ignored; a malformed event whose data is required for identity or
/// a terminal outcome fails clearly instead of being guessed at. A second,
/// different thread id is a conflict: the first id is kept and the stream is
/// rejected, so a later event can never overwrite it.
fn parse_codex_events(stdout: &str) -> Result<CodexEvents> {
    let mut events = CodexEvents::default();
    for (index, line) in stdout.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // A human warning on stdout is not a lifecycle event.
            continue;
        };
        let Some(kind) = value["type"].as_str() else {
            continue;
        };
        match kind {
            "thread.started" => {
                let thread = value["thread_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|thread| !thread.is_empty())
                    .with_context(|| {
                        format!(
                            "Codex reported a malformed thread.started event on line {}",
                            index + 1
                        )
                    })?;
                // Store one canonical spelling so a repeat of the same UUID is
                // accepted, while a different id is a protocol error. Keeping
                // the first id also means a requested id that appears last
                // cannot erase an earlier conflict.
                let canonical = uuid::Uuid::parse_str(thread)
                    .map(|id| id.hyphenated().to_string())
                    .unwrap_or_else(|_| thread.to_owned());
                match &events.thread_id {
                    None => events.thread_id = Some(canonical),
                    Some(first) if *first != canonical => {
                        bail!("Codex reported conflicting thread ids {first} and {canonical}")
                    }
                    Some(_) => {}
                }
            }
            "turn.completed" => {
                ensure!(
                    value["usage"].is_object(),
                    "Codex reported a malformed turn.completed event on line {}",
                    index + 1
                );
                events.completed = true;
            }
            "turn.failed" => {
                let message = value["error"]["message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .with_context(|| {
                        format!(
                            "Codex reported a malformed turn.failed event on line {}",
                            index + 1
                        )
                    })?;
                events.failure = Some(message.to_owned());
            }
            "item.completed" => {
                if value["item"]["type"] == "agent_message" {
                    let text = value["item"]["text"].as_str().with_context(|| {
                        format!(
                            "Codex reported a malformed agent_message item on line {}",
                            index + 1
                        )
                    })?;
                    events.answer = Some(text.to_owned());
                }
            }
            "error" => {
                if let Some(message) = value["message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                {
                    events.error = Some(message.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(events)
}

fn failure_detail(stdout: &str, stderr: &str) -> String {
    clip(&tail(&format!("{stdout}\n{stderr}"), 12))
}

/// The final answer Codex wrote with `--output-last-message`. It is only a
/// fallback for a completed turn whose JSONL carried no agent message. A
/// missing or empty file means the turn produced no text; an unreadable or
/// non-UTF-8 file is an error, never treated as an answer.
fn read_final_message(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Ok(Some(text.trim_end().to_owned())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            let detail = error.to_string();
            Err(error).with_context(|| {
                format!(
                    "Codex wrote its final message to {}, but the file could not be read: {detail}",
                    path.display()
                )
            })
        }
    }
}

/// Starts the agent with the arguments the tool exposes and waits for it.
/// Tests call this with a controlled fake executable; production resolves the
/// executable from PATH first.
async fn run_agent(
    agent: &Agent,
    program: &Path,
    input: &Value,
    cwd: &str,
    timeout: Duration,
) -> Result<Value> {
    let prompt = prompt(input)?;
    let session_requested = requested_session(agent, input)?;
    let answer_dir = tempfile::tempdir()?;
    let answer_file = answer_dir.path().join("answer.txt");
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments(agent, input, prompt, cwd, &answer_file)?)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if agent.command == "dcl" {
        command.env(NESTED_ENV, "1");
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let started = std::time::Instant::now();
    let child = command
        .spawn()
        .with_context(|| format!("Cannot start {}", program.display()))?;
    // Stops the agent and everything it started when the call is cancelled.
    let _tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("The agent has no process ID")?,
    )?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => bail!(
            "{} did not finish within {} seconds and was stopped",
            agent.name,
            timeout.as_secs()
        ),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut cost = None;
    let mut note: Option<&str> = None;
    let (answer, session, failed) = if agent.json_result() {
        let mut session = None;
        let (answer, failed) = match serde_json::from_str::<Value>(stdout.trim()) {
            Ok(result) => {
                cost = result["total_cost_usd"].as_f64();
                session = result["session_id"].as_str().map(str::to_owned);
                (
                    result["result"].as_str().unwrap_or_default().to_owned(),
                    result["is_error"] == true || !output.status.success(),
                )
            }
            Err(_) => (stdout.trim().to_owned(), !output.status.success()),
        };
        (answer, session, failed)
    } else {
        let events = parse_codex_events(&stdout)?;
        let thread = events.thread_id.with_context(|| {
            format!(
                "{} did not report a thread id; the session cannot be continued: {}",
                agent.name,
                failure_detail(&stdout, &stderr)
            )
        })?;
        let thread = uuid::Uuid::parse_str(&thread)
            .with_context(|| format!("{} reported an invalid thread id {thread}", agent.name))?
            .to_string();
        if let Some(requested) = session_requested.as_deref() {
            ensure!(
                thread.eq_ignore_ascii_case(requested),
                "{} resumed thread {thread}, not the requested session {requested}",
                agent.name
            );
        }
        if let Some(failure) = events.failure {
            bail!(
                "{} failed: {}\nIf this is a sign-in or API key problem, ask the user to fix it: {}.",
                agent.name,
                clip(&failure),
                agent.login
            );
        }
        ensure!(
            events.completed,
            "{} ended without a completed turn: {}",
            agent.name,
            events.error.map_or_else(|| failure_detail(&stdout, &stderr), |error| clip(&error))
        );
        ensure!(
            output.status.success(),
            "{} exited with {}: {}",
            agent.name,
            output.status,
            failure_detail(&stdout, &stderr)
        );
        // Prefer the streamed answer. A completed turn with no agent message
        // may still have written its final message to the file; only when both
        // are empty is the turn reported as successful without an answer, so a
        // legitimate tool-only turn is never turned into a failure.
        let (answer, note_text) = match events.answer.filter(|text| !text.trim().is_empty()) {
            Some(answer) => (answer, None),
            None => match read_final_message(&answer_file)? {
                Some(answer) => (answer, None),
                None => (
                    String::new(),
                    Some("Codex completed without a final message"),
                ),
            },
        };
        note = note_text;
        (answer, Some(thread), false)
    };
    if failed {
        let detail = if answer.is_empty() {
            failure_detail(&stdout, &stderr)
        } else {
            answer
        };
        bail!(
            "{} failed ({}):\n{}\nIf this is a sign-in or API key problem, ask the user to fix it: {}.",
            agent.name,
            output.status,
            clip(&detail),
            agent.login
        );
    }
    let mut result = json!({
        "agent": agent.name,
        "mode": input["mode"].as_str().unwrap_or("read-only"),
        "answer": clip(&answer),
        "elapsed_seconds": started.elapsed().as_secs(),
        "cost_usd": cost,
        "session_id": session,
    });
    if let Some(note) = note {
        result["note"] = json!(note);
    }
    Ok(result)
}

pub(super) async fn execute(tool: &str, input: Value, cwd: &str) -> Result<Value> {
    let agent = agent(tool).context("Unknown external agent")?;
    // Report a missing prompt before looking for the executable.
    prompt(&input)?;
    let program = super::lsp_servers::find_on_path(agent.command).with_context(|| {
        format!(
            "{} is not installed: `{}` was not found on PATH",
            agent.name, agent.command
        )
    })?;
    let timeout = input["timeout_secs"]
        .as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(10, MAX_TIMEOUT_SECS);
    run_agent(agent, &program, &input, cwd, Duration::from_secs(timeout)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex() -> &'static Agent {
        &AGENTS[2]
    }

    #[test]
    fn arguments_map_modes_and_keep_the_prompt_after_double_dash() {
        let file = Path::new("/tmp/answer.txt");
        let claude = arguments(
            &AGENTS[1],
            &json!({"mode":"edit","model":"sonnet"}),
            "--help me",
            "/p",
            file,
        )
        .unwrap();
        assert_eq!(
            claude,
            [
                "-p",
                "--output-format",
                "json",
                "--permission-mode",
                "acceptEdits",
                "--model",
                "sonnet",
                "--",
                "--help me"
            ]
        );
        let codex_args = arguments(codex(), &json!({}), "review", "/p", file).unwrap();
        assert_eq!(
            codex_args,
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "--cd",
                "/p",
                "--output-last-message",
                "/tmp/answer.txt",
                "--",
                "review"
            ]
        );
        assert!(arguments(codex(), &json!({"mode":"anything"}), "x", "/p", file).is_err());
    }

    #[test]
    fn session_ids_are_validated_and_resume_keeps_the_agent_options() {
        let file = Path::new("/tmp/answer.txt");
        let id = "0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695";
        for agent in AGENTS {
            let resumed = arguments(agent, &json!({"session_id":id,"mode":"edit"}), "continue", "/p", file)
                .unwrap();
            assert!(resumed.iter().any(|arg| arg == id), "{resumed:?}");
            assert!(schema(agent)["properties"]["session_id"].is_object());
            assert!(arguments(
                agent,
                &json!({"session_id":"--dangerously-skip-permissions"}),
                "x",
                "/p",
                file
            )
            .is_err());
        }
        // Codex keeps the sandbox, cwd and model on the resume path too.
        let resumed = arguments(
            codex(),
            &json!({"session_id":id,"mode":"edit","model":"gpt-6"}),
            "continue",
            "/p",
            file,
        )
        .unwrap();
        assert!(resumed.contains(&"resume".to_owned()), "{resumed:?}");
        assert!(resumed.contains(&"workspace-write".to_owned()), "{resumed:?}");
        assert!(resumed.contains(&"/p".to_owned()), "{resumed:?}");
        assert!(resumed.contains(&"gpt-6".to_owned()), "{resumed:?}");
    }

    /// Every equivalent UUID spelling is normalized before it reaches the
    /// agent, for all three agents, so the resumed thread and the requested
    /// session are the same canonical value.
    #[test]
    fn session_ids_reach_every_agent_as_one_canonical_uuid() {
        let file = Path::new("/tmp/answer.txt");
        for agent in AGENTS {
            for form in [
                SESSION.to_uppercase(),
                SESSION.replace('-', ""),
                format!("{{{SESSION}}}"),
                format!("urn:uuid:{SESSION}"),
            ] {
                let args =
                    arguments(agent, &json!({"session_id":form}), "again", "/p", file).unwrap();
                assert!(args.iter().any(|arg| arg == SESSION), "{form}: {args:?}");
                assert!(
                    !args.iter().any(|arg| arg == &form),
                    "{form}: the raw spelling reached the agent: {args:?}"
                );
            }
        }
    }

    /// A malformed id fails while the arguments are built, before any process
    /// starts, so a bad resume request never runs a turn.
    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_session_ids_fail_before_the_agent_starts() {
        for form in [
            "not-a-uuid",
            "--dangerously-skip-permissions",
            "0cc3f1f4-67cc-4d6d-aa91",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let fake = fake_agent(
                dir.path(),
                &events(&[
                    thread_started(SESSION),
                    agent_message("ran anyway"),
                    turn_completed(),
                ]),
                "",
                0,
            );
            let error = run_agent(
                codex(),
                &fake.program,
                &json!({"prompt":"continue","session_id":form}),
                dir.path().to_str().unwrap(),
                Duration::from_secs(10),
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(error.contains("session UUID"), "{form}: {error}");
            assert!(!fake.argv.exists(), "{form}: the agent was started");
        }
    }

    /// The process receives the canonical id and the reported thread matches
    /// it, so an equivalent spelling is a successful resume rather than a run
    /// that is then reported as a mismatch.
    #[cfg(unix)]
    #[tokio::test]
    async fn session_spellings_resume_the_same_thread_without_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_agent(
            dir.path(),
            &events(&[
                thread_started(SESSION),
                agent_message("again"),
                turn_completed(),
            ]),
            "",
            0,
        );
        for form in [
            SESSION.to_uppercase(),
            SESSION.replace('-', ""),
            format!("{{{SESSION}}}"),
            format!("urn:uuid:{SESSION}"),
        ] {
            let result = run_agent(
                codex(),
                &fake.program,
                &json!({"prompt":"again","session_id":form}),
                dir.path().to_str().unwrap(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            assert_eq!(result["session_id"], SESSION, "{form}");
            assert_eq!(result["answer"], "again", "{form}");
            let argv = recorded(&fake.argv);
            let resume = argv.iter().position(|arg| arg == "resume").unwrap();
            assert_eq!(argv[resume + 1], SESSION, "{form}: {argv:?}");
            assert!(!argv.iter().any(|arg| arg == &form), "{form}: {argv:?}");
        }
    }

    #[tokio::test]
    async fn missing_prompt_and_unknown_agents_are_rejected() {
        assert!(execute("Codex", json!({}), ".")
            .await
            .unwrap_err()
            .to_string()
            .contains("prompt"));
        assert!(execute("Other", json!({"prompt":"x"}), ".").await.is_err());
    }

    #[test]
    fn codex_parser_ignores_unknown_events_and_keeps_the_last_answer() {
        let stdout = [
            "not json at all",
            r#"{"type":"turn.started"}"#,
            r#"{"type":"future.event","anything":true}"#,
            r#"{"type":"thread.started","thread_id":"0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695"}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"first"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"final"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#,
        ]
        .join("\n");
        let events = parse_codex_events(&stdout).unwrap();
        assert_eq!(
            events.thread_id.as_deref(),
            Some("0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695")
        );
        assert_eq!(events.answer.as_deref(), Some("final"));
        assert!(events.completed);
        assert!(events.failure.is_none());
    }

    #[test]
    fn codex_parser_rejects_malformed_required_and_terminal_events() {
        for (stdout, expected) in [
            (r#"{"type":"thread.started"}"#, "malformed thread.started"),
            (
                r#"{"type":"thread.started","thread_id":""}"#,
                "malformed thread.started",
            ),
            (
                r#"{"type":"thread.started","thread_id":"0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695"}
{"type":"turn.failed"}"#,
                "malformed turn.failed",
            ),
            (
                r#"{"type":"thread.started","thread_id":"0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695"}
{"type":"turn.completed"}"#,
                "malformed turn.completed",
            ),
            (
                r#"{"type":"thread.started","thread_id":"0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695"}
{"type":"item.completed","item":{"id":"i","type":"agent_message"}}"#,
                "malformed agent_message",
            ),
            (
                r#"{"type":"thread.started","thread_id":"0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695"}
{"type":"thread.started","thread_id":"11111111-2222-3333-4444-555555555555"}"#,
                "conflicting thread ids",
            ),
        ] {
            assert!(
                parse_codex_events(stdout).unwrap_err().to_string().contains(expected),
                "{stdout}"
            );
        }
    }

    /// A repeated spelling of the same UUID is one identity, while a different
    /// id after the first is a conflict that names both.
    #[test]
    fn codex_parser_accepts_a_repeated_thread_and_rejects_a_conflict() {
        let repeated = [
            thread_started(SESSION),
            thread_started(&SESSION.to_uppercase()),
            agent_message("ok"),
            turn_completed(),
        ]
        .join("\n");
        let events = parse_codex_events(&repeated).unwrap();
        assert_eq!(events.thread_id.as_deref(), Some(SESSION));
        assert_eq!(events.answer.as_deref(), Some("ok"));

        let other = "11111111-2222-3333-4444-555555555555";
        let conflict = [thread_started(other), thread_started(SESSION)].join("\n");
        let error = parse_codex_events(&conflict).unwrap_err().to_string();
        assert!(error.contains("conflicting thread ids"), "{error}");
        assert!(error.contains(other) && error.contains(SESSION), "{error}");
    }

    const SESSION: &str = "0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695";

    fn thread_started(id: &str) -> String {
        json!({"type":"thread.started","thread_id":id}).to_string()
    }

    fn turn_completed() -> String {
        json!({"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0}})
            .to_string()
    }

    fn agent_message(text: &str) -> String {
        json!({"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":text}})
            .to_string()
    }

    fn events(lines: &[String]) -> String {
        format!("{}\n", lines.join("\n"))
    }

    #[cfg(unix)]
    struct FakeAgent {
        program: std::path::PathBuf,
        argv: std::path::PathBuf,
        cwd: std::path::PathBuf,
    }

    /// A controlled executable that records the argv and cwd it received and
    /// prints the given stdout/stderr, so tests assert the real process
    /// contract instead of only a formatter.
    #[cfg(unix)]
    fn fake_agent(dir: &Path, stdout: &str, stderr: &str, exit: i32) -> FakeAgent {
        use std::os::unix::fs::PermissionsExt;
        let stdout_path = dir.join("stdout.txt");
        let stderr_path = dir.join("stderr.txt");
        let argv = dir.join("argv.bin");
        let cwd = dir.join("cwd.txt");
        std::fs::write(&stdout_path, stdout).unwrap();
        std::fs::write(&stderr_path, stderr).unwrap();
        let program = dir.join("fake-agent");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\npwd > '{}'\ncat '{}'\ncat '{}' >&2\nexit {}\n",
                argv.display(),
                cwd.display(),
                stdout_path.display(),
                stderr_path.display(),
                exit
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        FakeAgent { program, argv, cwd }
    }

    /// A controlled executable like `fake_agent` that also honors
    /// `--output-last-message` by copying a fixed file to that path, which is
    /// how real Codex writes its final answer.
    #[cfg(unix)]
    fn fake_agent_writing_answer(
        dir: &Path,
        stdout: &str,
        stderr: &str,
        exit: i32,
        contents: &[u8],
    ) -> FakeAgent {
        use std::os::unix::fs::PermissionsExt;
        let stdout_path = dir.join("stdout.txt");
        let stderr_path = dir.join("stderr.txt");
        let argv = dir.join("argv.bin");
        let cwd = dir.join("cwd.txt");
        let source = dir.join("answer-source.bin");
        std::fs::write(&stdout_path, stdout).unwrap();
        std::fs::write(&stderr_path, stderr).unwrap();
        std::fs::write(&source, contents).unwrap();
        let program = dir.join("fake-agent");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\npwd > '{}'\nprev=''\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--output-last-message\" ]; then cat '{}' > \"$a\"; fi\n  prev=\"$a\"\ndone\ncat '{}'\ncat '{}' >&2\nexit {}\n",
                argv.display(),
                cwd.display(),
                source.display(),
                stdout_path.display(),
                stderr_path.display(),
                exit
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        FakeAgent { program, argv, cwd }
    }

    #[cfg(unix)]
    fn recorded(path: &Path) -> Vec<String> {
        std::fs::read(path)
            .unwrap()
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_create_records_cwd_argv_and_returns_the_reported_thread_id() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_agent(
            dir.path(),
            &events(&[
                thread_started(SESSION),
                agent_message("the answer"),
                turn_completed(),
            ]),
            "",
            0,
        );
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let result = run_agent(
            codex(),
            &fake.program,
            &json!({"prompt":"do the task"}),
            work.to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["answer"], "the answer");
        assert_eq!(result["mode"], "read-only");
        assert_eq!(
            std::fs::read_to_string(&fake.cwd).unwrap().trim(),
            work.to_str().unwrap()
        );
        let argv = recorded(&fake.argv);
        assert_eq!(
            argv[..5].iter().map(String::as_str).collect::<Vec<_>>(),
            ["exec", "--json", "--skip-git-repo-check", "--sandbox", "read-only"]
        );
        assert!(argv.contains(&"--cd".to_owned()), "{argv:?}");
        assert!(argv.contains(&work.to_string_lossy().into_owned()), "{argv:?}");
        assert!(!argv.contains(&"resume".to_owned()), "{argv:?}");
        assert_eq!(argv.last().unwrap().as_str(), "do the task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_resume_passes_the_exact_session_and_prompt_safely() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_agent(
            dir.path(),
            &events(&[thread_started(SESSION), agent_message("again"), turn_completed()]),
            "",
            0,
        );
        let prompt = "--help\n\"quoted\"\nline 3";
        let result = run_agent(
            codex(),
            &fake.program,
            &json!({"prompt":prompt,"session_id":SESSION,"mode":"edit","model":"gpt-6"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["answer"], "again");
        let argv = recorded(&fake.argv);
        let resume = argv.iter().position(|arg| arg == "resume").unwrap();
        assert_eq!(argv[resume + 1], SESSION);
        assert!(argv.contains(&"workspace-write".to_owned()), "{argv:?}");
        assert!(argv.contains(&"gpt-6".to_owned()), "{argv:?}");
        assert_eq!(argv.last().unwrap().as_str(), prompt);
        assert_eq!(argv[argv.len() - 2], "--");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_mismatched_resume_identity_fails() {
        let dir = tempfile::tempdir().unwrap();
        let other = "11111111-2222-3333-4444-555555555555";
        let fake = fake_agent(
            dir.path(),
            &events(&[thread_started(other), agent_message("wrong"), turn_completed()]),
            "",
            0,
        );
        let error = run_agent(
            codex(),
            &fake.program,
            &json!({"prompt":"continue","session_id":SESSION}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("not the requested session"), "{error}");
        assert!(error.contains(other) && error.contains(SESSION), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_conflicting_thread_ids_fail_create_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let other = "11111111-2222-3333-4444-555555555555";
        let conflicting = events(&[
            thread_started(other),
            thread_started(SESSION),
            agent_message("ambiguous"),
            turn_completed(),
        ]);
        // A new turn with two identities is rejected.
        let create = fake_agent(dir.path(), &conflicting, "", 0);
        let error = run_agent(
            codex(),
            &create.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("conflicting thread ids"), "{error}");

        // On resume the requested id appears last; it must not erase the
        // earlier conflict and turn the stream into an accepted resume.
        let resume = fake_agent(dir.path(), &conflicting, "", 0);
        let error = run_agent(
            codex(),
            &resume.program,
            &json!({"prompt":"continue","session_id":SESSION}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("conflicting thread ids"), "{error}");
        assert!(!error.contains("not the requested session"), "{error}");

        // Repeating the same UUID in another spelling stays one identity.
        let repeated = fake_agent(
            dir.path(),
            &events(&[
                thread_started(SESSION),
                thread_started(&SESSION.to_uppercase()),
                agent_message("ok"),
                turn_completed(),
            ]),
            "",
            0,
        );
        let result = run_agent(
            codex(),
            &repeated.program,
            &json!({"prompt":"continue","session_id":SESSION}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["answer"], "ok");
    }

    /// The streamed answer wins; the file is only read when the stream had no
    /// agent message, and an empty stream plus empty or missing file is a
    /// successful turn with an explicit note instead of a failure.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_prefers_the_streamed_answer_and_falls_back_to_the_file() {
        let dir = tempfile::tempdir().unwrap();

        let streamed = fake_agent_writing_answer(
            dir.path(),
            &events(&[
                thread_started(SESSION),
                agent_message("from stream"),
                turn_completed(),
            ]),
            "",
            0,
            b"from file",
        );
        let result = run_agent(
            codex(),
            &streamed.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result["answer"], "from stream");
        assert!(result.get("note").is_none(), "{result}");

        let file_only = fake_agent_writing_answer(
            dir.path(),
            &events(&[thread_started(SESSION), turn_completed()]),
            "",
            0,
            b"from file\n",
        );
        let result = run_agent(
            codex(),
            &file_only.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result["answer"], "from file");
        assert!(result.get("note").is_none(), "{result}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_completed_turn_without_any_message_succeeds_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        for contents in [b"".as_slice(), b"  \n".as_slice()] {
            let empty_file = fake_agent_writing_answer(
                dir.path(),
                &events(&[thread_started(SESSION), turn_completed()]),
                "",
                0,
                contents,
            );
            let result = run_agent(
                codex(),
                &empty_file.program,
                &json!({"prompt":"work"}),
                dir.path().to_str().unwrap(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            assert_eq!(result["answer"], "", "{result}");
            assert_eq!(
                result["note"], "Codex completed without a final message",
                "{result}"
            );
        }

        // No file was written at all, which is also a legitimate tool-only turn.
        let no_file = fake_agent(
            dir.path(),
            &events(&[thread_started(SESSION), turn_completed()]),
            "",
            0,
        );
        let result = run_agent(
            codex(),
            &no_file.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result["answer"], "", "{result}");
        assert_eq!(
            result["note"], "Codex completed without a final message",
            "{result}"
        );
    }

    /// A failed or incomplete turn stays failed even when an answer file
    /// exists, and an answer file that cannot be read is reported rather than
    /// presented as an answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_answer_file_never_hides_failure_or_a_read_error() {
        let dir = tempfile::tempdir().unwrap();

        let nonzero = fake_agent_writing_answer(
            dir.path(),
            &events(&[
                thread_started(SESSION),
                agent_message("partial"),
                turn_completed(),
            ]),
            "backend crashed",
            3,
            b"answer despite failure",
        );
        let error = run_agent(
            codex(),
            &nonzero.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("exited with"), "{error}");
        assert!(!error.contains("answer despite failure"), "{error}");

        let incomplete = fake_agent_writing_answer(
            dir.path(),
            &events(&[thread_started(SESSION), agent_message("partial")]),
            "",
            0,
            b"answer without completion",
        );
        let error = run_agent(
            codex(),
            &incomplete.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("without a completed turn"), "{error}");

        let invalid_utf8 = fake_agent_writing_answer(
            dir.path(),
            &events(&[thread_started(SESSION), turn_completed()]),
            "",
            0,
            b"\xff\xfe not utf-8",
        );
        let error = run_agent(
            codex(),
            &invalid_utf8.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("could not be read"), "{error}");
        assert!(error.contains("UTF-8"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_turn_failed_and_nonzero_exit_fail() {
        let dir = tempfile::tempdir().unwrap();
        let failed = fake_agent(
            dir.path(),
            &events(&[
                thread_started(SESSION),
                json!({"type":"turn.failed","error":{"message":"model unavailable"}}).to_string(),
            ]),
            "",
            1,
        );
        let error = run_agent(
            codex(),
            &failed.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("model unavailable"), "{error}");

        let nonzero = fake_agent(
            dir.path(),
            &events(&[thread_started(SESSION), agent_message("partial"), turn_completed()]),
            "backend crashed",
            3,
        );
        let error = run_agent(
            codex(),
            &nonzero.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("exited with"), "{error}");
        assert!(error.contains("backend crashed"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_missing_thread_or_terminal_event_fails_clearly() {
        let dir = tempfile::tempdir().unwrap();
        // A nonexistent session: Codex exits nonzero before any thread.started.
        let missing = fake_agent(
            dir.path(),
            "",
            "thread/resume failed: no rollout found for thread id 0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695",
            1,
        );
        let error = run_agent(
            codex(),
            &missing.program,
            &json!({"prompt":"continue","session_id":SESSION}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not report a thread id"), "{error}");
        assert!(error.contains("no rollout found"), "{error}");

        // Exit 0 is not success without turn.completed.
        let incomplete = fake_agent(
            dir.path(),
            &events(&[thread_started(SESSION), agent_message("partial")]),
            "",
            0,
        );
        let error = run_agent(
            codex(),
            &incomplete.program,
            &json!({"prompt":"work"}),
            dir.path().to_str().unwrap(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("without a completed turn"), "{error}");
    }

    #[cfg(target_os = "linux")]
    async fn wait_until_gone(pid: u32) -> bool {
        for _ in 0..100 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[cfg(target_os = "linux")]
    fn slow_agent(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let child = dir.join("child.pid");
        let program = dir.join("fake-agent");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                child.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (program, child)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_timeout_stops_the_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (program, child) = slow_agent(dir.path());
        let error = run_agent(
            codex(),
            &program,
            &json!({"prompt":"slow"}),
            dir.path().to_str().unwrap(),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not finish"), "{error}");
        let pid: u32 = std::fs::read_to_string(&child)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(wait_until_gone(pid).await, "child {pid} survived the timeout");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_cancellation_stops_the_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (program, child) = slow_agent(dir.path());
        let cwd = dir.path().to_string_lossy().into_owned();
        let handle = tokio::spawn(async move {
            run_agent(
                codex(),
                &program,
                &json!({"prompt":"slow"}),
                &cwd,
                Duration::from_secs(30),
            )
            .await
        });
        for _ in 0..100 {
            if child.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(child.exists(), "the fake agent did not start");
        handle.abort();
        let _ = handle.await;
        let pid: u32 = std::fs::read_to_string(&child)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_until_gone(pid).await,
            "child {pid} survived cancellation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_and_dcl_keep_their_json_result_and_resume_arguments() {
        for agent in [&AGENTS[0], &AGENTS[1]] {
            let dir = tempfile::tempdir().unwrap();
            let stdout = json!({
                "result":"agent answer",
                "session_id":SESSION,
                "is_error":false,
                "total_cost_usd":0.25
            })
            .to_string();
            let fake = fake_agent(dir.path(), &stdout, "", 0);
            let created = run_agent(
                agent,
                &fake.program,
                &json!({"prompt":"start"}),
                dir.path().to_str().unwrap(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            assert_eq!(created["answer"], "agent answer");
            assert_eq!(created["session_id"], SESSION);
            assert_eq!(created["cost_usd"], 0.25);
            let argv = recorded(&fake.argv);
            assert_eq!(
                argv[..3].iter().map(String::as_str).collect::<Vec<_>>(),
                ["-p", "--output-format", "json"]
            );
            assert_eq!(argv.last().unwrap().as_str(), "start");

            let resumed = run_agent(
                agent,
                &fake.program,
                &json!({"prompt":"again","session_id":SESSION}),
                dir.path().to_str().unwrap(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            assert_eq!(resumed["answer"], "agent answer");
            let argv = recorded(&fake.argv);
            assert!(
                argv.windows(2)
                    .any(|pair| pair[0] == "--resume" && pair[1] == SESSION),
                "{argv:?}"
            );
            assert_eq!(argv.last().unwrap().as_str(), "again");
        }
    }
}
