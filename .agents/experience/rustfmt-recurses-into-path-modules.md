---
name: rustfmt-recurses-into-path-modules
description: Running rustfmt on one Rust file also formats its #[path] child modules and reorders mod declarations, so untouched files appear modified
metadata:
  type: feedback
---

## Symptom

`rustfmt --edition 2021 crates/core/src/native_engine/tool_loop.rs` modified dozens of files that were clean in `git status`, such as `delegation.rs`, `plan_tool.rs` and several `*_tests.rs`, and also moved `mod` declarations inside `tool_loop.rs`.

## Cause

rustfmt follows `mod name;` and `#[path = "..."]` declarations and formats the child modules recursively. Its `reorder_modules` pass also sorts consecutive module declarations, including ones with attributes.

## Fix

Never treat `rustfmt <file>` as scoped to that file when it declares modules. After formatting, compare `git status --short` against the files known to be dirty before the run and restore the unexpected paths with `git checkout -- <paths>`; keep the intended files and check their diff for reordering noise.

## Verification

`git status --short` listed exactly the three intended files after restoring the accidental paths, and their diffs contained only the intended behavior changes plus formatting of already-dirty code.
