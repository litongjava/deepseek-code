---
name: responses-stateless-reasoning-replay
description: Responses compact can reject locally generated reasoning IDs after a chat-model switch.
---

## Symptom
Switching from a chat model to GPT Responses triggers an item-not-found error on the first native compact request.

## Cause
The shared input builder converted plain thinking to a reasoning item with a locally generated ID. A readable summary is not replayable upstream state. Dropping only foreign encrypted content also leaves the foreign ID in the request.

## Fix
For ordinary Responses history, omit plain thinking without native `provider_content` (do not synthesize local reasoning IDs) and drop foreign-issuer reasoning items, while replaying same-issuer native reasoning verbatim when it carries either nonblank `encrypted_content` or nonblank plaintext `summary`/`content` (e.g., `deepseek-v4.1-flash` over pass-through Responses proxies). Empty reasoning shells are still omitted. Keep the canonical compact window intact; leave chat replay and local history alone.

## Verification
`cargo test --locked -p deepseek-code-core --lib native_engine::provider_runtime::tests` checks the exact mixed-history input for ordinary and compact requests, including tool result pairing. `cargo test --locked -p deepseek-code-core --test native_compaction` checks native window persistence and replay against local endpoints.
