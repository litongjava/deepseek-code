#!/usr/bin/env python3
"""Validate a DeepSeek Code skill using only the Python 3 standard library.

Usage:
    quick_validate.py <skill-directory>

Checks:
- SKILL.md exists and starts with a YAML frontmatter block.
- Frontmatter parses as a simple mapping with string keys.
- `name` and `description` are present; the name uses characters DeepSeek Code
  accepts (letters, digits, `-`, `_`, `:`, `.`) and is at most 100 characters.
- Boolean invocation fields are real YAML booleans.
- The skill body has no unfinished `[TODO: ...]` placeholder outside code.

Unknown frontmatter keys are accepted because DeepSeek Code's loader ignores
them. This validator does not prove that the skill makes good decisions.
"""

import re
import sys
from pathlib import Path

MAX_SKILL_NAME_LENGTH = 100
NAME_PATTERN = re.compile(r"^[A-Za-z0-9._:-]+$")
KEY_PATTERN = re.compile(
    r'^(?:"([^"]+)"|\'([^\']+)\'|([A-Za-z0-9_.-]+))[ \t]*:(?:[ \t]+(.*))?$'
)
BLOCK_SCALARS = {"|", "|-", "|+", ">", ">-", ">+"}
BOOLEAN_KEYS = {"enabled", "user-invocable", "userInvocable", "disable-model-invocation"}


class FrontmatterError(ValueError):
    pass


def _parse_scalar(raw):
    """Parse the scalar forms used in skill frontmatter without YAML."""
    raw = raw.strip()
    if not raw:
        return ""
    if raw[0] == '"':
        if len(raw) < 2 or raw[-1] != '"':
            raise FrontmatterError("unterminated double-quoted value")
        inner = raw[1:-1]
        out = []
        index = 0
        while index < len(inner):
            char = inner[index]
            if char == "\\" and index + 1 < len(inner):
                following = inner[index + 1]
                mapping = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\"}
                if following in mapping:
                    out.append(mapping[following])
                    index += 2
                    continue
            out.append(char)
            index += 1
        return "".join(out)
    if raw[0] == "'":
        if len(raw) < 2 or raw[-1] != "'":
            raise FrontmatterError("unterminated single-quoted value")
        return raw[1:-1].replace("''", "'")
    # A plain scalar ends at an inline comment introduced by whitespace.
    match = re.search(r"[ \t]#", raw)
    if match:
        raw = raw[: match.start()].rstrip()
    return raw


def parse_frontmatter(text):
    """Return (metadata, body) for a normalized SKILL.md string."""
    text = text.lstrip("\ufeff").replace("\r\n", "\n")
    lines = text.split("\n")
    if not lines or lines[0].strip() != "---":
        raise FrontmatterError("No YAML frontmatter found")

    closing = None
    for index in range(1, len(lines)):
        if lines[index].strip() == "---":
            closing = index
            break
    if closing is None:
        raise FrontmatterError("Unterminated YAML frontmatter")

    metadata = {}
    block_key = None
    block_style = None
    block_lines = []

    def flush_block():
        nonlocal block_key, block_style, block_lines
        if block_key is None:
            return
        indents = [
            len(line) - len(line.lstrip(" \t"))
            for line in block_lines
            if line.strip()
        ]
        indent = min(indents) if indents else 0
        merged = [line[indent:] if line.strip() else "" for line in block_lines]
        if block_style == ">":
            value = " ".join(part.strip() for part in merged if part.strip())
        else:
            value = "\n".join(merged).strip("\n")
        metadata[block_key] = value
        block_key = None
        block_style = None
        block_lines = []

    saw_key = False
    for line in lines[1:closing]:
        if block_key is not None and (not line.strip() or line[0] in " \t"):
            block_lines.append(line.rstrip())
            continue
        flush_block()
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line[0] in " \t":
            if not saw_key:
                raise FrontmatterError("Unexpected indented line in frontmatter")
            # Continuation of a nested mapping or list; top-level validation
            # only needs scalars.
            continue
        if line.lstrip().startswith("- "):
            raise FrontmatterError("Frontmatter must be a YAML mapping, not a list")
        match = KEY_PATTERN.match(line)
        if not match:
            raise FrontmatterError("Invalid frontmatter line: " + line.rstrip())
        key = next(group for group in match.groups()[:3] if group is not None)
        if key in metadata:
            raise FrontmatterError("Duplicate frontmatter key: " + key)
        raw_value = match.group(4) or ""
        if raw_value.strip() in BLOCK_SCALARS:
            block_key = key
            block_style = raw_value.strip()[0]
            block_lines = []
            metadata[key] = ""
        else:
            metadata[key] = _parse_scalar(raw_value)
        saw_key = True
    flush_block()

    return metadata, "\n".join(lines[closing + 1 :])


def validate_skill(skill_path):
    skill_path = Path(skill_path).expanduser()
    if not skill_path.is_dir():
        return False, "Skill directory not found: " + str(skill_path)
    skill_md = skill_path / "SKILL.md"
    if not skill_md.is_file():
        return False, "SKILL.md not found"

    try:
        text = skill_md.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return False, "SKILL.md is not valid UTF-8"
    except OSError as error:
        return False, "Cannot read SKILL.md: " + str(error)

    try:
        metadata, body = parse_frontmatter(text)
    except FrontmatterError as error:
        return False, str(error)

    if "name" not in metadata:
        return False, "Missing 'name' in frontmatter"
    if "description" not in metadata:
        return False, "Missing 'description' in frontmatter"

    name = metadata["name"].strip()
    if not name:
        return False, "Name must not be empty"
    if not NAME_PATTERN.match(name):
        return (
            False,
            "Name '" + name + "' may contain only letters, digits, '-', '_', ':', and '.'",
        )
    if len(name) > MAX_SKILL_NAME_LENGTH:
        return (
            False,
            "Name is too long (" + str(len(name)) + " characters). Maximum is "
            + str(MAX_SKILL_NAME_LENGTH) + " characters.",
        )

    description = metadata["description"].strip()
    if not description:
        return False, "Description must not be empty"
    if description.startswith("[TODO:"):
        return False, "Description contains an unfinished TODO placeholder"

    for key in BOOLEAN_KEYS:
        if key in metadata and metadata[key].strip().lower() not in ("true", "false"):
            return False, key + " must be a YAML boolean (true or false)"

    fence_marker = None
    fence_length = 0
    for line in body.splitlines():
        fence = re.match(r"^[ \t]*(?:(?:[-+*]|\d+[.)])[ \t]+)?(`{3,}|~{3,})(.*)$", line)
        if fence:
            marker = fence.group(1)
            if fence_marker is None:
                fence_marker = marker[0]
                fence_length = len(marker)
            elif (
                marker[0] == fence_marker
                and len(marker) >= fence_length
                and not fence.group(2).strip()
            ):
                fence_marker = None
                fence_length = 0
            continue
        if fence_marker is None and re.fullmatch(
            r"[ ]{0,3}\[TODO:[^\n]*\][ \t]*", line
        ):
            return False, "Skill instructions contain an unfinished TODO placeholder"

    return True, "Skill is valid!"


def main():
    if len(sys.argv) != 2:
        print("Usage: python quick_validate.py <skill_directory>")
        sys.exit(1)
    valid, message = validate_skill(sys.argv[1])
    print(message)
    sys.exit(0 if valid else 1)


if __name__ == "__main__":
    main()
