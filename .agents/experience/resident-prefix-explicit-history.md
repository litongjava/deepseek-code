---
name: resident-prefix-explicit-history
description: Freeze the initial resident index before rendering caller-supplied history; a first-iteration reload can drop tool results absent from the shard.
---

**Symptom:** A Responses request built from caller-supplied history loses its typed tool result, or loses its tool output during native compression, even though the caller supplied it.

**Cause:** Without durable prompt admission, the shard may lack a first-index snapshot. Initializing one in the tool loop reports a memory change, which reloads history from the shard. That history need not include the caller's messages. Even an empty enabled index has a visible path reminder.

**Fix:** Before rendering the caller's first window, freeze its index if absent. Recheck under the shard write lock; never replace a previously frozen prefix.

**Verification:** `cargo test --locked -p deepseek-code-core --test native_compaction -- base64_model_boundary_typed_responses base64_model_boundary_returned_window_and_replay` passed both previously failing cases.
