---
name: codegraph
description: "Use CodeGraph code intelligence to inspect symbols, call paths, and impact (initializing or syncing the .codegraph index when needed)"
user-invocable: true
disable-model-invocation: false
---
Use CodeGraph to answer this code question or plan this change before editing files.

1. Check for a `.codegraph/` index at or above the project root with
   `codegraph status` to see whether the index exists, how current it is and
   which project it describes. If no index exists yet, run `codegraph init`
   (or `codegraph index` to rebuild) to build the local index before querying.
2. Use the `codegraph` CLI directly: `codegraph explore "<symbols or question>"`,
   `codegraph node <name>`, or `codegraph callers|callees|impact <symbol>`.
3. Treat the result as verbatim source with line numbers plus the call path
   among the shown symbols. Consider those spans already read; open a file
   separately only for lines outside the returned view or to confirm an edit.
4. Use `impact` before editing a shared symbol and state which callers make the
   change risky. Do not infer call paths the tool did not return.
5. After a large edit, run `codegraph sync` so later queries are not stale. If
   `codegraph` is unavailable or indexing fails, fall back to Grep, Glob and
   Read.

Report which command answered the question, the symbols and file:line
references that matter, and any part of the answer that comes from reasoning
rather than the index.
