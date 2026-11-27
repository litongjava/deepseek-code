---
name: read-only-observer-exit
description: Leaving an occupied session as a read-only observer must not call quit_worker or cancel shared hooks.
metadata:
  type: project
---

## Symptom

`/exit` (or Ctrl+C/EOF/signal) from a terminal observing another worker's turn
stopped or interrupted the owner: its managed job was cancelled, running shells
were stopped, the persistent worker was shut down, and `SessionEnd` hooks plus a
shared async-hook `cancel` file ran for a session that had not ended.

## Cause

The REPL treated every `/exit` as "quit this session": it unconditionally called
`session_trust::quit_worker`, and teardown always called `end_hooks` and
`Session::close`, whose `async_hooks::cancel_shared` writes a `cancel` marker
into the shared hook directory.

## Fix

Decide the exit action from `observing_session`: only a proven `Ok(false)` may
stop the worker. `Ok(true)` and probe errors leave the observer. Track
`observing_exit` across all loop exits (exit commands, Ctrl+C, EOF, signals,
`/background`, `/restart`) and use `Session::close_owned_local`, which cancels
only this process's async-hook registry and skips `end_hooks`/`cancel_shared`.

## Verification

`exit_tests::only_a_proven_idle_session_may_stop_the_worker_on_exit`,
`session::request_usage_tests::read_only_shutdown_leaves_jobs_worker_and_shared_hooks_untouched`,
and a real tmux PTY run where `/exit` in the observer left the owner running and
the owner completed all 20 streamed text parts.
