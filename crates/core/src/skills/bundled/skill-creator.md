
# Skill Creator

Create skills that give DeepSeek Code useful, non-obvious guidance without constraining unrelated work.

> Adapted from OpenAI Codex's `skill-creator` (Apache-2.0). These files were
> modified for DeepSeek Code; see `NOTICE.md` and `license.txt` in this skill
> directory.

## Core Principles

**Assume DeepSeek Code is already capable.** Include only information that changes its decisions or improves its work. Remove generic advice, repeated instructions, speculative edge cases, and examples that do not materially clarify the task.

**Preserve user intent and scope.** A skill should support the requested task, not replace the user's chosen product, expand the assignment, modify unrelated configuration, or imply permission for additional external actions. Do not turn a particular example, past failure, or personal preference into a universal requirement.

Approval to complete a task does not expand its scope or execution permissions. For retrying or externally mutating workflows, define a stopping condition proportional to the risk.

**Match specificity to the risk.** Give the model room to choose an appropriate approach when multiple approaches are reasonable. Use detailed steps, deterministic scripts, or absolute language only when correctness, safety, permissions, or a genuinely fragile workflow requires them.

For open-ended work, describe the outcome and relevant decision criteria. For workflows with a preferred shape, offer useful examples or configurable scripts. Reserve fixed sequences and narrow parameters for operations where deviation would cause a concrete problem. Preserve non-obvious operational invariants, distinguish actual requirements from optional recommendations or local conventions, and avoid restating policies already enforced elsewhere.

**Keep discovery cheap and precise.** Skill names and descriptions are available before a skill is loaded. Describe the actual capability and when it applies, adding exclusions only when they prevent likely misrouting. Avoid exhaustive capability lists and catchalls that attract unrelated requests.

Keep skills self-contained; refer to another skill or tool only when the requested workflow genuinely requires it and it is available in the target environment. Specialized review, hardening, or audit workflows should apply when requested or genuinely needed, not merely because ordinary work touches the same subject.

**Disclose detail progressively.** Keep shared purpose, essential constraints, and useful routing in `SKILL.md`. Put substantial mode-specific guidance, schemas, examples, or procedures in supporting references and read only the references relevant to the current task. A simple self-contained skill does not need a router or extra files.

## Anatomy of a Skill

Every skill is a folder containing a required `SKILL.md` file and any optional resources its actual workflow needs:

```text
skill-name/
|-- SKILL.md                 Required skill instructions
|   |-- YAML frontmatter     Required name and description
|   `-- Markdown body        Instructions loaded when the skill is used
|-- scripts/                 Optional executable helpers
|-- references/              Optional documentation loaded as needed
`-- assets/                  Optional files used in generated output
```

Choose the structure that fits the actual task. Some skills are short and self-contained; others route among operating modes or delegate complex mechanics to scripts. Avoid creating directories, placeholders, examples, or ancillary documentation without a clear use.

### SKILL.md

The YAML frontmatter identifies the skill and determines when it should be considered. Include the required `name` and `description`, and use the DeepSeek Code fields listed in [references/frontmatter.md](references/frontmatter.md) when they are needed.

The Markdown body is loaded only when the skill is used. Put the purpose, essential workflow, real constraints, and useful links there. Keep detailed procedures and examples in supporting references when they are relevant only to particular modes.

Skill information is disclosed in three stages:

1. **Name and description:** Available during skill selection, so keep them concise and discriminating.
2. **SKILL.md body:** Loaded when the skill applies, so keep its instructions relevant to that task.
3. **Supporting resources:** Read or execute only when the current task actually needs them.

The entrypoint should be as short as the task permits while retaining important constraints. A large upper bound is not a target: move conditional detail into references when doing so improves clarity or context use, rather than waiting for the file to become unwieldy.

### Scripts

Use `scripts/` for executable code when the same logic would otherwise be rewritten repeatedly or deterministic execution materially improves reliability.

- **Example:** `scripts/rotate_pdf.py` for a PDF operation that would otherwise require recreating the same code.
- **Useful for:** Repeated transformations, reliable API operations, data processing, and other concrete automation.
- **Validation:** Run new or changed scripts to verify their behavior. Scripts can usually be executed without loading their full implementation into context, although an agent may need to inspect them when patching or adapting them.
- **Dependencies:** DeepSeek Code does not install Python packages for a skill. Keep helper scripts runnable with the Python 3 standard library only, or document the dependency and ask before adding it.

### References

Use `references/` for documentation that is needed only in particular contexts.

- **Examples:** `references/schema.md` for database tables, `references/policies.md` for domain rules, `references/api_docs.md` for an API, or separate writing guides for different deliverables.
- **Useful for:** Schemas, API documentation, company policies, format-specific procedures, detailed workflows, and substantial examples.
- **Routing:** Link each reference from `SKILL.md` or another relevant resource and explain when it should be read. The model reads referenced files with the `Read` tool when the skill is invoked.

For large references, include useful search terms or a short contents section when that makes the needed material easier to find.

### Assets

Use `assets/` for files that belong in generated output rather than in the model's instructions.

- **Examples:** `assets/logo.png`, `assets/slides.pptx`, `assets/font.ttf`, or `assets/frontend-template/`.
- **Useful for:** Templates, images, fonts, icons, boilerplate projects, and other files copied or adapted into the result.
- **Context:** Do not load assets as instructions unless the task requires inspecting them.

### Invocation Policy

DeepSeek Code exposes skills in two ways: the model can invoke a skill through the `Skill` tool, and a user can run it as `/skill-name` in the CLI or select it in the app. Frontmatter controls both:

- `user-invocable` (alias `userInvocable`): allow explicit `/skill-name` invocation. Defaults to true.
- `disable-model-invocation`: keep the skill out of automatic model selection; the user can still invoke it explicitly. Defaults to false.

Automatic skill selection is allowed by default. Change that default only when the user explicitly requests an explicit-only skill:

```yaml
disable-model-invocation: true
```

Keep the skill discoverable and require authorization immediately before an actual mutation; do not infer explicit-only invocation from sensitive operations or required approvals. Preserve an existing skill's invocation policy unless the user asks to change it.

### What Not to Include

Include files that directly support the skill's work. Avoid adding a `README.md`, installation guide, changelog, duplicated quick reference, or other auxiliary documentation unless a specific task or packaging requirement calls for it.

## Progressive Disclosure in Practice

For a skill with multiple substantial modes, keep the shared guidance and mode-selection criteria in `SKILL.md`. Link each supporting reference where its use becomes relevant. Do not load every reference by default, duplicate reference content in the entrypoint, or add a routing layer when there is nothing meaningful to route.

For example, a deployment skill can keep provider selection in `SKILL.md` and separate provider details:

```text
cloud-deploy/
|-- SKILL.md
`-- references/
    |-- aws.md
    |-- gcp.md
    `-- azure.md
```

When the user chooses AWS, read `references/aws.md`; do not also load the GCP and Azure guides. The same pattern can separate business domains, deliverable types, or other genuinely distinct operating modes.

A short skill can instead route to details only when an advanced operation needs them:

```markdown
## Documents

Handle ordinary edits directly.

- For tracked changes, read [references/redlining.md](references/redlining.md).
- For document internals, read [references/ooxml.md](references/ooxml.md).
```

These examples illustrate options, not a required structure. Choose the organization that makes the skill easier to use without loading irrelevant material.

## Create or Update a Skill

Adapt the work to the request. Creating a complex new skill may involve understanding realistic use cases, choosing supporting resources, initializing files, writing instructions, and validating the result. A narrow update to an existing skill may require only a focused edit and validation.

Ask clarifying questions only when the missing information matters and cannot be reasonably inferred. Respect a user-specified location. Otherwise create a user skill in the DeepSeek Code data directory: `$DEEPSEEK_CODE_DATA_DIR/skills` when that variable is set, or `~/.deepseek-code/skills` by default. DeepSeek Code also reads user skills from `$CLAUDE_CONFIG_DIR/skills` (default `~/.claude/skills`) and project skills from `.claude/skills` in the workspace; a skill placed there is discovered without copying it into the data directory.

Keep automatic skill selection enabled unless the user explicitly requests an explicit-only skill. When the intended invocation mode is genuinely unclear and matters to the requested workflow, ask whether the user wants normal automatic discovery or explicit-only invocation; otherwise preserve the default. Do not infer explicit-only invocation from sensitive operations or required approvals: keep the skill discoverable and require authorization immediately before the actual mutation. Preserve an existing skill's invocation policy unless the user asks to change it.

For a new or substantially revised skill, consider the actual requests it should handle and which reusable resources would improve those tasks:

- A repeated PDF transformation may justify a `scripts/rotate_pdf.py` helper.
- An application-building workflow may benefit from an `assets/frontend-template/` starter.
- A data-analysis skill may need a `references/schema.md` guide to avoid rediscovering table relationships.

Create those resources only when their concrete benefit justifies them. If the user has already explained the task clearly, proceed without requesting additional examples.

### Naming

- Use lowercase letters, digits, and hyphens when no existing convention requires otherwise.
- Keep names short and under 64 characters.
- Namespace by tool or domain when doing so improves discovery.
- Name the skill folder after the skill. DeepSeek Code accepts letters, digits, `-`, `_`, `:`, and `.` in a frontmatter name; keep `/`, spaces, and control characters out.
- Avoid a name that matches a built-in slash command; the built-in command wins.

### Initialize a New Skill

For a new skill, use the bundled initializer when it helps create the required files consistently:

```bash
python3 scripts/init_skill.py <skill-name> --path <output-directory> [--resources scripts,references,assets] [--examples]
```

For example:

```bash
python3 scripts/init_skill.py my-skill --path "${DEEPSEEK_CODE_DATA_DIR:-$HOME/.deepseek-code}/skills"
python3 scripts/init_skill.py my-skill --path "${DEEPSEEK_CODE_DATA_DIR:-$HOME/.deepseek-code}/skills" --resources references
```

Request only the resource directories the skill needs. Use `--examples` only when concrete placeholders would help, and replace or remove them before finishing. Do not initialize an existing skill again.

The initializer creates the skill directory, a concise `SKILL.md` starter, and any requested resource directories. It uses only the Python 3 standard library. `--path` defaults to the data-directory `skills` folder when omitted.

### Write the Instructions

The frontmatter `description` should briefly explain what the skill does and when it applies. Include a meaningful boundary when similar requests should not activate the skill.

For example:

```yaml
description: Create or edit Word documents when formatting, tracked changes, or comments require document-specific handling.
```

Put detailed workflows, tool choices, examples, and operating modes in the body or relevant references rather than listing them all in the description.

Write only the instructions needed for another DeepSeek Code instance to perform the task well. State the desired outcome, non-obvious context, real constraints, and relevant references or tools. Preserve the user's explicit choices and existing authorization boundaries. Avoid prescribing a fixed structure, process, or number of steps when the task does not require one.

### Validate and Iterate

Validate the completed skill with:

```bash
python3 scripts/quick_validate.py <path/to/skill-folder>
```

The validator checks the frontmatter DeepSeek Code reads, the naming rules, and unfinished scaffold placeholders; it does not prove that the skill makes good decisions. It uses only the Python 3 standard library, so it works without installing PyYAML. Also check that descriptions remain discriminating, instructions preserve user intent, references are discoverable, and any added scripts actually work.

When testing is warranted, verify observable behavior or meaningful invariants. Avoid tests that merely match generated wording, headings, or regex patterns.

Improve the skill based on real usage or demonstrated failures. Prefer a narrow correction to accumulating universal rules for every observed example.

## Independent Forward-Testing

Use an independent subagent pass when a skill is sufficiently complex or risky that realistic behavioral validation would add meaningful confidence, and when delegation is available and authorized. Ordinary creation or small edits do not automatically require subagents.

Give the evaluating agent a realistic user request, the skill, and the minimum raw artifacts needed to perform the task. Do not provide the intended answer, suspected bug, proposed fix, or prior conclusions unless the evaluation genuinely requires them.

For example:

```text
Use the Skill tool with skill=skill-name and place the skill at /path/to/skill-name to complete this realistic request.
```

Keep the evaluation scoped to permitted resources and side effects. Use an isolated temporary workspace for generated artifacts so they do not enter the working tree or contaminate later evaluations. Ask for approval when the proposed evaluation would require additional authorization, affect a live production system, or impose substantial time or cost. Review the actual outcome and artifacts, then make only changes supported by the observed behavior.
