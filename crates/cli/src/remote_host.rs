//! `/remote-control listen`: shares this dcl with a dcl client on another
//! machine. The session continues in a child dcl (`--resume`) inside a
//! pseudo-terminal; this process relays that terminal to the local one and to
//! one remote client, which sees and drives the same screen. A client can
//! disconnect and reconnect at any time; the session keeps running here.
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use deepseek_code_core::services::remote_control::peer::{self, Channel, Token};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};

/// Set in the child dcl: `PORT:SECRET` of this process's loopback control socket.
pub const HOST_ENV: &str = "DCL_REMOTE_CONTROL_HOST";
const SCROLLBACK: usize = 10_000;
const CLIENT_QUEUE: usize = 4096;

/// Where to listen, from `/remote-control listen [--host ADDR] [--port PORT]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub host: IpAddr,
    pub port: u16,
}

impl Options {
    pub fn parse(words: &[&str]) -> Result<Self> {
        const USAGE: &str = "Usage: /remote-control listen [--host ADDR] [--port PORT]";
        let mut options = Self {
            host: IpAddr::from([0, 0, 0, 0]),
            port: peer::DEFAULT_PORT,
        };
        let mut rest = words.iter();
        while let Some(word) = rest.next() {
            match *word {
                "--host" => {
                    let host = rest.next().context(USAGE)?;
                    options.host = host
                        .trim_matches(['[', ']'])
                        .parse()
                        .with_context(|| format!("Invalid --host {host}"))?;
                }
                "--port" => {
                    options.port = rest
                        .next()
                        .context(USAGE)?
                        .parse()
                        .context("Invalid --port")?
                }
                other => anyhow::bail!("Unknown argument {other}. {USAGE}"),
            }
        }
        Ok(options)
    }
}

/// A bound listener and its token.
pub struct Listening {
    listener: TcpListener,
    token: Token,
    pub url: String,
}

impl Listening {
    pub async fn bind(options: &Options) -> Result<Self> {
        let address = SocketAddr::new(options.host, options.port);
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("Cannot listen on {address}"))?;
        let bound = listener.local_addr()?;
        Ok(Self {
            listener,
            token: Token::generate()?,
            url: SocketAddr::new(peer::reachable_ip(bound.ip()), bound.port()).to_string(),
        })
    }

    pub fn banner(&self) -> String {
        format!(
            "Remote Control listening on {}\nToken: {}\nOn the other machine: dcl remote-control connect {}",
            self.url, self.token, self.url
        )
    }
}

/// Arguments that restart this dcl on the same session: the original ones
/// without session selection or an initial prompt, plus `--resume ID`.
pub fn child_args(
    original: &[String],
    prompt: Option<&str>,
    session: &str,
    workspace: &Path,
) -> Vec<String> {
    let mut args = Vec::new();
    let mut prompt = prompt;
    let mut rest = original.iter().peekable();
    while let Some(arg) = rest.next() {
        let flag = arg.split('=').next().unwrap_or(arg);
        let inline = arg.contains('=');
        match flag {
            "-r" | "--resume" | "--session-id" | "--name" | "-C" | "--path" => {
                if !inline {
                    rest.next();
                }
            }
            "-w" | "--worktree" => {
                if !inline
                    && rest
                        .peek()
                        .is_some_and(|next| !next.starts_with('-') && Some(next.as_str()) != prompt)
                {
                    rest.next();
                }
            }
            "-c" | "--continue" | "--fork-session" | "--auto-route" => {}
            _ if prompt == Some(arg.as_str()) => prompt = None,
            _ => args.push(arg.clone()),
        }
    }
    args.extend([
        "-C".into(),
        workspace.to_string_lossy().into_owned(),
        "--resume".into(),
        session.into(),
    ]);
    args
}

/// A terminal query the host answers itself, so the local terminal and the
/// remote one do not both reply to the child.
#[derive(Debug, PartialEq)]
enum Piece {
    Data(Vec<u8>),
    CursorPosition,
    DeviceAttributes,
}

const QUERIES: [(&[u8], Option<fn() -> Piece>); 4] = [
    (b"\x1b[6n", Some(|| Piece::CursorPosition)),
    (b"\x1b[c", Some(|| Piece::DeviceAttributes)),
    (b"\x1b[0c", Some(|| Piece::DeviceAttributes)),
    // Keyboard enhancement probe: unanswered, as by a basic terminal.
    (b"\x1b[?u", None),
];

/// Splits child output into data and queries; keeps a query split across reads.
#[derive(Default)]
struct QueryFilter {
    carry: Vec<u8>,
}

impl QueryFilter {
    fn feed(&mut self, chunk: &[u8]) -> Vec<Piece> {
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(chunk);
        let mut pieces = Vec::new();
        let mut data = Vec::new();
        let mut index = 0;
        'scan: while index < input.len() {
            if input[index] == 0x1b {
                let rest = &input[index..];
                for (query, piece) in QUERIES {
                    if rest.starts_with(query) {
                        if !data.is_empty() {
                            pieces.push(Piece::Data(std::mem::take(&mut data)));
                        }
                        if let Some(piece) = piece {
                            pieces.push(piece());
                        }
                        index += query.len();
                        continue 'scan;
                    }
                }
                if QUERIES.iter().any(|(query, _)| query.starts_with(rest)) {
                    self.carry = rest.to_vec();
                    break;
                }
            }
            data.push(input[index]);
            index += 1;
        }
        if !data.is_empty() {
            pieces.push(Piece::Data(data));
        }
        pieces
    }
}

/// Bytes that redraw the session on a newly attached client: the scrollback,
/// then the screen, cursor and input modes.
fn snapshot(parser: &mut vt100::Parser) -> Vec<u8> {
    let (rows, cols) = parser.screen().size();
    let mut out = b"\x1b[0m\r\n".to_vec();
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let history = screen.scrollback();
    for offset in (1..=history).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = screen.rows_formatted(0, cols).next() {
            out.extend(row);
            out.extend(b"\x1b[0m\r\n");
        }
    }
    screen.set_scrollback(0);
    // Scroll the client's screen clear so the session starts at its top left.
    out.extend(b"\r\n".repeat(rows as usize));
    out.extend(screen.state_formatted());
    out.extend(screen.cursor_state_formatted());
    out.extend(screen.attributes_formatted());
    out
}

#[derive(Debug)]
enum FromClient {
    Input(Vec<u8>),
    Size(u16, u16),
    Closed(String),
}

/// One remote client; its connection runs in a task so a slow network never
/// stalls the local terminal.
struct Client {
    peer: SocketAddr,
    outbound: mpsc::Sender<Value>,
    inbound: mpsc::Receiver<FromClient>,
    size: Option<(u16, u16)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    fn start(mut channel: Channel) -> Self {
        let peer = channel.peer;
        let (outbound, mut queue) = mpsc::channel::<Value>(CLIENT_QUEUE);
        let (events, inbound) = mpsc::channel(256);
        let task = tokio::spawn(async move {
            let mut keep_alive = tokio::time::interval(peer::KEEP_ALIVE);
            let mut last_seen = Instant::now();
            let reason = loop {
                tokio::select! {
                    frame = queue.recv() => match frame {
                        Some(frame) => if let Err(error) = channel.send(&frame).await { break format!("{error:#}") },
                        None => return,
                    },
                    frame = channel.recv() => match frame {
                        Ok(Some(frame)) => {
                            last_seen = Instant::now();
                            let event = match frame["type"].as_str() {
                                Some("input") => frame["data"].as_str().and_then(|data| BASE64.decode(data).ok()).map(FromClient::Input),
                                Some("size") => match (frame["cols"].as_u64(), frame["rows"].as_u64()) {
                                    (Some(cols), Some(rows)) if cols > 0 && rows > 0 => Some(FromClient::Size(cols.min(1000) as u16, rows.min(1000) as u16)),
                                    _ => None,
                                },
                                _ => None,
                            };
                            if let Some(event) = event {
                                if events.send(event).await.is_err() { return; }
                            }
                        }
                        Ok(None) => break "disconnected".into(),
                        Err(error) => break format!("{error:#}"),
                    },
                    _ = keep_alive.tick() => {
                        if last_seen.elapsed() > peer::IDLE_TIMEOUT { break "timed out".into(); }
                        if let Err(error) = channel.send(&json!({"type": "keep_alive"})).await { break format!("{error:#}") }
                    },
                }
            };
            let _ = events.send(FromClient::Closed(reason)).await;
        });
        Self {
            peer,
            outbound,
            inbound,
            size: None,
            task,
        }
    }

    /// False when the client cannot keep up and must be dropped.
    fn send(&self, frame: Value) -> bool {
        self.outbound.try_send(frame).is_ok()
    }
}

/// A request from the child dcl's `/remote-control` command.
enum Control {
    Status(oneshot::Sender<String>),
    Listen(Options, oneshot::Sender<Result<String, String>>),
    Stop(oneshot::Sender<String>),
    Subscribe(mpsc::UnboundedSender<String>),
}

async fn serve_control(
    stream: TcpStream,
    secret: String,
    requests: mpsc::Sender<Control>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Value = serde_json::from_str(&line)?;
    anyhow::ensure!(request["secret"] == secret.as_str(), "Bad control secret");
    let reply = |text: Result<String, String>| match text {
        Ok(text) => json!({"ok": true, "text": text}),
        Err(error) => json!({"ok": false, "text": error}),
    };
    let response = match request["op"].as_str().unwrap_or_default() {
        "subscribe" => {
            let (sender, mut notices) = mpsc::unbounded_channel();
            requests.send(Control::Subscribe(sender)).await?;
            while let Some(notice) = notices.recv().await {
                write
                    .write_all(format!("{}\n", json!({"notice": notice})).as_bytes())
                    .await?;
            }
            return Ok(());
        }
        "status" => {
            let (sender, answer) = oneshot::channel();
            requests.send(Control::Status(sender)).await?;
            reply(Ok(answer.await?))
        }
        "stop" => {
            let (sender, answer) = oneshot::channel();
            requests.send(Control::Stop(sender)).await?;
            reply(Ok(answer.await?))
        }
        "listen" => {
            let words: Vec<String> =
                serde_json::from_value(request["args"].clone()).unwrap_or_default();
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            match Options::parse(&words) {
                Ok(options) => {
                    let (sender, answer) = oneshot::channel();
                    requests.send(Control::Listen(options, sender)).await?;
                    reply(answer.await?)
                }
                Err(error) => reply(Err(format!("{error:#}"))),
            }
        }
        other => reply(Err(format!("Unknown request {other}"))),
    };
    write.write_all(format!("{response}\n").as_bytes()).await?;
    Ok(())
}

/// Sends one request to the host from the child dcl.
pub async fn control(op: &str, args: &[&str]) -> Result<String> {
    let (port, secret) = host_address().context("Not running under /remote-control listen")?;
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{}\n", json!({"secret": secret, "op": op, "args": args})).as_bytes())
        .await?;
    let line = BufReader::new(read)
        .lines()
        .next_line()
        .await?
        .context("Remote Control host did not answer")?;
    let response: Value = serde_json::from_str(&line)?;
    let text = response["text"].as_str().unwrap_or_default().to_owned();
    anyhow::ensure!(response["ok"] == true, "{text}");
    Ok(text)
}

/// Host notices (client connected, disconnected) for the child dcl.
pub async fn subscribe() -> Result<mpsc::UnboundedReceiver<String>> {
    let (port, secret) = host_address().context("Not running under /remote-control listen")?;
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{}\n", json!({"secret": secret, "op": "subscribe"})).as_bytes())
        .await?;
    let (sender, notices) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(notice) = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|value| value["notice"].as_str().map(str::to_owned))
            {
                if sender.send(notice).is_err() {
                    break;
                }
            }
        }
    });
    Ok(notices)
}

pub fn host_address() -> Option<(u16, String)> {
    let value = std::env::var(HOST_ENV).ok()?;
    let (port, secret) = value.split_once(':')?;
    Some((port.parse().ok()?, secret.to_owned()))
}

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size()
        .ok()
        .filter(|(cols, rows)| *cols > 0 && *rows > 0)
        .unwrap_or((80, 24))
}

/// Restores the local terminal however the relay ends.
struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Local keyboard input as terminal bytes.
fn spawn_local_input() -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        #[cfg(unix)]
        {
            let mut stdin = std::io::stdin();
            let mut buffer = [0u8; 4096];
            while let Ok(count) = stdin.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        }
        #[cfg(not(unix))]
        while let Ok(event) = crossterm::event::read() {
            let bytes = crate::remote_client::event_bytes(&event);
            if !bytes.is_empty() && sender.send(bytes).is_err() {
                break;
            }
        }
    });
    receiver
}

struct Host {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: vt100::Parser,
    size: (u16, u16),
    local: (u16, u16),
    listening: Option<Listening>,
    client: Option<Client>,
    /// The client attached but has not sent its size, so it has no snapshot yet.
    awaiting_size: bool,
    subscribers: Vec<mpsc::UnboundedSender<String>>,
    machine: String,
    workspace: PathBuf,
}

impl Host {
    fn notify(&mut self, notice: String) {
        self.subscribers
            .retain(|subscriber| subscriber.send(notice.clone()).is_ok());
    }

    fn write_child(&mut self, bytes: &[u8]) {
        let _ = self
            .writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush());
    }

    /// The pty follows the smaller of the local and remote terminals.
    fn resize(&mut self) {
        let size = match self.client.as_ref().and_then(|client| client.size) {
            Some((cols, rows)) => (cols.min(self.local.0), rows.min(self.local.1)),
            None => self.local,
        };
        if size != self.size {
            self.size = size;
            self.parser.screen_mut().set_size(size.1, size.0);
            let _ = self.master.resize(PtySize {
                rows: size.1,
                cols: size.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    fn drop_client(&mut self, reason: &str) {
        if let Some(client) = self.client.take() {
            self.awaiting_size = false;
            self.notify(format!("Remote Control client {} {reason}.", client.peer));
            self.resize();
        }
    }

    fn send_client(&mut self, frame: Value) {
        if self
            .client
            .as_ref()
            .is_some_and(|client| !client.send(frame))
        {
            self.drop_client("could not keep up and was disconnected");
        }
    }

    fn status(&self) -> String {
        match (&self.listening, &self.client) {
            (None, _) => "Remote Control is not listening.".into(),
            (Some(listening), Some(client)) => format!(
                "Remote Control listening on {}: client {}",
                listening.url, client.peer
            ),
            (Some(listening), None) => format!(
                "Remote Control listening on {}: waiting for a client",
                listening.url
            ),
        }
    }

    fn attach(&mut self, channel: Channel) {
        if self.client.is_some() {
            // The newer connection wins, so a client can reconnect at once
            // after its network dropped without a clean close.
            self.drop_client("was replaced by a new connection");
        }
        let client = Client::start(channel);
        let peer = client.peer;
        client.send(json!({
            "type": "hello",
            "machine": self.machine,
            "cwd": self.workspace.to_string_lossy(),
        }));
        self.client = Some(client);
        self.awaiting_size = true;
        self.notify(format!("Remote Control client connected from {peer}."));
    }

    fn client_size(&mut self, cols: u16, rows: u16) {
        if let Some(client) = self.client.as_mut() {
            client.size = Some((cols, rows));
        }
        self.resize();
        if std::mem::take(&mut self.awaiting_size) {
            let screen = snapshot(&mut self.parser);
            self.send_client(json!({"type": "output", "data": BASE64.encode(screen)}));
        }
    }

    fn output(&mut self, chunk: &[u8], filter: &mut QueryFilter, stdout: &mut impl Write) {
        for piece in filter.feed(chunk) {
            match piece {
                Piece::Data(data) => {
                    self.parser.process(&data);
                    let _ = stdout.write_all(&data);
                    if self.client.is_some() && !self.awaiting_size {
                        self.send_client(json!({"type": "output", "data": BASE64.encode(&data)}));
                    }
                }
                Piece::CursorPosition => {
                    let (row, col) = self.parser.screen().cursor_position();
                    self.write_child(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                Piece::DeviceAttributes => self.write_child(b"\x1b[?62;22c"),
            }
        }
        let _ = stdout.flush();
    }
}

/// Runs the session in a child dcl and relays it until the child exits.
/// Returns the child's exit code.
pub async fn run(listening: Listening, args: Vec<String>, workspace: &Path) -> Result<i32> {
    let local = terminal_size();
    let pty = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: local.1,
            cols: local.0,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| anyhow::anyhow!("Cannot open a pseudo-terminal: {error}"))?;
    let control = TcpListener::bind(("127.0.0.1", 0)).await?;
    let secret = uuid::Uuid::new_v4().simple().to_string();
    let mut command = CommandBuilder::new(std::env::current_exe()?);
    command.args(&args);
    command.cwd(workspace);
    command.env(
        HOST_ENV,
        format!("{}:{secret}", control.local_addr()?.port()),
    );
    // The emulator starts where the local cursor is, so its answers to cursor
    // queries match what the local terminal shows.
    let start_row = crossterm::cursor::position().map_or(0, |(_, row)| row);
    let mut child = pty
        .slave
        .spawn_command(command)
        .map_err(|error| anyhow::anyhow!("Cannot start the session: {error}"))?;
    drop(pty.slave);
    let mut reader = pty
        .master
        .try_clone_reader()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let writer = pty
        .master
        .take_writer()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (output_sender, mut output) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16384];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || output_sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let (exit_sender, mut exited) = oneshot::channel();
    std::thread::spawn(move || {
        let code = child
            .wait()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(1);
        let _ = exit_sender.send(code);
    });

    let mut parser = vt100::Parser::new(local.1, local.0, SCROLLBACK);
    parser.process(format!("\x1b[{};1H", start_row + 1).as_bytes());
    let mut host = Host {
        master: pty.master,
        writer,
        parser,
        size: local,
        local,
        listening: Some(listening),
        client: None,
        awaiting_size: false,
        subscribers: Vec::new(),
        machine: std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "localhost".into()),
        workspace: workspace.to_owned(),
    };
    crossterm::terminal::enable_raw_mode()?;
    let _raw = RawMode;
    let mut input = spawn_local_input();
    let (requests_sender, mut requests) = mpsc::channel::<Control>(16);
    let (authenticated, mut handshakes) = mpsc::channel::<Channel>(8);
    let mut filter = QueryFilter::default();
    let mut stdout = std::io::stdout();
    let mut size_check = tokio::time::interval(Duration::from_millis(250));
    let mut output_open = true;
    let code = loop {
        tokio::select! {
            chunk = output.recv(), if output_open => match chunk {
                Some(chunk) => host.output(&chunk, &mut filter, &mut stdout),
                None => output_open = false,
            },
            code = &mut exited => {
                // Show what the child printed last.
                while let Ok(chunk) = output.try_recv() { host.output(&chunk, &mut filter, &mut stdout); }
                break code.unwrap_or(1);
            },
            Some(bytes) = input.recv() => host.write_child(&bytes),
            accepted = async { host.listening.as_ref().unwrap().listener.accept().await }, if host.listening.is_some() => {
                if let Ok((stream, address)) = accepted {
                    let token = host.listening.as_ref().unwrap().token.clone();
                    let authenticated = authenticated.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(peer::HANDSHAKE_TIMEOUT, peer::handshake(stream, &token, false)).await {
                            Ok(Ok(channel)) => { let _ = authenticated.send(channel).await; }
                            Ok(Err(error)) => tracing::warn!("Remote Control rejected {address}: {error:#}"),
                            Err(_) => tracing::warn!("Remote Control handshake with {address} timed out"),
                        }
                    });
                }
            },
            Some(channel) = handshakes.recv() => {
                if host.listening.is_some() { host.attach(channel); }
            },
            Some(event) = async { host.client.as_mut().unwrap().inbound.recv().await }, if host.client.is_some() => match event {
                FromClient::Input(bytes) => host.write_child(&bytes),
                FromClient::Size(cols, rows) => host.client_size(cols, rows),
                FromClient::Closed(reason) => host.drop_client(&reason),
            },
            accepted = control.accept() => {
                if let Ok((stream, _)) = accepted {
                    let secret = secret.clone();
                    let requests = requests_sender.clone();
                    tokio::spawn(async move {
                        if let Err(error) = serve_control(stream, secret, requests).await {
                            tracing::warn!("Remote Control control request failed: {error:#}");
                        }
                    });
                }
            },
            Some(request) = requests.recv() => match request {
                Control::Status(reply) => { let _ = reply.send(host.status()); },
                Control::Subscribe(subscriber) => host.subscribers.push(subscriber),
                Control::Stop(reply) => {
                    let text = if host.listening.take().is_some() {
                        host.drop_client("was disconnected");
                        "Remote Control stopped listening."
                    } else {
                        "Remote Control is not listening."
                    };
                    let _ = reply.send(text.into());
                },
                Control::Listen(options, reply) => {
                    let result = match &host.listening {
                        Some(_) => Ok(host.status()),
                        None => match Listening::bind(&options).await {
                            Ok(listening) => {
                                let banner = listening.banner();
                                host.listening = Some(listening);
                                Ok(banner)
                            }
                            Err(error) => Err(format!("{error:#}")),
                        },
                    };
                    let _ = reply.send(result);
                },
            },
            _ = size_check.tick() => {
                let local = terminal_size();
                if local != host.local {
                    host.local = local;
                    host.resize();
                }
            },
        }
    };
    if let Some(client) = host.client.take() {
        // Let the client show that the session ended.
        let _ = client
            .outbound
            .send(json!({"type": "exit", "code": code}))
            .await;
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while !client.outbound.is_closed() && client.outbound.capacity() < CLIENT_QUEUE {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn child_resumes_the_session_and_keeps_other_options() {
        let workspace = Path::new("/work/w");
        let args = child_args(
            &strings(&[
                "--data-dir",
                "/d",
                "-c",
                "--model",
                "m",
                "--name",
                "x",
                "fix it",
                "--resume=old",
                "-C",
                "/p",
            ]),
            Some("fix it"),
            "s1",
            workspace,
        );
        assert_eq!(
            args,
            strings(&[
                "--data-dir",
                "/d",
                "--model",
                "m",
                "-C",
                "/work/w",
                "--resume",
                "s1"
            ])
        );
        let args = child_args(
            &strings(&["-w", "tree", "--base-url", "u"]),
            None,
            "s2",
            workspace,
        );
        assert_eq!(
            args,
            strings(&["--base-url", "u", "-C", "/work/w", "--resume", "s2"])
        );
        let args = child_args(
            &strings(&["-w", "--session-id", "id"]),
            None,
            "s3",
            workspace,
        );
        assert_eq!(args, strings(&["-C", "/work/w", "--resume", "s3"]));
    }

    #[test]
    fn listen_options_parse() {
        assert_eq!(
            Options::parse(&[]).unwrap(),
            Options {
                host: [0, 0, 0, 0].into(),
                port: 30086
            }
        );
        assert_eq!(
            Options::parse(&["--host", "[::1]", "--port", "9"]).unwrap(),
            Options {
                host: "::1".parse().unwrap(),
                port: 9
            }
        );
        assert!(Options::parse(&["--port"]).is_err());
        assert!(Options::parse(&["--port", "x"]).is_err());
        assert!(Options::parse(&["now"]).is_err());
    }

    #[test]
    fn queries_are_answered_by_the_host_even_when_split() {
        let mut filter = QueryFilter::default();
        assert_eq!(
            filter.feed(b"ab\x1b[6ncd\x1b[?u\x1b[c\x1b[1m"),
            vec![
                Piece::Data(b"ab".to_vec()),
                Piece::CursorPosition,
                Piece::Data(b"cd".to_vec()),
                Piece::DeviceAttributes,
                Piece::Data(b"\x1b[1m".to_vec()),
            ]
        );
        assert_eq!(filter.feed(b"x\x1b["), vec![Piece::Data(b"x".to_vec())]);
        assert_eq!(
            filter.feed(b"6ny"),
            vec![Piece::CursorPosition, Piece::Data(b"y".to_vec())]
        );
        assert_eq!(filter.feed(b"\x1b"), vec![]);
        assert_eq!(filter.feed(b"[2J"), vec![Piece::Data(b"\x1b[2J".to_vec())]);
    }

    #[test]
    fn snapshots_replay_history_and_screen() {
        let mut parser = vt100::Parser::new(3, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\n\x1b[1mfive");
        let replay = snapshot(&mut parser);
        let mut client = vt100::Parser::new(3, 20, 100);
        client.process(&replay);
        assert_eq!(client.screen().contents(), parser.screen().contents());
        assert_eq!(
            client.screen().cursor_position(),
            parser.screen().cursor_position()
        );
        let text = String::from_utf8_lossy(&replay);
        assert!(text.contains("one") && text.contains("two"));
    }
}
