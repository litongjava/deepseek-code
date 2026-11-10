use std::{future::Future, sync::mpsc, time::Duration};

/// Keep paused Tokio time stationary during real socket I/O. Only the test's
/// explicit advances may expire timers; the wall-clock watchdog still bounds
/// failures if an expected network response or retry event never arrives.
pub async fn run<F: Future>(future: F) -> F::Output {
    let (release, released) = mpsc::channel::<()>();
    let (ready, started) = tokio::sync::oneshot::channel();
    let (expired, timeout) = tokio::sync::oneshot::channel();
    let watchdog = tokio::task::spawn_blocking(move || {
        // Tokio inhibits paused-clock auto-advance while a blocking task runs.
        let _ = ready.send(());
        if matches!(
            released.recv_timeout(Duration::from_secs(10)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            let _ = expired.send(());
        }
    });
    started.await.expect("manual clock watchdog started");
    let result = tokio::select! {
        result = future => result,
        _ = timeout => panic!("manual clock test exceeded 10 seconds of wall time"),
    };
    // Dropping the sender also releases the blocking task during unwinding.
    drop(release);
    watchdog.await.expect("manual clock watchdog stopped");
    result
}
