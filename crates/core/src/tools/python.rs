//! The Python tool: write a source file and run it with a selected interpreter,
//! without a shell between the tool and the process.
use crate::services::python_settings;
use crate::tools::background_shell::{self, Interaction};
use crate::tools::{sandbox, sync_runtime};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::{Duration, SystemTime},
};
use tokio::process::Command;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_ENV_KEYS: usize = 32;
const MAX_ENV_BYTES: usize = 16 * 1024;
const MAX_SAVE_COMPONENTS: usize = 9;
const MAX_SAVE_PATH: usize = 200;
const TEMP_PREFIX: &str = "dcl-python-";
const DEFAULT_DESCRIPTION: &str = "Run Python";

pub(crate) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        summary: Some("Run Python code as __main__ with the workspace interpreter, without a shell. Prefer this tool over a Bash heredoc or `python -c` for pure Python snippets".into()),
        name: "Python".into(),
        description: "Run Python code, prefer this tool over a Bash heredoc or `python -c` for pure Python snippets and numeric experiments, and use Bash when the task needs shell syntax, script arguments, or `python -m`. This is Python, not a shell: pipes, redirection, `$VAR`, `&&` and globs are not interpreted. The code runs from a file, so `__file__` is set and `input()` returns EOF. Set `save_name` to keep the script under the workspace `temp/` directory and return its path as `script_path`; without it the script is temporary and deleted after the call. `env` is merged over the inherited environment before the interpreter starts; `PYTHONPATH` is merged rather than replaced, and the working directory stays importable. Long jobs can use `run_in_background` and are read with `TaskGet`.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "code": {
                    "description": "Python source to run as __main__.  Python only: shell syntax such as pipes, redirection, $VAR and && is not interpreted. Use Bash for `python -m ...`, scripts with arguments, or untrusted code. The code runs from a file, so __file__ is set; stdin is empty, so input() returns EOF.",
                    "type": "string"
                },
                "save_name": {
                    "description": "Optional. Save the script under <workspace>/temp/<save_name> instead of a temporary file, and return the absolute path as script_path. Must be a relative .py path without .., for example python/script/001.py. An existing file is overwritten. Omit to run from a temporary file that is deleted after the call.",
                    "type": "string"
                },
                "env": {
                    "description": "Optional environment variables merged over the inherited environment for this run. Use it for values that must exist before the interpreter starts, such as PYTHONPATH, PYTHONWARNINGS or PYTHONUNBUFFERED. Do not put secrets here: tool inputs are stored in the session transcript. At most 32 keys and 16 KiB.",
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                },
                "interpreter": {
                    "description": "Optional. Absolute path to a python executable, or a program name on PATH. Mutually exclusive with conda; omit both to use the workspace interpreter.",
                    "type": "string"
                },
                "conda": {
                    "description": "Optional. Conda environment name; its python executable is used. Mutually exclusive with interpreter. Discover names with `conda env list` in Bash.",
                    "type": "string"
                },
                "cwd": {
                    "description": "Optional. Working directory; defaults to the session workspace. Must be inside the workspace or an allowed path. A relative path resolves against the session workspace.",
                    "type": "string"
                },
                "timeout": {
                    "description": "Foreground wait in milliseconds, 1-600000 (default 120000). With run_in_background, an optional task timeout.",
                    "type": "number",
                    "minimum": 1,
                    "maximum": 600000
                },
                "run_in_background": {
                    "description": "Return a task ID immediately; read output with TaskGet and stop it with TaskStop (default false).",
                    "type": "boolean"
                },
                "description": {
                    "description": "Short label for the user, such as \"Inspect session table\".",
                    "type": "string"
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedInterpreter {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub source: &'static str,
}

pub(crate) struct ResolveOptions<'a> {
    pub workspace: &'a Path,
    pub interpreter: Option<&'a str>,
    pub conda: Option<&'a str>,
    pub settings_path: Option<&'a str>,
}

/// Resolve in the fixed order: parameter, conda, settings, VIRTUAL_ENV,
/// CONDA_PREFIX, workspace virtualenvs, then PATH.
pub(crate) fn resolve_interpreter(options: ResolveOptions<'_>) -> Result<ResolvedInterpreter> {
    if let Some(interpreter) = options.interpreter {
        return resolve_program(interpreter, "parameter");
    }
    if let Some(conda) = options.conda {
        return resolve_conda(conda);
    }
    if let Some(path) = options.settings_path {
        return resolve_program(path, "settings").context("Cannot use the configured python.path");
    }
    if let Some(interpreter) = resolve_prefix_env("VIRTUAL_ENV", "VIRTUAL_ENV") {
        return Ok(interpreter);
    }
    if let Some(interpreter) = resolve_prefix_env("CONDA_PREFIX", "CONDA_PREFIX") {
        return Ok(interpreter);
    }
    for name in [".venv", "venv", "env"] {
        if let Some(program) = python_in_prefix(&options.workspace.join(name)) {
            return Ok(ResolvedInterpreter {
                program,
                args: Vec::new(),
                source: "venv",
            });
        }
    }
    if cfg!(windows) {
        if let Some(program) = find_on_path("py") {
            return Ok(ResolvedInterpreter {
                program,
                args: vec!["-3".into()],
                source: "PATH",
            });
        }
    }
    for name in ["python3", "python"] {
        if let Some(program) = find_on_path(name) {
            return Ok(ResolvedInterpreter {
                program,
                args: Vec::new(),
                source: "PATH",
            });
        }
    }
    bail!(
        "Python interpreter not found. Tried settings.python.path, VIRTUAL_ENV, CONDA_PREFIX, {}, {}, {}, py -3, python3 and python. Use Bash to run `py -0p`, `conda env list`, or `which python`.",
        options.workspace.join(".venv").display(),
        options.workspace.join("venv").display(),
        options.workspace.join("env").display()
    )
}

fn resolve_program(value: &str, source: &'static str) -> Result<ResolvedInterpreter> {
    let value = value.trim();
    ensure!(!value.is_empty(), "Python interpreter cannot be empty.");
    let path = Path::new(value);
    let program = if path.is_absolute() {
        ensure!(path.is_file(), "Python interpreter not found: {value}");
        path.to_path_buf()
    } else {
        ensure!(
            !value.contains(['/', '\\']),
            "Python interpreter must be an absolute path or a program name: {value}"
        );
        find_on_path(value).with_context(|| {
            format!(
                "Python interpreter `{value}` was not found on PATH. Use Bash to run `py -0p`, `conda env list`, or `which python`."
            )
        })?
    };
    Ok(ResolvedInterpreter {
        program,
        args: launcher_args(path),
        source,
    })
}

fn launcher_args(program: &Path) -> Vec<String> {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(name.as_str(), "py" | "py.exe") {
        vec!["-3".into()]
    } else {
        Vec::new()
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    crate::tools::lsp_servers::find_on_path(name)
        .filter(|path| path.is_file())
        .filter(|path| !is_windows_store_alias(path))
}

fn is_windows_store_alias(path: &Path) -> bool {
    path.to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
        .contains("\\windowsapps\\")
}

fn python_in_prefix(prefix: &Path) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        vec![
            prefix.join("python.exe"),
            prefix.join("Scripts").join("python.exe"),
        ]
    } else {
        vec![prefix.join("bin").join("python"), prefix.join("python")]
    };
    candidates.into_iter().find(|path| path.is_file())
}

fn resolve_prefix_env(variable: &str, source: &'static str) -> Option<ResolvedInterpreter> {
    resolve_prefix_value(std::env::var_os(variable), source)
}

fn resolve_prefix_value(
    value: Option<OsString>,
    source: &'static str,
) -> Option<ResolvedInterpreter> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(ResolvedInterpreter {
        program: python_in_prefix(&PathBuf::from(value))?,
        args: Vec::new(),
        source,
    })
}

struct CondaCache {
    conda: PathBuf,
    config_mtime: SystemTime,
    envs: Vec<(String, PathBuf)>,
}

static CONDA_CACHE: Mutex<Option<CondaCache>> = Mutex::new(None);

fn resolve_conda(name: &str) -> Result<ResolvedInterpreter> {
    let name = name.trim();
    ensure!(!name.is_empty(), "Python conda must name an environment.");
    let conda = find_on_path("conda").context(
        "Python conda was requested but conda was not found on PATH. Use Bash to run `conda env list`, or omit conda to use the workspace interpreter.",
    )?;
    let envs = conda_envs(&conda)?;
    let Some((_, prefix)) = envs.iter().find(|(env, _)| env == name) else {
        bail!("Conda environment not found: {name}. Discover names with `conda env list` in Bash.")
    };
    let program = python_in_prefix(prefix).with_context(|| {
        format!(
            "Conda environment `{name}` has no python executable under {}",
            prefix.display()
        )
    })?;
    Ok(ResolvedInterpreter {
        program,
        args: Vec::new(),
        source: "conda",
    })
}

fn conda_envs(conda: &Path) -> Result<Vec<(String, PathBuf)>> {
    let config = std::env::var_os("CONDARC")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".condarc")));
    let config_mtime = config
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok());
    if let Some(config_mtime) = config_mtime {
        let cached = CONDA_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = cached.as_ref() {
            if cache.conda == conda && cache.config_mtime == config_mtime {
                return Ok(cache.envs.clone());
            }
        }
    }
    let output = std::process::Command::new(conda)
        .args(["env", "list", "--json"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("Cannot run {}", conda.display()))?;
    ensure!(
        output.status.success(),
        "`conda env list --json` failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let value: Value = serde_json::from_slice(&output.stdout)
        .context("`conda env list --json` did not return JSON")?;
    let envs: Vec<(String, PathBuf)> = value["envs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .filter_map(|prefix| {
            let name = prefix.file_name()?.to_string_lossy().into_owned();
            Some((name, prefix))
        })
        .collect();
    if let Some(config_mtime) = config_mtime {
        *CONDA_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(CondaCache {
            conda: conda.to_path_buf(),
            config_mtime,
            envs: envs.clone(),
        });
    }
    Ok(envs)
}

/// The default interpreter line for the session environment block.
/// The interpreter chosen when the session environment was frozen. The tool
/// compares later resolutions against it, so a default `.venv` does not add an
/// `interpreter:` line to every call, while a `.venv` created mid-session does.
static DEFAULT_INTERPRETERS: Mutex<Option<HashMap<PathBuf, Option<PathBuf>>>> = Mutex::new(None);

fn workspace_key(workspace: &Path) -> PathBuf {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
}

fn remember_default(workspace: &Path, interpreter: Option<PathBuf>) {
    let mut defaults = DEFAULT_INTERPRETERS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    defaults
        .get_or_insert_with(HashMap::new)
        .insert(workspace_key(workspace), interpreter);
}

fn default_snapshot(workspace: &Path) -> Option<Option<PathBuf>> {
    DEFAULT_INTERPRETERS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .and_then(|defaults| defaults.get(&workspace_key(workspace)).cloned())
}

fn same_program(requested: &str, resolved: &Path) -> bool {
    let requested = Path::new(requested.trim());
    requested.is_absolute() && path_key(requested) == path_key(resolved)
}

pub(crate) fn environment_line(workspace: Option<&Path>) -> Option<String> {
    let workspace = workspace.unwrap_or_else(|| Path::new("."));
    let settings_path = crate::runtime::resolve_default_data_dir()
        .ok()
        .and_then(|root| python_settings::load(&root).ok())
        .and_then(|settings| settings.path);
    let interpreter = match resolve_interpreter(ResolveOptions {
        workspace,
        interpreter: None,
        conda: None,
        settings_path: settings_path.as_deref(),
    }) {
        Ok(interpreter) => interpreter,
        Err(_) => {
            remember_default(workspace, None);
            return None;
        }
    };
    remember_default(workspace, Some(interpreter.program.clone()));
    let version = version_of(&interpreter);
    let path = interpreter.program.to_string_lossy();
    Some(match version {
        Some(version) => format!(" - Python: {path} ({version})"),
        None => format!(" - Python: {path}"),
    })
}

fn version_of(interpreter: &ResolvedInterpreter) -> Option<String> {
    let mut command = std::process::Command::new(&interpreter.program);
    command
        .args(&interpreter.args)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    let line = text.lines().next()?.trim();
    let version = line.strip_prefix("Python ").unwrap_or(line).trim();
    (!version.is_empty()).then(|| version.to_owned())
}

pub(crate) fn execute_blocking(input: Value, cwd: &str) -> Result<Value> {
    let cwd = cwd.to_owned();
    sync_runtime::run(async move { execute(input, &cwd).await })
}

pub(crate) async fn execute(input: Value, cwd: &str) -> Result<Value> {
    sweep_old_temp_scripts();
    let code = input["code"]
        .as_str()
        .filter(|code| !code.trim().is_empty())
        .context("Python requires code.")?;
    let save_name = parse_save_name(&input)?;
    let env = parse_env(&input)?;
    let interpreter_arg = optional_string(
        &input,
        "interpreter",
        "Python interpreter must be a string.",
    )?;
    let conda_arg = optional_string(&input, "conda", "Python conda must be a string.")?;
    ensure!(
        interpreter_arg.is_none() || conda_arg.is_none(),
        "Pass at most one of interpreter and conda; omit both to use the workspace interpreter."
    );
    let background = input["run_in_background"].as_bool().unwrap_or(false);
    let timeout = parse_timeout(&input, background)?;

    let workspace = Path::new(cwd);
    ensure!(
        workspace.is_dir(),
        "Python working directory does not exist: {cwd}"
    );
    let workdir = resolve_workdir(&input, workspace, cwd)?;
    let settings = python_settings::load(&background_shell::settings_root()?)?;
    let interpreter = resolve_interpreter(ResolveOptions {
        workspace,
        interpreter: interpreter_arg,
        conda: conda_arg,
        settings_path: settings.path.as_deref(),
    })?;
    let interpreter_changed = if let Some(requested) = interpreter_arg.or(conda_arg) {
        !same_program(requested, &interpreter.program)
    } else {
        match default_snapshot(workspace) {
            Some(Some(default)) => path_key(&default) != path_key(&interpreter.program),
            Some(None) => true,
            None => false,
        }
    };

    let saved = save_name.is_some();
    let script = match save_name {
        Some(ref name) => saved_script_path(workspace, name)?,
        None => std::env::temp_dir().join(format!("{TEMP_PREFIX}{}.py", uuid::Uuid::new_v4())),
    };
    let script_arg = display_script_path(&script);
    let command = build_command(&input, &interpreter, &script_arg, &workdir, &env)?;
    if let Err(error) = write_script(&script, code, saved) {
        if !saved {
            let _ = std::fs::remove_file(&script);
        }
        return Err(error);
    }

    // Keep the same precheck position as Bash. A synthetic interpreter path
    // cannot match a command rule; Python(*) and the sandbox are the real gate.
    let synthetic = format!("{} {}", interpreter.program.display(), script_arg.display());
    if !super::path_scope::bypass_permissions()
        && !super::path_scope::approved_auto_command(&synthetic)
        && super::is_dangerous_command(&synthetic)
    {
        if !saved {
            let _ = std::fs::remove_file(&script);
        }
        return Ok(json!({
            "error": "Command blocked by security filter: dangerous command detected",
            "is_error": true,
            "blocked": true
        }));
    }

    let description = input["description"]
        .as_str()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .unwrap_or(DEFAULT_DESCRIPTION);
    let display = input["description"]
        .as_str()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            save_name
                .as_ref()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| {
            script
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "python".into())
        });
    // Task metadata keeps the display name, never the source: logs must not
    // grow with the model's code.
    let result = if background {
        background_shell::start(
            command,
            timeout,
            "python",
            description,
            &display,
            None,
            Interaction::None,
        )
    } else {
        background_shell::run_foreground(
            command,
            timeout.unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS)),
            "python",
            description,
            &display,
            None,
        )
        .await
    };
    let mut result = match result {
        Ok(result) => result,
        Err(error) => {
            if !saved {
                let _ = std::fs::remove_file(&script);
            }
            return Err(error);
        }
    };
    result["interpreter"] = json!(interpreter.program.to_string_lossy());
    result["interpreter_source"] = json!(interpreter.source);
    result["interpreter_changed"] = json!(interpreter_changed);
    if let Some(requested) = interpreter_arg.or(conda_arg) {
        result["interpreter_requested"] = json!(requested.trim());
    }
    result["script_saved"] = json!(saved);
    if saved {
        let path = display_script_path(&script.canonicalize().unwrap_or(script.clone()));
        result["script_path"] = json!(path.to_string_lossy());
    }
    if !saved {
        let still_running = background
            || result["backgrounded"] == true
            || (result["status"] == "running" && result["task_id"].is_string());
        if !still_running {
            let _ = std::fs::remove_file(&script);
        }
    }
    Ok(result)
}

fn optional_string<'a>(input: &'a Value, key: &str, message: &str) -> Result<Option<&'a str>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => bail!("{message}"),
    }
}

fn parse_timeout(input: &Value, background: bool) -> Result<Option<Duration>> {
    match input.get("timeout").filter(|value| !value.is_null()) {
        None if background => Ok(None),
        None => Ok(Some(Duration::from_millis(DEFAULT_TIMEOUT_MS))),
        Some(value) => {
            let millis = value
                .as_u64()
                .filter(|millis| (1..=MAX_TIMEOUT_MS).contains(millis))
                .ok_or_else(|| {
                    anyhow::anyhow!("Python timeout must be between 1 and 600000 milliseconds.")
                })?;
            Ok(Some(Duration::from_millis(millis)))
        }
    }
}

fn resolve_workdir(input: &Value, workspace: &Path, cwd: &str) -> Result<PathBuf> {
    let Some(value) = optional_string(input, "cwd", "Python cwd must be a string.")? else {
        return Ok(workspace.to_path_buf());
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(workspace.to_path_buf());
    }
    let path = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        workspace.join(value)
    };
    ensure!(
        path.is_dir(),
        "Python cwd does not exist: {}",
        path.display()
    );
    ensure!(
        super::is_path_allowed(&path, cwd),
        "Python cwd is outside the allowed paths: {}",
        path.display()
    );
    Ok(path)
}

fn parse_env(input: &Value) -> Result<Vec<(String, String)>> {
    let Some(value) = input.get("env").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let Some(object) = value.as_object() else {
        bail!("Python env must be an object of string values.");
    };
    ensure!(
        object.len() <= MAX_ENV_KEYS,
        "Python env allows at most 32 variables and 16 KiB."
    );
    let mut total = 0usize;
    let mut env: Vec<(String, String)> = Vec::new();
    for (key, value) in object {
        let Some(value) = value.as_str() else {
            bail!("Python env must be an object of string values.");
        };
        ensure!(
            valid_env_key(key),
            "Python env key must match [A-Za-z_][A-Za-z0-9_]*: {key}"
        );
        ensure!(
            !reserved_env(key),
            "Python env cannot set host-reserved variable: {key}"
        );
        total = total.saturating_add(key.len()).saturating_add(value.len());
        ensure!(
            total <= MAX_ENV_BYTES,
            "Python env allows at most 32 variables and 16 KiB."
        );
        if cfg!(windows) {
            if let Some(existing) = env
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
            {
                existing.0 = key.clone();
                existing.1 = value.to_owned();
                continue;
            }
        }
        env.push((key.clone(), value.to_owned()));
    }
    Ok(env)
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn reserved_env(key: &str) -> bool {
    crate::services::background_sessions::INTERNAL_ENV
        .iter()
        .any(|name| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case(key)
            } else {
                *name == key
            }
        })
}

fn parse_save_name(input: &Value) -> Result<Option<PathBuf>> {
    let Some(name) = optional_string(input, "save_name", "Python save_name must be a string.")?
    else {
        return Ok(None);
    };
    let name = name.trim();
    ensure!(
        !name.is_empty() && name.len() <= MAX_SAVE_PATH,
        "Python save_name must be a relative .py path under temp/."
    );
    let parts: Vec<&str> = name.split(['/', '\\']).collect();
    ensure!(
        !parts
            .iter()
            .any(|part| part.is_empty() || matches!(*part, "." | "..")),
        "Python save_name must be a relative .py path under temp/."
    );
    ensure!(
        parts.len() <= MAX_SAVE_COMPONENTS,
        "Python save_name must be a relative .py path under temp/."
    );
    ensure!(
        parts.iter().all(|part| !part.contains([':', '\0'])),
        "Python save_name must be a relative .py path under temp/."
    );
    let file = parts.last().unwrap();
    ensure!(
        file.len() > 3 && file.ends_with(".py"),
        "Python save_name must be a relative .py path under temp/."
    );
    Ok(Some(parts.into_iter().map(OsString::from).collect()))
}

fn saved_script_path(workspace: &Path, relative: &Path) -> Result<PathBuf> {
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("Cannot resolve {}", workspace.display()))?;
    let root = workspace.join("temp");
    std::fs::create_dir_all(&root).with_context(|| format!("Cannot create {}", root.display()))?;
    let root = root
        .canonicalize()
        .with_context(|| format!("Cannot resolve {}", root.display()))?;
    ensure!(
        root.starts_with(&workspace),
        "Python save_name must stay under the workspace temp directory."
    );
    let target = root.join(relative);
    if let Ok(metadata) = std::fs::symlink_metadata(&target) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "Python save_name target is a symlink: {}",
            target.display()
        );
        ensure!(
            metadata.is_file(),
            "Python save_name target is not a file: {}",
            target.display()
        );
    }
    let parent = target
        .parent()
        .context("Python save_name has no parent directory.")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Cannot create {}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("Cannot resolve {}", parent.display()))?;
    ensure!(
        parent.starts_with(&root),
        "Python save_name must stay under {}.",
        root.display()
    );
    Ok(target)
}

fn display_script_path(path: &Path) -> PathBuf {
    // `canonicalize` returns a verbatim `\\?\` path on Windows. Python accepts
    // it, but the model preview should stay an ordinary absolute path.
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

fn write_script(script: &Path, code: &str, saved: bool) -> Result<()> {
    if !saved {
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create {}", parent.display()))?;
        }
    }
    std::fs::write(script, code)
        .with_context(|| format!("Cannot write the Python script {}", script.display()))
}

fn build_command(
    input: &Value,
    interpreter: &ResolvedInterpreter,
    script: &Path,
    workdir: &Path,
    env: &[(String, String)],
) -> Result<Command> {
    let mut command = match input.get("__sandbox").filter(|value| value.is_object()) {
        Some(policy) => {
            let (program, args) = sandbox::wrap(policy, &interpreter.program.to_string_lossy())?;
            let mut command = Command::new(program);
            command.args(args);
            command.args(&interpreter.args);
            command
        }
        None => {
            let mut command = Command::new(&interpreter.program);
            command.args(&interpreter.args);
            command
        }
    };
    command.args(["-X", "utf8", "-u"]).arg(script);
    command.current_dir(workdir).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    crate::services::background_sessions::strip_internal_env(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }
    let inherited = std::env::var_os("PYTHONPATH");
    command.env(
        "PYTHONPATH",
        merged_pythonpath(workdir, env, inherited.as_deref())?,
    );
    Ok(command)
}

fn merged_pythonpath(
    workdir: &Path,
    env: &[(String, String)],
    inherited: Option<&std::ffi::OsStr>,
) -> Result<OsString> {
    let requested = env
        .iter()
        .rev()
        .find(|(key, _)| env_key_is(key, "PYTHONPATH"))
        .map(|(_, value)| value.as_str());
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |path: PathBuf| {
        if path.as_os_str().is_empty() {
            return;
        }
        let key = path_key(&path);
        if seen.insert(key) {
            paths.push(path);
        }
    };
    push(workdir.to_path_buf());
    if let Some(requested) = requested {
        for path in std::env::split_paths(requested) {
            push(path);
        }
    }
    if let Some(inherited) = inherited {
        for path in std::env::split_paths(inherited) {
            push(path);
        }
    }
    Ok(std::env::join_paths(paths)?)
}

fn env_key_is(key: &str, expected: &str) -> bool {
    if cfg!(windows) {
        key.eq_ignore_ascii_case(expected)
    } else {
        key == expected
    }
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    };
    text.trim_end_matches('/').to_owned()
}

fn sweep_old_temp_scripts() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(24 * 60 * 60));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(TEMP_PREFIX) || !name.ends_with(".py") {
            continue;
        }
        if entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| cutoff.is_some_and(|cutoff| modified < cutoff))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_file(dir: &Path) -> PathBuf {
        let path = dir.join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        std::fs::write(&path, b"fixture").unwrap();
        path
    }

    #[test]
    fn definition_is_deferred_and_requires_code() {
        let definition = definition();
        assert_eq!(definition.name, "Python");
        assert_eq!(definition.input_schema["required"], json!(["code"]));
        assert!(
            definition
                .description
                .contains("prefer this tool over a Bash heredoc"),
            "{}",
            definition.description
        );
        assert_eq!(
            definition.input_schema["additionalProperties"],
            Value::Bool(false)
        );
        assert_eq!(
            definition.input_schema["properties"]["env"]["additionalProperties"]["type"],
            "string"
        );
        assert!(!crate::services::customization::is_core_tool("Python"));
        assert_eq!(
            crate::services::customization::default_loading("Python"),
            crate::services::customization::ToolLoading {
                enabled: false,
                on_demand: true
            }
        );
    }

    #[test]
    fn explicit_interpreter_and_launcher_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let python = python_file(dir.path());
        let resolved = resolve_program(python.to_str().unwrap(), "parameter").unwrap();
        assert_eq!(resolved.program, python);
        assert!(resolved.args.is_empty());

        let launcher = dir.path().join(if cfg!(windows) { "py.exe" } else { "py" });
        std::fs::write(&launcher, b"fixture").unwrap();
        let resolved = resolve_program(launcher.to_str().unwrap(), "parameter").unwrap();
        assert_eq!(resolved.args, ["-3"]);

        assert!(resolve_program("definitely-not-a-python-xyz", "parameter").is_err());
    }

    #[test]
    fn settings_path_wins_over_path_discovery_and_does_not_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let python = python_file(dir.path());
        let resolved = resolve_interpreter(ResolveOptions {
            workspace: dir.path(),
            interpreter: None,
            conda: None,
            settings_path: Some(python.to_str().unwrap()),
        })
        .unwrap();
        assert_eq!(resolved.program, python);
        assert_eq!(resolved.source, "settings");

        let error = resolve_interpreter(ResolveOptions {
            workspace: dir.path(),
            interpreter: None,
            conda: None,
            settings_path: Some("/definitely/missing/python"),
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("configured python.path"), "{error}");
    }

    #[test]
    fn prefix_resolution_finds_virtualenv_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("env");
        let bin = prefix.join(if cfg!(windows) { "Scripts" } else { "bin" });
        std::fs::create_dir_all(&bin).unwrap();
        let python = bin.join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        std::fs::write(&python, b"fixture").unwrap();
        assert_eq!(python_in_prefix(&prefix), Some(python.clone()));
        let resolved = resolve_prefix_value(Some(prefix.into_os_string()), "VIRTUAL_ENV").unwrap();
        assert_eq!(resolved.program, python);
        assert_eq!(resolved.source, "VIRTUAL_ENV");
        assert_eq!(
            resolve_prefix_value(Some(OsString::new()), "VIRTUAL_ENV"),
            None
        );
    }

    #[test]
    fn dangerous_text_inside_code_is_not_a_shell_command() {
        assert!(!crate::tools::is_dangerous_command("print(\"rm -rf /\")"));
    }

    #[cfg(unix)]
    #[test]
    fn conda_env_list_uses_prefix_basenames() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let conda = dir.path().join("conda");
        std::fs::write(
            &conda,
            "#!/bin/sh\nprintf '%s' '{\"envs\":[\"/opt/conda\",\"/tmp/envs/data\"]}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&conda, std::fs::Permissions::from_mode(0o755)).unwrap();
        let envs = conda_envs(&conda).unwrap();
        assert_eq!(
            envs,
            [
                ("conda".to_owned(), PathBuf::from("/opt/conda")),
                ("data".to_owned(), PathBuf::from("/tmp/envs/data"))
            ]
        );
    }

    #[test]
    fn interpreter_errors_name_the_requested_value() {
        let err = resolve_program("/definitely/missing/python", "parameter")
            .unwrap_err()
            .to_string();
        assert!(err.contains("/definitely/missing/python"), "{err}");
    }

    #[test]
    fn env_rejects_bad_keys_reserved_names_and_oversized_values() {
        assert!(parse_env(&json!({"env": {"1BAD": "x"}})).is_err());
        assert!(parse_env(&json!({"env": {"A": 1}})).is_err());
        assert!(parse_env(&json!({
            "env": {"DEEPSEEK_CODE_BACKGROUND_TOKEN": "x"}
        }))
        .is_err());
        let many = (0..33)
            .map(|index| (format!("K{index}"), json!("x")))
            .collect::<serde_json::Map<_, _>>();
        assert!(parse_env(&json!({"env": many})).is_err());
        assert!(parse_env(&json!({"env": {"BIG": "x".repeat(MAX_ENV_BYTES)}})).is_err());
        assert_eq!(
            parse_env(&json!({"env": {"A_1": "x"}})).unwrap(),
            [("A_1".to_owned(), "x".to_owned())]
        );
    }

    #[test]
    fn pythonpath_is_merged_workdir_first_without_duplicates() {
        let workdir = Path::new("/workspace/project");
        let requested = std::env::join_paths(["/requested", "/workspace/project"]).unwrap();
        let inherited = std::env::join_paths(["/inherited", "/requested"]).unwrap();
        let env = vec![(
            "PYTHONPATH".to_owned(),
            requested.to_string_lossy().into_owned(),
        )];
        let merged = merged_pythonpath(workdir, &env, Some(&inherited)).unwrap();
        let paths: Vec<String> = std::env::split_paths(&merged)
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "/workspace/project".to_owned(),
                "/requested".to_owned(),
                "/inherited".to_owned()
            ]
        );
    }

    #[test]
    fn save_names_stay_under_temp_and_remain_relative() {
        assert_eq!(
            parse_save_name(&json!({"save_name": "python/script/001.py"}))
                .unwrap()
                .unwrap(),
            PathBuf::from("python").join("script").join("001.py")
        );
        for name in [
            "/absolute.py",
            "../escape.py",
            "./dot.py",
            "notes.txt",
            ".py",
            "a:b.py",
            "a/./b.py",
            "a//b.py",
            "a/../b.py",
            "one/two/three/four/five/six/seven/eight/nine/ten.py",
        ] {
            assert!(
                parse_save_name(&json!({ "save_name": name })).is_err(),
                "{name}"
            );
        }
        assert!(parse_save_name(&json!({"save_name": "a".repeat(201) + ".py"})).is_err());
    }

    #[test]
    fn save_root_rejects_symlink_targets() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("temp");
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("kept.py");
        std::fs::write(&target, b"old").unwrap();
        assert_eq!(
            saved_script_path(workspace.path(), Path::new("kept.py"))
                .unwrap()
                .canonicalize()
                .unwrap(),
            target.canonicalize().unwrap()
        );

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let link = root.join("link.py");
            std::os::unix::fs::symlink(outside.path().join("outside.py"), &link).unwrap();
            assert!(saved_script_path(workspace.path(), Path::new("link.py")).is_err());
        }
    }

    #[test]
    fn timeout_is_bounded_and_background_is_optional() {
        assert_eq!(
            parse_timeout(&json!({}), false).unwrap(),
            Some(Duration::from_millis(120_000))
        );
        assert_eq!(parse_timeout(&json!({}), true).unwrap(), None);
        assert_eq!(
            parse_timeout(&json!({"timeout": 5}), false).unwrap(),
            Some(Duration::from_millis(5))
        );
        for timeout in [json!(0), json!(600_001), json!(1.5), json!("1000")] {
            assert!(parse_timeout(&json!({ "timeout": timeout }), false).is_err());
        }
    }

    #[test]
    fn windows_store_aliases_are_not_path_candidates() {
        assert!(is_windows_store_alias(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\python.exe"
        )));
        assert!(!is_windows_store_alias(Path::new(
            r"C:\Python313\python.exe"
        )));
    }

    #[test]
    fn sandbox_policy_wraps_the_interpreter_without_a_shell() {
        let workspace = tempfile::tempdir().unwrap();
        let interpreter = ResolvedInterpreter {
            program: PathBuf::from("/usr/bin/python3"),
            args: Vec::new(),
            source: "parameter",
        };
        let plain = build_command(
            &json!({}),
            &interpreter,
            Path::new("/tmp/script.py"),
            workspace.path(),
            &[],
        )
        .unwrap();
        assert_eq!(
            plain.as_std().get_program().to_string_lossy(),
            "/usr/bin/python3"
        );
        let policy = json!({
            "writable": [workspace.path().to_string_lossy()],
            "network": false,
            "network_rules": null,
            "allow_unsandboxed": true
        });
        let wrapped = build_command(
            &json!({"__sandbox": policy}),
            &interpreter,
            Path::new("/tmp/script.py"),
            workspace.path(),
            &[],
        );
        #[cfg(unix)]
        {
            let wrapped = wrapped.unwrap();
            let program = wrapped.as_std().get_program().to_string_lossy();
            assert!(
                program.ends_with("bwrap") || program == "/usr/bin/sandbox-exec",
                "{program}"
            );
        }
        #[cfg(windows)]
        assert!(
            wrapped.is_err(),
            "Windows must refuse the sandbox explicitly"
        );
    }

    #[tokio::test]
    async fn interpreter_and_conda_are_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute(
            json!({
                "code": "print(1)",
                "interpreter": "python3",
                "conda": "base"
            }),
            dir.path().to_str().unwrap(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("at most one"), "{err}");
    }

    #[tokio::test]
    async fn cwd_outside_the_workspace_is_rejected_before_execution() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let err = execute(
            json!({"code": "print(1)", "cwd": outside.path().to_str().unwrap()}),
            workspace.path().to_str().unwrap(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside the allowed paths"), "{err}");
    }

    #[tokio::test]
    async fn background_python_returns_a_readable_stoppable_task() {
        let workspace = tempfile::tempdir().unwrap();
        if resolve_interpreter(ResolveOptions {
            workspace: workspace.path(),
            interpreter: None,
            conda: None,
            settings_path: None,
        })
        .is_err()
        {
            eprintln!("skipping background Python check: no interpreter found");
            return;
        }
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let cwd = workspace.path().to_str().unwrap().to_owned();
        let result = background_shell::scope(
            Some((root.path().to_path_buf(), session.clone())),
            execute(
                json!({
                    "code": "print('background-ready', flush=True)",
                    "run_in_background": true,
                    "save_name": "background.py"
                }),
                &cwd,
            ),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "running");
        assert_eq!(result["script_saved"], true);
        let id = result["task_id"].as_str().unwrap().to_owned();
        let value = crate::tools::tasks::execute_for_session_async(
            "TaskGet",
            json!({"task_id": id, "block": true, "timeout": 5_000}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(value["status"], "completed", "{value}");
        assert!(
            value["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("background-ready"),
            "{value}"
        );
        assert!(workspace.path().join("temp/background.py").exists());

        let result = background_shell::scope(
            Some((root.path().to_path_buf(), session.clone())),
            execute(
                json!({
                    "code": "import time\nprint('live', flush=True)\ntime.sleep(30)",
                    "run_in_background": true
                }),
                &cwd,
            ),
        )
        .await
        .unwrap();
        let id = result["task_id"].as_str().unwrap().to_owned();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let value = crate::tools::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id": id, "block": false}),
                root.path(),
                &session,
            )
            .unwrap();
            if value["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("live")
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{value}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let stopped = crate::tools::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id": id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true);
        let value = crate::tools::tasks::execute_for_session_async(
            "TaskGet",
            json!({"task_id": id, "block": true, "timeout": 5_000}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(value["status"], "cancelled", "{value}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_sandbox_runs_python_inside_the_profile() {
        let workspace = tempfile::tempdir().unwrap();
        if resolve_interpreter(ResolveOptions {
            workspace: workspace.path(),
            interpreter: None,
            conda: None,
            settings_path: None,
        })
        .is_err()
        {
            eprintln!("skipping sandboxed Python check: no interpreter found");
            return;
        }
        let policy = json!({
            "writable": [workspace.path().to_string_lossy()],
            "network": false,
            "network_rules": null,
            "allow_unsandboxed": true
        });
        let result = execute(
            json!({"code": "print('sandboxed')", "__sandbox": policy}),
            workspace.path().to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result["stdout"], "sandboxed\n", "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
    }
}
