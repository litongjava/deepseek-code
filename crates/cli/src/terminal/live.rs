//! One terminal owner while a model turn runs. The idle editor is inactive.
use super::change_view;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute, terminal,
};
use serde_json::Value;
use std::{
    io::{self, IsTerminal, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc as async_mpsc, oneshot};
use unicode_width::UnicodeWidthChar;

pub enum Action {
    GoalDecision { id: String, approved: bool },
    CyclePermission,
    Submit(String),
    SideQuestion(String),
    Cancel,
    Detach,
    Draft(String),
}
enum Update {
    GoalProposal(Option<(String, String)>),
    PermissionMode(String),
    Text(String),
    Notice(String),
    Styled(Vec<change_view::Line>),
    Activity(String),
    HookActivity(Option<String>),
    Usage(Value),
    Question(String, oneshot::Sender<String>),
    Choice(Vec<change_view::Line>, String, Vec<String>, oneshot::Sender<String>),
    Detail(String, String, String),
    Thinking(String),
    Stop(Option<String>),
}
pub struct LiveTurn {
    updates: mpsc::Sender<Update>,
    pub actions: async_mpsc::UnboundedReceiver<Action>,
    worker: Option<thread::JoinHandle<()>>,
}
impl LiveTurn {
    pub fn goal_proposal(&self, proposal: Option<(String, String)>) {
        let _ = self.updates.send(Update::GoalProposal(proposal));
    }
    pub fn start(
        mode: &str,
        bindings: crate::keybindings::Keybindings,
        history: Vec<String>,
        tip: Option<String>,
    ) -> Option<Self> {
        if crate::terminal::screen_reader()
            || !io::stdin().is_terminal()
            || !io::stdout().is_terminal()
            || !io::stderr().is_terminal()
            || std::env::var("TERM").as_deref() == Ok("dumb")
        {
            return None;
        }
        let (updates, receiver) = mpsc::channel();
        let (actions, action_receiver) = async_mpsc::unbounded_channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let mode = mode.to_owned();
        let worker = thread::spawn(move || {
            let result = terminal::enable_raw_mode()
                .and_then(|_| execute!(io::stderr(), EnableBracketedPaste));
            if result.is_err() {
                let _ = terminal::disable_raw_mode();
                let _ = ready.send(false);
                return;
            }
            let _guard = RawGuard;
            let _ = ready.send(true);
            let mut state = State::new(mode);
            state.bindings = bindings;
            state.history = history;
            state.history_index = state.history.len();
            state.tip = tip;
            let mut screen = Frame::default();
            let _ = run(&mut state, &mut screen, receiver, actions);
            let _ = screen.clear();
        });
        if initialized.recv().ok() != Some(true) {
            let _ = worker.join();
            return None;
        }
        Some(Self {
            updates,
            actions: action_receiver,
            worker: Some(worker),
        })
    }
    pub fn detail(&self, id: &str, title: &str, body: &str) {
        let _ = self.updates.send(Update::Detail(
            id.into(),
            title.into(),
            body.chars().take(65536).collect(),
        ));
    }
    pub fn thinking(&self, text: &str) {
        let _ = self.updates.send(Update::Thinking(text.into()));
    }
    pub fn text(&self, text: &str) {
        let _ = self.updates.send(Update::Text(text.into()));
    }
    pub fn notice(&self, text: &str) {
        let _ = self.updates.send(Update::Notice(text.into()));
    }
    fn styled(&self, lines: Vec<change_view::Line>) {
        let _ = self.updates.send(Update::Styled(lines));
    }
    pub fn permission_mode(&self, mode: &str) {
        let _ = self.updates.send(Update::PermissionMode(mode.into()));
    }
    pub fn activity(&self, text: &str) {
        let _ = self.updates.send(Update::Activity(text.into()));
    }
    pub fn usage(&self, value: &Value) {
        let _ = self.updates.send(Update::Usage(value.clone()));
    }
    pub fn question(&self, text: String) -> oneshot::Receiver<String> {
        let (send, receive) = oneshot::channel();
        let _ = self.updates.send(Update::Question(text, send));
        receive
    }
    /// A numbered menu below a preview; the reply is the chosen number.
    pub fn choice(&self, lines: Vec<change_view::Line>, question: String, options: Vec<String>) -> oneshot::Receiver<String> {
        let (send, receive) = oneshot::channel();
        let _ = self.updates.send(Update::Choice(lines, question, options, send));
        receive
    }
    pub fn finish(mut self, error: Option<&str>) -> Vec<Action> {
        let _ = self.updates.send(Update::Stop(error.map(str::to_owned)));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let mut pending = Vec::new();
        while let Ok(action) = self.actions.try_recv() {
            pending.push(action);
        }
        pending
    }
}
impl Drop for LiveTurn {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.updates.send(Update::Stop(Some("Interrupted".into())));
            let _ = worker.join();
        }
    }
}
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), DisableBracketedPaste, DisableMouseCapture);
        let _ = terminal::disable_raw_mode();
    }
}
#[derive(Default, Clone)]
struct Draft {
    characters: Vec<char>,
    cursor: usize,
}
impl Draft {
    fn insert_paste(&mut self, text: &str) -> bool {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let count = normalized.chars().filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t')).count();
        if self.characters.len().saturating_add(count) > 32_768 { return false; }
        self.insert(&normalized);
        true
    }
    fn insert(&mut self, text: &str) {
        for ch in text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        {
            if self.characters.len() >= 32_768 {
                break;
            }
            self.characters.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }
    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.characters).into_iter().collect()
    }
    fn set(&mut self, text: &str) {
        self.characters.clear();
        self.cursor = 0;
        self.insert(text);
    }
    fn visible(&self, width: usize) -> (String, usize) {
        let mut start = 0;
        while self.characters[start..self.cursor]
            .iter()
            .map(|ch| ch.width().unwrap_or(1).max(1))
            .sum::<usize>()
            >= width.max(1)
        {
            start += 1;
        }
        let cursor = self.characters[start..self.cursor]
            .iter()
            .map(|ch| ch.width().unwrap_or(1).max(1))
            .sum();
        let text = self.characters[start..]
            .iter()
            .map(|ch| {
                if *ch == '\n' {
                    '↵'
                } else if *ch == '\t' {
                    ' '
                } else {
                    *ch
                }
            })
            .collect::<String>();
        (clip(&text, width), cursor)
    }
}
struct Choice {
    question: String,
    options: Vec<String>,
    selected: usize,
}

struct State {
    goal_proposal: Option<(String, String)>,
    mode: String,
    activity: String,
    hook_activity: Option<String>,
    draft: Draft,
    saved_draft: Option<Draft>,
    answer: Option<oneshot::Sender<String>>,
    /// A pending menu question; its reply goes through `answer`.
    choice: Option<Choice>,
    partial: String,
    usage: super::progress::ReportedUsage,
    generated_characters: usize,
    estimate_pending: bool,
    started: Instant,
    activity_started: Instant,
    bindings: crate::keybindings::Keybindings,
    key_pending: crate::keybindings::Pending,
    details: super::input::editor::DetailsPanel,
    detail_entries: Vec<(String, String, String)>,
    thinking_characters: usize,
    thinking_entry: Option<usize>,
    history: Vec<String>,
    history_index: usize,
    history_draft: String,
    search_query: Option<String>,
    tip: Option<String>,
    markdown: super::markdown::MarkdownStream,
    /// The next text line starts an answer block and gets the `●` marker.
    block_start: bool,
    /// Shown while the model works, as in "Brewing… (12s)".
    verb: &'static str,
    frame: usize,
}

/// Spinner glyphs, played forward and back.
const SPINNER: &[&str] = if cfg!(target_os = "macos") {
    &["\u{b7}", "\u{2722}", "\u{2733}", "\u{2736}", "\u{273b}", "\u{273d}"]
} else {
    &["\u{b7}", "\u{2722}", "*", "\u{2736}", "\u{273b}", "\u{273d}"]
};

const VERBS: &[&str] = &[
    "Brewing", "Pondering", "Composing", "Crafting", "Mulling", "Tinkering", "Cogitating", "Assembling",
    "Percolating", "Untangling", "Sketching", "Weaving", "Noodling", "Puzzling", "Forging", "Polishing",
    "Simmering", "Calibrating", "Distilling", "Orchestrating", "Spelunking", "Whittling", "Kneading",
    "Marinating", "Scheming", "Synthesizing", "Wrangling", "Conjuring", "Deliberating", "Fermenting",
];

/// Shown in the closing line, as in "✻ Churned for 7m 7s".
const PAST_VERBS: &[&str] = &[
    "Baked", "Brewed", "Churned", "Cogitated", "Cooked", "Crunched", "Mulled", "Pondered",
    "Sautéed", "Simmered", "Worked", "Crafted", "Whittled", "Kneaded", "Distilled",
];

fn spinner_glyph(frame: usize) -> &'static str {
    let cycle = SPINNER.len() * 2;
    let position = frame % cycle;
    SPINNER[if position < SPINNER.len() { position } else { cycle - 1 - position }]
}

/// `12s`, `4m 13s` or `1h 2m`.
fn short_duration(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// `950` or `13.7k`.
fn short_count(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{:.1}k", count as f64 / 1000.0)
    }
}

impl State {
    fn new(mode: String) -> Self {
        Self {
            goal_proposal: None,
            mode,
            activity: "Waiting for response".into(),
            hook_activity: None,
            draft: Draft::default(),
            saved_draft: None,
            answer: None,
            choice: None,
            partial: String::new(),
            usage: Default::default(),
            generated_characters: 0,
            estimate_pending: false,
            started: Instant::now(),
            activity_started: Instant::now(),
            bindings: Default::default(),
            key_pending: Default::default(),
            details: Default::default(),
            detail_entries: Vec::new(),
            thinking_characters: 0,
            thinking_entry: None,
            history: Vec::new(),
            history_index: 0,
            history_draft: String::new(),
            search_query: None,
            tip: None,
            markdown: super::markdown::MarkdownStream::new(std::env::var_os("NO_COLOR").is_none()),
            block_start: true,
            verb: VERBS[std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.subsec_nanos() as usize)
                % VERBS.len()],
            frame: 0,
        }
    }
    /// The activity line: `✢ Brewing… (4m 13s · ↓ 13.7k tokens · thinking)`.
    fn status_line(&self) -> String {
        let label = if self.answer.is_some() {
            "Waiting for your answer".to_owned()
        } else if let Some(hook) = &self.hook_activity {
            hook.clone()
        } else if self.activity.starts_with("Running") || self.activity == "Cancelling" {
            self.activity.clone()
        } else {
            self.verb.to_owned()
        };
        let mut details = vec![short_duration(self.started.elapsed().as_secs())];
        let tokens = (self.generated_characters as u64).div_ceil(4).max(self.usage.output().unwrap_or(0));
        if tokens > 0 {
            details.push(format!("\u{2193} {} tokens", short_count(tokens)));
        }
        if self.activity.contains("thinking") {
            details.push("thinking".into());
        }
        format!("{} {label}\u{2026} ({})", spinner_glyph(self.frame), details.join(" \u{b7} "))
    }
    fn detail(&mut self, id: String, title: String, body: String) {
        if let Some(entry) = self.detail_entries.iter_mut().find(|entry| entry.0 == id) {
            *entry = (id, title, body);
        } else {
            self.detail_entries.push((id, title, body));
        }
        self.details.update_entries(
            self.detail_entries
                .iter()
                .map(|(_, title, body)| (title.clone(), body.clone()))
                .collect(),
        );
    }
    fn append_thinking(&mut self, text: &str) {
        let characters = text.chars().count();
        self.generated_characters = self.generated_characters.saturating_add(characters);
        self.estimate_pending = true;
        let index = match self.thinking_entry {
            Some(index) => index,
            None => {
                let index = self
                    .detail_entries
                    .iter()
                    .position(|entry| entry.0 == "thinking")
                    .unwrap_or(self.detail_entries.len());
                self.detail("thinking".into(), "Thinking".into(), String::new());
                self.thinking_entry = Some(index);
                index
            }
        };
        let remaining = 65536usize.saturating_sub(self.thinking_characters);
        let end = text
            .char_indices()
            .nth(remaining)
            .map_or(text.len(), |(byte, _)| byte);
        let appended = &text[..end];
        self.thinking_characters += characters.min(remaining);
        self.detail_entries[index].2.push_str(appended);
        self.details.append_body(index, appended);
    }

    fn cycle_permission(&self, actions: &async_mpsc::UnboundedSender<Action>) {
        // This is a mode-control event, never a queued prompt or an answer.
        let _ = actions.send(Action::CyclePermission);
    }
    fn decide_goal(&mut self, approved: bool, actions: &async_mpsc::UnboundedSender<Action>) {
        if let Some((id, _)) = self.goal_proposal.take() {
            let _ = actions.send(Action::GoalDecision { id, approved });
        }
    }

    fn question(&mut self, reply: oneshot::Sender<String>) {
        self.dismiss_closed_question();
        self.saved_draft = Some(std::mem::take(&mut self.draft));
        self.answer = Some(reply);
    }
    fn dismiss_closed_question(&mut self) -> bool {
        if self.answer.as_ref().is_some_and(oneshot::Sender::is_closed) {
            self.answer = None;
            self.choice = None;
            self.draft = self.saved_draft.take().unwrap_or_default();
            return true;
        }
        false
    }
    /// Menu keys: arrows move, a number or Enter answers. Esc and Ctrl+C
    /// fall through and interrupt the turn as everywhere else.
    fn choice_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let Some(choice) = self.choice.as_mut() else {
            return false;
        };
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = choice.options.len();
        match key.code {
            KeyCode::Esc => return false,
            KeyCode::Char('c' | 'o' | 'b' | 'l') if control => return false,
            KeyCode::Up | KeyCode::Char('k') => choice.selected = (choice.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => choice.selected = (choice.selected + 1) % count,
            KeyCode::Char(ch @ '1'..='9') if !control => {
                let index = ch as usize - '1' as usize;
                if index < count {
                    choice.selected = index;
                    self.answer_choice();
                }
            }
            KeyCode::Enter => self.answer_choice(),
            _ => {}
        }
        true
    }
    fn answer_choice(&mut self) {
        let Some(choice) = self.choice.take() else {
            return;
        };
        if let Some(answer) = self.answer.take() {
            let _ = answer.send((choice.selected + 1).to_string());
        }
        self.draft = self.saved_draft.take().unwrap_or_default();
    }
    fn submit(&mut self, actions: &async_mpsc::UnboundedSender<Action>) -> Option<String> {
        let text = self.draft.take();
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(text);
            self.draft = self.saved_draft.take().unwrap_or_default();
            return None;
        }
        if text.trim().is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.history_index = self.history.len();
        self.history_draft = String::new();
        if let Some(question) = text.trim().strip_prefix("/btw ") {
            if question.trim().is_empty() {
                return Some("Usage: /btw QUESTION".into());
            }
            let _ = actions.send(Action::SideQuestion(question.trim().into()));
            return Some("Side question submitted; main task continues.".into());
        }
        if text.trim() == "/background" {
            let _ = actions.send(Action::Detach);
            return Some("Detaching from the active worker; its request continues.".into());
        }
        let (command, argument) = text.trim().split_once(char::is_whitespace).unwrap_or((text.trim(), ""));
        if command == "/goal" && (argument.trim().is_empty() || crate::session_commands::is_goal_clear(argument)) {
            let _ = actions.send(Action::Submit(text));
            return None;
        }
        if text.trim().starts_with('/') && !(self.goal_proposal.is_some() && self.activity.starts_with("Task complete")) {
            return Some("While working, use /btw QUESTION or type a prompt to queue; other commands are available after completion.".into());
        }
        let _ = actions.send(Action::Submit(text.clone()));
        Some(format!("\n{text}\n\nPrompt queued for after this turn."))
    }
}
fn clip(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        let size = ch.width().unwrap_or(0);
        if used + size > width {
            break;
        }
        used += size;
        result.push(ch);
    }
    result
}
fn terminal_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect::<String>()
        .replace('\t', "    ")
        .replace('\n', "\r\n")
}
fn content_budget(height: u16) -> usize {
    (height as usize).saturating_sub(9).clamp(2, 8)
}
/// Return complete display rows with their original UTF-8 byte endpoints.
fn visual_lines(text: &str, width: usize) -> Vec<(String, usize)> {
    let mut result = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let size = ch.width().unwrap_or(0);
        if cells + size > width.max(1) && !line.is_empty() {
            result.push((std::mem::take(&mut line), index));
            cells = 0;
        }
        line.push(ch);
        cells += size;
        end = index + ch.len_utf8();
    }
    result.push((line, end));
    result
}
fn flush_wrapped(state: &mut State, screen: &mut Frame) -> io::Result<()> {
    let (width, height) = terminal::size().unwrap_or((80, 24));
    // One content row is reserved for the disclosure once tool/thinking exists.
    let budget = content_budget(height)
        .saturating_sub(usize::from(!state.detail_entries.is_empty()))
        .max(1);
    let rows = visual_lines(&state.partial, (width as usize).saturating_sub(1));
    if rows.len() > budget {
        let count = rows.len() - budget;
        let end = rows[count - 1].1;
        screen.log(
            &rows[..count]
                .iter()
                .map(|(line, _)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        state.partial.drain(..end);
    }
    Ok(())
}
/// Logs one finished answer line through the Markdown renderer. The first
/// line of an answer block gets the reference CLI's `●`, later lines indent.
fn emit_text_line(state: &mut State, screen: &mut Frame, line: &str) {
    for rendered in state.markdown.line(line) {
        log_answer_line(state, screen, &rendered);
    }
}

fn log_answer_line(state: &mut State, screen: &mut Frame, rendered: &str) {
    if rendered.is_empty() {
        screen.log_rendered("");
    } else if std::mem::take(&mut state.block_start) {
        screen.log_rendered(&format!("\u{25cf} {rendered}"));
    } else {
        screen.log_rendered(&format!("  {rendered}"));
    }
}

/// Ends the current answer block before tool output, notices or the footer.
fn finish_text(state: &mut State, screen: &mut Frame) {
    if !state.partial.is_empty() {
        let line = std::mem::take(&mut state.partial);
        emit_text_line(state, screen, &line);
    }
    for rendered in state.markdown.finish() {
        log_answer_line(state, screen, &rendered);
    }
    state.block_start = true;
}

#[derive(Default)]
struct Frame {
    lines: Vec<String>,
    cursor_up: usize,
    cursor_column: usize,
    size: (u16, u16),
    panel_toggle: Option<super::input::editor::HitTarget>,
    panel_height: usize,
    panel_viewport: usize,
    /// Output not yet written. Erasing the frame, logging above it and drawing
    /// it again go out in one write so the terminal never shows a blank frame.
    pending: String,
}
impl Frame {
    fn anchor(&mut self, out: &mut String) {
        if self.cursor_up > 0 {
            out.push_str(&format!("\x1b[{}B", self.cursor_up));
        }
        out.push('\r');
        self.cursor_up = 0;
    }
    /// Queues erasing the frame, leaving the cursor on its first row.
    fn erase(&mut self) {
        if self.lines.is_empty() {
            return;
        }
        let mut out = std::mem::take(&mut self.pending);
        out.push_str("\x1b[?25l");
        self.anchor(&mut out);
        out.push_str("\x1b[2K");
        let rows = self.lines.len().min(
            terminal::size()
                .map(|(_, height)| usize::from(height))
                .unwrap_or(self.lines.len())
                .max(1),
        );
        for _ in 1..rows {
            out.push_str("\x1b[1A\r\x1b[2K");
        }
        self.lines.clear();
        self.pending = out;
    }
    /// Erases the frame now and writes anything queued.
    fn clear(&mut self) -> io::Result<()> {
        self.erase();
        self.flush()
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut out = std::mem::take(&mut self.pending);
        out.push_str("\x1b[?25h");
        write(&out)
    }
    /// Queues a plain line above the frame; the next draw writes both.
    fn log(&mut self, text: &str) -> io::Result<()> {
        self.erase();
        self.pending.push_str(&terminal_text(text));
        self.pending.push_str("\r\n");
        Ok(())
    }
    /// Like `log` for lines that already carry trusted style escapes.
    fn log_rendered(&mut self, line: &str) {
        self.erase();
        self.pending.push_str(line);
        self.pending.push_str("\r\n");
    }
    fn draw(&mut self, state: &mut State) -> io::Result<()> {
        let size = terminal::size().unwrap_or((80, 24));
        let width = (size.0 as usize).saturating_sub(1).max(4);
        let prompt = if state.answer.is_some() { "Answer \u{203a} " } else { "\u{203a} " };
        let prompt_width = prompt.chars().count();
        let (draft, cursor) = state.draft.visible(width.saturating_sub(prompt_width + 1));
        let status = clip(&state.status_line(), width);
        // Footer as in the reference CLI: the mode label (none for ask), then hints.
        let indicator = crate::terminal::style::mode_indicator(&state.mode);
        let hint = if state.choice.is_some() {
            "esc to interrupt"
        } else if state.answer.is_some() {
            "enter to submit your answer \u{b7} esc to interrupt"
        } else {
            "esc to interrupt"
        };
        let mode = match &indicator {
            Some((label, _)) => format!("{label} \u{b7} {hint}"),
            None => hint.to_owned(),
        };
        let rule = "─".repeat(width);
        let tip_line = state
            .tip
            .as_ref()
            .map(|tip| format!("  \u{23bf}  Tip: {tip}"))
            .unwrap_or_default();
        let budget = content_budget(size.1);
        let panel = state.details.render(width, budget);
        self.panel_toggle = panel.toggle;
        self.panel_height = panel.lines.len();
        self.panel_viewport = panel.viewport;
        let mut content = if state.details.expanded {
            panel
                .lines
                .iter()
                .map(|(text, _)| text.clone())
                .collect::<Vec<_>>()
        } else {
            let content = visual_lines(&state.partial, width)
                .into_iter()
                .map(|(line, _)| line)
                .collect::<Vec<_>>();
            if !panel.lines.is_empty() {
                // Keep the disclosure reachable without hiding the streaming text.
                self.panel_toggle = None;
            }
            content
        };
        content.resize(budget, String::new());
        content.truncate(budget);
        let mut lines = vec![
            String::new(),
            format!(
                "\x1b[{}m{}\x1b[0m",
                crate::terminal::style::activity(),
                status
            ),
            clip(
                if state.answer.is_some() && state.goal_proposal.is_none() {
                    "  \u{2191} Answer the question above: type your choice or reply, then press Enter."
                } else if state.goal_proposal.is_some() {
                    if state.answer.is_some() { "  Goal proposal pending; finish the current question first." }
                    else { "  Proposed goal: Ctrl+Y approve · Ctrl+N decline. Work continues while you decide." }
                } else {
                    &tip_line
                },
                width,
            ),
            rule.clone(),
            format!("{prompt}{draft}"),
            rule,
            clip(&mode, width),
        ];
        if std::env::var_os("NO_COLOR").is_some() {
            lines[1] = status;
        } else {
            use crossterm::style::{ResetColor, SetForegroundColor};
            for index in [2, 6] {
                // A pending question is the one thing the user must act on.
                let color = if index == 2 && state.answer.is_some() {
                    crate::terminal::style::accent()
                } else {
                    crate::terminal::style::muted()
                };
                lines[index] = format!("{}{}{}", SetForegroundColor(color), lines[index], ResetColor);
            }
            if let Some((label, color)) = &indicator {
                let footer = clip(&mode, width);
                if let Some(rest) = footer.strip_prefix(label.as_str()) {
                    lines[6] = format!(
                        "{}{label}{}{rest}{}",
                        SetForegroundColor(*color),
                        SetForegroundColor(crate::terminal::style::muted()),
                        ResetColor
                    );
                }
            }
            for index in [3, 5] {
                lines[index] = format!(
                    "{}{}{}",
                    SetForegroundColor(crate::terminal::style::border()),
                    lines[index],
                    ResetColor
                );
            }
        }
        // The content viewport has a fixed height throughout the turn, so a long
        // stream grows into scrollback without making the composer jump.
        if !state.details.expanded && !panel.lines.is_empty() {
            let label = panel.lines[0].0.clone();
            content[budget - 1] = label;
            self.panel_height = budget;
            self.panel_toggle = panel.toggle.map(|mut hit| {
                hit.row = budget - 1;
                hit
            });
        }
        let choice_rows = state.choice.as_ref().map(|choice| choice_lines(choice, &lines[1], &mode, width));
        let choice_cursor = choice_rows.as_ref().map(|(rows, selected)| rows.len() - *selected);
        if let Some((rows, _)) = choice_rows {
            lines = rows;
        }
        content.extend(lines);
        let mut lines = content;
        let rows = (size.1 as usize).saturating_sub(1).max(3);
        if lines.len() > rows {
            let removed = lines.len() - rows;
            lines.drain(0..removed);
            self.panel_toggle = self.panel_toggle.and_then(|mut hit| {
                hit.row = hit.row.checked_sub(removed)?;
                Some(hit)
            });
            self.panel_height = self.panel_height.saturating_sub(removed);
        }
        let input_row = lines.len().saturating_sub(choice_cursor.unwrap_or(3));
        let cursor_column = if choice_cursor.is_some() { 1 } else { (prompt_width + cursor).min(width) };
        if self.pending.is_empty()
            && self.lines == lines
            && self.size == size
            && self.cursor_column == cursor_column
        {
            return Ok(());
        }
        if !self.lines.is_empty() && (self.size != size || self.lines.len() != lines.len()) {
            self.erase();
        }
        let mut out = std::mem::take(&mut self.pending);
        out.push_str("\x1b[?25l");
        self.anchor(&mut out);
        if self.lines.is_empty() {
            out.push_str(&lines.join("\r\n"));
        } else {
            out.push_str(&row_updates(&self.lines, &lines));
        }
        self.cursor_up = lines.len() - 1 - input_row;
        if self.cursor_up > 0 {
            out.push_str(&format!("\x1b[{}A", self.cursor_up));
        }
        out.push_str(&format!("\r\x1b[{cursor_column}C\x1b[?25h"));
        self.lines = lines;
        self.size = size;
        self.cursor_column = cursor_column;
        write(&out)
    }
}
/// The frame below the activity line while a menu question is pending:
/// the question, `❯ 1. Yes` style options and a key hint. Returns the rows and
/// the selected row counted from the end.
fn choice_lines(choice: &Choice, status: &str, mode: &str, width: usize) -> (Vec<String>, usize) {
    use crossterm::style::{Attribute, ResetColor, SetAttribute, SetForegroundColor};
    let color = std::env::var_os("NO_COLOR").is_none();
    let paint = |text: String, fg: crossterm::style::Color| {
        if color { format!("{}{text}{}", SetForegroundColor(fg), ResetColor) } else { text }
    };
    let rule = paint("\u{2500}".repeat(width), crate::terminal::style::border());
    let question = clip(&format!(" {}", terminal_text(&choice.question)), width);
    let mut rows = vec![
        String::new(),
        status.to_owned(),
        rule,
        if color { format!("{}{question}{}", SetAttribute(Attribute::Bold), SetAttribute(Attribute::Reset)) } else { question },
    ];
    for (index, option) in choice.options.iter().enumerate() {
        let text = clip(&format!(" {} {}. {}", if index == choice.selected { "\u{276f}" } else { " " }, index + 1, terminal_text(option)), width);
        rows.push(if index == choice.selected { paint(text, crate::terminal::style::accent()) } else { text });
    }
    let hint = mode.replace("esc to interrupt", "\u{2191}\u{2193} select \u{b7} enter confirm \u{b7} esc to interrupt");
    rows.push(paint(clip(&format!(" {hint}"), width), crate::terminal::style::muted()));
    let selected = 1 + choice.options.len() - choice.selected;
    (rows, selected)
}

fn row_updates(previous: &[String], next: &[String]) -> String {
    let mut output = String::new();
    for (row, line) in next.iter().enumerate() {
        if previous.get(row) == Some(line) {
            continue;
        }
        let up = next.len() - 1 - row;
        if up > 0 {
            output.push_str(&format!("\x1b[{up}A"));
        }
        output.push('\r');
        output.push_str(line);
        output.push_str("\x1b[K");
        if up > 0 {
            output.push_str(&format!("\x1b[{up}B"));
        }
        output.push('\r');
    }
    output
}

fn write(text: &str) -> io::Result<()> {
    let mut output = io::stderr().lock();
    // Synchronized output: terminals that support it present the whole update
    // at once; others ignore the private mode.
    output.write_all(b"\x1b[?2026h")?;
    output.write_all(text.as_bytes())?;
    output.write_all(b"\x1b[?2026l")?;
    output.flush()
}
fn run(
    state: &mut State,
    screen: &mut Frame,
    updates: mpsc::Receiver<Update>,
    actions: async_mpsc::UnboundedSender<Action>,
) -> io::Result<()> {
    let mut last_tick = Instant::now();
    let mut input_reader = super::paste_input::Reader::new()?;
    screen.draw(state)?;
    loop {
        let mut dirty = false;
        loop {
            match updates.try_recv() {
                Ok(Update::GoalProposal(proposal)) => {
                    if let Some((id, condition)) = &proposal {
                        if state.goal_proposal.as_ref().is_none_or(|old| old.0 != *id) {
                            screen.log(&format!("Proposed session goal:\n{}\nCtrl+Y approves this condition; Ctrl+N declines. The current task continues.", terminal_text(condition)))?;
                        }
                    }
                    state.goal_proposal = proposal;
                    dirty = true;
                },
                Ok(Update::PermissionMode(mode)) => { state.mode = mode; dirty = true; },
                Ok(Update::Text(text)) => {
                    state.generated_characters = state
                        .generated_characters
                        .saturating_add(text.chars().count());
                    state.estimate_pending = true;
                    if state.activity.contains("thinking")
                        || state.activity == "Waiting for response"
                    {
                        state.activity = "Composing".into();
                        state.activity_started = Instant::now();
                    }
                    state
                        .partial
                        .push_str(&terminal_text(&text).replace("\r\n", "\n"));
                    while let Some(index) = state.partial.find('\n') {
                        let line = state.partial[..index].to_owned();
                        state.partial.drain(..index + 1);
                        emit_text_line(state, screen, &line);
                    }
                    dirty = true;
                }
                Ok(Update::Notice(text)) => {
                    finish_text(state, screen);
                    screen.log(&text)?;
                    dirty = true;
                }
                Ok(Update::Styled(lines)) => {
                    finish_text(state, screen);
                    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                    let color = std::env::var_os("NO_COLOR").is_none();
                    for line in lines {
                        screen.log_rendered(&change_view::render(&line, width, color));
                    }
                    dirty = true;
                }
                Ok(Update::HookActivity(activity)) => {
                    state.hook_activity = activity;
                    dirty = true;
                }
                Ok(Update::Activity(activity)) => {
                    if state.activity != activity {
                        state.activity_started = Instant::now();
                    }
                    state.activity = activity;
                    dirty = true;
                }
                Ok(Update::Usage(value)) => {
                    state.usage.update(&value);
                    if value["output_tokens"]
                        .as_u64()
                        .or_else(|| value["completion_tokens"].as_u64())
                        .is_some_and(|value| value > 0)
                    {
                        state.estimate_pending = false;
                    }
                    dirty = true;
                }
                Ok(Update::Detail(id, title, body)) => {
                    state.detail(id, title, body);
                    dirty = true;
                }
                Ok(Update::Thinking(text)) => {
                    state.append_thinking(&text);
                    dirty = true;
                }
                Ok(Update::Choice(lines, question, options, reply)) => {
                    finish_text(state, screen);
                    if state.details.expanded {
                        state.details.toggle();
                    }
                    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                    let color = std::env::var_os("NO_COLOR").is_none();
                    for line in lines {
                        screen.log_rendered(&change_view::render(&line, width, color));
                    }
                    state.question(reply);
                    state.choice = Some(Choice { question, options, selected: 0 });
                    dirty = true;
                }
                Ok(Update::Question(text, reply)) => {
                    finish_text(state, screen);
                    if state.details.expanded {
                        state.details.toggle();
                    }
                    screen.log(&text)?;
                    state.question(reply);
                    dirty = true;
                }
                Ok(Update::Stop(error)) => {
                    finish_text(state, screen);
                    let draft = if state.answer.is_some() {
                        state.saved_draft.take().unwrap_or_default().take()
                    } else {
                        state.draft.take()
                    };
                    if !draft.is_empty() {
                        let _ = actions.send(Action::Draft(draft));
                    }
                    screen.erase();
                    // As in the reference CLI, after a blank line:
                    // "✻ Churned for 7m 7s · done 10:41 AM".
                    let footer = format!(
                        "\u{273b} {} for {} \u{b7} {} {}",
                        PAST_VERBS[state.frame.wrapping_add(state.started.elapsed().subsec_nanos() as usize) % PAST_VERBS.len()],
                        short_duration(state.started.elapsed().as_secs()),
                        error.as_deref().unwrap_or("done"),
                        chrono::Local::now().format("%-I:%M %p")
                    );
                    screen.log("")?;
                    if std::env::var_os("NO_COLOR").is_some() {
                        screen.log(&footer)?;
                    } else {
                        screen.log_rendered(&change_view::render(
                            &change_view::muted(&footer),
                            usize::MAX,
                            true,
                        ));
                    }
                    return screen.flush();
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        dirty |= state.dismiss_closed_question();
        if let Some(event) = input_reader.next(Duration::from_millis(25))? {
            // Cancellation may arrive while waiting for a key. Never submit a
            // dismissed approval draft as a newly queued user prompt.
            if state.dismiss_closed_question() {
                flush_wrapped(state, screen)?;
                screen.draw(state)?;
                continue;
            }
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = match state.bindings.resolve(key, &mut state.key_pending) {
                        crate::keybindings::Resolution::Key(key) => key,
                        crate::keybindings::Resolution::Consumed => continue,
                    };
                    if crate::keybindings::is_cycle_permission(key) {
                        state.cycle_permission(&actions);
                        continue;
                    }
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    if !(control && key.code == KeyCode::Char('r')) {
                        state.search_query = None;
                    }
                    if state.choice_key(key) {
                    } else if control && matches!(key.code, KeyCode::Char('y' | 'n')) && state.goal_proposal.is_some() && state.answer.is_none() {
                        state.decide_goal(key.code == KeyCode::Char('y'), &actions);
                    } else if control && key.code == KeyCode::Char('b') {
                        let _ = actions.send(Action::Detach);
                    } else if control && key.code == KeyCode::Char('o') {
                        state.details.toggle();
                    } else if control && key.code == KeyCode::Char('r') && state.answer.is_none() {
                        let query = state
                            .search_query
                            .get_or_insert_with(|| state.draft.characters.iter().collect())
                            .clone();
                        if let Some(index) = (0..state.history_index)
                            .rev()
                            .find(|&index| state.history[index].contains(&query))
                        {
                            if state.history_index == state.history.len() {
                                state.history_draft = state.draft.characters.iter().collect();
                            }
                            state.history_index = index;
                            state.draft.set(&state.history[index]);
                        }
                    } else if control && key.code == KeyCode::Char('l') {
                        screen.clear()?;
                    } else if control && key.code == KeyCode::Char('u') {
                        state.draft.characters.drain(..state.draft.cursor);
                        state.draft.cursor = 0;
                    } else if control && key.code == KeyCode::Char('k') {
                        state.draft.characters.truncate(state.draft.cursor);
                    } else if control
                        && key.code == KeyCode::Char('d')
                        && state.draft.characters.is_empty()
                    {
                        let _ = actions.send(Action::Submit("/exit".into()));
                        let _ = actions.send(Action::Cancel);
                    } else if state.details.expanded
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                    {
                        state.details.scroll_by(
                            if key.code == KeyCode::PageUp {
                                -(screen.panel_viewport as isize)
                            } else {
                                screen.panel_viewport as isize
                            },
                            screen.panel_viewport,
                        );
                    } else if key.code == KeyCode::Up
                        && state.answer.is_none()
                        && state.history_index > 0
                    {
                        if state.history_index == state.history.len() {
                            state.history_draft = state.draft.characters.iter().collect();
                        }
                        state.history_index -= 1;
                        state.draft.set(&state.history[state.history_index]);
                    } else if key.code == KeyCode::Down
                        && state.answer.is_none()
                        && state.history_index < state.history.len()
                    {
                        state.history_index += 1;
                        let text = state
                            .history
                            .get(state.history_index)
                            .unwrap_or(&state.history_draft);
                        state.draft.set(text);
                    } else if (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                        || key.code == KeyCode::Esc
                    {
                        let _ = actions.send(Action::Cancel);
                        state.activity = "Cancelling".into();
                    } else {
                        match key.code {
                            KeyCode::Enter
                                if key.modifiers.contains(KeyModifiers::SHIFT)
                                    || key.modifiers.contains(KeyModifiers::ALT) =>
                            {
                                state.draft.insert("\n")
                            }
                            KeyCode::Enter => {
                                if let Some(message) = state.submit(&actions) {
                                    screen.log(&message)?;
                                }
                            }
                            KeyCode::Backspace if state.draft.cursor > 0 => {
                                state.draft.cursor -= 1;
                                state.draft.characters.remove(state.draft.cursor);
                            }
                            KeyCode::Delete
                                if state.draft.cursor < state.draft.characters.len() =>
                            {
                                state.draft.characters.remove(state.draft.cursor);
                            }
                            KeyCode::Left => {
                                state.draft.cursor = state.draft.cursor.saturating_sub(1)
                            }
                            KeyCode::Right => {
                                state.draft.cursor =
                                    (state.draft.cursor + 1).min(state.draft.characters.len())
                            }
                            KeyCode::Home => state.draft.cursor = 0,
                            KeyCode::End => state.draft.cursor = state.draft.characters.len(),
                            KeyCode::Char(ch)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                state.draft.insert(&ch.to_string())
                            }
                            _ => {}
                        }
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    let (_, cursor_y) = crossterm::cursor::position()?;
                    let input_row = screen.lines.len().saturating_sub(1 + screen.cursor_up);
                    let origin = cursor_y.saturating_sub(input_row as u16);
                    if let Some(row) = mouse.row.checked_sub(origin) {
                        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                            && screen.panel_toggle.is_some_and(|hit| {
                                hit.contains(row as usize, mouse.column as usize)
                            })
                        {
                            state.details.toggle();
                            dirty = true;
                        } else if (row as usize) < screen.panel_height {
                            match mouse.kind {
                                MouseEventKind::ScrollDown => {
                                    state.details.scroll_by(3, screen.panel_viewport);
                                    dirty = true;
                                }
                                MouseEventKind::ScrollUp => {
                                    state.details.scroll_by(-3, screen.panel_viewport);
                                    dirty = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    if !state.draft.insert_paste(&text) {
                        screen.log("Paste not inserted: the active draft limit is 32768 characters. Shorten the text and paste again.")?;
                    }
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        if last_tick.elapsed() >= Duration::from_millis(120) {
            last_tick = Instant::now();
            state.frame = state.frame.wrapping_add(1);
            dirty = true;
        }
        if dirty {
            flush_wrapped(state, screen)?;
            screen.draw(state)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_paste_is_rejected_without_partial_draft_changes() {
        let mut draft=Draft::default();draft.insert("keep");
        assert!(!draft.insert_paste(&"x".repeat(32_768)));
        assert_eq!(draft.take(),"keep");
        assert!(draft.insert_paste("first\r\n\n中文"));
        assert_eq!(draft.take(),"first\n\n中文");
    }
    #[test]
    fn permission_cycle_preserves_pending_answer_and_queued_draft() {
        let mut state = State::new("ask".into());
        state.draft.insert("queued draft");
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.draft.insert("unfinished answer");
        let (actions, mut events) = async_mpsc::unbounded_channel();
        state.cycle_permission(&actions);
        assert!(matches!(events.try_recv().unwrap(), Action::CyclePermission));
        assert!(received.try_recv().is_err());
        assert!(state.answer.is_some());
        state.submit(&actions);
        assert_eq!(received.try_recv().unwrap(), "unfinished answer");
        assert_eq!(state.draft.take(), "queued draft");
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn long_thinking_stream_keeps_exact_unicode_prefix_and_all_usage_characters() {
        let mut state = State::new("ask".into());
        state.draft.insert("未提交 draft");
        state.detail(
            "tool-first".into(),
            "Read".into(),
            "unchanged tool output".into(),
        );
        let delta = "中🦀e\u{301}\n0123456789";
        let mut complete = String::new();
        for index in 0..6000 {
            state.append_thinking(delta);
            complete.push_str(delta);
            if index == 2000 {
                state.detail("tool-second".into(), "Bash".into(), "second output".into());
            }
        }
        assert_eq!(state.generated_characters, complete.chars().count());
        assert_eq!(state.thinking_characters, 65536);
        assert!(state.estimate_pending);
        assert_eq!(state.detail_entries.len(), 3);
        assert_eq!(state.detail_entries[0].2, "unchanged tool output");
        assert_eq!(state.detail_entries[2].2, "second output");
        let expected = complete.chars().take(65536).collect::<String>();
        assert_eq!(
            state.detail_entries[state.thinking_entry.unwrap()].2,
            expected
        );
        // A later non-thinking update rebuilds the panel from canonical detail
        // storage, so the prefix must remain exact there too.
        state.detail(
            "tool-first".into(),
            "Read completed".into(),
            "updated output".into(),
        );
        state.details.toggle();
        let panel = state.details.render(100, 12);
        assert!(panel
            .lines
            .iter()
            .any(|(line, _)| line.contains("中🦀e\u{301}")));
        assert_eq!(state.draft.take(), "未提交 draft");
    }

    #[test]
    fn streamed_thinking_refreshes_open_details_without_resetting_disclosure() {
        let mut state = State::new("ask".into());
        state.append_thinking("first ");
        state.details.toggle();
        assert!(state
            .details
            .render(60, 8)
            .lines
            .iter()
            .any(|(line, _)| line == "first "));
        state.append_thinking("second 中文");
        assert!(state.details.expanded);
        assert!(state
            .details
            .render(60, 8)
            .lines
            .iter()
            .any(|(line, _)| line == "first second 中文"));
        assert_eq!(
            state.generated_characters,
            "first second 中文".chars().count()
        );
        state.details.toggle();
        state.append_thinking(" 🦀");
        state.details.toggle();
        assert!(state
            .details
            .render(60, 8)
            .lines
            .iter()
            .any(|(line, _)| line == "first second 中文 🦀"));
    }

    #[test]
    fn wrapped_rows_preserve_unicode_bytes_and_combining_marks() {
        let text = "ab\u{4e2d}e\u{301}xyz";
        let rows = visual_lines(text, 4);
        assert_eq!(
            rows.iter().map(|(row, _)| row.as_str()).collect::<String>(),
            text
        );
        assert_eq!(rows[0], ("ab\u{4e2d}".into(), 5));
        for (_, end) in rows {
            assert!(text.is_char_boundary(end));
        }
    }
    #[test]
    fn detail_updates_preserve_expansion_and_input_draft() {
        let mut state = State::new("default".into());
        state.draft.insert("unfinished prompt");
        state.detail("tool".into(), "Read".into(), "first".into());
        state.details.toggle();
        state.detail("tool".into(), "Read completed".into(), "second".into());
        assert!(state.details.expanded);
        let panel = state.details.render(60, 8);
        let hit = panel.toggle.unwrap();
        assert!(hit.contains(hit.row, 0));
        assert!(panel.lines.iter().any(|(line, _)| line.contains("second")));
        state.details.toggle();
        assert_eq!(state.draft.take(), "unfinished prompt");
    }
    #[test]
    fn noninteractive_ignores_invalid_keybinding_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keybindings.json");
        std::fs::write(&path, "not valid JSON").unwrap();
        assert!(TurnDisplay::new(false, "default", &path, vec![]).is_ok());
        assert!(TurnDisplay::new(true, "default", &path, vec![]).is_err());
    }

    #[test]
    fn stable_timer_updates_never_clear_the_frame_or_scroll() {
        let before = vec!["answer".into(), "working 1s".into(), "draft".into()];
        assert_eq!(row_updates(&before, &before), "");
        let after = vec!["answer".into(), "working 2s".into(), "draft".into()];
        let update = row_updates(&before, &after);
        assert!(update.contains("working 2s"));
        assert!(!update.contains("answer"));
        assert!(!update.contains("draft"));
        assert!(!update.contains('\n'));
        assert!(!update.contains("\x1b[2K"));
    }
    #[test]
    fn questions_preserve_drafts_and_do_not_queue_answers() {
        let mut state = State::new("ask".into());
        state.draft.insert("queued draft");
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.draft.insert("yes");
        let (actions, mut events) = async_mpsc::unbounded_channel();
        state.submit(&actions);
        assert_eq!(received.try_recv().unwrap(), "yes");
        assert_eq!(state.draft.take(), "queued draft");
        assert!(events.try_recv().is_err());
    }
    #[test]
    fn menu_questions_answer_with_arrows_numbers_and_leave_esc_to_interrupt() {
        use crossterm::event::KeyEvent;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut state = State::new("ask".into());
        state.draft.insert("queued draft");
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { question: "Proceed?".into(), options: vec!["Yes".into(), "Always".into(), "No".into()], selected: 0 });
        assert!(state.choice_key(key(KeyCode::Up)));
        assert_eq!(state.choice.as_ref().unwrap().selected, 2);
        assert!(state.choice_key(key(KeyCode::Down)));
        assert!(state.choice_key(key(KeyCode::Down)));
        assert!(state.choice_key(key(KeyCode::Char('x'))));
        assert!(!state.choice_key(key(KeyCode::Esc)));
        assert!(state.choice_key(key(KeyCode::Enter)));
        assert_eq!(received.try_recv().unwrap(), "2");
        assert!(state.choice.is_none() && state.answer.is_none());
        assert_eq!(state.draft.take(), "queued draft");

        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { question: "Proceed?".into(), options: vec!["Yes".into(), "No".into()], selected: 0 });
        assert!(state.choice_key(key(KeyCode::Char('3'))));
        assert!(received.try_recv().is_err());
        assert!(state.choice_key(key(KeyCode::Char('2'))));
        assert_eq!(received.try_recv().unwrap(), "2");
        let (rows, selected) = choice_lines(&Choice { question: "Proceed?".into(), options: vec!["Yes".into(), "No".into()], selected: 1 }, "status", "esc to interrupt", 40);
        assert_eq!(rows.len() - selected, 5);
    }
    #[test]
    fn cancelled_question_discards_answer_and_restores_original_draft() {
        let mut state = State::new("ask".into());
        state.draft.insert("original draft");
        let (reply, receive) = oneshot::channel();
        state.question(reply);
        state.draft.insert("late yes");
        drop(receive);
        assert!(state.dismiss_closed_question());
        assert_eq!(state.draft.take(), "original draft");
        assert!(state.answer.is_none());
        assert!(!state.dismiss_closed_question());
        let (reply, mut receive) = oneshot::channel();
        state.question(reply);
        state.draft.insert("fresh no");
        state.submit(&async_mpsc::unbounded_channel().0);
        assert_eq!(receive.try_recv().unwrap(), "fresh no");
    }
    #[test]
    fn side_questions_and_queued_prompts_are_distinct() {
        let mut state = State::new("ask".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        state.draft.insert("/btw question");
        state.submit(&send);
        assert!(matches!(
            receive.try_recv().unwrap(),
            Action::SideQuestion(_)
        ));
        state.draft.insert("next task");
        state.submit(&send);
        assert!(matches!(receive.try_recv().unwrap(), Action::Submit(_)));
        state.draft.insert("/permissions bypass");
        state.submit(&send);
        assert!(receive.try_recv().is_err());
    }
    #[test]
    fn live_goal_controls_reach_owner_without_accepting_other_slash_commands() {
        let mut state = State::new("ask".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        for prompt in ["/goal", "/goal clear", "/goal STOP", "/goal off"] {
            state.draft.insert(prompt);
            assert!(state.submit(&send).is_none());
            assert!(matches!(receive.try_recv().unwrap(), Action::Submit(text) if text == prompt));
        }
        state.draft.insert("/goal start a different task");
        assert!(state.submit(&send).is_some());
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn goal_decision_is_separate_from_prompts_and_preserves_the_draft() {
        let mut state = State::new("ask".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        state.draft.insert("my unfinished next prompt");
        state.goal_proposal = Some(("proposal-id".into(), "Run tests".into()));
        state.decide_goal(true, &send);
        assert!(matches!(receive.try_recv().unwrap(), Action::GoalDecision { id, approved: true } if id == "proposal-id"));
        assert_eq!(state.draft.take(), "my unfinished next prompt");
        state.decide_goal(false, &send);
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn unanswered_goal_does_not_block_commands_after_the_model_finishes() {
        let mut state = State::new("ask".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        state.goal_proposal = Some(("proposal-id".into(), "Run tests".into()));
        state.draft.insert("/model");
        state.submit(&send);
        assert!(receive.try_recv().is_err());
        state.activity = "Task complete · goal proposal awaiting your choice".into();
        state.draft.insert("/model");
        state.submit(&send);
        assert!(matches!(receive.try_recv().unwrap(), Action::Submit(command) if command == "/model"));
    }
}

/// Unified API: exactly one of the live actor and legacy progress writes output.
pub struct TurnDisplay {
    workspace: Option<std::path::PathBuf>,
    pub live: Option<LiveTurn>,
    hooks: std::collections::BTreeMap<String, String>,
    fallback: super::progress::TurnProgress,
    tools: std::collections::HashMap<String, (Instant, Value)>,
    finished: std::collections::HashMap<String, (String, Value, String, bool)>,
    /// Lines added and removed per file this turn, for the closing summary.
    changes: std::collections::BTreeMap<String, (u64, u64)>,
}
impl TurnDisplay {
    pub fn new(
        interactive: bool,
        mode: &str,
        keybindings: &std::path::Path,
        history: Vec<String>,
    ) -> anyhow::Result<Self> {
        let live = if interactive {
            LiveTurn::start(
                mode,
                crate::keybindings::load(keybindings)?,
                history,
                keybindings.parent().and_then(super::tips::next),
            )
        } else {
            None
        };
        let mut fallback = super::progress::TurnProgress::new(interactive && live.is_none());
        fallback.set_permission_mode(mode);
        Ok(Self {
            workspace: None,
            live,
            hooks: Default::default(),
            fallback,
            tools: Default::default(),
            finished: Default::default(),
            changes: Default::default(),
        })
    }
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }
    pub fn with_workspace(mut self, workspace: &std::path::Path) -> Self {
        self.workspace = Some(workspace.to_owned());
        self.fallback.set_workspace(workspace);
        self
    }
    pub fn set_permission_mode(&mut self, mode: &str) {
        if let Some(live) = &self.live { live.permission_mode(mode); }
        self.fallback.set_permission_mode(mode);
    }
    pub fn set_activity(&mut self, text: &str) {
        if let Some(live) = &self.live {
            live.activity(text);
        } else {
            self.fallback.set_activity(text);
        }
    }
    pub fn hook_progress(&mut self, id: &str, message: Option<&str>) {
        if let Some(message) = message { self.hooks.insert(id.into(), message.into()); }
        else { self.hooks.remove(id); }
        self.render_hooks();
    }
    pub fn hook_snapshot(&mut self, hooks: Vec<(String, String)>) {
        self.hooks = hooks.into_iter().collect();
        self.render_hooks();
    }
    fn render_hooks(&mut self) {
        let message = (!self.hooks.is_empty()).then(|| self.hooks.values().map(|message| super::progress::single_line(message)).collect::<Vec<_>>().join(" · "));
        if let Some(live) = &self.live { let _ = live.updates.send(Update::HookActivity(message)); }
        else { self.fallback.hook_activity(message); }
    }
    pub fn record_thinking(&self, text: &str) {
        if let Some(live) = &self.live {
            live.thinking(text);
        }
    }
    pub fn thinking(&mut self) {
        self.set_activity("Sketching \u{b7} thinking");
    }
    pub fn reported_usage(&mut self, value: &Value) {
        if let Some(live) = &self.live {
            live.usage(value);
        } else {
            self.fallback.reported_usage(value);
        }
    }
    pub fn before_text(&mut self, text: &str) {
        if let Some(live) = &self.live {
            live.text(text);
        } else {
            self.fallback.before_text(text);
        }
    }
    pub fn tool_started(&mut self, id: &str, name: &str, input: &Value) {
        if let Some(live) = &self.live {
            if self.tools.contains_key(id) || self.finished.contains_key(id) {
                return;
            }
            self.tools.insert(id.to_owned(), (Instant::now(), input.clone()));
            live.detail(id, name, &format!("Input:\n{}", input));
            // The call is printed once, with its result, when it finishes; the
            // status line names what is running meanwhile.
            live.activity(&format!(
                "Running {}",
                super::tool_view::label(name, input, self.workspace.as_deref())
            ));
        } else {
            self.fallback.tool_started(id, name, input);
        }
    }
    pub fn tool_finished(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        output: &str,
        failed: bool,
    ) {
        if self.finished.get(id).is_some_and(|previous| {
            previous.0 == name
                && previous.1 == *input
                && previous.2 == output
                && previous.3 == failed
        }) {
            return;
        }
        self.finished.insert(
            id.to_owned(),
            (name.into(), input.clone(), output.into(), failed),
        );
        if !failed {
            if let Ok(value) = serde_json::from_str::<Value>(output) {
                let change = &value["file_change"];
                if let Some(path) = change["path"].as_str() {
                    let entry = self.changes.entry(path.to_owned()).or_default();
                    entry.0 += change["added"].as_u64().unwrap_or(0);
                    entry.1 += change["removed"].as_u64().unwrap_or(0);
                }
            }
        }
        if let Some(live) = &self.live {
            let previous = self.tools.remove(id);
            let elapsed = previous.as_ref().map(|(start, _)| start.elapsed());
            let body = format!("Input:\n{}\n\nOutput:\n{}", input, output);
            live.detail(
                id,
                &format!("{name} - {}", if failed { "error" } else { "completed" }),
                &body,
            );
            let change = matches!(name, "Write" | "Edit" | "MultiEdit")
                .then(|| change_view::change(output, failed))
                .flatten();
            if let Some(lines) = change {
                live.detail(
                    id,
                    &format!("{name} - completed"),
                    &format!("{}\n\n{}", change_view::plain(&lines), body),
                );
                let display_lines =
                    change_view::change_in_workspace(output, failed, self.workspace.as_deref())
                        .unwrap_or_else(|| lines.clone());
                let mut preview = display_lines.iter().take(14).cloned().collect::<Vec<_>>();
                if lines.len() > 14 {
                    preview.push(change_view::success("More changes in details (Ctrl+O)"));
                }
                live.styled(preview);
            } else {
                // A mirrored completion can arrive without the call's input;
                // label it with the input seen when the call started.
                let started = previous.as_ref().map(|(_, input)| input);
                let label_input = match started {
                    Some(started) if input.as_object().is_none_or(|fields| fields.is_empty()) => started,
                    _ => input,
                };
                live.styled(super::tool_view::finished(
                    name,
                    label_input,
                    output,
                    failed,
                    elapsed,
                    self.workspace.as_deref(),
                ));
            }
            live.activity("Working");
        } else {
            self.fallback.tool_finished(id, name, input, output, failed);
        }
    }
    pub fn agent_progress(&mut self, id: &str, kind: &str, status: &str, tool: Option<&str>) {
        if let Some(live) = &self.live {
            live.notice(&format!(
                "  Agent {kind} [{id}] \u{b7} {status} {}",
                tool.unwrap_or("")
            ));
        } else {
            self.fallback.agent_progress(id, kind, status, tool);
        }
    }
    pub fn tick(&mut self) {
        if self.live.is_none() {
            self.fallback.tick();
        }
    }
    pub fn clear(&mut self) {
        if self.live.is_none() {
            self.fallback.clear();
        }
    }
    pub fn notice(&mut self, text: &str) {
        if let Some(live) = &self.live {
            live.notice(text);
        } else {
            self.fallback.clear();
            eprintln!("{text}");
        }
    }
    pub async fn action(&mut self) -> Option<Action> {
        match &mut self.live {
            Some(live) => live.actions.recv().await,
            None => std::future::pending().await,
        }
    }
    /// `● Changed 2 files (+13 −3)` with one line per file.
    fn change_summary(&self) -> Vec<change_view::Line> {
        if self.changes.is_empty() {
            return Vec::new();
        }
        let counts = |(added, removed): (u64, u64)| match (added, removed) {
            (added, 0) => format!("+{added}"),
            (0, removed) => format!("\u{2212}{removed}"),
            (added, removed) => format!("+{added} \u{2212}{removed}"),
        };
        let total = self.changes.values().fold((0, 0), |sum, (a, r)| (sum.0 + a, sum.1 + r));
        let files = self.changes.len();
        let mut lines = vec![change_view::tool_call(
            &format!("Changed {files} file{} ({})", if files == 1 { "" } else { "s" }, counts(total)),
            "Ctrl+O shows the diffs",
            false,
        )];
        let labels: Vec<String> = self
            .changes
            .keys()
            .map(|path| super::display_path::label(path, self.workspace.as_deref()))
            .collect();
        let width = labels.iter().map(|label| label.chars().count()).max().unwrap_or(0);
        for (label, change) in labels.iter().zip(self.changes.values()) {
            lines.push(change_view::muted(&format!("    {label:<width$}  {}", counts(*change))));
        }
        lines
    }
    pub fn finish(&mut self, error: Option<&str>) -> Vec<Action> {
        if let Some(live) = self.live.take() {
            let summary = self.change_summary();
            if !summary.is_empty() {
                live.styled(summary);
            }
            live.finish(error)
        } else {
            self.fallback.finish(error);
            Vec::new()
        }
    }
}


#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn changed_files_are_summarised_per_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut display = TurnDisplay::new(false, "ask", &temp.path().join("keys.json"), vec![]).unwrap().with_workspace(temp.path());
        let output = |path: &str, added: u64, removed: u64| {
            serde_json::json!({"success":true,"file_change":{"path":temp.path().join(path),"added":added,"removed":removed}}).to_string()
        };
        display.tool_finished("1", "Edit", &serde_json::json!({}), &output("src/main.rs", 10, 3), false);
        display.tool_finished("2", "Edit", &serde_json::json!({}), &output("src/main.rs", 2, 0), false);
        display.tool_finished("3", "Write", &serde_json::json!({}), &output("README.md", 4, 0), false);
        display.tool_finished("4", "Write", &serde_json::json!({}), &output("ignored.md", 9, 0), true);
        let lines: Vec<String> = display.change_summary().iter().map(|line| change_view::render(line, usize::MAX, false)).collect();
        assert_eq!(lines[0], "\u{25cf} Changed 2 files (+16 \u{2212}3) \u{b7} Ctrl+O shows the diffs");
        assert_eq!(lines[1], "    README.md    +4");
        assert_eq!(lines[2], "    src/main.rs  +12 \u{2212}3");
    }
    #[test]
    fn hook_progress_preserves_other_running_hooks_and_underlying_activity() {
        let temp = tempfile::tempdir().unwrap();
        let mut display = TurnDisplay::new(false, "ask", &temp.path().join("keys.json"), vec![]).unwrap();
        display.set_activity("Working: Read");
        display.hook_progress("one", Some("Check A"));
        display.hook_progress("two", Some("Check B"));
        display.hook_progress("one", None);
        assert_eq!(display.hooks.values().collect::<Vec<_>>(), vec!["Check B"]);
        display.hook_progress("two", None);
        assert!(display.hooks.is_empty());
    }

    #[test]
    fn distinct_results_update_live_details_with_actual_input_without_duplicate_counts() {
        let temp = tempfile::tempdir().unwrap();
        let mut display =
            TurnDisplay::new(false, "ask", &temp.path().join("keys.json"), vec![]).unwrap();
        let (send, receive) = mpsc::channel();
        let (_, actions) = async_mpsc::unbounded_channel();
        display.live = Some(LiveTurn {
            updates: send,
            actions,
            worker: None,
        });
        let input = serde_json::json!({"file_path":"a.txt"});
        display.tool_started("one", "Read", &input);
        display.tool_finished("one", "Read", &input, "old", false);
        let mut state = State::new("ask".into());
        for update in receive.try_iter() {
            if let Update::Detail(id, title, body) = update {
                state.detail(id, title, body);
            }
        }
        assert_eq!(state.detail_entries.len(), 1);
        display.tool_finished("one", "Read", &input, "old", false);
        assert!(
            receive.try_recv().is_err(),
            "identical mirror must not reset activity or redraw"
        );
        for (name, input, output, failed) in [
            (
                "Read",
                serde_json::json!({"file_path":"a.txt"}),
                "new",
                false,
            ),
            (
                "Read",
                serde_json::json!({"file_path":"b.txt"}),
                "new",
                false,
            ),
            (
                "Read",
                serde_json::json!({"file_path":"b.txt"}),
                "new",
                true,
            ),
            (
                "Other",
                serde_json::json!({"file_path":"b.txt"}),
                "new",
                true,
            ),
        ] {
            display.tool_finished("one", name, &input, output, failed);
            let mut updated = false;
            for update in receive.try_iter() {
                if let Update::Detail(id, title, body) = update {
                    assert!(body.contains(&input.to_string()));
                    assert!(body.contains(output));
                    assert!(title.contains(name));
                    assert_eq!(title.contains("error"), failed);
                    state.detail(id, title, body);
                    updated = true;
                }
            }
            assert!(updated);
            assert_eq!(state.detail_entries.len(), 1);
            assert_eq!(display.finished.len(), 1);
            assert!(display.tools.is_empty());
        }
    }
}
