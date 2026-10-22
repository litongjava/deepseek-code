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
}

enum Message {
    Powerup(
        claude_core::runtime::RuntimePaths,
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
    Vim(bool),
    Read(Request),
}

struct Request {
    cancelled: Arc<AtomicBool>,
    prompt: String,
    repl: bool,
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
}

impl TerminalInput {
    pub fn new() -> Self {
        let (requests, rx) = std::sync::mpsc::channel::<Message>();
        let (notices, notice_rx) = std::sync::mpsc::channel();
        let (suggestions, suggestion_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let terminal = !crate::terminal::screen_reader()
                && io::stdin().is_terminal()
                && io::stdout().is_terminal()
                && io::stderr().is_terminal()
                && std::env::var("TERM").as_deref() != Ok("dumb");
            let mut editor = editor::PromptEditor::default();
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
                    Message::Vim(on) => {
                        editor.set_vim(on);
                        continue;
                    }
                    Message::Read(request) => request,
                };
                if request.response.is_closed() {
                    continue;
                }
                let result = if !request.repl {
                    super::answer_input::read(&mut plain, &request.prompt, &request.cancelled)
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
        }
    }

    pub fn notice(&self, text: String) {
        let _ = self.notices.send(text);
    }

    /// The existing input thread owns modal keys as well as ordinary prompts.
    pub async fn powerup(
        &mut self,
        paths: claude_core::runtime::RuntimePaths,
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
    pub fn restore_draft(&self, draft: String) {
        let _ = self.requests.send(Message::Draft(draft));
    }
    pub fn set_permission_mode(&self, mode: &str) {
        let _ = self.requests.send(Message::PermissionMode(mode.into()));
    }

    /// Ends an outstanding interactive prompt read so another source can run a
    /// turn. Its draft is kept, and a line submitted meanwhile is re-queued.
    /// Returns false when the read cannot be interrupted (plain stdin).
    pub async fn yield_prompt(&mut self) -> io::Result<bool> {
        let Some(pending) = self.pending.as_mut() else { return Ok(true) };
        if !pending.repl {
            return Ok(false);
        }
        pending.cancelled.store(true, Ordering::Release);
        match tokio::time::timeout(std::time::Duration::from_millis(500), &mut pending.receive).await {
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

    pub async fn read_answer(&mut self, prompt: &str) -> io::Result<Option<String>> {
        match self.read(prompt, false).await? {
            Some(PromptEvent::Line(line)) => Ok(Some(line)),
            None => Ok(None),
            Some(PromptEvent::CyclePermission) => Err(io::Error::other(
                "Permission cycle is not a question answer",
            )),
        }
    }

    // Compatibility for voice consumers; errors/interruption terminate input.
    pub async fn recv(&mut self) -> Option<String> {
        self.read_answer("").await.ok().flatten()
    }

    async fn read(&mut self, prompt: &str, repl: bool) -> io::Result<Option<PromptEvent>> {
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
        let result = (&mut self.pending.as_mut().unwrap().receive).await;
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
                super::super::answer_input::read(&mut reader, "", &flag)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Interrupted
            );
            println!("CANCEL_ACK");
            io::stdout().flush().unwrap();
            assert_eq!(
                super::super::answer_input::read(&mut reader, "", &AtomicBool::new(false))
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
}
