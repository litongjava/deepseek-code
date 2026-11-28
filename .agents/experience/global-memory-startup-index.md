---
name: global-memory-startup-index
description: Initialize the enabled global memory index before a dcl session starts, without touching authored rows on restart.
---

Symptom: A new data root had no global MEMORY.md at session startup. On Windows, ordinary drive paths to the missing directory could also fail managed scope matching.

Cause: Session startup did not initialize the index, and a missing directory cannot be canonicalized to compare ordinary and verbatim Windows paths.

Fix: Initialize the enabled global index through the scoped directory's exclusive create, preserving an existing file. Use safe prefix simplification for lexical scope comparisons when canonicalization is unavailable.

Verification: `global_memory_first_use_with_plain_path` covers disabled startup, index creation, preservation and managed entry reads and edits. Two `dcl --init-only` launches against the same isolated data root verified creation and preservation of a prewritten index.

For Project and Session, wait for the final workspace and use the selected shard, not the launch cwd or the public session ID. Do not open the writable catalog to create the index. Skip initialization on Plan, missing-workspace inspection and occupied sessions. `startup_indexes_before_input_follow_scope_switches_and_skip_observers` checks isolated CLI startup/resume, scope switches, a held execution lease and byte preservation; `missing_directory_resumes_read_only_then_restores_writable_mode` checks the in-process transition.
