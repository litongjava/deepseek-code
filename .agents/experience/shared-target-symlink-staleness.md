---
name: shared-target-symlink-staleness
description: Worktrees sharing `.build/deepseek-code` via a `target` symlink can hit stale rlib caches when another worktree compiled more recently; fix with `touch` or `cargo clean -p` instead of debugging `Cargo.toml`
metadata:
  type: feedback
---

## Symptom

After editing a workspace crate (for example adding a field or function in `deepseek-code-core`), `cargo test` in a worktree whose `target` is symlinked to `/Users/ping/code/project/project-harness/.build/deepseek-code` fails with compile errors claiming the newly added field or item does not exist.

## Cause

Another worktree sharing the same `target` directory compiled an older version of the crate with a newer mtime, so Cargo reuses the stale `.rlib` instead of recompiling this worktree's edited source file.

## Fix

Do not spend turns inspecting `Cargo.toml`, `build.rs`, or `grep`ping for duplicate crate definitions. Immediately force Cargo to recompile the edited crate:

```sh
touch <edited-source-file>
```

If that is not enough, clear only the affected workspace crate's build artifacts:

```sh
cargo clean -p deepseek-code-core
```

Then re-run the targeted `cargo test --locked ...` command.
