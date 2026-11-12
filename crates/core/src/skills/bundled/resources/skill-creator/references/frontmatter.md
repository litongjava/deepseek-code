# SKILL.md frontmatter in DeepSeek Code

DeepSeek Code reads `SKILL.md` files found in user, project, plugin, and bundled
skill roots. The YAML frontmatter must be a mapping. Unknown keys are ignored,
so a file can keep extra metadata for other tools; the keys below are the ones
DeepSeek Code acts on.

## Keys

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `name` | string | folder name | Skill and slash-command name. Keep it to letters, digits, `-`, `_`, `:`, and `.`; `/`, whitespace, and control characters are rejected when the skill is loaded. |
| `description` | string | empty | Short description shown in skill listings and used to decide when the skill applies. |
| `whenToUse` | string | none | Extra usage hint kept by the skill manager. `when` and `when_to_use` are accepted aliases. |
| `enabled` | boolean | `true` | `false` removes the skill from normal discovery and from the enabled list. Must be a real YAML boolean. |
| `user-invocable` | boolean | `true` | Allows explicit `/skill-name` invocation. `userInvocable` is an accepted alias. |
| `disable-model-invocation` | boolean | `false` | `true` keeps the skill out of automatic model selection while retaining explicit invocation. |
| `allowedTools` | list of strings | none | Parsed and kept with the skill; the current engine does not use it to restrict tools. Normal permissions still apply. |
| `model` | string | none | Parsed and kept with the skill; it does not override the session model. |
| `triggers` | list of strings | none | Parsed and kept with the skill. |
| `context` | string | none | `fork` runs the skill in an isolated child session. Other values are rejected as unsupported. |
| `shell` | string | host default | Interpreter for `!` backtick shell expansion inside the skill body. Expansion still requires normal Bash approval. |
| `agent` | string | `general` | Agent definition used by an isolated `context: fork` child. |
| `od` | mapping | none | Open Design metadata used by the design-skill screens. Leave it out of ordinary skills. |
| `license` | string | none | Free-form license name. DeepSeek Code ignores it; keep it when the skill has its own license. |
| `metadata` | mapping | none | Ignored by DeepSeek Code; safe to keep for other tools. |

## Rules enforced at load time

- The file must start with `---` and close the frontmatter with a `---` line.
  A leading UTF-8 BOM and CRLF line endings are accepted.
- Frontmatter must parse as a YAML mapping. `enabled`, `user-invocable`,
  `userInvocable`, and `disable-model-invocation` must be booleans when present.
- A skill file larger than 1 MiB is skipped by the discovery scanner.
- A skill whose computed name contains a character outside
  letters/digits/`-`/`_`/`:`/`.` is skipped; invalid files never replace a valid
  definition with the same name.

`scripts/quick_validate.py` checks the required `name` and `description`, the
name character set, boolean fields, and unfinished `[TODO: ...]` placeholders.
It does not reject unknown keys, matching DeepSeek Code's tolerant parser.

## Invocation

- The model invokes a skill through the `Skill` tool with `skill=<name>`.
- The user invokes it as `/skill-name` in the CLI or selects it in the app.
- `user-invocable: false` blocks the slash form only.
- `disable-model-invocation: true` blocks the model form only; the slash form
  still works.
