//! Bounded, stateless incremental output pages for `TaskGet`.
//!
//! A cursor records how many bytes of each append-only stream a reader has
//! already seen. It is never stored server-side, so two readers holding the
//! same cursor see the same bytes and cannot consume each other's output. A
//! cursor conveys no permission: every read still resolves the task through the
//! caller's own session first.
//!
//! Generation is taken from the **opened** file, and the same handle is used for
//! validation, the read and the next cursor, so replacing the path between those
//! steps cannot label new bytes with old offsets. Supported mutations, per
//! stream: appending (generation unchanged), creating a stream the cursor never
//! advanced on, rotation to a new inode, and truncation below the cursor
//! position. Reported as a stale cursor: a missing consumed stream, a length
//! below the cursor position, and a changed inode.
//! **Not detected:** a same-inode rewrite, or truncation followed by regrowing
//! past the saved offset; and on Windows there is no portable inode, so rotation
//! is not detected there. Those limits are documented rather than claimed.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// Page size used when a cursor is supplied without `max_output_bytes`.
pub(crate) const DEFAULT_PAGE_BYTES: u64 = 16 * 1024;
/// Hard ceiling for one page, so a single read stays predictable.
pub(crate) const MAX_PAGE_BYTES: u64 = 1024 * 1024;
/// Minimum page size: a page must be able to hold one UTF-8 character.
pub(crate) const MIN_PAGE_BYTES: u64 = 4;
/// Ceiling on how much of a JSONL log one page may scan to reach one complete
/// record. A record larger than this cannot be paged at all.
const EVENT_SCAN_LIMIT: u64 = 8 * 1024 * 1024;
const CURSOR_PREFIX: &str = "tg1.";
const STALE: &str = "TaskGet cursor is stale: the task output was replaced, truncated or rewritten. Read again without a cursor to start a new page sequence";

/// How a single `TaskGet` call wants its log text.
#[derive(Debug, Clone)]
pub(crate) enum PageRequest {
    /// No cursor and no bound: keep the historical full-snapshot behaviour.
    Disabled,
    /// A bounded page, optionally continuing from a previous cursor.
    Page {
        cursor: Option<Cursor>,
        max_bytes: u64,
    },
}

impl PageRequest {
    pub(crate) fn parse(input: &Value) -> Result<Self> {
        let cursor = match input.get("cursor") {
            Some(value) if !value.is_null() => Some(Cursor::decode(
                value.as_str().context("TaskGet cursor must be a string")?,
            )?),
            _ => None,
        };
        let bound = match input.get("max_output_bytes") {
            Some(value) if !value.is_null() => Some(
                value
                    .as_u64()
                    .context("TaskGet max_output_bytes must be a positive integer")?,
            ),
            _ => None,
        };
        if cursor.is_none() && bound.is_none() {
            return Ok(Self::Disabled);
        }
        let max_bytes = bound.unwrap_or(DEFAULT_PAGE_BYTES);
        ensure!(
            (MIN_PAGE_BYTES..=MAX_PAGE_BYTES).contains(&max_bytes),
            "TaskGet max_output_bytes must be {MIN_PAGE_BYTES}..={MAX_PAGE_BYTES}; a smaller page cannot hold one UTF-8 character"
        );
        Ok(Self::Page { cursor, max_bytes })
    }

    /// Cursor page mode can only be used where an append-only stream exists.
    pub(crate) fn cursor(&self) -> Option<&Cursor> {
        match self {
            Self::Page { cursor, .. } => cursor.as_ref(),
            Self::Disabled => None,
        }
    }

    pub(crate) fn max_bytes(&self) -> Option<u64> {
        match self {
            Self::Page { max_bytes, .. } => Some(*max_bytes),
            Self::Disabled => None,
        }
    }
}

/// Opaque reader position. Bound to a task, a backend and one output generation
/// per stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    v: u8,
    task: String,
    backend: String,
    out_gen: String,
    err_gen: String,
    out: u64,
    err: u64,
    /// Bytes of the record starting at `out` already returned. Only the JSONL
    /// backend uses it; the default keeps older cursors readable.
    #[serde(default)]
    out_skip: u64,
    /// Whether a text-bearing JSONL record was already returned before `out`.
    /// Raw bytes alone are not enough: metadata records advance the offset but
    /// do not contribute a record separator.
    #[serde(default)]
    out_joined: bool,
}

impl Cursor {
    pub(crate) fn new(
        task: &str,
        backend: &str,
        out_gen: String,
        err_gen: String,
        out: u64,
        err: u64,
    ) -> Self {
        Self {
            v: 1,
            task: task.to_owned(),
            backend: backend.to_owned(),
            out_gen,
            err_gen,
            out,
            err,
            out_skip: 0,
            out_joined: false,
        }
    }

    pub(crate) fn with_out_skip(mut self, skip: u64) -> Self {
        self.out_skip = skip;
        self
    }

    pub(crate) fn with_out_join(mut self, joined: bool) -> Self {
        self.out_joined = joined;
        self
    }

    pub(crate) fn out(&self) -> u64 {
        self.out
    }

    pub(crate) fn err(&self) -> u64 {
        self.err
    }

    pub(crate) fn out_skip(&self) -> u64 {
        self.out_skip
    }

    pub(crate) fn out_joined(&self) -> bool {
        self.out_joined
    }

    pub(crate) fn out_gen(&self) -> &str {
        &self.out_gen
    }

    pub(crate) fn err_gen(&self) -> &str {
        &self.err_gen
    }

    pub(crate) fn encode(&self) -> String {
        // Serialisation cannot fail for these plain fields; an empty payload
        // would decode as malformed rather than silently resetting the reader.
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        format!("{CURSOR_PREFIX}{}", hex(&bytes))
    }

    pub(crate) fn decode(value: &str) -> Result<Self> {
        let encoded = value
            .strip_prefix(CURSOR_PREFIX)
            .context("TaskGet cursor is malformed")?;
        let bytes = unhex(encoded).context("TaskGet cursor is malformed")?;
        let cursor: Self = serde_json::from_slice(&bytes).context("TaskGet cursor is malformed")?;
        ensure!(
            cursor.v == 1,
            "TaskGet cursor has an unsupported version; read again without a cursor"
        );
        Ok(cursor)
    }

    /// Ownership is re-checked by the caller; this only ensures the cursor was
    /// issued for this exact task and backend.
    pub(crate) fn validate(&self, task: &str, backend: &str) -> Result<()> {
        ensure!(
            self.task == task,
            "TaskGet cursor belongs to a different task"
        );
        ensure!(
            self.backend == backend,
            "TaskGet cursor was issued for a different output backend; read again without a cursor"
        );
        Ok(())
    }

    /// A stream the reader never advanced on may legitimately have been created
    /// after the cursor was issued, so only streams with consumed progress are
    /// compared. Progress includes a partial JSONL record (`out_skip > 0`) and
    /// metadata bytes already consumed, not just a non-zero byte offset.
    pub(crate) fn validate_stream(&self, upto: u64, stored: &str, current: &str) -> Result<()> {
        self.validate_stream_at(upto, self.out_skip, stored, current)
    }

    /// Explicit per-stream progress, for callers whose streams do not share the
    /// cursor's single JSONL skip value (stderr must pass `0`).
    pub(crate) fn validate_stream_at(
        &self,
        upto: u64,
        skip: u64,
        stored: &str,
        current: &str,
    ) -> Result<()> {
        ensure!(upto == 0 && skip == 0 || stored == current, STALE);
        Ok(())
    }
}

/// One stream's page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StreamPage {
    pub(crate) text: String,
    /// File offset of the next record (or byte) to read.
    pub(crate) next: u64,
    /// Text bytes already returned from the record at `next`.
    pub(crate) skip: u64,
    /// Whether a text-bearing record precedes the next position, so the next
    /// page knows whether a record separator is due.
    pub(crate) joined: bool,
    pub(crate) truncated: bool,
    /// Bytes that could not be decoded and became U+FFFD.
    pub(crate) invalid_bytes: usize,
    /// Identity of the opened file this page came from, for the next cursor.
    pub(crate) generation: String,
}

impl StreamPage {
    fn absent() -> Self {
        Self {
            generation: "none".into(),
            ..Default::default()
        }
    }
}

/// One opened append-only stream. Identity, length and bytes all come from this
/// single handle, so a rename between validation and reading cannot mix them.
pub(crate) struct Stream {
    file: std::fs::File,
    device: u64,
    inode: u64,
    size: u64,
}

impl Stream {
    pub(crate) fn generation(&self) -> String {
        format!("{}:{}", self.device, self.inode)
    }
}

/// Open `path` once and capture its identity and length. A missing file is
/// reported as `None`; that is only acceptable at offset 0, because nothing was
/// consumed from it.
pub(crate) fn open_stream(path: &Path, upto: u64) -> Result<Option<Stream>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(upto == 0, "{STALE}: the task output no longer exists");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    // Windows has no portable inode here, so rotation is not detected there.
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    ensure!(
        metadata.len() >= upto,
        "{STALE}: the task output is shorter than the cursor position"
    );
    Ok(Some(Stream {
        file,
        device,
        inode,
        size: metadata.len(),
    }))
}

/// Generation of a stream that may be absent, without keeping the handle.
pub(crate) fn stream_generation(path: &Path, upto: u64) -> Result<String> {
    Ok(open_stream(path, upto)?
        .map(|stream| stream.generation())
        .unwrap_or_else(|| "none".into()))
}

/// Generation of an already-opened stream, for validating a cursor against the
/// same snapshot that will be read.
pub(crate) fn stream_generation_of(stream: &Option<Stream>) -> String {
    stream
        .as_ref()
        .map(Stream::generation)
        .unwrap_or_else(|| "none".into())
}

/// Result of decoding one bounded byte window.
struct Decoded {
    text: String,
    consumed: usize,
    truncated: bool,
    invalid: usize,
}

/// Decode at most `budget` bytes of UTF-8 text, never splitting a character and
/// never emitting more text bytes than the budget. An incomplete trailing
/// sequence is left for the next page while the producer may still be writing;
/// a terminal end of stream replaces it and reports the bytes as invalid.
fn decode_bounded(bytes: &[u8], budget: usize, terminal: bool) -> Decoded {
    const REPLACEMENT: &str = "\u{FFFD}";
    let mut text = String::new();
    let mut consumed = 0usize;
    let mut invalid = 0usize;
    let mut truncated = false;
    while consumed < bytes.len() {
        let rest = &bytes[consumed..];
        let (valid, invalid_len, incomplete) = match std::str::from_utf8(rest) {
            Ok(_) => (rest.len(), 0usize, false),
            Err(error) => (
                error.valid_up_to(),
                error.error_len().unwrap_or(0),
                error.error_len().is_none(),
            ),
        };
        if valid > 0 {
            let room = budget.saturating_sub(text.len());
            if valid > room {
                // Cut inside the valid run on a character boundary.
                let mut end = room;
                while end > 0 && std::str::from_utf8(&rest[..end]).is_err() {
                    end -= 1;
                }
                if end > 0 {
                    text.push_str(std::str::from_utf8(&rest[..end]).expect("valid prefix"));
                    consumed += end;
                }
                truncated = true;
                break;
            }
            text.push_str(std::str::from_utf8(&rest[..valid]).expect("valid prefix"));
            consumed += valid;
        }
        if incomplete {
            if !terminal {
                // The producer may finish this character later; keep the offset
                // here rather than consuming or replacing it now.
                break;
            }
            // At a terminal end of stream those bytes can never become valid.
            let remaining = bytes.len() - consumed;
            if budget.saturating_sub(text.len()) < REPLACEMENT.len() {
                truncated = true;
                break;
            }
            text.push_str(REPLACEMENT);
            invalid += remaining;
            consumed = bytes.len();
            break;
        }
        if invalid_len > 0 {
            if budget.saturating_sub(text.len()) < REPLACEMENT.len() {
                truncated = true;
                break;
            }
            text.push_str(REPLACEMENT);
            invalid += invalid_len;
            consumed += invalid_len;
            continue;
        }
        break;
    }
    Decoded {
        text,
        consumed,
        truncated,
        invalid,
    }
}

/// Read at most `budget` bytes of text from `offset` using the opened snapshot.
pub(crate) fn read_stream_page(
    stream: Option<&mut Stream>,
    offset: u64,
    budget: u64,
    terminal: bool,
) -> Result<StreamPage> {
    let Some(stream) = stream else {
        ensure!(offset == 0, "{STALE}: the task output no longer exists");
        return Ok(StreamPage::absent());
    };
    ensure!(
        offset <= stream.size,
        "{STALE}: the task output is shorter than the cursor position"
    );
    let generation = stream.generation();
    let size = stream.size;
    stream.file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    (&mut stream.file)
        .take(budget.max(4).min(MAX_PAGE_BYTES))
        .read_to_end(&mut bytes)?;
    let at_end = offset + bytes.len() as u64 >= size;
    let decoded = decode_bounded(&bytes, budget as usize, terminal && at_end);
    let next = offset + decoded.consumed as u64;
    Ok(StreamPage {
        text: decoded.text,
        next,
        skip: 0,
        joined: false,
        truncated: next < size || decoded.truncated,
        invalid_bytes: decoded.invalid,
        generation,
    })
}

/// Read the `result`/`delta` text of complete JSONL records from `offset`.
/// `skip` continues inside the record at `offset`, so a record larger than one
/// page is returned across several pages. `joined` says whether a text-bearing
/// record was already returned before `offset`, which decides whether the first
/// text record of this page needs the snapshot's `"\n"` separator. Records
/// without a string `result`/`delta` are dropped exactly as the full snapshot
/// drops them, so metadata never creates a separator. The scan window grows
/// geometrically up to [`EVENT_SCAN_LIMIT`] before a record is declared
/// unpaginatable, so a large record is never misreported because of the first
/// small window.
pub(crate) fn read_stream_events(
    stream: Option<&mut Stream>,
    offset: u64,
    skip: u64,
    budget: u64,
    terminal: bool,
    joined: bool,
) -> Result<StreamPage> {
    let Some(stream) = stream else {
        ensure!(offset == 0, "{STALE}: the task output no longer exists");
        return Ok(StreamPage::absent());
    };
    ensure!(
        offset <= stream.size,
        "{STALE}: the task output is shorter than the cursor position"
    );
    let generation = stream.generation();
    let size = stream.size;
    let mut scan = budget.saturating_mul(8).max(64 * 1024).saturating_add(skip);
    let mut bytes = Vec::new();
    let mut complete;
    loop {
        let limit = scan.min(EVENT_SCAN_LIMIT).min(size.saturating_sub(offset));
        bytes.clear();
        stream.file.seek(SeekFrom::Start(offset))?;
        (&mut stream.file).take(limit).read_to_end(&mut bytes)?;
        complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let at_end = offset + bytes.len() as u64 >= size;
        if complete > 0 || at_end || limit >= EVENT_SCAN_LIMIT {
            break;
        }
        // A complete record is not visible yet: widen the window, still bounded.
        scan = scan.saturating_mul(4).min(EVENT_SCAN_LIMIT);
    }
    let at_end = offset + bytes.len() as u64 >= size;
    ensure!(
        complete > 0 || bytes.is_empty() || at_end,
        "TaskGet output record exceeds the {EVENT_SCAN_LIMIT} byte page limit; this backend cannot page it"
    );
    // A terminal stream's last record may legitimately have no trailing
    // newline; parsing it is what the full snapshot does. While the producer is
    // still running an unterminated record is left for the next page.
    if complete == 0 && !bytes.is_empty() && at_end && terminal {
        complete = bytes.len();
    }

    let mut text = String::new();
    let mut consumed = 0usize;
    // Whether a text-bearing record has already been returned, either by an
    // earlier page or earlier in this one. Raw offset is not enough: metadata
    // records can be consumed without contributing any joined text.
    let mut prior_text = joined;
    let mut skip_left = skip;
    let mut first_line = true;
    for line in bytes[..complete].split_inclusive(|byte| *byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.is_empty() {
            // Blank lines are not records and are dropped by the snapshot.
            if first_line {
                ensure!(
                    skip == 0,
                    "TaskGet cursor is stale: its position no longer matches an output record. Read again without a cursor"
                );
                first_line = false;
            }
            consumed += line.len();
            continue;
        }
        let event: Value =
            serde_json::from_slice(body).context("Invalid background stream JSON")?;
        // Keep the full snapshot's selection exactly: a record contributes text
        // only when `result` or else `delta` is a string. A genuine empty string
        // is real text; a missing or non-string field is not.
        let raw = event
            .get("result")
            .and_then(Value::as_str)
            .or_else(|| event.get("delta").and_then(Value::as_str));
        let Some(raw) = raw else {
            // Metadata is consumed but contributes no text, so it must not
            // advance the join state or add a separator.
            if first_line {
                ensure!(
                    skip == 0,
                    "TaskGet cursor is stale: its position no longer matches an output record. Read again without a cursor"
                );
                first_line = false;
            }
            consumed += line.len();
            continue;
        };
        // Snap to a character boundary: cursors only advance by whole
        // characters, but a hand-built cursor is not trusted.
        let mut start = (skip_left as usize).min(raw.len());
        while start > 0 && !raw.is_char_boundary(start) {
            start -= 1;
        }
        skip_left -= start as u64;
        // `skip` only applies to the record at `offset`; any other first line
        // means the cursor no longer matches the stream.
        let continuation = first_line && skip > 0;
        if first_line {
            ensure!(
                skip_left == 0,
                "TaskGet cursor is stale: its position no longer matches an output record. Read again without a cursor"
            );
            first_line = false;
        }
        let piece = &raw[start..];
        // The snapshot joins the text of text-bearing records with "\n". A
        // continuation inside one record never repeats its separator, and
        // skipped metadata never makes `prior_text` true.
        let separator = if continuation {
            0
        } else {
            usize::from(prior_text)
        };
        let extra = separator + piece.len();
        if text.len() + extra > budget as usize {
            let room = budget as usize - text.len();
            let mut end = room.saturating_sub(separator).min(piece.len());
            while end > 0 && !piece.is_char_boundary(end) {
                end -= 1;
            }
            ensure!(
                end > 0 || consumed > 0,
                "TaskGet max_output_bytes cannot hold the next UTF-8 character; request a larger page"
            );
            if end > 0 {
                if separator == 1 {
                    text.push('\n');
                }
                text.push_str(&piece[..end]);
            }
            return Ok(StreamPage {
                text,
                next: offset + consumed as u64,
                skip: (start + end) as u64,
                joined: true,
                truncated: true,
                invalid_bytes: 0,
                generation,
            });
        }
        if separator == 1 {
            text.push('\n');
        }
        text.push_str(piece);
        prior_text = true;
        consumed += line.len();
    }
    let next = offset + consumed as u64;
    Ok(StreamPage {
        text,
        next,
        skip: 0,
        joined: prior_text,
        truncated: next < size,
        invalid_bytes: 0,
        generation,
    })
}

/// Convenience wrappers that open, read and drop one stream. Callers that need
/// the generation for a cursor must use the `*_stream_*` forms directly.
pub(crate) fn read_bytes_page(
    path: &Path,
    offset: u64,
    budget: u64,
    terminal: bool,
) -> Result<StreamPage> {
    let mut stream = open_stream(path, offset)?;
    read_stream_page(stream.as_mut(), offset, budget, terminal)
}

pub(crate) fn read_event_page(
    path: &Path,
    offset: u64,
    skip: u64,
    budget: u64,
    terminal: bool,
) -> Result<StreamPage> {
    let mut stream = open_stream(path, offset)?;
    read_stream_events(stream.as_mut(), offset, skip, budget, terminal, false)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_file(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stdout.log");
        std::fs::write(&path, bytes).unwrap();
        (root, path)
    }

    #[test]
    fn cursor_round_trips_and_rejects_malformed_values() {
        let cursor = Cursor::new("task-a", "shell", "1:2".into(), "none".into(), 10, 4);
        let encoded = cursor.encode();
        assert!(encoded.starts_with("tg1."));
        assert_eq!(Cursor::decode(&encoded).unwrap(), cursor);
        for bad in [
            "",
            "tg1",
            "tg1.",
            "tg1.zz",
            "tg2.00",
            "not-a-cursor",
            "tg1.7b7d",
        ] {
            assert!(Cursor::decode(bad).is_err(), "{bad} must be rejected");
        }
        let mut other = cursor.clone();
        other.v = 9;
        assert!(Cursor::decode(&other.encode()).is_err());
    }

    #[test]
    fn cursor_is_bound_to_task_and_backend() {
        let cursor = Cursor::new("task-a", "shell", "gen".into(), "none".into(), 0, 0);
        cursor.validate("task-a", "shell").unwrap();
        assert!(cursor.validate("task-b", "shell").is_err());
        assert!(cursor.validate("task-a", "job").is_err());
    }

    #[test]
    fn page_request_validation_is_explicit() {
        assert!(matches!(
            PageRequest::parse(&serde_json::json!({})).unwrap(),
            PageRequest::Disabled
        ));
        assert_eq!(
            PageRequest::parse(&serde_json::json!({"max_output_bytes":4}))
                .unwrap()
                .max_bytes(),
            Some(4)
        );
        assert!(PageRequest::parse(&serde_json::json!({"cursor":"tg1.00"})).is_err());
        for bad in [
            serde_json::json!({"max_output_bytes":0}),
            serde_json::json!({"max_output_bytes":3}),
            serde_json::json!({"max_output_bytes":MAX_PAGE_BYTES + 1}),
            serde_json::json!({"max_output_bytes":"16"}),
            serde_json::json!({"max_output_bytes":-1}),
            serde_json::json!({"max_output_bytes":1.5}),
            serde_json::json!({"cursor":7}),
        ] {
            assert!(PageRequest::parse(&bad).is_err(), "{bad} must be rejected");
        }
        assert_eq!(
            PageRequest::parse(&serde_json::json!({"cursor":null,"max_output_bytes":null}))
                .unwrap()
                .max_bytes(),
            None
        );
    }

    #[test]
    fn a_stream_created_after_the_cursor_is_not_stale() {
        let cursor = Cursor::new("task", "shell", "none".into(), "none".into(), 0, 0);
        // stdout was never advanced on, so its new generation is accepted.
        cursor.validate_stream(0, cursor.out_gen(), "7:9").unwrap();
        // stderr was advanced on, so its generation must match.
        assert!(cursor.validate_stream(4, cursor.err_gen(), "7:9").is_err());
        cursor.validate_stream(4, "7:9", "7:9").unwrap();
    }

    #[test]
    fn incomplete_tail_is_retained_while_running_and_replaced_when_terminal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stdout.log");
        // A four-byte character split across two writes.
        std::fs::write(&path, b"ab\xF0\x9F").unwrap();
        let running = read_bytes_page(&path, 0, 16, false).unwrap();
        assert_eq!(running.text, "ab");
        assert_eq!(running.next, 2);
        assert!(running.truncated);
        // The continuation arrives: no character is lost or duplicated.
        std::fs::write(&path, b"ab\xF0\x9F\x98\x80cd").unwrap();
        let rest = read_bytes_page(&path, running.next, 16, false).unwrap();
        assert_eq!(rest.text, "😀cd");

        // A terminal stream ends where it ends: the bytes can never complete.
        std::fs::write(&path, b"ab\xF0\x9F").unwrap();
        let terminal = read_bytes_page(&path, 0, 16, true).unwrap();
        assert_eq!(terminal.text, "ab\u{FFFD}");
        assert_eq!(terminal.invalid_bytes, 2);
        assert_eq!(terminal.next, 4);
        assert!(!terminal.truncated);
    }

    #[test]
    fn invalid_bytes_never_exceed_the_text_budget() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stdout.log");
        // Each invalid byte decodes to a three-byte replacement character.
        std::fs::write(&path, [0xffu8; 8]).unwrap();
        let page = read_bytes_page(&path, 0, 4, true).unwrap();
        assert!(page.text.len() <= 4, "{page:?}");
        assert_eq!(page.text, "\u{FFFD}");
        assert_eq!(page.invalid_bytes, 1);
        assert!(page.truncated);
    }

    #[test]
    fn rejects_page_too_small_for_one_character() {
        let (_root, path) = temp_file("😀tail".as_bytes());
        let page = read_bytes_page(&path, 0, 4, false).unwrap();
        assert_eq!(page.text, "😀");
        assert_eq!(page.next, 4);
        assert!(page.truncated);
        // A budget below one character makes no progress rather than looping,
        // and the public request layer rejects such a bound outright.
        let too_small = read_bytes_page(&path, 0, MIN_PAGE_BYTES - 1, false).unwrap();
        assert_eq!(too_small.text, "");
        assert_eq!(too_small.next, 0);
        assert!(too_small.truncated);
    }

    #[test]
    fn utf8_boundary_is_never_split() {
        let text = "aé€😀b";
        let (_root, path) = temp_file(text.as_bytes());
        for cap in MIN_PAGE_BYTES..=16 {
            let mut offset = 0;
            let mut rebuilt = String::new();
            for _ in 0..32 {
                let page = read_bytes_page(&path, offset, cap, false).unwrap();
                rebuilt.push_str(&page.text);
                if page.next == offset || !page.truncated {
                    break;
                }
                offset = page.next;
            }
            assert_eq!(rebuilt, text, "cap {cap}");
        }
    }

    #[test]
    fn byte_pages_are_exact_bounds_and_stale_is_explicit() {
        let (_root, path) = temp_file(b"0123456789");
        let first = read_bytes_page(&path, 0, 4, false).unwrap();
        assert_eq!(
            (first.text.as_str(), first.next, first.truncated),
            ("0123", 4, true)
        );
        let next = read_bytes_page(&path, first.next, 4, false).unwrap();
        assert_eq!(
            (next.text.as_str(), next.next, next.truncated),
            ("4567", 8, true)
        );
        let last = read_bytes_page(&path, next.next, 4, false).unwrap();
        assert_eq!(
            (last.text.as_str(), last.next, last.truncated),
            ("89", 10, false)
        );
        // Replaying the same cursor is deterministic and independent.
        assert_eq!(read_bytes_page(&path, 4, 4, false).unwrap(), next);
        // A cursor past the end is stale, never a silent restart.
        assert!(read_bytes_page(&path, 11, 4, false).is_err());
    }

    #[test]
    fn generation_detects_rotation_and_truncation_and_documents_the_limit() {
        let (root, path) = temp_file(b"consumer-prefix-and-more");
        let generation = stream_generation(&path, 8).unwrap();
        // Appending keeps the generation stable.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"appended").unwrap();
        drop(file);
        assert_eq!(stream_generation(&path, 8).unwrap(), generation);

        // Truncation below the cursor is reported.
        std::fs::write(&path, b"short").unwrap();
        assert!(stream_generation(&path, 8).is_err());
        assert!(read_bytes_page(&path, 8, 4, false).is_err());

        // Rotation to a new file is a new identity.
        let rotated = root.path().join("rotated.log");
        std::fs::write(&rotated, b"consumer-prefix-and-more").unwrap();
        assert_ne!(
            stream_generation(&path, 0).unwrap(),
            stream_generation(&rotated, 0).unwrap()
        );

        // Documented limit: a same-inode rewrite of equal-or-longer length is
        // NOT detected, and the module does not claim otherwise.
        let before = stream_generation(&rotated, 0).unwrap();
        std::fs::write(&rotated, b"CONSUMER-PREFIX-AND-MORE").unwrap();
        assert_eq!(stream_generation(&rotated, 0).unwrap(), before);

        // A missing file at a non-zero offset is stale, not empty output.
        assert!(stream_generation(&root.path().join("gone.log"), 4).is_err());
        assert_eq!(
            stream_generation(&root.path().join("gone.log"), 0).unwrap(),
            "none"
        );
    }

    #[test]
    fn one_opened_snapshot_is_used_for_validation_read_and_next_generation() {
        let (root, path) = temp_file(b"original-bytes");
        let mut stream = open_stream(&path, 0).unwrap().unwrap();
        let generation = stream.generation();
        // Replace the path with a new inode while the reader holds the old one.
        let replacement = root.path().join("replacement.log");
        std::fs::write(&replacement, b"replacement-bytes-and-more").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let page = read_stream_page(Some(&mut stream), 0, 32, false).unwrap();
        // The page came from the opened inode and is labelled with its identity.
        assert_eq!(page.text, "original-bytes");
        assert_eq!(page.generation, generation);
        // A later read of the path sees the replacement: the generation differs,
        // so a cursor from the old snapshot is reported stale.
        assert_ne!(stream_generation(&path, 0).unwrap(), generation);
    }

    #[test]
    fn event_pages_follow_complete_records_and_join_like_a_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        let mut body = String::new();
        for piece in ["alpha", "βeta", "gamma", "delta"] {
            body.push_str(&serde_json::json!({"delta":piece}).to_string());
            body.push('\n');
        }
        std::fs::write(&path, body.as_bytes()).unwrap();

        let mut offset = 0;
        let mut skip = 0;
        let mut joined = false;
        let mut pages = Vec::new();
        for _ in 0..16 {
            let mut stream = open_stream(&path, offset).unwrap();
            let page = read_stream_events(stream.as_mut(), offset, skip, 8, false, joined).unwrap();
            assert!(page.text.len() <= 8, "{page:?}");
            pages.push(page.text.clone());
            offset = page.next;
            skip = page.skip;
            joined = page.joined;
            if !page.truncated {
                break;
            }
        }
        // Each page carries the record separator due at its start, so pages
        // concatenate into the same text as the full snapshot.
        assert_eq!(pages.concat(), "alpha\nβeta\ngamma\ndelta");
    }

    /// The full `job_snapshot` text rule: only records whose `result` (or else
    /// `delta`) is a string contribute, and their text is joined with "\n".
    fn legacy_snapshot(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|event| {
                event["result"]
                    .as_str()
                    .or_else(|| event["delta"].as_str())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Drive the real cursor state across pages.
    fn paginate(path: &Path, budget: u64, terminal: bool) -> String {
        let mut offset = 0;
        let mut skip = 0;
        let mut joined = false;
        let mut text = String::new();
        for _ in 0..512 {
            let mut stream = open_stream(path, offset).unwrap();
            let page = read_stream_events(stream.as_mut(), offset, skip, budget, terminal, joined)
                .unwrap();
            text.push_str(&page.text);
            offset = page.next;
            skip = page.skip;
            joined = page.joined;
            if !page.truncated {
                break;
            }
        }
        text
    }

    fn event_log(records: &[Value]) -> Vec<u8> {
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn metadata_records_never_create_separators() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        // The parent probe counterexample: metadata records before, between and
        // after genuine text must be dropped exactly like the full snapshot.
        std::fs::write(
            &path,
            event_log(&[
                serde_json::json!({"type":"system"}),
                serde_json::json!({"delta":"a"}),
                serde_json::json!({"type":"heartbeat"}),
                serde_json::json!({"delta":"b"}),
                serde_json::json!({"type":"done"}),
            ]),
        )
        .unwrap();
        let page = read_event_page(&path, 0, 0, 64, true).unwrap();
        assert_eq!(page.text, "a\nb");
        assert_eq!(legacy_snapshot(&path), "a\nb");
        assert!(page.joined);
        for budget in [4, 5, 8] {
            assert_eq!(paginate(&path, budget, true), "a\nb", "budget {budget}");
        }
    }

    #[test]
    fn metadata_only_pages_do_not_join_the_first_text_record() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        // A metadata prefix larger than the first 64 KiB scan window forces one
        // or more pages that consume raw bytes and emit no text at all.
        let mut body = Vec::new();
        for index in 0..3000 {
            body.extend(
                format!("{}\n", serde_json::json!({"type":"heartbeat","seq":index})).as_bytes(),
            );
        }
        body.extend(event_log(&[serde_json::json!({"delta":"a"})]));
        std::fs::write(&path, &body).unwrap();

        let first = read_event_page(&path, 0, 0, 4, true).unwrap();
        assert_eq!(first.text, "");
        assert!(first.next > 0, "metadata bytes must still be consumed");
        assert!(!first.joined, "no text record was returned yet");
        assert!(first.truncated);

        // Continuing directly from that page must not invent a leading "\n".
        let mut stream = open_stream(&path, first.next).unwrap();
        let second = read_stream_events(
            stream.as_mut(),
            first.next,
            first.skip,
            4,
            true,
            first.joined,
        )
        .unwrap();
        assert_eq!(second.text, "a");
        assert_eq!(paginate(&path, 4, true), "a");
    }

    #[test]
    fn empty_text_records_still_join_like_the_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        // An empty string is real text; an absent or non-string field is not.
        std::fs::write(
            &path,
            event_log(&[
                serde_json::json!({"type":"system"}),
                serde_json::json!({"delta":""}),
                serde_json::json!({"result":"first","delta":"ignored"}),
                serde_json::json!({"result":5,"delta":"second"}),
                serde_json::json!({"type":"done"}),
            ]),
        )
        .unwrap();
        assert_eq!(legacy_snapshot(&path), "\nfirst\nsecond");
        let page = read_event_page(&path, 0, 0, 64, true).unwrap();
        assert_eq!(page.text, "\nfirst\nsecond");
        for budget in [4, 5, 7, 16] {
            assert_eq!(
                paginate(&path, budget, true),
                "\nfirst\nsecond",
                "budget {budget}"
            );
        }

        // Two empty records join to one separator, and the first page already
        // reports that a text record was returned.
        std::fs::write(
            &path,
            event_log(&[
                serde_json::json!({"delta":""}),
                serde_json::json!({"delta":""}),
            ]),
        )
        .unwrap();
        let first = read_event_page(&path, 0, 0, 64, true).unwrap();
        assert_eq!(first.text, "\n");
        assert!(first.joined);
        assert_eq!(legacy_snapshot(&path), "\n");
    }

    #[test]
    fn event_pages_match_the_legacy_snapshot_across_metadata_and_utf8() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        let long = "z".repeat(40);
        std::fs::write(
            &path,
            event_log(&[
                serde_json::json!({"type":"system"}),
                serde_json::json!({"delta":"α"}),
                serde_json::json!({"type":"heartbeat"}),
                serde_json::json!({"delta":""}),
                serde_json::json!({"result":"x"}),
                serde_json::json!({"result":false,"delta":"😀"}),
                serde_json::json!({"delta":long}),
                serde_json::json!({"type":"done"}),
            ]),
        )
        .unwrap();
        let expected = legacy_snapshot(&path);
        assert_eq!(expected, "α\n\nx\n😀\n".to_owned() + &long);
        for budget in [5, 6, 8, 16, 64] {
            assert_eq!(paginate(&path, budget, true), expected, "budget {budget}");
        }
    }

    #[test]
    fn partial_first_record_progress_rejects_a_rotated_stream() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        std::fs::write(
            &path,
            event_log(&[serde_json::json!({"delta":"abcdefghijk"})]),
        )
        .unwrap();
        let first = read_event_page(&path, 0, 0, 4, false).unwrap();
        assert_eq!(
            (first.text.as_str(), first.next, first.skip),
            ("abcd", 0, 4)
        );

        // Appending keeps the same generation and the continuation replays.
        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        appended
            .write_all(format!("{}\n", serde_json::json!({"delta":"tail"})).as_bytes())
            .unwrap();
        drop(appended);
        let replay = read_event_page(&path, first.next, first.skip, 4, false).unwrap();
        assert_eq!(replay.text, "efgh");
        assert_eq!((replay.next, replay.skip), (0, 8));

        // The probe's cursor shape: byte offset 0 but an intra-record skip.
        let cursor = Cursor::new(
            "task",
            "job",
            first.generation.clone(),
            "none".into(),
            first.next,
            0,
        )
        .with_out_skip(first.skip);
        assert_eq!(cursor.out(), 0);
        assert_eq!(cursor.out_skip(), 4);
        // Unchanged generation accepts the cursor.
        assert!(cursor
            .validate_stream(cursor.out(), cursor.out_gen(), cursor.out_gen())
            .is_ok());
        // Rotation to a new inode must be rejected even though out == 0.
        let replacement = root.path().join("replacement.jsonl");
        std::fs::write(
            &replacement,
            event_log(&[serde_json::json!({"delta":"123456789XY"})]),
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let rotated = stream_generation(&path, 0).unwrap();
        assert!(cursor
            .validate_stream(cursor.out(), cursor.out_gen(), &rotated)
            .is_err());
    }

    #[test]
    fn metadata_bytes_consumed_bind_the_generation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        std::fs::write(&path, event_log(&[serde_json::json!({"type":"system"})])).unwrap();
        let page = read_event_page(&path, 0, 0, 8, true).unwrap();
        assert_eq!(page.text, "");
        assert!(page.next > 0);
        assert!(!page.joined);
        let cursor = Cursor::new(
            "task",
            "job",
            page.generation.clone(),
            "none".into(),
            page.next,
            0,
        );
        let replacement = root.path().join("replacement.jsonl");
        std::fs::write(
            &replacement,
            event_log(&[serde_json::json!({"type":"system"})]),
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let rotated = stream_generation(&path, 0).unwrap();
        assert!(cursor
            .validate_stream(cursor.out(), cursor.out_gen(), &rotated)
            .is_err());
    }

    #[test]
    fn a_record_larger_than_one_page_is_split_instead_of_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        let long = "x".repeat(64);
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({"delta":long}),
                serde_json::json!({"delta":"tail"})
            )
            .as_bytes(),
        )
        .unwrap();
        let mut offset = 0;
        let mut skip = 0;
        let mut joined = false;
        let mut pieces = Vec::new();
        for _ in 0..32 {
            let mut stream = open_stream(&path, offset).unwrap();
            let page = read_stream_events(stream.as_mut(), offset, skip, 8, false, joined).unwrap();
            assert!(page.text.len() <= 8, "{page:?}");
            if !page.text.is_empty() {
                pieces.push(page.text.clone());
            }
            offset = page.next;
            skip = page.skip;
            joined = page.joined;
            if !page.truncated {
                break;
            }
        }
        assert_eq!(pieces.join(""), format!("{long}\ntail"));
    }

    #[test]
    fn a_megabyte_record_is_paged_within_a_small_bound() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        // Far below the 8 MiB ceiling but far above the first small scan window.
        let long = "x".repeat(1024 * 1024 + 17);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::json!({"delta":long})).as_bytes(),
        )
        .unwrap();
        let mut offset = 0;
        let mut skip = 0;
        let mut text = String::new();
        for _ in 0..4096 {
            let page = read_event_page(&path, offset, skip, 16 * 1024, false).unwrap();
            assert!(page.text.len() <= 16 * 1024, "{page:?}");
            text.push_str(&page.text);
            offset = page.next;
            skip = page.skip;
            if !page.truncated {
                break;
            }
        }
        assert_eq!(text.len(), long.len());
        assert_eq!(text, long);
    }

    #[test]
    fn a_terminal_unterminated_record_is_parsed_instead_of_spinning() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        std::fs::write(
            &path,
            serde_json::json!({"delta":"tail"}).to_string().as_bytes(),
        )
        .unwrap();
        // Running: the record may still gain its newline; no progress is claimed.
        let running = read_event_page(&path, 0, 0, 16, false).unwrap();
        assert_eq!(running.text, "");
        assert_eq!(running.next, 0);
        assert!(running.truncated);
        // Terminal: the bytes are all there is, so the record is returned.
        let terminal = read_event_page(&path, 0, 0, 16, true).unwrap();
        assert_eq!(terminal.text, "tail");
        assert!(!terminal.truncated);
    }

    #[test]
    fn a_record_beyond_the_scan_ceiling_is_reported_explicitly() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output.jsonl");
        let long = "y".repeat(EVENT_SCAN_LIMIT as usize + 1);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::json!({"delta":long})).as_bytes(),
        )
        .unwrap();
        let error = read_event_page(&path, 0, 0, 16, false).unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("page limit"), "{text}");
    }
}
