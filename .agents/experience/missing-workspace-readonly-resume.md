---
name: Missing workspace read-only resume
description: Interactive dcl can inspect a saved session whose project or worktree directory was deleted, while every execution path stays strict.
---

## Symptom

`dcl --resume <id>` and `/resume` failed with `Folder must be an existing absolute directory` or `Conversation worktree is unavailable` even though the session history was intact.

## Cause

Normal resume resolved the workspace before opening history, so a deleted project folder or bound worktree aborted the transition. The same resolution also guarded trust, archived state and canonical folder identity.

## Fix

- `SessionDb::code_selection_for_inspection` reports `ResumedWorkspace::Missing` for deleted project folders or bound worktrees, but keeps every other strict check. Existing folders must still canonicalize to their recorded trusted paths. `Missing` carries both the recorded display `path` and the actual `missing_path`, so a multi-folder project names the deleted folder rather than its surviving primary.
- Interactive startup and `/resume` use the inspection API; print, SDK, fork, init-only and worker paths keep strict resume.
- A missing session gets a display-only observation binding and no workspace, hook, permission-mode, coordination or session-DB writes. REPL prompts, shell/memory shortcuts and mutating commands are rejected through one allowlist; `turn`/`clear`/`branch`/permission switches also guard independently.
- Recreating the directory, or switching to a valid session, restores writable mode and the remembered lifecycle ownership. A non-SDK interactive host records lifecycle ownership even when it starts on a missing folder (`remember_lifecycle_host`), so the later writable resume runs SessionStart while the missing folder still runs no hook.
- `Session` keeps `real_providers`, the last genuinely writable catalog. Resuming from a missing/observation binding never reads the display snapshot's mutated providers: with saved configuration it reloads and validates the saved provider/model, otherwise it resolves only from the preserved real catalog (or a fresh `select_saved`). A deleted provider or a force-enabled disabled provider is therefore rejected with `Saved provider/model is unavailable; current session retained` even when recreating the same session directory.
- Read-only REPL lines bypass `Composer::push`, so `/multiline` and trailing backslashes cannot leave a pending composer that swallows `/exit`, `/resume` or `/status`. Rejected prompts are not restored into the input box.
- Inside a missing session, bare `/resume` opens the picker with `all` already selected; `-C`/`--path` compares canonical directories only when the recorded primary still exists.

## Verification

- `inspection_tests` (core) cover missing project/worktree, trust, changed canonical paths, archived/non-Code, projectless sessions and a multi-folder project where the surviving primary must stay canonical.
- `missing_workspace_tests`, `read_only_submission_tests`, `launch_directory_tests` and `resume_tests` (CLI) cover valid→missing→valid, recreate, deleted/disabled provider promotion, lifecycle ownership, multi-folder notices, the command allowlist, composer bypass and the footer label.
- Real `dcl` + tmux with a local SSE mock: strict `--print --resume` still fails; `/resume` and direct `--resume` show read-only history, reject prompts without changing `session.db`, `/history` and `/status` work; `/multiline` and a trailing backslash are rejected without trapping later commands; the missing-session `/resume` picker defaults to all; `Ctrl+A` shows `[missing]`; switching back and recreating the directory restore writable mode. Evidence in `dev-support/deepseek-code/resume-missing-readonly-20261126/`.
