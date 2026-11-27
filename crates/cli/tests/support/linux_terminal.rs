use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    fs, io,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::ExitStatus,
    time::{Duration, Instant},
};

pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const CPU_BUDGET: Duration = Duration::from_millis(700);
pub(crate) const CURSOR_QUERY: &[u8] = b"\x1b[6n";
pub(crate) const PROMPT: &[u8] = "❯".as_bytes();

#[derive(Clone, Copy)]
pub(crate) enum Trigger {
    OutputError,
    Close,
    Signal(libc::c_int),
}

struct Reap {
    child: Box<dyn Child + Send + Sync>,
    pid: libc::pid_t,
    waited: bool,
}

impl Reap {
    fn signal(&self, signal: libc::c_int) {
        assert!(!self.waited, "the child has already been reaped");
        let result = unsafe { libc::kill(self.pid, signal) };
        assert_eq!(result, 0, "sending signal: {}", io::Error::last_os_error());
    }

    fn cpu_before_exit(&self) -> Duration {
        let stat = fs::read_to_string(format!("/proc/{}/stat", self.pid)).unwrap();
        let (_, fields) = stat.rsplit_once(')').unwrap();
        let ticks: u64 = fields
            .split_whitespace()
            .skip(11) // utime and stime follow eleven fields starting at state.
            .take(2)
            .map(|field| field.parse::<u64>().unwrap())
            .sum();
        let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        assert!(ticks_per_second > 0);
        Duration::from_secs_f64(ticks as f64 / ticks_per_second as f64)
    }

    fn try_reap(&mut self) -> Option<(ExitStatus, Duration)> {
        let mut status = 0;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let result =
            unsafe { libc::wait4(self.pid, &mut status, libc::WNOHANG, usage.as_mut_ptr()) };
        if result == 0
            || (result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted)
        {
            return None;
        }
        assert_eq!(
            result,
            self.pid,
            "waiting for dcl: {}",
            io::Error::last_os_error()
        );
        self.waited = true;
        let usage = unsafe { usage.assume_init() };
        let cpu = Duration::from_secs((usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64)
            + Duration::from_micros((usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as u64);
        Some((ExitStatus::from_raw(status), cpu))
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if !self.waited {
            // portable-pty starts each fixture in its own session. Fault-injection
            // tests also own a tracer, which can have a separate tracee process.
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
            let _ = self.child.kill();
            while let Err(error) = self.child.wait() {
                if error.kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

pub(crate) struct Terminal {
    process: Reap,
    master: Option<Box<dyn MasterPty + Send>>,
    pub(crate) transcript: Vec<u8>,
    answered_through: usize,
}

impl Terminal {
    pub(crate) fn command(
        mut command: CommandBuilder,
        workspace: &Path,
        data: &Path,
        coordination: &Path,
        controlling_terminal: bool,
        screen_reader: bool,
    ) -> CommandBuilder {
        command.env_clear();
        command.args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "accept-edits",
        ]);
        command.arg("--data-dir");
        command.arg(data);
        command.arg("--coordination-dir");
        command.arg(coordination);
        command.cwd(workspace);
        command.env("HOME", workspace.parent().unwrap());
        command.env("PATH", std::env::var_os("PATH").unwrap());
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        command.set_controlling_tty(controlling_terminal);
        if screen_reader {
            command.arg("--ax-screen-reader");
        }
        command
    }

    pub(crate) fn spawn(command: CommandBuilder) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let fd = pty.master.as_raw_fd().unwrap();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );

        // The master itself is the only reference. Taking a portable-pty
        // writer would keep it open and send a VEOF byte when dropped.
        let child = pty.slave.spawn_command(command).unwrap();
        let pid = child.process_id().expect("Linux child has a PID") as libc::pid_t;
        let process = Reap {
            child,
            pid,
            waited: false,
        };
        drop(pty.slave);
        Self {
            process,
            master: Some(pty.master),
            transcript: Vec::new(),
            answered_through: 0,
        }
    }

    fn send(&self, bytes: &[u8]) {
        let fd = self.master.as_ref().unwrap().as_raw_fd().unwrap();
        let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(
            count,
            bytes.len() as isize,
            "PTY write: {}",
            io::Error::last_os_error()
        );
    }

    pub(crate) fn pump(&mut self, answer_cursor: bool) {
        let Some(master) = self.master.as_ref() else {
            return;
        };
        let fd = master.as_raw_fd().unwrap();
        let mut buffer = [0_u8; 8192];
        let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count <= 0 {
            return;
        }
        self.transcript.extend_from_slice(&buffer[..count as usize]);
        if answer_cursor {
            while let Some(offset) = self.transcript[self.answered_through..]
                .windows(CURSOR_QUERY.len())
                .position(|bytes| bytes == CURSOR_QUERY)
            {
                self.answered_through += offset + CURSOR_QUERY.len();
                self.send(b"\x1b[37;3R");
            }
        }
    }

    pub(crate) async fn wait_output(&mut self, text: &[u8], answer_cursor: bool) {
        let deadline = Instant::now() + READY_TIMEOUT;
        while !self
            .transcript
            .windows(text.len())
            .any(|bytes| bytes == text)
        {
            self.pump(answer_cursor);
            assert!(
                Instant::now() < deadline,
                "missing {:?} in {}",
                String::from_utf8_lossy(text),
                String::from_utf8_lossy(&self.transcript)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) async fn submit(&mut self, line: &str, screen_reader: bool) {
        self.transcript.clear();
        self.answered_through = 0;
        self.send(line.as_bytes());
        // Wait for the draft to finish painting. An immediate Enter after a
        // text burst can be interpreted as a multiline paste.
        if !screen_reader {
            self.wait_output(line.as_bytes(), true).await;
        }
        self.send(b"\r");
    }

    pub(crate) fn pid(&self) -> u32 {
        self.process.pid as u32
    }

    pub(crate) async fn expect_exit(&mut self, trigger: Trigger) {
        let before = self.process.cpu_before_exit();
        let started = Instant::now();
        match trigger {
            Trigger::OutputError => {}
            Trigger::Close => drop(self.master.take()),
            Trigger::Signal(signal) => self.process.signal(signal),
        }
        loop {
            if let Some((status, total)) = self.process.try_reap() {
                let elapsed = started.elapsed();
                let cpu = total.saturating_sub(before);
                eprintln!("PTY exit: {status}, elapsed {elapsed:?}, CPU {cpu:?}");
                assert_ne!(status.code(), Some(101), "a dcl thread panicked");
                assert_eq!(status.code(), Some(1), "expected cooperative failure");
                assert!(elapsed < EXIT_TIMEOUT, "dcl took {elapsed:?} to exit");
                assert!(cpu < CPU_BUDGET, "dcl used {cpu:?} CPU during shutdown");
                return;
            }
            assert!(
                started.elapsed() < EXIT_TIMEOUT,
                "dcl did not exit after terminal shutdown"
            );
            self.pump(true);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// An image prompt selects the direct engine while keeping the live renderer.
pub(crate) fn image_prompt(workspace: &Path, prompt: &str) -> String {
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=",
    ).unwrap();
    fs::write(workspace.join("pixel.png"), png).unwrap();
    format!("{prompt} pixel.png")
}

pub(crate) fn write_provider(data: &Path, endpoint: &str) {
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/providers.json"),
        serde_json::json!([{
            "id":"fixture","name":"Fixture","base_url":format!("{endpoint}/v1"),
            "api_key":"fixture-only","api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                "supports_vision":false,"supports_web_search":false}]
        }])
        .to_string(),
    )
    .unwrap();
}
