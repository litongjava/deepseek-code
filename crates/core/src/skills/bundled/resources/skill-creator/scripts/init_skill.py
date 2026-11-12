#!/usr/bin/env python3
"""Create a new DeepSeek Code skill from a small template.

Usage:
    init_skill.py <skill-name> [--path <path>] [--resources scripts,references,assets] [--examples]

Examples:
    init_skill.py my-new-skill
    init_skill.py my-new-skill --path ~/.deepseek-code/skills
    init_skill.py my-api-helper --path ./skills --resources scripts --examples

The initializer uses only the Python 3 standard library and never edits an
existing skill directory.
"""

import argparse
import os
import re
import sys
from pathlib import Path

MAX_SKILL_NAME_LENGTH = 100
ALLOWED_RESOURCES = {"scripts", "references", "assets"}

SKILL_TEMPLATE = """---
name: {skill_name}
description: "Explain what this skill does and when it applies."
---

# {skill_title}

State the outcome this skill should produce, the non-obvious constraints, and
the supporting files or tools it needs. Replace this paragraph with guidance
specific to the task.
"""

EXAMPLE_SCRIPT = '''#!/usr/bin/env python3
"""Example helper for {skill_name}.

Replace or delete this file. Keep helpers runnable with the Python 3 standard
library unless the skill documents and the user approves an extra dependency.
"""


def main():
    print("This is an example script for {skill_name}")


if __name__ == "__main__":
    main()
'''

EXAMPLE_REFERENCE = """# Reference for {skill_title}

Replace this with maintained, task-specific details that DeepSeek Code would
not reliably know, such as operational constraints, local schemas, or fragile
integration behavior.

Delete this file if no supported workflow needs it.
"""

EXAMPLE_ASSET = """# Example asset placeholder

Replace this with an actual asset (template, image, font, boilerplate
directory, or data file) that belongs in generated output. Assets are not
loaded as instructions.
"""


def default_skills_path():
    """The data-directory skills folder DeepSeek Code manages."""
    data_dir = os.environ.get("DEEPSEEK_CODE_DATA_DIR")
    root = Path(data_dir).expanduser() if data_dir else Path.home() / ".deepseek-code"
    return root / "skills"


def normalize_skill_name(skill_name):
    """Normalize to a lowercase hyphenated folder name."""
    normalized = skill_name.strip().lower()
    normalized = re.sub(r"[^a-z0-9]+", "-", normalized)
    normalized = normalized.strip("-")
    normalized = re.sub(r"-{2,}", "-", normalized)
    return normalized


def title_case_skill_name(skill_name):
    return " ".join(word.capitalize() for word in skill_name.split("-"))


def parse_resources(raw_resources):
    if not raw_resources:
        return []
    resources = [item.strip() for item in raw_resources.split(",") if item.strip()]
    invalid = sorted({item for item in resources if item not in ALLOWED_RESOURCES})
    if invalid:
        allowed = ", ".join(sorted(ALLOWED_RESOURCES))
        print("[ERROR] Unknown resource type(s): " + ", ".join(invalid))
        print("   Allowed: " + allowed)
        sys.exit(1)
    deduped = []
    seen = set()
    for resource in resources:
        if resource not in seen:
            deduped.append(resource)
            seen.add(resource)
    return deduped


def create_resource_dirs(skill_dir, skill_name, skill_title, resources, include_examples):
    for resource in resources:
        resource_dir = skill_dir / resource
        resource_dir.mkdir(exist_ok=True)
        if resource == "scripts" and include_examples:
            example_script = resource_dir / "example.py"
            example_script.write_text(EXAMPLE_SCRIPT.format(skill_name=skill_name))
            example_script.chmod(0o755)
            print("[OK] Created scripts/example.py")
        elif resource == "references" and include_examples:
            (resource_dir / "api_reference.md").write_text(
                EXAMPLE_REFERENCE.format(skill_title=skill_title)
            )
            print("[OK] Created references/api_reference.md")
        elif resource == "assets" and include_examples:
            (resource_dir / "example_asset.txt").write_text(EXAMPLE_ASSET)
            print("[OK] Created assets/example_asset.txt")
        else:
            print("[OK] Created " + resource + "/")


def init_skill(skill_name, path, resources, include_examples):
    skill_dir = Path(path).expanduser().resolve() / skill_name
    if skill_dir.exists():
        print("[ERROR] Skill directory already exists: " + str(skill_dir))
        return None

    try:
        skill_dir.mkdir(parents=True, exist_ok=False)
        print("[OK] Created skill directory: " + str(skill_dir))
    except OSError as error:
        print("[ERROR] Error creating directory: " + str(error))
        return None

    skill_title = title_case_skill_name(skill_name)
    try:
        (skill_dir / "SKILL.md").write_text(
            SKILL_TEMPLATE.format(skill_name=skill_name, skill_title=skill_title)
        )
        print("[OK] Created SKILL.md")
    except OSError as error:
        print("[ERROR] Error creating SKILL.md: " + str(error))
        return None

    if resources:
        try:
            create_resource_dirs(
                skill_dir, skill_name, skill_title, resources, include_examples
            )
        except OSError as error:
            print("[ERROR] Error creating resource directories: " + str(error))
            return None

    print("\n[OK] Skill '" + skill_name + "' initialized successfully at " + str(skill_dir))
    print("\nNext steps:")
    print("1. Edit SKILL.md: replace the description and body with task-specific guidance")
    if resources and include_examples:
        print("2. Customize or delete the example files in the requested resource folders")
    elif resources:
        print("2. Add only the resources the skill actually needs")
    else:
        print("2. Create scripts/, references/, or assets/ only if the skill needs them")
    print("3. Run: python3 " + str(Path(__file__).resolve().parent / "quick_validate.py") + " " + str(skill_dir))
    print("4. Test the skill on a realistic request before relying on it")
    return skill_dir


def main():
    parser = argparse.ArgumentParser(
        description="Create a new DeepSeek Code skill directory with a SKILL.md template."
    )
    parser.add_argument("skill_name", help="Skill name (normalized to hyphen-case)")
    parser.add_argument(
        "--path",
        default=str(default_skills_path()),
        help="Output directory for the skill (default: data-directory skills folder)",
    )
    parser.add_argument(
        "--resources",
        default="",
        help="Comma-separated list: scripts,references,assets",
    )
    parser.add_argument(
        "--examples",
        action="store_true",
        help="Create example files inside the selected resource directories",
    )
    args = parser.parse_args()

    raw_skill_name = args.skill_name
    skill_name = normalize_skill_name(raw_skill_name)
    if not skill_name:
        print("[ERROR] Skill name must include at least one letter or digit.")
        sys.exit(1)
    if len(skill_name) > MAX_SKILL_NAME_LENGTH:
        print(
            "[ERROR] Skill name '" + skill_name + "' is too long ("
            + str(len(skill_name)) + " characters). Maximum is "
            + str(MAX_SKILL_NAME_LENGTH) + " characters."
        )
        sys.exit(1)
    if skill_name != raw_skill_name:
        print("Note: Normalized skill name from '" + raw_skill_name + "' to '" + skill_name + "'.")

    resources = parse_resources(args.resources)
    if args.examples and not resources:
        print("[ERROR] --examples requires --resources to be set.")
        sys.exit(1)

    print("Initializing skill: " + skill_name)
    print("   Location: " + args.path)
    if resources:
        print("   Resources: " + ", ".join(resources))
        if args.examples:
            print("   Examples: enabled")
    else:
        print("   Resources: none (create as needed)")
    print()

    result = init_skill(skill_name, args.path, resources, args.examples)
    sys.exit(0 if result else 1)


if __name__ == "__main__":
    main()
