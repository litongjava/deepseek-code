Create a skill that knows how to run this project's app: $ARGUMENTS

Work out how the application starts by reading its manifests, scripts, README and configuration: the command, required environment variables (names only, never values), ports, prerequisite services, and how to tell it is ready (a log line, an HTTP endpoint or a health check). Prefer the project's own scripts over ad-hoc commands. Check what you can with read-only commands; do not install dependencies or start long-running processes without asking.

Then write `.claude/skills/run/SKILL.md` with a front-matter `name: run` and a one-line `description`, followed by numbered steps to start the app, wait until it is ready, verify it responds, and stop it cleanly. Note anything uncertain. Tell the user the skill is available as `/run` from the next prompt.
