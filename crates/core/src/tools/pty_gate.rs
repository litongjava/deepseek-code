//! Execution gate for the managed Linux PTY child.
//!
//! The PTY is not given to the user program directly. It is given to this
//! gate, which runs as the same host executable in a private mode. The gate
//! connects to a private Unix socket in the task directory and blocks there.
//! Only after the owner sends `GO` — which happens after the supervisor
//! acknowledged the exact terminal session identity — does the gate `exec` the
//! requested program. The gate never forks.
//!
//! If the owner disappears first, the connection reaches EOF and the gate exits
//! **without running the user program**. Before ownership exists the session
//! contains only this gate, so it cannot contain the user program or any
//! descendant; nothing has to be guessed from `/proc` afterwards.
//!
//! # Descriptor and data hygiene
//!
//! The specification travels **only over the socket**, never through inherited
//! descriptors and never through a file on disk: the complete environment can
//! contain credentials. No descriptor has to be made inheritable, so a
//! concurrently spawned child cannot inherit either end, and `portable-pty`'s
//! `close_random_fds` (which closes everything above stderr) cannot break the
//! handshake. The listener is bound inside the task directory and restricted to
//! `0600`; it is removed as soon as the gate connected.
#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use super::pty_transport::SpawnSpec;

/// Private variable of the gate process. It never reaches the user program.
pub(crate) const GATE_SOCKET_ENV: &str = "DEEPSEEK_CODE_PTY_GATE_SOCKET";

const MAGIC: &[u8; 8] = b"DCLGATE1";
/// Hard bound on the serialized specification.
const MAX_SPEC_BYTES: usize = 1 << 20;
const MAX_ENTRIES: usize = 4096;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_TICK: Duration = Duration::from_millis(2);

/// Everything the user program needs, captured before the gate starts.
pub(crate) struct GateSpec {
    pub executable: OsString,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
}

impl GateSpec {
    fn from_spawn(spec: &SpawnSpec) -> Self {
        Self {
            executable: spec.executable.clone(),
            arguments: spec.arguments.clone(),
            environment: spec.environment.clone(),
        }
    }
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn encode(spec: &GateSpec) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(MAGIC);
    put(&mut out, spec.executable.as_bytes());
    out.extend_from_slice(&(spec.arguments.len() as u32).to_be_bytes());
    for argument in &spec.arguments {
        put(&mut out, argument.as_bytes());
    }
    out.extend_from_slice(&(spec.environment.len() as u32).to_be_bytes());
    for (key, value) in &spec.environment {
        put(&mut out, key.as_bytes());
        put(&mut out, value.as_bytes());
    }
    out
}

fn take_u32(bytes: &[u8], position: &mut usize) -> Result<u32> {
    let end = position
        .checked_add(4)
        .filter(|end| *end <= bytes.len())
        .context("the gate specification is truncated")?;
    let value = u32::from_be_bytes(bytes[*position..end].try_into().unwrap());
    *position = end;
    Ok(value)
}

fn take_bytes(bytes: &[u8], position: &mut usize) -> Result<Vec<u8>> {
    let length = take_u32(bytes, position)? as usize;
    let end = position
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .context("the gate specification is truncated")?;
    let value = bytes[*position..end].to_vec();
    *position = end;
    Ok(value)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<GateSpec> {
    if bytes.len() > MAX_SPEC_BYTES || !bytes.starts_with(MAGIC) {
        bail!("the gate specification is not recognized");
    }
    let mut position = MAGIC.len();
    let executable = OsString::from_vec(take_bytes(bytes, &mut position)?);
    let arguments_len = take_u32(bytes, &mut position)? as usize;
    if arguments_len > MAX_ENTRIES {
        bail!("the gate specification has too many arguments");
    }
    let mut arguments = Vec::with_capacity(arguments_len);
    for _ in 0..arguments_len {
        arguments.push(OsString::from_vec(take_bytes(bytes, &mut position)?));
    }
    let environment_len = take_u32(bytes, &mut position)? as usize;
    if environment_len > MAX_ENTRIES {
        bail!("the gate specification has too many environment entries");
    }
    let mut environment = Vec::with_capacity(environment_len);
    for _ in 0..environment_len {
        let key = OsString::from_vec(take_bytes(bytes, &mut position)?);
        let value = OsString::from_vec(take_bytes(bytes, &mut position)?);
        environment.push((key, value));
    }
    Ok(GateSpec {
        executable,
        arguments,
        environment,
    })
}

/// The gate process and the owner's side of its private handshake.
pub(crate) struct PreparedGate {
    pub spawn: SpawnSpec,
    listener: Option<UnixListener>,
    socket_path: PathBuf,
    specification: Vec<u8>,
    stream: Option<UnixStream>,
}

impl Drop for PreparedGate {
    fn drop(&mut self) {
        // Closing the stream is what makes a still-waiting gate exit without
        // running the user program.
        self.stream = None;
        self.listener = None;
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// The executable and fixed arguments that start this gate.
fn gate_command() -> Result<(PathBuf, Vec<OsString>)> {
    #[cfg(test)]
    {
        Ok((
            std::env::current_exe().context("Locating the test executable")?,
            vec![
                OsString::from("--exact"),
                OsString::from("tools::pty_gate::tests::pty_gate_process_entry"),
                OsString::from("--quiet"),
            ],
        ))
    }
    #[cfg(not(test))]
    {
        Ok((
            std::env::current_exe().context("Locating the host executable for the PTY gate")?,
            Vec::new(),
        ))
    }
}

/// A short, private socket path.
///
/// The task directory contains two UUIDs and overflows `sun_path`; the
/// endpoint uses the same short runtime-base scheme, so the gate does too.
fn gate_socket_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let uid = std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .unwrap_or(0);
    let directory = base.join(format!("dcl-pty-{uid}"));
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("Creating {}", directory.display()))?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Restricting {}", directory.display()))?;
    Ok(directory.join(format!("gate-{}.sock", uuid::Uuid::new_v4())))
}

/// Prepare the gate for `original`.
///
/// The returned `spawn` starts the gate itself (not the user program). After
/// spawning, call [`PreparedGate::accept_specification`] before publishing the
/// session, and [`PreparedGate::release`] only after the supervisor
/// acknowledged it.
pub(crate) fn prepare(original: &SpawnSpec) -> Result<PreparedGate> {
    let specification = encode(&GateSpec::from_spawn(original));
    if specification.len() > MAX_SPEC_BYTES {
        bail!("the managed PTY specification is too large");
    }
    let socket_path = gate_socket_path()?;
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).with_context(|| {
        format!(
            "Binding the execution gate socket {}",
            socket_path.display()
        )
    })?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .context("Restricting the execution gate socket")?;
    listener
        .set_nonblocking(true)
        .context("Making the execution gate socket nonblocking")?;
    let (executable, arguments) = gate_command()?;
    let spawn = SpawnSpec {
        executable: executable.into(),
        arguments,
        cwd: original.cwd.clone(),
        environment: vec![(
            OsString::from(GATE_SOCKET_ENV),
            socket_path.clone().into_os_string(),
        )],
        rows: original.rows,
        cols: original.cols,
    };
    Ok(PreparedGate {
        spawn,
        listener: Some(listener),
        socket_path,
        specification,
        stream: None,
    })
}

impl PreparedGate {
    /// Wait for the gate to connect and send it the specification.
    ///
    /// This is also the readiness barrier: the user program is not started by
    /// the specification alone, but a gate that never connects cannot run it.
    pub(crate) fn accept_specification(&mut self, deadline: Instant) -> Result<()> {
        let Some(listener) = self.listener.as_ref() else {
            bail!("the execution gate listener is already released");
        };
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("Accepting the execution gate connection"),
            }
            if Instant::now() >= deadline {
                bail!(
                    "the execution gate did not connect in time; the user program was not started"
                );
            }
            std::thread::sleep(ACCEPT_TICK);
        };
        // The listener is no longer needed; keep the accepted stream only.
        self.listener = None;
        let _ = std::fs::remove_file(&self.socket_path);
        stream
            .set_write_timeout(Some(SOCKET_TIMEOUT))
            .context("Bounding the execution gate handshake")?;
        let mut stream = stream;
        stream
            .write_all(&(self.specification.len() as u32).to_be_bytes())
            .context("Sending the execution gate specification length")?;
        stream
            .write_all(&self.specification)
            .context("Sending the execution gate specification")?;
        stream
            .flush()
            .context("Flushing the execution gate handshake")?;
        stream
            .set_write_timeout(None)
            .context("Clearing the execution gate handshake bound")?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Release the user program. Only call this after the supervisor
    /// acknowledged the session identity.
    pub(crate) fn release(&mut self) -> Result<()> {
        let Some(stream) = self.stream.as_mut() else {
            bail!("the execution gate connection is not established");
        };
        stream
            .write_all(b"G")
            .context("Releasing the execution gate")?;
        stream
            .flush()
            .context("Flushing the execution gate release")?;
        // The gate has the byte; closing our side also turns a later owner
        // death into EOF for any read the gate still has to do.
        self.stream = None;
        Ok(())
    }
}

/// Host entry: run the gate when this process was started for it.
///
/// This is the only entry: CLI, Desktop and the test helper all call it.
/// Returns `Ok(None)` for a normal host start.
pub(crate) fn run_from_env() -> Result<Option<i32>> {
    let Some(socket_path) = std::env::var_os(GATE_SOCKET_ENV) else {
        return Ok(None);
    };
    std::env::remove_var(GATE_SOCKET_ENV);
    Ok(Some(gate_main(Path::new(&socket_path))))
}

fn gate_main(socket_path: &Path) -> i32 {
    // The owner is the process that bound the socket. If it died before this
    // connect, there is nothing to run and nothing to guess.
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(stream) => stream,
        Err(_) => return 0,
    };
    let mut length = [0u8; 4];
    if stream.read_exact(&mut length).is_err() {
        // EOF before the specification: the owner disappeared.
        return 0;
    }
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_SPEC_BYTES {
        eprintln!("managed PTY gate: the specification length is invalid");
        return 64;
    }
    let mut bytes = vec![0u8; length];
    if stream.read_exact(&mut bytes).is_err() {
        return 0;
    }
    let spec = match decode(&bytes) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("managed PTY gate: {error:#}");
            return 64;
        }
    };
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            // The owner is gone before it released the user program.
            Ok(0) => return 0,
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("managed PTY gate: waiting for the owner failed: {error}");
                return 70;
            }
        }
    }
    if byte[0] != b'G' {
        // A damaged or unexpected release byte never starts the user program.
        eprintln!("managed PTY gate: unexpected start byte {:#04x}", byte[0]);
        return 64;
    }
    drop(stream);

    let program = resolve_program(&spec.executable, &spec.environment);
    let mut command = Command::new(&program);
    command.args(&spec.arguments);
    command.env_clear();
    command.envs(
        spec.environment
            .iter()
            .map(|(key, value)| (key.as_os_str(), value.as_os_str())),
    );
    // `exec` keeps this PID, so the user program becomes the session leader.
    let error = command.exec();
    eprintln!(
        "managed PTY gate could not execute {:?}: {error}",
        spec.executable
    );
    127
}

/// Resolve the requested program before `env_clear` applies.
///
/// A path containing `/` is used as given. A bare name is searched in the
/// *user* environment's `PATH`, which is the environment the program receives.
fn resolve_program(executable: &OsStr, environment: &[(OsString, OsString)]) -> PathBuf {
    let path = PathBuf::from(executable);
    if executable.as_bytes().contains(&b'/') {
        return path;
    }
    let search = environment
        .iter()
        .find(|(key, _)| key.as_os_str() == OsStr::new("PATH"))
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    for directory in search.as_bytes().split(|byte| *byte == b':') {
        let directory = if directory.is_empty() {
            Path::new(".")
        } else {
            Path::new(OsStr::from_bytes(directory))
        };
        let candidate = directory.join(&path);
        if candidate.is_file() {
            return candidate;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pty_transport;

    const BUDGET: Duration = Duration::from_secs(20);

    fn shell_spec(script: &str, cwd: &Path) -> SpawnSpec {
        SpawnSpec {
            executable: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-c"), OsString::from(script)],
            cwd: cwd.to_path_buf(),
            environment: vec![
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (OsString::from("GATE_PROBE"), OsString::from("kept-bytes")),
            ],
            rows: 24,
            cols: 80,
        }
    }

    /// Start the gate through the real `prepare` + `pty_transport::spawn`
    /// path, not a second copy of the protocol.
    fn start_gate(script: &str, directory: &Path) -> (pty_transport::PtyTransport, PreparedGate) {
        let mut gate = prepare(&shell_spec(script, directory)).unwrap();
        let transport = pty_transport::spawn(&gate.spawn).expect("the gate starts on the PTY");
        gate.accept_specification(Instant::now() + BUDGET)
            .expect("the gate connects");
        (transport, gate)
    }

    fn wait_for(what: &str, deadline: Instant, mut check: impl FnMut() -> bool) {
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_specification_round_trips_without_losing_bytes() {
        let spec = GateSpec {
            executable: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-c"), OsString::from("printf hi")],
            environment: vec![
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (
                    OsString::from("WEIRD"),
                    OsString::from_vec(vec![0x66, 0xff, 0x6f]),
                ),
            ],
        };
        let decoded = decode(&encode(&spec)).unwrap();
        assert_eq!(decoded.executable, spec.executable);
        assert_eq!(decoded.arguments, spec.arguments);
        assert_eq!(decoded.environment, spec.environment);
    }

    #[test]
    fn a_truncated_or_foreign_specification_is_refused() {
        assert!(decode(b"not a spec").is_err());
        let mut encoded = encode(&GateSpec {
            executable: OsString::from("/bin/sh"),
            arguments: Vec::new(),
            environment: Vec::new(),
        });
        encoded.truncate(encoded.len() - 2);
        assert!(decode(&encoded).is_err());
    }

    #[test]
    fn go_runs_the_user_program_exactly_once_after_release() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let (transport, mut gate) = start_gate("printf ran >> ran", directory.path());
        // Before the release byte nothing may run.
        assert!(!marker.exists(), "the gate ran before GO");
        gate.release().unwrap();
        wait_for("the user program", Instant::now() + BUDGET, || {
            marker.exists()
        });
        let _ = transport.stop();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
    }

    #[test]
    fn eof_never_runs_the_user_program() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let (transport, gate) = start_gate("printf ran > ran", directory.path());
        // Close the owner's side instead of sending GO: the gate sees EOF.
        drop(gate);
        let _ = transport.stop();
        assert!(
            !marker.exists(),
            "the gate executed the user program after EOF"
        );
    }

    #[test]
    fn a_corrupt_release_byte_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let (transport, mut gate) = start_gate("printf ran > ran", directory.path());
        gate.stream.as_mut().unwrap().write_all(b"X").unwrap();
        let _ = transport.stop();
        assert!(
            !marker.exists(),
            "a corrupt release byte started the user program"
        );
    }

    #[test]
    fn argv_environment_and_cwd_reach_the_user_program() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("fidelity");
        let script = "printf '%s|%s|%s' \"$0\" \"$GATE_PROBE\" \"$(pwd)\" > fidelity";
        let (transport, mut gate) = start_gate(script, directory.path());
        gate.release().unwrap();
        wait_for("the fidelity marker", Instant::now() + BUDGET, || {
            marker.exists()
        });
        let _ = transport.stop();
        let observed = std::fs::read_to_string(&marker).unwrap();
        let expected = format!(
            "/bin/sh|kept-bytes|{}",
            directory.path().canonicalize().unwrap().display()
        );
        assert_eq!(observed, expected);
    }

    /// A concurrently spawned child must not inherit any gate descriptor.
    #[test]
    fn a_concurrent_child_inherits_no_gate_descriptor() {
        use std::process::{Command, Stdio};
        let directory = tempfile::tempdir().unwrap();
        let listing = directory.path().join("links");
        let (transport, mut gate) = start_gate("printf ran > ran", directory.path());
        // Report the *targets* of the child's descriptors: a pipeline or a
        // directory listing would itself create descriptors and make a count
        // meaningless.
        let file = std::fs::File::create(&listing).unwrap();
        let status = Command::new("/bin/sh")
            .args([
                "-c",
                // A descriptor can vanish between the glob and readlink; that
                // is the probe's own transient state, not a test failure.
                "for f in /proc/self/fd/*; do readlink \"$f\"; done 2>/dev/null; exit 0",
            ])
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let links = std::fs::read_to_string(&listing).unwrap();
        assert!(
            !links.lines().any(|line| line.contains("socket:")),
            "a concurrent child inherited a gate socket descriptor: {links}"
        );
        gate.release().unwrap();
        wait_for("the gated program", Instant::now() + BUDGET, || {
            directory.path().join("ran").exists()
        });
        let _ = transport.stop();
    }

    /// Helper process: the PTY child in tests. It uses exactly the entry the
    /// CLI and Desktop call, so a broken entry cannot be hidden by the helper.
    #[test]
    fn pty_gate_process_entry() {
        if std::env::var_os(GATE_SOCKET_ENV).is_none() {
            return;
        }
        match run_from_env() {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => std::process::exit(2),
            Err(error) => {
                eprintln!("managed PTY gate: {error:#}");
                std::process::exit(70);
            }
        }
    }
}
