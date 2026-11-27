---
name: codegraph
description: "Use CodeGraph code intelligence to inspect symbols, call paths, and impact (initializing or syncing the .codegraph index when needed)"
user-invocable: true
disable-model-invocation: false
---
Use CodeGraph for fast symbol lookup, call-chain tracing, and change-impact analysis before editing files.

1. **Query directly when READY**: if the workspace index status below is READY, skip `codegraph status` and query immediately with `codegraph explore "<symbols or question>"`, `codegraph node <name>`, or `codegraph callers|callees|impact <symbol>`.
2. **Initialize first when NOT INITIALIZED**: if the workspace index status below is NOT INITIALIZED (common in fresh git worktrees), run `codegraph init -y` (non-interactive) first to build `.codegraph/codegraph.db`, then query with `codegraph explore|node|callers|callees|impact`.
3. **Start with CodeGraph on turn 1**: use `codegraph explore`, `codegraph callers`, or `codegraph impact` for symbol lookup, call-chain tracing, and change-impact analysis instead of serial `Grep`/`Bash` hops.
4. **Avoid redundant reads & check impact**: treat CodeGraph results as verbatim source with line numbers and verified call paths—open a file separately only for lines outside the returned view or to prepare an edit. Run `codegraph impact <symbol>` before changing a shared symbol; do not infer call paths the tool did not return.
5. **Sync or fall back cleanly**: after large edits, run `codegraph sync` before further CodeGraph queries. Only if `codegraph` is unavailable or `codegraph init -y` fails, fall back cleanly to `Read`/`Grep`/`Glob`.
