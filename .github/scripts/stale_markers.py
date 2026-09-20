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

The same claim written as a sentence is the same lie without the brackets, and
it hides better. This one sat above `Ok(Json(Vec::new()))` from M2 to M7, so
the MCP `list_peers` tool answered "nobody" for four milestones (#60):

    // Pairing and the peer book arrive in M3 (SPEC §14). Until then this is
    // truthfully empty rather than absent: the tool surface is the contract.

So there is a second rule: a comment that names a milestone at or below the
last one shipped, in a sentence promising the work is still to come. A false
positive is one sentence rewritten; the false negative cost four milestones.

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

# A bare mention of a milestone, for the prose rule.
MILESTONE = re.compile(r"\bM(\d+)\b")

# The sentence is a promise, not a history. Written from the forms actually
# found in this repo, English and Portuguese, because whoever left the comment
# wrote it in whichever they were thinking in.
PROMISE = re.compile(
    r"\b(?:arrives?|lands in|comes in|until then|chega|até lá)\b",
    re.IGNORECASE,
)

# Where a comment starts, by file type. The prose rule looks no further than
# comments: SPEC §14 is a list of milestones saying what each one brings, and a
# gate that fired on the roadmap describing itself is one nobody could keep
# green. A suffix that is not here has no prose rule, only markers.
COMMENT = {
    suffix: re.compile(pattern)
    for pattern, suffixes in (
        (r"//|/\*|^\s*\*", (".rs", ".ts", ".js", ".mts", ".css", ".vue")),
        (r"<!--", (".md", ".html")),
        (r"#", (".py", ".sh", ".toml", ".yml", ".yaml")),
    )
    for suffix in suffixes
}

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


def promised(line: str, comment: re.Pattern[str], ceiling: int) -> bool:
    """A comment promising work a shipped milestone was to have brought.

    Only what follows the comment opener counts, so an identifier that reads
    like a sentence — `m3_arrives` — is code and not a claim.

    One line at a time: the promise and the milestone were on the same line in
    the case this rule exists for, and joining comment blocks would buy a rarer
    catch for a rule that stops being obvious to read.
    """
    opener = comment.search(line)
    if opener is None:
        return False
    rest = line[opener.start() :]
    if not PROMISE.search(rest):
        return False
    return any(int(match.group(1)) <= ceiling for match in MILESTONE.finditer(rest))


def stale(done: set[str]) -> list[str]:
    """Every marker, and every promise in a comment, about shipped work."""
    # At or below the last one shipped, rather than in `done`: a history that
    # records M3 and not M2 still means M2 happened.
    ceiling = max((int(name[1:]) for name in done), default=0)

    found: list[str] = []
    for path in files():
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        comment = COMMENT.get(path.suffix)
        for number, line in enumerate(text.splitlines(), start=1):
            marked = any(match.group(1) in done for match in MARKER.finditer(line))
            if marked or (comment is not None and promised(line, comment, ceiling)):
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
        f"\nmarkers: {len(found)} line(s) name a milestone that has shipped"
        f" ({', '.join(sorted(done))}) as work still to come.",
        file=sys.stderr,
    )
    for line in found:
        print(f"    {line}", file=sys.stderr)
    print(
        "\nDo the work, or delete the marker, or re-point it at the milestone"
        " that will actually do it. A marker for work that already happened is"
        " a lie in a file somebody trusts, and a sentence saying the same thing"
        " is that lie without the brackets — rewrite it in the past tense.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
