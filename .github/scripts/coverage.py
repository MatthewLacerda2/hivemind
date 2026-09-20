#!/usr/bin/env python3
"""`coverage.py` — the coverage number and the verdict, not the table.

`just cov-gate` used to end with a thirty-line per-file table, in a session
where the question was always "is it still above the floor" (#71). The table is
worth reading when coverage has dropped and you are looking for what is
untested — which is `just cov`, opt-in, and still prints all of it.

So this prints one line: the percentage, the floor, and which side of it we are
on. It reads llvm-cov's own JSON summary rather than scraping the human table,
because the table's columns move between versions and its `TOTAL` row means
nothing without its header.

**Output that cannot be parsed is a failure, never a pass.** If the export ran
in an empty tree, or printed a diagnostic instead of JSON, there is no number —
and "no number" must not read as "above the floor". That is the same confusion
`CLAUDE.md` records about a check list that was empty rather than green.

    coverage.py --floor 85 -- cargo llvm-cov report --json --summary-only …
"""

from __future__ import annotations

import json
import subprocess
import sys
from dataclasses import dataclass


@dataclass(frozen=True)
class Lines:
    """The line coverage totals, as llvm-cov counts them."""

    count: int
    covered: int
    percent: float


def line_totals(text: str) -> Lines | None:
    """The line totals in an llvm-cov JSON export, or `None` if there are none.

    `None` covers every way of not having a number: empty output, a diagnostic,
    JSON of another shape, a run with no files in it. The caller treats all of
    them as a failure, because none of them is evidence of coverage.
    """
    # `raw_decode` from the first brace rather than `loads` over the whole
    # thing: cargo puts the occasional warning on the same stream, and a
    # tolerable prefix should not turn a real measurement into "no number".
    start = text.find("{") if isinstance(text, str) else -1
    if start < 0:
        return None
    try:
        report, _ = json.JSONDecoder().raw_decode(text[start:])
    except ValueError:
        return None

    if not isinstance(report, dict):
        return None
    data = report.get("data") or []
    if not isinstance(data, list) or not data:
        return None
    totals = data[0].get("totals") if isinstance(data[0], dict) else None
    lines = totals.get("lines") if isinstance(totals, dict) else None
    if not isinstance(lines, dict):
        return None
    try:
        count = int(lines["count"])
        covered = int(lines["covered"])
        percent = float(lines["percent"])
    except (KeyError, TypeError, ValueError):
        return None

    # A report over nothing has a percentage of zero and means nothing by it.
    if count <= 0:
        return None

    return Lines(count, covered, percent)


def verdict(lines: Lines | None, floor: float) -> tuple[bool, str]:
    """Whether the floor holds, and the one line that says so."""
    if lines is None:
        return False, (
            f"{'FAILED':<8}{'coverage':<14}no line totals in the report —"
            " nothing was measured, which is not the same as passing"
        )

    body = (
        f"lines {lines.percent:.2f}%"
        f" ({lines.covered} of {lines.count}), floor {floor:g}%"
    )
    if lines.percent + 1e-9 < floor:
        return False, f"{'FAILED':<8}{'coverage':<14}{body}"
    return True, f"{'ok':<8}{'coverage':<14}{body}"


def parse(argv: list[str]) -> tuple[float, list[str]]:
    """The floor and the command that produces the report."""
    floor = 0.0
    command: list[str] = []
    rest = list(argv)
    while rest:
        arg = rest.pop(0)
        if arg == "--floor":
            floor = float(rest.pop(0))
        elif arg == "--":
            command = rest
            break
        else:
            raise ValueError(f"coverage.py: unknown argument {arg}")
    return floor, command


def main(argv: list[str]) -> int:
    try:
        floor, command = parse(argv[1:])
    except (ValueError, IndexError) as exc:
        print(f"{exc}", file=sys.stderr)
        print(
            "usage: coverage.py --floor 85 -- cargo llvm-cov report --json"
            " --summary-only",
            file=sys.stderr,
        )
        return 2
    if not command:
        print("coverage.py: no report command given", file=sys.stderr)
        return 2

    try:
        done = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
            check=False,
        )
    except (FileNotFoundError, PermissionError) as exc:
        # A report that could not be asked for is not a report.
        print(verdict(None, floor)[1], flush=True)
        print(f"  {command[0]}: {exc}", file=sys.stderr)
        return 1

    # Both halves are needed: a process that failed has no verdict to give, and
    # neither has one that exited 0 without printing a number.
    measured = line_totals(done.stdout) if done.returncode == 0 else None
    ok, line = verdict(measured, floor)
    print(line, flush=True)
    if not ok:
        # The command's own diagnosis of why there is no number.
        sys.stderr.write(done.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
