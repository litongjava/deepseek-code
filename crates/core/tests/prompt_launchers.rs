//! Production CLI entry points must encode origin before the positional tail.
use regex::Regex;
use std::path::Path;

fn bypasses<'a>(file: &str, source: &'a str) -> Vec<&'a str> {
    static CONSTRUCTOR: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let constructors = CONSTRUCTOR
        .get_or_init(|| Regex::new(r"\b(?:Command|CommandBuilder)::new\s*\(\s*").unwrap());
    let mut hits = Vec::new();
    static SELF_EXE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let self_exe = SELF_EXE.get_or_init(|| Regex::new(r"\bcurrent_exe\s*\(").unwrap());
    static ARG_CALL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static ORIGIN_ARGS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let origin_args = ORIGIN_ARGS.get_or_init(|| {
        Regex::new(r"^background_sessions::PromptOrigin::Generated\.arguments\(\s*&?\w+\s*\)\s*\)")
            .unwrap()
    });
    let calls = ARG_CALL.get_or_init(|| Regex::new(r"\.(arg|args)\s*\(\s*").unwrap());
    // The workflow and batch launchers receive current_exe from their callers.
    // Check argv at the command constructor, including subsequent builder calls.
    for function in source.split("fn ") {
        let body = function
            .split_once("\n}")
            .map_or(function, |(body, _)| body);
        let name = function.split('(').next().unwrap_or("");
        if reviewed_test_lookup(file, name) {
            continue;
        }
        // These producers pass a completed argv through Launch or return it to
        // the spawning process. Inspect the positional tail before that handoff.
        let builder = match (file, name) {
            ("batch.rs", "supervise")
            | ("child_session.rs", "prepare_child_session")
            | ("external_agents.rs", "arguments") => Some("PromptOrigin::Generated.arguments("),
            ("restart.rs", "fresh") => Some("origin.arguments("),
            ("session.rs", "worker_arguments") => Some("provenance.arguments("),
            _ => None,
        };
        if let Some(builder) = builder {
            if !body.contains(builder) || body.contains(r#""--""#) {
                hits.push(name);
            }
        }
        let mut found = constructors.find_iter(body);
        let Some(constructor) = found.next() else {
            continue;
        };
        let direct = std::iter::once(constructor).chain(found).any(|m| {
            let executable = &body[m.end()..];
            executable.starts_with("\"dcl\"") || executable.starts_with("\"dcl.exe\"")
        });
        if direct {
            hits.push(constructor.as_str());
            continue;
        }
        let launch = &body[constructor.end()..];
        // Functions with an injected executable receive it from a current_exe
        // lookup elsewhere. Only their reviewed option lists may precede the
        // origin builder. Validate argument expressions independently of prompt spelling.
        let options: &[&str] = match (file, name) {
            ("workflow_runner.rs", "agent_command") => &["args)"],
            ("batch_commands.rs", "planner_command") => &["arguments)", "options)"],
            ("restart.rs", "command") => &["launch.arguments)"],
            ("remote_host.rs", "run") => &["&launch.arguments)"],
            ("batch.rs", "supervisor_command") => &[],
            _ if self_exe.is_match(body) => &[],
            _ => continue,
        };
        for call in calls.captures_iter(launch) {
            let matched = call.get(0).unwrap();
            let tail = &launch[matched.end()..];
            if &call[1] == "args"
                && (options.iter().any(|opt| tail.starts_with(opt)) || origin_args.is_match(tail))
            {
                continue;
            }
            if file == "child_session.rs"
                && name == "standalone_child_executable"
                && tail.starts_with("\"--version\")")
            {
                continue;
            }
            hits.push(matched.as_str());
        }
    }
    hits
}

// A launcher can hide its positional argument behind arbitrary variable names.
// Require review of every production function that resolves the running binary,
// including helpers that return it for a separate spawn site.
fn reviewed_self_lookup(file: &str, function: &str) -> bool {
    matches!(
        (file, function),
        (
            "workflow_runner.rs" | "remote_host.rs" | "workflow_commands.rs",
            "run"
        ) | ("batch_commands.rs", "launch")
            | ("worker_frontend.rs", "single_turn")
            | ("restart.rs", "command")
            | (
                "worker_binding.rs",
                "retain_for_transfer" | "start_retirement"
            )
            | ("background_commands.rs", "handle")
            | ("skill_commands.rs", "prepare_invocation")
            | ("child_session.rs", "standalone_child_executable")
            | ("script_workflow.rs", "workflow_child_command")
            | ("toolscript_job.rs", "start_bound_job")
            | ("request_identity.rs", "build")
            | ("checks.rs", "current")
            | ("pty_gate.rs", "gate_command")
            | ("pty_supervisor.rs", "supervisor_command")
            | ("conpty.rs", "start")
            | ("sandbox.rs", "bridge_executable")
            | ("dcd-server.rs", "default_web_dir")
    ) || reviewed_test_lookup(file, function)
}

fn reviewed_test_lookup(file: &str, function: &str) -> bool {
    matches!(
        (file, function),
        // Inline platform tests and their fixtures.
        (
            "async_hooks.rs",
            "shared_delivery_is_once_and_requires_live_owner"
                | "process_exit_kills_background_tree"
        ) | ("workspace.rs", "spawn_git_fixture" | "git_fixture_command")
            | (
                "background_shell.rs",
                "cross_process_writer_feeds_an_owner_in_another_process"
                    | "owner_loss_stops_a_cross_process_write_waiting_for_the_lock"
                    | "spawn"
            )
    )
}

#[test]
fn renamed_workflow_prompt_requires_origin_builder() {
    let batch = include_str!("../src/services/batch.rs");
    let builder = "background_sessions::PromptOrigin::Generated.arguments(&prompt(batch, index))";
    assert!(batch.contains(builder));
    for tail in [r#"["--".into(), task]"#, "[task]"] {
        assert!(!bypasses("batch.rs", &batch.replace(builder, tail)).is_empty());
    }
    let source = include_str!("../../cli/src/workflow_runner.rs");
    let builder = ".args(background_sessions::PromptOrigin::Generated.arguments(prompt))";
    assert!(source.contains(builder));
    for replacement in [
        r#".args(["--", task])"#,
        ".arg(task.trim_end())",
        r#".args([std::ffi::OsStr::new("--"), task.as_ref()])"#,
    ] {
        let mutation = source
            .replace(builder, replacement)
            .replace("prompt: &str", "task: &str");
        assert!(
            !bypasses("workflow_runner.rs", &mutation).is_empty(),
            "missed {replacement}"
        );
    }
}

#[test]
fn dcl_prompt_launches_require_the_origin_builder() {
    for mutation in [
        r#"Command::new(std::env::current_exe()?).args(["-p", text]).spawn()"#,
        r#"let binary = std::env::current_exe()?; Command::new(binary).args([task]).spawn()"#,
        r#"Command::new("dcl").arg("--print").arg("hello").spawn()"#,
        r#"Command::new(std::env::current_exe()?).arg("hello").spawn()"#,
        r#"let mut cmd=Command::new(std::env::current_exe()?); cmd.arg("--").arg(instruction).spawn()"#,
        r#"let mut args=vec![]; args.extend(["--".into(), directive]); Command::new(std::env::current_exe()?).args(args).spawn()"#,
    ] {
        assert!(!bypasses("", mutation).is_empty(), "missed {mutation}");
    }
    assert!(bypasses("", r#"Command::new("grep").arg("--").arg(prompt)"#).is_empty());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let test_modules = Regex::new(r"(?ms)^#\[cfg\(test\)\]\s*mod \w+\s*\{.*?^\}").unwrap();
    let symbols = Regex::new(r"\bfn\s+(\w+)\s*[<(]|\b(current_exe)\s*\(").unwrap();
    let mut violations = Vec::new();
    for tree in ["crates/core/src", "crates/cli/src", "src-tauri/src"] {
        for entry in walkdir::WalkDir::new(root.join(tree)) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "rs")
                || path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("tests.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let source = test_modules.replace_all(&source, "");
            let mut production = source.into_owned();
            if path.ends_with("services/background_sessions.rs") {
                let start = production.find("impl PromptOrigin {").unwrap();
                let end = start + production[start..].find("\n}").unwrap() + 2;
                production.replace_range(start..end, "");
            }
            let file = path.file_name().unwrap().to_str().unwrap();
            let mut function = "";
            for hit in symbols.captures_iter(&production) {
                if let Some(name) = hit.get(1) {
                    function = name.as_str();
                } else if !reviewed_self_lookup(file, function) {
                    violations.push(format!(
                        "Unreviewed executable lookup in {}::{function}",
                        path.display()
                    ));
                }
            }
            for hit in bypasses(file, &production) {
                violations.push(format!("{}: {hit}", path.display()));
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
