// Check each producer at the model boundary against its ordinary rendered text.

use super::super::ToolLoopExecutor;
use super::{command_preview, tests::executor};
use crate::tools::{background_shell, base64_collapse, monitor, tasks};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

fn check_view(label: &str, root: &Path, original: &str, view: &str, encoded: bool) {
    let runs = base64_collapse::runs(original);
    assert_eq!(!runs.is_empty(), encoded, "{label}: fixture coverage");
    if !encoded {
        assert_eq!(original, view, "{label}");
        return;
    }
    assert!(
        base64_collapse::runs(view).is_empty(),
        "{label}: raw payload"
    );
    let refs: Vec<_> = view.split("; full text: ").skip(1).collect();
    assert_eq!(refs.len(), runs.len(), "{label}");
    for (reference, run) in refs.into_iter().zip(runs) {
        let (file, rest) = reference.split_once(", line ").unwrap();
        let line: usize = rest.split_once(']').unwrap().0.parse().unwrap();
        let path = Path::new(file);
        assert!(path.starts_with(root.join("cache/tool-outputs")));
        assert_eq!(
            path.file_stem().unwrap().to_str().unwrap(),
            format!("{:x}", Sha256::digest(original))
        );
        let saved = std::fs::read_to_string(path).unwrap();
        assert_eq!(saved, original, "{label}");
        assert_eq!(
            line,
            1 + original[..run.range.start]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
        );
        assert!(saved
            .lines()
            .nth(line - 1)
            .unwrap()
            .contains(&original[run.range]));
    }
}

fn check_raw_references(label: &str, root: &Path, raw: &str, view: &str) {
    assert!(view.contains("base64 data omitted"), "{label}");
    assert!(base64_collapse::runs(view).is_empty(), "{label}");
    let hash = format!("{:x}", Sha256::digest(raw));
    for reference in view.split("; full text: ").skip(1) {
        let (file, rest) = reference.split_once(", line ").unwrap();
        let line: usize = rest.split_once(']').unwrap().0.parse().unwrap();
        let file = Path::new(file);
        assert!(file.starts_with(root.join("cache/tool-outputs")));
        assert_eq!(file.file_stem().unwrap().to_str().unwrap(), hash);
        assert_eq!(std::fs::read_to_string(file).unwrap(), raw);
        assert!(
            raw.lines().nth(line - 1).unwrap().contains("QUJD")
                || raw.lines().nth(line - 1).unwrap().contains("QQQQ")
        );
    }
}

fn check_tool(
    label: &str,
    engine: &mut ToolLoopExecutor,
    root: &Path,
    name: &str,
    raw: &str,
    encoded: bool,
) {
    let original = command_preview(
        name,
        raw,
        Some(root),
        None,
        engine.summary_enabled(Some(root)),
    );
    let live = engine.retained_tool_output(name, raw);
    if encoded {
        check_raw_references(label, root, raw, &live);
    } else {
        assert_eq!(live, original, "{label}");
    }
    engine.messages = vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","type":"function","function":{"name":name,"arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":raw}),
    ];
    engine.prepare_model_input().unwrap();
    let replay = serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap();
    assert_eq!(replay[1]["content"], live, "{label}: OpenAI replay");
    engine.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"old","name":name,"input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"old","content":raw}]}),
    ];
    engine.prepare_model_input().unwrap();
    let replay = serde_json::to_value(engine.build_anthropic_messages().unwrap()).unwrap();
    assert_eq!(
        replay[1]["content"][0]["content"], live,
        "{label}: Anthropic replay"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn base64_model_boundary_matrix() {
    use crate::tools::execute_tool_async;
    use std::io::Write;
    use std::time::Duration;

    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root<with brackets>");
    std::fs::create_dir(&root).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let mut engine = executor(&root).with_conv_id(session.clone());
    for encoded in [false, true] {
        let payload = if encoded {
            "QUJD".repeat(256)
        } else {
            "流水.".repeat(256)
        };
        let text = format!("first line\n!{payload}!\n");
        std::fs::write(root.join("fixture"), &text).unwrap();
        let large = if encoded {
            "QUJD".repeat(32768)
        } else {
            ".".repeat(131072)
        };
        let timeout_source = format!("{text}!{large}!\n{text}");
        std::fs::write(root.join("timeout-fixture"), &timeout_source).unwrap();
        for timeout in [false, true] {
            let input = if timeout {
                json!({"command":"cat timeout-fixture; sleep 2","timeout":650})
            } else {
                json!({"command":"cat fixture"})
            };
            let raw = background_shell::scope(
                Some((root.clone(), session.clone())),
                execute_tool_async("Bash", input, root.to_str().unwrap()),
            )
            .await
            .unwrap();
            if timeout {
                assert_eq!(raw["timed_out_to_background"], true);
                assert!(raw["stdout"].as_str().unwrap().len() <= 32 * 1024);
                assert!(raw["stdout"].as_str().unwrap().len() < timeout_source.len());
                let snapshot = tasks::execute_for_session_async(
                    "TaskGet",
                    json!({"task_id":raw["task_id"],"block":true,"timeout":15000}),
                    &root,
                    &session,
                )
                .await
                .unwrap();
                assert_eq!(snapshot["status"], "completed");
                check_tool(
                    "shell snapshot",
                    &mut engine,
                    &root,
                    "TaskGet",
                    &snapshot.to_string(),
                    encoded,
                );
            }
            assert!(raw["stdout"].as_str().unwrap().contains(&payload));
            check_tool(
                if timeout {
                    "timeout handoff"
                } else {
                    "foreground Bash"
                },
                &mut engine,
                &root,
                "Bash",
                &raw.to_string(),
                encoded,
            );
        }

        // A completed console task can have an arbitrarily long append log.
        let task = tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"console","metadata":{"managed_shell":true,"interaction":"pty"}}),
            &root,
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"completed"}),
            &root,
            &session,
        )
        .unwrap();
        let logs = root.join("tasks/shell").join(&session).join(id);
        std::fs::create_dir_all(&logs).unwrap();
        let mib = 1024 * 1024;
        let log = format!("{text}{}{text}", ".".repeat(mib - text.len()));
        std::fs::write(logs.join("stdout.log"), &log).unwrap();
        std::fs::write(logs.join("stderr.log"), "").unwrap();
        let get = |input| tasks::execute_for_session("TaskGet", input, &root, &session).unwrap();
        let snapshot = get(json!({"id":id,"block":false}));
        assert_eq!(snapshot["stdout"].as_str().unwrap().len(), mib);
        check_tool(
            "bounded PTY snapshot",
            &mut engine,
            &root,
            "TaskGet",
            &snapshot.to_string(),
            encoded,
        );
        let first = get(json!({"id":id,"max_output_bytes":mib}));
        let page = get(json!({"id":id,"max_output_bytes":8192,"cursor":first["next_cursor"]}));
        assert_eq!(page["stdout"], text);
        check_tool(
            "PTY page beyond 1 MiB, also replayed without a sidecar",
            &mut engine,
            &root,
            "TaskGet",
            &page.to_string(),
            encoded,
        );

        let monitor_line = payload.repeat(8);
        std::fs::write(root.join("monitor-fixture"), format!("{monitor_line}\n")).unwrap();
        let mut command = tokio::process::Command::new("cat");
        command.arg(root.join("monitor-fixture"));
        let started = background_shell::scope(Some((root.clone(), session.clone())), async {
            monitor::start(
                command,
                "watch",
                "cat fixture",
                Duration::from_secs(10),
                None,
            )
        })
        .await
        .unwrap();
        let id = started["task_id"].as_str().unwrap();
        let mut events = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                events.extend(monitor::collect_events(&root, &session).unwrap());
                if events.iter().any(|event| event.kind == "exited") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let records = root
            .join("tasks/shell")
            .join(&session)
            .join(id)
            .join("events.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&records)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"seq":999,"kind":"output","text":format!("legacy!{payload}!") })
        )
        .unwrap();
        let legacy = monitor::collect_events(&root, &session).unwrap();
        let live = events
            .iter()
            .find(|event| event.kind == "output" && event.text.contains(&payload))
            .unwrap();
        assert_eq!(
            live.text,
            monitor_line.chars().take(4096).collect::<String>()
        );
        assert!(live
            .summary()
            .ends_with(&monitor_line.chars().take(200).collect::<String>()));
        for (label, event) in [("live Monitor", live), ("legacy Monitor", &legacy[0])] {
            let notice = event.notification();
            assert!(notice.contains(&payload));
            let input = crate::services::input_queue::DeliveredInput::resolve(
                Some(&root),
                &session,
                notice.clone(),
            );
            let queued = engine.queued_model_content(vec![input]);
            check_view(label, &root, &notice, queued[0].as_str().unwrap(), encoded);
            engine.messages = vec![json!({"role":"user","content":notice})];
            engine.prepare_model_input().unwrap();
            let replay = serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap();
            assert_eq!(replay[0]["content"], queued[0]);
            assert_eq!(engine.messages[0]["content"], queued[0]);
        }
    }
}

#[test]
fn base64_i6_cut_references_the_complete_result_and_preserves_counts() {
    let temp = tempfile::tempdir().unwrap();
    // Arrange a canonical quartet on each edge of the actual retained preview.
    let root = temp.path();
    let engine = executor(root);
    let data = "Q".repeat(40_000);
    let raw = json!({"stdout":format!("!{data}!"),"exit_code":0}).to_string();
    let original = command_preview("Bash", &raw, Some(root), None, true);
    let view = engine.retained_tool_output("Bash", &raw);
    check_raw_references("I6", root, &raw, &view);
    let path = original
        .lines()
        .find(|line| line.starts_with("Saved tool result ("))
        .unwrap()
        .split_once("): ")
        .unwrap()
        .1;
    assert!(view.contains(path));
    assert!(std::fs::read_to_string(path).unwrap().contains(&data));
    assert!(view.contains("base64: 1 block collapsed (40000 encoded bytes)"));
    assert!(view.contains("Omitted output:"));
}

#[test]
fn base64_model_boundary_preserves_the_other_stream_and_every_visible_reference() {
    let temp = tempfile::tempdir().unwrap();
    let engine = executor(temp.path());
    let stdout = format!("{}\nLAST-ORDINARY-DIAGNOSTIC!", ".".repeat(1_052_698));
    let stderr = "QUJD".repeat(256);
    let (raw, metadata) = crate::tools::bash_process::fixture_capture(
        [stdout.as_bytes(), stderr.as_bytes()],
        1 << 20,
        102400,
        0,
    );
    let id = "mixed-streams";
    super::save_metadata(temp.path(), id, metadata.as_ref().unwrap()).unwrap();
    let ordinary = command_preview("Bash", &raw, Some(temp.path()), metadata.as_ref(), true);
    assert!(ordinary.contains("LAST-ORDINARY-DIAGNOSTIC!"));
    let live = engine.retained_tool_output_for_id("Bash", &raw, Some(id));
    assert!(live.contains("LAST-ORDINARY-DIAGNOSTIC!"));
    check_raw_references("mixed streams", temp.path(), &raw, &live);
    let many = format!("!{}!\n", stderr).repeat(140);
    let projected = super::super::project_for_model("TaskGet", &many, Some(temp.path()), "session")
        .unwrap()
        .text;
    assert_eq!(projected.matches("; full text: ").count(), 140);
    check_view("140 historical runs", temp.path(), &many, &projected, true);
}

#[test]
fn base64_model_boundary_without_a_settings_path_uses_the_runtime_directory() {
    // Isolate the runtime directory from parallel tests and the user's home.
    const CHILD: &str = "I41_PROJECTION_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = Path::new(&root);
        let mut engine = executor(root);
        engine.search_config_path = None;
        let original = format!("!{}!", "QUJD".repeat(256));
        let live = engine.retained_tool_output("Bash", &original);
        check_view("default runtime directory", root, &original, &live, true);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let name = concat!(
        module_path!(),
        "::base64_model_boundary_without_a_settings_path_uses_the_runtime_directory"
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1])
        .env(CHILD, temp.path())
        .env(crate::runtime::DATA_DIR_ENV, temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[test]
fn base64_model_boundary_replays_plaintext_in_native_windows() {
    let dir = tempfile::tempdir().unwrap();
    let payload = "QUJD".repeat(256);
    let page = format!("historical page\n!{payload}!");
    let notice =
        format!("<task-notification>\n<kind>monitor</kind>\n!{payload}!\n</task-notification>");
    let mut messages = vec![json!({"role":"user","content":"Native compact window",
    "provider_content":{"compaction":true,"items":[
        {"type":"function_call","call_id":"call","name":"TaskGet"},
        {"type":"function_call_output","call_id":"call","output":page},
        {"role":"user","content":[{"type":"input_text","text":notice}]},
        {"type":"compaction","encrypted_content":payload}
    ]}})];
    super::prepare_native_window(
        &mut messages[0]["provider_content"],
        Some(dir.path()),
        "window",
    );
    let items = &messages[0]["provider_content"]["items"];
    check_view(
        "native stored page",
        dir.path(),
        &page,
        items[1]["output"].as_str().unwrap(),
        true,
    );
    check_view(
        "native Monitor",
        dir.path(),
        &notice,
        items[2]["content"][0]["text"].as_str().unwrap(),
        true,
    );
    assert_eq!(items[3]["encrypted_content"], payload);
    let expected = messages.clone();
    super::prepare_native_window(
        &mut messages[0]["provider_content"],
        Some(dir.path()),
        "window",
    );
    assert_eq!(messages, expected);
}

#[test]
fn base64_model_boundary_consumers_use_model_rows() {
    // Storage owns the legacy fallback. Display, export and conflict hashing
    // may use ToolCallRow. Model builders have no reason to obtain one.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let raw = regex::Regex::new(
        r"(?isx)
        \b(?:get_tool_calls_for_message|ToolCallRow|user_content_for_display)\b
        |\b\w+\s*\.\s*(?-i:output)\b
        |\b\w+\s*(?:\[\s*\x22output\x22\s*\]|\.get\(\s*\x22output\x22\s*\))
        |\b(?:r|reader|session)\s*\.\s*tools\s*\(
        |\bselect\b[^\x22;]{0,1000}?\boutput\b[^\x22;]{0,250}?\bfrom\s+tool_calls\b
    ",
    )
    .unwrap();
    let test_modules = regex::Regex::new(r"(?ms)^#\[cfg\(test\)\]\s*mod\s+\w+\s*\{.*?^\}").unwrap();
    let audit = |file: &Path, body: &str| {
        if let Some(hit) = raw.find(body) {
            panic!(
                "{} reads a display row through {}",
                file.display(),
                hit.as_str()
            );
        }
    };
    for relative in [
        "native_engine/engine_core.rs",
        "native_engine/tool_loop.rs",
        "native_engine/model_tool_output.rs",
        "native_engine/provider_runtime.rs",
        "native_engine/provider_runtime",
        "native_engine/context_compressor.rs",
        "services/compaction.rs",
        "services/compaction",
        "tools/context_search.rs",
        "super_agent/review.rs",
        "db/context_search_repo.rs",
        "memory/mod.rs",
    ] {
        for entry in walkdir::WalkDir::new(src.join(relative)) {
            let entry = entry.unwrap();
            let file = entry.path();
            if file.extension() != Some("rs".as_ref())
                || file
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("tests")
            {
                continue;
            }
            let text = std::fs::read_to_string(file).unwrap();
            let production = test_modules.replace_all(&text, "");
            // Protocol envelopes contain a gateway output array.
            let production = if file.ends_with("compaction/native.rs") {
                production.replace(r#"result["output"]"#, "gateway_items")
            } else if file.ends_with("provider_runtime/events.rs") {
                production.replace(r#"response["output"]"#, "gateway_items")
            } else {
                production.into_owned()
            };
            audit(file, &production);
        }
    }
    // Mixed-purpose modules also contain legitimate display/export readers.
    for (file, start, end) in [
        (
            "db/session_store/child_reader.rs",
            "pub(crate) fn review_messages(",
            "impl Stores",
        ),
        (
            "runtime.rs",
            "pub(crate) fn replay_history(",
            "impl SessionStore<DbManager>",
        ),
        (
            "db/session_store/lifecycle/copy.rs",
            "pub fn prepare_summary_part(",
            "pub fn branch(",
        ),
    ] {
        let path = src.join(file);
        let body = std::fs::read_to_string(&path).unwrap();
        audit(
            &path,
            body.split_once(start).unwrap().1.split_once(end).unwrap().0,
        );
    }
}

#[test]
fn base64_model_boundary_typed_blocks_keep_image_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = executor(dir.path());
    let payload = "QUJD".repeat(256);
    let blocks = json!([
        {"type":"text","text":format!("header\n{payload}\n"),"citation":"fixture"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":payload}},
        {"type":"text","text":"ordinary"}
    ]);
    engine.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Bash","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":blocks}]}),
    ];
    engine.prepare_model_input().unwrap();
    let request = serde_json::to_value(engine.build_anthropic_messages().unwrap()).unwrap();
    let view = &request[1]["content"][0]["content"];
    assert_eq!(view[0]["citation"], "fixture");
    assert_eq!(
        view.as_array().unwrap()[1..],
        blocks.as_array().unwrap()[1..]
    );
    let text = view[0]["text"].as_str().unwrap();
    assert!(
        text.contains("base64: 1 block collapsed (1024 encoded bytes)") && !text.contains(&payload)
    );
    let path = text
        .split_once("; full text: ")
        .unwrap()
        .1
        .rsplit_once(", line 1]")
        .unwrap()
        .0;
    assert_eq!(std::fs::read_to_string(path).unwrap(), blocks.to_string());
}
