#!/usr/bin/env python3
"""`size.py` — fail if a Rust file carries more code than its limit.

**A ratchet, not a target.** The limits start just above the largest file that
exists, so nothing here has to be split today and nothing can get worse
tomorrow. They come down as files are split, and the number in this file is the
record of how far that has got.

Blank lines and comments are free. `missing_docs` is a merge gate here and the
house style is to explain *why*, so a cap that counted prose would put those
two rules in opposition and split files whose code was never the problem.

Source and test code are counted separately, and the split is by `#[cfg(test)]`
rather than by path — most tests in this repo live in the file they test, and a
path-based rule would call a 300-line module with a 600-line test module a
900-line source file.

    just size
"""

from __future__ import annotations

import pathlib
import re
import sys
from dataclasses import dataclass

ROOT = pathlib.Path(__file__).resolve().parents[2]

# Where the ratchet stands. Lower these as files are split; never raise them.
#
# Measured, not chosen. They landed at 800/600 against an 816-source
# `commands.rs` and a 609-test `peer.rs`, just under both, so two small trims
# paid for the gate rather than a refactor — a gate that passes on the day it
# arrives is a gate nobody knows works.
#
# These come down with #100, which split `service.rs` from 791 source into a
# `service/` folder of which the largest part is 182. The largest left are 651
# source (`hivemind-api/src/local.rs`) and 558 test
# (`hivemind-cli/tests/single_daemon.rs`), and the numbers sit just above each:
# the split that lowers a limit is not also the branch that has to split the
# next file down the list.
SOURCE_LIMIT = 660
TEST_LIMIT = 560

SEARCHED = ("crates",)

# `#[cfg(test)]` on its own line, with or without an attribute above it.
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")


@dataclass
class Counted:
    source: int
    test: int


def count(text: str, is_test_file: bool) -> Counted:
    """Lines of code in `text`, split by whether they are test code.

    A line is code when it is not blank and not wholly a comment. Trailing
    comments after code still count as code, which is right: the code is there.
    """
    source = test = 0
    in_block_comment = False
    # Depth of the `#[cfg(test)]` module currently open, or None outside one.
    test_depth: int | None = None
    depth = 0
    pending_cfg_test = False

    for line in text.splitlines():
        stripped = line.strip()

        # Block comments first: everything inside is prose.
        if in_block_comment:
            if "*/" in stripped:
                in_block_comment = False
            continue
        if stripped.startswith("/*"):
            if "*/" not in stripped:
                in_block_comment = True
            continue

        counts = bool(stripped) and not stripped.startswith("//")

        if CFG_TEST.match(line):
            pending_cfg_test = True

        opened = stripped.count("{") - stripped.count("}")
        if pending_cfg_test and "{" in stripped:
            test_depth = depth
            pending_cfg_test = False

        if counts:
            # `pending_cfg_test` is still set on the attribute line itself and
            # on anything between it and the module's brace. Those are test
            # code: counting them as source was an off-by-one the counter's own
            # tests found.
            if is_test_file or test_depth is not None or pending_cfg_test:
                test += 1
            else:
                source += 1

        depth += opened
        if test_depth is not None and depth <= test_depth:
            test_depth = None

    return Counted(source, test)


def files() -> list[pathlib.Path]:
    found: list[pathlib.Path] = []
    for entry in SEARCHED:
        found.extend(sorted((ROOT / entry).rglob("*.rs")))
    return found


def measure() -> list[tuple[str, Counted]]:
    out: list[tuple[str, Counted]] = []
    for path in files():
        relative = path.relative_to(ROOT).as_posix()
        # `crates/*/tests/*.rs` is an integration test: all of it is test code.
        is_test_file = "/tests/" in relative
        try:
            out.append((relative, count(path.read_text(encoding="utf-8"), is_test_file)))
        except OSError:
            continue
    return out


def main(argv: list[str]) -> int:
    measured = measure()

    if "--report" in argv:
        # Not a gate: what to split next, largest first.
        for name, counted in sorted(
            measured, key=lambda row: row[1].source + row[1].test, reverse=True
        )[:15]:
            print(f"{counted.source:5} source {counted.test:5} test  {name}")
        return 0

    over = [
        (name, counted)
        for name, counted in measured
        if counted.source > SOURCE_LIMIT or counted.test > TEST_LIMIT
    ]

    if not over:
        biggest_source = max((c.source for _, c in measured), default=0)
        biggest_test = max((c.test for _, c in measured), default=0)
        print(
            f"size: {len(measured)} files, all within {SOURCE_LIMIT} lines of"
            f" source / {TEST_LIMIT} of test. Largest: {biggest_source} source,"
            f" {biggest_test} test."
        )
        return 0

    print(f"\nsize: {len(over)} file(s) over the limit.", file=sys.stderr)
    for name, counted in over:
        which = []
        if counted.source > SOURCE_LIMIT:
            which.append(f"{counted.source} source > {SOURCE_LIMIT}")
        if counted.test > TEST_LIMIT:
            which.append(f"{counted.test} test > {TEST_LIMIT}")
        print(f"    {name}: {', '.join(which)}", file=sys.stderr)
    print(
        "\nSplit it by concern — group into a subfolder rather than adding a"
        " filename prefix. The limits are a ratchet: they come down as files"
        " are split and are never raised.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
