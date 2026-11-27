---
name: resume-observer-cursor-handoff
description: A resumed rich screen and the live observer must share cursors, or the running turn is replayed.
metadata:
  type: project
---

## Symptom

`dcl --resume ID` rendered the running turn's published text/tools, then one
second later the observer printed the same turn again as plain lines, pushing
the banner and rich blocks off screen.

## Cause

`show_resumed` wrote rich history but did not tell the observer what it had
rendered. The observer started with empty cursors and replayed every published
segment. It also rendered `output: None` tools as `No result recorded`, even
while the owner was still executing them.

## Fix

`show_resumed` records a `ResumedView` (per assistant message: rendered text
length and completed tool ids) and hands it to the observer with
`seed_from_resumed`; `preannounce` preserves those cursors for the same session
identity. While a session is occupied, `conversation` skips still-running tools
so the observer can commit them as rich blocks on completion.

## Verification

`repl::a_resumed_read_only_view_seeds_the_observer_without_replaying` and the
tmux PTY run where `#1 Read(fixture.txt)` and `FIRST_STREAM_LINE` each appeared
exactly once after the observer attached.
