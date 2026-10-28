Prepare auto mode for this environment: $ARGUMENTS

Auto mode lets a classifier model approve tool calls that stay within what the user asked; explicit deny and ask rules always win over it.

1. Describe the environment from evidence: the project type, its build and test commands, where generated files go, and sensitive locations (credentials, `.env` files, deployment scripts, production configuration).
2. Propose rules that make auto mode safer and smoother: `deny` rules for secrets and destructive operations (for example `Read(.env*)`, `Bash(git push --force *)`, `Bash(rm -rf *)`), `ask` rules for deployments and anything that reaches shared systems, and a few `allow` rules for routine read-only commands.
3. After the user agrees, add them to `.claude/settings.json` (team) or `.claude/settings.local.json` (personal) under `permissions`, keeping existing entries.
4. Explain how to enable auto mode: `/permissions auto-config set {"enabled":true,"classifier":{"kind":"current"}}`, then Shift+Tab or `/permissions auto`; and how to turn it off with `/permissions auto-config off`.
