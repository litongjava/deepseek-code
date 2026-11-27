//! Model-only rendering. Stored results, hooks and UI events keep their original data.
use crate::performance::{
    preview_with_ranges, retain_tool_output_with_ranges, PreviewRanges, LONG_COMMAND_OUTPUT_BYTES,
    MODEL_TOOL_OUTPUT_BYTES,
};
use crate::tools::bash_process::CommandMetadata;
use serde_json::Value;
use std::{ops::Range, path::Path};

const DIGEST_BUDGET: usize = 2048;

#[path = "command_output_summary.rs"]
mod command_output_summary;

/// Only actual overrides bypass a later light cut, including when a
/// provider reuses a call ID. Keep fingerprints instead of copying image blocks.
#[derive(Default, Clone)]
pub(crate) struct AppliedOverrides(
    std::collections::HashMap<String, std::collections::HashSet<[u8; 32]>>,
);

impl AppliedOverrides {
    fn fingerprint(content: &Value) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        match content.as_str() {
            Some(text) => Sha256::digest(text.as_bytes()).into(),
            None => Sha256::digest(content.to_string().as_bytes()).into(),
        }
    }
    pub(super) fn record(&mut self, id: &str, content: &Value) {
        self.0
            .entry(id.to_owned())
            .or_default()
            .insert(Self::fingerprint(content));
    }
    pub(crate) fn contains(&self, id: &str, content: &Value) -> bool {
        self.0
            .get(id)
            .is_some_and(|views| views.contains(&Self::fingerprint(content)))
    }
}

impl super::ToolLoopExecutor {
    pub(super) fn queued_model_content(
        &mut self,
        inputs: Vec<crate::services::input_queue::DeliveredInput>,
    ) -> Vec<Value> {
        inputs
            .into_iter()
            .map(|input| {
                let original = super::monitor_notification(&input.text).then(|| input.text.clone());
                let content = input.model_content(
                    self.search_config_path
                        .as_deref()
                        .and_then(crate::data_layout::data_root_of),
                    self.conv_id.as_deref().unwrap_or_default(),
                );
                if original
                    .as_deref()
                    .is_some_and(|raw| content.as_str().is_some_and(|text| text != raw))
                {
                    self.projected_results.record("", &content);
                }
                content
            })
            .collect()
    }

    /// Replace only complete results that actually collapsed. Ordinary history
    /// retains its original size until the provider builder renders it.
    pub(super) fn project_history(&mut self, messages: &mut [Value]) -> anyhow::Result<()> {
        use crate::db::model_results::StoredContent;
        let root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of);
        let session = self.conv_id.as_deref().unwrap_or_default();
        let history = self.session_search_storage();
        let cwd = std::path::PathBuf::from(self.get_workspace_cwd());
        let overrides = &mut self.projected_results;
        for message in messages.iter_mut() {
            if let Some(window) = message.get_mut("provider_content") {
                prepare_native_window(window, root, session);
            }
            if message["role"] == "user" {
                let stored = message
                    .as_object_mut()
                    .and_then(|m| m.remove("_collapsed_monitor"))
                    == Some(Value::Bool(true));
                if let Some(content) = message.get_mut("content") {
                    if stored || project_notification(content, root, session) {
                        overrides.record("", content);
                    }
                }
            }
        }
        let mut prepare = |conn: Option<&rusqlite::Connection>| {
            visit_history_outputs(messages, |name, id, content, openai, _| {
                let stored = match conn {
                    Some(conn) => {
                        crate::db::model_results::provider_content(conn, session, id, name, openai)?
                    }
                    None => StoredContent::Missing,
                };
                let view = match stored {
                    StoredContent::Ordinary => return Ok(false),
                    StoredContent::Override(view) => view,
                    StoredContent::Missing => {
                        if let Some(raw) = content.as_str() {
                            let Some(view) = crate::db::model_results::result_override(
                                name,
                                raw,
                                root,
                                session,
                                Some(id),
                                &cwd,
                            ) else {
                                return Ok(false);
                            };
                            Value::String(view.text)
                        } else {
                            if !project_result_content(name, content, root, session) {
                                return Ok(false);
                            }
                            content.clone()
                        }
                    }
                };
                overrides.record(id, &view);
                *content = view;
                Ok(true)
            })
        };
        if let Some(history) = history {
            history.read(|conn, _| prepare(Some(conn)))
        } else {
            prepare(None)
        }
    }

    pub(super) fn render_history(&self, messages: &mut [Value]) -> anyhow::Result<()> {
        visit_history_outputs(messages, |name, id, content, openai, collapsed| {
            if !collapsed {
                if let Some(text) = content.as_str() {
                    *content = Value::String(self.ordinary_preview(name, text, Some(id)));
                } else if !openai {
                    self.retain_content(name, content);
                }
            }
            Ok(false)
        })
    }

    pub(super) fn retain_content(&self, name: &str, content: &mut Value) {
        if let Some(text) = content.as_str() {
            *content = Value::String(self.ordinary_preview(name, text, None));
            return;
        }
        if let Some(blocks) = content.as_array() {
            // Text-only MCP blocks share one budget. Images and opaque blocks stay intact.
            if blocks.iter().all(|block| {
                block["type"] == "text"
                    && block["text"].is_string()
                    && block.as_object().is_some_and(|o| o.len() == 2)
            }) {
                let text = blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                *content = Value::String(self.ordinary_preview(name, &text, None));
            }
        }
    }
}

pub(crate) fn project_result_content(
    name: &str,
    content: &mut Value,
    root: Option<&Path>,
    session: &str,
) -> bool {
    if !super::eligible(name) {
        return false;
    }
    let projected = match content {
        Value::Array(_) => super::project_for_model(name, &content.to_string(), root, session)
            .map(|view| serde_json::from_str(&view.text).unwrap_or(Value::String(view.text))),
        Value::String(raw) => {
            super::project_for_model(name, raw, root, session).map(|view| Value::String(view.text))
        }
        _ => None,
    };
    if let Some(projected) = projected {
        *content = projected;
        true
    } else {
        false
    }
}

pub(crate) fn ordinary_preview(
    tool_name: &str,
    output: &str,
    id: Option<&str>,
    root: Option<&Path>,
    cwd: &Path,
) -> String {
    if is_shell_result(tool_name) {
        let enabled = crate::config::command_output::enabled(root, cwd);
        let metadata = if enabled {
            root.zip(id)
                .and_then(|(root, id)| std::fs::read(metadata_path(root, id)).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        } else {
            None
        };
        return command_preview(tool_name, output, root, metadata.as_ref(), enabled);
    }
    let cleaned = super::model_tool_output(tool_name, output);
    // Read already selects the requested line range.
    if let Some(root) = root.filter(|_| tool_name != "Read") {
        let preview = super::compact_tool_output(tool_name, &cleaned);
        crate::performance::retain_tool_output(root, &cleaned, &preview)
    } else {
        cleaned
    }
}

fn project_text_blocks(
    content: &mut Value,
    mut project: impl FnMut(&str) -> Option<String>,
) -> bool {
    let mut changed = false;
    if let Some(blocks) = content.as_array_mut() {
        for block in blocks
            .iter_mut()
            .filter(|block| matches!(block["type"].as_str(), Some("text" | "input_text")))
        {
            if let Some(view) = block["text"].as_str().and_then(&mut project) {
                block["text"] = Value::String(view);
                changed = true;
            }
        }
    } else if let Some(view) = content.as_str().and_then(project) {
        *content = Value::String(view);
        changed = true;
    }
    changed
}

pub(crate) fn project_notification(
    content: &mut Value,
    root: Option<&Path>,
    session: &str,
) -> bool {
    project_text_blocks(content, |text| {
        if super::monitor_notification(text) {
            super::project_for_model("Monitor", text, root, session).map(|v| v.text)
        } else {
            None
        }
    })
}

/// Keep the exact gateway reply for display and its model-facing window together.
pub(crate) fn seal_native_window(window: &mut Value, root: Option<&Path>, session: &str) {
    if window["compaction"] != true || window["model_projected"] == true {
        return;
    }
    let original = window["items"].clone();
    prepare_native_window(window, root, session);
    if window["items"] != original {
        window["model_items"] = window["items"].take();
        window["items"] = original;
    }
}

// A saved Responses compact window can contain plaintext messages alongside
// encrypted reasoning. Only identified results and Monitor text are projected.
pub(crate) fn prepare_native_window(window: &mut Value, root: Option<&Path>, session: &str) {
    if let Some(items) = window.as_object_mut().and_then(|o| o.remove("model_items")) {
        window["items"] = items;
        return;
    }
    if window["compaction"] != true || window["model_projected"] == true {
        return;
    }
    let Some(items) = window.get_mut("items").and_then(Value::as_array_mut) else {
        return;
    };
    let mut names = std::collections::HashMap::<String, String>::new();
    let mut changed = false;
    for item in items {
        if item["role"] == "user" {
            if let Some(content) = item.get_mut("content") {
                changed |= project_notification(content, root, session);
            }
        } else if item["type"] == "function_call" {
            if let (Some(id), Some(name)) = (item["call_id"].as_str(), item["name"].as_str()) {
                names.insert(id.into(), name.into());
            }
        } else if item["type"] == "function_call_output" {
            if let Some(name) = item["call_id"].as_str().and_then(|id| names.get(id)) {
                if let Some(output) = item.get_mut("output") {
                    changed |= project_result_content(name, output, root, session);
                }
            }
        }
    }
    if changed {
        window["model_projected"] = Value::Bool(true);
    }
}

fn visit_history_outputs(
    messages: &mut [Value],
    mut result: impl FnMut(&str, &str, &mut Value, bool, bool) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut names = std::collections::HashMap::<String, String>::new();
    for message in messages {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                if let (Some(id), Some(name)) =
                    (call["id"].as_str(), call["function"]["name"].as_str())
                {
                    names.insert(id.into(), name.into());
                }
            }
        }
        if message["role"] == "tool" {
            if let Some((id, name)) = message["tool_call_id"]
                .as_str()
                .and_then(|id| names.get_key_value(id))
            {
                let collapsed = message.as_object_mut().unwrap().remove("_collapsed")
                    == Some(Value::Bool(true));
                if result(name, id, &mut message["content"], true, collapsed)? {
                    message["_collapsed"] = Value::Bool(true);
                }
            }
        } else if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                if block["type"] == "tool_use" {
                    if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                        names.insert(id.into(), name.into());
                    }
                } else if block["type"] == "tool_result" {
                    if let Some((id, name)) = block["tool_use_id"]
                        .as_str()
                        .and_then(|id| names.get_key_value(id))
                    {
                        let collapsed = block.as_object_mut().unwrap().remove("_collapsed")
                            == Some(Value::Bool(true));
                        if result(name, id, &mut block["content"], false, collapsed)? {
                            block["_collapsed"] = Value::Bool(true);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn render(name: &str, output: &str, body_budget: Option<usize>) -> String {
    render_with_spans(name, output, body_budget, &mut [None, None])
}

struct BodySpan {
    start: usize,
    ranges: PreviewRanges,
}

fn render_with_spans(
    name: &str,
    output: &str,
    body_budget: Option<usize>,
    spans: &mut [Option<BodySpan>; 2],
) -> String {
    // ToolLoad's structured result (loaded names, schemas) is kept for the
    // transcript, the UI and the SDK. The model only needs the content line:
    // the next request already carries the loaded definitions, and repeating
    // them here would grow every later request.
    if name == "ToolLoad" {
        return tool_load_content(output).unwrap_or_else(|| output.into());
    }
    if !matches!(
        name,
        "Bash" | "Python" | "Read" | "Write" | "Edit" | "MultiEdit" | "ApplyPatch"
    ) {
        return output.into();
    }
    let (prefix, json) = output
        .strip_prefix("Error: ")
        .map_or(("", output), |s| ("Error: ", s));
    // Hooks can append context after the JSON value; preserve that suffix verbatim.
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(Value::Object(mut fields))) = stream.next() else {
        return output.into();
    };
    let suffix = &json[stream.byte_offset()..];
    if name == "ApplyPatch" {
        // Per-file diffs stay in the UI/records; the model keeps the counts and
        // path lists, including every unrestored path.
        fields.remove("file_change");
        if let Some(Value::Array(changes)) = fields.get_mut("changes") {
            for change in changes {
                if let Some(object) = change.as_object_mut() {
                    object.remove("file_change");
                }
            }
        }
        return format!("{prefix}{}{suffix}", Value::Object(fields));
    }
    if matches!(name, "Write" | "Edit" | "MultiEdit") {
        if fields.remove("file_change").is_none() {
            return output.into();
        }
        return format!("{prefix}{}{suffix}", Value::Object(fields));
    }
    let mut parts = Vec::new();
    let mut bodies = Vec::new();
    let mut span_parts = Vec::new();
    if name == "Read" {
        let Some(content) = fields
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return output.into();
        };
        // Images are handled by the multimodal path, never flattened here.
        if fields.contains_key("image") {
            return output.into();
        }
        fields.remove("content");
        if let Some(lines) = fields.remove("lines") {
            if fields.get("truncated") == Some(&Value::Bool(true)) {
                parts.push(format!(
                    "File has {lines} lines; more lines remain. Use offset/limit to continue."
                ));
            }
        }
        fields.remove("truncated");
        bodies.push(content);
    } else {
        if !fields.contains_key("stdout")
            && !fields.contains_key("stderr")
            && !fields.contains_key("error")
        {
            return output.into();
        }
        let requested_shell = fields
            .remove("shell")
            .and_then(|shell| shell.as_str().map(str::to_owned));
        let python = name == "Python";
        if python {
            // The interpreter is only news when it is not the one requested, or
            // when a project environment replaced the default silently.
            if let Some(Value::String(path)) = fields.remove("interpreter") {
                let requested = fields
                    .remove("interpreter_requested")
                    .and_then(|value| value.as_str().map(str::to_owned));
                let source = fields
                    .remove("interpreter_source")
                    .and_then(|value| value.as_str().map(str::to_owned));
                let changed = fields
                    .remove("interpreter_changed")
                    .and_then(|value| value.as_bool());
                let show = match changed {
                    Some(changed) => changed,
                    None => match requested.as_deref() {
                        Some(requested) => !same_path(requested, &path),
                        None => matches!(
                            source.as_deref(),
                            Some("VIRTUAL_ENV" | "CONDA_PREFIX" | "venv")
                        ),
                    },
                };
                if show {
                    parts.push(format!("interpreter: {path}"));
                }
            } else {
                fields.remove("interpreter_source");
                fields.remove("interpreter_requested");
                fields.remove("interpreter_changed");
            }
            if let Some(Value::String(path)) = fields.remove("script_path") {
                parts.push(format!("script: {path}"));
            }
            fields.remove("script_saved");
        }
        // Python's interpreter/script lines are not a status; an empty run still
        // says it completed.
        let status_start = parts.len();
        // A zero exit code is the normal case; only a failure needs naming.
        let mut exit_shown = false;
        if fields.get("exit_code").is_none_or(Value::is_null) {
            if let Some(signal) = fields
                .get("signal")
                .and_then(Value::as_u64)
                .filter(|s| *s > 0)
            {
                parts.push(format!("Command terminated by signal {signal}"));
                fields.remove("signal");
                exit_shown = true;
            }
        }
        if let Some(code) = fields.remove("exit_code") {
            if !exit_shown && code.as_i64().is_some_and(|code| code != 0 && code != -1) {
                parts.push(format!("Exit code: {code}"));
                exit_shown = true;
            }
        }
        if fields.get("success") == Some(&Value::Bool(false)) && !exit_shown {
            parts.push("Command failed".into());
        }
        fields.remove("success");
        // A foreground shell reports its directory on first call, change, reset,
        // or after resume/compaction, above stdout.
        if let Some(cwd) = fields
            .remove("cwd")
            .and_then(|cwd| cwd.as_str().map(str::to_owned))
        {
            parts.push(format!("cwd: {cwd}"));
        }
        for (key, label) in [
            ("timed_out", "Command timed out"),
            ("is_error", "Tool reported an error"),
            (
                "stdout_truncated",
                "Stdout is incomplete (capture or tool limit)",
            ),
            (
                "stderr_truncated",
                "Stderr is incomplete (capture or tool limit)",
            ),
        ] {
            if fields.remove(key) == Some(Value::Bool(true)) {
                parts.push(label.into());
            }
        }
        // A pipeline closed early is not a failure; say so in words instead of
        // leaving a raw flag for the model to decode.
        if fields.remove("pipeline_sigpipe") == Some(Value::Bool(true)) {
            parts.push("Output ended early: a later command in the pipeline closed it".into());
        }
        // The executable that ran is only news when it is not the one asked
        // for; a matching shell needs no line at all.
        if let Some(Value::String(path)) = fields.remove("shell_path") {
            if let Some(requested) = requested_shell.as_deref() {
                if !shell_matches(requested, &path) {
                    parts.push(format!("Shell: {path} (requested {requested})"));
                }
            }
        }
        if let Some(error) = fields.remove("error") {
            parts.push(format!(
                "Error: {}",
                error
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string())
            ));
        }
        let streams = ["stdout", "stderr"]
            .iter()
            .filter(|key| {
                fields
                    .get(**key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
            .count()
            .max(1);
        let (stdout_label, stderr_label) = if python {
            ("stdout", "stderr")
        } else {
            ("Output", "Stderr")
        };
        for (index, (key, label)) in [("stdout", stdout_label), ("stderr", stderr_label)]
            .into_iter()
            .enumerate()
        {
            if let Some(Value::String(text)) = fields.remove(key) {
                if !text.is_empty() {
                    let budget = body_budget.map_or(text.len(), |budget| budget / streams);
                    let (text, ranges) = preview_with_ranges(&text, budget);
                    span_parts.push((index, label.len() + 2, ranges));
                    bodies.push(format!("{label}:\n{text}"));
                }
            }
        }
        if python && bodies.is_empty() && parts.len() == status_start {
            parts.push("(Completed with no output)".into());
        }
        // Without a status line or a second stream, the `Output:` label adds
        // nothing: the body is simply the command's output. Python keeps its
        // lowercase `stdout:` label even when it is the only line.
        if !python && prefix.is_empty() && parts.is_empty() && fields.is_empty() {
            if let [(0, label_len, ranges)] = span_parts.as_slice() {
                spans[0] = Some(BodySpan {
                    start: 0,
                    ranges: ranges.clone(),
                });
                return format!("{}{suffix}", &bodies[0][*label_len..]);
            }
            if bodies.is_empty() {
                // A successful command with nothing to say must still read as
                // finished rather than as an unfinished task.
                return format!("(Completed with no output){suffix}");
            }
        }
    }
    // Preserve extensions (diagnostics, paths, task IDs) rather than dropping unknown fields.
    if !fields.is_empty() {
        parts.push(format!("Metadata: {}", Value::Object(fields)));
    }
    let mut offset = prefix.len() + parts.iter().map(|part| part.len() + 1).sum::<usize>();
    for ((index, label_len, ranges), body) in span_parts.into_iter().zip(&bodies) {
        spans[index] = Some(BodySpan {
            start: offset + label_len,
            ranges,
        });
        offset += body.len() + 1;
    }
    parts.extend(bodies);
    format!("{prefix}{}{suffix}", parts.join("\n"))
}

/// ToolLoad's model-visible text: the result's own `content` line. A hook
/// context appended after the JSON stays, but the loaded schemas do not.
fn tool_load_content(output: &str) -> Option<String> {
    let json = output.strip_prefix("Error: ").unwrap_or(output);
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(value)) = stream.next() else {
        return None;
    };
    let content = value.get("content")?.as_str()?;
    Some(format!("{content}{}", &json[stream.byte_offset()..]))
}

pub(super) fn is_shell_result(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "Python" | "PowerShell" | "TaskGet" | "TaskOutput"
    )
}

pub(super) fn metadata_path(root: &Path, id: &str) -> std::path::PathBuf {
    // Transcript row ids prefix the occurrence with the assistant message id.
    let id = id.rsplit(':').next().unwrap_or(id);
    let digest = ring::digest::digest(&ring::digest::SHA256, id.as_bytes());
    let key: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    root.join("cache/tool-output-metadata")
        .join(format!("{key}.json"))
}

pub(super) fn save_metadata(
    root: &Path,
    id: &str,
    capture: &CommandMetadata,
) -> std::io::Result<()> {
    use std::io::Write;
    let path = metadata_path(root, id);
    let dir = root.join("cache/tool-output-metadata");
    std::fs::create_dir_all(&dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec(capture)?)?;
    file.persist(path)?;
    Ok(())
}

pub(super) fn command_preview(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    metadata: Option<&CommandMetadata>,
    enabled: bool,
) -> String {
    command_preview_with_room(name, raw, root, metadata, enabled, 0, &mut None)
}

pub(crate) fn projected_command_notes(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    id: Option<&str>,
    cwd: &Path,
) -> String {
    let Some(root) = root else {
        return String::new();
    };
    if !is_shell_result(name) || raw.len() < LONG_COMMAND_OUTPUT_BYTES {
        return String::new();
    }
    let metadata = id
        .and_then(|id| std::fs::read(metadata_path(root, id)).ok())
        .and_then(|v| serde_json::from_slice(&v).ok());
    let mut digest = None;
    command_preview_with_room(
        name,
        raw,
        Some(root),
        metadata.as_ref(),
        crate::config::command_output::enabled(Some(root), cwd),
        0,
        &mut digest,
    );
    let mut notes = String::new();
    let cleaned = render(name, raw, None);
    if cleaned.len() > MODEL_TOOL_OUTPUT_BYTES {
        if let Ok(path) =
            crate::performance::save_output_in(&root.join("cache/tool-outputs"), cleaned.as_bytes())
        {
            notes = format!(
                "\nSaved tool result ({} bytes, may already be truncated by the tool): {}",
                cleaned.len(),
                super::model_projection::recovery_path(&path)
            );
        }
    }
    if let Some(digest) = digest {
        notes.push_str("\n\n");
        notes.push_str(&digest);
    }
    notes
}

fn command_preview_with_room(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    metadata: Option<&CommandMetadata>,
    enabled: bool,
    reserved: usize,
    digest: &mut Option<String>,
) -> String {
    use command_output_summary::Stream;
    let shell_like = matches!(name, "Bash" | "Python");
    let capture = match metadata {
        Some(CommandMetadata::Shell(s)) => Some(s),
        _ => None,
    };
    let page_bytes = match metadata {
        Some(CommandMetadata::Page { bytes, widths, .. }) => Some((bytes, widths)),
        _ => None,
    };
    let cleaned = render(name, raw, None);
    let mut spans = [None, None];
    let draft = if shell_like && root.is_some() {
        render_with_spans(name, raw, Some(LONG_COMMAND_OUTPUT_BYTES), &mut spans)
    } else {
        if shell_like {
            render_with_spans(name, raw, None, &mut spans);
        }
        cleaned.clone()
    };
    let retain = |limit| match root {
        Some(root) => retain_tool_output_with_ranges(root, &cleaned, &draft, limit),
        None => (draft.clone(), Some(PreviewRanges::whole(draft.len()))),
    };
    let (mut base, retained) = retain(MODEL_TOOL_OUTPUT_BYTES - reserved);
    if !enabled {
        return base;
    }
    let task = matches!(name, "TaskGet" | "TaskOutput");
    let json = raw.strip_prefix("Error: ").unwrap_or(raw);
    let mut input = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(value @ Value::Object(_))) = input.next() else {
        return base;
    };
    if task && value["metadata"]["interaction"] == "pty" {
        return base;
    }
    if !shell_like && value.get("model_output_summary").is_some() {
        return base;
    }
    let json_end = raw.len() - json.len() + input.byte_offset();
    // TaskGet renders managed-shell result strings as stdout, which can itself
    // be JSON. Obtain their status from private capture metadata.
    let nested = if task && value["metadata"]["managed_shell"] != true {
        value["result"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
    } else {
        None
    };
    let command = nested
        .as_ref()
        .or(value["result"]
            .as_object()
            .filter(|_| task)
            .map(|_| &value["result"]))
        .unwrap_or(&value);
    let legacy_capture = &command["output_capture"];
    let strings = |key: &str| value[key].as_str().unwrap_or("");
    let (stdout, stderr) = (strings("stdout"), strings("stderr"));
    let streams = if task && stdout.is_empty() && stderr.is_empty() {
        [("output", strings("output")), ("stderr", stderr)]
    } else {
        [("stdout", stdout), ("stderr", stderr)]
    };
    let page = value["output_mode"] == "page";
    let page_end = value["next_cursor"]
        .as_str()
        .filter(|_| page)
        .and_then(|cursor| crate::tools::output_page::Cursor::decode(cursor).ok());
    let state = value["status"]
        .as_str()
        .or_else(|| (value["timed_out"] == true).then_some("timed out"));
    let mut streams = streams.map(|(key, text)| {
        let index = usize::from(key == "stderr");
        let snapshot = match metadata {
            Some(
                CommandMetadata::Snapshot(streams) | CommandMetadata::ShellSnapshot { streams, .. },
            ) => Some(&streams[index]),
            _ => None,
        };
        let stats = capture.and_then(|capture| match key {
            "stdout" => Some(&capture.stdout),
            "stderr" => Some(&capture.stderr),
            _ => None,
        });
        // Snapshot/page text belongs to this log read. Stored command totals
        // may describe a different or more complete result.
        let legacy = |field: &str| {
            if snapshot.is_some() || page {
                return None;
            }
            usize::try_from(legacy_capture[key][field].as_u64()?).ok()
        };
        let flagged = match key {
            "stdout" => value["stdout_truncated"] == true,
            "stderr" => value["stderr_truncated"] == true,
            _ => false,
        };
        let bytes = snapshot
            .filter(|s| !s.from_tail || s.omitted > 0)
            .map(|s| s.read.saturating_add(s.omitted))
            .or_else(|| stats.map(|s| s.bytes))
            .or_else(|| legacy("bytes"));
        let lines = stats.map(|s| s.lines).or_else(|| legacy("lines"));
        let source_ranges = stats.map(|s| s.ranges.clone()).filter(|ranges| {
            ranges
                .iter()
                .all(|r| r.start <= r.end && text.get(r.clone()).is_some())
        });
        let offset = page_end.as_ref().map(|end| {
            if key == "stderr" {
                end.err()
            } else {
                end.out()
            }
        });
        let partial = snapshot.is_some_and(|s| s.omitted > 0 || s.from_tail)
            || page
            || flagged
            || (task && value["output_truncated"] == true)
            || bytes.is_some_and(|n| n > text.len())
            || state.is_some_and(|state| {
                matches!(state, "running" | "pending" | "starting" | "timed out")
            })
            || value["backgrounded"] == true;
        (
            key,
            Stream {
                text,
                excerpts: stats.and_then(|s| s.excerpts.clone()),
                relative: flagged && source_ranges.is_none()
                    || snapshot.is_some_and(|s| s.from_tail),
                window_bytes: page_bytes
                    .map(|(bytes, _)| bytes[index])
                    .or_else(|| snapshot.map(|s| s.read)),
                bytes,
                lines: lines.or_else(|| {
                    (page || !partial || snapshot.is_some_and(|s| !s.from_tail && s.omitted == 0))
                        .then(|| {
                            crate::performance::command_output::ByteLines::measure(text.as_bytes())
                                .lines()
                        })
                }),
                first_lines: stats.map(|s| s.first_lines.clone()).unwrap_or_default(),
                widths: stats
                    .map(|s| &s.widths)
                    .or_else(|| snapshot.map(|s| &s.widths))
                    .or_else(|| page_bytes.map(|(_, w)| &w[usize::from(key == "stderr")]))
                    .cloned()
                    .unwrap_or_default(),
                source_ranges,
                visible: Vec::new(),
                page_continuation: page
                    && (offset
                        != page_bytes
                            .map(|(bytes, _)| bytes[usize::from(key == "stderr")] as u64)
                            .or_else(|| u64::try_from(text.len()).ok())
                        || page_bytes.is_none() && !value["output_gap"].is_null()),
            },
        )
    });
    // A shell's authoritative source length is measured before replacement.
    // Text-only backends supply UTF-8 stream values as their source.
    let bytes = page_bytes.map(|(bytes, _)| *bytes).unwrap_or_else(|| {
        streams.each_ref().map(|(_, stream)| {
            stream.window_bytes.or(stream.bytes).unwrap_or_else(|| {
                // A legacy Bash transcript may lack raw capture metadata. A
                // replacement could represent just one byte; qualify only
                // when even the lower bound reaches the threshold.
                if shell_like
                    || (task && value["metadata"]["managed_shell"] == true)
                    || value["output_gap"]["reason"] == "invalid_utf8"
                {
                    stream.text.len() - 2 * stream.text.matches('�').count()
                } else {
                    stream.text.len()
                }
            })
        })
    });
    if !crate::performance::command_output::qualifies(bytes) {
        return base;
    }

    let json_result = !shell_like && base == raw && base.len() <= MODEL_TOOL_OUTPUT_BYTES;
    let outer = retained.unwrap_or_else(|| {
        if shell_like {
            // Without a spill, the retained result is the full cleaned output.
            render_with_spans(name, raw, None, &mut spans);
        }
        PreviewRanges::whole(cleaned.len())
    });
    let serialized_elided = if shell_like || json_result {
        0
    } else {
        cleaned.len().saturating_sub(outer.kept())
    };
    // RawValue borrows the original JSON spelling, including escaped keys and
    // strings. Serialized byte cuts must be projected before counting lines.
    let json_fields = (serialized_elided > 0)
        .then(|| {
            serde_json::from_str::<std::collections::HashMap<String, &serde_json::value::RawValue>>(
                &raw[raw.len() - json.len()..json_end],
            )
            .ok()
        })
        .flatten();
    let mut copies = Vec::new();
    if let Some(fields) = &json_fields {
        for (key, field) in fields {
            if let Some(text) = value[key].as_str() {
                let encoded = field.get();
                let start = encoded.as_ptr() as usize - raw.as_ptr() as usize;
                let visible = visible_json_string(text, encoded, start, &outer);
                if !visible.is_empty() {
                    copies.push((key.as_str(), text, visible));
                }
            }
        }
    }
    for ((key, stream), span) in streams.iter_mut().zip(spans) {
        let all = [0..stream.text.len()];
        let sources = stream.source_ranges.as_deref().unwrap_or(&all);
        if json_result {
            stream.visible = sources.to_vec();
        } else if let Some(span) = span {
            stream.visible = visible_ranges(sources, &span, &outer);
            copies.push((*key, stream.text, stream.visible.clone()));
        }
    }
    if !json_result {
        for (name, stream) in &mut streams {
            if stream.text.is_empty() {
                continue;
            }
            for (key, alias, kept) in &copies {
                if key == name {
                    stream.visible.extend_from_slice(kept);
                } else {
                    let managed = task
                        && value["metadata"]["managed_shell"] == true
                        && matches!(*key, "output" | "result");
                    project_alias(name, stream.text, alias, kept, managed, &mut stream.visible);
                }
            }
            stream.visible.sort_unstable_by_key(|r| r.start);
            stream.visible.dedup_by(|next, previous| {
                if next.start > previous.end {
                    return false;
                }
                previous.end = previous.end.max(next.end);
                true
            });
        }
    }

    let duration_ms = capture
        .map(|s| s.duration_ms)
        .or_else(|| legacy_capture["duration_ms"].as_u64())
        .or_else(|| value["duration_ms"].as_u64())
        .or_else(|| {
            value["finished_ms"]
                .as_u64()?
                .checked_sub(value["started_ms"].as_u64()?)
        });
    // A complete JSON message already exposes an explicit duration field.
    let duration_ms = duration_ms.filter(|ms| {
        !(json_result && value["duration_ms"].as_u64() == Some(*ms))
            && !base.contains(&format!(r#""duration_ms":{ms}"#))
    });
    let summary = command_output_summary::summarize(&streams, duration_ms, &base);
    if summary.is_empty() {
        return base;
    }
    let available = MODEL_TOOL_OUTPUT_BYTES
        .saturating_sub(base.len())
        .min(DIGEST_BUDGET);
    let encode = |text: &str| {
        if json_result {
            format!(
                ",\"model_output_summary\":{}",
                serde_json::to_string(text).expect("string serialization")
            )
        } else {
            format!("\n\n{text}")
        }
    };
    let appendix = encode(&summary);
    if appendix.len() > available && root.is_some() && reserved == 0 {
        return command_preview_with_room(
            name,
            raw,
            root,
            metadata,
            enabled,
            DIGEST_BUDGET,
            digest,
        );
    }
    if appendix.len() <= available {
        *digest = Some(
            summary
                .lines()
                .filter(|line| !line.contains(" | "))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if json_result {
            base.insert_str(json_end - 1, &appendix);
        } else {
            base.push_str(&appendix);
        }
    }
    base
}

/// A stored managed result can include status text, labels and the shell's
/// own head/tail cut. Map only verbatim source spans into stream coordinates.
fn project_alias(
    name: &str,
    stream: &str,
    alias: &str,
    kept: &[Range<usize>],
    managed: bool,
    out: &mut Vec<Range<usize>>,
) {
    let mut project = |at: usize, source: Range<usize>| {
        for r in kept {
            let start = r.start.max(at);
            let end = r.end.min(at + source.len());
            if start < end {
                out.push(source.start + (start - at)..source.start + (end - at));
            }
        }
    };
    if alias.contains(stream) {
        for (at, _) in alias.match_indices(stream) {
            project(at, 0..stream.len());
        }
        return;
    }
    if !managed {
        return;
    }
    let label = if name == "stdout" {
        "Output:\n"
    } else {
        "Stderr:\n"
    };
    let body = match alias.split_once(label) {
        Some((_, s)) => s,
        None => alias,
    };
    let body = if name == "stdout" {
        body.split_once("\nStderr:\n").map_or(body, |(s, _)| s)
    } else {
        body
    };
    let at = body.as_ptr() as usize - alias.as_ptr() as usize;
    // An alias may have been clipped before TaskGet read its log. Its prefix
    // and suffix have known origins only when the source bytes agree.
    if let Some((head, cut)) = body.split_once("\n... (") {
        if let Some((marker, tail)) = cut.split_once(") ...\n") {
            if marker.starts_with("output truncated; ")
                || marker == "capture truncated; middle omitted"
            {
                if stream.starts_with(head) {
                    project(at, 0..head.len());
                }
                if stream.ends_with(tail) {
                    project(
                        at + body.len() - tail.len(),
                        stream.len() - tail.len()..stream.len(),
                    );
                }
            }
        }
    }
}

/// Map complete visible JSON string scalars back to the decoded stream.
fn visible_json_string(
    text: &str,
    encoded: &str,
    start: usize,
    outer: &PreviewRanges,
) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut pos = 1; // opening quote
    for (offset, ch) in text.char_indices() {
        let width = match encoded.as_bytes().get(pos..) {
            Some([b'\\', b'u', ..]) if ch.len_utf8() == 4 => 12, // surrogate pair
            Some([b'\\', b'u', ..]) => 6,
            Some([b'\\', ..]) => 2,
            _ => ch.len_utf8(),
        };
        let end = pos + width;
        if [&outer.head, &outer.tail]
            .iter()
            .any(|r| r.start <= start + pos && start + end <= r.end)
        {
            if let Some(last) = spans.last_mut().filter(|r| r.end == offset) {
                last.end += ch.len_utf8();
            } else {
                spans.push(offset..offset + ch.len_utf8());
            }
        }
        pos = end;
    }
    spans
}

fn visible_ranges(
    sources: &[Range<usize>],
    span: &BodySpan,
    outer: &PreviewRanges,
) -> Vec<Range<usize>> {
    let mut visible = Vec::new();
    for (inner, offset) in span.ranges.parts() {
        let start = span.start + offset;
        for (kept, _) in outer.parts() {
            for source in sources {
                let from = inner
                    .start
                    .max(source.start)
                    .max(inner.start + kept.start.saturating_sub(start));
                let to = inner
                    .end
                    .min(source.end)
                    .min(inner.start + kept.end.saturating_sub(start));
                if from < to {
                    visible.push(from..to);
                }
            }
        }
    }
    visible
}

/// Compare a requested interpreter with the resolved path. A bare program name
/// differs from the absolute path it resolved to; an absolute path is unchanged.
fn same_path(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        let value = value.replace('\\', "/");
        let value = value.trim_end_matches('/');
        if cfg!(windows) {
            value.to_ascii_lowercase()
        } else {
            value.to_owned()
        }
    };
    normalize(left) == normalize(right)
}

/// The executable name a shell path or request resolves to. Both separators
/// are accepted because a stored result can be replayed on another platform.
fn shell_stem(value: &str) -> String {
    let name = value
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(value);
    let name = if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".exe")
    {
        &name[..name.len() - 4]
    } else {
        name
    };
    name.to_ascii_lowercase()
}

/// Whether the shell that actually ran is the one that was requested. The
/// names are compared as executable stems, so `powershell.exe` matches a
/// request for `powershell` while `pwsh` stays distinct.
fn shell_matches(requested: &str, shell_path: &str) -> bool {
    shell_stem(requested) == shell_stem(shell_path)
}

#[cfg(test)]
#[path = "model_projection_tests.rs"]
mod projection_tests;

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::tools::bash_process::ShellCapture;

    #[test]
    fn apply_patch_keeps_paths_and_counts_but_not_per_file_diffs() {
        let raw = json!({
            "success": false,
            "content": "Applied 1 change(s); 1 could not be restored",
            "applied": [{"path":"/w/a.txt","raw_path":"a.txt","action":"updated","destination":null}],
            "unchanged": [],
            "rolled_back": [],
            "unrestored": [{"path":"/w/b.txt","raw_path":"b.txt","message":"external edit","backup":"/w/.b.backup"}],
            "changes": [{
                "path": "/w/a.txt",
                "raw_path": "a.txt",
                "action": "updated",
                "file_change": {"version": 1, "added": 1, "removed": 1, "rows": [{"text": "secret diff row"}]}
            }],
            "changes_truncated": false,
            "file_change": {"version": 1, "added": 1, "removed": 1}
        })
        .to_string();
        let rendered = render("ApplyPatch", &raw, None);
        let value: Value = serde_json::from_str(&rendered).unwrap();
        // Unresolved paths and counts survive; bulk diff rows do not.
        assert_eq!(value["unrestored"][0]["path"], "/w/b.txt");
        assert_eq!(value["changes"][0]["path"], "/w/a.txt");
        assert!(value["changes"][0].get("file_change").is_none());
        assert!(value.get("file_change").is_none());
        assert!(!rendered.contains("secret diff row"));
        // Hook context after the JSON is preserved verbatim.
        let with_context = format!("{raw}\nPreToolUse context: keep me");
        let rendered = render("ApplyPatch", &with_context, None);
        assert!(rendered.contains("keep me"), "{rendered}");
    }

    #[test]
    fn model_tool_output_preserves_payload_errors_and_hook_suffix() {
        let raw = json!({"stdout":"{\"user_json\": 1}\n", "stderr":"", "exit_code":0,"success":true,"shell":"bash","stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(render("Bash", &raw, None), "{\"user_json\": 1}\n");
        assert_eq!(render("mcp__fixture__Bash", &raw, None), raw);
        let error = format!(
            "Error: {}\nPostToolUseFailure context: inspect dependency",
            json!({"stdout":"partial","stderr":"last error","exit_code":7,"success":false,"timed_out":true,"stdout_truncated":true,"task_id":"task-1"})
        );
        let rendered = render("Bash", &error, None);
        for expected in [
            "Exit code: 7",
            "Command timed out",
            "Stdout is incomplete",
            "last error",
            "task-1",
            "PostToolUseFailure context: inspect dependency",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        assert_eq!(
            render("Bash", "Error: Starting shell command", None),
            "Error: Starting shell command"
        );
        let read = json!({"content":"   51\tcode", "lines":100,"truncated":true,"lsp_diagnostics":{"error":"missing name"}}).to_string();
        let rendered = render("Read", &read, None);
        assert!(rendered.contains("more lines remain"));
        assert!(rendered.contains("missing name"));
        assert!(rendered.ends_with("   51\tcode"));
    }

    #[test]
    fn changed_shell_directory_is_reported_above_stdout() {
        let raw = json!({
            "stdout": "a\n",
            "stderr": "",
            "cwd": "/repo/deepseek-code-data-layout",
            "exit_code": 0
        })
        .to_string();
        let rendered = render("Bash", &raw, None);
        assert!(rendered.contains("cwd: /repo/deepseek-code-data-layout"), "{rendered}");
        assert!(
            rendered.find("cwd: /repo/deepseek-code-data-layout")
                < rendered.find("Output:\na\n"),
            "{rendered}"
        );
    }

    #[test]
    fn model_tool_output_short_or_disabled_shell_output_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let stdout = include_str!("test_summary_fixtures/vitest-pass.stdout");
        let raw = format!(
            "{}\nHook context: preserve this",
            json!({"stdout":stdout, "stderr":"diagnostic", "exit_code":0})
        );
        let original = render("Bash", &raw, None);
        assert_eq!(engine.retained_tool_output("Bash", &raw), original);
        assert_eq!(engine.retained_tool_output("mcp__fixture__Bash", &raw), raw);
        let hook_only = format!("{}\n{stdout}", json!({"stdout":"hello", "exit_code":0}));
        assert_eq!(
            engine.retained_tool_output("Bash", &hook_only),
            render("Bash", &hook_only, None)
        );
        let raw = json!({"stdout":"line\n".repeat(3000), "exit_code":0}).to_string();
        let original = render("Bash", &raw, None);
        assert_eq!(command_preview("Bash", &raw, None, None, false), original);
    }

    #[test]
    fn model_tool_output_short_literal_capture_marker_is_exact() {
        let stdout = "... (capture truncated; middle omitted) ...\n";
        assert_eq!(stdout.len(), 44);
        let raw = json!({"stdout":stdout,"stderr":"","stdout_truncated":false,"stderr_truncated":false,"success":true,"exit_code":0,"shell":"bash"}).to_string();
        let temp = tempfile::tempdir().unwrap();
        let expected = stdout;
        assert_eq!(
            command_preview("Bash", &raw, Some(temp.path()), None, true),
            expected
        );
        assert_eq!(
            command_preview("Bash", &raw, Some(temp.path()), None, false),
            expected
        );
    }

    #[test]
    fn model_tool_output_command_lines_follow_unlabeled_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = format!(
            "error: head\n{}\nfatal: middle\n{}\nFAILED tail\n",
            "h".repeat(8000),
            "t".repeat(8000)
        );
        let raw = json!({"stdout":stdout,"stderr":"","shell":"bash","shell_path":"/bin/bash","success":true,"exit_code":0,"pipeline_sigpipe":false}).to_string();
        let result = command_preview("Bash", &raw, Some(dir.path()), None, true);
        let (body, summary) = result.split_once("\n\nOmitted output:").unwrap();
        assert!(body.starts_with("error: head\n"));
        assert!(!body.contains("fatal: middle"));
        assert!(!summary.contains("error: head"));
        assert!(summary.contains(r#"stdout L3 | "fatal: middle""#));
        assert!(!summary.contains("FAILED tail"));
    }

    #[test]
    fn model_tool_output_preserves_long_console_task_outcomes_and_managed_shell_status() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = r#"{"exit_code":91,"final_drain_complete":true}"#;
        let mut task = json!({"task_id":"fixture","stdout":"x".repeat(12000),"stderr":"","output":outcome,"result":outcome,"metadata":{"managed_shell":true,"interaction":"pty"}});
        for name in ["TaskGet", "TaskOutput"] {
            let raw = task.to_string();
            assert_eq!(
                command_preview(name, &raw, Some(dir.path()), None, true),
                raw
            );
        }
        // A plain managed shell can print JSON with a fabricated exit code.
        task["metadata"]["interaction"] = json!("pipe");
        let raw = task.to_string();
        assert_eq!(
            command_preview("TaskGet", &raw, Some(dir.path()), None, true),
            raw
        );
    }

    #[test]
    fn model_tool_output_counts_source_once_across_snapshot_aliases() {
        let dir = tempfile::tempdir().unwrap();
        for count in [600, 1100] {
            let stdout = (0..count)
                .map(|i| format!("row-{i:04}-diagnostic\n"))
                .collect::<String>();
            let raw = json!({"status":"completed","metadata":{"managed_shell":true},"stdout":stdout,"stderr":"","output":stdout,"result":stdout}).to_string();
            let view = command_preview("TaskGet", &raw, Some(dir.path()), None, true);
            let (body, appendix) = view
                .split_once("\n\nOmitted output:")
                .unwrap_or((&view, ""));
            let (head, tail) = body.split_once("\n... (output truncated; ").unwrap();
            let tail = tail
                .split_once(" bytes before preview) ...\n")
                .unwrap()
                .1
                .split_once("\nSaved tool result")
                .unwrap()
                .0;
            // Independent oracle: locate every ASCII source byte in each
            // actual JSON field, counting a two-byte LF escape atomically.
            let mut seen = vec![false; stdout.len()];
            for field in ["stdout", "result", "output"] {
                let key = format!(r#""{field}":""#);
                let mut pos = raw.find(&key).unwrap() + key.len();
                for (i, ch) in stdout.bytes().enumerate() {
                    let next = pos + if ch == b'\n' { 2 } else { 1 };
                    seen[i] |= next <= head.len() || pos >= raw.len() - tail.len();
                    pos = next;
                }
            }
            let missing = seen.iter().filter(|b| !**b).count();
            if count == 600 {
                assert_eq!(missing, 0);
                assert!(appendix.is_empty());
            } else {
                let lines = seen
                    .chunks_exact(20)
                    .filter(|line| !line.contains(&true))
                    .count();
                assert_eq!(
                    appendix.trim(),
                    format!("stdout: {missing} bytes, {lines} whole lines")
                );
            }
        }
    }

    #[test]
    fn model_tool_output_elapsed_requires_ten_seconds_and_hidden_information() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = vec![b'x'; 12_000];
        let (raw, capture) =
            crate::tools::bash_process::fixture_capture([&stdout, b""], 100_000, 100_000, 0);
        let mut meta = capture.unwrap();
        for ms in [9999, 10000] {
            let CommandMetadata::Shell(ref mut capture) = meta else {
                unreachable!()
            };
            capture.duration_ms = ms;
            let view = command_preview("Bash", &raw, Some(dir.path()), Some(&meta), true);
            if ms == 9999 {
                assert_eq!(view.as_bytes(), stdout);
            } else {
                assert_eq!(view, format!("{}\n\nElapsed: 10000 ms", "x".repeat(12_000)));
            }
        }
    }

    #[test]
    fn model_tool_output_reviewed_capture_counts_exclude_markers_and_keep_excerpts_unique() {
        for large in [false, true] {
            let (stdout, capture) = crate::tools::bash_process::review_capture(large);
            let raw = json!({"stdout":stdout,"stderr":"","stdout_truncated":true,"exit_code":0})
                .to_string();
            let temp = tempfile::tempdir().unwrap();
            let metadata = CommandMetadata::Shell(capture);
            let result = command_preview("Bash", &raw, Some(temp.path()), Some(&metadata), true);
            let CommandMetadata::Shell(capture) = metadata else {
                unreachable!()
            };
            if !large {
                assert_eq!(result, render("Bash", &raw, None));
                continue;
            }
            let (body, summary) = result.split_once("\n\nOmitted output:").unwrap();
            let stream = &capture.stdout;
            assert_eq!((stream.bytes, stream.lines), (52_428_888, 819_204));
            let (_, stdout) = body.split_once("Output:\n").unwrap();
            let (stdout, _) = stdout.rsplit_once("\nSaved tool result (").unwrap();
            let (head, clipped) = stdout.split_once("\n... (output truncated; ").unwrap();
            let (_, tail) = clipped.split_once(" bytes before preview) ...\n").unwrap();
            let shown = head.len() + tail.len();
            let lines = head.lines().count() + tail.lines().count();
            // One invalid source byte became three UTF-8 preview bytes.
            assert!(
                summary.contains(&format!(
                    "stdout: {} bytes, {} whole lines",
                    stream.bytes - (shown - 2),
                    stream.lines - lines
                )),
                "{summary}"
            );
            assert!(!summary.contains("stderr:"));
            assert_eq!(
                result.matches("warning: final fixture").count(),
                1,
                "{summary}"
            );
            assert!(
                summary.contains(r#"stdout L409602 | "error: midpoint sentinel""#),
                "{summary}"
            );
            assert!(result.len() <= MODEL_TOOL_OUTPUT_BYTES);
        }
    }

    #[tokio::test]
    async fn model_tool_output_desktop_runtime_uses_selected_data_dir_for_opt_out() {
        use crate::native_engine::{engine_core::ChatRequest, provider_manager::ProviderManager};
        use crate::runtime;
        use axum::{routing::post, Json, Router};

        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = runtime::SessionStore::new(runtime.open_database().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, mut requests) = mpsc::channel(4);
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let tx = tx.clone();
            async move {
                tx.send(body).await.unwrap();
                ([("content-type", "text/event-stream")], concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolved = executor(&workspace).provider;
        let mut provider = resolved.provider;
        provider.models = vec![resolved.model];
        provider.base_url = base;
        let providers = ProviderManager::in_memory(
            vec![provider],
            runtime.layout().config_file("providers.json"),
        );
        // The Desktop Bridge builds its NativeEngine with this same data-root path.
        let engine = runtime::create_engine(
            &runtime,
            store.db.clone(),
            Arc::new(Mutex::new(providers)),
            runtime::default_permissions(),
            Arc::new(crate::cache::FileCache::new()),
        );
        for enabled in [true, false] {
            if !enabled {
                crate::config::command_output::set(&runtime.data_dir, false).unwrap();
            }
            let id = store.create(&workspace, "fixture", "fixture").unwrap();
            let raw = json!({"stdout": "test line\n".repeat(1400), "exit_code":0}).to_string();
            let request: ChatRequest = serde_json::from_value(json!({
                "conversation_id": id, "provider_id": "fixture", "model": "fixture",
                "workspace_path": workspace,
                "messages": [
                    {"role":"user","content":"run"},
                    {"role":"assistant","content":"","tool_calls":[
                        {"id":"fixture-shell","type":"function","function":{"name":"Bash","arguments":"{}"}}
                    ]},
                    {"role":"tool","tool_call_id":"fixture-shell","content":raw},
                    {"role":"user","content":"continue"}
                ]
            })).unwrap();
            let mut events = engine.send_message(request).await.unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    match events.recv().await.unwrap() {
                        EngineEvent::MessageStop { .. } => break,
                        EngineEvent::Error(message) => panic!("{message}"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            engine.finish_turn(&id).await;
            let request = requests.recv().await.unwrap();
            let rendered = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert_eq!(rendered.contains("Omitted output:"), enabled, "{rendered}");
        }
        server.abort();
    }

    #[test]
    fn model_tool_output_long_powershell_and_taskget_keep_their_json_contract() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        for name in ["PowerShell", "TaskGet"] {
            let raw = json!({
                "stdout":format!("{}\nlast line", ".".repeat(12_300)),
                "stderr":"warning: retrying\nerror: stopped",
                "exit_code":3,
                "duration_ms":129,
                "next_cursor":"fixture"
            });
            let rendered = engine.retained_tool_output(name, &raw.to_string());
            assert_eq!(engine.retained_tool_output(name, &rendered), rendered);
            assert_eq!(rendered, raw.to_string());
        }

        let short_page = json!({"stdout":"tiny","output_mode":"page","output_truncated":true,"next_cursor":"fixture"}).to_string();
        assert_eq!(
            engine.retained_tool_output("TaskGet", &short_page),
            short_page
        );
    }

    #[test]
    fn model_tool_output_spilled_taskget_keeps_preview_and_uses_nested_status() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let raw = json!({
            "stdout":"line\n".repeat(6000),
            "stderr":"error: failed",
            "status":"failed",
            "result":json!({"exit_code":7,"output_capture":{"duration_ms":314}}).to_string(),
        })
        .to_string();
        let rendered = engine.retained_tool_output("TaskGet", &raw);
        assert!(rendered.len() <= MODEL_TOOL_OUTPUT_BYTES);
        let (preview, summary) = rendered.split_once("\n\nOmitted output:").unwrap();
        let (_, file) = preview.rsplit_once("): ").unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), raw);
        assert!(summary.contains("stdout: ") && summary.contains("whole lines"));
        assert!(
            !summary.contains("Exit")
                && !summary.contains("stderr")
                && !summary.contains("Elapsed")
        );
    }

    #[test]
    fn model_tool_output_complete_near_capacity_json_stays_intact() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let raw = json!({"stdout":".".repeat(15_900), "next_cursor":"fixture", "exit_code":0})
            .to_string();
        assert!(raw.len() < MODEL_TOOL_OUTPUT_BYTES);
        assert_eq!(engine.retained_tool_output("TaskGet", &raw), raw);
    }

    #[test]
    fn model_tool_output_shows_only_output_and_required_status() {
        // A lone stdout body needs neither its label nor an exit-code line.
        let raw = json!({"stdout":"ok\n","stderr":"","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash","pipeline_sigpipe":false,"stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok\n");
        // Two streams keep their labels so the model can tell them apart.
        let raw = json!({"stdout":"out","stderr":"err","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "Output:\nout\nStderr:\nerr");
        let raw = json!({"stdout":"","stderr":"err\n","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "Stderr:\nerr\n");
        // A status line keeps the label even for a lone stdout body.
        let raw = json!({"stdout":"ok\n","stderr":"","exit_code":0,"success":true,"stdout_truncated":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Stdout is incomplete (capture or tool limit)\nOutput:\nok\n"
        );
        // A silent success must not read as an unfinished task.
        let raw = json!({"stdout":"","stderr":"","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash","pipeline_sigpipe":false,"stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(render("Bash", &raw, None), "(Completed with no output)");
        // A failure still names its non-zero code.
        let raw = json!({"stdout":"","stderr":"boom\n","exit_code":2,"success":false,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "Exit code: 2\nStderr:\nboom\n");
        // A failure without a code keeps the existing line.
        let raw = json!({"stderr":"denied\n","success":false,"shell":"bash"}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Command failed\nStderr:\ndenied\n"
        );
    }

    #[test]
    fn python_output_consumes_interpreter_and_script_with_lowercase_labels() {
        assert!(is_shell_result("Python"));
        let script = r"D:\project\temp\python\script\001.py";
        let result = |interpreter: &str, source: &str, requested: Option<&str>| {
            let mut value = json!({
                "success": true,
                "exit_code": 0,
                "stdout": "hello\n",
                "stderr": "",
                "stdout_truncated": false,
                "stderr_truncated": false,
                "shell": "python",
                "interpreter": interpreter,
                "interpreter_source": source,
                "interpreter_changed": requested.is_some_and(|requested| requested != interpreter)
                    || (requested.is_none() && source == "venv"),
                "script_saved": true,
                "script_path": script
            });
            if let Some(requested) = requested {
                value["interpreter_requested"] = json!(requested);
            }
            render("Python", &value.to_string(), None)
        };
        // A workspace virtualenv is the default, but it replaced PATH silently.
        assert_eq!(
            result("/usr/bin/python3", "venv", None),
            format!("interpreter: /usr/bin/python3\nscript: {script}\nstdout:\nhello\n")
        );
        // An absolute request that resolved to itself is not news.
        assert_eq!(
            result("/usr/bin/python3", "parameter", Some("/usr/bin/python3")),
            format!("script: {script}\nstdout:\nhello\n")
        );
        // A bare name and a conda environment resolve to a concrete path.
        assert_eq!(
            result("/opt/py/bin/python", "parameter", Some("python")),
            format!("interpreter: /opt/py/bin/python\nscript: {script}\nstdout:\nhello\n")
        );
        assert_eq!(
            result(r"C:\envs\data\python.exe", "conda", Some("data")),
            format!("interpreter: C:\\envs\\data\\python.exe\nscript: {script}\nstdout:\nhello\n")
        );
        // A PATH default needs no line, and a temporary script has no path.
        let temporary = json!({
            "success": true,
            "exit_code": 0,
            "stdout": "hello\n",
            "stderr": "",
            "shell": "python",
            "interpreter": "/usr/bin/python3",
            "interpreter_source": "PATH",
            "script_saved": false
        })
        .to_string();
        assert_eq!(render("Python", &temporary, None), "stdout:\nhello\n");

        // A default `.venv` is already in the environment block, so it is not
        // news even though the source is `venv`.
        let default_venv = json!({
            "success": true,
            "exit_code": 0,
            "stdout": "hello\n",
            "interpreter": "/repo/.venv/bin/python",
            "interpreter_source": "venv",
            "interpreter_changed": false,
            "script_saved": false
        })
        .to_string();
        assert_eq!(render("Python", &default_venv, None), "stdout:\nhello\n");

        let failed = json!({
            "success": false,
            "exit_code": 1,
            "stdout": "",
            "stderr": "Traceback (most recent call last):\n",
            "interpreter": "/usr/bin/python3",
            "interpreter_source": "PATH",
            "script_saved": true,
            "script_path": script
        })
        .to_string();
        assert_eq!(
            render("Python", &failed, None),
            format!(
                "script: {script}\nExit code: 1\nstderr:\nTraceback (most recent call last):\n"
            )
        );

        let empty = json!({
            "success": true,
            "exit_code": 0,
            "stdout": "",
            "stderr": "",
            "interpreter": "/usr/bin/python3",
            "interpreter_source": "PATH",
            "script_saved": true,
            "script_path": script
        })
        .to_string();
        assert_eq!(
            render("Python", &empty, None),
            format!("script: {script}\n(Completed with no output)")
        );
        for hidden in [
            "Metadata:",
            "interpreter_source",
            "interpreter_changed",
            "script_saved",
            "shell",
        ] {
            assert!(
                !render("Python", &failed, None).contains(hidden),
                "{hidden}"
            );
        }
    }

    #[test]
    fn model_tool_output_explains_a_pipeline_cut_short() {
        let raw = json!({"stdout":"first\n","stderr":"","exit_code":0,"success":true,"pipeline_sigpipe":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        let rendered = render("Bash", &raw, None);
        assert_eq!(
            rendered,
            "Output ended early: a later command in the pipeline closed it\nOutput:\nfirst\n"
        );
        assert!(!rendered.contains("pipeline_sigpipe"), "{rendered}");
        assert!(!rendered.contains("Metadata:"), "{rendered}");
        // A normal success never mentions the flag.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"pipeline_sigpipe":false,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok");
    }

    #[test]
    fn model_tool_output_names_only_a_fallback_shell() {
        // The running executable matches the request, so its path stays hidden.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"shell":"pwsh","shell_path":"C:\\Program Files\\PowerShell\\7\\pwsh.exe"}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok");
        // Windows PowerShell is not a request for pwsh.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"shell":"pwsh","shell_path":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Shell: C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe (requested pwsh)\nOutput:\nok"
        );
        // Case and the .exe suffix do not make powershell a fallback.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"shell":"PowerShell","shell_path":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.EXE"}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok");
    }

    #[test]
    fn model_tool_output_keeps_unknown_fields_in_metadata() {
        let raw = json!({"stdout":"ok\n","stderr":"","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash","task_id":"task-1","note":"kept"}).to_string();
        let rendered = render("Bash", &raw, None);
        assert!(rendered.starts_with("Metadata: {"), "{rendered}");
        assert!(rendered.contains("\"note\":\"kept\""), "{rendered}");
        assert!(rendered.contains("\"task_id\":\"task-1\""), "{rendered}");
        assert!(rendered.ends_with("Output:\nok\n"), "{rendered}");
        assert!(!rendered.contains("shell_path"), "{rendered}");
        assert!(!rendered.contains("exit_code"), "{rendered}");
    }

    #[test]
    fn model_tool_output_leaves_task_results_alone() {
        // TaskGet/TaskOutput snapshots keep their structured shape here; the
        // managed-shell payload inside them is rendered by the task layer.
        let snapshot = json!({"task_id":"task-1","status":"completed","stdout":"done\n","stderr":"","result":"{\"exit_code\":0,\"shell_path\":\"/bin/bash\"}"}).to_string();
        assert_eq!(render("TaskGet", &snapshot, None), snapshot);
        assert_eq!(render("TaskOutput", &snapshot, None), snapshot);
    }

    #[test]
    fn tool_load_reply_is_the_content_line_without_loaded_schemas() {
        let raw = json!({
            "success": true,
            "loaded_tools": ["WebFetch"],
            "tools": [{
                "name": "WebFetch",
                "description": "Fetch a URL and return its readable text.",
                "input_schema": {"type":"object","properties":{"url":{"type":"string"}}}
            }],
            "content": "Loaded WebFetch. Available from the next request."
        })
        .to_string();
        assert_eq!(
            render("ToolLoad", &raw, None),
            "Loaded WebFetch. Available from the next request."
        );
        assert!(!render("ToolLoad", &raw, None).contains("input_schema"));

        let failed = json!({
            "success": false,
            "loaded_tools": [],
            "tools": [],
            "unknown": ["Nope"],
            "unavailable": [{"name":"computer_use","reason":"disabled by tool settings"}],
            "content": "Unknown tools: Nope. Unavailable tools: computer_use (disabled by tool settings).",
        })
        .to_string();
        let rendered = render("ToolLoad", &failed, None);
        assert_eq!(
            rendered,
            "Unknown tools: Nope. Unavailable tools: computer_use (disabled by tool settings)."
        );
        assert!(!rendered.contains("\"unknown\""), "{rendered}");

        // Hook context appended after the JSON survives the lean rendering.
        let with_context = format!("{raw}\nPostToolUse context: keep me");
        assert_eq!(
            render("ToolLoad", &with_context, None),
            "Loaded WebFetch. Available from the next request.\nPostToolUse context: keep me"
        );
        // A non-JSON legacy result is left as it is.
        assert_eq!(render("ToolLoad", "plain text", None), "plain text");
    }

    pub(super) fn executor(root: &std::path::Path) -> ToolLoopExecutor {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (tx, _rx) = mpsc::channel(16);
        ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            root.to_string_lossy().into(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root).config_file("config.json"),
        )
    }

    #[test]
    fn model_tool_output_replay_matches_fresh_for_both_protocols_and_forks() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let id = store.create(temp.path(), "fixture", "fixture").unwrap();
        let assistant = store.append(&id, "assistant", "").unwrap();
        let stdout = format!(
            "START\n{}\nTEST SUMMARY\n{}",
            "中🙂".repeat(30_000),
            include_str!("test_summary_fixtures/vitest-pass.stdout")
        );
        let raw = format!(
            "Error: {}\nHook context: keep this",
            json!({"stdout":stdout,"stderr":"FINAL ERROR","exit_code":7,"success":false})
        );
        let occurrence = "inv-fixture-occurrence";
        let row_id = format!("{assistant}:{occurrence}");
        let stream = |text: &str| crate::tools::bash_process::StreamCapture {
            bytes: text.len(),
            lines: text.lines().count(),
            first_lines: vec![0],
            excerpts: None,
            widths: Default::default(),
            capture_elided_bytes: 0,
            tool_elided_text_bytes: 0,
            ranges: std::iter::once(0..text.len()).collect(),
        };
        let capture = ShellCapture {
            duration_ms: 314,
            stdout: stream(&stdout),
            stderr: stream("FINAL ERROR"),
        };
        save_metadata(
            &paths.data_dir,
            occurrence,
            &CommandMetadata::Shell(capture),
        )
        .unwrap();
        store
            .db
            .fixture_shard(&id, |conn| {
                crate::db::message_repo::insert_tool_call(
                    conn,
                    &row_id,
                    &assistant,
                    "Bash",
                    Some("{}"),
                    Some(&raw),
                    true,
                    0,
                )
            })
            .unwrap()
            .unwrap();
        let mut engine = executor(&paths.data_dir);
        let fresh = engine.retained_tool_output_for_id("Bash", &raw, Some(occurrence));
        assert!(fresh.len() <= 16_000);
        for expected in [
            "Exit code: 7",
            "START",
            "TEST SUMMARY",
            "FINAL ERROR",
            "Hook context: keep this",
            "Saved tool result",
            "Omitted output:",
            "stdout: ",
        ] {
            assert!(fresh.contains(expected), "missing {expected}");
        }
        assert!(
            !fresh.contains("Test summary"),
            "nonzero exit with a pass-only footer"
        );
        let fork = store.fork(&id).unwrap();
        for session in [&id, &fork] {
            for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
                engine.messages = store.model_history(session, &format).unwrap();
                let built = match format {
                    ApiFormat::OpenAI => {
                        serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap()
                    }
                    ApiFormat::Anthropic => {
                        serde_json::to_value(engine.build_anthropic_messages().unwrap()).unwrap()
                    }
                };
                let actual = if matches!(format, ApiFormat::OpenAI) {
                    &built[1]["content"]
                } else {
                    &built[1]["content"][0]["content"]
                };
                assert_eq!(actual, &json!(fresh));
            }
        }
        assert_eq!(
            std::fs::read_dir(paths.data_dir.join("cache/tool-outputs"))
                .unwrap()
                .count(),
            1
        );
        let spill = std::fs::read_dir(paths.data_dir.join("cache/tool-outputs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(spill).unwrap(),
            render("Bash", &raw, None)
        );
        let stored = store
            .db
            .fixture_shard(&id, |conn| crate::db::message_repo::get_tool_calls_for_message(conn, &assistant))
            .unwrap()
            .unwrap();
        assert_eq!(stored[0].output.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn model_tool_output_mcp_text_budget_preserves_images_and_opaque_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let mut content =
            json!([{"type":"text","text":"头".repeat(20_000)},{"type":"text","text":"TAIL"}]);
        engine.retain_content("mcp__fixture__logs", &mut content);
        assert!(content.as_str().unwrap().len() <= 16_000);
        assert!(content.as_str().unwrap().contains("TAIL"));
        for original in [
            json!([{"type":"image","source":{"data":"abc"}},{"type":"text","text":"caption"}]),
            json!([{"type":"text","text":"signed","signature":"opaque"}]),
        ] {
            let mut content = original.clone();
            engine.retain_content("mcp__fixture__logs", &mut content);
            assert_eq!(content, original);
        }
    }

    #[tokio::test]
    async fn model_tool_output_reaches_actual_provider_requests() {
        use axum::{http::header, routing::post, Json, Router};
        for format in ["openai", "anthropic"] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source.txt");
            std::fs::write(&source, "source line\n").unwrap();
            let calls = vec![
                (
                    "shell",
                    "Bash",
                    json!({"command":concat!(
                        "node -e \"process.stdout.write('START'+ 'x'.repeat(200000)+'TEST SUMMARY\\n",
                        "    Running tests/fail.rs (target/debug/deps/fail-abc)\\n\\nrunning 1 test\\n",
                        "test failing_case ... FAILED\\n",
                        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\\n",
                        "');process.stderr.write('FINAL ERROR');process.exitCode=7\"",
                    )}),
                ),
                ("read", "Read", json!({"file_path":source})),
            ];
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let requests = captured.clone();
            let app = Router::new().route(if format == "anthropic" { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                let calls = calls.clone();
                async move {
                    let mut requests = requests.lock().await;
                    let first = requests.is_empty();
                    requests.push(body);
                    let mut events = Vec::new();
                    if format == "anthropic" {
                        events.push(json!({"type":"message_start","message":{"id":"fixture","model":"fixture","usage":{"input_tokens":1}}}));
                        if first {
                            for (index, (id, name, input)) in calls.iter().enumerate() {
                                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
                                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
                                events.push(json!({"type":"content_block_stop","index":index}));
                            }
                        } else {
                            events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}));
                            events.push(json!({"type":"content_block_stop","index":0}));
                        }
                        events.push(json!({"type":"message_delta","delta":{"stop_reason":if first {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}));
                        events.push(json!({"type":"message_stop"}));
                    } else if first {
                        let calls: Vec<_> = calls.iter().enumerate().map(|(index, (id, name, input))| json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect();
                        events.push(json!({"id":"fixture","model":"fixture","choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}));
                        events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
                    } else {
                        events.push(json!({"id":"fixture","model":"fixture","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}));
                        events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
                    }
                    let mut stream = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                    if format == "openai" { stream.push_str("data: [DONE]\n\n"); }
                    ([(header::CONTENT_TYPE, "text/event-stream")], stream)
                }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut engine = executor(temp.path());
            let (tx, _events) = mpsc::channel(128);
            engine.event_tx = tx;
            engine.provider.provider.base_url = base;
            engine.provider.provider.api_format = if format == "anthropic" {
                ApiFormat::Anthropic
            } else {
                ApiFormat::OpenAI
            };
            engine.messages = vec![json!({"role":"user","content":"run fixture"})];
            let result = tokio::time::timeout(Duration::from_secs(20), engine.execute()).await;
            server.abort();
            assert_eq!(result.unwrap().unwrap().0, "done");
            let requests = captured.lock().await;
            assert_eq!(requests.len(), 2);
            let mut outputs = Vec::new();
            for message in requests[1]["messages"].as_array().unwrap() {
                if message["role"] == "tool" {
                    outputs.push(message["content"].as_str().unwrap());
                }
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        if block["type"] == "tool_result" {
                            outputs.push(block["content"].as_str().unwrap());
                        }
                    }
                }
            }
            assert_eq!(outputs.len(), 2);
            for expected in [
                "Exit code: 7",
                "START",
                "TEST SUMMARY",
                "FINAL ERROR",
                "Stdout is incomplete",
            ] {
                assert!(
                    outputs[0].contains(expected),
                    "{format}: missing {expected}: {}",
                    outputs[0]
                );
            }
            assert!(outputs[0].len() <= 16_000);
            assert!(
                !outputs[0].contains("Test summary") && outputs[0].contains("stdout: "),
                "{format}: {}",
                outputs[0]
            );
            let (_, appendix) = outputs[0].split_once("Omitted output:").unwrap();
            assert!(
                !appendix.contains("Exit")
                    && !appendix.contains("Elapsed")
                    && !appendix.contains("stderr")
            );
            assert_eq!(outputs[1], "     1\tsource line");
        }
    }
}

#[cfg(test)]
#[path = "command_output_invariants.rs"]
mod invariants;
