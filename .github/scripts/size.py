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

That attribute is not always in the file it governs. `#[cfg(test)] mod y;` in
`x.rs` makes the whole of `x/y.rs` test code, and nothing inside `x/y.rs` says
so, which had five such files counting as production code (#110). So the
declaration is read from the parent. The declaration, not the name: `_tests` is
a convention, and `peer/hello.rs` sits beside `peer/hello_tests.rs`.

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
# These came down with #100, which split `service.rs` from 791 source into a
# `service/` folder of which the largest part is 182. The largest left are 656
# source (`hivemind-api/src/local.rs`) and 558 test
# (`hivemind-cli/tests/single_daemon.rs`), and the numbers sit just above each:
# the split that lowers a limit is not also the branch that has to split the
# next file down the list.
#
# #110 moved five child test modules out of the source column, the largest 520
# (`hivemind-api/src/web/page_tests.rs`). Neither number moves for it: the
# largest of each kind is a file it did not reclassify, and a limit is lowered
# by the branch that splits that file, not by one that stops miscounting
# others.
SOURCE_LIMIT = 660
TEST_LIMIT = 560

SEARCHED = ("crates",)

# `#[cfg(test)]` on its own line, with or without an attribute above it.
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")

# `mod y;` — a declaration of a module living in another file, as opposed to an
# inline `mod y { … }`. Any visibility may precede it.
MOD_DECL = re.compile(
    r"^\s*(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)

# Another attribute may sit between `#[cfg(test)]` and what it applies to.
ATTRIBUTE = re.compile(r"^\s*#!?\[")

# These own the directory they sit in; any other file owns a subdirectory named
# after itself. `mod y;` resolves against that.
DIRECTORY_OWNERS = ("lib.rs", "main.rs", "mod.rs")


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

        # `pending_cfg_test` is still set on the attribute line itself and on
        # anything between it and the module's brace. Those are test code:
        # counting them as source was an off-by-one the counter's own tests
        # found.
        is_test_line = is_test_file or test_depth is not None or pending_cfg_test

        if pending_cfg_test:
            if "{" in stripped:
                test_depth = depth
                pending_cfg_test = False
            elif MOD_DECL.match(line):
                # `#[cfg(test)] mod y;` opens no brace, so the attribute is
                # spent here rather than left pending over the rest of the file.
                # `y`'s own file is handled by `declared_test_files`.
                pending_cfg_test = False

        if counts:
            if is_test_line:
                test += 1
            else:
                source += 1

        depth += opened
        if test_depth is not None and depth <= test_depth:
            test_depth = None

    return Counted(source, test)


def read_sources() -> dict[pathlib.Path, str]:
    """Every Rust file under `SEARCHED`, by path, with its text."""
    sources: dict[pathlib.Path, str] = {}
    for entry in SEARCHED:
        for path in sorted((ROOT / entry).rglob("*.rs")):
            try:
                sources[path] = path.read_text(encoding="utf-8")
            except OSError:
                continue
    return sources


def mod_declarations(text: str) -> list[tuple[str, bool]]:
    """Each `mod y;` in `text`, and whether `#[cfg(test)]` applies to it."""
    declared: list[tuple[str, bool]] = []
    pending_cfg_test = False

    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("//"):
            continue

        match = MOD_DECL.match(line)
        if match:
            declared.append((match.group(1), pending_cfg_test))
            pending_cfg_test = False
        elif CFG_TEST.match(line):
            pending_cfg_test = True
        elif not ATTRIBUTE.match(line):
            # Anything else consumes the attribute, so an inline
            # `#[cfg(test)] mod tests { … }` cannot hand it to the next
            # declaration down.
            pending_cfg_test = False

    return declared


def module_files(parent: pathlib.Path, name: str) -> list[pathlib.Path]:
    """Where `mod name;` inside `parent` puts that module's file."""
    folder = (
        parent.parent
        if parent.name in DIRECTORY_OWNERS
        else parent.parent / parent.stem
    )
    return [folder / f"{name}.rs", folder / name / "mod.rs"]


def declared_test_files(sources: dict[pathlib.Path, str]) -> set[pathlib.Path]:
    """Files that are test code because of how their parent declares them.

    Repeated to a fixed point, because a file that is test code in its entirety
    needs no `#[cfg(test)]` on the modules it declares in turn — and so has
    none.
    """
    found: set[pathlib.Path] = set()
    growing = True

    while growing:
        growing = False
        for parent, text in sources.items():
            parent_is_test = parent in found
            for name, cfg_test in mod_declarations(text):
                if not (cfg_test or parent_is_test):
                    continue
                for child in module_files(parent, name):
                    if child in sources and child not in found:
                        found.add(child)
                        growing = True

    return found


def measure() -> list[tuple[str, Counted]]:
    sources = read_sources()
    declared = declared_test_files(sources)
    out: list[tuple[str, Counted]] = []
    for path, text in sources.items():
        relative = path.relative_to(ROOT).as_posix()
        # `crates/*/tests/*.rs` is an integration test: all of it is test code.
        is_test_file = "/tests/" in relative or path in declared
        out.append((relative, count(text, is_test_file)))
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
