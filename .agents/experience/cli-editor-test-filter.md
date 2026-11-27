---
name: cli-editor-test-filter
description: The dcl slash-completion tests are named terminal::input::editor::tests, not terminal::editor::tests
metadata:
  type: project
---

## Symptom

`cargo test --locked -p deepseek-code-cli --bin dcl terminal::editor::tests::`
reports `0 passed; 618 filtered out`, so a task that names this filter gets no
evidence even though the tests exist.

## Cause

`crates/cli/src/terminal/editor.rs` is declared through `terminal/input/mod.rs`,
so its test paths gain the `input` segment and are listed as
`terminal::input::editor::tests::*`.

## Fix

Filter with `terminal::input::editor::tests::` for these tests. Confirm the
matched count with `cargo test ... -- --list | grep editor`.

## Verification

`cargo test --locked -p deepseek-code-cli --bin dcl terminal::input::editor::tests::`
runs 37 tests: 37 passed, 1 ignored, 0 failed (2026-11-26).
