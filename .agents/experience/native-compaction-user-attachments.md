---
name: native-compaction-user-attachments
description: Restore missing user-role host blocks beside a Responses compaction window, not inside its display message.
---

**Symptom:** A rendered boundary appears to carry the environment or skill catalog, but the next Responses request omits it. Blindly appending everything can duplicate provider-preserved user parts.

**Cause:** Responses sends the returned native items verbatim instead of sending the boundary row's display text. A compact-time Session index and a first-turn Session path reminder are different attachments.

**Fix:** Compare the classified user blocks with visible user content in the native items and emit missing blocks in a separate user message. Keep the initial Session location out of that message and preserve the compact-time Session index placement.

**Verification:** `memory_native_boundary_keeps_the_index_and_opaque_items` captures a request with a provider-preserved catalog and a missing environment block, checks the exact native items, and counts each marker in user content and outside system instructions.
