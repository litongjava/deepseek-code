//! One terminal owner while a model turn runs. The idle editor is inactive.
use super::change_view;
use super::tool_view::Gathering;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
        MouseEventKind,
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub enum Action {
    GoalDecision { id: String, approved: bool },
    CyclePermission,
    Submit(String),
    Queue(String),
    SideQuestion(String),
    Cancel,
    Detach,
    Draft(String),
    /// Up in an empty input while messages wait: take them back to edit.
    EditQueued,
    /// Ctrl+B during a Bash call: it continues in the background.
    BackgroundShell,
    /// ctrl+x ctrl+s: deliver waiting messages now, moving a running Bash
    /// call to the background so the turn reaches its next boundary.
    SendNow,
}
enum Update {
    GoalProposal(Option<(String, String)>),
    PermissionMode(String),
    Text(String),
    Notice(String),
    Styled(Vec<change_view::Line>),
    FileSummary(Vec<change_view::Line>, Vec<super::file_review::File>),
    /// A tool result with hidden lines: the short form to show, and the full
    /// one when the terminal reports a click (Shift-drag still lets the
    /// terminal select text). Ctrl+O lists the full
    /// text without a click.
    Expandable(Vec<change_view::Line>, Vec<change_view::Line>),
    /// A call started: its header, shown as `● Bash(ls)  ⎿  Running…`, and
    /// whether it runs a shell command.
    ToolStarted(String, change_view::Line, bool),
    /// A call finished: its result replaces the running form in one frame,
    /// with the full form a click shows when there is one.
    Finished(String, Vec<change_view::Line>, Option<Vec<change_view::Line>>),
    /// The session's working directory, shown in the footer.
    Workspace(String),
    ShellContext(std::path::PathBuf, String),
    /// The model and reasoning effort, shown in the footer.
    ModelLabel(String),
    Activity(String),
    HookActivity(Option<String>),
    Usage(Value),
    Question(String, oneshot::Sender<String>),
    Choice(Vec<change_view::Line>, String, Vec<String>, Option<String>, oneshot::Sender<String>),
    QuestionChoice(String, Vec<String>, Vec<Vec<change_view::Line>>, bool, oneshot::Sender<String>),
    Detail(String, String, String),
    Thinking(String),
    /// A read-only call joins, finishes or leaves the folded summary line.
    Group(String, Gathering, String, GroupStep),
    /// Other tool output follows; the summary line is final.
    GroupEnd,
    /// Messages sent during the turn that the model has not read yet.
    Queued(Vec<String>),
    /// Background shells still running when the turn ends.
    Shells(usize),
    /// Background agents still running when the turn ends.
    Agents(usize),
    AgentProgress(String, String, String),
    /// The todo list ctrl+t shows, as (content, status).
    Todos(Vec<(String, String)>),
    /// The conversation before this turn, for the Ctrl+O transcript.
    Transcript(Vec<change_view::Line>),
    PinnedPrompt(String),
    /// A queued message the model has now read.
    QueuedDelivered(String),
    SetDraft(String),
    Stop(Option<String>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupStep {
    Start,
    Done,
    Failed,
}
/// Consecutive read-only calls shown as one line, as in the reference CLI's
/// "Searched for 2 patterns". File reads keep their own named tool blocks.
struct Group {
    calls: Vec<(String, Gathering, bool)>,
    /// The latest call, shown under the line while the group grows.
    hint: String,
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
        *crate::terminal::TURN_COMPLETION.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // The first frame clears from the cursor down, over any input box the
        // previous turn left for the prompt.
        crate::terminal::take_handoff();
        let (updates, receiver) = mpsc::channel();
        let (actions, action_receiver) = async_mpsc::unbounded_channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let mode = mode.to_owned();
        let worker = thread::spawn(move || {
            let result = terminal::enable_raw_mode()
                // Capture tool disclosure clicks and drag selection.
                .and_then(|_| execute!(io::stderr(), EnableBracketedPaste, EnableMouseCapture));
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
            let mut screen = Frame { scrollback: super::scrollback::Scrollback::take(), ..Frame::default() };
            let _ = run(&mut state, &mut screen, receiver, actions);
            let _ = screen.clear();
            // The prompt that follows starts right below these lines.
            std::mem::take(&mut screen.scrollback).save();
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
    fn group(&self, id: &str, kind: Gathering, label: String, step: GroupStep) {
        let _ = self.updates.send(Update::Group(id.into(), kind, label, step));
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
    fn tool_started(&self, id: &str, header: change_view::Line, shell: bool) {
        let _ = self.updates.send(Update::ToolStarted(id.into(), header, shell));
    }
    fn finished(&self, id: &str, short: Vec<change_view::Line>, full: Option<Vec<change_view::Line>>) {
        let _ = self.updates.send(Update::Finished(id.into(), short, full));
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
        let _ = self.updates.send(Update::Choice(lines, question, options, None, send));
        receive
    }
    /// An AskUserQuestion menu; the reply is the option number, or `N\ntext`
    /// for a typed answer on the last option.
    pub fn question_choice(&self, question: String, options: Vec<String>, previews: Vec<Vec<change_view::Line>>, multi: bool) -> oneshot::Receiver<String> {
        let (send, receive) = oneshot::channel();
        let _ = self.updates.send(Update::QuestionChoice(question, options, previews, multi, send));
        receive
    }
    /// A plan review menu. Answers: the option number, `N\ntext` with typed
    /// changes, `esc` to reject, or `edit` after ctrl+g.
    pub fn plan_choice(&self, lines: Vec<change_view::Line>, question: String, options: Vec<String>, footer: String) -> oneshot::Receiver<String> {
        let (send, receive) = oneshot::channel();
        let _ = self.updates.send(Update::Choice(lines, question, options, Some(footer), send));
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
    pasted: bool,
    paste_end: usize,
    paste_start: usize,
    characters: Vec<char>,
    cursor: usize,
}
impl Draft {
    fn insert_paste(&mut self, text: &str) -> bool {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let count = normalized.chars().filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t')).count();
        if self.characters.len().saturating_add(count) > 32_768 { return false; }
        self.paste_start = self.cursor;
        self.insert(&normalized);
        self.pasted = true;
        self.paste_end = self.cursor;
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
        self.pasted = false;
        self.cursor = 0;
        std::mem::take(&mut self.characters).into_iter().collect()
    }
    fn set(&mut self, text: &str) {
        self.characters.clear();
        self.cursor = 0;
        self.insert(text);
    }
    fn visible(&self, width: usize) -> (String, usize) {
        if self.pasted {
            if let Some(label) = super::paste_input::paste_label(&self.characters.iter().skip(self.paste_start).take(self.paste_end.saturating_sub(self.paste_start)).collect::<String>()) {
                let text = clip(&format!("{}{}{}", self.characters.iter().take(self.paste_start).collect::<String>(), label, self.characters.iter().skip(self.paste_end).collect::<String>()), width);
                return (text.clone(), text.chars().count());
            }
        }
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
    /// What is being approved, drawn in the frame and erased once answered,
    /// as in the reference CLI.
    preview: Vec<change_view::Line>,
    expanded: bool,
    preview_offset: usize,
    question: String,
    options: Vec<String>,
    selected: usize,
    /// Text typed after Tab on Yes or No, sent with the answer.
    amend: Option<String>,
    /// A plan review: the last choice takes typed changes, and the footer
    /// names the plan file instead of the key hint.
    plan_footer: Option<String>,
    /// An AskUserQuestion menu: each option's preview, shown while it is
    /// selected; the last option takes a typed answer.
    option_previews: Option<Vec<Vec<change_view::Line>>>,
    /// A multiSelect question: which options are checked.
    checked: Option<Vec<bool>>,
}
impl Choice {
    fn command_preview(&self) -> bool {
        self.preview.first().is_some_and(|line| matches!(change_view::plain(std::slice::from_ref(line)).trim(), "Bash command" | "PowerShell command"))
    }
    fn click_preview(&mut self, row: usize, total: usize) -> bool {
        if !self.command_preview() || row < 3 || row >= total.saturating_sub(self.options.len() + 2) {
            return false;
        }
        self.expanded = !self.expanded;
        self.preview_offset = 0;
        true
    }
    fn preview_rows(&self, width: usize, color: bool) -> Vec<String> {
        self.preview.iter().flat_map(|line| {
            let text = change_view::render(line, if self.expanded { usize::MAX } else { width }, color);
            if self.expanded { super::markdown::wrap_styled(&text, "", "", width.max(1)) } else { vec![text] }
        }).collect()
    }
    fn preview_room(&self, height: usize) -> usize {
        height.saturating_sub(3 + self.options.len() + 4).max(1)
    }
    fn scroll_preview(&mut self, up: bool, width: usize, height: usize) {
        let end = self.preview_rows(width, false).len().saturating_sub(self.preview_room(height));
        let current = self.preview_offset.min(end);
        self.preview_offset = if up { current.saturating_sub(1) } else { current.saturating_add(1).min(end) };
    }

    fn amendable(&self) -> bool {
        if self.plan_footer.is_some() || self.option_previews.is_some() {
            return self.selected + 1 == self.options.len();
        }
        self.selected == 0 || self.selected + 1 == self.options.len()
    }
    /// An option's label; while amending, `Yes, use -5 instead` or the
    /// reference CLI's placeholder.
    fn label(&self, index: usize) -> String {
        let option = terminal_text(&self.options[index]);
        match &self.amend {
            Some(text) if index == self.selected && (self.plan_footer.is_some() || self.option_previews.is_some()) => {
                if text.is_empty() { option } else { terminal_text(text) }
            }
            Some(text) if index == self.selected => {
                let (word, placeholder) = if index == 0 {
                    ("Yes", "and tell DeepSeek Code what to do next")
                } else {
                    ("No", "and tell DeepSeek Code what to do differently")
                };
                format!("{word}, {}", if text.is_empty() { placeholder.to_owned() } else { terminal_text(text) })
            }
            _ => match &self.checked {
                // The typed-answer entry has no box.
                Some(checked) if index + 1 < self.options.len() => format!("[{}] {option}", if checked[index] { "x" } else { " " }),
                _ => option,
            },
        }
    }
}

struct State {
    goal_proposal: Option<(String, String)>,
    mode: String,
    /// The model and effort, then the session's working directory, at the
    /// right of the footer.
    model_label: String,
    workspace: String,
    shell_context: Option<(std::path::PathBuf, String)>,
    shell_expanded: bool,
    /// The highlighted `/` command, and whether Esc closed the menu.
    menu_selected: usize,
    menu_hidden: bool,
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
    group: Option<Group>,
    /// Calls that have started and not finished, in the order they started.
    running: Vec<Running>,
    /// Messages typed during the turn, shown until the model reads them.
    queued: Vec<String>,
    transcript: Vec<change_view::Line>,
    shells: usize,
    /// When ctrl+x was pressed, for the ctrl+x ctrl+s chord.
    chord: Option<Instant>,
    todos: Vec<(String, String)>,
    agents: usize,
    foreground_agents: Vec<(String, String)>,
    agent_panel: Vec<crate::background_commands::Agent>,
    /// Draft states before each edit, for ctrl+_.
    undo: Vec<(Vec<char>, usize)>,
    /// When reasoning began and last streamed, for "Thought for 2s".
    thinking_span: Option<(Instant, Instant)>,
    thinking_characters: usize,
    thinking_entry: Option<usize>,
    /// The current reasoning, for the live `● Thinking for 2s…` preview.
    thinking_preview: String,
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

/// A call in progress, drawn where its result will be written.
struct Running {
    id: String,
    header: change_view::Line,
    started: Instant,
    shell: bool,
}

/// Rows of reasoning shown under `● Thinking for 2s…` while it streams.
const THINKING_PREVIEW_ROWS: usize = 2;

/// Commands `submit` accepts while the model works.
const LIVE_COMMANDS: &[&str] = &["/btw", "/background", "/goal", "/status", "/tasks"];

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
            model_label: String::new(),
            workspace: String::new(),
            shell_context: None,
            shell_expanded: false,
            menu_selected: 0,
            menu_hidden: false,
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
            group: None,
            running: Vec::new(),
            queued: Vec::new(),
            transcript: Vec::new(),
            shells: 0,
            chord: None,
            todos: Vec::new(),
            agents: 0,
            foreground_agents: Vec::new(),
            agent_panel: Vec::new(),
            undo: Vec::new(),
            thinking_span: None,
            thinking_characters: 0,
            thinking_entry: None,
            thinking_preview: String::new(),
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
    /// The `/` menu while the model works: only the commands that run
    /// during a turn, as (name, description).
    fn menu(&self) -> Vec<(String, String)> {
        if self.menu_hidden || self.answer.is_some() || self.choice.is_some() {
            return Vec::new();
        }
        let commands: Vec<(String, String)> = crate::command_catalog::COMMANDS
            .iter()
            .filter(|(name, _)| LIVE_COMMANDS.contains(name))
            .map(|(name, description)| (name.to_string(), description.to_string()))
            .collect();
        let text: String = self.draft.characters.iter().collect();
        super::input::editor::matches(&commands, &text).into_iter().map(|index| commands[index].clone()).collect()
    }
    /// Menu keys: arrows move, Tab or Enter fills in the command. True when
    /// the key was one of them.
    fn menu_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let items = self.menu();
        if items.is_empty() || key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return false;
        }
        let selected = self.menu_selected.min(items.len() - 1);
        let text: String = self.draft.characters.iter().collect();
        match key.code {
            KeyCode::Up => self.menu_selected = (selected + items.len() - 1) % items.len(),
            KeyCode::Down => self.menu_selected = (selected + 1) % items.len(),
            KeyCode::Esc => self.menu_hidden = true,
            KeyCode::Tab => self.draft.set(&format!("{} ", items[selected].0)),
            KeyCode::Enter if text != items[selected].0 => self.draft.set(&format!("{} ", items[selected].0)),
            _ => return false,
        }
        true
    }
    /// The activity line: `✢ Brewing… (4m 13s · ↓ 13.7k tokens · thinking)`.
    fn status_line(&self) -> String {
        let label = if self.answer.is_some() {
            "Waiting for your answer".to_owned()
        } else if let Some(hook) = &self.hook_activity {
            hook.clone()
        } else if self.activity.starts_with("Running Bash") || (self.activity.starts_with("Running") && !self.running.is_empty()) {
            // The call itself shows above, in its transcript form.
            "Working".to_owned()
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
    fn choice_key(&mut self, mut key: crossterm::event::KeyEvent) -> bool {
        let Some(choice) = self.choice.as_mut() else {
            return false;
        };
        if crate::keybindings::is_cycle_permission(key) {
            key.code = KeyCode::BackTab;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = choice.options.len();
        let plan = choice.plan_footer.is_some();
        if plan && control && key.code == KeyCode::Char('g') {
            // ctrl+g: the caller opens the plan file in an editor.
            if let Some(answer) = self.answer.take() {
                let _ = answer.send("edit".into());
            }
            self.choice = None;
            self.draft = self.saved_draft.take().unwrap_or_default();
            return true;
        }
        if let Some(text) = choice.amend.as_mut() {
            // As in the reference CLI, Tab on Yes or No turns the option into a
            // text field; the note goes to the model with the answer.
            match key.code {
                KeyCode::Char('c' | 'o' | 'b' | 'l') if control => return false,
                // A plan's typed changes: shift+tab approves with them as a note.
                KeyCode::BackTab if plan && !text.trim().is_empty() => {
                    let note = text.trim().to_owned();
                    if let Some(answer) = self.answer.take() {
                        let _ = answer.send(format!("2\n{note}"));
                    }
                    self.choice = None;
                    self.draft = self.saved_draft.take().unwrap_or_default();
                }
                KeyCode::Enter if plan && text.trim().is_empty() => {}
                KeyCode::Esc | KeyCode::Tab => choice.amend = None,
                KeyCode::Enter => self.answer_choice(),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(ch) if !control => text.push(ch),
                KeyCode::Up => {
                    choice.amend = None;
                    choice.selected = (choice.selected + count - 1) % count;
                }
                KeyCode::Down => {
                    choice.amend = None;
                    choice.selected = (choice.selected + 1) % count;
                }
                _ => {}
            }
            return true;
        }
        let question = choice.option_previews.is_some();
        // multiSelect: space or a number toggles an option, enter submits.
        if let (Some(checked), None, false) = (choice.checked.as_mut(), choice.amend.as_ref(), control) {
            match key.code {
                KeyCode::Char(' ') if choice.selected + 1 < count => {
                    checked[choice.selected] = !checked[choice.selected];
                    return true;
                }
                KeyCode::Char(ch @ '1'..='9') if (ch as usize - '1' as usize) + 1 < count => {
                    let index = ch as usize - '1' as usize;
                    checked[index] = !checked[index];
                    choice.selected = index;
                    return true;
                }
                KeyCode::Enter if choice.selected + 1 < count => {
                    self.answer_choice();
                    return true;
                }
                _ => {}
            }
        }
        if (plan || question) && choice.selected + 1 == count && !control {
            // The last choice of a plan or question is a text field: typing starts it.
            if let KeyCode::Char(ch) = key.code {
                choice.amend = Some(ch.to_string());
                return true;
            }
            if key.code == KeyCode::Enter {
                return true;
            }
        }
        match key.code {
            // shift+tab picks the choice labelled with it, such as "allow all
            // edits during this session (shift+tab)".
            KeyCode::BackTab => {
                if let Some(index) = choice.options.iter().position(|option| option.contains("(shift+tab)")) {
                    choice.selected = index;
                    self.answer_choice();
                }
            }
            // A rejected plan stops the turn, as in the reference CLI; a
            // dismissed question tells the model there is no answer.
            KeyCode::Esc if plan || question => {
                if let Some(answer) = self.answer.take() {
                    let _ = answer.send("esc".into());
                }
                self.choice = None;
                self.draft = self.saved_draft.take().unwrap_or_default();
            }
            // As in the reference CLI, Esc cancels the call: a bare No, which
            // also stops the turn.
            KeyCode::Esc => {
                choice.selected = count - 1;
                self.answer_choice();
            }
            KeyCode::Char('c' | 'o' | 'b' | 'l') if control => return false,
            KeyCode::Up | KeyCode::Char('k') => choice.selected = (choice.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') => choice.selected = (choice.selected + 1) % count,
            KeyCode::Tab => {
                if choice.amendable() {
                    choice.amend = Some(String::new());
                }
            }
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
            // multiSelect: `m:` and the checked numbers, then any typed answer.
            let number = match &choice.checked {
                Some(checked) => format!("m:{}", checked.iter().enumerate().filter(|(_, on)| **on).map(|(index, _)| (index + 1).to_string()).collect::<Vec<_>>().join(",")),
                None => (choice.selected + 1).to_string(),
            };
            let _ = answer.send(match choice.amend.as_deref().map(str::trim) {
                Some(note) if !note.is_empty() => format!("{number}\n{note}"),
                _ => number,
            });
        }
        self.draft = self.saved_draft.take().unwrap_or_default();
    }
    fn queue_message(&mut self, actions: &async_mpsc::UnboundedSender<Action>) -> bool {
        if self.answer.is_some() || self.draft.characters.iter().all(|ch| ch.is_whitespace()) { return false; }
        let text = self.draft.take();
        self.undo.clear();
        self.queued.push(text.clone());
        let _ = actions.send(Action::Queue(text));
        true
    }
    fn submit(&mut self, actions: &async_mpsc::UnboundedSender<Action>) -> Option<String> {
        let text = self.draft.take();
        self.undo.clear();
        if self.answer.is_none() {
            // A stashed draft comes back at the next prompt.
            let mut stash = crate::terminal::stash();
            stash.1 = stash.0.is_some();
        }
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
        // Status reads state only, so it answers now instead of after the turn.
        if crate::session_commands::is_live_inspection(&text) {
            let _ = actions.send(Action::Submit(text));
            return None;
        }
        if text.trim().starts_with('/') && !(self.goal_proposal.is_some() && self.activity.starts_with("Task complete")) {
            return Some("While working, use /btw QUESTION, /status, /tasks, /tasks show ID or type a prompt to queue; other commands are available after completion.".into());
        }
        let _ = actions.send(Action::Submit(text.clone()));
        // Plain text waits under the work until the model reads it.
        if crate::session::queues_mid_turn(&text) {
            return None;
        }
        Some(format!("\n{text}\n\nPrompt queued for after this turn."))
    }
}
/// The prompt's editing shortcuts in the input during a turn, as in the
/// reference CLI: Ctrl+A/E, Ctrl+U/K and Ctrl+Y, ctrl+_ undo, `\` + Enter,
/// ctrl+g and ctrl+s. True when the key was one of them.
fn edit_draft(state: &mut State, screen: &mut Frame, key: crossterm::event::KeyEvent) -> io::Result<bool> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let before = (state.draft.characters.clone(), state.draft.cursor);
    match key.code {
        KeyCode::Char('a') if control => state.draft.cursor = 0,
        KeyCode::Char('e') if control => state.draft.cursor = state.draft.characters.len(),
        KeyCode::Char('u') if control => {
            let cut: String = state.draft.characters.drain(..state.draft.cursor).collect();
            state.draft.cursor = 0;
            if !cut.is_empty() {
                *crate::terminal::killed() = cut;
            }
        }
        KeyCode::Char('k') if control => {
            let cut: String = state.draft.characters.drain(state.draft.cursor..).collect();
            if !cut.is_empty() {
                *crate::terminal::killed() = cut;
            }
        }
        KeyCode::Char('y') if control => {
            let killed = crate::terminal::killed().clone();
            state.draft.insert(&killed);
        }
        KeyCode::Char('\x1f') | KeyCode::Char('_' | '7' | '/' | '-') if control || key.code == KeyCode::Char('\x1f') => {
            if let Some((characters, cursor)) = state.undo.pop() {
                state.draft.characters = characters;
                state.draft.cursor = cursor.min(state.draft.characters.len());
            }
            return Ok(true);
        }
        KeyCode::Char('s') if control => {
            let mut stash = crate::terminal::stash();
            if state.draft.characters.is_empty() {
                if let Some(text) = stash.0.take() {
                    state.draft.set(&text);
                }
            } else {
                *stash = (Some(state.draft.take()), false);
            }
        }
        KeyCode::Char('g') if control => {
            screen.clear()?;
            let edited = super::input::editor::edit_externally(&state.draft.characters.iter().collect::<String>())?;
            state.draft.set(&edited);
            screen.size = (0, 0);
        }
        KeyCode::Enter
            if key.modifiers.is_empty()
                && state.draft.cursor == state.draft.characters.len()
                && state.draft.characters.last() == Some(&'\\') =>
        {
            state.draft.characters.pop();
            state.draft.cursor = state.draft.characters.len();
            state.draft.insert("\n");
        }
        _ => return Ok(false),
    }
    if state.draft.characters != before.0 {
        state.undo.push(before);
    }
    Ok(true)
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
    (height as usize).saturating_sub(10).clamp(2, 8)
}
/// Calls in progress in their transcript form: `● #2 Bash(ls)` over
/// `⎿  Running…`. A shell command adds the Ctrl+B hint and, once clicked,
/// its latest output. Returns the rows and those of the running shell call.
fn running_lines(state: &State, width: usize, budget: usize, color: bool) -> (Vec<String>, Option<std::ops::Range<usize>>) {
    let mut rows = Vec::new();
    let mut shell = None;
    let foreground = state.activity.starts_with("Running Bash");
    for (index, call) in state.running.iter().enumerate() {
        if index > 0 {
            rows.push(String::new());
        }
        let first = rows.len();
        rows.push(change_view::render(&call.header, width, color));
        let elapsed = call.started.elapsed().as_secs();
        let detail = if elapsed >= 2 { format!(" ({})", short_duration(elapsed)) } else { String::new() };
        rows.push(change_view::render(&change_view::running(&detail), width, color));
        if !(call.shell && foreground && index + 1 == state.running.len()) {
            continue;
        }
        if state.shell_expanded {
            let output = state.shell_context.as_ref().map(|(root, session)| {
                deepseek_code_core::tools::background_shell::foreground_output(root, session)
                    .unwrap_or_else(|error| format!("Could not read output: {error}"))
            }).unwrap_or_default();
            let output = terminal_text(&output);
            let lines: Vec<_> = output.trim_end().lines().map(|row| clip(row, width.saturating_sub(5))).collect();
            let room = budget.saturating_sub(rows.len() + 2);
            if lines.is_empty() {
                rows.push("     Waiting for command output\u{2026}".into());
            } else {
                rows.extend(lines.iter().skip(lines.len().saturating_sub(room)).map(|row| format!("     {row}")));
            }
        }
        // A long command can continue in the background, as in the reference CLI.
        if call.started.elapsed() >= Duration::from_secs(3) {
            let hint = if state.shell_expanded { "     Ctrl+B to run in background \u{b7} click to hide output" } else { "     Ctrl+B to run in background \u{b7} click to show output" };
            rows.push(if color { change_view::render(&change_view::muted(hint), width, true) } else { clip(hint, width) });
        }
        shell = Some(first..rows.len());
    }
    (rows, shell)
}
/// Return complete display rows with their original UTF-8 byte endpoints.
/// Rows break after the last space that fits; a space reaching the edge
/// starts the next row, and a word longer than a row breaks at the edge.
fn visual_lines(text: &str, width: usize) -> Vec<(String, usize)> {
    let width = width.max(1);
    let mut result = Vec::new();
    let mut start = 0;
    let mut cells = 0;
    // Byte offset just after the row's last space and the cells up to it.
    let mut space: Option<(usize, usize)> = None;
    for (index, ch) in text.char_indices() {
        let size = ch.width().unwrap_or(0);
        if cells + size > width && index > start {
            let (at, before) = match space.take() {
                Some(split) if ch != ' ' => split,
                _ => (index, cells),
            };
            result.push((text[start..at].to_owned(), at));
            cells -= before;
            start = at;
        }
        cells += size;
        if ch == ' ' {
            space = Some((index + 1, cells));
        }
    }
    result.push((text[start..].to_owned(), text.len()));
    result
}
/// The answer prefix of a streamed row: `● ` opens a block, later rows indent.
fn answer_prefix(state: &State, row: usize) -> &'static str {
    if row == 0 && state.block_start { "\u{25cf} " } else { "  " }
}
fn streamed_rows(state: &State, width: usize, budget: usize) -> Vec<String> {
    let rows = visual_lines(&state.partial, width.saturating_sub(2));
    let first = rows.len().saturating_sub(budget);
    rows.into_iter().enumerate().skip(first)
        .map(|(index, (line, _))| if line.is_empty() { line } else { format!("{}{line}", answer_prefix(state, index)) })
        .collect()
}

fn stream_text(state: &mut State, screen: &mut Frame, text: &str) {
    finish_group(state, screen);
    if state.thinking_span.is_some() {
        finish_answer(state, screen);
        finish_thought(state, screen);
    }
    if state.block_start && state.partial.is_empty() && !text.is_empty() { screen.separate_block(); }
    state.generated_characters = state.generated_characters.saturating_add(text.chars().count());
    state.estimate_pending = true;
    if state.activity.contains("thinking") || state.activity == "Waiting for response" {
        state.activity = "Composing".into();
        state.activity_started = Instant::now();
    }
    state.partial.push_str(&terminal_text(text).replace("\r\n", "\n"));
    while let Some(index) = state.partial.find('\n') {
        let line = state.partial[..index].to_owned();
        state.partial.drain(..index + 1);
        emit_text_line(state, screen, &line);
    }
}

/// Logs one finished answer line through the Markdown renderer. The first
/// line of an answer block gets the reference CLI's `●`, later lines indent.
fn emit_text_line(state: &mut State, screen: &mut Frame, line: &str) {
    state.markdown.set_width(terminal::size().map_or(80, |(cols, _)| cols as usize).saturating_sub(3));
    for rendered in state.markdown.line(line) {
        log_answer_line(state, screen, &rendered);
    }
}

fn log_answer_line(state: &mut State, screen: &mut Frame, rendered: &str) {
    if rendered.is_empty() {
        screen.log_rendered("");
        return;
    }
    if state.block_start && screen.scrollback.needs_separator() { screen.log_rendered(""); }
    let first = if std::mem::take(&mut state.block_start) { "\u{25cf} " } else { "  " };
    // Wrap at word boundaries with the answer's indent instead of letting the
    // terminal break words at the edge.
    let width = terminal::size().map_or(80, |(cols, _)| cols as usize).saturating_sub(1);
    let indent = " ".repeat(2 + super::markdown::hanging_indent(rendered));
    for row in super::markdown::wrap_styled(rendered, first, &indent, width) {
        screen.log_rendered(&row);
    }
}

/// Writes the folded read-only calls as their final summary line.
fn finish_group(state: &mut State, screen: &mut Frame) {
    let Some(group) = state.group.take() else {
        return;
    };
    if group.calls.is_empty() {
        return;
    }
    screen.separate_block();
    let kinds: Vec<Gathering> = group.calls.iter().map(|call| call.1).collect();
    let summary = super::tool_view::gathering_summary(&kinds, false);
    let line = format!("  {}", capitalized(&summary));
    if std::env::var_os("NO_COLOR").is_some() {
        let _ = screen.log(&line);
    } else {
        screen.log_rendered(&change_view::render(&change_view::muted(&line), usize::MAX, true));
    }
}
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}
/// Reasoning while it streams, as in the reference CLI: `● Thinking for 2s…`
/// and the latest reasoning as one paragraph under `⎿`.
fn thinking_lines(state: &State, width: usize, budget: usize) -> Vec<String> {
    let seconds = state.thinking_span.map_or(0, |(first, _)| first.elapsed().as_secs()).max(1);
    let mut lines = vec![format!("\u{25cf} Thinking for {seconds}s\u{2026}")];
    let text = terminal_text(&state.thinking_preview).split_whitespace().collect::<Vec<_>>().join(" ");
    let rows: Vec<String> = visual_lines(&text, width.saturating_sub(5).max(1)).into_iter().map(|(row, _)| row.trim().to_owned()).collect();
    let skip = rows.len().saturating_sub(budget.saturating_sub(1).max(1));
    for (index, row) in rows.into_iter().skip(skip).enumerate() {
        let prefix = if index == 0 { "  \u{23bf}  " } else { "     " };
        lines.push(format!("{prefix}{row}"));
    }
    lines
}
/// The folded calls while they run: the summary and the latest call.
fn group_lines(group: &Group, width: usize) -> Vec<String> {
    let kinds: Vec<Gathering> = group.calls.iter().map(|call| call.1).collect();
    let running = group.calls.iter().any(|call| !call.2);
    let summary = capitalized(&super::tool_view::gathering_summary(&kinds, running));
    vec![
        clip(&format!("  {summary}"), width),
        clip(&format!("  \u{23bf}  {}", group.hint), width),
    ]
}
fn group_call(state: &mut State, id: String, kind: Gathering, label: String, step: GroupStep) {
    if step == GroupStep::Failed {
        // A failed call is printed on its own with its error.
        if let Some(group) = &mut state.group {
            group.calls.retain(|call| call.0 != id);
        }
        return;
    }
    let group = state.group.get_or_insert_with(|| Group {
        calls: Vec::new(),
        hint: String::new(),
    });
    let done = step == GroupStep::Done;
    match group.calls.iter_mut().find(|call| call.0 == id) {
        Some(call) => call.2 |= done,
        None => group.calls.push((id, kind, done)),
    }
    group.hint = label;
}

/// Ends the current answer block before tool output, notices or the footer.
fn finish_text(state: &mut State, screen: &mut Frame) {
    finish_group(state, screen);
    finish_answer(state, screen);
    finish_thought(state, screen);
}
/// Clear reasoning timing without adding a completed-thought transcript row.
fn finish_thought(state: &mut State, screen: &mut Frame) {
    let Some((first, last)) = state.thinking_span.take() else { return; };
    let text = std::mem::take(&mut state.thinking_preview);
    if text.trim().is_empty() { return; }
    let (short, full) = super::tool_view::thought(&text, Some(last.duration_since(first).as_secs().max(1)));
    screen.log_block(short, full);
}
/// Writes the streamed answer so far; the text before folded calls keeps its
/// own block, as in the reference CLI.
fn finish_answer(state: &mut State, screen: &mut Frame) {
    if !state.partial.is_empty() {
        let line = std::mem::take(&mut state.partial);
        emit_text_line(state, screen, &line);
    }
    state.markdown.set_width(terminal::size().map_or(80, |(cols, _)| cols as usize).saturating_sub(3));
    for rendered in state.markdown.finish() {
        log_answer_line(state, screen, &rendered);
    }
    state.block_start = true;
}

#[derive(Default)]
struct Frame {
    pinned: super::pinned_prompt::PinnedPrompt,
    lines: Vec<String>,
    /// Frame row of the cursor: the input line, or the selected menu option.
    input_row: usize,
    cursor_column: usize,
    size: (u16, u16),
    panel_toggle: Option<super::input::editor::HitTarget>,
    panel_height: usize,
    panel_viewport: usize,
    /// Output that must precede the next frame, such as clearing the rows
    /// below a toggled tool result.
    pending: String,
    /// Transcript lines not written yet, with their screen rows. They go out
    /// with the next frame in one write, so the terminal never shows a blank
    /// frame.
    logs: Vec<(String, usize)>,
    /// Screen row of the frame's first line; the transcript ends right above.
    top: Option<u16>,
    /// Blank rows between the live content and the status line. The content
    /// stays right below the transcript, where its finished form is written,
    /// while the input box keeps the bottom rows.
    padding: usize,
    /// Whether the frame ends with the input box, rather than a menu question.
    input_box: bool,
    /// Whether the turn's first frame has moved a short transcript down onto it.
    aligned: bool,
    /// Frame rows of the running Bash call; a click shows its output.
    shell_rows: Option<std::ops::Range<usize>>,
    /// Lines logged above the frame, for clicks on tool results.
    scrollback: super::scrollback::Scrollback,
    logged: Vec<String>,
    history_view: super::transcript_pager::HistoryView,
    composer_height: usize,
}
impl Frame {
    fn detailed_log(&self, state: &State, width: usize, color: bool) -> Vec<String> {
        let mut logged = self.logged.clone();
        for (_, title, body) in &state.detail_entries {
            if logged.last().is_some_and(|row| !super::markdown::plain_rendered(row).trim().is_empty()) { logged.push(String::new()); }
            logged.push(change_view::render(&change_view::heading(&format!("\u{25cf} {title}")), width, color));
            logged.extend(body.lines().map(|line| change_view::render(&change_view::body(&format!("  {line}")), width, color)));
        }
        if !state.partial.is_empty() {
            if logged.last().is_some_and(|row| !super::markdown::plain_rendered(row).trim().is_empty()) { logged.push(String::new()); }
            logged.extend(streamed_rows(state, width, usize::MAX));
        }
        logged
    }

    fn refresh_history(&mut self, state: &State, width: usize) {
        self.history_view.refresh(&[], &self.scrollback.presentation(), width);
        if state.activity.starts_with("Running Bash") {
            self.history_view.append(&format!("\n{}", state.activity), width);
        }
        if !state.queued.is_empty() {
            self.history_view.append(&format!("\n{}\n", state.queued.join("\n")), width);
        }
        if !state.partial.is_empty() {
            self.history_view.append(&format!("{}{}", answer_prefix(state, 0), terminal_text(&state.partial)), width);
        } else if let Some(group) = &state.group {
            for line in group_lines(group, width) {
                self.history_view.append(&line, width);
            }
        }
    }

    fn history_key(&mut self, state: &State, key: crossterm::event::KeyEvent) -> io::Result<bool> {
        if state.details.expanded && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            return Ok(false);
        }
        if !super::transcript_pager::HistoryView::is_navigation(key) {
            return Ok(false);
        }
        let size = terminal::size().unwrap_or((80, 24));
        self.refresh_history(state, size.0.saturating_sub(1) as usize);
        self.history_view.navigate(key, (size.1 as usize).saturating_sub(self.composer_height))
    }

    /// Writes the queued transcript lines from the frame's first row, after
    /// clearing the old frame, and moves the frame below them.
    fn write_logs(&mut self, out: &mut String, rows: usize) {
        let Some(top) = self.top else { return; };
        if self.logs.is_empty() {
            return;
        }
        out.push_str(&format!("\x1b[{};1H\x1b[J", top + 1));
        let mut row = top as usize;
        for (text, height) in self.logs.drain(..) {
            out.push_str(&text);
            out.push_str("\r\n");
            row += height;
        }
        // Lines written past the bottom row scrolled the screen.
        self.top = Some(row.min(rows.saturating_sub(1)) as u16);
        self.lines.clear();
    }
    /// Writes anything queued and removes the frame from the screen.
    fn clear(&mut self) -> io::Result<()> {
        self.history_view.close()?;
        let rows = terminal::size().map_or(24, |(_, rows)| rows as usize).max(1);
        let mut out = std::mem::take(&mut self.pending);
        if self.top.is_none() && !self.logs.is_empty() {
            self.top = Some(crossterm::cursor::position().map_or(0, |(_, row)| row));
        }
        self.write_logs(&mut out, rows);
        if let (Some(top), false) = (self.top, self.lines.is_empty()) {
            out.push_str(&format!("\x1b[{};1H\x1b[J", top + 1));
        }
        self.lines.clear();
        if out.is_empty() {
            return Ok(());
        }
        write(&out)
    }
    /// Ends the turn in the idle prompt's layout: the closing lines join the
    /// transcript and the input box stays on its rows, so the prompt that
    /// follows takes over without the box moving or blinking.
    fn hand_off(&mut self) -> io::Result<()> {
        self.history_view.close()?;
        let rows = terminal::size().map_or(24, |(_, rows)| rows as usize).max(1);
        match self.hand_off_output(rows) {
            Some((out, row)) => {
                write(&out)?;
                crate::terminal::hand_off(row);
                Ok(())
            }
            None => self.clear(),
        }
    }
    /// The hand-off output and the row the prompt continues on.
    fn hand_off_output(&mut self, rows: usize) -> Option<(String, u16)> {
        if !self.input_box || self.lines.len() < 4 || self.top.is_none() || rows < 8 {
            return None;
        }
        let input_box = self.lines[self.lines.len() - 4..].to_vec();
        let mut out = std::mem::take(&mut self.pending);
        self.write_logs(&mut out, rows);
        let mut top = self.top.unwrap_or(0) as usize;
        // The prompt writes a separator row and its own blank row above the box.
        let height = 2 + input_box.len();
        if top + height > rows {
            out.push_str(&format!("\x1b[{rows};1H"));
            for _ in 0..top + height - rows {
                out.push_str("\r\n");
            }
            top = rows - height;
        }
        for row in top..rows - input_box.len() {
            out.push_str(&format!("\x1b[{};1H\x1b[K", row + 1));
        }
        for (index, line) in input_box.iter().enumerate() {
            out.push_str(&format!("\x1b[{};1H{line}\x1b[K", rows - input_box.len() + index + 1));
        }
        out.push_str(&format!("\x1b[{};1H\x1b[?25h", top + 1));
        self.lines.clear();
        self.top = None;
        Some((format!("\x1b[?25l{out}"), top as u16))
    }
    /// Queues a plain line above the frame; the next draw writes both.
    fn log(&mut self, text: &str) -> io::Result<()> {
        self.log_line(terminal_text(text), None);
        Ok(())
    }
    /// Like `log` for lines that already carry trusted style escapes.
    fn log_rendered(&mut self, line: &str) {
        self.log_line(line.to_owned(), None);
    }
    fn log_line(&mut self, text: String, block: Option<usize>) {
        self.logged.push(text.clone());
        let height = self.scrollback.push(text.clone(), block);
        self.logs.push((text, height));
    }
    fn separate_block(&mut self) {
        if self.scrollback.needs_separator() { self.log_rendered(""); }
    }
    /// Logs a tool result that a click expands and collapses.
    fn log_block(&mut self, short: Vec<change_view::Line>, full: Vec<change_view::Line>) {
        self.separate_block();
        let (block, lines) = self.scrollback.block(short, full);
        for line in lines {
            self.log_line(line, Some(block));
        }
    }
    /// Switches a clicked tool result between its short and full form and
    /// redraws everything below it, the frame included.
    fn toggle_block(&mut self, row: u16) -> bool {
        let Some((start, lines)) = self.top.and_then(|top| self.scrollback.toggle(top, row)) else {
            return false;
        };
        self.logs.clear();
        self.pending = format!("\x1b[{};1H\x1b[J", start + 1);
        self.lines.clear();
        self.top = Some(start);
        for (line, block) in lines {
            self.log_line(line, block);
        }
        true
    }
    /// Places the frame right below the transcript and pads it to the bottom
    /// row: blank rows go between the content and the status line. Returns
    /// the output, or `None` when the screen already shows this frame.
    fn place(
        &mut self,
        mut lines: Vec<String>,
        content_rows: usize,
        mut input_row: usize,
        cursor_column: usize,
        size: (u16, u16),
        cursor_y: &mut dyn FnMut() -> usize,
    ) -> Option<String> {
        let rows = (size.1 as usize).max(1);
        let mut out = std::mem::take(&mut self.pending);
        if self.top.is_none() || self.size != size {
            // The first frame, a terminal reflow after a resize, or a
            // full-screen view in between: the cursor shows where we are.
            if self.size != (0, 0) && self.size != size {
                // Recompute hit targets without discarding conversation history.
                self.scrollback.reflow(size.0 as usize);
            }
            let cursor_y = cursor_y();
            let top = if self.lines.is_empty() { cursor_y } else { cursor_y.saturating_sub(self.input_row) };
            let top = top.min(rows - 1);
            out.push_str(&format!("\x1b[{};1H\x1b[J", top + 1));
            self.top = Some(top as u16);
            self.lines.clear();
        }
        // Scroll before writing the new rows. ConPTY paints what is pending
        // right before each scroll, so the first scroll comes before any
        // change and every later one after putting the status line and input
        // box back on their final rows: no flush shows them moved or missing.
        let log_rows: usize = self.logs.iter().map(|(_, height)| height).sum();
        if let Some(top) = self.top.map(usize::from) {
            let lift = (top + log_rows + lines.len()).saturating_sub(rows).min(top);
            let anchored = &lines[content_rows.min(lines.len())..];
            let anchored = &anchored[anchored.len().saturating_sub(rows)..];
            for step in 0..lift {
                // Only over rows the transcript has already scrolled out of.
                if step > 0 && rows - anchored.len() >= top - step {
                    for (index, line) in anchored.iter().enumerate() {
                        out.push_str(&format!("\x1b[{};1H{line}\x1b[K", rows - anchored.len() + index + 1));
                    }
                }
                out.push_str(&format!("\x1b[{rows};1H\r\n"));
            }
            if lift > 0 {
                self.top = Some((top - lift) as u16);
                self.lines.clear();
            }
        }
        self.write_logs(&mut out, rows);
        let mut top = self.top.unwrap_or(0) as usize;
        let mut repaint = self.lines.is_empty();
        if top + lines.len() > rows {
            // The frame grew past the bottom: the transcript scrolls up.
            out.push_str(&format!("\x1b[{rows};1H"));
            for _ in 0..top + lines.len() - rows {
                out.push_str("\r\n");
            }
            top = rows.saturating_sub(lines.len());
            repaint = true;
        }
        let mut padding = rows - top - lines.len().min(rows - top);
        if !std::mem::replace(&mut self.aligned, true) {
            // The turn's first frame: a short transcript moves down onto it,
            // as the idle prompt keeps it, so the turn ends without moving it.
            let scroll = super::pinned_prompt::scroll_transcript_down(top, padding);
            if !scroll.is_empty() {
                out.push_str(&scroll);
                top += padding;
                padding = 0;
                repaint = true;
            }
        }
        lines.splice(content_rows..content_rows, std::iter::repeat_n(String::new(), padding));
        input_row += padding;
        for (index, line) in lines.iter().enumerate() {
            if !repaint && self.lines.get(index) == Some(line) {
                continue;
            }
            out.push_str(&format!("\x1b[{};1H{line}\x1b[K", top + index + 1));
        }
        self.size = size;
        if out.is_empty() && self.input_row == input_row && self.cursor_column == cursor_column {
            return None;
        }
        out.push_str(&format!("\x1b[{};{}H\x1b[?25h", top + input_row + 1, cursor_column + 1));
        self.lines = lines;
        self.top = Some(top as u16);
        self.padding = padding;
        self.input_row = input_row;
        self.cursor_column = cursor_column;
        Some(format!("\x1b[?25l{out}"))
    }
    fn draw(&mut self, state: &mut State) -> io::Result<()> {
        let size = terminal::size().unwrap_or((80, 24));
        let width = (size.0 as usize).saturating_sub(1).max(4);
        let color = std::env::var_os("NO_COLOR").is_none();
        let prompt = if state.answer.is_some() { "Answer \u{203a} ".to_owned() } else { format!("{} ", crate::terminal::prompt_glyph()) };
        let prompt_width = prompt.chars().count();
        let (draft, cursor) = state.draft.visible(width.saturating_sub(prompt_width + 1));
        let status = clip(&state.status_line(), width);
        let indicator = crate::terminal::style::mode_indicator(&state.mode);
        let mode = format!("{} {}", crate::terminal::style::status_mode(&state.mode), state.model_label);
        let mode = if super::scrollback::compact() { format!("{mode} · Compact · Alt+M") } else { mode };
        let rule = "─".repeat(width);
        let budget = if state.shell_expanded { (size.1 as usize).saturating_sub(10).max(2) } else { content_budget(size.1) };
        let panel = state.details.render(width, budget);
        self.panel_toggle = panel.toggle;
        self.panel_height = panel.lines.len();
        self.panel_viewport = panel.viewport;
        self.shell_rows = None;
        // The live content flows right below the transcript: the reasoning
        // preview, the streaming answer and running calls in the form they
        // keep once finished, so nothing jumps when they join the transcript.
        let mut content: Vec<String> = if state.details.expanded {
            panel.lines.iter().map(|(text, _)| text.clone()).collect()
        } else {
            self.panel_toggle = None;
            self.panel_height = 0;
            // The reference CLI shows reasoning on its own, without the disclosure row.
            let thinking = state.partial.is_empty()
                && state.thinking_span.is_some()
                && state.group.as_ref().is_none_or(|group| group.calls.is_empty());
            let mut content = match &state.group {
                Some(group) if state.partial.is_empty() && !group.calls.is_empty() => group_lines(group, width),
                _ if thinking => thinking_lines(state, width, THINKING_PREVIEW_ROWS + 1),
                _ if state.partial.is_empty() => Vec::new(),
                _ => streamed_rows(state, width, budget),
            };
            let (running, shell) = running_lines(state, width, budget, color);
            if !running.is_empty() {
                if !content.is_empty() { content.push(String::new()); }
                self.shell_rows = shell.map(|rows| rows.start + content.len()..rows.end + content.len());
                content.extend(running);
            }
            if crate::terminal::TODOS_VISIBLE.load(std::sync::atomic::Ordering::Relaxed) && !state.todos.is_empty() {
                // ctrl+t: the todo list below the work in progress.
                if !content.is_empty() { content.push(String::new()); }
                content.extend(crate::terminal::todo_rows(&state.todos, width).into_iter().map(|(row, done, current)| {
                    if done { format!("\x1b[2m{row}\x1b[0m") } else if current { format!("\x1b[1m{row}\x1b[0m") } else { row }
                }));
            }
            if !state.queued.is_empty() {
                // Messages waiting for the model, below all work in progress.
                if !content.is_empty() { content.push(String::new()); }
                content.extend(state.queued.iter().map(|text| clip(&format!("{} {}", crate::terminal::prompt_glyph(), terminal_text(text).replace('\n', " ")), width)));
                content.push("  ctrl+x ctrl+s to send now".into());
            }
            content
        };
        if content.len() > budget {
            let skip = content.len() - budget;
            content.drain(..skip);
            self.shell_rows = self.shell_rows.take().filter(|rows| rows.end > skip).map(|rows| rows.start.saturating_sub(skip)..rows.end - skip);
        }
        // One blank row separates the content from the transcript, as it will
        // once the content is written there.
        if !content.is_empty() && self.scrollback.needs_separator() {
            content.insert(0, String::new());
            self.shell_rows = self.shell_rows.take().map(|rows| rows.start + 1..rows.end + 1);
            self.panel_toggle = self.panel_toggle.map(|mut hit| {
                hit.row += 1;
                hit
            });
        }
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
                    deepseek_code_core::config::ui_language::text("Tab to queue message · Enter to steer message", "Tab 排队消息 · Enter 引导当前任务")
                },
                width,
            ),
            rule.clone(),
            if draft.is_empty() && !state.queued.is_empty() && state.answer.is_none() {
                format!("{prompt}\x1b[2mPress up to edit queued messages\x1b[0m")
            } else {
                format!("{prompt}{draft}")
            },
            rule,
            clip(&mode, width),
        ];
        if !color {
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
        // A blank row above the input box, as at the idle prompt, so the
        // finished turn hands over without moving the box.
        lines.insert(3, String::new());
        let menu = state.menu();
        if !menu.is_empty() && !state.details.expanded {
            // The `/` menu sits right above the input box.
            let selected = state.menu_selected.min(menu.len() - 1);
            let shown = menu.len().min(5);
            let name_width = menu.iter().map(|(name, _)| name.width()).max().unwrap_or(0) + 2;
            let rows: Vec<String> = menu.iter().take(shown).enumerate().map(|(index, (name, description))| {
                let prefix = if index == selected { "  \u{276f} " } else { "    " };
                let row = clip(&format!("{prefix}{name:<name_width$}{description}"), width);
                match (color, index == selected) {
                    (false, _) => row,
                    (true, true) => format!("{}{row}\x1b[0m", crossterm::style::SetForegroundColor(crate::terminal::style::selected())),
                    (true, false) => format!("{}{row}\x1b[0m", crossterm::style::SetForegroundColor(crate::terminal::style::muted())),
                }
            }).collect();
            lines.splice(4..4, rows);
        }
        // Between the turn status and editor; preserve the cursor's distance
        // from the bottom and reserve input room on small screens.
        let mut agent_rows = crate::background_commands::agent_rows(&state.agent_panel, width);
        let limit = (size.1 as usize).saturating_sub(10).min(8);
        if agent_rows.len() > limit && limit > 1 {
            let hidden = agent_rows.len() - limit + 1;
            agent_rows.truncate(limit - 1);
            agent_rows.push(clip(&format!("  … {hidden} more agents"), width));
        } else { agent_rows.truncate(limit); }
        lines.splice(2..2, agent_rows);
        let choice_rows = state.choice.as_ref().map(|choice| choice_lines(choice, &lines[1], &mode, width, size.1 as usize));
        if choice_rows.is_some() {
            // The approval box takes the place of the streaming area.
            content.clear();
            self.panel_toggle = None;
            self.panel_height = 0;
            self.shell_rows = None;
        }
        let choice_cursor = choice_rows.as_ref().map(|(_, selected, column)| (*selected, *column));
        if let Some((rows, _, _)) = choice_rows {
            lines = rows;
        }
        self.input_box = choice_cursor.is_none();
        self.composer_height = lines.len();
        if super::scrollback::compact() && !self.history_view.active() {
            self.refresh_history(state, width);
            self.history_view.show_compact((size.1 as usize).saturating_sub(lines.len()))?;
        }
        if self.history_view.active() {
            self.refresh_history(state, width);
            let input_row = lines
                .len()
                .saturating_sub(choice_cursor.map_or(3, |(row, _)| row));
            let cursor_column =
                choice_cursor.map_or((prompt_width + cursor).min(width), |(_, column)| column);
            self.history_view
                .draw(&lines, input_row, cursor_column, size)?;
            return Ok(());
        }
        let mut content_rows = content.len();
        content.extend(lines);
        let mut lines = content;
        let rows = (size.1 as usize).max(1);
        let limit = rows.saturating_sub(1).max(1);
        let mut input_row = lines.len().saturating_sub(choice_cursor.map_or(3, |(row, _)| row));
        if lines.len() > limit {
            let viewport = super::prompt_display::viewport(lines.len(), input_row, limit);
            let removed = viewport.start;
            lines = lines[viewport].to_vec();
            input_row = input_row.saturating_sub(removed);
            content_rows = content_rows.saturating_sub(removed);
            self.panel_toggle = self.panel_toggle.and_then(|mut hit| {
                hit.row = hit.row.checked_sub(removed)?;
                (hit.row < lines.len()).then_some(hit)
            });
            self.panel_height = self.panel_height.saturating_sub(removed).min(lines.len());
            self.shell_rows = self.shell_rows.take().filter(|rows| rows.end > removed).map(|rows| rows.start.saturating_sub(removed)..rows.end - removed);
        }
        let cursor_column = match choice_cursor {
            Some((_, column)) => column,
            None => (prompt_width + cursor).min(width),
        };
        let Some(out) = self.place(lines, content_rows, input_row, cursor_column, size, &mut || {
            crossterm::cursor::position().map_or(0, |(_, row)| row as usize)
        }) else {
            return Ok(());
        };
        write(&out)?;
        let top = self.top.unwrap_or(0);
        let origin = top;
        let mut background = self.scrollback.screen(origin, size.1);
        for (index, text) in self.lines.iter().enumerate() {
            if let Some(row) = background.get_mut(origin as usize + index) { *row = text.clone(); }
        }
        self.pinned.draw(width, size.1 as usize, &background)
    }
}
/// The frame below the activity line while a menu question is pending:
/// the question, `❯ 1. Yes` style options and a key hint. Returns the rows,
/// the selected row counted from the end and the cursor column.
fn choice_lines(choice: &Choice, status: &str, mode: &str, width: usize, height: usize) -> (Vec<String>, usize, usize) {
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
    ];
    // The preview fits above the question; an edit's full diff is printed when
    // the call finishes.
    let room = height.saturating_sub(rows.len() + choice.options.len() + 3 + usize::from(choice.plan_footer.is_some())).max(1);
    // A question shows the selected option's preview, as in the reference CLI.
    let preview = match &choice.option_previews {
        Some(previews) => previews.get(choice.selected).map(Vec::as_slice).unwrap_or_default(),
        None => choice.preview.as_slice(),
    };
    if choice.command_preview() {
        let rendered = choice.preview_rows(width, color);
        let available = choice.preview_room(height);
        let start = choice.preview_offset.min(rendered.len().saturating_sub(available));
        rows.extend(rendered.into_iter().skip(start).take(available));
        let hint = if choice.expanded { " Click command to collapse; wheel scrolls 1 line" } else { " Click command to show full command" };
        rows.push(paint(clip(hint, width), crate::terminal::style::muted()));
    } else {
    let shown = if preview.len() > room { room.saturating_sub(1) } else { preview.len() };
    for line in &preview[..shown] {
        rows.push(change_view::render(line, width, color));
    }
    if shown < preview.len() {
        rows.push(paint(clip(&format!(" \u{2026} {} more lines", preview.len() - shown), width), crate::terminal::style::muted()));
    }
    }
    rows.push(if color { format!("{}{question}{}", SetAttribute(Attribute::Bold), SetAttribute(Attribute::Reset)) } else { question });
    for index in 0..choice.options.len() {
        let text = clip(&format!(" {} {}. {}", if index == choice.selected { "\u{276f}" } else { " " }, index + 1, choice.label(index)), width);
        rows.push(if index == choice.selected { paint(text, crate::terminal::style::accent()) } else { text });
    }
    if choice.plan_footer.is_some() {
        rows.push(paint(clip("      shift+tab to approve with this feedback", width), crate::terminal::style::muted()));
    }
    let hint = if let Some(footer) = &choice.plan_footer {
        footer.clone()
    } else if choice.amend.is_some() {
        "enter confirm \u{b7} esc to cancel amend".to_owned()
    } else if choice.checked.is_some() {
        mode.replace("esc to interrupt", "\u{2191}\u{2193} select \u{b7} space to toggle \u{b7} enter to submit \u{b7} esc to cancel")
    } else if choice.option_previews.is_some() {
        let last = if choice.amendable() { " \u{b7} tab to type" } else { "" };
        mode.replace("esc to interrupt", &format!("\u{2191}\u{2193} select \u{b7} enter confirm{last} \u{b7} esc to cancel"))
    } else if choice.amendable() {
        mode.replace("esc to interrupt", "\u{2191}\u{2193} select \u{b7} enter confirm \u{b7} tab to amend \u{b7} esc to cancel")
    } else {
        mode.replace("esc to interrupt", "\u{2191}\u{2193} select \u{b7} enter confirm \u{b7} esc to cancel")
    };
    rows.push(paint(clip(&format!(" {hint}"), width), crate::terminal::style::muted()));
    let selected = 1 + usize::from(choice.plan_footer.is_some()) + choice.options.len() - choice.selected;
    // The cursor follows typed text while amending.
    let column = match &choice.amend {
        Some(text) if !text.is_empty() => {
            let label = format!(" \u{276f} {}. {}", choice.selected + 1, choice.label(choice.selected));
            UnicodeWidthStr::width(label.as_str()).min(width)
        }
        _ => 1,
    };
    (rows, selected, column)
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
    let mut selection = super::mouse_selection::Selection::default();
    let mut last_tick = Instant::now();
    let mut last_agents = Instant::now() - Duration::from_secs(1);
    let mut agent_context = None;
    let mut input_reader = super::paste_input::Reader::new()?;
    screen.draw(state)?;
    loop {
        let mut dirty = false;
        while !selection.retained() {
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
                    stream_text(state, screen, &text);
                    dirty = true;
                }
                Ok(Update::Notice(text)) => {
                    finish_text(state, screen);
                    screen.separate_block();
                    screen.log(&text)?;
                    dirty = true;
                }
                Ok(Update::Styled(lines)) => {
                    finish_text(state, screen);
                    if !lines.is_empty() { screen.separate_block(); }
                    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                    let color = std::env::var_os("NO_COLOR").is_none();
                    for line in lines {
                        screen.log_rendered(&change_view::render(&line, width, color));
                    }
                    dirty = true;
                }
                Ok(Update::FileSummary(lines, files)) => {
                    finish_text(state, screen);
                    screen.separate_block();
                    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                    let color = std::env::var_os("NO_COLOR").is_none();
                    if let Some(header) = lines.first() {
                        screen.log_rendered(&change_view::render(header, width, color));
                    }
                    let files = std::sync::Arc::new(files);
                    for selected in 0..files.len() {
                        let (block, rows) = screen.scrollback.file(super::file_review::Review { files: files.clone(), selected });
                        for row in rows { screen.log_line(row, Some(block)); }
                    }
                    dirty = true;
                }
                Ok(Update::Expandable(short, full)) => {
                    finish_text(state, screen);
                    screen.log_block(short, full);
                    dirty = true;
                }
                Ok(Update::ToolStarted(id, header, shell)) => {
                    // Earlier text and reasoning join the transcript first, so
                    // the call shows right below them, where it will stay.
                    finish_text(state, screen);
                    if !state.running.iter().any(|call| call.id == id) {
                        state.running.push(Running { id, header, started: Instant::now(), shell });
                    }
                    dirty = true;
                }
                Ok(Update::Finished(id, short, full)) => {
                    state.running.retain(|call| call.id != id);
                    finish_text(state, screen);
                    match full {
                        Some(full) => screen.log_block(short, full),
                        None if short.is_empty() => {}
                        None => {
                            screen.separate_block();
                            let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                            let color = std::env::var_os("NO_COLOR").is_none();
                            for line in short {
                                screen.log_rendered(&change_view::render(&line, width, color));
                            }
                        }
                    }
                    dirty = true;
                }
                Ok(Update::ShellContext(root, session)) => { state.shell_context = Some((root, session)); }
                Ok(Update::Workspace(path)) => {
                    state.workspace = path;
                    dirty = true;
                }
                Ok(Update::ModelLabel(label)) => {
                    state.model_label = label;
                    dirty = true;
                }
                Ok(Update::HookActivity(activity)) => {
                    state.hook_activity = activity;
                    dirty = true;
                }
                Ok(Update::Activity(activity)) => {
                    if state.activity != activity {
                        state.activity_started = Instant::now();
                        state.shell_expanded = false;
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
                    let now = Instant::now();
                    if state.thinking_span.is_none() {
                        finish_group(state, screen);
                        finish_answer(state, screen);
                        screen.separate_block();
                        state.thinking_preview.clear();
                    }
                    state.thinking_span = Some((state.thinking_span.map_or(now, |span| span.0), now));
                    state.thinking_preview.push_str(&text);
                    state.append_thinking(&text);
                    dirty = true;
                }
                Ok(Update::Group(id, kind, label, step)) => {
                    finish_thought(state, screen);
                    if state.group.is_none() {
                        finish_answer(state, screen);
                        screen.separate_block();
                    }
                    group_call(state, id, kind, label, step);
                    dirty = true;
                }
                Ok(Update::GroupEnd) => {
                    finish_thought(state, screen);
                    finish_group(state, screen);
                    dirty = true;
                }
                Ok(Update::Transcript(lines)) => state.transcript = lines,
                Ok(Update::PinnedPrompt(text)) => { screen.pinned.text = text; dirty = true; },
                Ok(Update::Shells(count)) => state.shells = count,
                Ok(Update::Agents(count)) => state.agents = count,
                Ok(Update::AgentProgress(id, kind, status)) => {
                    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
                        state.foreground_agents.retain(|(key, _)| *key != id);
                    } else if !state.foreground_agents.iter().any(|(key, _)| *key == id) {
                        state.foreground_agents.push((id, kind));
                    }
                    last_agents = Instant::now() - Duration::from_secs(1);
                    dirty = true;
                },
                Ok(Update::Todos(todos)) => {
                    state.todos = todos;
                    dirty = true;
                }
                Ok(Update::Queued(pending)) => {
                    state.queued = pending;
                    dirty = true;
                }
                Ok(Update::QueuedDelivered(text)) => {
                    finish_text(state, screen);
                    if let Some(index) = state.queued.iter().position(|queued| *queued == text) {
                        state.queued.remove(index);
                    }
                    screen.pinned.text = text.clone();
                    super::pinned_prompt::remember(&text);
                    // As in the reference CLI the message joins the transcript
                    // where the model read it; a task notification was already
                    // announced when the command finished.
                    if deepseek_code_core::tools::background_shell::notification_summary(&text)
                        .is_none()
                    {
                        let (block, rows) = screen.scrollback.prompt(&text);
                        for row in rows {
                            screen.log_line(row, Some(block));
                        }
                    }
                    dirty = true;
                }
                Ok(Update::SetDraft(text)) => {
                    state.draft.set(&text);
                    dirty = true;
                }
                Ok(Update::Choice(lines, question, options, plan_footer, reply)) => {
                    finish_text(state, screen);
                    if state.details.expanded {
                        state.details.toggle();
                    }
                    state.question(reply);
                    state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: lines, question, options, selected: 0, amend: None, plan_footer, option_previews: None, checked: None });
                    dirty = true;
                }
                Ok(Update::QuestionChoice(question, options, previews, multi, reply)) => {
                    finish_text(state, screen);
                    if state.details.expanded {
                        state.details.toggle();
                    }
                    state.question(reply);
                    let checked = multi.then(|| vec![false; options.len()]);
                    state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question, options, selected: 0, amend: None, plan_footer: None, option_previews: Some(previews), checked });
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
                    screen.history_view.close()?;
                    finish_text(state, screen);
                    let draft = if state.answer.is_some() {
                        state.saved_draft.take().unwrap_or_default().take()
                    } else {
                        state.draft.take()
                    };
                    if !draft.is_empty() {
                        let _ = actions.send(Action::Draft(draft));
                    }
                    state.running.clear();
                    // As in the reference CLI, after a blank line:
                    // "✻ Churned for 7m 7s · done 10:41 AM".
                    let mut footer = format!(
                        "\u{273b} {} for {} \u{b7} {} {}",
                        PAST_VERBS[state.frame.wrapping_add(state.started.elapsed().subsec_nanos() as usize) % PAST_VERBS.len()],
                        short_duration(state.started.elapsed().as_secs()),
                        error.as_deref().unwrap_or("done"),
                        chrono::Local::now().format("%-I:%M %p")
                    );
                    // As in the reference CLI, a turn that leaves agents running
                    // says so instead of its duration.
                    if state.agents > 0 && error.is_none() {
                        footer = format!("\u{273b} Waiting for {} background agent{} to finish", state.agents, if state.agents == 1 { "" } else { "s" });
                    }
                    // As in the reference CLI: "· 1 shell still running".
                    if state.shells > 0 {
                        footer.push_str(&format!(" \u{b7} {} shell{} still running", state.shells, if state.shells == 1 { "" } else { "s" }));
                    }
                    // An interrupted turn ends as in the reference CLI, inviting the
                    // next instruction instead of reporting a duration.
                    let interrupted = error.as_deref().is_some_and(|error| {
                        let error = error.to_ascii_lowercase();
                        error.contains("interrupt") || error.contains("cancelled")
                    });
                    if interrupted {
                        let line = "  \u{23bf}  Interrupted \u{b7} What should DeepSeek Code do instead?";
                        if std::env::var_os("NO_COLOR").is_some() {
                            screen.log(line)?;
                        } else {
                            screen.log_rendered(&change_view::render(&change_view::failure(line), usize::MAX, true));
                        }
                        return screen.hand_off();
                    }
                    screen.log("")?;
                    screen.log_rendered(&change_view::render(
                        &change_view::muted(&footer),
                        usize::MAX,
                        std::env::var_os("NO_COLOR").is_none(),
                    ));
                    *crate::terminal::TURN_COMPLETION.lock().unwrap_or_else(|e| e.into_inner()) = Some(footer);
                    return screen.hand_off();
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        dirty |= state.dismiss_closed_question();
        if let Some(event) = input_reader.next(Duration::from_millis(25))? {
            if super::shortcuts::requested_in_input(&event, state.draft.characters.is_empty()) {
                selection.clear()?;
                let background = if screen.history_view.active() {
                    let composer = screen.lines.iter().rev().take(screen.composer_height).cloned().collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>();
                    screen.history_view.screen_rows(screen.size.1 as usize, &composer)
                } else {
                    let origin = screen.top.unwrap_or(0);
                    let mut background = screen.scrollback.screen(origin, screen.size.1);
                    for (index, text) in screen.lines.iter().enumerate() {
                        if let Some(row) = background.get_mut(origin as usize + index) { *row = text.clone(); }
                    }
                    background
                };
                super::shortcuts::show(super::shortcuts::Context::Running, &background, &mut || input_reader.next(Duration::from_millis(25)))?;
                screen.draw(state)?;
                continue;
            }
            // Cancellation may arrive while waiting for a key. Never submit a
            // dismissed approval draft as a newly queued user prompt.
            if state.dismiss_closed_question() {
                screen.draw(state)?;
                continue;
            }
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    selection.clear()?;
                    if state.draft.cursor < state.draft.paste_end || matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Home) { state.draft.pasted = false; }
                    let key = match state.bindings.resolve(key, &mut state.key_pending) {
                        crate::keybindings::Resolution::Key(key) => key,
                        crate::keybindings::Resolution::Consumed => continue,
                    };
                    if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char('>' | '<' | '.' | ',')) {
                        let command = if matches!(key.code, KeyCode::Char('>' | '.')) { "/effort +" } else { "/effort -" };
                        let _ = actions.send(Action::Submit(command.into()));
                        continue;
                    }
                    if super::scrollback::compact_key(key) {
                        super::scrollback::toggle_compact();
                        screen.history_view.close()?;
                        screen.draw(state)?;
                        continue;
                    }
                    if screen.history_key(state, key)? {
                        screen.draw(state)?;
                        continue;
                    }
                    if screen.history_view.active() && !super::scrollback::compact() {
                        screen.history_view.close()?;
                        if key.code == KeyCode::Esc {
                            screen.draw(state)?;
                            continue;
                        }
                    }
                    // With a menu open, shift+tab belongs to the menu.
                    if crate::keybindings::is_cycle_permission(key) && state.choice.is_none() {
                        state.cycle_permission(&actions);
                        continue;
                    }
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    if !(control && key.code == KeyCode::Char('r')) {
                        state.search_query = None;
                    }
                    let before: String = state.draft.characters.iter().collect();
                    if state.choice_key(key) || state.menu_key(key) {
                    } else if control && matches!(key.code, KeyCode::Char('y' | 'n')) && state.goal_proposal.is_some() && state.answer.is_none() {
                        state.decide_goal(key.code == KeyCode::Char('y'), &actions);
                    } else if crate::images::is_image_paste_key(key) && state.answer.is_none() {
                        // As at the prompt: an image on the clipboard joins the
                        // message being written as [Image #N].
                        match crate::images::paste() {
                            Some(crate::images::Pasted::Image(placeholder)) => {
                                state.undo.push((state.draft.characters.clone(), state.draft.cursor));
                                state.draft.insert(&placeholder);
                            }
                            Some(crate::images::Pasted::Text(text)) => {
                                if !state.draft.insert_paste(&text) {
                                    screen.log("Paste not inserted: the active draft limit is 32768 characters. Shorten the text and paste again.")?;
                                }
                            }
                            None => {}
                        }
                    } else if control && key.code == KeyCode::Char('t') {
                        crate::terminal::TODOS_VISIBLE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                    } else if control && key.code == KeyCode::Char('x') {
                        state.chord = Some(Instant::now());
                        continue;
                    } else if control && key.code == KeyCode::Char('s') && state.chord.take().is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
                        let _ = actions.send(Action::SendNow);
                    } else if control && key.code == KeyCode::Char('b') && state.activity.starts_with("Running Bash") {
                        // As in the reference CLI, a running command can continue
                        // in the background; otherwise Ctrl+B detaches the session.
                        let _ = actions.send(Action::BackgroundShell);
                    } else if control && key.code == KeyCode::Char('b') {
                        let _ = actions.send(Action::Detach);
                    } else if control && key.code == KeyCode::Char('o') {
                        // As at the prompt: the transcript so far, this turn's
                        // calls included, in the full-screen pager.
                        let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                        let color = std::env::var_os("NO_COLOR").is_none();
                        let logged = screen.detailed_log(state, width, color);
                        let outcome = super::transcript_pager::run_with_logged(&state.transcript, &logged, &mut || input_reader.next(Duration::from_millis(100)))?;
                        if matches!(outcome, super::transcript_pager::Outcome::PrintToScrollback) {
                            for line in &state.transcript {
                                screen.log_rendered(&change_view::render(line, width, color));
                            }
                            for line in &logged {
                                screen.log_rendered(line);
                            }
                        }
                        screen.size = (0, 0);
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
                        && state.draft.characters.is_empty()
                        && !state.queued.is_empty()
                    {
                        let _ = actions.send(Action::EditQueued);
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
                    } else if state.answer.is_none() && edit_draft(state, screen, key)? {
                        // The prompt's editing shortcuts, during the turn too.
                    } else {
                        match key.code {
                            KeyCode::Enter
                                if key.modifiers.contains(KeyModifiers::SHIFT)
                                    || key.modifiers.contains(KeyModifiers::ALT) =>
                            {
                                state.draft.insert("\n")
                            }
                            KeyCode::Tab if state.answer.is_none() => {
                                state.queue_message(&actions);
                            }
                            KeyCode::Enter => {
                                if let Some(message) = state.submit(&actions) {
                                    screen.log(&message)?;
                                }
                            }
                            KeyCode::Backspace if state.draft.cursor > 0 => {
                                state.undo.push((state.draft.characters.clone(), state.draft.cursor));
                                state.draft.cursor -= 1;
                                state.draft.characters.remove(state.draft.cursor);
                            }
                            KeyCode::Delete
                                if state.draft.cursor < state.draft.characters.len() =>
                            {
                                state.undo.push((state.draft.characters.clone(), state.draft.cursor));
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
                                state.undo.push((state.draft.characters.clone(), state.draft.cursor));
                                state.draft.insert(&ch.to_string())
                            }
                            _ => {}
                        }
                    }
                    if state.draft.characters.iter().collect::<String>() != before {
                        // A changed draft opens the menu again at its first match.
                        state.menu_hidden = false;
                        state.menu_selected = 0;
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
                        if let Some(choice) = state.choice.as_mut().filter(|choice| choice.command_preview()) {
                            if choice.expanded {
                                choice.scroll_preview(mouse.kind == MouseEventKind::ScrollUp, screen.size.0.saturating_sub(1) as usize, screen.size.1 as usize);
                                selection.clear()?;
                                screen.draw(state)?;
                            }
                            continue;
                        }
                    }
                    if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
                        selection.clear()?;
                        screen.pinned.expanded = false;
                        screen.refresh_history(state, screen.size.0.saturating_sub(1) as usize);
                        screen.history_view.wheel(mouse.kind == MouseEventKind::ScrollUp, (screen.size.1 as usize).saturating_sub(screen.composer_height))?;
                        screen.draw(state)?;
                        continue;
                    }
                    if screen.history_view.active() {
                        let composer = screen
                            .lines
                            .iter()
                            .rev()
                            .take(screen.composer_height)
                            .cloned()
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>();
                        let snapshot = screen
                            .history_view
                            .screen_rows(screen.size.1 as usize, &composer);
                        if let Some(click) = selection.event(mouse, snapshot.clone())? {
                            if click.kind == MouseEventKind::Down(MouseButton::Left) {
                                if let Some(index) = screen.history_view.row_index(
                                    click.row as usize,
                                    (screen.size.1 as usize).saturating_sub(composer.len()),
                                ) {
                                    if let Some(review) = screen.scrollback.review_presentation(
                                        index,
                                        screen.size.0.saturating_sub(1) as usize,
                                    ) {
                                        super::file_review::show(review, &snapshot, &mut || {
                                            input_reader.next(Duration::from_millis(100))
                                        })?;
                                        screen.size = (0, 0);
                                    } else {
                                        screen.scrollback.click_presentation(
                                            index,
                                            screen.size.0.saturating_sub(1) as usize,
                                        );
                                    }
                                }
                            }
                            screen.draw(state)?;
                        }
                        continue;
                    }
                    let mut snapshot = screen.scrollback.screen(screen.top.unwrap_or(0), screen.size.1);
                    for (index, text) in screen.lines.iter().enumerate() {
                        if let Some(row) =
                            snapshot.get_mut(screen.top.unwrap_or(0) as usize + index)
                        {
                            *row = text.clone();
                        }
                    }
                    let Some(mouse) = selection.event(mouse, snapshot.clone())? else {
                        continue;
                    };
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && screen.pinned.click(mouse.row)
                    {
                        screen.pinned.expanded = false;
                        let width = screen.size.0.saturating_sub(1) as usize;
                        let row = screen.scrollback.expand_prompt(&screen.pinned.text, width);
                        screen.refresh_history(state, width);
                        screen.history_view.open_at(row)?;
                        screen.draw(state)?;
                        continue;
                    }
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                        if let Some(id) = screen
                            .scrollback
                            .prompt_at(screen.top.unwrap_or(0), mouse.row)
                        {
                            let width = screen.size.0.saturating_sub(1) as usize;
                            let row = screen.scrollback.expand_prompt_id(id, width);
                            screen.refresh_history(state, width);
                            screen.history_view.open_at(row)?;
                            screen.draw(state)?;
                            continue;
                        }
                        if let Some(review) = screen
                            .scrollback
                            .review_at(screen.top.unwrap_or(0), mouse.row)
                        {
                            super::file_review::show(review, &snapshot, &mut || {
                                input_reader.next(Duration::from_millis(100))
                            })?;
                            screen.size = (0, 0);
                            screen.draw(state)?;
                            continue;
                        }
                    }
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && state.activity.starts_with("Running Bash")
                        && mouse.row.checked_sub(screen.top.unwrap_or(0))
                            .is_some_and(|row| screen.shell_rows.as_ref().is_some_and(|rows| rows.contains(&(row as usize))))
                    {
                        state.shell_expanded = !state.shell_expanded;
                        screen.draw(state)?;
                        continue;
                    }
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                        if let Some(choice) = state.choice.as_mut().filter(|choice| choice.command_preview()) {
                            let origin = screen.top.unwrap_or(0) as usize + screen.padding;
                            let total = screen.lines.len().saturating_sub(screen.padding);
                            if (mouse.row as usize).checked_sub(origin).is_some_and(|row| choice.click_preview(row, total)) {
                                screen.draw(state)?;
                                continue;
                            }
                        }
                    }
                    // The details panel is live content at the top of the frame.
                    let origin = screen.top.unwrap_or(0);
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && screen.top.is_some_and(|top| mouse.row < top)
                    {
                        // A click on a tool result above the frame shows it in
                        // full; another click shows the short form again.
                        dirty |= screen.toggle_block(mouse.row);
                    } else if let Some(row) = mouse.row.checked_sub(origin) {
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
                                    state.details.scroll_by(1, screen.panel_viewport);
                                    dirty = true;
                                }
                                MouseEventKind::ScrollUp => {
                                    state.details.scroll_by(-1, screen.panel_viewport);
                                    dirty = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    screen.history_view.close()?;
                    // A dragged or pasted image file shows as `[Image #N]`, as at the prompt.
                    let text = crate::images::pasted_path(&text).map_or(text, |path| format!("{} ", crate::images::attach(path)));
                    if !state.draft.insert_paste(&text) {
                        screen.log("Paste not inserted: the active draft limit is 32768 characters. Shorten the text and paste again.")?;
                    }
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        if last_agents.elapsed() >= Duration::from_secs(1) {
            last_agents = Instant::now();
            if let Some((root, session)) = &state.shell_context {
                if agent_context.is_none() {
                    agent_context = deepseek_code_core::runtime::RuntimePaths::new(root.clone()).ok()
                        .and_then(|paths| crate::services::context(&paths).ok());
                }
                if let Some(context) = &agent_context {
                    state.agent_panel = crate::background_commands::running_agents(&context.paths, &context.db, session);
                    state.agent_panel.extend(state.foreground_agents.iter().filter_map(|(id, kind)| crate::background_commands::foreground_agent(context, id, kind)));

                    dirty = true;
                }
            }
        }
        if last_tick.elapsed() >= Duration::from_millis(120) {
            last_tick = Instant::now();
            state.frame = state.frame.wrapping_add(1);
            dirty = true;
        }
        if dirty && !selection.retained() {
            screen.draw(state)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn composer(status: &str) -> Vec<String> {
        ["", status, "hint", "", "rule", "> draft", "rule", "footer"].map(String::from).to_vec()
    }
    fn place(frame: &mut Frame, terminal: &mut vt100::Parser, content: &[&str], status: &str, cursor: usize) {
        let mut lines: Vec<String> = content.iter().map(|row| row.to_string()).collect();
        lines.extend(composer(status));
        let input_row = lines.len() - 3;
        let (rows, cols) = terminal.screen().size();
        if let Some(output) = frame.place(lines, content.len(), input_row, 2, (cols, rows), &mut || cursor) {
            terminal.process(output.as_bytes());
        }
    }
    fn screen(terminal: &vt100::Parser) -> Vec<String> {
        terminal.screen().rows(0, 80).collect()
    }

    #[test]
    fn live_content_follows_the_transcript_and_the_input_box_never_moves() {
        for height in [16u16, 24, 40] {
            let rows = height as usize;
            let mut terminal = vt100::Parser::new(height, 80, 200);
            terminal.process(b"banner\r\nprompt\r\n");
            let mut frame = Frame::default();
            // The first frame moves the short transcript down onto it.
            place(&mut frame, &mut terminal, &["", "\u{25cf} Thinking for 1s\u{2026}", "  \u{23bf}  reasoning"], "thinking", 2);
            let first = screen(&terminal);
            assert_eq!(first[rows - 1], "footer");
            assert_eq!(first[rows - 3], "> draft");
            assert_eq!(first[rows - 7], "thinking");
            assert_eq!(first[rows - 9], "  \u{23bf}  reasoning");
            assert_eq!(first[rows - 11], "");
            assert_eq!(first[rows - 12], "prompt");
            assert_eq!(terminal.screen().cursor_position(), (height - 3, 2));
            // The reasoning joins the transcript and the call shows in its place.
            frame.log_rendered("Thought for 1s 1 line");
            place(&mut frame, &mut terminal, &["", "\u{25cf} #1 Bash(ls)", "  \u{23bf}  Running\u{2026}"], "working", 0);
            let second = screen(&terminal);
            assert_eq!(second[rows - 12], "Thought for 1s 1 line");
            assert_eq!(second[rows - 10], "\u{25cf} #1 Bash(ls)");
            assert_eq!(second[rows - 9], "  \u{23bf}  Running\u{2026}");
            assert_eq!(second[rows - 13], "prompt");
            for row in [rows - 8, rows - 5, rows - 4, rows - 3, rows - 2, rows - 1] {
                assert_eq!(first[row], second[row], "row {row} at height {height}");
            }
            // A shorter frame leaves blank rows above the status line instead
            // of moving anything.
            frame.log_rendered("");
            frame.log_rendered("\u{25cf} #1 Bash(ls)");
            frame.log_rendered("  \u{23bf}  file");
            place(&mut frame, &mut terminal, &[], "working", 0);
            let third = screen(&terminal);
            assert_eq!(third[rows - 9], "  \u{23bf}  file");
            assert_eq!(third[rows - 7], "working");
            assert_eq!(third[rows - 1], "footer");
            // The turn hands over in the idle prompt's layout: completion, a
            // separator row and the prompt's blank row above the same box.
            frame.log_rendered("");
            frame.log_rendered("\u{273b} Worked for 2s");
            frame.input_box = true;
            let (output, row) = frame.hand_off_output(rows).unwrap();
            terminal.process(output.as_bytes());
            let done = screen(&terminal);
            assert_eq!(row as usize, rows - 6);
            assert_eq!(done[rows - 7], "\u{273b} Worked for 2s");
            assert_eq!(done[rows - 8], "");
            assert_eq!(done[rows - 9], "  \u{23bf}  file");
            assert_eq!(done[rows - 6], "");
            assert_eq!(done[rows - 5], "");
            for row in rows - 4..rows {
                assert_eq!(done[row], third[row]);
            }
            assert_eq!(terminal.screen().cursor_position(), (height - 6, 0));
        }
    }

    #[test]
    fn a_turn_starting_on_the_bottom_row_keeps_the_rows_above_it() {
        let mut terminal = vt100::Parser::new(20, 80, 200);
        for index in 0..20 {
            terminal.process(format!("row {index}\r\n").as_bytes());
        }
        let mut frame = Frame::default();
        place(&mut frame, &mut terminal, &[], "status", 19);
        let rows = screen(&terminal);
        assert_eq!(rows[10], "row 18");
        assert_eq!(rows[11], "row 19");
        assert_eq!(rows[13], "status");
        assert_eq!(rows[19], "footer");
    }

    #[test]
    fn unchanged_frames_write_nothing_and_scrolls_precede_new_rows() {
        let mut terminal = vt100::Parser::new(20, 80, 200);
        let mut frame = Frame::default();
        let lines = || {
            let mut lines = vec![String::new(), "live".to_owned()];
            lines.extend(composer("status"));
            lines
        };
        place(&mut frame, &mut terminal, &["", "live"], "status", 0);
        let input_row = lines().len() - 3;
        assert!(frame.place(lines(), 2, input_row, 2, (80, 20), &mut || 0).is_none());
        // A full screen: each logged line scrolls before any row changes.
        for index in 0..30 {
            frame.log_rendered(&format!("line {index}"));
            let output = frame.place(lines(), 2, input_row, 2, (80, 20), &mut || 0).unwrap();
            if index >= 10 {
                let scroll = output.find("\r\n").unwrap();
                let first_write = output.find("line ").unwrap();
                assert!(scroll < first_write, "the scroll precedes the new rows");
            }
            terminal.process(output.as_bytes());
            let rows = screen(&terminal);
            assert_eq!(rows[19], "footer");
            assert_eq!(rows[17], "> draft");
            assert_eq!(rows[index.min(9)], format!("line {index}"));
        }
    }

    /// Run in ConPTY to inspect the real renderer, cursor, resize and removal.
    #[test]
    #[ignore = "requires an interactive terminal"]
    fn agent_panel_terminal_fixture() {
        let mut state = State::new("default".into());
        state.draft.insert("draft stays editable");
        state.agent_panel = ["Grepping mysql-schema.sql and sentinel usage", "Locating forward/proxy code", "Searching dubbo pages for registries"].iter().enumerate().map(|(i, description)| crate::background_commands::Agent {
            agent_type: "explore".into(), description: (*description).into(),
            started_ms: chrono::Utc::now().timestamp_millis() as u64 - [66000, 57000, 52000][i],
            output_tokens: [57800, 91800, 53200][i],
        }).collect();
        let mut frame = Frame::default();
        for step in 0..80 {
            if step == 50 { state.agent_panel.clear(); }
            frame.draw(&mut state).unwrap();
            if step < 50 {
                let rows: Vec<_> = frame.lines.iter().map(|line| super::super::tool_view::result_text(line)).collect();
                let main = rows.iter().position(|line| line.contains("● main")).unwrap();
                let draft = rows.iter().position(|line| line.contains("draft stays editable")).unwrap();
                assert!(main < draft);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn tab_queue_and_enter_steer_use_distinct_actions() {
        let mut state = State::new("mode".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        state.draft.insert("later message");
        assert!(state.queue_message(&send));
        assert!(matches!(receive.try_recv().unwrap(), Action::Queue(text) if text == "later message"));
        state.draft.insert("steer now");
        state.submit(&send);
        assert!(matches!(receive.try_recv().unwrap(), Action::Submit(text) if text == "steer now"));
        assert!(!state.queue_message(&send));
    }

    #[test]
    fn approval_command_expands_and_scrolls_one_row_without_approving() {
        let command = (0..40).map(|i| format!("echo row-{i:02} {}", "x".repeat(90))).collect::<Vec<_>>().join("\n");
        let approval = super::super::tool_view::approval("Bash", &serde_json::json!({"command": command}), None, false);
        let mut choice = Choice { expanded: false, preview_offset: 0, preview: approval.lines, question: approval.question, options: approval.options, selected: 0, amend: None, plan_footer: None, option_previews: None, checked: None };
        assert!(choice.command_preview());
        assert!(change_view::plain(&choice.preview).contains("row-39"));
        let collapsed = choice.preview_rows(40, false);
        assert!(!choice.click_preview(1, 20));
        assert!(!choice.click_preview(18, 20));
        assert!(choice.click_preview(6, 20));
        let expanded = choice.preview_rows(40, false);
        assert!(expanded.len() > collapsed.len());
        assert!(expanded.iter().all(|row| UnicodeWidthStr::width(row.as_str()) <= 40));
        choice.scroll_preview(false, 40, 24);
        assert_eq!(choice.preview_offset, 1);
        choice.scroll_preview(true, 40, 24);
        assert_eq!(choice.preview_offset, 0);
        for _ in 0..200 { choice.scroll_preview(false, 40, 24); }
        let (rows, _, _) = choice_lines(&choice, "status", "mode", 40, 24);
        assert!(rows.iter().any(|row| row.contains("row-39")));
        assert!(rows.iter().any(|row| row.contains("Yes")));
        assert_eq!(choice.selected, 0);
        assert!(choice.click_preview(6, 20));
        assert!(!choice.expanded);
        assert_eq!(choice.preview_offset, 0);
    }

    #[test]
    fn streamed_markdown_is_independent_of_delta_boundaries() {
        let text = format!("**{}**\n- {}\n```rust\nlet text = \"{}\";\n```\n", "a".repeat(700), "word ".repeat(160), "中".repeat(240));
        let render = |chunk_size: usize| {
            let mut state = State::new("ask".into());
            state.markdown = super::super::markdown::MarkdownStream::new(true);
            let mut screen = Frame::default();
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(chunk_size) {
                let delta: String = chunk.iter().collect();
                stream_text(&mut state, &mut screen, &delta);
                let partial = state.partial.clone();
                let preview = streamed_rows(&state, 40, 2);
                assert!(preview.len() <= 2);
                assert_eq!(state.partial, partial);
            }
            finish_text(&mut state, &mut screen);
            screen.logged
        };
        let whole = render(text.chars().count());
        assert_eq!(render(17), whole);
        assert!(whole.iter().any(|line| line.contains("\x1b[1m")));
        assert!(!whole.iter().any(|line| line.contains("**")));
    }

    #[test]
    fn detailed_transcript_includes_completed_lines_notices_and_partial_once() {
        let mut state = State::new("ask".into());
        let mut screen = Frame::default();
        stream_text(&mut state, &mut screen, "ANSWER_SENTINEL\n");
        screen.log("NOTICE_SENTINEL").unwrap();
        stream_text(&mut state, &mut screen, "PARTIAL_SENTINEL");
        let logged = screen.detailed_log(&state, 80, false).join("\n");
        for text in ["ANSWER_SENTINEL", "NOTICE_SENTINEL", "PARTIAL_SENTINEL"] {
            assert_eq!(logged.matches(text).count(), 1);
        }
        assert!(logged.find("ANSWER_SENTINEL") < logged.find("NOTICE_SENTINEL"));
        assert!(logged.find("NOTICE_SENTINEL") < logged.find("PARTIAL_SENTINEL"));
    }

    #[test]
    fn large_paste_displays_label_but_submits_original_text() {
        let mut draft = Draft::default();
        let text = "中文\n".repeat(100);
        assert!(draft.insert_paste(&text));
        assert!(draft.visible(80).0.starts_with("[Pasted Content 300 chars]"));
        draft.insert(" more");
        assert!(draft.visible(80).0.contains("chars] more"));
        assert_eq!(draft.take(), format!("{text} more"));
    }

    #[test]
    fn paste_label_preserves_typed_prefix_and_suffix() {
        let mut draft = Draft::default();
        draft.insert("before after");
        draft.cursor = 7;
        let pasted = "line\n".repeat(100);
        assert!(draft.insert_paste(&pasted));
        let visible = draft.visible(120).0;
        assert!(visible.starts_with("before [Pasted Content"), "{visible}");
        assert!(visible.ends_with("after"), "{visible}");
        assert_eq!(draft.take(), format!("before {pasted}after"));
    }
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
    fn thinking_preview_shows_the_latest_reasoning_as_one_paragraph() {
        let mut state = State::new("ask".into());
        let now = Instant::now();
        state.thinking_span = Some((now, now));
        state.thinking_preview = "The user asked.\n\nLet me   check the files first, then answer.".into();
        let lines = thinking_lines(&state, 30, 3);
        assert_eq!(lines[0], "\u{25cf} Thinking for 1s\u{2026}");
        let rows: Vec<String> = lines[1..].iter().map(|line| line.replace("\x1b[2m", "").replace("\x1b[0m", "")).collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("  \u{23bf}  "), "{rows:?}");
        assert!(rows[1].starts_with("     ") && rows[1].ends_with("answer."), "{rows:?}");
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
        let words = visual_lines("one two three", 8);
        assert_eq!(words.iter().map(|(row, _)| row.as_str()).collect::<Vec<_>>(), ["one two ", "three"]);
        assert_eq!(words[0].1, 8);
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
        let mut terminal = vt100::Parser::new(24, 80, 100);
        let mut frame = Frame::default();
        place(&mut frame, &mut terminal, &["answer"], "working 1s", 0);
        let mut lines = vec!["answer".to_owned()];
        lines.extend(composer("working 2s"));
        let update = frame.place(lines, 1, 6, 2, (80, 24), &mut || 0).unwrap();
        assert!(update.contains("working 2s"));
        assert!(!update.contains("answer"));
        assert!(!update.contains("draft"));
        assert!(!update.contains('\n'));
        assert!(!update.contains("\x1b[J"));
    }
    #[test]
    fn bash_activity_keeps_command_out_of_working_status() {
        let mut state = State::new("ask".into());
        state.activity = "Running Bash(echo fixture)".into();
        let status = state.status_line();
        assert!(status.contains("Working"));
        assert!(!status.contains("echo fixture"));
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
    fn tab_adds_a_note_to_yes_or_no() {
        use crossterm::event::KeyEvent;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut state = State::new("ask".into());
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Proceed?".into(), options: vec!["Yes".into(), "Always".into(), "No".into()], selected: 1, amend: None, plan_footer: None, option_previews: None, checked: None });
        // The remember choice takes no note.
        assert!(state.choice_key(key(KeyCode::Tab)));
        assert!(state.choice.as_ref().unwrap().amend.is_none());
        assert!(state.choice_key(key(KeyCode::Up)));
        assert!(state.choice_key(key(KeyCode::Tab)));
        assert_eq!(state.choice.as_ref().unwrap().label(0), "Yes, and tell DeepSeek Code what to do next");
        for ch in "use -5 instead".chars() {
            assert!(state.choice_key(key(KeyCode::Char(ch))));
        }
        assert_eq!(state.choice.as_ref().unwrap().label(0), "Yes, use -5 instead");
        let (_, _, column) = choice_lines(state.choice.as_ref().unwrap(), "status", "esc", 80, 24);
        assert_eq!(column, " \u{276f} 1. Yes, use -5 instead".chars().count());
        // Esc leaves the note without interrupting.
        assert!(state.choice_key(key(KeyCode::Esc)));
        assert!(state.choice.as_ref().unwrap().amend.is_none());
        assert!(state.choice_key(key(KeyCode::Up)));
        assert!(state.choice_key(key(KeyCode::Tab)));
        assert_eq!(state.choice.as_ref().unwrap().label(2), "No, and tell DeepSeek Code what to do differently");
        for ch in "keep it".chars() {
            state.choice_key(key(KeyCode::Char(ch)));
        }
        assert!(state.choice_key(key(KeyCode::Enter)));
        assert_eq!(received.try_recv().unwrap(), "3\nkeep it");
    }
    #[test]
    fn menu_questions_answer_with_arrows_numbers_and_esc_as_no() {
        use crossterm::event::KeyEvent;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut state = State::new("ask".into());
        state.draft.insert("queued draft");
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Proceed?".into(), options: vec!["Yes".into(), "Always".into(), "No".into()], selected: 0, amend: None, plan_footer: None, option_previews: None, checked: None });
        assert!(state.choice_key(key(KeyCode::Up)));
        assert_eq!(state.choice.as_ref().unwrap().selected, 2);
        assert!(state.choice_key(key(KeyCode::Down)));
        assert!(state.choice_key(key(KeyCode::Down)));
        assert!(state.choice_key(key(KeyCode::Char('x'))));
        assert!(state.choice_key(key(KeyCode::Enter)));
        assert_eq!(received.try_recv().unwrap(), "2");
        assert!(state.choice.is_none() && state.answer.is_none());
        assert_eq!(state.draft.take(), "queued draft");

        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Proceed?".into(), options: vec!["Yes".into(), "No".into()], selected: 0, amend: None, plan_footer: None, option_previews: None, checked: None });
        assert!(state.choice_key(key(KeyCode::Char('3'))));
        assert!(received.try_recv().is_err());
        assert!(state.choice_key(key(KeyCode::Char('2'))));
        assert_eq!(received.try_recv().unwrap(), "2");
        // Esc answers the last choice, No.
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Proceed?".into(), options: vec!["Yes".into(), "Always".into(), "No".into()], selected: 0, amend: None, plan_footer: None, option_previews: None, checked: None });
        assert!(state.choice_key(key(KeyCode::Esc)));
        assert_eq!(received.try_recv().unwrap(), "3");
        // A question shows the focused option's preview and takes typed text last.
        let previews = vec![vec![change_view::body("tabs preview")], vec![change_view::body("spaces preview")], Vec::new()];
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Indent?".into(), options: vec!["Tabs".into(), "Spaces".into(), "Type something else".into()], selected: 0, amend: None, plan_footer: None, option_previews: Some(previews), checked: None });
        let shown = |state: &State| choice_lines(state.choice.as_ref().unwrap(), "status", "esc to interrupt", 60, 24).0.join("\n");
        assert!(shown(&state).contains("tabs preview") && !shown(&state).contains("spaces preview"));
        assert!(state.choice_key(key(KeyCode::Down)));
        assert!(shown(&state).contains("spaces preview") && !shown(&state).contains("tabs preview"));
        assert!(state.choice_key(key(KeyCode::Down)));
        assert!(shown(&state).contains("tab to type"));
        for ch in "own".chars() { assert!(state.choice_key(key(KeyCode::Char(ch)))); }
        assert!(state.choice_key(key(KeyCode::Enter)));
        assert_eq!(received.try_recv().unwrap(), "3\nown");
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Indent?".into(), options: vec!["Tabs".into(), "Type something else".into()], selected: 0, amend: None, plan_footer: None, option_previews: Some(vec![Vec::new(), Vec::new()]), checked: None });
        assert!(state.choice_key(key(KeyCode::Esc)));
        assert_eq!(received.try_recv().unwrap(), "esc");
        // multiSelect: space and numbers toggle, enter submits the checked set.
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.choice = Some(Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Features?".into(), options: vec!["A".into(), "B".into(), "C".into(), "Type something else".into()], selected: 0, amend: None, plan_footer: None, option_previews: Some(vec![Vec::new(); 4]), checked: Some(vec![false; 4]) });
        assert!(state.choice_key(key(KeyCode::Char(' '))));
        assert!(state.choice_key(key(KeyCode::Char('3'))));
        assert!(shown(&state).contains("[x] A") && shown(&state).contains("[ ] B") && shown(&state).contains("[x] C"));
        assert!(shown(&state).contains("space to toggle"));
        assert!(received.try_recv().is_err());
        assert!(state.choice_key(key(KeyCode::Enter)));
        assert_eq!(received.try_recv().unwrap(), "m:1,3");
        let (rows, selected, _) = choice_lines(&Choice { expanded: false, preview_offset: 0, preview: Vec::new(), question: "Proceed?".into(), options: vec!["Yes".into(), "No".into()], selected: 1, amend: None, plan_footer: None, option_previews: None, checked: None }, "status", "esc to interrupt", 40, 24);
        assert_eq!(rows.len() - selected, 5);
        // The approval preview sits between the rule and the question and is
        // clipped to the screen; answering removes the whole box.
        let preview = (0..30).map(|row| change_view::body(&format!("line {row}"))).collect();
        let choice = Choice { expanded: false, preview_offset: 0, preview, question: "Proceed?".into(), options: vec!["Yes".into(), "No".into()], selected: 0, amend: None, plan_footer: None, option_previews: None, checked: None };
        let (rows, _, _) = choice_lines(&choice, "status", "esc to interrupt", 40, 20);
        let strip = |row: &String| {
            let mut text = String::new();
            let mut escape = false;
            for ch in row.chars() {
                match (escape, ch) {
                    (_, '\x1b') => escape = true,
                    (true, 'm') => escape = false,
                    (true, _) => {}
                    (false, ch) => text.push(ch),
                }
            }
            text
        };
        let plain: Vec<String> = rows.iter().map(strip).collect();
        assert_eq!(rows.len(), 19);
        assert_eq!(plain[3], "line 0");
        assert!(plain.iter().any(|row| row.contains("more lines")), "{plain:?}");
        assert!(plain.iter().position(|row| row.contains("Proceed?")) > plain.iter().position(|row| row.contains("more lines")));
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
    fn status_answers_during_a_turn_while_other_commands_wait() {
        let mut state = State::new("ask".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        for prompt in ["/status", "/status --check", "/tasks", "/tasks show example"] {
            state.draft.insert(prompt);
            assert!(state.submit(&send).is_none());
            assert!(matches!(receive.try_recv().unwrap(), Action::Submit(text) if text == prompt));
        }
        state.draft.insert("/cost");
        assert!(state.submit(&send).is_some());
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
    fn slash_menu_lists_only_commands_that_run_during_a_turn() {
        let mut state = State::new("ask".into());
        state.draft.set("/");
        let names: Vec<_> = state.menu().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["/background", "/btw", "/goal", "/status", "/tasks"]);
        state.draft.set("/bt");
        assert!(state.menu_key(crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(state.draft.characters.iter().collect::<String>(), "/btw ");
        assert!(state.menu().is_empty());
        state.draft.set("/zz");
        assert!(state.menu().is_empty());
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
    running_title: Option<super::title::RunningTitle>,
    workspace: Option<std::path::PathBuf>,
    pub live: Option<LiveTurn>,
    hooks: std::collections::BTreeMap<String, String>,
    fallback: super::progress::TurnProgress,
    tools: std::collections::HashMap<String, (Instant, Value)>,
    finished: std::collections::HashMap<String, (String, Value, String, bool)>,
    /// Lines added and removed per file this turn, for the closing summary.
    changes: std::collections::BTreeMap<String, (u64, u64)>,
    change_details: std::collections::BTreeMap<String, Vec<change_view::Line>>,
    /// The session-wide numbers of this turn's shell commands.
    numbers: std::collections::HashMap<String, usize>,
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
            running_title: interactive.then(super::title::RunningTitle::new),
            workspace: None,
            live,
            hooks: Default::default(),
            fallback,
            tools: Default::default(),
            finished: Default::default(),
            changes: Default::default(),
            change_details: Default::default(),
            numbers: Default::default(),
        })
    }
    pub fn restore_numbers(&mut self, session: &crate::session::Session) -> anyhow::Result<()> {
        self.numbers = session.store.db.with_conn(|conn| -> anyhow::Result<_> {
            let mut numbers = std::collections::HashMap::new();
            for message in deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, &session.id)? {
                for tool in deepseek_code_core::db::message_repo::get_tool_calls_for_message(conn, &message.id)? {
                    let number = numbers.len() + 1;
                    let id = tool.id.strip_prefix(&format!("{}:", message.id)).unwrap_or(&tool.id).to_owned();
                    numbers.insert(id, number);
                }
            }
            Ok(numbers)
        })??;
        self.fallback.restore_numbers(self.numbers.clone());
        Ok(())
    }
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }
    pub fn set_shell_context(&self, root: &std::path::Path, session: &str) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::ShellContext(root.to_path_buf(), session.to_owned()));
        }
    }
    pub fn with_workspace(mut self, workspace: &std::path::Path) -> Self {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::Workspace(crate::terminal::home_path(workspace)));
        }
        self.workspace = Some(workspace.to_owned());
        self.fallback.set_workspace(workspace);
        self
    }
    /// The footer's `deepseek-flash · ● high`.
    pub fn set_model_label(&self, label: &str) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::ModelLabel(label.to_owned()));
        }
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
            let label = super::tool_view::label(name, input, self.workspace.as_deref());
            let next = self.numbers.values().copied().max().unwrap_or(0) + 1;
            let number = Some(*self.numbers.entry(id.to_owned()).or_insert(next));
            let header = super::tool_view::numbered(vec![change_view::tool_call(&label, "", false)], number);
            live.tool_started(id, header.into_iter().next().unwrap_or_else(|| change_view::tool_call(&label, "", false)), matches!(name, "Bash" | "PowerShell"));
            let activity = super::tool_view::task_output_activity(name, input)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Running {label}"));
            live.activity(&activity);
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
        let next = self.numbers.values().copied().max().unwrap_or(0) + 1;
        self.numbers.entry(id.to_owned()).or_insert(next);
        if !failed {
            if let Ok(value) = serde_json::from_str::<Value>(output) {
                let change = &value["file_change"];
                if let Some(path) = change["path"].as_str() {
                    let entry = self.changes.entry(path.to_owned()).or_default();
                    entry.0 += change["added"].as_u64().unwrap_or(0);
                    entry.1 += change["removed"].as_u64().unwrap_or(0);
                    let details = self.change_details.entry(path.to_owned()).or_default();
                    if !details.is_empty() { details.push(change_view::muted("--- Next edit ---")); }
                    if let Some(lines) = change_view::change_in_workspace(output, false, self.workspace.as_deref()) {
                        details.extend(lines.into_iter().skip(1));
                    } else {
                        details.push(change_view::muted("Diff unavailable for this change."));
                    }
                }
            }
        }
        if let (Some(live), false, true) = (&self.live, failed, name == "TodoWrite") {
            // Keep the ctrl+t list current as the model updates it.
            let todos = input["todos"].as_array().into_iter().flatten().map(|todo| (todo["content"].as_str().unwrap_or_default().to_owned(), todo["status"].as_str().unwrap_or("pending").to_owned())).collect();
            let _ = live.updates.send(Update::Todos(todos));
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
                let display_lines = super::tool_view::numbered(
                    change_view::change_in_workspace(output, failed, self.workspace.as_deref())
                        .unwrap_or_else(|| lines.clone()), self.numbers.get(id).copied());
                // Summary plus four diff rows; a click, where the terminal reports one,
                // shows the diff. Ctrl+O has the full text regardless.
                if display_lines.len() > 1 {
                    live.finished(id, change_view::collapsed_change(&display_lines), Some(display_lines));
                } else {
                    live.finished(id, display_lines, None);
                }
            } else {
                // A mirrored completion can arrive without the call's input;
                // label it with the input seen when the call started.
                let started = previous.as_ref().map(|(_, input)| input);
                let label_input = match started {
                    Some(started) if input.as_object().is_none_or(|fields| fields.is_empty()) => started,
                    _ => input,
                };
                let number = self.numbers.get(id).copied();
                let short = super::tool_view::numbered(super::tool_view::finished(
                    name,
                    label_input,
                    output,
                    failed,
                    elapsed,
                    self.workspace.as_deref(),
                ), number);
                if name != "TodoWrite" {
                    let full = super::tool_view::numbered(super::tool_view::expanded_block(name, label_input, output, failed, elapsed, self.workspace.as_deref()), number);
                    live.finished(id, short, Some(full));
                } else {
                    live.finished(id, short, None);
                }
            }
            live.activity("Working");
        } else {
            self.fallback.tool_finished(id, name, input, output, failed);
        }
    }
    /// Lines written into the transcript as they are, such as a rejected plan.
    pub fn styled_lines(&mut self, lines: Vec<change_view::Line>) {
        match &self.live {
            Some(live) => live.styled(lines),
            None => self.notice(&change_view::plain(&lines)),
        }
    }
    /// The todo list for ctrl+t.
    pub fn set_todos(&mut self, todos: Vec<(String, String)>) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::Todos(todos));
        }
    }
    /// Background agents still running when the turn ends.
    pub fn set_running_agents(&mut self, count: usize) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::Agents(count));
        }
    }
    /// Background shells still running, named in the closing line.
    pub fn set_running_shells(&mut self, count: usize) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::Shells(count));
        }
    }
    /// The conversation before this turn, for Ctrl+O.
    pub fn set_pinned_prompt(&self, text: &str) {
        super::pinned_prompt::remember(text);
        if let Some(live) = &self.live { let _ = live.updates.send(Update::PinnedPrompt(text.into())); }
    }
    pub fn set_transcript(&mut self, lines: Vec<change_view::Line>) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::Transcript(lines));
        }
    }
    /// Messages sent during the turn and not yet read by the model.
    pub fn queued(&mut self, pending: Vec<String>) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::Queued(pending));
        }
    }
    pub fn queued_delivered(&mut self, text: &str) {
        match &self.live {
            Some(live) => {
                let _ = live.updates.send(Update::QueuedDelivered(text.into()));
            }
            None => self.notice(&format!("{} {text}", crate::terminal::prompt_glyph())),
        }
    }
    pub fn set_draft(&mut self, text: &str) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::SetDraft(text.into()));
        }
    }
    pub fn agent_progress(&mut self, id: &str, kind: &str, status: &str, tool: Option<&str>) {
        if let Some(live) = &self.live {
            let _ = live.updates.send(Update::AgentProgress(id.into(), kind.into(), status.into()));
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
            "Click a file to review · Ctrl+O shows the diffs",
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
        self.running_title.take();
        if let Some(live) = self.live.take() {
            let summary = self.change_summary();
            if !summary.is_empty() {
                let files = self.changes.keys().zip(summary.iter().skip(1)).map(|(path, summary)| super::file_review::File {
                    label: super::display_path::label(path, self.workspace.as_deref()),
                    summary: summary.clone(),
                    details: self.change_details.get(path).cloned().unwrap_or_default(),
                }).collect();
                let _ = live.updates.send(Update::FileSummary(summary, files));
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
    fn review_retains_ordered_edits_and_ignores_failed_and_duplicate_results() {
        let temp = tempfile::tempdir().unwrap();
        let mut display = TurnDisplay::new(false, "ask", &temp.path().join("keys.json"), vec![]).unwrap().with_workspace(temp.path());
        let output = |text: &str| serde_json::json!({"success":true,"file_change":{"version":1,"verified":true,"path":"a.rs","added":1,"removed":1,"rows":[{"kind":"removed","old_line":1,"text":"before"},{"kind":"added","new_line":1,"text":text}]}}).to_string();
        display.tool_finished("1", "Edit", &serde_json::json!({}), &output("first"), false);
        display.tool_finished("1", "Edit", &serde_json::json!({}), &output("first"), false);
        display.tool_finished("2", "Edit", &serde_json::json!({}), &output("second"), false);
        display.tool_finished("3", "Edit", &serde_json::json!({}), &output("failed"), true);
        let text = change_view::plain(&display.change_details["a.rs"]);
        assert_eq!(text.matches("first").count(), 1);
        assert!(text.find("first").unwrap() < text.find("second").unwrap());
        assert!(text.contains("before"));
        assert!(!text.contains("failed"));
    }

    #[test]
    #[ignore = "requires an interactive terminal"]
    fn history_live_terminal_fixture() {
        let mut history = super::super::scrollback::Scrollback::default();
        let prompt = (0..80)
            .map(|i| format!("LIVE_INPUT_{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = history.record_prompt(&prompt);
        for row in rows {
            eprintln!("{row}");
        }
        history.save();
        let temp = tempfile::tempdir().unwrap();
        let live = LiveTurn::start(
            "ask",
            crate::keybindings::load(&temp.path().join("keys.json")).unwrap(),
            vec![],
            None,
        )
        .unwrap();
        live.notice("Live history acceptance");
        std::thread::sleep(Duration::from_secs(12));
        live.finish(None);
    }

    #[test]
    #[ignore = "requires an interactive terminal"]
    fn file_review_terminal_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let mut display = TurnDisplay::new(true, "ask", &temp.path().join("keys.json"), vec![]).unwrap().with_workspace(temp.path());
        display.notice("File review acceptance fixture");
        for (index, path) in ["src/calc.rs", "notes.txt"].iter().enumerate() {
            let mut rows = vec![serde_json::json!({"kind":"removed","old_line":1,"text":"return 41;"})];
            rows.extend((1..=65).map(|n| serde_json::json!({"kind":"added","new_line":n,"text":format!("return {}; // change {n}", 41 + n)})));
            let output = serde_json::json!({"success":true,"file_change":{"version":1,"verified":true,"path":temp.path().join(path),"added":65,"removed":1,"rows":rows}}).to_string();
            display.tool_finished(&index.to_string(), "Edit", &serde_json::json!({}), &output, false);
        }
        display.finish(None);
        let mut editor = super::super::input::editor::PromptEditor::default();
        editor.set_draft("draft stays editable".into());
        let answer = editor.readline("❯ ").unwrap();
        assert!(matches!(answer, Some(super::super::input::PromptEvent::Line(text)) if text == "draft stays editable"));
        let live = LiveTurn::start("ask", crate::keybindings::load(&temp.path().join("keys.json")).unwrap(), vec![], None).unwrap();
        live.notice("Next turn: previous file changes remain clickable");
        std::thread::sleep(Duration::from_secs(20));
        live.finish(None);
    }
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
        assert_eq!(lines[0], "\u{25cf} Changed 2 files (+16 \u{2212}3) \u{b7} Click a file to review · Ctrl+O shows the diffs");
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
    fn streaming_answer_has_separator_before_its_first_newline() {
        let mut state = State::new("ask".into());
        let mut screen = Frame::default();
        screen.log_block(vec![change_view::body("tool")], vec![change_view::body("tool")]);
        state.block_start = true;
        stream_text(&mut state, &mut screen, "partial");
        assert_eq!(screen.logged.len(), 2);
        assert_eq!(screen.logged[1], "");
        stream_text(&mut state, &mut screen, " answer");
        assert_eq!(screen.logged.len(), 2);
        finish_answer(&mut state, &mut screen);
        assert_eq!(screen.logged.len(), 3);
        assert!(screen.logged[2].contains("partial answer"));
    }

    #[test]
    fn thought_and_answer_have_exactly_one_separator() {
        let mut state = State::new("ask".into());
        let now = Instant::now();
        state.thinking_span = Some((now, now));
        state.thinking_preview = "reasoning".into();
        let mut screen = Frame::default();
        finish_thought(&mut state, &mut screen);
        state.block_start = true;
        log_answer_line(&mut state, &mut screen, "answer");
        assert_eq!(screen.logged.len(), 3);
        assert_eq!(screen.logged[1], "");
        assert!(screen.logged[2].contains("answer"));
    }

    #[test]
    fn completed_thinking_is_a_collapsed_block() {
        let mut state = State::new("ask".into());
        let now = Instant::now();
        state.thinking_span = Some((now, now));
        state.thinking_preview = "first\nsecond".into();
        let mut screen = Frame::default();
        finish_thought(&mut state, &mut screen);
        assert!(state.thinking_span.is_none());
        assert!(screen.logged[0].contains("Thought for 1s 2 lines (click to expand)"));
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
