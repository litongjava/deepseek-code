---
name: workspace-hook-settings-path
description: CLI integration tests must write workspace hooks to .claude/settings.json, not .claude/config/settings.json
metadata:
  type: project
---

## Symptom

`cargo test -p deepseek-code-cli --test task_input` fails in
`cli_task_input_records_each_call_and_fires_tool_hooks` and
`cli_task_input_pre_hook_denial_writes_no_stdin_bytes` with an empty hook
journal (0 PreToolUse events) on every platform, while the durable TaskInput
rows and the model-visible denial are still recorded.

## Cause

Main commit 919930e1 (data directory layout v2) rewrote the test helper
`write_hooks` to `write_config_file(.claude/config/settings.json)`. The layout
change only applies to the data directory; `HookRunner::load_with_configs`
still reads workspace hooks from `.claude/settings.json` and
`.claude/settings.local.json`, so the hook never matched. Platform runtime
tests had not run on main since 2026-11-22, so CI did not catch it.

## Fix

Write the workspace settings back to `.claude/settings.json` in
`crates/cli/tests/task_input.rs`.

## Verification

`cargo test --locked -p deepseek-code-cli --test task_input` reports 5 passed,
0 failed.
