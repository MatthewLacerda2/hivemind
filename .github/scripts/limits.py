#!/usr/bin/env python3
"""`limits.py` — fixed caps on what an agent reads, and on the scripts.

`size.py` holds the Rust. This holds everything else that has a limit:

| What | Cap |
|---|---|
| any `CLAUDE.md` | 300 lines |
| any `.md` under `.claude/skills/` | 500 lines |
| a skill's `name` | 64 characters |
| a skill's `description` | 1024 characters |
| any `.py`, `.js` or `.ts` | 400 lines |

**These are not ratchets.** `size.py`'s numbers were measured against the
largest file and only come down; these were chosen, and stay where they are.

**Every line counts, prose included**, which is the opposite of `size.py`. A
Rust comment is free there because `missing_docs` asks for it. Here the prose
is the cost: `CLAUDE.md` is loaded into every session before any work starts,
and a skill's whole file is loaded whenever it is used. The name and the
description are read on every turn to decide whether to load a skill at all,
and 64 and 1024 are what the Agent Skills format allows.

Files are what git tracks plus what it would track — untracked and not
ignored — so a new file is held to its cap before anybody remembers to add it.

    just limits
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
from dataclasses import dataclass

ROOT = pathlib.Path(__file__).resolve().parents[2]

CLAUDE_MD_LINES = 300
SKILL_MD_LINES = 500
SCRIPT_LINES = 400
NAME_CHARS = 64
DESCRIPTION_CHARS = 1024

SKILLS = ".claude/skills/"
SCRIPT_SUFFIXES = (".py", ".js", ".ts")

# Generated, and held to its source rather than to a cap: `web-build` rebuilds
# it from `web/src/` and fails if it differs from what is checked in.
GENERATED = frozenset({"crates/hivemind-api/assets/hivemind.js"})

FIELD = re.compile(r"^([A-Za-z0-9_-]+):\s*(.*)$")

# A YAML block scalar: the value starts on the next line.
BLOCK = frozenset({"", ">", "|", ">-", "|-", ">+", "|+"})


@dataclass
class Measured:
    """The largest file of each kind, so a pass can say how close it came."""

    kind: str
    path: str
    lines: int
    cap: int


def cap_for(path: str) -> tuple[str, int] | None:
    """Which kind of file `path` is, and its cap, or None if it has none."""
    if path in GENERATED:
        return None
    name = path.rsplit("/", 1)[-1]
    if name == "CLAUDE.md":
        return "CLAUDE.md", CLAUDE_MD_LINES
    if path.startswith(SKILLS) and name.endswith(".md"):
        return "skill", SKILL_MD_LINES
    if name.endswith(SCRIPT_SUFFIXES):
        return "script", SCRIPT_LINES
    return None


def unquote(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        return value[1:-1]
    return value


def frontmatter(text: str) -> dict[str, str] | None:
    """The `key: value` fields between the opening and closing `---`.

    Enough YAML for a skill's header: plain and quoted values, and block
    scalars, whose lines are joined with a space. None when there is no header
    or it is never closed.
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return None
    fields: dict[str, str] = {}
    key = None
    for line in lines[1:]:
        if line.strip() == "---":
            return fields
        match = FIELD.match(line)
        if match:
            key, value = match.groups()
            fields[key] = "" if value.strip() in BLOCK else unquote(value)
        elif key and line[:1].isspace():
            fields[key] = f"{fields[key]} {line.strip()}".strip()
    return None


def skill_problems(path: str, text: str) -> list[str]:
    fields = frontmatter(text)
    if fields is None:
        return [f"{path}: no `---` header, so no name or description"]
    problems = []
    for field, cap in (("name", NAME_CHARS), ("description", DESCRIPTION_CHARS)):
        value = fields.get(field, "")
        if not value:
            problems.append(f"{path}: `{field}` is missing or empty")
        elif len(value) > cap:
            problems.append(f"{path}: `{field}` is {len(value)} characters > {cap}")
    return problems


def check(files: dict[str, str]) -> tuple[list[str], list[Measured]]:
    """Every problem in `files` (path to text), and the largest of each kind."""
    problems: list[str] = []
    largest: dict[str, Measured] = {}
    for path, text in sorted(files.items()):
        if path.startswith(SKILLS) and path.endswith("/SKILL.md"):
            problems.extend(skill_problems(path, text))
        found = cap_for(path)
        if found is None:
            continue
        kind, cap = found
        lines = len(text.splitlines())
        if lines > cap:
            problems.append(f"{path}: {lines} lines > {cap}")
        if kind not in largest or lines > largest[kind].lines:
            largest[kind] = Measured(kind, path, lines, cap)
    return problems, list(largest.values())


def read_files(root: pathlib.Path = ROOT) -> dict[str, str]:
    """Every file git tracks or would track that has a cap or is a skill."""
    listed = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root,
        capture_output=True,
        check=True,
    ).stdout.decode("utf-8")
    files: dict[str, str] = {}
    for path in filter(None, listed.split("\0")):
        if cap_for(path) is None and not path.startswith(SKILLS):
            continue
        try:
            files[path] = (root / path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            # Deleted but still in the index, or not text: nothing to measure.
            continue
    return files


def main() -> int:
    problems, largest = check(read_files())

    # A gate that found nothing to measure passes, and says nothing about why.
    # Both of these exist, so not finding them means the search broke.
    kinds = {m.kind for m in largest}
    for expected in ("CLAUDE.md", "skill"):
        if expected not in kinds:
            problems.append(f"found no {expected} to check; the search is broken")

    if problems:
        print(f"\nlimits: {len(problems)} problem(s).", file=sys.stderr)
        for problem in problems:
            print(f"    {problem}", file=sys.stderr)
        print(
            "\nSplit by concern. A long CLAUDE.md usually holds procedure that"
            " belongs in a skill, which loads only when it is used.",
            file=sys.stderr,
        )
        return 1

    summary = ", ".join(
        f"{m.kind} {m.lines}/{m.cap} ({m.path})"
        for m in sorted(largest, key=lambda m: m.kind)
    )
    print(f"limits: all within their caps. Largest: {summary}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
