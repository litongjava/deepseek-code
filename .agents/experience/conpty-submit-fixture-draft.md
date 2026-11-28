---
name: conpty-submit-fixture-draft
description: A ConPTY fixture can leave a prompt in the idle editor when it writes the draft and carriage return together.
---

## Symptom
The WebFetch permission integration test timed out waiting for the menu. The last frame still showed the full draft in the idle input box.

## Cause
Writing `Fetch fixture\r` as one input chunk did not produce a separate submit action.

## Fix
Write the draft, wait for the editor row to display it, then send `\r` separately, as in the prompt submission stability tests.

## Verification
`cargo test --locked -p deepseek-code-cli --test prompt_submit_stability -- web_fetch_http_prompt` passed all three tests on Windows, including six interactive ConPTY selections.
