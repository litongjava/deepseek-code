//! `dcl remote-control connect`: attaches this terminal to a dcl session that
//! another machine shares with `/remote-control listen`. The whole dcl screen
//! is mirrored, so every command, menu and shortcut works as on the host;
//! prompts, tools and file changes run there. Ctrl+] disconnects and leaves
//! the session running on the host.
use crate::terminal::input::TerminalInput;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use deepseek_code_core::services::remote_control::peer::{self, Token};
use serde_json::json;
use std::{
    io::Write,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Ctrl+]: disconnect, as in telnet.
const DETACH: u8 = 0x1d;

pub async fn run(address: &str, token: Option<&str>) -> Result<()> {
    let token = match token.filter(|token| !token.trim().is_empty()) {
        Some(token) => token.to_owned(),
        None => TerminalInput::new()
            .read_secret("Token: ")
            .await?
            .filter(|token| !token.trim().is_empty())
            .context("Remote Control needs the token shown by /remote-control listen")?,
    };
    let token = Token::parse(&token)?;
    eprintln!("Connecting to {address}…");
    let mut channel = tokio::select! {
        channel = peer::connect(address, &token) => channel?,
        _ = tokio::signal::ctrl_c() => bail!("Connection cancelled"),
    };
    let hello = tokio::time::timeout(peer::HANDSHAKE_TIMEOUT, channel.recv())
        .await
        .context("The host did not answer")??
        .context("The host closed the connection")?;
    match hello["type"].as_str() {
        Some("hello") => {}
        Some("error") => bail!(
            "{}",
            hello["message"].as_str().unwrap_or("Rejected by the host")
        ),
        _ => bail!("Unexpected answer from the host"),
    }
    eprintln!(
        "Connected to {}: {}. Ctrl+] disconnects.",
        hello["machine"].as_str().unwrap_or_default(),
        hello["cwd"].as_str().unwrap_or_default()
    );

    crossterm::terminal::enable_raw_mode()?;
    let guard = Terminal;
    let mut size = terminal_size();
    channel
        .send(&json!({"type": "size", "cols": size.0, "rows": size.1}))
        .await?;
    let mut input = spawn_input();
    let mut stdout = std::io::stdout();
    let mut size_check = tokio::time::interval(Duration::from_millis(250));
    let mut keep_alive = tokio::time::interval(peer::KEEP_ALIVE);
    let mut last_seen = Instant::now();
    let ending = loop {
        tokio::select! {
            frame = channel.recv() => {
                let frame = match frame {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break Ending::Lost("The host closed the connection".into()),
                    Err(error) => break Ending::Lost(format!("{error:#}")),
                };
                last_seen = Instant::now();
                match frame["type"].as_str() {
                    Some("output") => {
                        if let Some(data) = frame["data"].as_str().and_then(|data| BASE64.decode(data).ok()) {
                            stdout.write_all(&data)?;
                            stdout.flush()?;
                        }
                    }
                    Some("exit") => break Ending::Exited(frame["code"].as_i64().unwrap_or(0)),
                    Some("error") => break Ending::Lost(frame["message"].as_str().unwrap_or("Disconnected by the host").to_owned()),
                    _ => {}
                }
            },
            bytes = input.recv() => {
                let Some(bytes) = bytes else { break Ending::Detached };
                let (send, detach) = match bytes.iter().position(|byte| *byte == DETACH) {
                    Some(index) => (&bytes[..index], true),
                    None => (&bytes[..], false),
                };
                if !send.is_empty() {
                    channel.send(&json!({"type": "input", "data": BASE64.encode(send)})).await?;
                }
                if detach { break Ending::Detached; }
            },
            _ = size_check.tick() => {
                let current = terminal_size();
                if current != size {
                    size = current;
                    channel.send(&json!({"type": "size", "cols": size.0, "rows": size.1})).await?;
                }
            },
            _ = keep_alive.tick() => {
                if last_seen.elapsed() > peer::IDLE_TIMEOUT { break Ending::Lost("The host stopped answering".into()); }
                channel.send(&json!({"type": "keep_alive"})).await?;
            },
        }
    };
    drop(guard);
    match ending {
        Ending::Detached => {
            eprintln!("Disconnected. The session continues on {address}; run this command again to reconnect.");
            Ok(())
        }
        Ending::Exited(code) => {
            eprintln!("The session ended on the host.");
            if code != 0 {
                std::process::exit(code as i32);
            }
            Ok(())
        }
        Ending::Lost(reason) => bail!(
            "{reason}. The session continues on {address}; run this command again to reconnect."
        ),
    }
}

enum Ending {
    Detached,
    Exited(i64),
    Lost(String),
}

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size()
        .ok()
        .filter(|(cols, rows)| *cols > 0 && *rows > 0)
        .unwrap_or((80, 24))
}

/// Leaves raw mode and undoes modes the remote dcl may have left enabled.
struct Terminal;

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stderr = std::io::stderr();
        let _ = stderr.write_all(
            b"\x1b[0m\x1b[?25h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1l\x1b[999;1H\r\n",
        );
        let _ = stderr.flush();
    }
}

/// Keyboard, mouse and paste input as terminal bytes.
fn spawn_input() -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        #[cfg(unix)]
        {
            use std::io::Read;
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
            let bytes = event_bytes(&event);
            if !bytes.is_empty() && sender.send(bytes).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Encodes a key or paste event as the bytes a terminal would send. Used
/// where the console delivers events rather than bytes.
#[cfg_attr(unix, allow(dead_code))]
pub fn event_bytes(event: &crossterm::event::Event) -> Vec<u8> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    let key = match event {
        Event::Paste(text) => return [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat(),
        Event::Key(key) if key.kind != KeyEventKind::Release => key,
        _ => return Vec::new(),
    };
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut bytes = match key.code {
        KeyCode::Char(c) if control && c.is_ascii_alphabetic() => {
            vec![c.to_ascii_lowercase() as u8 - b'a' + 1]
        }
        KeyCode::Char(']') if control => vec![DETACH],
        KeyCode::Char(' ') if control => vec![0],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + n - 1],
        KeyCode::F(n @ 5..=12) => {
            format!("\x1b[{}~", [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]).into_bytes()
        }
        _ => Vec::new(),
    };
    if alt && !bytes.is_empty() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn console_events_become_terminal_bytes() {
        assert_eq!(
            event_bytes(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            b"a"
        );
        assert_eq!(
            event_bytes(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            b"\x03"
        );
        assert_eq!(
            event_bytes(&key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            [DETACH]
        );
        assert_eq!(
            event_bytes(&key(KeyCode::Char('x'), KeyModifiers::ALT)),
            b"\x1bx"
        );
        assert_eq!(
            event_bytes(&key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            b"\x1b[Z"
        );
        assert_eq!(
            event_bytes(&key(KeyCode::Up, KeyModifiers::NONE)),
            b"\x1b[A"
        );
        assert_eq!(
            event_bytes(&key(KeyCode::F(5), KeyModifiers::NONE)),
            b"\x1b[15~"
        );
        assert_eq!(
            event_bytes(&Event::Paste("hi".into())),
            b"\x1b[200~hi\x1b[201~"
        );
        assert_eq!(event_bytes(&Event::FocusGained), b"");
    }
}
