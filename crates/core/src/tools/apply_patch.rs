//! Parse the native apply-patch format and compute expected new file bytes in
//! memory.
//!
//! This module deliberately performs no filesystem access and is not reachable
//! from a model request or a tool dispatch. The caller supplies the current
//! content of every path the patch names; [`plan_patch`] returns the byte-level
//! changes a later filesystem stage may apply, or an error. A failed parse or
//! plan therefore cannot leave a partial filesystem change behind.
//!
//! # Grammar
//!
//! The format is line oriented. Lines are separated by LF or CRLF; a trailing
//! `\r` is part of the separator and never part of a content line. The patch
//! text is exactly:
//!
//! ```text
//! "*** Begin Patch" LF
//! operation+
//! "*** End Patch" LF?
//! ```
//!
//! with these operations:
//!
//! ```text
//! add     := "*** Add File: " path LF ("+" line LF)*
//! delete  := "*** Delete File: " path LF
//! update  := "*** Update File: " path LF ("*** Move to: " path LF)? hunk*
//! hunk    := ("@@" | "@@ " context) LF change+ ("*** End of File" LF)?
//! change  := (" " | "-" | "+") line LF
//! ```
//!
//! A `*** Move to:` line is only valid immediately after an update header. An
//! update needs at least one hunk or a move destination; a hunk needs at least
//! one change line. Insertion-only hunks are accepted here but must prove an
//! unambiguous location when the update is computed: an empty file, a unique
//! `@@ context` anchor (the insert follows that line), or `*** End of File`
//! (the insert appends to the file). Without one of these the update fails.
//! A `@@ context` header narrows the search to the unique line equal to
//! `context`; the hunk's context and removal lines must then match at or after
//! that line (inclusive, so the anchor may itself be a context line). Operation
//! and hunk headers must not be indented and must appear in the order above. A
//! marker-shaped line is content only when it has the hunk prefix, for example
//! `+*** End Patch`. Paths are opaque data in this module (the filesystem stage
//! resolves scope); only emptiness, surrounding whitespace and NUL are rejected.
//!
//! # Newlines and final-newline contract
//!
//! Patch content lines never carry terminators. The terminator of each patch
//! line is structural:
//!
//! - Add File joins its `+` lines with `\n` and appends one final `\n` when at
//!   least one line exists. No `+` lines creates an empty file. The format has
//!   no way to create a file whose last line lacks a newline.
//! - Update matches whole logical lines after removing their terminators.
//!   Context and untouched lines keep their exact original bytes. Inserted
//!   lines use the first existing line terminator in the file (`\n` when the
//!   file has none).
//! - Update preserves the source file's final-newline state. If the file had no
//!   final newline, the result has none unless a hunk removes that final line,
//!   in which case the preceding line's existing terminator remains.
//! - A UTF-8 BOM is removed from the logical first line for matching and
//!   re-emitted byte for byte. Invalid UTF-8 or NUL-containing content is
//!   rejected when a text hunk must be applied. Delete never decodes content,
//!   and a move without hunks copies arbitrary bytes verbatim.
use std::collections::BTreeMap;

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_PREFIX: &str = "*** Add File: ";
const DELETE_FILE_PREFIX: &str = "*** Delete File: ";
const UPDATE_FILE_PREFIX: &str = "*** Update File: ";
const MOVE_TO_PREFIX: &str = "*** Move to: ";
const END_OF_FILE_MARKER: &str = "*** End of File";
const BOM: &str = "\u{feff}";

impl ParsedPatch {
    /// Every path this patch touches, including both endpoints of a move.
    pub(crate) fn checkpoint_paths(&self) -> Vec<String> {
        self.operations
            .iter()
            .flat_map(|operation| match operation {
                PatchOperation::Add { path, .. }
                | PatchOperation::Update { path, .. }
                | PatchOperation::Delete { path } => vec![path.clone()],
                PatchOperation::Move { from, to, .. } => vec![from.clone(), to.clone()],
            })
            .collect()
    }
}

/// A parsed patch. Operations appear in patch order; duplicate target paths
/// have already been rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPatch {
    pub operations: Vec<PatchOperation>,
}

/// A typed patch operation. A `*** Move to:` header turns the update into a
/// `Move` operation carrying the content hunks that apply before the move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchOperation {
    Add {
        path: String,
        contents: Vec<u8>,
    },
    Update {
        path: String,
        hunks: Vec<UpdateHunk>,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
        hunks: Vec<UpdateHunk>,
    },
}

/// One contextual update hunk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UpdateHunk {
    /// Text after an `@@ ` header, used to narrow the search.
    pub context: Option<String>,
    /// Context, removal and insertion lines in patch order.
    pub lines: Vec<HunkLine>,
    /// `*** End of File` requires the matched old-side block to end the file.
    pub end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Parse and validate patch text without touching any file.
pub(crate) fn parse_patch(patch: &str) -> Result<ParsedPatch, PatchError> {
    if let Some(offset) = patch.find('\0') {
        let line = patch[..offset].matches('\n').count() + 1;
        return Err(PatchError::Syntax {
            line,
            message: "patch contains a NUL byte; binary patch text is unsupported".into(),
        });
    }

    // Split into logical lines, treating a trailing CR as part of a CRLF
    // separator. One trailing empty entry is the patch argument's final
    // newline; anything else after `*** End Patch` is rejected below.
    let mut lines: Vec<(&str, usize)> = patch
        .split('\n')
        .enumerate()
        .map(|(index, line)| (line.strip_suffix('\r').unwrap_or(line), index + 1))
        .collect();
    if matches!(lines.last(), Some((text, _)) if text.is_empty()) {
        lines.pop();
    }

    let Some(&(first, first_line)) = lines.first() else {
        return Err(PatchError::Syntax {
            line: 1,
            message: format!("patch is empty; expected '{BEGIN_PATCH_MARKER}'"),
        });
    };
    if first != BEGIN_PATCH_MARKER {
        return Err(PatchError::Syntax {
            line: first_line,
            message: format!("expected '{BEGIN_PATCH_MARKER}' on the first line, found {first:?}"),
        });
    }

    let mut index = 1;
    let mut operations = Vec::new();
    loop {
        let Some(&(line, line_number)) = lines.get(index) else {
            let line = lines.last().map(|(_, number)| number + 1).unwrap_or(2);
            return Err(PatchError::Syntax {
                line,
                message: format!("missing '{END_PATCH_MARKER}' at the end of the patch"),
            });
        };
        if line == END_PATCH_MARKER {
            index += 1;
            break;
        }
        if let Some(raw_path) = line.strip_prefix(ADD_FILE_PREFIX) {
            let path = parse_path(raw_path, line_number, "Add File")?;
            index += 1;
            let mut added = Vec::new();
            while let Some(&(text, _)) = lines.get(index) {
                let Some(content) = text.strip_prefix('+') else {
                    break;
                };
                added.push(content);
                index += 1;
            }
            let mut contents = added.join("\n");
            if !added.is_empty() {
                contents.push('\n');
            }
            operations.push(PatchOperation::Add {
                path,
                contents: contents.into_bytes(),
            });
            continue;
        }
        if let Some(raw_path) = line.strip_prefix(UPDATE_FILE_PREFIX) {
            let path = parse_path(raw_path, line_number, "Update File")?;
            let (next, operation) = parse_update(&lines, index + 1, path, line_number)?;
            operations.push(operation);
            index = next;
            continue;
        }
        if let Some(raw_path) = line.strip_prefix(DELETE_FILE_PREFIX) {
            let path = parse_path(raw_path, line_number, "Delete File")?;
            operations.push(PatchOperation::Delete { path });
            index += 1;
            continue;
        }
        if line.starts_with("*** ") {
            return Err(PatchError::Syntax {
                line: line_number,
                message: format!("unknown patch operation {line:?}"),
            });
        }
        return Err(PatchError::Syntax {
            line: line_number,
            message: format!("expected a patch operation header, found {line:?}"),
        });
    }

    if let Some(&(_, line_number)) = lines.get(index) {
        return Err(PatchError::Syntax {
            line: line_number,
            message: format!("trailing content after '{END_PATCH_MARKER}'"),
        });
    }

    if operations.is_empty() {
        return Err(PatchError::Syntax {
            line: first_line,
            message: "patch contains no operations; nothing to apply".into(),
        });
    }

    validate_target_paths(&operations)?;
    Ok(ParsedPatch { operations })
}

fn parse_update(
    lines: &[(&str, usize)],
    mut index: usize,
    path: String,
    header_line: usize,
) -> Result<(usize, PatchOperation), PatchError> {
    let mut move_to = None;
    if let Some(&(text, line_number)) = lines.get(index) {
        if let Some(raw_path) = text.strip_prefix(MOVE_TO_PREFIX) {
            move_to = Some(parse_path(raw_path, line_number, "Move destination")?);
            index += 1;
        }
    }

    let mut hunks = Vec::new();
    loop {
        let Some(&(text, line_number)) = lines.get(index) else {
            break;
        };
        if text == END_PATCH_MARKER || text.starts_with("*** ") {
            break;
        }

        let mut hunk = UpdateHunk::default();
        let hunk_line = line_number;
        if text == "@@" {
            index += 1;
        } else if let Some(context) = text.strip_prefix("@@ ") {
            if !context.is_empty() {
                hunk.context = Some(context.to_string());
            }
            index += 1;
        }

        loop {
            let Some(&(line, line_number)) = lines.get(index) else {
                break;
            };
            if line == "@@" || line.starts_with("@@ ") || line.starts_with("*** ") {
                break;
            }
            let parsed = if let Some(content) = line.strip_prefix(' ') {
                HunkLine::Context(content.to_string())
            } else if let Some(content) = line.strip_prefix('-') {
                HunkLine::Remove(content.to_string())
            } else if let Some(content) = line.strip_prefix('+') {
                HunkLine::Add(content.to_string())
            } else {
                return Err(PatchError::Syntax {
                    line: line_number,
                    message: format!(
                        "invalid hunk line {line:?}; expected a leading ' ', '-' or '+'"
                    ),
                });
            };
            hunk.lines.push(parsed);
            index += 1;
        }

        if let Some(&(line, _)) = lines.get(index) {
            if line == END_OF_FILE_MARKER {
                hunk.end_of_file = true;
                index += 1;
                if let Some(&(next, next_line)) = lines.get(index) {
                    if !next.starts_with("*** ") {
                        return Err(PatchError::Syntax {
                            line: next_line,
                            message: format!("unexpected content after '{END_OF_FILE_MARKER}'"),
                        });
                    }
                }
            }
        }

        if hunk.lines.is_empty() {
            return Err(PatchError::Syntax {
                line: hunk_line,
                message: "hunk must contain at least one change line".into(),
            });
        }
        hunks.push(hunk);
    }

    if hunks.is_empty() && move_to.is_none() {
        return Err(PatchError::Syntax {
            line: header_line,
            message: format!("update for {path:?} has no hunks and no move destination"),
        });
    }

    let operation = match move_to {
        Some(to) => PatchOperation::Move {
            from: path,
            to,
            hunks,
        },
        None => PatchOperation::Update { path, hunks },
    };
    Ok((index, operation))
}

fn parse_path(raw: &str, line: usize, role: &str) -> Result<String, PatchError> {
    if raw.is_empty() {
        return Err(PatchError::Syntax {
            line,
            message: format!("{role} path must not be empty"),
        });
    }
    if raw != raw.trim() {
        return Err(PatchError::Syntax {
            line,
            message: format!("{role} path must not have leading or trailing whitespace"),
        });
    }
    if raw.contains('\0') {
        return Err(PatchError::Syntax {
            line,
            message: format!("{role} path must not contain NUL"),
        });
    }
    Ok(raw.to_string())
}

fn validate_target_paths(operations: &[PatchOperation]) -> Result<(), PatchError> {
    let mut seen: BTreeMap<&str, &'static str> = BTreeMap::new();
    for operation in operations {
        let targets: Vec<(&str, &'static str)> = match operation {
            PatchOperation::Add { path, .. } => vec![(path.as_str(), "Add File")],
            PatchOperation::Update { path, .. } => vec![(path.as_str(), "Update File")],
            PatchOperation::Delete { path } => vec![(path.as_str(), "Delete File")],
            PatchOperation::Move { from, to, .. } => {
                vec![
                    (from.as_str(), "Move source"),
                    (to.as_str(), "Move destination"),
                ]
            }
        };
        for (path, role) in targets {
            if let Some(previous) = seen.insert(path, role) {
                return Err(PatchError::Conflict {
                    path: path.to_string(),
                    message: format!(
                        "path is used by both {previous} and {role}; merge the operations into one block or choose distinct paths"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// The current state supplied by the caller. A key is present when the path
/// exists; a missing key means the path does not exist in this in-memory view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PatchInputs {
    files: BTreeMap<String, Vec<u8>>,
}

impl PatchInputs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, path: impl Into<String>, contents: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), contents.into());
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    pub(crate) fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }
}

/// The changes a parsed patch would make. This is data only; nothing is
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchPlan {
    pub changes: Vec<PlannedChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlannedChange {
    Add {
        path: String,
        contents: Vec<u8>,
    },
    Update {
        path: String,
        old_contents: Vec<u8>,
        new_contents: Vec<u8>,
    },
    Delete {
        path: String,
        old_contents: Vec<u8>,
    },
    Move {
        from: String,
        to: String,
        old_contents: Vec<u8>,
        new_contents: Vec<u8>,
    },
}

/// Compute byte-level changes from the supplied in-memory state.
///
/// Preflight is explicit:
/// - Add fails when the target already exists.
/// - Delete, Update and Move source fail when the source does not exist.
/// - Move fails when the destination already exists.
/// - Any failure returns no plan, so callers cannot apply a partial result.
pub(crate) fn plan_patch(
    patch: &ParsedPatch,
    inputs: &PatchInputs,
) -> Result<PatchPlan, PatchError> {
    let mut changes = Vec::with_capacity(patch.operations.len());
    for operation in &patch.operations {
        match operation {
            PatchOperation::Add { path, contents } => {
                if inputs.contains(path) {
                    return Err(PatchError::Conflict {
                        path: path.clone(),
                        message: "Add File target already exists in the supplied state".into(),
                    });
                }
                changes.push(PlannedChange::Add {
                    path: path.clone(),
                    contents: contents.clone(),
                });
            }
            PatchOperation::Update { path, hunks } => {
                let old_contents = existing(inputs, path, "Update File")?;
                let new_contents = apply_update(path, old_contents, hunks)?;
                changes.push(PlannedChange::Update {
                    path: path.clone(),
                    old_contents: old_contents.to_vec(),
                    new_contents,
                });
            }
            PatchOperation::Delete { path } => {
                let old_contents = existing(inputs, path, "Delete File")?;
                changes.push(PlannedChange::Delete {
                    path: path.clone(),
                    old_contents: old_contents.to_vec(),
                });
            }
            PatchOperation::Move { from, to, hunks } => {
                let old_contents = existing(inputs, from, "Move source")?;
                if inputs.contains(to) {
                    return Err(PatchError::Conflict {
                        path: to.clone(),
                        message: "Move destination already exists in the supplied state".into(),
                    });
                }
                let new_contents = apply_update(from, old_contents, hunks)?;
                changes.push(PlannedChange::Move {
                    from: from.clone(),
                    to: to.clone(),
                    old_contents: old_contents.to_vec(),
                    new_contents,
                });
            }
        }
    }
    Ok(PatchPlan { changes })
}

fn existing<'a>(inputs: &'a PatchInputs, path: &str, role: &str) -> Result<&'a [u8], PatchError> {
    inputs.get(path).ok_or_else(|| PatchError::Preflight {
        path: path.to_string(),
        message: format!("{role} target does not exist in the supplied state"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceLine {
    text: String,
    term: Option<&'static str>,
}

fn apply_update(path: &str, contents: &[u8], hunks: &[UpdateHunk]) -> Result<Vec<u8>, PatchError> {
    // A move/update without hunks is a pure rename: arbitrary bytes are valid.
    if hunks.is_empty() {
        return Ok(contents.to_vec());
    }
    let text = std::str::from_utf8(contents).map_err(|error| PatchError::File {
        path: path.to_string(),
        message: format!(
            "supplied content is not valid UTF-8 at byte {}; binary input is unsupported",
            error.valid_up_to()
        ),
    })?;
    if text.contains('\0') {
        return Err(PatchError::File {
            path: path.to_string(),
            message: "supplied content contains a NUL byte; binary input is unsupported".into(),
        });
    }

    let (has_bom, body) = match text.strip_prefix(BOM) {
        Some(body) => (true, body),
        None => (false, text),
    };
    let mut lines = split_lines(body);
    let preferred_terminator = lines.iter().find_map(|line| line.term).unwrap_or("\n");
    let preserve_no_final_newline = !body.is_empty() && !body.ends_with('\n');
    let mut search_start = 0;

    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let hunk_number = hunk_index + 1;
        let mut first_possible = search_start;
        if let Some(context) = hunk.context.as_deref() {
            // The hunk may include the anchor line itself as a context line, so
            // start searching at the anchor rather than after it.
            first_possible = find_context_anchor(&lines, context, search_start, path, hunk_number)?;
        }

        let old_pattern: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        if old_pattern.is_empty() {
            // Insertion-only hunk: an empty file needs no location, a unique
            // `@@ context` inserts after that line, and End of File appends.
            let insert_at = if let Some(context) = hunk.context.as_deref() {
                find_context_anchor(&lines, context, search_start, path, hunk_number)? + 1
            } else if lines.is_empty() {
                0
            } else if hunk.end_of_file {
                lines.len()
            } else {
                return Err(PatchError::Update {
                    path: path.to_string(),
                    hunk: hunk_number,
                    message: "insertion-only hunk needs an unambiguous location: an empty file, a unique '@@ context' anchor, or '*** End of File'".into(),
                });
            };
            let additions: Vec<SourceLine> = hunk
                .lines
                .iter()
                .filter_map(|line| match line {
                    HunkLine::Add(text) => Some(SourceLine {
                        text: text.clone(),
                        term: None,
                    }),
                    HunkLine::Context(_) | HunkLine::Remove(_) => None,
                })
                .collect();
            let added = additions.len();
            lines.splice(insert_at..insert_at, additions);
            search_start = insert_at + added;
            continue;
        }

        let match_start = find_unique_match(
            &lines,
            &old_pattern,
            first_possible,
            hunk.end_of_file,
            path,
            hunk_number,
        )?;

        let mut replacement = Vec::with_capacity(hunk.lines.len());
        let mut old_offset = 0;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(_) => {
                    // Keep the matched line's exact text and terminator so
                    // mixed and CRLF files retain untouched bytes.
                    replacement.push(lines[match_start + old_offset].clone());
                    old_offset += 1;
                }
                HunkLine::Remove(_) => {
                    old_offset += 1;
                }
                HunkLine::Add(text) => replacement.push(SourceLine {
                    text: text.clone(),
                    term: None,
                }),
            }
        }
        let new_len = replacement.len();
        let match_end = match_start + old_offset;
        lines.splice(match_start..match_end, replacement);
        search_start = match_start + new_len;
    }

    let line_count = lines.len();
    for (index, line) in lines.iter_mut().enumerate() {
        if line.term.is_some() {
            continue;
        }
        if preserve_no_final_newline && index + 1 == line_count {
            continue;
        }
        line.term = Some(preferred_terminator);
    }

    let mut output = String::with_capacity(body.len() + lines.len() * 4);
    if has_bom {
        output.push_str(BOM);
    }
    for line in &lines {
        output.push_str(&line.text);
        if let Some(terminator) = line.term {
            output.push_str(terminator);
        }
    }
    Ok(output.into_bytes())
}

fn split_lines(body: &str) -> Vec<SourceLine> {
    let mut lines = Vec::new();
    let bytes = body.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            let (text_end, terminator) = if index > start && bytes[index - 1] == b'\r' {
                (index - 1, "\r\n")
            } else {
                (index, "\n")
            };
            lines.push(SourceLine {
                text: body[start..text_end].to_string(),
                term: Some(terminator),
            });
            index += 1;
            start = index;
        } else {
            index += 1;
        }
    }
    if start < body.len() {
        lines.push(SourceLine {
            text: body[start..].to_string(),
            term: None,
        });
    }
    lines
}

fn find_context_anchor(
    lines: &[SourceLine],
    context: &str,
    start: usize,
    path: &str,
    hunk: usize,
) -> Result<usize, PatchError> {
    let matches: Vec<usize> = (start..lines.len())
        .filter(|&index| lines[index].text == context)
        .collect();
    match matches.as_slice() {
        [] => Err(PatchError::Update {
            path: path.to_string(),
            hunk,
            message: format!(
                "context line {context:?} was not found at or after line {}",
                start + 1
            ),
        }),
        [index] => Ok(*index),
        _ => Err(PatchError::Update {
            path: path.to_string(),
            hunk,
            message: format!(
                "context line {context:?} matches {} locations (lines {}); add more context to make the hunk unique",
                matches.len(),
                line_list(&matches)
            ),
        }),
    }
}

fn find_unique_match(
    lines: &[SourceLine],
    pattern: &[&str],
    start: usize,
    end_of_file: bool,
    path: &str,
    hunk: usize,
) -> Result<usize, PatchError> {
    let mut matches = Vec::new();
    if pattern.len() <= lines.len() && start <= lines.len() - pattern.len() {
        for at in start..=lines.len() - pattern.len() {
            if pattern
                .iter()
                .enumerate()
                .all(|(offset, expected)| lines[at + offset].text == *expected)
            {
                matches.push(at);
            }
        }
    }

    if end_of_file {
        matches.retain(|at| at + pattern.len() == lines.len());
        if matches.is_empty() {
            return Err(PatchError::Update {
                path: path.to_string(),
                hunk,
                message: format!(
                    "end-of-file hunk did not match the last {} line(s); searched from line {}",
                    pattern.len(),
                    start + 1
                ),
            });
        }
    }

    match matches.as_slice() {
        [] => Err(PatchError::Update {
            path: path.to_string(),
            hunk,
            message: format!(
                "context/removal lines were not found (searched from line {}); first expected line: {:?}",
                start + 1,
                pattern[0]
            ),
        }),
        [at] => Ok(*at),
        _ => Err(PatchError::Update {
            path: path.to_string(),
            hunk,
            message: format!(
                "context/removal lines match {} locations (lines {}); add more context to make the hunk unique",
                matches.len(),
                line_list(&matches)
            ),
        }),
    }
}

fn line_list(matches: &[usize]) -> String {
    let shown: Vec<String> = matches
        .iter()
        .take(8)
        .map(|index| (index + 1).to_string())
        .collect();
    if matches.len() > shown.len() {
        format!("{}, ...", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PatchError {
    #[error("invalid patch at line {line}: {message}")]
    Syntax { line: usize, message: String },
    #[error("cannot apply update to '{path}' at hunk {hunk}: {message}")]
    Update {
        path: String,
        hunk: usize,
        message: String,
    },
    #[error("cannot use '{path}' in the patch: {message}")]
    File { path: String, message: String },
    #[error("cannot preflight '{path}': {message}")]
    Preflight { path: String, message: String },
    #[error("conflicting patch target '{path}': {message}")]
    Conflict { path: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(entries: &[(&str, &str)]) -> PatchInputs {
        let mut inputs = PatchInputs::new();
        for (path, contents) in entries {
            inputs.insert(*path, contents.as_bytes());
        }
        inputs
    }

    fn plan_text(patch: &str, entries: &[(&str, &str)]) -> Result<PatchPlan, PatchError> {
        plan_patch(&parse_patch(patch)?, &inputs(entries))
    }

    fn planned_update<'a>(plan: &'a PatchPlan, path: &str) -> &'a [u8] {
        plan.changes
            .iter()
            .find_map(|change| match change {
                PlannedChange::Update {
                    path: updated,
                    new_contents,
                    ..
                } if updated == path => Some(new_contents.as_slice()),
                PlannedChange::Move {
                    from, new_contents, ..
                } if from == path => Some(new_contents.as_slice()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no update for {path}: {plan:?}"))
    }

    fn syntax_message(patch: &str) -> (usize, String) {
        match parse_patch(patch) {
            Err(PatchError::Syntax { line, message }) => (line, message),
            other => panic!("expected a syntax error for {patch:?}, got {other:?}"),
        }
    }

    fn update_error(patch: &str, entries: &[(&str, &str)]) -> (String, usize, String) {
        match plan_text(patch, entries) {
            Err(PatchError::Update {
                path,
                hunk,
                message,
            }) => (path, hunk, message),
            other => panic!("expected an update error, got {other:?}"),
        }
    }

    fn conflict_error(patch: &str, entries: &[(&str, &str)]) -> (String, String) {
        match plan_text(patch, entries) {
            Err(PatchError::Conflict { path, message }) => (path, message),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    fn preflight_error(patch: &str, entries: &[(&str, &str)]) -> (String, String) {
        match plan_text(patch, entries) {
            Err(PatchError::Preflight { path, message }) => (path, message),
            other => panic!("expected a preflight error, got {other:?}"),
        }
    }

    #[test]
    fn parses_mixed_multi_file_patch_into_typed_operations() {
        let patch = "\
*** Begin Patch
*** Add File: dir/new.txt
+hello
+world
*** Update File: src/app.rs
@@ fn main()
 fn main() {
-    old();
+    new();
 }
@@ fn helper()
 fn helper() {}
*** Update File: src/legacy.rs
*** Move to: src/renamed.rs
@@
-old
+new
*** Delete File: old.txt
*** End Patch";
        let parsed = parse_patch(patch).unwrap();
        assert_eq!(parsed.operations.len(), 4);
        assert_eq!(
            parsed.operations[0],
            PatchOperation::Add {
                path: "dir/new.txt".into(),
                contents: b"hello\nworld\n".to_vec(),
            }
        );
        match &parsed.operations[1] {
            PatchOperation::Update { path, hunks } => {
                assert_eq!(path, "src/app.rs");
                assert_eq!(hunks.len(), 2);
                assert_eq!(hunks[0].context.as_deref(), Some("fn main()"));
                assert_eq!(
                    hunks[0].lines,
                    vec![
                        HunkLine::Context("fn main() {".into()),
                        HunkLine::Remove("    old();".into()),
                        HunkLine::Add("    new();".into()),
                        HunkLine::Context("}".into()),
                    ]
                );
                assert!(!hunks[0].end_of_file);
                assert_eq!(hunks[1].context.as_deref(), Some("fn helper()"));
                assert_eq!(
                    hunks[1].lines,
                    vec![HunkLine::Context("fn helper() {}".into())]
                );
            }
            other => panic!("expected an update, got {other:?}"),
        }
        match &parsed.operations[2] {
            PatchOperation::Move { from, to, hunks } => {
                assert_eq!(from, "src/legacy.rs");
                assert_eq!(to, "src/renamed.rs");
                assert_eq!(
                    hunks[0].lines,
                    vec![HunkLine::Remove("old".into()), HunkLine::Add("new".into())]
                );
            }
            other => panic!("expected a move, got {other:?}"),
        }
        assert_eq!(
            parsed.operations[3],
            PatchOperation::Delete {
                path: "old.txt".into()
            }
        );
    }

    #[test]
    fn empty_patch_is_an_explicit_error() {
        for patch in [
            "*** Begin Patch\n*** End Patch",
            "*** Begin Patch\n*** End Patch\n",
            "*** Begin Patch\r\n*** End Patch\r\n",
        ] {
            let (line, message) = syntax_message(patch);
            assert_eq!(line, 1, "{patch:?}");
            assert!(message.contains("no operations"), "{message}");
        }
    }

    #[test]
    fn rejects_malformed_headers_operations_and_trailing_garbage() {
        let (line, message) = syntax_message("");
        assert_eq!(line, 1);
        assert!(message.contains("patch is empty"));

        let (line, message) = syntax_message("not a patch");
        assert_eq!(line, 1);
        assert!(message.contains("*** Begin Patch"));

        let (line, message) = syntax_message("*** Begin Patch\n*** Add File: a\n+x");
        assert_eq!(line, 4);
        assert!(message.contains("missing '*** End Patch'"));

        let (line, message) = syntax_message("*** Begin Patch\n*** Rename File: a\n*** End Patch");
        assert_eq!(line, 2);
        assert!(message.contains("unknown patch operation"));

        let (line, message) = syntax_message("*** Begin Patch\n*** Begin Patch\n*** End Patch");
        assert_eq!(line, 2);
        assert!(message.contains("unknown patch operation"));

        let (line, message) =
            syntax_message("*** Begin Patch\n*** Add File: a\nnot a body line\n*** End Patch");
        assert_eq!(line, 3);
        assert!(message.contains("expected a patch operation header"));

        let (line, message) =
            syntax_message("*** Begin Patch\n*** Update File: a\n?bad\n*** End Patch");
        assert_eq!(line, 3);
        assert!(message.contains("invalid hunk line"));

        let (line, message) = syntax_message("*** Begin Patch\n*** End Patch\ngarbage");
        assert_eq!(line, 3);
        assert!(message.contains("trailing content"));

        let (line, message) = syntax_message("*** Begin Patch\n*** End Patch\n\n");
        assert_eq!(line, 3);
        assert!(message.contains("trailing content"));

        let (line, message) = syntax_message("\n*** Begin Patch\n*** End Patch");
        assert_eq!(line, 1);
        assert!(message.contains("*** Begin Patch"));

        let (line, message) = syntax_message("*** End Patch");
        assert_eq!(line, 1);
        assert!(message.contains("*** Begin Patch"));

        let (line, message) = syntax_message("*** Begin Patch\n*** Update File: a\n*** End Patch");
        assert_eq!(line, 2);
        assert!(message.contains("no hunks"));
    }

    #[test]
    fn rejects_empty_paths_and_treats_marker_like_lines_as_content_only_when_prefixed() {
        for patch in [
            "*** Begin Patch\n*** Add File: \n+x\n*** End Patch",
            "*** Begin Patch\n*** Delete File:   \n*** End Patch",
            "*** Begin Patch\n*** Update File: \n@@\n-a\n+b\n*** End Patch",
            "*** Begin Patch\n*** Update File: a\n*** Move to: \n*** End Patch",
            "*** Begin Patch\n*** Add File:  spaced\n+x\n*** End Patch",
        ] {
            let (_, message) = syntax_message(patch);
            assert!(
                message.contains("must not be empty")
                    || message.contains("leading or trailing whitespace"),
                "{patch:?} produced {message:?}"
            );
        }

        let parsed = parse_patch(
            "*** Begin Patch\n*** Add File: a.txt\n+*** End Patch\n+*** Update File: victim.txt\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            parsed.operations,
            vec![PatchOperation::Add {
                path: "a.txt".into(),
                contents: b"*** End Patch\n*** Update File: victim.txt\n".to_vec(),
            }]
        );

        let (line, message) = syntax_message(
            "*** Begin Patch\n*** Add File: a.txt\n+ok\n*** Frobnicate File: victim\n*** End Patch",
        );
        assert_eq!(line, 4);
        assert!(message.contains("unknown patch operation"));
    }

    #[test]
    fn paths_are_opaque_data_in_this_round() {
        let parsed = parse_patch(
            "*** Begin Patch\n*** Add File: ../outside/x.txt\n+ok\n*** Add File: /tmp/y.txt\n+ok\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            parsed.operations,
            vec![
                PatchOperation::Add {
                    path: "../outside/x.txt".into(),
                    contents: b"ok\n".to_vec(),
                },
                PatchOperation::Add {
                    path: "/tmp/y.txt".into(),
                    contents: b"ok\n".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn rejects_duplicate_and_conflicting_target_operations() {
        let cases = [
            (
                "*** Begin Patch\n*** Add File: a\n+x\n*** Add File: a\n+y\n*** End Patch",
                "a",
            ),
            (
                "*** Begin Patch\n*** Add File: a\n+x\n*** Delete File: a\n*** End Patch",
                "a",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n@@\n-a\n+b\n*** Delete File: a\n*** End Patch",
                "a",
            ),
            (
                "*** Begin Patch\n*** Add File: dest\n+x\n*** Update File: a\n*** Move to: dest\n*** End Patch",
                "dest",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n*** Move to: a\n*** End Patch",
                "a",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n*** Move to: dest\n*** Update File: b\n*** Move to: dest\n*** End Patch",
                "dest",
            ),
        ];
        for (patch, expected_path) in cases {
            match parse_patch(patch) {
                Err(PatchError::Conflict { path, message }) => {
                    assert_eq!(path, expected_path, "{patch}");
                    assert!(message.contains("used by both"), "{message}");
                }
                other => panic!("expected a conflict for {patch:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn insertion_only_hunks_need_an_unambiguous_location() {
        let (path, hunk, message) = update_error(
            "*** Begin Patch\n*** Update File: f\n@@\n+added\n*** End Patch",
            &[("f", "one\ntwo\n")],
        );
        assert_eq!(path, "f");
        assert_eq!(hunk, 1);
        assert!(message.contains("unambiguous location"), "{message}");

        let (_, _, message) = update_error(
            "*** Begin Patch\n*** Update File: f\n@@ two\n+added\n*** End Patch",
            &[("f", "two\ntwo\n")],
        );
        assert!(message.contains("2 locations"), "{message}");
    }

    #[test]
    fn insertion_only_hunk_targets_an_empty_file_or_anchor() {
        let plan = plan_text(
            "*** Begin Patch\n*** Update File: empty\n@@\n+first\n+second\n*** End Patch",
            &[("empty", "")],
        )
        .unwrap();
        assert_eq!(planned_update(&plan, "empty"), b"first\nsecond\n");

        let plan = plan_text(
            "*** Begin Patch\n*** Update File: f\n@@ two\n+after\n*** End Patch",
            &[("f", "one\ntwo\nthree\n")],
        )
        .unwrap();
        assert_eq!(planned_update(&plan, "f"), b"one\ntwo\nafter\nthree\n");

        let plan = plan_text(
            "*** Begin Patch\n*** Update File: f\n@@\n+tail\n*** End of File\n*** End Patch",
            &[("f", "one\n")],
        )
        .unwrap();
        assert_eq!(planned_update(&plan, "f"), b"one\ntail\n");
    }

    #[test]
    fn move_without_hunks_preserves_binary_bytes() {
        let patch =
            parse_patch("*** Begin Patch\n*** Update File: f\n*** Move to: g\n*** End Patch")
                .unwrap();
        let mut inputs = PatchInputs::new();
        inputs.insert("f", vec![0xff, 0x00, b'a', 0x80]);
        let plan = plan_patch(&patch, &inputs).unwrap();
        assert_eq!(
            plan.changes,
            vec![PlannedChange::Move {
                from: "f".into(),
                to: "g".into(),
                old_contents: vec![0xff, 0x00, b'a', 0x80],
                new_contents: vec![0xff, 0x00, b'a', 0x80],
            }]
        );
    }

    #[test]
    fn applies_single_and_multiple_hunks_against_current_content() {
        let patch = "\
*** Begin Patch
*** Update File: f.txt
@@
 alpha
-beta
+BETA
+BETA2
@@
 delta
-epsilon
+EPSILON
*** End Patch";
        let plan = plan_text(patch, &[("f.txt", "alpha\nbeta\ngamma\ndelta\nepsilon\n")]).unwrap();
        assert_eq!(
            planned_update(&plan, "f.txt"),
            b"alpha\nBETA\nBETA2\ngamma\ndelta\nEPSILON\n"
        );
    }

    #[test]
    fn rejects_missing_and_ambiguous_matches_with_actionable_details() {
        let (path, hunk, message) = update_error(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n absent\n+x\n*** End Patch",
            &[("f.txt", "present\n")],
        );
        assert_eq!(path, "f.txt");
        assert_eq!(hunk, 1);
        assert!(message.contains("were not found"), "{message}");
        assert!(message.contains("absent"), "{message}");

        let (path, hunk, message) = update_error(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-same\n+changed\n*** End Patch",
            &[("f.txt", "same\nsame\n")],
        );
        assert_eq!(path, "f.txt");
        assert_eq!(hunk, 1);
        assert!(message.contains("2 locations"), "{message}");
        assert!(message.contains("lines 1, 2"), "{message}");

        let plan = plan_text(
            "*** Begin Patch\n*** Update File: f.txt\n@@ fn b()\n-same\n+changed\n*** End Patch",
            &[("f.txt", "fn a()\nsame\nfn b()\nsame\n")],
        )
        .unwrap();
        assert_eq!(
            planned_update(&plan, "f.txt"),
            b"fn a()\nsame\nfn b()\nchanged\n"
        );
    }

    #[test]
    fn preserves_unicode_bom_and_untouched_mixed_line_endings() {
        let source = "\u{feff}café\r\none\r\ntwo\r\n三\nkeep";
        let plan = plan_text(
            "*** Begin Patch\n*** Update File: f.txt\n@@ one\n one\n-two\n+二\n 三\n keep\n*** End Patch",
            &[("f.txt", source)],
        )
        .unwrap();
        assert_eq!(
            planned_update(&plan, "f.txt"),
            "\u{feff}café\r\none\r\n二\r\n三\nkeep".as_bytes()
        );

        let crlf_patch = "*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-café\r\n+CAFÉ\r\n*** End Patch\r\n";
        let plan = plan_text(crlf_patch, &[("f.txt", source)]).unwrap();
        assert_eq!(
            planned_update(&plan, "f.txt"),
            "\u{feff}CAFÉ\r\none\r\ntwo\r\n三\nkeep".as_bytes()
        );
    }

    #[test]
    fn defines_final_newline_state_and_add_file_contents() {
        let parsed = parse_patch(
            "*** Begin Patch\n*** Add File: add.txt\n+x\n+y\n*** Add File: empty.txt\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            parsed.operations,
            vec![
                PatchOperation::Add {
                    path: "add.txt".into(),
                    contents: b"x\ny\n".to_vec(),
                },
                PatchOperation::Add {
                    path: "empty.txt".into(),
                    contents: Vec::new(),
                },
            ]
        );

        let replace_last = "*** Begin Patch\n*** Update File: f\n@@\n-last\n+LAST\n*** End Patch";
        assert_eq!(
            planned_update(
                &plan_text(replace_last, &[("f", "first\nlast")]).unwrap(),
                "f"
            ),
            b"first\nLAST"
        );
        assert_eq!(
            planned_update(
                &plan_text(replace_last, &[("f", "first\nlast\n")]).unwrap(),
                "f"
            ),
            b"first\nLAST\n"
        );

        let append_at_eof = "*** Begin Patch\n*** Update File: f\n@@\n last\n+added\n*** End Patch";
        assert_eq!(
            planned_update(
                &plan_text(append_at_eof, &[("f", "first\nlast")]).unwrap(),
                "f"
            ),
            b"first\nlast\nadded"
        );

        let remove_last = "*** Begin Patch\n*** Update File: f\n@@\n-last\n*** End Patch";
        assert_eq!(
            planned_update(
                &plan_text(remove_last, &[("f", "first\nlast\n")]).unwrap(),
                "f"
            ),
            b"first\n"
        );
        assert_eq!(
            planned_update(
                &plan_text(remove_last, &[("f", "first\nlast")]).unwrap(),
                "f"
            ),
            b"first\n"
        );
        assert_eq!(
            planned_update(&plan_text(remove_last, &[("f", "last")]).unwrap(), "f"),
            b""
        );
    }

    #[test]
    fn preflight_rejects_existing_add_missing_targets_and_move_destination_conflicts() {
        let (path, message) = conflict_error(
            "*** Begin Patch\n*** Add File: a\n+x\n*** End Patch",
            &[("a", "old\n")],
        );
        assert_eq!(path, "a");
        assert!(message.contains("already exists"), "{message}");

        let (path, message) = preflight_error(
            "*** Begin Patch\n*** Delete File: missing\n*** End Patch",
            &[],
        );
        assert_eq!(path, "missing");
        assert!(message.contains("does not exist"), "{message}");

        let (path, message) = preflight_error(
            "*** Begin Patch\n*** Update File: missing\n@@\n-a\n+b\n*** End Patch",
            &[],
        );
        assert_eq!(path, "missing");
        assert!(message.contains("does not exist"), "{message}");

        let (path, message) = preflight_error(
            "*** Begin Patch\n*** Update File: missing\n*** Move to: dest\n*** End Patch",
            &[],
        );
        assert_eq!(path, "missing");
        assert!(message.contains("does not exist"), "{message}");

        let (path, message) = conflict_error(
            "*** Begin Patch\n*** Update File: a\n*** Move to: b\n*** End Patch",
            &[("a", "old\n"), ("b", "occupied\n")],
        );
        assert_eq!(path, "b");
        assert!(message.contains("destination already exists"), "{message}");

        let plan = plan_text(
            "*** Begin Patch\n*** Update File: m\n*** Move to: n\n*** End Patch",
            &[("m", "bytes")],
        )
        .unwrap();
        assert_eq!(
            plan.changes,
            vec![PlannedChange::Move {
                from: "m".into(),
                to: "n".into(),
                old_contents: b"bytes".to_vec(),
                new_contents: b"bytes".to_vec(),
            }]
        );

        let plan = plan_text(
            "*** Begin Patch\n*** Update File: move.txt\n*** Move to: new.txt\n@@\n-old\n+new\n*** End Patch",
            &[("move.txt", "old\n")],
        )
        .unwrap();
        assert_eq!(
            plan.changes,
            vec![PlannedChange::Move {
                from: "move.txt".into(),
                to: "new.txt".into(),
                old_contents: b"old\n".to_vec(),
                new_contents: b"new\n".to_vec(),
            }]
        );

        let plan = plan_text(
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch",
            &[("gone.txt", "unused\n")],
        )
        .unwrap();
        assert_eq!(
            plan.changes,
            vec![PlannedChange::Delete {
                path: "gone.txt".into(),
                old_contents: b"unused\n".to_vec(),
            }]
        );
    }

    #[test]
    fn plans_add_update_and_delete_across_multiple_files() {
        let plan = plan_text(
            "*** Begin Patch\n*** Add File: added.txt\n+new\n*** Update File: changed.txt\n@@\n-old\n+new\n*** Delete File: removed.txt\n*** End Patch",
            &[("changed.txt", "old\n"), ("removed.txt", "gone\n")],
        )
        .unwrap();
        assert_eq!(
            plan.changes,
            vec![
                PlannedChange::Add {
                    path: "added.txt".into(),
                    contents: b"new\n".to_vec(),
                },
                PlannedChange::Update {
                    path: "changed.txt".into(),
                    old_contents: b"old\n".to_vec(),
                    new_contents: b"new\n".to_vec(),
                },
                PlannedChange::Delete {
                    path: "removed.txt".into(),
                    old_contents: b"gone\n".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn rejects_binary_and_invalid_utf8_content_for_updates_but_allows_delete() {
        let patch =
            parse_patch("*** Begin Patch\n*** Update File: f\n@@\n-a\n+b\n*** End Patch").unwrap();
        let mut nul = PatchInputs::new();
        nul.insert("f", b"a\0b".to_vec());
        match plan_patch(&patch, &nul) {
            Err(PatchError::File { path, message }) => {
                assert_eq!(path, "f");
                assert!(message.contains("NUL"), "{message}");
            }
            other => panic!("expected a binary error, got {other:?}"),
        }

        let mut invalid = PatchInputs::new();
        invalid.insert("f", vec![0xff, 0xfe]);
        match plan_patch(&patch, &invalid) {
            Err(PatchError::File { path, message }) => {
                assert_eq!(path, "f");
                assert!(message.contains("UTF-8"), "{message}");
            }
            other => panic!("expected a UTF-8 error, got {other:?}"),
        }

        let move_patch =
            parse_patch("*** Begin Patch\n*** Update File: f\n*** Move to: g\n*** End Patch")
                .unwrap();
        assert!(matches!(
            plan_patch(&move_patch, &nul),
            Ok(PatchPlan { .. })
        ));

        let move_edit = parse_patch(
            "*** Begin Patch\n*** Update File: f\n*** Move to: g\n@@\n-a\n+b\n*** End Patch",
        )
        .unwrap();
        assert!(matches!(
            plan_patch(&move_edit, &nul),
            Err(PatchError::File { .. })
        ));

        let delete = parse_patch("*** Begin Patch\n*** Delete File: f\n*** End Patch").unwrap();
        let deleted = plan_patch(&delete, &nul).unwrap();
        assert_eq!(
            deleted.changes,
            vec![PlannedChange::Delete {
                path: "f".into(),
                old_contents: b"a\0b".to_vec(),
            }]
        );

        let (line, message) = syntax_message("*** Begin Patch\0\n*** End Patch");
        assert_eq!(line, 1);
        assert!(message.contains("NUL"), "{message}");
    }

    #[test]
    fn failures_produce_no_partial_plan_and_planning_never_touches_the_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut inputs = PatchInputs::new();
        inputs.insert("a.txt", std::fs::read(&path).unwrap());

        let parsed = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+2\n*** End Patch",
        )
        .unwrap();
        let plan = plan_patch(&parsed, &inputs).unwrap();
        assert_eq!(planned_update(&plan, "a.txt"), b"one\n2\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");

        let before = inputs.clone();
        let parsed = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+2\n*** Update File: b.txt\n@@\n-x\n+y\n*** End Patch",
        )
        .unwrap();
        assert!(plan_patch(&parsed, &inputs).is_err());
        assert_eq!(inputs, before);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn context_headers_and_end_of_file_anchor_hunks() {
        let plan = plan_text(
            "*** Begin Patch\n*** Update File: f\n@@\n x\n-x\n+y\n*** End of File\n*** End Patch",
            &[("f", "x\nx\n")],
        )
        .unwrap();
        assert_eq!(planned_update(&plan, "f"), b"x\ny\n");

        let (path, hunk, message) = update_error(
            "*** Begin Patch\n*** Update File: f\n@@\n-x\n+y\n*** End of File\n*** End Patch",
            &[("f", "x\ny\n")],
        );
        assert_eq!(path, "f");
        assert_eq!(hunk, 1);
        assert!(message.contains("end-of-file"), "{message}");
    }

    #[test]
    fn rejects_move_and_end_of_file_markers_in_wrong_positions() {
        for patch in [
            "*** Begin Patch\n*** Move to: a\n*** End Patch",
            "*** Begin Patch\n*** Update File: a\n@@\n-a\n+b\n*** Move to: c\n*** End Patch",
        ] {
            let (_, message) = syntax_message(patch);
            assert!(message.contains("unknown patch operation"), "{message}");
        }

        let (_, message) = syntax_message(
            "*** Begin Patch\n*** Update File: a\n@@\n*** End of File\n*** End Patch",
        );
        assert!(message.contains("at least one change line"), "{message}");

        let (line, message) = syntax_message(
            "*** Begin Patch\n*** Update File: a\n@@\n-a\n+b\n*** End of File\n+c\n*** End Patch",
        );
        assert_eq!(line, 7);
        assert!(message.contains("unexpected content after"), "{message}");
    }
}
