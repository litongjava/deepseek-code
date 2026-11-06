//! Lines logged above the input box that are still on screen, so a click on a
//! tool result can show it in full and a second click can fold it again. Only
//! rows below the clicked block are redrawn; scrollback stays as it was.
use super::change_view::{self, Line};
use crossterm::terminal;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

/// One logged line as written, the screen rows it takes and the tool result
/// it belongs to.
#[derive(Clone)]
pub(super) struct Logged {
    pub text: String,
    rows: usize,
    pub block: Option<usize>,
    flow: Option<std::sync::Arc<Flow>>,
}
#[derive(Clone)]
struct Flow {
    text: String,
    first: String,
    indent: String,
}
/// A tool result with hidden lines, shown short or in full.
#[derive(Clone)]
struct Block {
    copy: Option<(String, String)>,
    background: Option<(std::path::PathBuf, String, String)>,
    prompt: Option<String>,
    review: Option<super::file_review::Review>,
    short: Vec<Line>,
    full: Vec<Line>,
    header_open: bool,
    body_open: bool,
    whole: bool,
    compact_level: u8,
}

#[derive(Default, Clone)]
pub(crate) struct Scrollback {
    logged: VecDeque<Logged>,
    blocks: Vec<Block>,
    latest_prompt: String,
    welcome: Option<String>,
}

/// The last turn's lines, handed from the live view to the prompt after it.
static LAST: std::sync::Mutex<Option<Scrollback>> = std::sync::Mutex::new(None);

/// Return the prompt's retained rows to the next live turn on every exit path.
pub(super) struct Handoff(Scrollback);
impl std::ops::Deref for Handoff {
    type Target = Scrollback;
    fn deref(&self) -> &Scrollback { &self.0 }
}
impl std::ops::DerefMut for Handoff {
    fn deref_mut(&mut self) -> &mut Scrollback { &mut self.0 }
}
impl Drop for Handoff {
    fn drop(&mut self) { std::mem::take(&mut self.0).save(); }
}

static COMPACT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn compact() -> bool { COMPACT.load(std::sync::atomic::Ordering::Relaxed) }
pub(crate) fn toggle_compact() { COMPACT.fetch_xor(true, std::sync::atomic::Ordering::Relaxed); }
pub(crate) fn compact_key(key: crossterm::event::KeyEvent) -> bool {
    key.modifiers.contains(crossterm::event::KeyModifiers::ALT) && key.code == crossterm::event::KeyCode::Char('m')
}

impl Scrollback {
    pub(super) fn latest_prompt(&self) -> &str {
        &self.latest_prompt
    }
    /// Only a freshly drawn startup banner owns these rows. Restored history
    /// starts without them, regardless of whether it contains tools or text.
    pub(crate) fn set_welcome(&mut self, banner: String) {
        self.welcome = Some(banner);
    }
    pub(super) fn has_welcome(&self) -> bool {
        self.welcome.is_some()
    }
    pub(super) fn update_welcome(&mut self, bottom: usize) {
        if self.has_welcome() && self.logged.iter().map(|line| line.rows).sum::<usize>() >= bottom {
            self.welcome = None;
        }
    }
    pub(super) fn repaint_bottom(&self, limit: u16, width: u16) -> u16 {
        match &self.welcome {
            Some(banner) => (self.logged.iter().map(|line| line.rows).sum::<usize>()
                + banner
                    .lines()
                    .map(|line| screen_rows(line, width as usize))
                    .sum::<usize>())
            .min(limit as usize) as u16,
            None => limit,
        }
    }
    pub(super) fn handoff() -> Handoff { Handoff(Self::take()) }
    pub(crate) fn save(self) {
        *LAST.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(self);
    }
    pub(super) fn snapshot() -> Self {
        LAST.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap_or_default()
    }
    pub(crate) fn take() -> Self {
        LAST.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .unwrap_or_default()
    }
    pub(super) fn reflow(&mut self, columns: usize) {
        let mut previous: Option<std::sync::Arc<Flow>> = None;
        let mut previous_block = None;
        for mut line in std::mem::take(&mut self.logged) {
            if let Some(id) = line.block.filter(|id| {
                let entry = &self.blocks[*id];
                entry
                    .prompt
                    .as_ref()
                    .is_some_and(|text| !crate::background_commands::is_notification(text))
                    || entry
                        .short
                        .first()
                        .is_some_and(super::tool_view::is_shell_header)
            }) {
                if previous_block == Some(id) {
                    continue;
                }
                previous_block = Some(id);
                previous = None;
                let entry = &mut self.blocks[id];
                if let Some(text) = &entry.prompt {
                    (entry.short, entry.full) = prompt_lines(text, columns.saturating_sub(1));
                }
                let mut rows = if entry.header_open {
                    rendered_at_width(&entry.full[..1], columns.saturating_sub(1))
                } else {
                    collapsed_at_width(&entry.short[..1], columns.saturating_sub(1))
                };
                rows.extend(if entry.body_open {
                    rendered_at_width(&entry.full[1..], columns.saturating_sub(1))
                } else {
                    collapsed_at_width(&entry.short[1..], columns.saturating_sub(1))
                });
                for text in rows {
                    self.logged.push_back(Logged {
                        rows: screen_rows(&text, columns),
                        text,
                        block: Some(id),
                        flow: None,
                    });
                }
                continue;
            }
            previous_block = None;
            if let Some(flow) = &line.flow {
                if previous
                    .as_ref()
                    .is_some_and(|last| std::sync::Arc::ptr_eq(last, flow))
                {
                    continue;
                }
                previous = Some(flow.clone());
                for text in super::markdown::wrap_styled(
                    &flow.text,
                    &flow.first,
                    &flow.indent,
                    columns.saturating_sub(1),
                ) {
                    self.logged.push_back(Logged {
                        rows: screen_rows(&text, columns),
                        text,
                        block: None,
                        flow: Some(flow.clone()),
                    });
                }
            } else {
                previous = None;
                line.rows = screen_rows(&line.text, columns);
                self.logged.push_back(line);
            }
        }
    }
    /// Retain the logical line so a temporary narrow viewport is reversible.
    pub(crate) fn push_flow(
        &mut self,
        text: &str,
        first: &str,
        indent: &str,
        width: usize,
    ) -> Vec<String> {
        let flow = std::sync::Arc::new(Flow {
            text: text.into(),
            first: first.into(),
            indent: indent.into(),
        });
        let rows = super::markdown::wrap_styled(text, first, indent, width);
        for row in &rows {
            self.logged.push_back(Logged {
                text: row.clone(),
                rows: 1,
                block: None,
                flow: Some(flow.clone()),
            });
        }
        rows
    }
    pub(super) fn push_logged(&mut self, mut line: Logged) -> usize {
        line.rows = screen_rows(&line.text, terminal::size().unwrap_or((80, 24)).0 as usize);
        let rows = line.rows;
        self.logged.push_back(line);
        rows
    }
    /// Records a written line; returns the screen rows it takes.
    pub(crate) fn push(&mut self, text: String, block: Option<usize>) -> usize {
        let (columns, _height) = terminal::size().unwrap_or((80, 24));
        let rows = screen_rows(&text, columns as usize);
        self.logged.push_back(Logged {
            text,
            rows,
            block,
            flow: None,
        });
        rows
    }
    /// A tool result's short lines, to be written and recorded with `push`.
    pub(crate) fn block(&mut self, short: Vec<Line>, full: Vec<Line>) -> (usize, Vec<String>) {
        let lines = collapsed(&short);
        let plain = change_view::plain(&short);
        let whole = plain.trim_start().starts_with("Thought") || plain.contains("Updated ");
        self.blocks.push(Block {
            copy: None,
            background: None,
            prompt: None,
            review: None,
            short,
            full,
            header_open: false,
            body_open: false,
            whole,
            compact_level: 0,
        });
        (self.blocks.len() - 1, lines)
    }

    pub(super) fn bind_tool_copy(&mut self, number: usize, command: String, body: String) {
        let marker = format!("#{} ", number);
        if let Some(block) = self.blocks.iter_mut().rev().find(|block| {
            change_view::plain(&block.short[..block.short.len().min(1)]).contains(&marker)
        }) {
            block.copy = Some((command, body));
        }
    }
    pub(crate) fn bind_copy(&mut self, id: usize, command: String, body: String) {
        if let Some(block) = self.blocks.get_mut(id) {
            block.copy = Some((command, body));
        }
    }
    fn copy_logged(&self, row: usize) -> Option<String> {
        let line = self.logged.get(row)?;
        if let Some(id) = line.block {
            let block = &self.blocks[id];
            if let Some(prompt) = &block.prompt {
                return Some(prompt.clone());
            }
            if let Some((command, body)) = &block.copy {
                let header = self.logged.iter().position(|line| line.block == Some(id))?;
                let header_rows = if block.header_open {
                    rendered(&block.full[..block.full.len().min(1)]).len()
                } else {
                    collapsed(&block.short[..block.short.len().min(1)]).len()
                };
                return Some(
                    if row < header + header_rows {
                        command
                    } else {
                        body
                    }
                    .clone(),
                );
            }
            return Some(change_view::plain(&block.full));
        }
        let start = (0..=row)
            .rev()
            .find(|index| {
                self.logged[*index].block.is_some() || self.logged[*index].text.trim().is_empty()
            })
            .map_or(0, |index| index + 1);
        let end = (row..self.logged.len())
            .find(|index| {
                self.logged[*index].block.is_some() || self.logged[*index].text.trim().is_empty()
            })
            .unwrap_or(self.logged.len());
        (start < end).then(|| {
            self.logged
                .iter()
                .skip(start)
                .take(end - start)
                .map(|line| super::markdown::plain_rendered(&line.text))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }
    pub(super) fn copy_at(&self, bottom: u16, row: u16) -> Option<String> {
        self.copy_logged(self.line_at(bottom, row)?)
    }
    pub(super) fn copy_presentation(&self, row: usize, width: usize) -> Option<String> {
        let rows = if compact() {
            self.compact_rows()
        } else {
            self.logged
                .iter()
                .map(|line| (line.text.clone(), line.block))
                .collect()
        };
        let mut offset = 0;
        for (index, (text, id)) in rows.iter().enumerate() {
            let height = super::markdown::wrap_styled(text, "", "", width.max(1)).len();
            if row < offset + height {
                if !compact() {
                    return self.copy_logged(index);
                }
                if let Some(id) = id {
                    let block = &self.blocks[*id];
                    if let Some(prompt) = &block.prompt {
                        return Some(prompt.clone());
                    }
                    if let Some((command, body)) = &block.copy {
                        let first = index == 0 || rows[index - 1].1 != Some(*id);
                        return Some(if first { command } else { body }.clone());
                    }
                }
                return Some(super::markdown::plain_rendered(text));
            }
            offset += height;
        }
        None
    }

    pub(super) fn bind_background(&mut self, root: std::path::PathBuf, session: String, task: String) {
        if let Some(block) = self.blocks.last_mut() {
            block.background = Some((root, session, task));
            block.whole = true;
        }
    }

    /// Replace a running call in place, preserving later blocks and disclosure.
    pub(super) fn replace_block(
        &mut self,
        block: usize,
        short: Vec<Line>,
        full: Vec<Line>,
        bottom: u16,
    ) -> Option<(u16, Vec<(String, usize)>)> {
        let first = self
            .logged
            .iter()
            .position(|line| line.block == Some(block))?;
        let above: usize = self.logged.iter().skip(first).map(|line| line.rows).sum();
        let start = (bottom as usize).saturating_sub(above) as u16;
        let mut hidden = above.saturating_sub(bottom as usize);
        let entry = self.blocks.get_mut(block)?;
        entry.short = short;
        entry.full = full;
        let mut lines = if entry.header_open {
            rendered(&entry.full[..1])
        } else {
            collapsed(&entry.short[..1])
        };
        lines.extend(if entry.body_open {
            rendered(&entry.full[1..])
        } else {
            collapsed(&entry.short[1..])
        });
        let after = self
            .logged
            .drain(first..)
            .filter(|line| line.block != Some(block));
        let rows: Vec<_> = lines
            .into_iter()
            .map(|text| Logged {
                text,
                rows: 1,
                block: Some(block),
                flow: None,
            })
            .chain(after)
            .collect();
        let mut visible = Vec::new();
        let columns = terminal::size().unwrap_or((80, 24)).0 as usize;
        for mut line in rows {
            line.rows = screen_rows(&line.text, columns);
            self.logged.push_back(line.clone());
            // Keep the full record, but never replay a title already in native scrollback.
            if hidden >= line.rows {
                hidden -= line.rows;
                continue;
            }
            if hidden > 0 {
                for text in super::markdown::wrap_styled(&line.text, "", "", columns)
                    .into_iter()
                    .skip(hidden)
                {
                    visible.push((text, 1));
                }
                hidden = 0;
            } else {
                visible.push((line.text, line.rows));
            }
        }
        Some((start, visible))
    }

    fn refresh_background(&mut self, id: usize) {
        let entry = &mut self.blocks[id];
        let Some((root, session, task)) = &entry.background else { return; };
        // Use the session-owned task API, never paths supplied by tool output.
        let result = deepseek_code_core::tools::tasks::execute_for_session(
            "TaskOutput", serde_json::json!({"task_id": task, "block": false}), root, session,
        );
        let mut full = entry.full[..1].to_vec();
        match result {
            Ok(value) => {
                let status = value["status"].as_str().unwrap_or("unknown");
                let exit = value["result"]["exit_code"].as_i64().map(|code| format!(" · exit {code}")).unwrap_or_default();
                full.push(change_view::muted(&format!("  Background task {task} · {status}{exit} (click to collapse)")));
                if matches!(status, "completed" | "failed" | "cancelled") {
                    entry.short.truncate(1);
                    entry.short.push(change_view::muted(&format!("  Background task {status}{exit} (click to show output)")));
                }
                let output = super::tool_view::result_text(&value.to_string());
                if output.is_empty() { full.push(change_view::muted("  (No output yet)")); }
                else { full.extend(output.lines().map(|line| change_view::body(&format!("  {line}")))); }
            }
            Err(error) => full.push(change_view::muted(&format!("  Output unavailable: {} (click to collapse)", super::tool_view::plain_terminal_text(&error.to_string())))),
        }
        entry.full = full;
    }
    /// Keep prompt text and its disclosure target together across turns.
    pub(crate) fn prompt(&mut self, text: &str) -> (usize, Vec<String>) {
        if let Some(summary) = crate::background_commands::notification_text(text) {
            let lines = summary.lines().map(change_view::body).collect::<Vec<_>>();
            let (id, rows) = self.block(lines.clone(), lines);
            self.blocks[id].prompt = Some(summary);
            self.blocks[id].whole = true;
            return (id, rows);
        }
        if !text.trim_start().starts_with('/') {
            self.latest_prompt = text.to_owned();
        }
        let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
        let (short, full) = prompt_lines(text, width);
        let (id, rows) = self.block(short, full);
        self.blocks[id].prompt = Some(text.to_owned());
        self.blocks[id].whole = true;
        (id, rows)
    }
    pub(crate) fn record_prompt(&mut self, text: &str) -> Vec<String> {
        let (id, content) = self.prompt(text);
        let mut rows = Vec::new();
        if self.logged.is_empty() || self.needs_separator() {
            self.push(String::new(), None);
            rows.push(String::new());
        }
        for row in &content {
            self.push(row.clone(), Some(id));
        }
        rows.extend(content);
        if !crate::background_commands::is_notification(text) {
            self.push(String::new(), None);
            rows.push(String::new());
        }
        rows
    }
    /// Open the actual prompt in scrollable history instead of a clipped overlay.
    pub(super) fn expand_prompt(&mut self, text: &str, width: usize) -> usize {
        let id = self
            .blocks
            .iter()
            .rposition(|block| block.prompt.as_deref() == Some(text))
            .unwrap_or_else(|| {
                self.record_prompt(text);
                self.blocks.len() - 1
            });
        self.expand_prompt_id(id, width)
    }
    pub(super) fn expand_prompt_id(&mut self, id: usize, width: usize) -> usize {
        let index = self
            .logged
            .iter()
            .position(|line| line.block == Some(id))
            .unwrap();
        if !self.blocks[id].body_open {
            self.toggle_history(index);
        }
        self.blocks[id].compact_level = 2;
        let rows = if compact() {
            self.compact_rows()
        } else {
            self.logged
                .iter()
                .map(|line| (line.text.clone(), line.block))
                .collect()
        };
        rows.iter()
            .take_while(|(_, block)| *block != Some(id))
            .map(|(text, _)| super::markdown::wrap_styled(text, "", "", width.max(1)).len())
            .sum()
    }
    pub(super) fn file(&mut self, review: super::file_review::Review) -> (usize, Vec<String>) {
        let summary = review.files[review.selected].summary.clone();
        let (id, lines) = self.block(vec![summary.clone()], vec![summary]);
        self.blocks[id].review = Some(review);
        (id, lines)
    }
    pub(super) fn review_at(&self, bottom: u16, row: u16) -> Option<super::file_review::Review> {
        let id = self.logged.get(self.line_at(bottom, row)?)?.block?;
        self.blocks[id].review.clone()
    }
    pub(super) fn prompt_at(&self, bottom: u16, row: u16) -> Option<usize> {
        let id = self.logged.get(self.line_at(bottom, row)?)?.block?;
        self.blocks[id].prompt.as_ref().map(|_| id)
    }
    pub(super) fn review_presentation(
        &self,
        row: usize,
        width: usize,
    ) -> Option<super::file_review::Review> {
        let rows = if compact() {
            self.compact_rows()
        } else {
            self.logged
                .iter()
                .map(|line| (line.text.clone(), line.block))
                .collect()
        };
        let mut offset = 0;
        for (text, id) in rows {
            let height = super::markdown::wrap_styled(&text, "", "", width.max(1)).len();
            if row < offset + height { return self.blocks.get(id?)?.review.clone(); }
            offset += height;
        }
        None
    }
    pub(crate) fn needs_separator(&self) -> bool {
        self.logged.back().is_some_and(|line| !super::markdown::plain_rendered(&line.text).trim().is_empty())
    }
    pub(crate) fn all(&self) -> Vec<String> {
        self.logged.iter().map(|line| line.text.clone()).collect()
    }
    /// A lossless presentation over logged blocks, with one target per physical row.
    fn compact_rows(&self) -> Vec<(String, Option<usize>)> {
        let mut result = Vec::new();
        let mut index = 0;
        while index < self.logged.len() {
            let Some(first) = self.logged[index].block else {
                result.push((self.logged[index].text.clone(), None));
                index += 1;
                continue;
            };
            if self.blocks[first].review.is_some() || self.blocks[first].prompt.is_some() {
                if result.last().is_some_and(|(text, id): &(String, Option<usize>)| *id != Some(first) && !text.trim().is_empty()) {
                    result.push((String::new(), None));
                }
                result.push((self.logged[index].text.clone(), Some(first)));
                index += 1;
                continue;
            }
            let mut ids = vec![first];
            index += 1;
            while index < self.logged.len() {
                if let Some(id) = self.logged[index].block {
                    if self.blocks[id].review.is_some() || self.blocks[id].prompt.is_some() {
                        break;
                    }
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                    index += 1;
                } else if self.logged[index].text.trim().is_empty()
                    && self.logged.get(index + 1).and_then(|line| line.block)
                        .is_some_and(|id| self.blocks[id].prompt.is_none() && self.blocks[id].review.is_none()) {
                    index += 1;
                } else { break; }
            }
            if result.last().is_some_and(|(text, _)| !text.trim().is_empty()) {
                result.push((String::new(), None));
            }
            if self.blocks[first].compact_level == 0 {
                let mut counts = [0usize; 6];
                for id in &ids {
                    let title = change_view::plain(&self.blocks[*id].short[..1]);
                    let kind = if title.contains("Bash(") || title.contains("PowerShell(") { 0 }
                        else if title.contains("Read(") { 1 }
                        else if title.contains("Grep(") { 2 }
                        else if title.contains("Glob(") { 3 }
                        else if title.contains("Updated ") || title.contains("Write(") || title.contains("Edit(") { 4 }
                        else { 5 };
                    counts[kind] += 1;
                }
                let forms = [("ran", "shell command", "shell commands"), ("read", "file", "files"),
                    ("searched for", "pattern", "patterns"), ("listed", "directory", "directories"),
                    ("updated", "file", "files"), ("used", "tool", "tools")];
                let mut summary = forms.iter().enumerate().filter(|(i, _)| counts[*i] > 0)
                    .map(|(i, (verb, one, many))| format!("{verb} {} {}", counts[i], if counts[i] == 1 { one } else { many }))
                    .collect::<Vec<_>>().join(", ");
                if let Some(first) = summary.get_mut(..1) { first.make_ascii_uppercase(); }
                result.push((format!("  {summary}  [click to expand]"), Some(first)));
            } else {
                for (position, id) in ids.into_iter().enumerate() {
                    if position > 0 { result.push((String::new(), None)); }
                    let block = &self.blocks[id];
                    let rows = if block.compact_level == 2 { rendered(&block.full) } else { collapsed(&block.short) };
                    result.extend(rows.into_iter().map(|row| (row, Some(id))));
                }
            }
        }
        result
    }
    pub(crate) fn presentation(&self) -> Vec<String> {
        if compact() {
            self.compact_rows()
                .into_iter()
                .map(|(text, _)| text)
                .collect()
        } else {
            self.all()
        }
    }
    pub(crate) fn click_presentation(&mut self, row: usize, width: usize) -> bool {
        let rows = self.presentation();
        let mut offset = 0;
        let Some(row) = rows.iter().position(|text| {
            offset += super::markdown::wrap_styled(text, "", "", width.max(1)).len();
            row < offset
        }) else {
            return false;
        };
        if !compact() {
            return self.toggle_history(row);
        }
        let Some(id) = self.compact_rows().get(row).and_then(|(_, id)| *id) else {
            return false;
        };
        if self.blocks[id].prompt.is_some() {
            let Some(index) = self.logged.iter().position(|line| line.block == Some(id)) else {
                return false;
            };
            return self.toggle_history(index);
        }
        if self.blocks[id].review.is_some() {
            return false;
        }
        if self.blocks[id].compact_level == 0 {
            let mut index = self
                .logged
                .iter()
                .position(|line| line.block == Some(id))
                .unwrap();
            while index < self.logged.len() {
                if let Some(member) = self.logged[index].block {
                    if self.blocks[member].review.is_some() || self.blocks[member].prompt.is_some()
                    {
                        break;
                    }
                    self.blocks[member].compact_level = 1;
                    index += 1;
                } else if self.logged[index].text.trim().is_empty()
                    && self.logged.get(index + 1).is_some_and(|line| line.block.is_some()) { index += 1; }
                else { break; }
            }
        } else {
            if self.blocks[id].compact_level == 1 { self.refresh_background(id); }
            self.blocks[id].compact_level = if self.blocks[id].compact_level == 1 { 2 } else { 1 };
        }
        true
    }
    pub(super) fn toggle_history(&mut self, row: usize) -> bool {
        // Each logged entry is a physical row in the normal view.
        let Some(line) = self.logged.get(row) else {
            return false;
        };
        if line.block.is_none() {
            return false;
        }
        let Some((_, rows)) = self.toggle_index(row, 0) else {
            return false;
        };
        for line in rows {
            self.push_logged(line);
        }
        true
    }
    /// The logged line on screen row `row`, given the row below the last one.
    fn line_at(&self, bottom: u16, row: u16) -> Option<usize> {
        let mut bottom = bottom;
        for (index, line) in self.logged.iter().enumerate().rev() {
            let first = bottom.checked_sub(line.rows as u16)?;
            if row >= first && row < bottom {
                return Some(index);
            }
            bottom = first;
        }
        None
    }
    pub(super) fn screen(&self, bottom: u16, height: u16) -> Vec<String> {
        self.screen_at(
            bottom,
            height,
            terminal::size().unwrap_or((80, 24)).0 as usize,
        )
    }
    fn screen_at(&self, bottom: u16, height: u16, width: usize) -> Vec<String> {
        let mut rows = vec![String::new(); height as usize];
        let mut end = bottom as usize;
        for line in self.logged.iter().rev() {
            let wrapped = super::markdown::wrap_styled(&line.text, "", "", width);
            for text in wrapped.iter().rev() {
                if end == 0 {
                    return rows;
                }
                end -= 1;
                if end < rows.len() {
                    rows[end] = text.clone();
                }
            }
        }
        if let Some(banner) = &self.welcome {
            let welcome: Vec<_> = banner.lines()
                .flat_map(|line| super::markdown::wrap_styled(line, "", "", width))
                .collect();
            for text in welcome.into_iter().rev().take(end) {
                end -= 1;
                if end < rows.len() { rows[end] = text; }
            }
        }
        rows
    }
    /// Repaint only the current viewport, leaving the terminal scrollback intact.
    pub(super) fn repaint(&self, bottom: u16, height: u16, width: u16) -> String {
        let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
        for (row, text) in self
            .screen_at(bottom, height, width as usize)
            .iter()
            .take(bottom as usize)
            .enumerate()
        {
            out.push_str(&format!("\x1b[{};1H{text}\x1b[0m\x1b[K", row + 1));
        }
        out.push_str(&format!("\x1b[{};1H", bottom + 1));
        out
    }
    /// Switches the tool result on `row` between its short and full form.
    /// Returns the screen row to redraw from and the lines to write there.
    pub(super) fn toggle(&mut self, bottom: u16, row: u16) -> Option<(u16, Vec<Logged>)> {
        let index = self.line_at(bottom, row)?;
        self.toggle_index(index, bottom)
    }
    fn toggle_index(&mut self, index: usize, bottom: u16) -> Option<(u16, Vec<Logged>)> {
        let block = self.logged[index].block?;
        let first = self
            .logged
            .iter()
            .position(|line| line.block == Some(block))?;
        let above: usize = self.logged.iter().skip(first).map(|line| line.rows).sum();
        let start = (bottom as usize).saturating_sub(above) as u16;
        if !self.blocks[block].body_open { self.refresh_background(block); }
        let entry = &mut self.blocks[block];
        if entry.review.is_some() { return None; }
        let header_rows = if entry.header_open {
            rendered(&entry.full[..1]).len()
        } else {
            collapsed(&entry.short[..1]).len()
        };
        if entry.whole {
            entry.header_open = !entry.header_open;
            entry.body_open = entry.header_open;
        } else if index - first < header_rows {
            entry.header_open = !entry.header_open;
        } else {
            entry.body_open = !entry.body_open;
        }
        let mut lines = if entry.header_open {
            rendered(&entry.full[..1])
        } else {
            collapsed(&entry.short[..1])
        };
        lines.extend(if entry.body_open {
            rendered(&entry.full[1..])
        } else {
            collapsed(&entry.short[1..])
        });
        let after = self
            .logged
            .drain(first..)
            .filter(|line| line.block != Some(block));
        let rows = lines
            .into_iter()
            .map(|text| Logged {
                text,
                rows: 1,
                block: Some(block),
                flow: None,
            })
            .chain(after)
            .collect();
        Some((start, rows))
    }
}

/// Rules belong to the prompt block so every transcript path keeps them.
fn prompt_lines(text: &str, width: usize) -> (Vec<Line>, Vec<Line>) {
    let clean = super::tool_view::plain_terminal_text(text);
    let rows = super::prompt_display::layout(
        &format!("{} ", crate::terminal::prompt_glyph()),
        &clean.chars().collect::<Vec<_>>(),
        width.max(4),
    )
    .0;
    let mut full: Vec<Line> = rows
        .iter()
        .map(|row| change_view::styled(row, change_view::Style::TerminalDefault))
        .collect();
    let mut short = if rows.len() > 3 {
        let mut short = full[..2].to_vec();
        short.push(change_view::muted(&format!(
            "  +{} lines (click to expand)",
            rows.len() - 2
        )));
        full.push(change_view::muted("  (click to collapse)"));
        short
    } else {
        full.clone()
    };
    let rule = change_view::styled(
        &"─".repeat(width.max(4)),
        change_view::Style::TerminalDefault,
    );
    for lines in [&mut short, &mut full] {
        lines.insert(0, rule.clone());
        lines.push(rule.clone());
    }
    (short, full)
}

/// A tool result's lines as written above the input box.
fn collapsed(lines: &[Line]) -> Vec<String> {
    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
    collapsed_at_width(lines, width)
}

fn collapsed_at_width(lines: &[Line], width: usize) -> Vec<String> {
    lines
        .iter()
        .flat_map(|line| {
            let color = std::env::var_os("NO_COLOR").is_none();
            if super::tool_view::is_mcp_header(line) {
                let text = change_view::render(line, usize::MAX, color);
                return super::tool_view::wrap_call(&text, width);
            }
            let plain = change_view::plain(std::slice::from_ref(line));
            vec![
                if unicode_width::UnicodeWidthStr::width(plain.trim_end()) > width {
                    if super::tool_view::is_shell_header(line) && width >= 4 {
                        return vec![format!(
                            "{}...)",
                            change_view::render(line, width - 4, color)
                        )];
                    }
                    format!(
                        "{}…",
                        change_view::render(line, width.saturating_sub(1), color)
                    )
                } else {
                    change_view::render(line, width, color)
                },
            ]
        })
        .collect()
}

fn rendered(lines: &[Line]) -> Vec<String> {
    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
    rendered_at_width(lines, width)
}

fn rendered_at_width(lines: &[Line], width: usize) -> Vec<String> {
    let color = std::env::var_os("NO_COLOR").is_none();
    lines
        .iter()
        .flat_map(|line| {
            let text = change_view::render(line, usize::MAX, color);
            if super::tool_view::is_mcp_header(line) {
                super::tool_view::wrap_call(&text, width)
            } else {
                super::markdown::wrap_styled(&text, "", "", width)
            }
        })
        .collect()
}

/// Screen rows a written line takes at `columns`, style escapes excluded.
fn screen_rows(text: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    text.split("\r\n")
        .map(|line| {
            let mut cells = 0;
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                if ch == '\x1b' {
                    // CSI ends at its final byte, OSC at BEL or ST.
                    match chars.next() {
                        Some('[') => {
                            for ch in chars.by_ref() {
                                if ('@'..='~').contains(&ch) {
                                    break;
                                }
                            }
                        }
                        Some(']') => {
                            for ch in chars.by_ref() {
                                if ch == '\x07' || ch == '\\' {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    cells += ch.width().unwrap_or(0);
                }
            }
            cells.div_ceil(columns).max(1)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    #[test]
    fn latest_user_prompt_belongs_to_the_transcript_and_ignores_commands_and_notifications() {
        let mut history = super::Scrollback::default();
        history.record_prompt("FIRST");
        history.record_prompt("LATEST\n中文");
        history.record_prompt("/status");
        history.record_prompt("<task-notification><summary>Finished</summary></task-notification>");
        assert_eq!(history.latest_prompt(), "LATEST\n中文");
        assert!(super::Scrollback::default().latest_prompt().is_empty());
        assert!(!history.has_welcome());
        history.push_flow("A restored text-only answer", "● ", "  ", 80);
        assert!(!history.has_welcome());
        history.expand_prompt("FIRST", 40);
        assert_eq!(history.latest_prompt(), "LATEST\n中文");
    }

    #[test]
    fn welcome_survives_local_output_and_resize_until_it_scrolls_out() {
        let mut history = super::Scrollback::default();
        history.set_welcome("WELCOME\nMODEL\nWORKSPACE\n\n".into());
        history.push_flow("Plan mode enabled", "", "", 79);
        history.push_flow("Connection failed", "", "", 79);
        history.update_welcome(6);
        assert!(history.has_welcome());
        assert_eq!(
            history.screen_at(6, 24, 80)[..6],
            [
                "WELCOME",
                "MODEL",
                "WORKSPACE",
                "",
                "Plan mode enabled",
                "Connection failed"
            ]
        );
        history.reflow(40);
        assert_eq!(history.repaint_bottom(20, 40), 6);
        history.update_welcome(2);
        assert!(!history.has_welcome());
        history.reflow(80);
        assert_eq!(history.repaint_bottom(30, 80), 30);
    }

    #[test]
    fn task_notifications_render_summaries_without_user_prompt_or_xml() {
        let mut history = Scrollback::default();
        let text = "<task-notification>\n<summary>Agent one completed</summary>\n</task-notification>\n\n<task-notification>\n<summary>Agent two failed</summary>\n</task-notification>";
        let rows = history.record_prompt(text).join("\n");
        assert!(rows.contains("● Agent one completed"));
        assert!(rows.contains("● Agent two failed"));
        assert!(!rows.contains("task-notification"));
        assert!(!rows.contains(crate::terminal::prompt_glyph()));
        let compact = history
            .compact_rows()
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(compact.contains("Agent one completed") && compact.contains("Agent two failed"));
        assert!(!compact.contains("used 1 tool"));
    }

    #[test]
    fn copy_targets_keep_complete_commands_and_bodies_separate() {
        let mut history = Scrollback::default();
        let command = "echo  spaced\nsecond command";
        let body = "first\n\n中文 last";
        let (id, rows) = history.block(
            vec![
                change_view::tool_call("Bash(echo...)", "", false),
                change_view::tool_output("first", true, false),
            ],
            vec![],
        );
        history.bind_copy(id, command.into(), body.into());
        for row in rows {
            history.push(row, Some(id));
        }
        assert_eq!(history.copy_logged(0).as_deref(), Some(command));
        assert_eq!(history.copy_logged(1).as_deref(), Some(body));
        assert_eq!(history.copy_presentation(0, 100).as_deref(), Some(command));
    }

    #[test]
    fn logical_answers_reflow_after_resize_and_tool_disclosure_without_losing_text() {
        let mut history = super::Scrollback::default();
        let short = vec![super::change_view::body("Read(file)")];
        let full = vec![short[0].clone(), super::change_view::body("details")];
        let (id, rows) = history.block(short, full);
        for row in rows {
            history.push(row, Some(id));
        }
        let text = "Auto 是用模型处理原本默认需要你确认的操作。";
        history.push_flow(text, "● ", "  ", 6);
        assert!(history.all().len() > 3);
        history.toggle_history(0);
        history.reflow(100);
        assert!(history.all().iter().any(|row| row == &format!("● {text}")));
        history.reflow(8);
        history.reflow(100);
        assert_eq!(
            history
                .all()
                .iter()
                .filter(|row| row.contains(text))
                .count(),
            1
        );
        let mut terminal = vt100::Parser::new(30, 100, 0);
        terminal.process(history.repaint(24, 30, 100).as_bytes());
        assert!(terminal.screen().contents().contains(text));
    }

    #[test]
    fn compact_keeps_blank_lines_between_tool_and_prompt_blocks() {
        let mut history = super::Scrollback::default();
        for label in ["Read(a)", "Grep(b)"] {
            let short = vec![super::change_view::body(label)];
            let (id, rows) = history.block(short.clone(), short);
            for row in rows { history.push(row, Some(id)); }
            history.push(String::new(), None);
        }
        history.record_prompt("NEXT INPUT");
        let short = vec![super::change_view::body("Bash(c)")];
        let (id, rows) = history.block(short.clone(), short);
        for row in rows { history.push(row, Some(id)); }
        for level in [0, 1, 2] {
            for block in &mut history.blocks { block.compact_level = level; }
            let rows = history.compact_rows();
            let input = rows
                .iter()
                .position(|(text, _)| text.contains("NEXT INPUT"))
                .unwrap();
            assert!(
                super::super::markdown::plain_rendered(&rows[input - 1].0)
                    .chars()
                    .all(|ch| ch == '─'),
                "level {level}"
            );
            assert!(
                super::super::markdown::plain_rendered(&rows[input + 1].0)
                    .chars()
                    .all(|ch| ch == '─'),
                "level {level}"
            );
            assert!(rows[input - 2].0.is_empty(), "level {level}");
            assert!(rows[input + 2].0.is_empty(), "level {level}");
            assert!(rows[input - 2].1.is_none());
        }
    }

    #[test]
    fn submitted_prompt_rules_survive_disclosure_and_resize_with_single_outer_spacing() {
        let mut history = Scrollback::default();
        history.push("Cooked for 45s".into(), None);
        history.record_prompt("推送阶段\n  im 服务 → ws 节点\n第三行\n最后一行");
        history.push("Thought".into(), None);
        let assert_rules = |rows: Vec<String>| {
            let plain: Vec<_> = rows
                .iter()
                .map(|row| super::super::markdown::plain_rendered(row))
                .collect();
            let rules: Vec<_> = plain
                .iter()
                .enumerate()
                .filter(|(_, row)| !row.is_empty() && row.chars().all(|ch| ch == '─'))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(rules.len(), 2);
            assert_eq!(plain[rules[0] - 1], "");
            assert_eq!(plain[rules[0] - 2], "Cooked for 45s");
            assert_eq!(plain[rules[1] + 1], "");
            assert_eq!(plain[rules[1] + 2], "Thought");
        };
        assert_rules(history.all());
        history.expand_prompt_id(0, 80);
        assert!(history.all().iter().any(|row| row.contains("最后一行")));
        for width in [24, 160] {
            history.reflow(width);
            assert_rules(history.all());
            for row in history.all() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(
                        super::super::markdown::plain_rendered(&row).as_str()
                    ) < width
                );
            }
        }
    }

    #[test]
    fn shell_headers_use_available_width_and_keep_full_commands_for_wrapped_disclosure() {
        use super::super::tool_view;
        let command = format!(
            "cat {}bootstrap.yml | sed -n '1,160p'",
            "gateway-server/".repeat(15)
        );
        for name in ["Bash", "PowerShell"] {
            let input = serde_json::json!({"command": command});
            let short = tool_view::numbered(
                tool_view::finished(name, &input, "done", false, None, None),
                Some(393),
            );
            let full = tool_view::numbered(
                tool_view::expanded(name, &input, "done", false, None),
                Some(393),
            );
            assert!(change_view::plain(&short).contains(&command));
            for width in [40, 180] {
                let rows = collapsed_at_width(&short[..1], width);
                assert_eq!(rows.len(), 1);
                let line = super::super::markdown::plain_rendered(&rows[0]);
                assert!(line.ends_with("...)"));
                assert_eq!(unicode_width::UnicodeWidthStr::width(line.as_str()), width);
                let rows = rendered_at_width(&full[..1], width);
                assert!(rows.len() > 1);
                assert!(rows
                    .iter()
                    .map(|line| super::super::markdown::plain_rendered(line))
                    .collect::<String>()
                    .contains("bootstrap.yml"));
            }
            let mut history = Scrollback::default();
            let (id, rows) = history.block(short, full);
            for row in rows {
                history.push(row, Some(id));
            }
            history.bind_copy(id, command.clone(), "done".into());
            assert!(history.toggle_history(0));
            assert!(history
                .all()
                .iter()
                .map(|row| super::super::markdown::plain_rendered(row))
                .collect::<String>()
                .contains("bootstrap.yml"));
            assert_eq!(history.copy_logged(0).as_deref(), Some(command.as_str()));
            assert!(history.toggle_history(0));
            history.reflow(180);
            assert_eq!(history.all().len(), 2);
            assert!(super::super::markdown::plain_rendered(&history.all()[0]).ends_with("...)"));
        }
    }
    #[test]
    fn background_click_refreshes_owned_logs_and_survives_prompt_handoff() {
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(root.path().join("tasks")).unwrap();
        let db = deepseek_code_core::db::DbManager::new(root.path().join("tasks").join(format!("{session}.sqlite3"))).unwrap();
        let record = json!({"id":task,"status":"completed","metadata":{"managed_shell":true},"output":{"exit_code":0}});
        db.with_conn(|conn| {
            conn.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY, body TEXT NOT NULL)").unwrap();
            conn.execute("INSERT INTO tasks VALUES (?1,?2)", [&task, &record.to_string()]).unwrap();
        }).unwrap();
        let logs = root.path().join("shell-tasks").join(&session).join(&task);
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("stdout.log"), "FIRST\n").unwrap();
        std::fs::write(logs.join("stderr.log"), "\x1b[31mERROR\x1b[0m\n").unwrap();
        let mut history = super::Scrollback::default();
        let short = vec![super::change_view::body("Bash(fixture)"), super::change_view::body("Running in background (click to show output)")];
        let (id, rows) = history.block(short.clone(), short);
        for row in rows { history.push(row, Some(id)); }
        history.bind_background(root.path().to_owned(), session, task);
        history.push("AFTER".into(), None);
        assert!(history.toggle_history(1));
        assert!(history.all().iter().any(|line| line.contains("FIRST")));
        assert!(history.all().iter().any(|line| line.contains("ERROR")));
        assert_eq!(history.all().last().unwrap(), "AFTER");
        assert!(history.toggle_history(1));
        assert!(!history.all().iter().any(|line| line.contains("FIRST")));
        std::fs::write(logs.join("stdout.log"), "FIRST\nLATEST\n").unwrap();
        // The same retained blocks are used by the live turn and idle prompt.
        let mut restored = history.clone();
        assert!(restored.toggle_history(1));
        assert!(restored.all().iter().any(|line| line.contains("LATEST")));
        restored.toggle_history(1);
        restored.blocks[id].background.as_mut().unwrap().1 = uuid::Uuid::new_v4().to_string();
        restored.toggle_history(1);
        assert!(restored.all().iter().any(|line| line.contains("Output unavailable")));
        assert!(!restored.all().iter().any(|line| line.contains("LATEST")));
        assert!(!logs.join("cancel").exists());
    }
    #[test]
    fn identical_prompts_keep_distinct_targets_and_literal_json() {
        let mut history = super::Scrollback::default();
        let text = "{\"stdout\":\"literal prompt\"}\nsecond\nthird\nlast";
        history.record_prompt(text);
        history.push("BETWEEN".into(), None);
        history.record_prompt(text);
        history.expand_prompt_id(0, 80);
        assert!(history.blocks[0].body_open);
        assert!(!history.blocks[1].body_open);
        assert!(history.all()[2].contains("stdout"));
    }
    #[test]
    fn user_text_retains_terminal_default_colors_after_history_reflow() {
        let mut history = super::Scrollback::default();
        history.record_prompt("EINK_USER\nSECOND\nTHIRD\nLAST");
        assert!(history.blocks[0].full.iter().take(4).all(|line|
            line.0.iter().all(|span| matches!(span.style, change_view::Style::TerminalDefault))));
        history.reflow(18);
        history.expand_prompt_id(0, 18);
        assert!(history.all().iter().any(|row| row.contains("LAST")));
        assert!(history.blocks[0].full.iter().take(4).all(|line|
            line.0.iter().all(|span| matches!(span.style, change_view::Style::TerminalDefault))));
    }

    #[test]
    fn prompt_history_survives_reflow_and_expands_without_losing_the_tail() {
        let mut history = super::Scrollback::default();
        let text = (0..120)
            .map(|i| format!("PROMPT_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        history.record_prompt(&text);
        history.push("AFTER_PROMPT".into(), None);
        assert!(!history.all().iter().any(|row| row.contains("PROMPT_119")));
        history.reflow(18);
        assert!(history.click_presentation(1, 18));
        assert!(history.all().iter().any(|row| row.contains("PROMPT_119")));
        assert_eq!(history.all().last().unwrap(), "AFTER_PROMPT");
        assert!(history.click_presentation(1, 18));
        assert!(!history.all().iter().any(|row| row.contains("PROMPT_119")));
        history.expand_prompt(&text, 18);
        assert!(history.all().iter().any(|row| row.contains("PROMPT_119")));
    }

    #[test]
    fn wrapped_history_click_targets_the_correct_block() {
        let mut history = super::Scrollback::default();
        history.push("01234567890123456789".into(), None);
        history.record_prompt("ONE\nTWO\nTHREE\nFOUR");
        assert!(history.click_presentation(3, 10));
        assert!(history.all().iter().any(|row| row.contains("FOUR")));
    }
    use super::*;

    #[test]
    fn file_review_targets_survive_expansion_and_remain_separate_in_compact_view() {
        let mut history = Scrollback::default();
        let (id, rows) = history.block(vec![change_view::body("tool")], vec![change_view::body("tool"), change_view::body("extra")]);
        for row in rows { history.push(row, Some(id)); }
        let files = std::sync::Arc::new(vec![super::super::file_review::File {
            label: "test.rs".into(), summary: change_view::body("test.rs +1"), details: vec![change_view::body("+new")],
        }]);
        let (id, rows) = history.file(super::super::file_review::Review { files, selected: 0 });
        for row in rows { history.push(row, Some(id)); }
        assert_eq!(history.review_at(2, 1).unwrap().files[0].label, "test.rs");
        assert!(history.review_at(2, 0).is_none());
        assert!(history.toggle_history(0));
        assert_eq!(history.review_at(3, 2).unwrap().files[0].label, "test.rs");
        assert!(history.compact_rows().last().unwrap().0.contains("test.rs +1"));
        assert!(!history.toggle_history(2));
    }

    #[test]
    fn screen_rows_count_wrapped_rows_without_style_escapes() {
        assert_eq!(screen_rows("", 10), 1);
        assert_eq!(screen_rows("\x1b[38;2;1;2;3mabcdefghij\x1b[0m", 10), 1);
        assert_eq!(screen_rows("abcdefghijk", 10), 2);
        assert_eq!(screen_rows("ab\r\ncd", 10), 2);
        assert_eq!(screen_rows("\u{4f60}\u{597d}\u{4f60}", 4), 2);
    }

    #[test]
    fn thought_header_content_and_footer_toggle_the_entire_block() {
        let mut history = Scrollback::default();
        let (short, full) = super::super::tool_view::thought("one\ntwo\nthree\nfour\nfive", Some(12));
        let (id, rows) = history.block(short, full);
        for row in rows { history.push(row, Some(id)); }
        assert!(history.all()[0].contains("Thought for 12s 5 lines (click to expand)"));
        assert!(history.toggle_history(0));
        assert_eq!(history.all().len(), 7);
        assert!(history.all()[6].contains("(click to collapse)"));
        assert!(history.toggle_history(2));
        assert_eq!(history.all().len(), 1);
        assert!(history.toggle_history(0));
        assert!(history.toggle_history(6));
        assert_eq!(history.all().len(), 1);
    }

    #[test]
    fn compact_groups_preserve_commentary_and_have_three_levels() {
        let mut history = Scrollback::default();
        for n in 0..2 {
            let short = vec![change_view::body(&format!("● Bash(command {n})"))];
            let full = vec![short[0].clone(), change_view::body("secret output")];
            let (id, rows) = history.block(short, full);
            for row in rows { history.push(row, Some(id)); }
            history.push(String::new(), None);
        }
        history.push("Assistant explanation".into(), None);
        let (id, rows) = history.block(vec![change_view::body("● Read(file)")], vec![change_view::body("file contents")]);
        for row in rows { history.push(row, Some(id)); }
        let rows = history.compact_rows();
        assert!(rows[0].0.contains("Ran 2 shell commands"));
        assert!(rows.iter().any(|(row, _)| row == "Assistant explanation"));
        assert!(rows.last().unwrap().0.contains("Read 1 file"));
        history.blocks[0].compact_level = 1;
        let rows = history.compact_rows();
        assert!(rows[0].0.contains("command 0"));
        assert!(rows[2].0.contains("command 1"));
        assert!(!rows.iter().any(|(row, _)| row.contains("secret output")));
        history.blocks[0].compact_level = 2;
        assert!(history.compact_rows().iter().any(|(row, _)| row.contains("secret output")));
        history.blocks[0].compact_level = 0;
        assert!(history.compact_rows()[0].0.contains("Ran 2 shell commands"));
        assert_eq!(history.all().len(), 6);
    }

    #[test]
    fn file_change_header_expands_all_diff_rows_and_body_collapses() {
        let mut history = Scrollback::default();
        let mut full = vec![change_view::success("Updated test.rs (+12 -0)")];
        full.extend((0..12).map(|i| change_view::body(&format!("line {i}"))));
        let (id, rows) = history.block(full[..5].to_vec(), full);
        for row in rows { history.push(row, Some(id)); }
        assert_eq!(history.all().len(), 5);
        assert!(history.toggle_history(0));
        assert_eq!(history.all().len(), 13);
        assert!(history.toggle_history(3));
        assert_eq!(history.all().len(), 5);
    }

    #[test]
    fn header_and_body_toggle_independently_and_collapse_again() {
        let mut lines = Scrollback::default();
        let short = vec![
            change_view::body("short header"),
            change_view::body("short body"),
        ];
        let full = vec![
            change_view::body("full header"),
            change_view::body("full body 1"),
            change_view::body("full body 2"),
        ];
        let (block, rows) = lines.block(short, full);
        for row in rows {
            lines.push(row, Some(block));
        }
        assert!(lines.toggle_history(1));
        assert!(lines.all()[0].contains("short header"));
        assert_eq!(lines.all().len(), 3);
        assert!(lines.toggle_history(0));
        assert!(lines.all()[0].contains("full header"));
        assert!(lines.toggle_history(1));
        assert_eq!(lines.all().len(), 2);
        assert!(lines.all()[0].contains("full header"));
        assert!(lines.toggle_history(0));
        assert!(lines.all()[0].contains("short header"));
    }
}

#[cfg(test)]
mod mcp_wrapping_tests {
    use super::*;

    #[test]
    fn collapsed_numbered_mcp_headers_keep_all_parameters() {
        let name = "mcp__codegraph__codegraph_explore";
        let input = serde_json::json!({"query": "中文🦀".repeat(150), "maxFiles": 12, "z": "secondary-tail"});
        let lines = super::super::tool_view::numbered(
            super::super::tool_view::finished(name, &input, "ok", false, None, None),
            Some(4),
        );
        let rows = collapsed(&lines);
        let text = super::super::tool_view::plain_terminal_text(&rows.concat());
        assert!(rows.len() > 3);
        assert!(text.contains(&format!("{name}({input})")));
        assert!(text.contains("#4"));
    }
}
