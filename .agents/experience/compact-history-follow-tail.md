---
name: compact-history-follow-tail
description: The automatic compact transcript re-pins its viewport before the running block is appended; re-pin in append too, and let Ctrl+End pass through top=None so show_compact can re-enter the alternate screen
metadata:
  type: feedback
---

## Symptom

With compact mode on, calls that started after the first compact frame stayed
below the viewport until Ctrl+End. The first frame pinned `HistoryView::top`
once, and later frames only rebuilt the document.

## Cause

`HistoryView::refresh` rebuilt the document and pinned `top`, but
`Frame::refresh_history` appends the running block and notices *after*
`refresh`, so the appended tail was outside the pinned page. Separately,
Ctrl+End leaves the alternate screen by setting `top = None`; a follow-tail
re-pin that runs while `top` is `None` would make the view active again and skip
the `show_compact` call that re-enters the alternate screen.

## Fix

`HistoryView` has `follow_tail` and the last content `page`. `show_compact`
turns it on and pins; `refresh` and `append` re-pin with the same
`all.len() - content_page(page)` formula, but only while `top` is `Some`.
Manual scroll paths (`navigate` except Ctrl+End, `wheel`, `drag_scroll`,
`open_at`) clear it; Ctrl+End sets it again and keeps `top = None`, so the next
compact draw re-enters through `show_compact`. Callers pass the content page to
`refresh`.

## Verification

`cargo test --locked -p deepseek-code-cli --bin dcl running_status_tests -- --test-threads=1`
passes 8 tests, including a two-wave viewport test (first frame with 0 or 1
calls, then the rest at 80x24/3, 80x14/2, 80x17/3) and a PageUp/Ctrl+End test.
The `terminal::transcript_pager` filter passes 14. A real Linux PTY with compact
enabled at the idle prompt showed every `● #N` pair at 4.4 s and 6.4 s at the
three sizes; the same scenario still scrolls with PageUp/PageDown and returns
with Ctrl+End.
