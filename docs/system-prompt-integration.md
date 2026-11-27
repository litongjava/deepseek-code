# Desktop system prompt integration

The common `QueryEngine::send_message_with_history` entry point composes the system prompt for every conversation request, including HTTP chat, native commands, H5, and scheduled requests using that engine. Previously the prompt module existed but was never called by this path, so requests with no explicit `system_prompt` had no desktop instructions.

## System prompt composition order

Static sections come first so they form a stable, cacheable prefix across projects and sessions (`prompt::compose_system_prompt_with_memory` / `prompt::compose_customized`):

1. Identity: the one-line `CODE_IDENTITY` for every session, Code and Desktop alike. It never claims to be Claude or made by Anthropic.
2. `prompt/code.md`: coding and harness rules (session guidance, confirmation and reporting rules, ambiguous requests, context management, native tool notes).
3. `prompt/behavior.md`: general response and tool behavior shared by every mode.
4. `prompt/memory.md`: memory guidance. It is omitted when the global memory switch is off, so a disabled memory never advertises capabilities it does not have.
5. Optional request-specific tail (uncached): user customization appends (when `PromptMode::Append`), the request's optional custom `system_prompt` preserved verbatim in `<additional_instructions>`, database project instructions (`Project instructions:`), team context, and active runtime mode notices (plan mode, deep reasoning, or non-interactive user).

Session-scoped facts do **not** live in the system prompt. They are attached as `<system-reminder>` blocks at the start of the first user message (`prompt::SessionReminders` / `prompt::first_message_reminders`), frozen on the session's first request where noted so later requests and resumes never rewrite the prefix:

1. User instructions (`AGENTS.md` / `CLAUDE.md` from the user data directory).
2. Standalone CLI frontend facts (`dcl` terminal reminder, when running the standalone CLI).
3. Project instructions (`AGENTS.md` / `CLAUDE.md` / `.claude/CLAUDE.md` / `CLAUDE.local.md` from the workspace ancestor chain, under the "Codebase and user instructions are shown below" preamble when user instructions are absent).
4. Scoped resident `MEMORY.md` indexes (`global`, `team`, `project`, `subagent`, `session`).
5. Project experience index (`.agents/experience/`, frozen per session).
6. Loadable on-demand tools (`ToolLoad` built-in and MCP directory, frozen per session).
7. Available model-invocable skills (frozen per session).
8. `# Working environment` (frozen per session in `cache/environment/<session>.txt`): optional project name, primary working directory, whether it is a Git repository, multi-folder project source folders when present, platform, architecture, shell, OS version, Node.js version, Python environment line, and the untrusted downloads/archives warning. It intentionally omits the selected model and today's date.
9. Session date (`Today's date is YYYY-MM-DD.`, frozen separately in `cache/environment/<session>.date.txt`; if the session outlives that date, a one-time `date_change_reminder` is appended to the latest turn via turn context).
10. Git status snapshot (`# gitStatus`, frozen once per session).
11. Invoked skills recovered after compaction (when applicable).

Mid-session updates (date rollover, loadable tool/skill/experience diffs, `UserPromptSubmit` hook context, invoked skill content, and `MEMORY.md` index changes) are recorded in turn context and attached to the latest user message rather than mutating the first message.

For Anthropic Messages requests, `performance::apply_prompt_cache` uses `prompt::cache_blocks` to send the static sections above as separate text blocks, each with `cache_control: {"type":"ephemeral"}`, and everything after them as one unmarked block. `cache_blocks` recognizes both the four-section and the memory-disabled three-section layout. On endpoints that accept a mid-conversation system message (DeepSeek and Anthropic, via `performance::accepts_mid_system`), `performance::move_system_tail` and `performance::move_system_tail_chat` move the dynamic tail into a `system` message right after the first user message so the leading `system` prompt stays identical across projects. Agent conversations (`apply_conversation_cache`) also mark the last user block as a rolling breakpoint so the next turn reuses the conversation so far. One-shot utility requests, such as hook judges, mark only the system. OpenAI-format providers such as DeepSeek cache by prefix automatically, so they get the same stable ordering with no extra fields.

The common entry point resolves the persisted conversation `workspace_mode` before composing the prompt. Every mode shares the same system prompt; the profile still selects the Code tool descriptions used by both protocol tool loops. Code tool descriptions are adapted to actual native schemas in `prompt/code.rs`; no tools or captured probe messages are imported from the sample. `ScheduleWakeup.reason` telemetry and host session/account/settings collection conventions are excluded. The full Chinese explanation is in the documentation workspace at `development/code-prompt.md`.

The engine passes this prompt to the tool loop; Anthropic and OpenAI clients send it as their native system field/message, and the JavaScript provider runtime consumes the same prompt. Request trace snapshots capture it, so the context-window detailed breakdown and trajectory inspector expose what was actually sent. Edit the relevant prompt Markdown files and rebuild to change the application defaults. Callers may still append task-specific instructions via `system_prompt`.

## Original desktop resources inspected

Inspected the user's installed Claude Desktop **2.31226.1.0** on 2026-10-14, reading only packaged application resources:

- `app/resources/app.asar`, especially `.vite/build/index.chunk-1xGiXs9t.js` (desktop system-prompt assembler and helper instructions).
- `.vite/build/index.chunk-BeqXJNqE.js` (session prompt caching, model-specific variants, and externally supplied prompt map).
- `.vite/build/index.chunk-fECI9__t.js` (memory consolidation and skill instructions).
- `app/resources/ion-dist/assets/v1` (bundled renderer code).

The package contains auxiliary instructions and composition logic. The assembler accepts `baseSystemPrompt`, `rendererAppends`, and `spSectionPrompts`; the session code also fetches model-specific `coworkSyspromptMap` variants. A complete authoritative main chat prompt was **not found in the inspected static resources**. This does not claim the complete prompt is unavailable elsewhere or that this project's prompt is an official Claude prompt.

Confirmed local auxiliary excerpts:

> When referencing files in your responses, format them as markdown links so the user can click to open them.

> Preferences, working style, key relationships, and recurring workflows are durable — keep and sharpen them.

> Convert "next week", "this quarter", "by Friday" to absolute dates so they stay readable later.

These ideas are adapted into the runtime Markdown files. Original instructions for `/sessions` VM mounts, special desktop tools, Dispatch, or cloud-only product surfaces are not enabled because this application has different capabilities. Memory operations use local file-based `MEMORY.md` resident indexes maintained through ordinary file tools (`Read`/`Edit`/`Write`), rather than the retired `memory_search` and `memory_ingest` tools. No original account data, browser cookies, credentials, or conversation transcripts are imported.

## Validation

Prompt unit tests (`crates/core/src/prompt/mod.rs`) and protocol request tests (`crates/core/src/native_engine/code_prompt_tests.rs`) cover the default system prompt, memory-switch composition, Anthropic cache blocks, mid-system tail movement, first-message reminder ordering, and per-session freezing of environment, date, loadable tools, skills, and experience. The ordinary provider request snapshots can be used to inspect the resulting combined prompt without a separate paid inference call.
