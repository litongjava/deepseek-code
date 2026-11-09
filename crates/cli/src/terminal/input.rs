//! Request-driven terminal input: one OS reader, idle until a consumer asks.
//! History is session-local and restricted to explicit REPL reads. Answers to
//! permission and provider questions never enter completion or history.
#[cfg(test)]
use rustyline::error::ReadlineError;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot};

#[path = "editor.rs"]
pub(crate) mod editor;

#[derive(Debug, PartialEq, Eq)]
pub enum PromptEvent {
    Line(String),
    CyclePermission,
    StepEffort(bool),
}

/// Whether a prompt can be edited in place and keys reach us before Enter.
fn interactive_terminal() -> bool {
    !crate::terminal::screen_reader()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

/// Cancellation while a synchronous command (such as `/compact`) owns the
/// turn. The existing input reader watches for Esc or Ctrl+C; no second
/// stdin reader is started.
pub struct CancelWatch {
    stop: Arc<AtomicBool>,
    signal: oneshot::Receiver<()>,
}

impl CancelWatch {
    /// Resolves `true` for a cancel key, `false` when the reader stopped
    /// without one. The caller keeps waiting on `false`.
    pub async fn wait(&mut self) -> bool {
        (&mut self.signal).await.is_ok()
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct WatchRequest {
    stop: Arc<AtomicBool>,
    signal: oneshot::Sender<()>,
    transcript: Arc<std::sync::Mutex<Vec<super::change_view::Line>>>,
}

impl WatchRequest {
    /// Reads keys while a synchronous command runs. Esc and Ctrl+C cancel;
    /// Ctrl+O still opens the transcript, so history stays readable during the
    /// wait. Resize is left to the progress renderer, which re-measures on its
    /// next tick. Other keys are discarded by policy: the prompt is not
    /// active, and replaying them later would submit input the user did not
    /// type at a prompt.
    fn run(self) {
        let mut signal = Some(self.signal);
        if crossterm::terminal::enable_raw_mode().is_err() {
            return;
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            match crossterm::event::poll(std::time::Duration::from_millis(50)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key))
                        if key.kind != crossterm::event::KeyEventKind::Release
                            && (key.code == crossterm::event::KeyCode::Esc
                                || (key.code == crossterm::event::KeyCode::Char('c')
                                    && key
                                        .modifiers
                                        .contains(crossterm::event::KeyModifiers::CONTROL))) =>
                    {
                        if let Some(signal) = signal.take() {
                            let _ = signal.send(());
                        }
                        break;
                    }
                    Ok(crossterm::event::Event::Key(key))
                        if key.kind != crossterm::event::KeyEventKind::Release
                            && key.code == crossterm::event::KeyCode::Char('o')
                            && key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        let lines = self
                            .transcript
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .clone();
                        if !lines.is_empty() {
                            let _ = crate::terminal::transcript_pager::view(&lines);
                            // The pager may leave the terminal usable but not in
                            // the mode we entered with.
                            if crossterm::terminal::enable_raw_mode().is_err() {
                                break;
                            }
                        }
                    }
                    Ok(crossterm::event::Event::Resize(_, _)) => {}
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

enum Message {
    Powerup(
        deepseek_code_core::runtime::RuntimePaths,
        String,
        oneshot::Sender<anyhow::Result<()>>,
    ),
    PermissionMode(String),
    Commands(Vec<(String, String)>),
    Details(Vec<(String, String)>),
    Transcript(Vec<super::change_view::Line>),
    Panel(Vec<(String, String)>, bool),
    Draft(String),
    Keybindings(crate::keybindings::Keybindings),
    Workspace(std::path::PathBuf),
    ContextNote(Option<String>),
    Shells(usize),
    ShellCounter(Box<dyn Fn() -> usize + Send>),
    Subagents(deepseek_code_core::services::ServiceContext, String),
    AgentDetails(Box<dyn Fn(&str) -> String + Send>),
    AgentRows(Box<dyn Fn() -> Vec<crate::background_commands::Agent> + Send>),
    Todos(Box<dyn Fn() -> Vec<(String, String)> + Send>),
    Vim(bool),
    Read(Request),
    Watch(WatchRequest),
}

struct Request {
    cancelled: Arc<AtomicBool>,
    prompt: String,
    repl: bool,
    /// Echo typed text as dots (API keys).
    secret: bool,
    response: oneshot::Sender<io::Result<Option<PromptEvent>>>,
}

struct PendingRead {
    receive: oneshot::Receiver<io::Result<Option<PromptEvent>>>,
    cancelled: Arc<AtomicBool>,
    repl: bool,
}

pub struct TerminalInput {
    notices: std::sync::mpsc::Sender<String>,
    suggestions: std::sync::mpsc::Sender<String>,
    requests: std::sync::mpsc::Sender<Message>,
    pending: Option<PendingRead>,
    fixture: Option<mpsc::UnboundedReceiver<String>>,
    queued: std::collections::VecDeque<PromptEvent>,
    effort: Arc<std::sync::Mutex<Option<String>>>,
    effort_steps: mpsc::UnboundedReceiver<bool>,
    /// The last rendered transcript, so Ctrl+O works while a command runs.
    transcript: Arc<std::sync::Mutex<Vec<super::change_view::Line>>>,
}

impl TerminalInput {
    pub fn new() -> Self {
        let (requests, rx) = std::sync::mpsc::channel::<Message>();
        let (notices, notice_rx) = std::sync::mpsc::channel();
        let (suggestions, suggestion_rx) = std::sync::mpsc::channel();
        let effort = Arc::new(std::sync::Mutex::new(None));
        let editor_effort = effort.clone();
        let transcript: Arc<std::sync::Mutex<Vec<super::change_view::Line>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread_transcript = transcript.clone();
        let (step_tx, effort_steps) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let terminal = interactive_terminal();
            let mut editor = editor::PromptEditor::default();
            editor.set_effort_control(editor_effort, step_tx);
            editor.set_notices(notice_rx);
            editor.set_suggestions(suggestion_rx);
            let mut plain = io::BufReader::new(io::stdin());
            for message in rx {
                let request = match message {
                    Message::Powerup(paths, argument, response) => {
                        if !response.is_closed() {
                            let _ = response.send(crate::powerup::run(&paths, &argument));
                        }
                        continue;
                    }
                    Message::PermissionMode(mode) => {
                        editor.set_permission_mode(mode);
                        continue;
                    }
                    Message::Commands(commands) => {
                        editor.set_commands(commands);
                        continue;
                    }
                    Message::Draft(draft) => {
                        editor.set_draft(draft);
                        continue;
                    }
                    Message::Panel(details, expanded) => {
                        editor.set_panel(details, expanded);
                        continue;
                    }
                    Message::Details(details) => {
                        editor.set_details(details);
                        continue;
                    }
                    Message::Transcript(lines) => {
                        *thread_transcript
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = lines.clone();
                        editor.set_transcript(lines);
                        continue;
                    }
                    Message::Keybindings(bindings) => {
                        editor.set_keybindings(bindings);
                        continue;
                    }
                    Message::Workspace(workspace) => {
                        editor.set_workspace(workspace);
                        continue;
                    }
                    Message::ContextNote(note) => {
                        editor.set_context_note(note);
                        continue;
                    }
                    Message::Shells(count) => {
                        editor.set_shells(count);
                        continue;
                    }
                    Message::ShellCounter(counter) => {
                        editor.set_shell_counter(counter);
                        continue;
                    }
                    Message::Subagents(context, session) => {
                        editor.set_subagents(context, session);
                        continue;
                    }
                    Message::AgentDetails(details) => {
                        editor.set_agent_details(details);
                        continue;
                    }
                    Message::AgentRows(rows) => {
                        editor.set_agent_rows(rows);
                        continue;
                    }
                    Message::Todos(todos) => {
                        editor.set_todos(todos);
                        continue;
                    }
                    Message::Vim(on) => {
                        editor.set_vim(on);
                        continue;
                    }
                    Message::Read(request) => request,
                    Message::Watch(request) => {
                        request.run();
                        continue;
                    }
                };
                if request.response.is_closed() {
                    continue;
                }
                let result = if !request.repl {
                    super::answer_input::read(
                        &mut plain,
                        &request.prompt,
                        &request.cancelled,
                        request.secret,
                    )
                    .map(|value| value.map(PromptEvent::Line))
                } else if terminal {
                    editor.readline_until(&request.prompt, &request.cancelled)
                } else {
                    read_plain(&mut plain, &request.prompt)
                        .map(|value| value.map(PromptEvent::Line))
                };
                let _ = request.response.send(result);
            }
        });
        Self {
            notices,
            suggestions,
            requests,
            pending: None,
            fixture: None,
            queued: Default::default(),
            effort,
            effort_steps,
            transcript,
        }
    }

    pub fn notice(&self, text: String) {
        let _ = self.notices.send(text);
    }

    /// The existing input thread owns modal keys as well as ordinary prompts.
    pub async fn powerup(
        &mut self,
        paths: deepseek_code_core::runtime::RuntimePaths,
        argument: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.pending.is_none(), "A prompt is still reading input");
        let (response, receive) = oneshot::channel();
        self.requests
            .send(Message::Powerup(paths, argument, response))
            .map_err(|_| anyhow::anyhow!("Terminal input closed"))?;
        receive
            .await
            .map_err(|_| anyhow::anyhow!("Terminal reader stopped"))?
    }

    pub fn set_commands(&self, commands: Vec<(String, String)>) {
        let _ = self.requests.send(Message::Commands(commands));
    }

    /// Reload before the next prompt. Invalid files leave the prior valid
    /// configuration intact and return an error for the caller to display.
    pub fn set_keybindings(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let bindings = crate::keybindings::load(path)?;
        self.requests
            .send(Message::Keybindings(bindings))
            .map_err(|_| anyhow::anyhow!("Terminal input closed"))
    }

    /// Set the last turn's tool titles and full display bodies before reading.
    pub fn set_panel(&self, details: Vec<(String, String)>, expanded: bool) {
        let _ = self.requests.send(Message::Panel(details, expanded));
    }

    /// Turns vim-style prompt editing on or off.
    pub fn set_vim(&self, on: bool) {
        let _ = self.requests.send(Message::Vim(on));
    }

    /// Shown in the footer while the context budget is low.
    pub fn set_context_note(&self, note: Option<String>) {
        let _ = self.requests.send(Message::ContextNote(note));
    }

    /// Background shells still running, for "1 shell · ↓ to manage".
    pub fn set_shells(&self, count: usize) {
        let _ = self.requests.send(Message::Shells(count));
    }

    /// The model and reasoning effort at the right of the footer, as
    /// "deepseek-flash · ● high".
    pub fn set_effort(&self, effort: Option<String>) {
        *self
            .effort
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = effort;
    }

    /// Rows for running background agents, shown above the editor.
    pub fn set_agent_rows(
        &self,
        rows: Box<dyn Fn() -> Vec<crate::background_commands::Agent> + Send>,
    ) {
        let _ = self.requests.send(Message::AgentRows(rows));
    }

    pub fn set_subagents(
        &self,
        context: deepseek_code_core::services::ServiceContext,
        session: String,
    ) {
        let _ = self.requests.send(Message::Subagents(context, session));
    }

    pub fn set_agent_details(&self, details: Box<dyn Fn(&str) -> String + Send>) {
        let _ = self.requests.send(Message::AgentDetails(details));
    }

    /// Reads the todo list that ctrl+t shows.
    pub fn set_todos(&self, todos: Box<dyn Fn() -> Vec<(String, String)> + Send>) {
        let _ = self.requests.send(Message::Todos(todos));
    }

    /// Recounts running shells while the prompt waits, so the footer drops
    /// a shell once it finishes.
    pub fn set_shell_counter(&self, counter: Box<dyn Fn() -> usize + Send>) {
        let _ = self.requests.send(Message::ShellCounter(counter));
    }

    /// The project whose files `@` completes.
    pub fn set_workspace(&self, workspace: &std::path::Path) {
        let _ = self.requests.send(Message::Workspace(workspace.to_owned()));
    }

    /// Where a background task delivers the suggested next prompt.
    pub fn suggestion_sender(&self) -> std::sync::mpsc::Sender<String> {
        self.suggestions.clone()
    }

    pub fn set_details(&self, details: Vec<(String, String)>) {
        let _ = self.requests.send(Message::Details(details));
    }

    /// The last turn's tool calls, printed into scrollback on Ctrl+O.
    pub fn set_transcript(&self, lines: Vec<super::change_view::Line>) {
        let _ = self.requests.send(Message::Transcript(lines));
    }

    pub fn queue_permission_cycle(&mut self) {
        self.queued.push_back(PromptEvent::CyclePermission);
    }

    pub fn queue_prompt(&mut self, prompt: String) {
        self.queued.push_back(PromptEvent::Line(prompt));
    }
    /// Takes back waiting prompts the user can see, leaving everything else queued.
    pub fn take_prompts(&mut self, shown: &[String]) -> Vec<String> {
        let mut taken = Vec::new();
        self.queued.retain(|event| match event {
            PromptEvent::Line(text) if shown.contains(text) => {
                taken.push(text.clone());
                false
            }
            _ => true,
        });
        taken
    }
    pub fn restore_draft(&self, draft: String) {
        let _ = self.requests.send(Message::Draft(draft));
    }
    pub fn set_permission_mode(&self, mode: &str) {
        let _ = self.requests.send(Message::PermissionMode(mode.into()));
    }

    /// True when answers come from a caller-supplied channel instead of the
    /// terminal; interactive pickers must then keep their text forms.
    pub fn is_fixture(&self) -> bool {
        self.fixture.is_some()
    }

    /// Arms Esc/Ctrl+C cancellation for a synchronous command that runs outside
    /// the prompt loop. `None` when no key can arrive (piped or minimalist input).
    pub fn watch_cancel(&mut self) -> io::Result<Option<CancelWatch>> {
        if self.fixture.is_some() || self.pending.is_some() || !interactive_terminal() {
            return Ok(None);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (signal, receiver) = oneshot::channel();
        self.requests
            .send(Message::Watch(WatchRequest {
                stop: stop.clone(),
                signal,
                transcript: self.transcript.clone(),
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Terminal input closed"))?;
        Ok(Some(CancelWatch {
            stop,
            signal: receiver,
        }))
    }

    /// Ends an outstanding interactive prompt read so another source can run a
    /// turn. Its draft is kept, and a line submitted meanwhile is re-queued.
    /// Returns false when the read cannot be interrupted (plain stdin).
    pub async fn yield_prompt(&mut self) -> io::Result<bool> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(true);
        };
        if !pending.repl {
            return Ok(false);
        }
        pending.cancelled.store(true, Ordering::Release);
        match tokio::time::timeout(std::time::Duration::from_millis(500), &mut pending.receive)
            .await
        {
            Err(_) => {
                pending.cancelled.store(false, Ordering::Release);
                Ok(false)
            }
            Ok(result) => {
                self.pending = None;
                match result {
                    Ok(Ok(Some(event))) => self.queued.push_front(event),
                    Ok(Err(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Ok(Err(error)) => return Err(error),
                    Ok(Ok(None)) | Err(_) => {}
                }
                Ok(true)
            }
        }
    }

    pub async fn read_prompt(&mut self, prompt: &str) -> io::Result<Option<PromptEvent>> {
        if let Some(queued) = self.queued.pop_front() {
            return Ok(Some(queued));
        }
        self.read(prompt, true).await
    }

    /// Dismiss only the outstanding answer generation and wait until the sole OS
    /// reader acknowledges it. A new prompt can never inherit its response.
    pub async fn dismiss_answer(&mut self) -> io::Result<()> {
        if self.pending.as_ref().is_some_and(|pending| pending.repl) {
            return Err(io::Error::other(
                "Cannot dismiss an ordinary prompt as an approval",
            ));
        }
        if let Some(mut pending) = self.pending.take() {
            pending.cancelled.store(true, Ordering::Release);
            let _ = (&mut pending.receive).await;
        }
        Ok(())
    }

    /// Like `read_answer`, but typed text is shown as dots.
    pub async fn read_secret(&mut self, prompt: &str) -> io::Result<Option<String>> {
        match self.read_with(prompt, false, true).await? {
            Some(PromptEvent::Line(line)) => Ok(Some(line)),
            None => Ok(None),
            Some(PromptEvent::CyclePermission | PromptEvent::StepEffort(_)) => {
                Err(io::Error::other("Shortcut is not a question answer"))
            }
        }
    }

    pub async fn read_answer(&mut self, prompt: &str) -> io::Result<Option<String>> {
        match self.read(prompt, false).await? {
            Some(PromptEvent::Line(line)) => Ok(Some(line)),
            None => Ok(None),
            Some(PromptEvent::CyclePermission | PromptEvent::StepEffort(_)) => Err(
                io::Error::other("Permission cycle is not a question answer"),
            ),
        }
    }

    // Compatibility for voice consumers; errors/interruption terminate input.
    pub async fn recv(&mut self) -> Option<String> {
        self.read_answer("").await.ok().flatten()
    }

    async fn read(&mut self, prompt: &str, repl: bool) -> io::Result<Option<PromptEvent>> {
        self.read_with(prompt, repl, false).await
    }

    async fn read_with(
        &mut self,
        prompt: &str,
        repl: bool,
        secret: bool,
    ) -> io::Result<Option<PromptEvent>> {
        if let Some(fixture) = self.fixture.as_mut() {
            return Ok(fixture.recv().await.map(PromptEvent::Line));
        }
        if self.pending.is_none() {
            let (response, receive) = oneshot::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            self.requests
                .send(Message::Read(Request {
                    cancelled: cancelled.clone(),
                    prompt: prompt.into(),
                    repl,
                    secret,
                    response,
                }))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Terminal input closed"))?;
            self.pending = Some(PendingRead {
                receive,
                cancelled,
                repl,
            });
        }
        // Keep the outstanding response in the handle across select! cancellation.
        // There is never a second reader racing for stdin or losing its next line.
        let result = tokio::select! {
            result = &mut self.pending.as_mut().unwrap().receive => result,
            Some(up) = self.effort_steps.recv(), if repl => return Ok(Some(PromptEvent::StepEffort(up))),
        };
        self.pending = None;
        result.unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Terminal reader stopped",
            ))
        })
    }
}

#[cfg(test)]
mod recall_tests {
    use super::*;

    #[test]
    fn taking_prompts_back_leaves_what_the_user_cannot_see() {
        let mut input = TerminalInput::from(mpsc::unbounded_channel().1);
        input.queue_prompt("/compact".into());
        input.queue_prompt("waiting".into());
        input.queue_permission_cycle();
        assert_eq!(input.take_prompts(&["waiting".into()]), ["waiting"]);
        assert!(input.take_prompts(&["waiting".into()]).is_empty());
        assert_eq!(input.queued.len(), 2);
        assert!(
            matches!(input.queued.front(), Some(PromptEvent::Line(text)) if text == "/compact")
        );
    }
}

#[cfg(test)]
impl From<mpsc::UnboundedReceiver<String>> for TerminalInput {
    fn from(fixture: mpsc::UnboundedReceiver<String>) -> Self {
        let (requests, _) = std::sync::mpsc::channel();
        Self {
            notices: std::sync::mpsc::channel().0,
            suggestions: std::sync::mpsc::channel().0,
            requests,
            pending: None,
            fixture: Some(fixture),
            queued: Default::default(),
            effort: Default::default(),
            transcript: Default::default(),
            effort_steps: mpsc::unbounded_channel().1,
        }
    }
}

#[cfg(test)]
fn map_readline(result: rustyline::Result<String>) -> io::Result<Option<String>> {
    match result {
        Ok(line) => Ok(Some(line)),
        Err(ReadlineError::Eof) => Ok(None),
        Err(ReadlineError::Interrupted) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Input cancelled",
        )),
        Err(error) => Err(io::Error::other(error.to_string())),
    }
}

fn read_plain(reader: &mut impl BufRead, prompt: &str) -> io::Result<Option<String>> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_answers_are_non_interactive() {
        let (_, fixture) = mpsc::unbounded_channel();
        assert!(TerminalInput::from(fixture).is_fixture());
        assert!(!TerminalInput::new().is_fixture());
    }

    #[tokio::test]
    async fn effort_shortcuts_keep_the_same_pending_editor_read() {
        let (requests, receive) = std::sync::mpsc::channel();
        let (steps, effort_steps) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            requests,
            notices: std::sync::mpsc::channel().0,
            suggestions: std::sync::mpsc::channel().0,
            pending: None,
            fixture: None,
            queued: Default::default(),
            effort: Default::default(),
            transcript: Default::default(),
            effort_steps,
        };
        steps.send(true).unwrap();
        assert!(matches!(
            input.read_prompt("prompt").await.unwrap(),
            Some(PromptEvent::StepEffort(true))
        ));
        let Message::Read(request) = receive.try_recv().unwrap() else {
            panic!()
        };
        for up in [false, true, true] {
            steps.send(up).unwrap();
            assert!(
                matches!(input.read_prompt("prompt").await.unwrap(), Some(PromptEvent::StepEffort(value)) if value == up)
            );
            assert!(receive.try_recv().is_err());
            assert!(!request.cancelled.load(Ordering::Acquire));
        }
        input.set_effort(Some("model · high".into()));
        assert_eq!(
            input.effort.lock().unwrap().as_deref(),
            Some("model · high")
        );
        request
            .response
            .send(Ok(Some(PromptEvent::Line("unchanged draft".into()))))
            .unwrap();
        assert!(
            matches!(input.read_prompt("prompt").await.unwrap(), Some(PromptEvent::Line(line)) if line == "unchanged draft")
        );
        assert!(input.pending.is_none());
    }

    #[tokio::test]
    async fn piped_answer_reader_cancels_without_stealing_the_next_line() {
        const FIXTURE: &str = "CLAUDE_TEST_PIPE_ANSWER_READER";
        if std::env::var_os(FIXTURE).is_some() {
            let mut reader = io::BufReader::new(io::stdin());
            let flag = Arc::new(AtomicBool::new(false));
            let cancelled = flag.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                cancelled.store(true, Ordering::Release);
            });
            assert_eq!(
                super::super::answer_input::read(&mut reader, "", &flag, false)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Interrupted
            );
            println!("CANCEL_ACK");
            io::stdout().flush().unwrap();
            assert_eq!(
                super::super::answer_input::read(&mut reader, "", &AtomicBool::new(false), false)
                    .unwrap()
                    .as_deref(),
                Some("FRESH_RESPONSE")
            );
            return;
        }
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","terminal::input::tests::piped_answer_reader_cancels_without_stealing_the_next_line","--nocapture"])
            .env(FIXTURE,"1").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()).kill_on_drop(true).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        stdin.write_all(b"STALE_ANSWER").await.unwrap();
        stdin.flush().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if stdout
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child ended before cancellation ACK")
                    .contains("CANCEL_ACK")
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        stdin.write_all(b"FRESH_RESPONSE\n").await.unwrap();
        drop(stdin);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn dismissed_answer_waits_for_reader_ack_and_cannot_answer_next_prompt() {
        let (requests, receive) = std::sync::mpsc::channel();
        let (suggestions, _) = std::sync::mpsc::channel();
        let (notices, _) = std::sync::mpsc::channel();
        let mut input = TerminalInput {
            requests,
            notices,
            suggestions,
            pending: None,
            fixture: None,
            queued: Default::default(),
            effort: Default::default(),
            transcript: Default::default(),
            effort_steps: mpsc::unbounded_channel().1,
        };
        let reader = std::thread::spawn(move || {
            let Message::Read(first) = receive.recv().unwrap() else {
                panic!()
            };
            while !first.cancelled.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // Even an answer already typed in the losing generation is discarded.
            first
                .response
                .send(Ok(Some(PromptEvent::Line("stale yes".into()))))
                .unwrap();
            let Message::Read(second) = receive.recv().unwrap() else {
                panic!()
            };
            assert!(!second.cancelled.load(Ordering::Acquire));
            second
                .response
                .send(Ok(Some(PromptEvent::Line("fresh no".into()))))
                .unwrap();
        });
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            input.read_answer("First?")
        )
        .await
        .is_err());
        tokio::time::timeout(std::time::Duration::from_secs(2), input.dismiss_answer())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            input.read_answer("Second?").await.unwrap().as_deref(),
            Some("fresh no")
        );
        reader.join().unwrap();
    }
    #[test]
    fn piped_input_preserves_unicode_spaces_and_eof() {
        let mut input = io::Cursor::new("  中文  \r\n\nlast");
        assert_eq!(
            read_plain(&mut input, "").unwrap().as_deref(),
            Some("  中文  ")
        );
        assert_eq!(read_plain(&mut input, "").unwrap().as_deref(), Some(""));
        assert_eq!(read_plain(&mut input, "").unwrap().as_deref(), Some("last"));
        assert_eq!(read_plain(&mut input, "").unwrap(), None);
    }
    #[test]
    fn interrupt_is_not_eof_or_a_literal_command() {
        assert_eq!(
            map_readline(Err(ReadlineError::Interrupted))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(map_readline(Err(ReadlineError::Eof)).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_cancel_watch_reports_its_key_and_stops_the_reader_on_drop() {
        let stop = Arc::new(AtomicBool::new(false));
        let (signal, receiver) = oneshot::channel();
        let mut watch = CancelWatch {
            stop: stop.clone(),
            signal: receiver,
        };
        signal.send(()).unwrap();
        assert!(watch.wait().await);
        assert!(!stop.load(Ordering::Acquire));
        drop(watch);
        assert!(stop.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_stopped_reader_does_not_look_like_a_cancellation() {
        let stop = Arc::new(AtomicBool::new(false));
        let (signal, receiver) = oneshot::channel::<()>();
        drop(signal);
        let mut watch = CancelWatch {
            stop,
            signal: receiver,
        };
        assert!(!watch.wait().await);
    }

    #[test]
    fn piped_input_cannot_arm_a_cancel_watch() {
        let mut input = TerminalInput::from(mpsc::unbounded_channel().1);
        assert!(input.watch_cancel().unwrap().is_none());
    }
}
