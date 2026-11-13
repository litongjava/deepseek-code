//! One interruption future for every way a terminal, service manager or
//! supervisor asks this process to stop.
//!
//! Ctrl-C stays a turn-level interruption, as before, and can interrupt a
//! later turn again. SIGTERM and SIGHUP take the same cooperative cancellation
//! path and then end the process with a failure status, so the active turn can
//! cancel its owned work (process trees, hooks, agent children) before the
//! runtime shuts down. A signal is never handled by exiting the process
//! directly: destructors of the running turn must still run.
//!
//! Every platform publishes signals through the same generation channel, so
//! the call sites behave alike: Windows Ctrl-C is delivered exactly like a
//! Unix interrupt, and the channel is not tied to one signal source.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::watch;

/// The process-wide listener. Installed once, from inside the runtime.
static LISTENER: OnceLock<Result<Arc<SignalState>, String>> = OnceLock::new();

/// Broadcast state shared by the signal listener and its waiters. Kept free of
/// OS signals so the notification semantics can be tested on any platform.
#[derive(Debug)]
struct SignalState {
    /// Counts notifications. A waiter records the count when it starts, so a
    /// signal is observed by each waiter exactly once and a later waiter is
    /// not woken by an earlier one.
    sender: watch::Sender<u64>,
    /// Kept alive so the channel never closes while the process runs.
    _receiver: watch::Receiver<u64>,
    /// Set by SIGTERM and SIGHUP: these stop the process rather than only the
    /// current turn.
    terminating: AtomicBool,
}

impl SignalState {
    fn new() -> Self {
        let (sender, receiver) = watch::channel(0u64);
        Self {
            sender,
            _receiver: receiver,
            terminating: AtomicBool::new(false),
        }
    }

    /// Publishes one signal. `terminating` also remembers that the process
    /// should exit after cleanup, including when no waiter exists yet.
    fn notify(&self, terminating: bool) {
        if terminating {
            self.terminating.store(true, Ordering::SeqCst);
        }
        self.sender
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::SeqCst)
    }

    /// Resolves on Ctrl-C, SIGTERM or SIGHUP. Every waiter resolves, so a
    /// listener never consumes a signal that belongs to another owner.
    async fn interrupted(&self) {
        let mut receiver = self.sender.subscribe();
        let seen = *receiver.borrow();
        loop {
            if self.is_terminating() {
                return;
            }
            if *receiver.borrow() != seen {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Resolves only for a signal that ends the process, including one
    /// received before this waiter started. Ctrl-C does not resolve it, so
    /// idle prompt editing keeps its current draft behaviour.
    async fn termination(&self) {
        let mut receiver = self.sender.subscribe();
        loop {
            if self.is_terminating() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

/// The registration/installation boundary. A failed listener setup is returned
/// to the caller, never swallowed: startup must refuse to continue because
/// Tokio does not restore default handling for a signal whose receiver was
/// dropped after a partial registration, so a partially installed handler
/// could silently ignore SIGTERM or SIGHUP.
fn initialize(
    start: impl FnOnce(&Arc<SignalState>) -> std::io::Result<()>,
) -> Result<Arc<SignalState>, String> {
    let state = Arc::new(SignalState::new());
    start(&state).map_err(|error| {
        format!(
            "Cannot listen for interruption signals ({error}); refusing to start because a partially installed handler could swallow SIGTERM or SIGHUP"
        )
    })?;
    Ok(state)
}

fn try_listener() -> Result<&'static Arc<SignalState>, &'static str> {
    LISTENER
        .get_or_init(|| initialize(|state| spawn_listener(state.clone())))
        .as_ref()
        .map_err(String::as_str)
}

#[cfg(unix)]
fn spawn_listener(state: Arc<SignalState>) -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let (interrupt, terminate, hangup) = register_signals(|kind| signal(kind))?;
    spawn_registered(state, interrupt, terminate, hangup);
    Ok(())
}

/// Registers all three signals in one place and stops at the first failure, so
/// the caller can refuse startup instead of running with a partial set.
#[cfg(unix)]
fn register_signals<R>(
    mut register: impl FnMut(tokio::signal::unix::SignalKind) -> std::io::Result<R>,
) -> std::io::Result<(R, R, R)> {
    use tokio::signal::unix::SignalKind;
    Ok((
        register(SignalKind::interrupt())?,
        register(SignalKind::terminate())?,
        register(SignalKind::hangup())?,
    ))
}

#[cfg(unix)]
fn spawn_registered(
    state: Arc<SignalState>,
    mut interrupt: tokio::signal::unix::Signal,
    mut terminate: tokio::signal::unix::Signal,
    mut hangup: tokio::signal::unix::Signal,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = interrupt.recv() => state.notify(false),
                _ = terminate.recv() => state.notify(true),
                _ = hangup.recv() => state.notify(true),
            }
        }
    });
}

/// Windows keeps its Ctrl-C behaviour, but publishes every Ctrl-C through the
/// same channel in a loop, so a later turn can be interrupted again.
#[cfg(not(unix))]
fn spawn_listener(state: Arc<SignalState>) -> std::io::Result<()> {
    spawn_ctrl_c_listener(state);
    Ok(())
}

fn spawn_ctrl_c_listener(state: Arc<SignalState>) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            state.notify(false);
        }
    });
}

/// Registers the handlers. Call this once, early, while the runtime is up, and
/// stop startup if it fails.
pub fn install() -> Result<(), &'static str> {
    try_listener().map(|_| ())
}

/// Resolves on Ctrl-C, SIGTERM or SIGHUP.
pub async fn interrupted() {
    match try_listener() {
        Ok(state) => state.interrupted().await,
        // Startup already refused to continue; a missing listener must never
        // cancel a turn that is otherwise running.
        Err(_) => std::future::pending().await,
    }
}

/// True after SIGTERM or SIGHUP: the process should exit nonzero once the
/// active turn has cancelled its owned work.
pub fn terminating() -> bool {
    try_listener().is_ok_and(|state| state.is_terminating())
}

/// Resolves only for a signal that ends the process.
pub async fn termination() {
    match try_listener() {
        Ok(state) => state.termination().await,
        Err(_) => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A failed installation is returned to the startup path, which then stops
    /// before any turn. It is not retried or silently downgraded.
    #[test]
    fn a_failed_installation_is_reported_instead_of_continuing() {
        let error = initialize(|_| Err(std::io::Error::other("injected failure"))).unwrap_err();
        assert!(
            error.contains("Cannot listen for interruption signals"),
            "{error}"
        );
        assert!(error.contains("refusing to start"), "{error}");
        assert!(error.contains("injected failure"), "{error}");
    }

    /// Registration stops at the first failure. The production function needs
    /// real streams, so the injected closure returns a plain counter: no OS
    /// handler is installed by this test, and it is safe in parallel.
    #[cfg(unix)]
    #[test]
    fn partial_registration_failure_stops_and_is_returned() {
        for failure_at in 1..=3 {
            let mut calls = 0;
            let result = register_signals(|_| {
                calls += 1;
                if calls == failure_at {
                    Err(std::io::Error::other(format!("failure {failure_at}")))
                } else {
                    Ok(calls)
                }
            });
            let error = result.err().expect("a partial registration must fail");
            assert_eq!(calls, failure_at, "registration continued after a failure");
            assert!(
                error.to_string().contains(&format!("failure {failure_at}")),
                "{error}"
            );
        }
    }

    /// Notification is what the converted Windows path now uses. Two waiters
    /// started before the signal both wake, a waiter started afterwards is not
    /// woken by the old signal, and the following signal wakes it.
    #[tokio::test]
    async fn each_notification_wakes_its_waiters_once() {
        let state = Arc::new(SignalState::new());
        let first = {
            let state = state.clone();
            tokio::spawn(async move { state.interrupted().await })
        };
        let second = {
            let state = state.clone();
            tokio::spawn(async move { state.interrupted().await })
        };
        tokio::task::yield_now().await;
        state.notify(false);
        timeout(Duration::from_secs(1), first)
            .await
            .expect("the first waiter was not woken")
            .unwrap();
        timeout(Duration::from_secs(1), second)
            .await
            .expect("the other waiter was not woken")
            .unwrap();

        let mut later = Box::pin(state.interrupted());
        assert!(
            timeout(Duration::from_millis(200), later.as_mut())
                .await
                .is_err(),
            "an earlier signal must not wake a later waiter"
        );
        state.notify(false);
        timeout(Duration::from_secs(1), later.as_mut())
            .await
            .expect("the next signal did not wake the waiter");
    }

    /// A termination signal is remembered when no waiter existed yet, and it
    /// also wakes a turn-level waiter.
    #[tokio::test]
    async fn a_termination_is_remembered_before_any_waiter() {
        let state = SignalState::new();
        state.notify(true);
        assert!(state.is_terminating());
        timeout(Duration::from_secs(1), state.termination())
            .await
            .expect("a termination received earlier was forgotten");
        timeout(Duration::from_secs(1), state.interrupted())
            .await
            .expect("a termination must interrupt the active turn");
    }

    /// Ctrl-C only interrupts the turn: it does not request a process exit.
    #[tokio::test]
    async fn an_interrupt_does_not_become_a_termination() {
        let state = SignalState::new();
        // Both waiters start before the signal, as a turn and an idle prompt
        // would.
        let mut turn = Box::pin(state.interrupted());
        let mut termination = Box::pin(state.termination());
        assert!(timeout(Duration::from_millis(50), turn.as_mut())
            .await
            .is_err());
        assert!(timeout(Duration::from_millis(50), termination.as_mut())
            .await
            .is_err());
        state.notify(false);
        assert!(!state.is_terminating());
        timeout(Duration::from_secs(1), turn.as_mut())
            .await
            .expect("Ctrl-C must still interrupt the turn");
        assert!(
            timeout(Duration::from_millis(200), termination.as_mut())
                .await
                .is_err(),
            "Ctrl-C must not end the prompt"
        );
    }
}
