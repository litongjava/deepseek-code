---
name: execution-lease-uses-incarnation
description: Execution leases are keyed by the session incarnation, not the conversation id.
metadata:
  type: project
---

## Symptom

`/resume` did not mark a session as `[running]` even though another process held
its execution lease. Probing `execution_active(root, conversation_id)` checked a
lock file that never existed.

## Cause

A turn acquires `state/locks/session-execution/<incarnation>.lock`, where the
incarnation is a fresh UUID stored in the directory's `conversations` row.
`ConversationRow.id` is the conversation id and differs from the incarnation.

## Fix

Use `Stores::execution_active_for(id)`: it resolves the current incarnation from
the directory (read-only, no shard open, no settlement) and then probes the
lease. `/resume` uses it together with one read-only scan of active managed jobs
(`background_sessions::active_session_ids`, which never rewrites job state).

## Verification

`extended_commands::resume_tests` (idle, lease, active job, JSON `running`),
`terminal::picker::tests::resume_search_matches_running_marker_and_session_id`,
and the real PTY `/resume` screen showing `[running]` plus the full session ID.
