#!/usr/bin/env python3
"""`boundaries.py` — hold the repo to the layering SPEC §3.1 and §10 describe.

A rule written only in prose is one that is true until somebody in a hurry
writes the obvious line. Every rule here is already obeyed; the point is that it
stays obeyed without anybody remembering it.

The granularity is the file, deliberately. Function-level rules need a parser
and a marker comment somebody has to remember to write — which is the problem
again, one level down. "SQL lives in this file" is the same shape of rule as
"queries live in this folder", and it is checkable by reading.

Python and not Rust because this is a scan over text that needs no domain type,
and a Rust tool would either join the workspace and cost a compile on every
build or become a second workspace to keep in step. `.github/scripts/` already
runs under `just scripts`.

    just boundaries
"""

from __future__ import annotations

import pathlib
import re
import sys
from dataclasses import dataclass

ROOT = pathlib.Path(__file__).resolve().parents[2]


@dataclass(frozen=True)
class Rule:
    """One boundary, what breaks it, and where it is allowed to live."""

    name: str
    # What a breach looks like in source text.
    pattern: str
    # Paths, relative to the repo root, that may contain it. A directory
    # entry permits everything under it.
    allowed: tuple[str, ...]
    # Why the rule exists. Printed on a breach, because somebody who just hit
    # it is the person who most needs the argument.
    why: str


RULES: tuple[Rule, ...] = (
    Rule(
        name="SQL lives in the index",
        pattern=r"\brusqlite\b|\bSELECT\s+\w|\bINSERT\s+INTO\b|\bCREATE\s+TABLE\b",
        allowed=("crates/hivemind-core/src/index.rs",),
        why=(
            "index.db is a derived cache (ADR 0002). Querying it from anywhere"
            " else makes it look like a second source of truth, and the next"
            " person to write a query there will not know to make it survive a"
            " rebuild."
        ),
    ),
    Rule(
        name="the core crate does no network I/O",
        # The crate name followed by `::`, which is how it is reached in Rust
        # source. An earlier version anchored this at the start of a line, as
        # if it were scanning Cargo.toml — so it matched `tokio = ...` and
        # never `use tokio::fs`, which is to say it never fired at all. The
        # test that writes a breaching file is what found that.
        #
        # `rustls_pki_types` is deliberately not caught: it is types with no
        # I/O, and hivemind-core uses it to hold a certificate.
        pattern=r"\b(axum|hyper|hyper_util|reqwest|rustls|tokio)::",
        allowed=(),
        why=(
            "SPEC §3.1: hivemind-core is the boring one — filesystem access is"
            " the only side effect it is allowed. Everything depends on it, so"
            " keeping it synchronous and dependency-light is what keeps the"
            " rest testable."
        ),
    ),
    Rule(
        name="the CLI reaches the store only where SPEC §10 allows",
        pattern=r"\bMailStore\b|\bIndex::open\b",
        allowed=(
            # `reindex`.
            "crates/hivemind-cli/src/commands.rs",
            # `hook check`, which moved out of `commands.rs` when sessions
            # gave it a second job (#52) and the file neared the size gate.
            # Same exception, same reason; it is the file that changed.
            "crates/hivemind-cli/src/hooks/check.rs",
        ),
        why=(
            "SPEC §10: the CLI talks to the daemon over 127.0.0.1 and never"
            " touches mail/ directly, except `hook check` and `reindex`. One"
            " implementation of every operation, and a CLI bug that cannot"
            " corrupt the store."
        ),
    ),
)

# Which crates each rule is asked about. A rule naming only files inside one
# crate would otherwise scan the whole tree to say nothing.
SCOPE: dict[str, tuple[str, ...]] = {
    "SQL lives in the index": ("crates",),
    "the core crate does no network I/O": ("crates/hivemind-core/src",),
    "the CLI reaches the store only where SPEC §10 allows": ("crates/hivemind-cli/src",),
}


def rust_files(scope: tuple[str, ...]) -> list[pathlib.Path]:
    """Every `.rs` file under `scope`, sorted so output is stable."""
    found: list[pathlib.Path] = []
    for directory in scope:
        found.extend(sorted((ROOT / directory).rglob("*.rs")))
    return found


def permitted(path: pathlib.Path, allowed: tuple[str, ...]) -> bool:
    relative = path.relative_to(ROOT).as_posix()
    return any(relative == entry or relative.startswith(f"{entry}/") for entry in allowed)


def breaches(rule: Rule) -> list[str]:
    """Every `file:line` that breaks `rule`, with the offending line."""
    expression = re.compile(rule.pattern, re.MULTILINE)
    found: list[str] = []

    for path in rust_files(SCOPE[rule.name]):
        if permitted(path, rule.allowed):
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as error:  # pragma: no cover - unreadable file
            found.append(f"{path}: cannot read: {error}")
            continue

        for number, line in enumerate(text.splitlines(), start=1):
            # A rule is about code, not about the comment explaining the rule.
            stripped = line.lstrip()
            if stripped.startswith("//") or stripped.startswith("/*"):
                continue
            if expression.search(line):
                relative = path.relative_to(ROOT).as_posix()
                found.append(f"{relative}:{number}: {stripped}")
    return found


def main() -> int:
    broken = 0
    for rule in RULES:
        found = breaches(rule)
        if not found:
            print(f"boundaries: ok — {rule.name}")
            continue

        broken += 1
        print(f"\nboundaries: BROKEN — {rule.name}", file=sys.stderr)
        print(f"  {rule.why}", file=sys.stderr)
        if rule.allowed:
            print(f"  Allowed in: {', '.join(rule.allowed)}", file=sys.stderr)
        for line in found:
            print(f"    {line}", file=sys.stderr)

    if broken:
        print(
            f"\n{broken} boundary/boundaries broken. Move the code, or change"
            " the rule in this file and say why in the commit.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
