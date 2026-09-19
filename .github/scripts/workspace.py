#!/usr/bin/env python3
"""`workspace.py` — is this machine fit to believe a green run from?

Two checks that have nothing to do with the code and everything to do with
whether a gate's answer means anything. Both are about the same thing: a
build that did not happen, reported as one that passed.

**A shared `CARGO_TARGET_DIR` is refused.** `CLAUDE.md` has carried the rule
since worktrees arrived — one worktree, one `target/` — and nothing checked
it. Cargo keys artifacts by package, version, features and profile, never by
source path, so two worktrees pointed at one directory overwrite each other's
output for anything a given build did not itself rebuild. The phantom
failures waste an hour. **The false green is why this is a gate**: it claims
every other gate passed on code that was never compiled.

**Low disk is a warning, not a refusal.** It is somebody else's judgement how
full their disk is, and a machine at 9 GB will finish most builds. But when it
does run out, it does not say "no disk" — it says `Error: something went wrong
on this node`, or a linker error, or four unrelated integration tests failing
at once (#61). Half an hour of this session went on exactly that, in the
middle of chasing a real flake, and the noise looked like signal.

So: the refusal is for the thing that lies, and the warning is for the thing
that merely confuses. `just ci` runs this before anything expensive compiles.

The two judgements are pure functions taking plain values, so the tests beside
this file need no disk and no environment.
"""

from __future__ import annotations

import os
import shutil
import sys

# The variable that breaks the one-worktree-one-target rule.
SHARED = "CARGO_TARGET_DIR"

# Below this many gigabytes free, say so. Measured rather than chosen: a clean
# `target/` here is about 13 GB and the workspace has carried two at once, so
# ten is roughly "one more build would not fit".
FLOOR_GB = 10

GB = 1024**3


def shared_target(env: dict[str, str]) -> list[str] | None:
    """Why a shared target directory is refused, or `None` if there is none.

    An empty value is not set, because `CARGO_TARGET_DIR=` is how somebody
    unsets it in a shell and cargo reads it the same way.
    """
    value = env.get(SHARED, "").strip()
    if not value:
        return None

    return [
        f"{SHARED} is set to {value}.",
        "Cargo keys artifacts by package, version, features and profile —"
        " never by source path — so two worktrees sharing one target"
        " directory overwrite each other's output for anything a given build"
        " did not itself rebuild.",
        "The wasted hour is the phantom failure. The reason this refuses is"
        " the false *green*: it claims these gates passed on code that was"
        " never compiled.",
        f"Unset it and run again. Every worktree has its own target/ by"
        f" default, which is the arrangement that works.",
    ]


def low_disk(free: int, floor_gb: int = FLOOR_GB) -> list[str] | None:
    """Why the disk is worth mentioning, or `None` if it is not.

    A warning and never a refusal: how full somebody's disk is, is their
    business, and a machine at 9 GB will finish most builds.
    """
    if free >= floor_gb * GB:
        return None

    return [
        f"{free / GB:.1f} GB free, which is below {floor_gb} GB.",
        "Running out mid-build does not say `no space left on device`. It"
        " says a linker error, or `something went wrong on this node`, or"
        " four unrelated integration tests failing at once (#61).",
        "`just reap` removes the target/ of every worktree whose branch has"
        " already merged.",
    ]


def main(argv: list[str]) -> int:
    quiet = "--quiet" in argv[1:]

    refusal = shared_target(dict(os.environ))
    if refusal:
        print(f"workspace: REFUSED — {refusal[0]}", file=sys.stderr)
        for line in refusal[1:]:
            print(f"  {line}", file=sys.stderr)
        return 1

    warning = low_disk(shutil.disk_usage(os.getcwd()).free)
    if warning:
        print(f"workspace: warning — {warning[0]}", file=sys.stderr)
        for line in warning[1:]:
            print(f"  {line}", file=sys.stderr)
    elif not quiet:
        print("workspace: ok — one target/ per worktree, room to build in")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
