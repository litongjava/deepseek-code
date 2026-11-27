//! Classify complete results before any provider or search window is cut.
use crate::{
    performance::save_output_in,
    tools::base64_collapse::{self, Run},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt::Write, path::Path};

#[derive(Default, Clone, Serialize, Deserialize)]
pub(crate) struct Recovery {
    pub path: String,
    pub spans: Vec<std::ops::Range<usize>>,
}

pub(crate) struct Projected {
    pub text: String,
    pub recovery: Recovery,
}

pub(crate) fn monitor_notification(text: &str) -> bool {
    text.starts_with(crate::tools::background_shell::NOTIFICATION_START)
        && text.lines().any(|line| line == "<kind>monitor</kind>")
}

pub(crate) fn eligible(name: &str) -> bool {
    matches!(name, "Bash" | "TaskGet" | "TaskOutput" | "Monitor")
}

// Map boundaries in decoded text back through JSON escapes in one pass, without
// allocating an offset for every byte of a potentially multi-megabyte result.
struct JsonOffsets<'a> {
    encoded: &'a [u8],
    decoded: &'a str,
    source: usize,
    logical: usize,
}
impl JsonOffsets<'_> {
    fn advance(&mut self, target: usize) -> usize {
        while self.logical < target {
            let count = target - self.logical;
            let window = &self.encoded[self.source..self.source + count];
            let plain = window.iter().position(|b| *b == b'\\').unwrap_or(count);
            self.source += plain;
            self.logical += plain;
            if self.logical < target {
                let ch = self.decoded[self.logical..].chars().next().unwrap();
                self.source += if self.encoded[self.source + 1] == b'u' {
                    6 * ch.len_utf16()
                } else {
                    2
                };
                self.logical += ch.len_utf8();
            }
        }
        self.source
    }
}

fn content_runs(text: &str) -> Vec<Run> {
    let json = text.strip_prefix("Error: ").unwrap_or(text).trim_start();
    if !json.starts_with(['{', '[']) {
        return base64_collapse::runs(text);
    }
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<serde::de::IgnoredAny>();
    if !matches!(stream.next(), Some(Ok(_))) {
        return base64_collapse::runs(text);
    }
    let start = text.len() - json.len();
    let end = start + stream.byte_offset();
    let mut result = Vec::new();
    let mut pos = start;
    while let Some(next) = text[pos..end].find('"') {
        let quote = pos + next;
        let mut token = serde_json::Deserializer::from_str(&text[quote..end]).into_iter::<String>();
        let value = token.next().unwrap().expect("validated JSON string");
        pos = quote + token.byte_offset();
        result.extend(string_runs(&text[quote + 1..pos - 1], &value, quote + 1));
    }
    result.extend(
        base64_collapse::runs(&text[end..])
            .into_iter()
            .map(|mut r| {
                r.range = r.range.start + end..r.range.end + end;
                r
            }),
    );
    result
}

fn string_runs(encoded: &str, value: &str, start: usize) -> Vec<Run> {
    if value.len() < base64_collapse::DATA_URI_THRESHOLD {
        return Vec::new();
    }
    let mut runs = content_runs(value);
    if !runs.is_empty() {
        let mut offsets = JsonOffsets {
            encoded: encoded.as_bytes(),
            decoded: value,
            source: 0,
            logical: 0,
        };
        for run in &mut runs {
            run.range = (start + offsets.advance(run.range.start))
                ..(start + offsets.advance(run.range.end));
        }
    }
    runs
}

/// Block metadata and image payloads are opaque to text projection.
fn typed_runs(raw: &str) -> Option<Vec<Run>> {
    use serde_json::value::RawValue;
    let parts = serde_json::from_str::<Vec<&RawValue>>(raw).ok()?;
    let mut runs = Vec::new();
    for part in parts {
        let fields =
            serde_json::from_str::<std::collections::HashMap<String, &RawValue>>(part.get())
                .ok()?;
        let kind = fields.get("type")?.get();
        if matches!(kind, "\"text\"" | "\"input_text\"") {
            let encoded = fields.get("text")?.get();
            let value = serde_json::from_str::<String>(encoded).ok()?;
            let start = encoded.as_ptr() as usize - raw.as_ptr() as usize + 1;
            runs.extend(string_runs(&encoded[1..encoded.len() - 1], &value, start));
        }
    }
    Some(runs)
}

pub(super) fn recovery_path(path: &Path) -> String {
    if let Some(text) = path.to_str() {
        return text.to_owned();
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut quoted = String::from("$'");
        for byte in path.as_os_str().as_bytes() {
            write!(quoted, "\\x{byte:02x}").unwrap();
        }
        quoted.push('\'');
        quoted
    }
    #[cfg(not(unix))]
    {
        path.display().to_string()
    }
}

pub(crate) fn has_qualifying_result(name: &str, raw: &str) -> bool {
    !result_runs(name, raw).1.is_empty()
}

fn result_runs(name: &str, raw: &str) -> (bool, Vec<Run>) {
    if !eligible(name) || raw.len() < base64_collapse::DATA_URI_THRESHOLD {
        return (false, Vec::new());
    }
    let typed = raw
        .trim_start()
        .starts_with('[')
        .then(|| typed_runs(raw))
        .flatten();
    let blocks = typed.is_some();
    let runs = typed.unwrap_or_else(|| content_runs(raw));
    (blocks, runs)
}

pub(crate) fn project_for_model(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    session: &str,
) -> Option<Projected> {
    let (blocks, runs) = result_runs(name, raw);
    if runs.is_empty() {
        return None;
    }
    let saved = root
        .map(Path::to_owned)
        .map_or_else(crate::runtime::resolve_default_data_dir, Ok)
        .and_then(|root| {
            Ok(save_output_in(
                &std::path::absolute(root)?
                    .join("cache/tool-outputs")
                    .join(format!("{:x}", Sha256::digest(session))),
                raw.as_bytes(),
            )?)
        });
    let path = match saved {
        Ok(path) => recovery_path(&path),
        Err(error) => {
            return Some(Projected {
                text: format!(
                    "[Could not present {name} output: unable to save the original text ({error})]"
                ),
                recovery: Recovery::default(),
            })
        }
    };
    // Insert ASCII tokens first. JSON rendering can unescape a nested result
    // and must never interpret quotes or backslashes from a filesystem path.
    let key = format!("{:x}", Sha256::digest(raw));
    let prefix = format!("!ref-{}!", &key[..16]);
    let mut template = String::new();
    let mut previous = 0;
    let mut line = 1;
    let mut bytes = 0;
    let mut references = Vec::new();
    for run in runs {
        let before = &raw[previous..run.range.start];
        line += before.bytes().filter(|&b| b == b'\n').count();
        template.push_str(before);
        write!(template, "{prefix}{}!", references.len()).unwrap();
        references.push(format!("[base64 data omitted: about {} bytes decoded, looks like {}; full text: {path}, line {line}]",run.decoded_len,run.kind));
        bytes += run.encoded_len;
        previous = run.range.end;
    }
    template.push_str(&raw[previous..]);
    if blocks {
        let mut values: Vec<serde_json::Value> =
            serde_json::from_str(&template).expect("markers preserve JSON syntax");
        if let Some(block) = values
            .iter_mut()
            .rev()
            .find(|b| b["text"].as_str().is_some_and(|s| s.contains(&prefix)))
        {
            let text = block["text"].as_str().unwrap().to_owned()
                + &collapse_note(references.len(), bytes);
            block["text"] = serde_json::Value::String(text);
        }
        let template = serde_json::to_string(&values).expect("JSON value");
        let escaped = references
            .iter()
            .map(|r| {
                let quoted = serde_json::to_string(r).expect("JSON string");
                quoted[1..quoted.len() - 1].to_owned()
            })
            .collect::<Vec<_>>();
        let (text, spans) = expand_references(&template, &prefix, &escaped, false);
        return Some(Projected {
            text,
            recovery: Recovery { path, spans },
        });
    }
    let rendered = super::model_tool_output(name, &template);
    let rendered = if root.is_some() && rendered.len() > crate::performance::MODEL_TOOL_OUTPUT_BYTES
    {
        let (preview, ranges) = crate::performance::preview_with_ranges(
            &rendered,
            crate::performance::MODEL_TOOL_OUTPUT_BYTES,
        );
        let (mut head, mut tail) = (ranges.head.end, ranges.tail.start);
        // Budget cuts cannot introduce a new valid prefix/suffix of an invalid
        // encoded token elsewhere in an otherwise projected result.
        let token = |b: u8| b.is_ascii_alphanumeric() || b"/+_-=".contains(&b);
        let bytes = rendered.as_bytes();
        if bytes.get(head).copied().is_some_and(token) {
            let length = bytes[..head]
                .iter()
                .rev()
                .take_while(|&&b| token(b))
                .count();
            if length >= base64_collapse::DATA_URI_THRESHOLD {
                head -= length;
            }
        }
        if tail > 0 && token(bytes[tail - 1]) {
            let length = bytes[tail..].iter().take_while(|&&b| token(b)).count();
            if length >= base64_collapse::DATA_URI_THRESHOLD {
                tail += length;
            }
        }
        for (at, _) in rendered.match_indices(&prefix) {
            let end = at + prefix.len() + rendered[at + prefix.len()..].find('!').unwrap_or(0) + 1;
            if (at..end).contains(&head) {
                head = at;
            }
            if (at..end).contains(&tail) {
                tail = end;
            }
        }
        format!(
            "{}{}{}",
            &rendered[..head],
            &preview[ranges.head.end..ranges.tail_offset],
            &rendered[tail..]
        )
    } else {
        rendered
    };
    let (mut text, spans) = expand_references(&rendered, &prefix, &references, true);
    text.push_str(&collapse_note(references.len(), bytes));
    Some(Projected {
        text,
        recovery: Recovery { path, spans },
    })
}

fn collapse_note(count: usize, bytes: usize) -> String {
    let blocks = if count == 1 { "block" } else { "blocks" };
    format!("\nbase64: {count} {blocks} collapsed ({bytes} encoded bytes)")
}

fn expand_references(
    rendered: &str,
    prefix: &str,
    references: &[String],
    include_omitted: bool,
) -> (String, Vec<std::ops::Range<usize>>) {
    let mut text = String::with_capacity(rendered.len());
    let mut rest = rendered;
    let mut spans = Vec::new();
    let mut included = vec![false; references.len()];
    while let Some((before, after)) = rest.split_once(prefix) {
        text.push_str(before);
        let Some((index, tail)) = after
            .split_once('!')
            .and_then(|(s, t)| Some((s.parse::<usize>().ok()?, t)))
        else {
            text.push_str(prefix);
            rest = after;
            continue;
        };
        if let Some(reference) = references.get(index) {
            included[index] = true;
            let start = text.len();
            text.push_str(reference);
            spans.push(start..text.len());
        }
        rest = tail;
    }
    text.push_str(rest);
    if include_omitted {
        for (reference, shown) in references.iter().zip(included) {
            if !shown {
                text.push('\n');
                let start = text.len();
                text.push_str(reference);
                spans.push(start..text.len());
            }
        }
    }
    (text, spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_model_boundary_covers_json_object_keys_in_command_output() {
        let temp = tempfile::tempdir().unwrap();
        let payload = "QUJD".repeat(256);
        let stdout = serde_json::json!({&payload: "value"}).to_string();
        let raw = serde_json::json!({"stdout":stdout}).to_string();
        let view = project_for_model("Bash", &raw, Some(temp.path()), "session").unwrap();
        assert!(!view.text.contains(&payload));
        assert_eq!(std::fs::read_to_string(&view.recovery.path).unwrap(), raw);
    }
    #[test]
    fn base64_shell_thresholds_and_recovery_failure() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["Bash", "TaskGet", "TaskOutput", "Monitor"] {
            for (prefix, size) in [("", 1024), ("data:text/plain;base64,", 256)] {
                let full = format!("{prefix}{}", "Q".repeat(size));
                assert!(project_for_model(
                    name,
                    &full[..full.len() - 1],
                    Some(temp.path()),
                    "session"
                )
                .is_none());
                let projected =
                    project_for_model(name, &full, Some(temp.path()), "session").unwrap();
                assert!(projected.text.contains("base64: 1 block collapsed"));
                let blocked = temp.path().join("file");
                std::fs::write(&blocked, "ordinary file").unwrap();
                let failure = project_for_model(name, &full, Some(&blocked), "session").unwrap();
                assert!(failure.text.contains("unable to save"));
                assert!(!failure.text.contains("full text:"));
                assert!(!failure.text.contains(&full));
            }
        }
        for name in ["Read", "Edit"] {
            assert!(
                project_for_model(name, &"Q".repeat(1024), Some(temp.path()), "session").is_none()
            );
        }
        let bash = crate::tools::get_tool_definitions()
            .into_iter()
            .find(|t| t.name == "Bash")
            .unwrap();
        assert!(bash.input_schema["properties"].get("raw").is_none());
    }
}
