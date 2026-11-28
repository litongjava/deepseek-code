---
name: sdk-late-permission-reply
description: Keep a cancelled SDK request ID so a late permission reply does not abort the next turn.
---

## Symptom
After interrupting a pending permission prompt, a host can send a late answer and lose its next turn.

## Cause
Bulk cancellation drained the pending requests without remembering their IDs. The response router treated a delayed answer as an unknown request rather than discarding it.

## Fix
Remember bulk-cancelled IDs in the same bounded queue used for single-request cancellation. Ignore a reply to a cancelled ID without applying its decision; continue rejecting unknown IDs.

## Verification
`cargo test --locked -p deepseek-code-cli --test sdk http_scopes::interrupt_then_late_answer` is covered by the verified `http_scopes::` integration run. After an interrupt, a late user-scope allow does not fetch or grant access; the next request prompts with a fresh ID.
