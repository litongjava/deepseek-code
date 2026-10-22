//! Cancellation-aware answer input. The request thread remains the only stdin reader.
use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, terminal,
};
use std::{
    io::{self, BufReader, IsTerminal, Read, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

fn cancelled(flag: &AtomicBool) -> io::Result<()> {
    if flag.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Approval dismissed",
        ))
    } else {
        Ok(())
    }
}

/// Reads one answer. `secret` shows typed or pasted text as dots, for API keys.
pub(super) fn read(
    reader: &mut BufReader<io::Stdin>,
    prompt: &str,
    flag: &AtomicBool,
    secret: bool,
) -> io::Result<Option<String>> {
    cancelled(flag)?;
    if io::stdin().is_terminal() {
        return read_terminal(prompt, flag, secret);
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut bytes = Vec::new();
    loop {
        cancelled(flag)?;
        if reader.buffer().is_empty() && !ready()? {
            continue;
        }
        let mut byte = [0];
        match reader.read(&mut byte)? {
            0 if bytes.is_empty() => return Ok(None),
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ => bytes.push(byte[0]),
        }
    }
    cancelled(flag)?;
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

struct Raw;
impl Drop for Raw {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}
fn read_terminal(prompt: &str, flag: &AtomicBool, secret: bool) -> io::Result<Option<String>> {
    terminal::enable_raw_mode()?;
    let _raw = Raw;
    let mut reader = super::paste_input::Reader::new()?;
    let mut value = String::new();
    let safe_prompt: String = prompt
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .collect();
    eprint!("{}", safe_prompt.replace('\n', "\r\n"));
    io::stderr().flush()?;
    loop {
        if let Err(error) = cancelled(flag) {
            // Keep prior prompt text but discard the canceled answer draft. No
            // global input flush: queued events for later prompts remain intact.
            if super::screen_reader() {
                eprint!("\r\n");
                io::stderr().flush()?;
            } else {
                execute!(
                    io::stderr(),
                    terminal::Clear(terminal::ClearType::CurrentLine),
                    cursor::MoveToColumn(0)
                )?;
            }
            return Err(error);
        }
        let Some(event) = reader.next(Duration::from_millis(25))? else {
            continue;
        };
        cancelled(flag)?;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('c') => {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "Input cancelled",
                            ))
                        }
                        KeyCode::Char('d') if value.is_empty() => return Ok(None),
                        _ => continue,
                    }
                }
                match key.code {
                    KeyCode::Enter => {
                        cancelled(flag)?;
                        eprint!("\r\n");
                        io::stderr().flush()?;
                        return Ok(Some(value));
                    }
                    KeyCode::Backspace => {
                        if let Some(ch) = value.pop() {
                            use unicode_width::UnicodeWidthChar;
                            let cells = if secret { 1 } else { ch.width().unwrap_or(0) };
                            for _ in 0..cells {
                                eprint!("\x08 \x08");
                            }
                        }
                    }
                    KeyCode::Char(ch) => {
                        value.push(ch);
                        if secret {
                            eprint!("\u{2022}");
                        } else {
                            eprint!("{ch}");
                        }
                    }
                    KeyCode::Esc => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Input cancelled",
                        ))
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
                value.push_str(&text);
                if secret {
                    eprint!("{}", "\u{2022}".repeat(text.chars().count()));
                } else {
                    eprint!("{text}");
                }
            }
            _ => {}
        }
        io::stderr().flush()?;
    }
}

#[cfg(unix)]
fn ready() -> io::Result<bool> {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
    }
    let mut fd = PollFd {
        fd: 0,
        events: 1,
        revents: 0,
    };
    let result = unsafe { poll(&mut fd, 1, 25) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result > 0)
    }
}
#[cfg(windows)]
fn ready() -> io::Result<bool> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetFileType(handle: *mut c_void) -> u32;
        fn PeekNamedPipe(
            handle: *mut c_void,
            buffer: *mut c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
    }
    unsafe {
        let handle = GetStdHandle(-10i32 as u32);
        if GetFileType(handle) != 3 {
            return Ok(true);
        }
        let mut available = 0;
        if PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        ) == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(109) {
                return Ok(true);
            } // EOF
            return Err(error);
        }
        if available == 0 {
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(available > 0)
    }
}
#[cfg(not(any(unix, windows)))]
fn ready() -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Cancellable input is unavailable",
    ))
}
