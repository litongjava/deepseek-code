---
name: auto-mode-setup
description: "Prepare auto mode for your environment, plus permission rules"
user-invocable: true
disable-model-invocation: true
---
Prepare auto mode for this environment: $ARGUMENTS

Auto mode allows routine local work, asks the user for hard boundaries such as dangerous shell, protected paths, local network, MCP writes, GitHub writes and Config writes, and uses an optional classifier only for unclassified tools; explicit deny and ask rules always win over it.

1. Describe the environment from evidence: the project type, its build and test commands, where generated files go, and sensitive locations (credentials, `.env` files, deployment scripts, production configuration).
2. Propose rules that make auto mode safer and smoother: `deny` rules for secrets and destructive operations (for example `Read(.env*)`, `Bash(git push --force *)`, `Bash(rm -rf *)`), `ask` rules for deployments and anything that reaches shared systems, and a few `allow` rules for routine read-only commands.
3. After the user agrees, add them to `.claude/settings.json` (team) or `.claude/settings.local.json` (personal) under `permissions`, keeping existing entries.
4. Explain how to enable auto mode: `/permissions auto` or Shift+Tab. The classifier is optional; configure it with `/permissions auto-config set {"enabled":true,"classifier":{"kind":"current"}}` only if you want unclassified tools reviewed by a model, and turn it off with `/permissions auto-config off`.
