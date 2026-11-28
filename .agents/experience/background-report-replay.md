---
name: background-report-replay
description: Match old background reports by task ID and persisted notification origin, not adjacent user text.
---

## Symptom
A resumed parent could send worker output to the model as system instructions, with a user notification merely pointing to it.

## Cause
The old collector wrote the report and admitted its notification separately. A replay window can contain the report but not the notification; typed text can also look identical to the generated placeholder.

## Fix
Use the queue admission as the agent receipt. On replay, replace a matching generated placeholder, or emit a standalone user notification when there is no trusted match. Do not apply a physical compaction cut that splits the projected pair.

## Verification
`runtime::background_replay::tests::persisted_pair_matches_only_generated_notice_and_respects_cut` checks both history formats, and `agent_report_is_delivered_once_as_the_notification_result` checks queue handoff after reopening.
