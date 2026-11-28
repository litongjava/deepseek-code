//! Human-facing output. Preserve the caller's write policy unless the failed
//! destination is a terminal whose hangup has been independently confirmed.
use std::{
    fmt,
    io::{self, Write},
    sync::OnceLock,
};

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

struct TerminalWriter<W>(Stream, W);

impl<W: Write> Write for TerminalWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.1.write(bytes).map_err(|error| self.0.classify(error))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.1.flush().map_err(|error| self.0.classify(error))
    }
}

pub(crate) fn stderr() -> impl Write + Send {
    TerminalWriter(Stream::Stderr, io::stderr())
}
pub(crate) fn stderr_locked() -> impl Write {
    TerminalWriter(Stream::Stderr, io::stderr().lock())
}
pub(crate) fn stdout() -> impl Write + Send {
    TerminalWriter(Stream::Stdout, io::stdout())
}
pub(crate) fn stdout_locked() -> impl Write {
    TerminalWriter(Stream::Stdout, io::stdout().lock())
}

#[derive(Debug)]
struct Disconnected(io::Error);

impl fmt::Display for Disconnected {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(fmt)
    }
}
impl std::error::Error for Disconnected {}

pub(crate) fn is_disconnected(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<Disconnected>())
}

pub(crate) fn stdout_error(error: io::Error) -> io::Error {
    Stream::Stdout.classify(error)
}

impl Stream {
    fn endpoint(self) -> &'static Endpoint {
        &TERMINALS.get_or_init(capture)[self as usize]
    }

    fn classify(self, error: io::Error) -> io::Error {
        let possible_hangup = matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
        );
        #[cfg(unix)]
        let possible_hangup = possible_hangup || error.raw_os_error() == Some(libc::EIO);
        #[cfg(windows)]
        let possible_hangup =
            possible_hangup || matches!(error.raw_os_error(), Some(6 | 233 | 1167));
        if possible_hangup && self.endpoint().notify_if_lost(self) {
            return io::Error::new(error.kind(), Disconnected(error));
        }
        error
    }
}

// A hung-up PTY slave no longer answers isatty/tcgetpgrp. Remember its
// identity while live, then check the *same* endpoint on a failed write.
static TERMINALS: OnceLock<[Endpoint; 2]> = OnceLock::new();
fn capture() -> [Endpoint; 2] {
    [Stream::Stdout, Stream::Stderr].map(Endpoint::capture)
}
pub(crate) fn capture_terminals() {
    TERMINALS.get_or_init(capture);
}

#[cfg(unix)]
use platform::Endpoint;
#[cfg(unix)]
mod platform {
    use super::Stream;
    use std::os::fd::RawFd;

    type Device = (libc::dev_t, libc::ino_t, libc::dev_t);

    fn device(fd: RawFd) -> Option<Device> {
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return None;
        }
        let stat = unsafe { stat.assume_init() };
        Some((stat.st_dev, stat.st_ino, stat.st_rdev))
    }

    impl Stream {
        fn fd(self) -> RawFd {
            match self {
                Self::Stdout => libc::STDOUT_FILENO,
                Self::Stderr => libc::STDERR_FILENO,
            }
        }
    }

    pub(super) struct Endpoint {
        device: Option<Device>,
        controlling: bool,
    }

    impl Endpoint {
        pub(super) fn capture(stream: Stream) -> Self {
            Self::from_fd(stream.fd(), libc::STDIN_FILENO)
        }

        pub(super) fn from_fd(fd: RawFd, input: RawFd) -> Self {
            let session = unsafe { libc::tcgetsid(fd) };
            let controlling = session >= 0 && session == unsafe { libc::getsid(0) };
            let id = device(fd).filter(|id| {
                controlling || (unsafe { libc::isatty(input) } == 1 && Some(*id) == device(input))
            });
            Self {
                device: id,
                controlling,
            }
        }

        pub(super) fn is_terminal(&self) -> bool {
            self.device.is_some()
        }
        pub(super) fn notify_if_lost(&self, stream: Stream) -> bool {
            if !self.hung_up_at(stream.fd(), libc::STDIN_FILENO) {
                return false;
            }
            if self.controlling {
                crate::signals::notify_terminal_loss();
                true
            } else {
                // A shared non-controlling PTY can only stop an interactive
                // frontend through its independently watched input.
                crate::signals::check_terminal_hangup()
            }
        }

        pub(super) fn hung_up_at(&self, fd: RawFd, input: RawFd) -> bool {
            let Some(original) = self.device else {
                return false;
            };
            if device(fd) != Some(original) {
                return false;
            }
            let mut poll = libc::pollfd {
                fd,
                events: 0,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 0) } <= 0 || poll.revents & libc::POLLHUP == 0 {
                return false;
            }
            // Some hosts attach all streams to a PTY without assigning a
            // controlling terminal. Its shared input must itself report HUP.
            if self.controlling {
                return true;
            }
            device(input) == Some(original)
        }
    }
}

#[cfg(windows)]
use platform::Endpoint;
#[cfg(windows)]
mod platform {
    use super::Stream;
    use std::{ffi::c_void, io, os::windows::io::AsRawHandle};
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn GetConsoleCP() -> u32;
    }
    impl Stream {
        fn handle(self) -> *mut c_void {
            match self {
                Self::Stdout => io::stdout().as_raw_handle(),
                Self::Stderr => io::stderr().as_raw_handle(),
            }
        }
    }
    pub(super) struct Endpoint(Option<usize>);
    impl Endpoint {
        pub(super) fn capture(stream: Stream) -> Self {
            let handle = stream.handle();
            Self((unsafe { GetConsoleMode(handle, &mut 0) } != 0).then_some(handle as usize))
        }
        pub(super) fn is_terminal(&self) -> bool {
            self.0.is_some()
        }
        pub(super) fn notify_if_lost(&self, stream: Stream) -> bool {
            let handle = stream.handle();
            let detached = self.0 == Some(handle as usize)
                && unsafe { GetConsoleMode(handle, &mut 0) == 0 && GetConsoleCP() == 0 };
            if detached {
                crate::signals::notify_terminal_loss();
            }
            detached
        }
    }
}

// Screen-reader print failures used to unwind the frontend. Keep that fatal
// disposition while allowing the turn and SessionEnd hook to finish first.
#[derive(Clone, Debug)]
pub(crate) struct StdoutWriteFailed(std::sync::Arc<io::Error>);
impl fmt::Display for StdoutWriteFailed {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "failed printing to stdout: {}", self.0)
    }
}
impl std::error::Error for StdoutWriteFailed {}
pub(crate) fn stream_write_error(error: io::Error) -> anyhow::Error {
    if is_disconnected(&error) {
        error.into()
    } else {
        StdoutWriteFailed(error.into()).into()
    }
}

pub(crate) fn finish_stream_line() -> anyhow::Result<()> {
    let mut out = stdout_locked();
    // The newline uses print semantics; the explicit flush is fallible.
    if let Err(error) = writeln!(out) {
        if !is_disconnected(&error) {
            return Err(stream_write_error(error));
        }
    } else if let Err(error) = out.flush() {
        if !is_disconnected(&error) {
            return Err(error.into());
        }
    }
    Ok(())
}

// Keep every formatted display write inside the same boundary. Machine
// encoders and renderers that return Result continue to propagate errors.
macro_rules! print {
    ($($arg:tt)*) => { $crate::terminal_output::print_stdout(format_args!($($arg)*)) };
}
macro_rules! println {
    () => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format_args!($($arg)*)) };
}
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::terminal_output::print_stderr(format_args!($($arg)*)) };
}
macro_rules! eprintln {
    () => { eprint!("\n") };
    ($($arg:tt)*) => { eprint!("{}\n", format_args!($($arg)*)) };
}

pub(crate) fn print_stdout(args: std::fmt::Arguments<'_>) {
    if Stream::Stdout.endpoint().is_terminal() {
        display_result("stdout", stdout_locked().write_fmt(args));
    } else {
        std::print!("{args}");
    }
}

pub(crate) fn print_stderr(args: std::fmt::Arguments<'_>) {
    if Stream::Stderr.endpoint().is_terminal() {
        display_result("stderr", stderr_locked().write_fmt(args));
    } else {
        std::eprint!("{args}");
    }
}

fn display_result(name: &str, result: io::Result<()>) {
    if let Err(error) = result {
        // Preserve std's behavior for unrelated failures, e.g. redirected
        // output on a full filesystem.
        if !is_disconnected(&error) {
            panic!("failed printing to {name}: {error}");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
