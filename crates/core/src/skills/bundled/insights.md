Generate a report analyzing my DeepSeek Code sessions: $ARGUMENTS

Gather evidence with read-only commands:
- `dcl sessions list` for the saved sessions (titles, projects, times, message counts);
- `dcl sessions show ID` for a few representative or recent sessions.

Write a concise report covering: how many sessions and in which projects, the kinds of tasks worked on, recurring friction (repeated corrections, failed tool calls, permission prompts that were always approved), and concrete suggestions — project instructions to add, permission rules or skills that would save time, and habits that worked well. Quote evidence briefly and never include secrets, tokens or full file contents. Save the report as `deepseek-code-insights.md` in the current directory only if the user asks; otherwise show it.
