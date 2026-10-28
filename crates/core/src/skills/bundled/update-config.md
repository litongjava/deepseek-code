Change DeepSeek Code settings: $ARGUMENTS

Settings are JSON files, merged in this order (later wins):
- the user settings file `settings.json` in the data directory;
- the project file `.claude/settings.json` (checked in);
- the local file `.claude/settings.local.json` (not checked in).

They hold `permissions` (`allow`, `ask`, `deny` arrays of rules such as `Bash(npm test *)` or `Read(.env)`) and `hooks` (event name → matcher groups with `command`, `http` or `prompt` hooks). Environment variables for tools are not read from these files; set them in the shell that starts `dcl`.

Read the relevant file before editing and keep everything already there. Choose the narrowest scope that fits the request (local for personal tweaks, project for team-wide rules, user for all projects) and say which file you changed. For hooks, explain the event, matcher and what runs; for permissions, prefer the narrowest rule that works. Never place secrets in checked-in files. Changes apply from the next turn; `/hooks` and `/permissions` show the result.
