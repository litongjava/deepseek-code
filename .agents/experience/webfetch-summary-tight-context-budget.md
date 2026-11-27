---
name: webfetch-summary-tight-context-budget
description: WebFetch summaries fail before sending when context_window equals max_tokens; clamp the auxiliary budget in generate_with_budget, not generate_request
metadata:
  type: feedback
---

## Symptom

On a model whose `context_window` equals its `max_tokens` (for example `gpt-4` at 8192/8192 in `src/data/providerCatalog.json`), a WebFetch page longer than 20,000 cleaned characters failed before any HTTP request with `Generation input exceeds the configured context budget`. The existing prefix fallback did not catch it because the error is a plain `ensure!`, not `Truncated`/`InvalidSummary`.

## Cause

`generate_request` requires `input_estimate + max_tokens + PROTOCOL_MARGIN (1024) <= context_window`. `generate_with_budget` (the WebFetch summary path) passed the full 8,192-token budget, so `8192 + 1024 > 8192` for every non-empty page.

## Fix

Treat the requested auxiliary budget as a ceiling in `generate_with_budget` (`crates/core/src/native_engine/generation.rs`): compute `context_window - input_tokens(system, input) - PROTOCOL_MARGIN` and use the smaller value. When the room is zero, keep the requested budget so the existing context check still reports the real problem instead of a zero-budget error. `generate_with_budget` is called only by the WebFetch summary path; do not move this clamp into `generate_request` or the `Summarizer`, because that would change compaction's budget policy.

## Verification

`cargo test --locked -p deepseek-code-core --lib web_fetch` (22 passed) includes `a_tight_context_reserves_only_the_output_room_that_fits` (8192/8192) and `a_roomy_context_keeps_the_configured_summary_budget` (128000 window still requests 8192). `cargo test --locked -p deepseek-code-core --lib native_engine::generation::tests` (15 passed) includes the exact-clamp unit test, and `cargo test --locked -p deepseek-code-core --test compaction_overflow` (7 passed, 1 ignored) confirms compaction's budget behavior is unchanged.
