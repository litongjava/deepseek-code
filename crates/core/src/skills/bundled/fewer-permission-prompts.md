Reduce permission prompts for this project with safe allow rules: $ARGUMENTS

1. Identify the read-only commands this project relies on: inspect its manifests and scripts (package.json, Cargo.toml, Makefile, pyproject.toml and similar) and the project instructions. Typical candidates are test, lint, type-check and build-inspection commands that only read files, plus read-only git commands.
2. Leave out anything that writes files outside build output, installs dependencies, reaches the network, deletes, or runs arbitrary scripts.
3. Propose a short, prioritized list of rules in dcl's syntax, for example `Bash(npm run lint *)` or `Bash(cargo test *)`, each with one line on why it is safe.
4. After the user agrees, add them to `.claude/settings.json` under `permissions.allow` (create the file if needed, keep existing entries), or tell the user to add them for this session with `/permissions allow RULE`.

Never add rules that grant write access or broad wildcards such as `Bash(*)`.
