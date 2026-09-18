#!/usr/bin/env python3
"""`stale_markers.py` — refuse a `TODO(Mn)` whose milestone has shipped.

A marker naming future work is useful. The same marker after that work landed
is a lie in a file somebody trusts, and it is invisible: nothing reads it, so
nothing complains.

Three were found in this repo at M6, one of them inside the very document the
milestone was supposed to have finished:

    docs/protocol.md:208  <!-- TODO(M1): the slug table … -->
    README.md:77          <!-- TODO(M5): screenshot of the web UI … -->
    README.md:85          <!-- TODO(M1): generated key-by-key reference … -->

M1 shipped four milestones earlier. Nobody was going to notice.

Which milestones have shipped is read from `SPEC.md` rather than kept here, so
there is one place to update when a milestone lands and this cannot drift from
it. A milestone counts as shipped when `CHANGELOG.md` or the git history says
so — see [`shipped`].

    just markers
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]

# `TODO(M3)`, `FIXME(M4)`, in any file type. The milestone is the whole point:
# a bare `TODO` has no expiry and this says nothing about it.
#
# A backticked one is prose *about* a marker — "this used to carry a
# `TODO(M1)`" — and not a marker. Writing that sentence is how the distinction
# was noticed: the gate caught the two comments explaining what it had just
# found. Real markers are never quoted, because nothing renders them.
MARKER = re.compile(r"(?<!`)\b(?:TODO|FIXME|XXX)\((M\d+)\)(?!`)")

# Where to look. Generated files and dependencies are somebody else's markers.
SEARCHED = ("crates", "docs", "web/src", "README.md", "SPEC.md", "CONTRIBUTING.md")
SKIP_SUFFIXES = (".lock", ".json", ".min.js")


def shipped() -> set[str]:
    """Milestones with a merge commit in the history.

    Read from git rather than from a list here: the list would be the thing to
    forget to update, which is the failure this script is about.

    A milestone is shipped when a merge commit mentions its branch — the
    branches are named `m3-peers`, `m4-attachments` and so on, and every one
    was merged with a merge commit by policy (ADR 0009).
    """
    try:
        log = subprocess.run(
            ["git", "log", "--merges", "--format=%s"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError):
        # No git history to read — a tarball, or a shallow clone. Saying
        # nothing is shipped means this gate passes, which is the right way
        # for it to fail: it is a tidiness check, not a correctness one.
        return set()

    return {
        f"M{match.group(1)}"
        for match in re.finditer(r"\bfrom [\w-]+/m(\d+)-", log, re.IGNORECASE)
    }


def files() -> list[pathlib.Path]:
    found: list[pathlib.Path] = []
    for entry in SEARCHED:
        path = ROOT / entry
        if path.is_file():
            found.append(path)
        elif path.is_dir():
            found.extend(
                candidate
                for candidate in sorted(path.rglob("*"))
                if candidate.is_file() and candidate.suffix not in SKIP_SUFFIXES
            )
    return found


def stale(done: set[str]) -> list[str]:
    """Every marker naming a milestone in `done`."""
    found: list[str] = []
    for path in files():
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for number, line in enumerate(text.splitlines(), start=1):
            for match in MARKER.finditer(line):
                if match.group(1) in done:
                    relative = path.relative_to(ROOT).as_posix()
                    found.append(f"{relative}:{number}: {line.strip()}")
    return found


def main() -> int:
    done = shipped()
    if not done:
        print("markers: no shipped milestones found in the history — nothing to check")
        return 0

    found = stale(done)
    if not found:
        print(f"markers: ok — nothing left over from {', '.join(sorted(done))}")
        return 0

    print(
        f"\nmarkers: {len(found)} marker(s) name a milestone that has shipped"
        f" ({', '.join(sorted(done))}).",
        file=sys.stderr,
    )
    for line in found:
        print(f"    {line}", file=sys.stderr)
    print(
        "\nDo the work, or delete the marker, or re-point it at the milestone"
        " that will actually do it. A marker for work that already happened is"
        " a lie in a file somebody trusts.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
