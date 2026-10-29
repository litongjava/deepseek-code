//! Runs a Workflow script, as in the reference CLI: plain JavaScript that
//! starts with `export const meta = {...}` and orchestrates subagents with
//! agent(), parallel(), pipeline() and phase(). Each agent() is a child
//! `dcl -p` with the parent's provider, model and permission mode. Results
//! are cached per (prompt, options) so a later run can resume.
use anyhow::{bail, Context, Result};
use rquickjs::{prelude::{Async, Func}, AsyncContext, AsyncRuntime, CatchResultExt, Promise};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PRELUDE: &str = r#"
const args = JSON.parse(__ARGS);
async function agent(prompt, opts) {
  const reply = JSON.parse(await __agent(JSON.stringify({ prompt: String(prompt), opts: opts || {} })));
  if ('error' in reply) throw new Error(reply.error);
  return reply.ok;
}
function parallel(tasks) {
  return Promise.all(tasks.map(task => typeof task === 'function' ? task() : task));
}
function pipeline(items, ...stages) {
  return Promise.all(items.map(async (item, index) => {
    let value = item;
    for (const stage of stages) value = await stage(value, index);
    return value;
  }));
}
function phase(title) { __phase(String(title)); }
function log(...parts) { __progress(parts.map(String).join(' ')); }
"#;

/// The `meta` literal and the script body with `export` removed.
pub fn split_meta(script: &str) -> Result<(String, String)> {
    let trimmed = script.trim_start();
    let rest = trimmed.strip_prefix("export const meta").context("A workflow script must begin with `export const meta = {...}`")?;
    let open = rest.find('{').context("meta must be an object literal")?;
    anyhow::ensure!(rest[..open].trim() == "=", "A workflow script must begin with `export const meta = {{...}}`");
    let mut depth = 0;
    let mut end = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in rest[open..].char_indices() {
        if let Some(q) = quote {
            if escaped { escaped = false } else if ch == '\\' { escaped = true } else if ch == q { quote = None }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '{' => depth += 1,
            '}' => { depth -= 1; if depth == 0 { end = Some(open + index + 1); break; } }
            _ => {}
        }
    }
    let end = end.context("meta object is not closed")?;
    Ok((rest[open..end].to_owned(), rest[end..].to_owned()))
}

struct Runner {
    exe: PathBuf,
    agent_args: Vec<String>,
    cwd: PathBuf,
    run_dir: PathBuf,
    resume_dir: Option<PathBuf>,
    slots: tokio::sync::Semaphore,
}

/// Append one progress event to the run's progress.jsonl, read by /workflows.
pub fn record(run_dir: &Path, event: Value) {
    use std::io::Write;
    let mut event = event;
    event["at_ms"] = json!(chrono::Utc::now().timestamp_millis());
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(run_dir.join("progress.jsonl")) {
        let _ = writeln!(file, "{event}");
    }
}

fn cache_key(prompt: &str, opts: &Value) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("{prompt}\u{0}{opts}").as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Runner {
    async fn agent(&self, spec: &str) -> Result<String> {
        let spec: Value = serde_json::from_str(spec)?;
        let prompt = spec["prompt"].as_str().unwrap_or_default();
        anyhow::ensure!(!prompt.trim().is_empty(), "agent() needs a prompt");
        let opts = &spec["opts"];
        let label = opts["label"].as_str().unwrap_or_else(|| prompt.lines().next().unwrap_or_default());
        let key = cache_key(prompt, opts);
        let phase = opts["phase"].as_str().unwrap_or_default();
        let cached = self.run_dir.join("agents").join(format!("{key}.json"));
        for dir in self.resume_dir.iter().map(|dir| dir.join("agents")).chain(std::iter::once(self.run_dir.join("agents"))) {
            if let Ok(saved) = std::fs::read_to_string(dir.join(format!("{key}.json"))) {
                println!("\u{21bb} {label} (cached)");
                record(&self.run_dir, json!({"kind":"agent_cached","label":label,"phase":phase,"key":key}));
                if !cached.exists() { let _ = std::fs::write(&cached, &saved); }
                return Ok(saved);
            }
        }
        let _slot = self.slots.acquire().await?;
        println!("\u{25cf} {label}");
        record(&self.run_dir, json!({"kind":"agent_start","label":label,"phase":phase,"key":key}));
        let result = self.run_agent(prompt, opts, label).await;
        match &result {
            Ok(text) => {
                std::fs::write(&cached, text)?;
                println!("\u{2713} {label}");
                record(&self.run_dir, json!({"kind":"agent_done","label":label,"phase":phase,"key":key}));
            }
            Err(error) => record(&self.run_dir, json!({"kind":"agent_failed","label":label,"phase":phase,"key":key,"error":format!("{error:#}")})),
        }
        result
    }

    async fn run_agent(&self, prompt: &str, opts: &Value, label: &str) -> Result<String> {
        let mut args = self.agent_args.clone();
        if let Some(model) = opts["model"].as_str().filter(|m| !m.is_empty()) {
            match args.iter().position(|arg| arg == "--model") {
                Some(at) => args[at + 1] = model.to_owned(),
                None => args.extend(["--model".into(), model.into()]),
            }
        }
        args.extend(["-p".into(), "--output-format".into(), "json".into()]);
        if !opts["schema"].is_null() {
            args.extend(["--json-schema".into(), opts["schema"].to_string()]);
        }
        args.extend(["--".into(), prompt.into()]);
        let output = tokio::process::Command::new(&self.exe).args(&args).current_dir(&self.cwd)
            .env_remove(claude_core::services::background_sessions::CONTROL_ENV).env_remove(claude_core::services::background_sessions::WORKER_ENV)
            .stdin(std::process::Stdio::null()).kill_on_drop(true).output().await
            .with_context(|| format!("Cannot start agent {label}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let result: Value = serde_json::from_str(stdout.trim()).with_context(|| {
            // Show whatever the child printed: stderr, else the start of stdout.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stderr.trim().is_empty() { stdout.chars().take(500).collect::<String>() } else { stderr.into_owned() };
            format!("Agent {label} failed ({}): {}", output.status, detail.trim())
        })?;
        if result["is_error"] == true || !output.status.success() {
            let reason = [&result["result"], &result["error"], &result["subtype"]].into_iter().filter_map(Value::as_str).find(|text| !text.trim().is_empty()).map(str::to_owned)
                .unwrap_or_else(|| result.to_string().chars().take(500).collect());
            bail!("Agent {label} failed ({}): {reason}", output.status);
        }
        let value = if opts["schema"].is_null() { result["result"].clone() } else { result["structured_output"].clone() };
        Ok(value.to_string())
    }
}

pub struct Run<'a> {
    pub script: &'a str,
    pub args: Value,
    pub run_dir: &'a Path,
    pub resume_dir: Option<&'a Path>,
    pub cwd: &'a Path,
    pub agent_args: Vec<String>,
    pub concurrency: usize,
}

/// Runs the script and returns its result.
pub async fn run(run: Run<'_>) -> Result<Value> {
    let (meta, body) = split_meta(run.script)?;
    std::fs::create_dir_all(run.run_dir.join("agents"))?;
    let runner = Arc::new(Runner {
        exe: std::env::current_exe()?,
        agent_args: run.agent_args,
        cwd: run.cwd.to_path_buf(),
        run_dir: run.run_dir.to_path_buf(),
        resume_dir: run.resume_dir.map(Path::to_path_buf),
        slots: tokio::sync::Semaphore::new(run.concurrency.max(1)),
    });
    let runtime = AsyncRuntime::new()?;
    let context = AsyncContext::full(&runtime).await?;
    let source = format!("{PRELUDE}\nconst meta = {meta};\n(async () => {{\n{body}\n}})().then(result => JSON.stringify(result === undefined ? null : result))");
    let args_json = run.args.to_string();
    let result: Result<String> = context.async_with(async |ctx| {
        let globals = ctx.globals();
        let agent_runner = runner.clone();
        globals.set("__ARGS", args_json)?;
        globals.set("__agent", Func::from(Async(move |spec: String| {
            let runner = agent_runner.clone();
            async move {
                // Failures reach the script as exceptions thrown by agent().
                Ok::<_, rquickjs::Error>(match runner.agent(&spec).await {
                    Ok(text) => format!("{{\"ok\":{text}}}"),
                    Err(error) => json!({"error": format!("{error:#}")}).to_string(),
                })
            }
        })))?;
        let (log_dir, phase_dir) = (runner.run_dir.clone(), runner.run_dir.clone());
        globals.set("__progress", Func::from(move |text: String| { println!("{text}"); record(&log_dir, json!({"kind":"log","text":text})); }))?;
        globals.set("__phase", Func::from(move |title: String| { println!("Phase: {title}"); record(&phase_dir, json!({"kind":"phase","title":title})); }))?;
        let promise: Promise = ctx.eval(source).catch(&ctx).map_err(|error| anyhow::anyhow!("Workflow script error: {error}"))?;
        promise.into_future::<String>().await.catch(&ctx).map_err(|error| anyhow::anyhow!("Workflow failed: {error}"))
    }).await;
    runtime.idle().await;
    Ok(serde_json::from_str(&result?)?)
}
