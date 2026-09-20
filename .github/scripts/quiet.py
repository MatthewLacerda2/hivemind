#!/usr/bin/env python3
"""`quiet.py` — run a long command and report only what was asked of it.

A session that took M7 and six bugs spent **259 thousand tokens**, a quarter of
its context, on command output (#71). Almost none of it was information:
`just ci` printing a thirty-line coverage table, `cargo nextest` naming 450
tests that passed, `cargo clippy` repeating one warning in four files. The
question behind every one of those runs was the same, and it had three possible
answers — it passed, it did not, or here is the first thing that broke.

So this wrapper runs a command, and on success says one line. On failure it says
**everything**, because that is the run where the output is the point.

Two properties it must not lose, both of them the reason this file exists rather
than a `| tail -5` somebody remembers half the time:

**The exit status is untouched.** A failure fails, with the same code it would
have had unwrapped. A wrapper that swallowed a failure would be the false green
`CLAUDE.md` calls the worst failure mode in this repo.

**Absent and passing are different states, and only a completed process earns a
tick.** The verdict comes from the exit status of a process this file spawned
and reaped — never from the output, and never from the *absence* of output. The
precedent is the "wait for CI" loop that counted unfinished checks and read zero
of them as green. A summariser that grepped for `test result: ok` would make the
same mistake the first time a command died before printing anything. Three
states it therefore reports separately:

- a process that could not be spawned, or died on a signal, has **failed** —
  there is no output to grep and nothing was verified;
- a command exiting `SKIPPED` (79) announces that it *declined* to check
  anything, usually because an optional tool is absent. That prints as `skipped`
  and never as `ok`, because "the tool is missing" and "the check passed" are
  the two states this docstring is about. The wrapper exits 0 for it, so a gate
  that was always allowed to skip still passes CI;
- exit 0 is `ok`, whatever it printed or did not print.

Three ways in:

    quiet.py --label lint -- cargo clippy --workspace -- -D warnings
    quiet.py --gates fmt-check lint test        # each one as `just <gate>`
    quiet.py --verbose --label lint -- …        # stream the lot, summarise too

`--gates` runs recipes in order and stops at the first failure, naming what did
not run. An empty gate list is refused rather than reported as a clean sweep,
for exactly the reason above: nothing ran, so nothing passed.

Nesting is handled: inside a wrapper already reporting on us, a second wrapper
passes the command through untouched rather than printing a second line. That is
what lets `just ci` wrap `just lint` while `just lint` on its own is still
quiet.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field

# A command exiting with this says "I checked nothing, and here is why". It is
# not a failure and it is not a pass. 79 is EX_CONFIG from sysexits(3): the
# nearest thing to "this machine is not set up to run me".
SKIPPED = 79

# Set for children, so a wrapper inside a wrapper keeps quiet and lets the outer
# one do the reporting.
WRAPPED = "HIVEMIND_GATE_WRAPPED"

# The escape hatch, as an environment variable so it reaches nested wrappers.
VERBOSE = "HIVEMIND_GATE_VERBOSE"

OK, SKIP, FAIL = "ok", "skipped", "failed"

# Wide enough for the longest recipe name in the justfile, so the lines align.
_LABEL_WIDTH = 14


@dataclass(frozen=True)
class Outcome:
    """What happened, and what the wrapper should print and exit with."""

    label: str
    state: str
    code: int
    seconds: float
    lines: list[str] = field(default_factory=list)

    @property
    def status(self) -> int:
        """The wrapper's own exit status.

        A skip is a pass for whoever called us — the gate declined, which it was
        entitled to do. Everything else is reported as it happened.
        """
        return 0 if self.state in (OK, SKIP) else self.code


def elapsed(seconds: float) -> str:
    """A duration in whichever unit reads as a number rather than as `0.0`."""
    if seconds < 90:
        return f"{seconds:.0f}s"
    return f"{seconds / 60:.1f}m"


def _why_signal(code: int) -> str:
    try:
        return signal.Signals(-code).name
    except ValueError:  # pragma: no cover - a signal number Python cannot name
        return f"signal {-code}"


def verdict(
    label: str,
    returncode: int,
    seconds: float,
    output: str,
    tail: int = 0,
) -> Outcome:
    """Judge a finished command, by its exit status and nothing else.

    `output` decides what gets *printed*, never whether it passed. `tail` asks
    for the last few non-blank lines on success — `cargo nextest`'s last line
    counts the tests that ran, which is the difference between "the suite
    passed" and "the suite was empty", and that difference is this file's whole
    subject.
    """
    printable = [line for line in output.splitlines() if line.strip()]
    stamp = elapsed(seconds)

    if returncode == 0:
        head = f"{OK:<8}{label:<{_LABEL_WIDTH}}{stamp}"
        if tail <= 0:
            return Outcome(label, OK, 0, seconds, [head])
        kept = printable[-tail:] if printable else ["(no output)"]
        return Outcome(label, OK, 0, seconds, [f"{head}  {' · '.join(kept)}"])

    if returncode == SKIPPED:
        reason = printable[0] if printable else "no reason given"
        return Outcome(
            label,
            SKIP,
            returncode,
            seconds,
            [f"{'skip':<8}{label:<{_LABEL_WIDTH}}{stamp}  {reason}", *printable[1:]],
        )

    if returncode < 0:
        why = f"killed by {_why_signal(returncode)}"
        code = 128 - returncode
    else:
        why = f"exit {returncode}"
        code = returncode

    body = printable or ["(the command produced no output at all)"]
    return Outcome(
        label,
        FAIL,
        code,
        seconds,
        [f"{'FAILED':<8}{label:<{_LABEL_WIDTH}}{stamp}  {why}", "", *body],
    )


def _emit(lines: list[str]) -> None:
    for line in lines:
        print(line, flush=True)


def run(label: str, command: list[str], tail: int = 0, verbose: bool = False) -> Outcome:
    """Run `command`, capturing its output unless `verbose`."""
    env = dict(os.environ)
    env[WRAPPED] = label
    if verbose:
        env[VERBOSE] = "1"

    started = time.monotonic()
    try:
        if verbose:
            # Streamed rather than captured: whoever asked for everything wants
            # it as it happens. The status is still ours to report.
            code = subprocess.call(command, env=env)
            output = ""
        else:
            done = subprocess.run(
                command,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                check=False,
            )
            code, output = done.returncode, done.stdout
    except (FileNotFoundError, PermissionError) as exc:
        # No output to summarise, because nothing ran. 127 is what a shell says.
        return verdict(label, 127, time.monotonic() - started, f"{command[0]}: {exc}")

    return verdict(label, code, time.monotonic() - started, output, tail=tail)


def gate_command(gate: str) -> list[str]:
    """How a gate names itself to `just`."""
    return ["just", gate]


def run_gates(gates: list[str], verbose: bool = False, runner=run) -> int:
    """Run each gate in order, one line each, stopping at the first failure.

    An empty list exits non-zero. A summary over no gates that read "all green"
    is the mistake `CLAUDE.md` records against the old wait-for-CI loop, and it
    is cheaper to refuse it here than to recognise it later.
    """
    if not gates:
        print("ci: no gates to run, which is not the same as passing", file=sys.stderr)
        return 2

    started = time.monotonic()
    tally = {OK: 0, SKIP: 0}
    for index, gate in enumerate(gates, start=1):
        outcome = runner(gate, gate_command(gate), verbose=verbose)
        _emit(outcome.lines)
        if outcome.state == FAIL:
            unrun = len(gates) - index
            print(
                f"\nci: FAILED at {gate} ({index} of {len(gates)});"
                f" {unrun} not run",
                flush=True,
            )
            return outcome.status
        tally[outcome.state] += 1

    took = elapsed(time.monotonic() - started)
    skipped = f", {tally[SKIP]} skipped" if tally[SKIP] else ""
    print(
        f"\nci: {tally[OK]} of {len(gates)} gates ok{skipped} in {took}",
        flush=True,
    )
    return 0


@dataclass
class Invocation:
    """A parsed command line."""

    verbose: bool = False
    label: str = ""
    tail: int = 0
    gates: list[str] = field(default_factory=list)
    command: list[str] = field(default_factory=list)


def parse(argv: list[str], env: dict[str, str] | None = None) -> Invocation:
    """Parse arguments by hand, because the command may contain its own `--`.

    `cargo clippy … -- -D warnings` is the case: only the first `--` separates
    this wrapper's options from the command, and the rest belongs to the
    command.
    """
    env = {} if env is None else env
    left, command = argv, []
    if "--" in argv:
        cut = argv.index("--")
        left, command = argv[:cut], argv[cut + 1 :]

    call = Invocation(command=command)
    call.verbose = bool(env.get(VERBOSE, "").strip())

    rest = list(left)
    while rest:
        arg = rest.pop(0)
        if arg in ("-v", "--verbose"):
            call.verbose = True
        elif arg == "--label":
            call.label = rest.pop(0) if rest else ""
        elif arg == "--tail":
            call.tail = int(rest.pop(0)) if rest else 0
        elif arg == "--gates":
            while rest and not rest[0].startswith("-"):
                call.gates.append(rest.pop(0))
        else:
            raise ValueError(f"quiet.py: unknown argument {arg}")

    return call


def main(argv: list[str], env: dict[str, str] | None = None) -> int:
    """`env` is the environment the *decisions* are read from.

    Passed in rather than read from `os.environ` at the point of use, because
    the first version read it directly and its own tests then behaved
    differently depending on whether they were themselves running inside a
    wrapper — `just scripts` passed alone and failed under `just ci`. An
    ambient input that changes an outcome is an input, and belongs in the
    signature.
    """
    env = dict(os.environ) if env is None else env
    try:
        call = parse(argv[1:], env)
    except (ValueError, IndexError) as exc:
        print(f"{exc}", file=sys.stderr)
        print(
            "usage: quiet.py [--verbose] --label NAME [--tail N] -- COMMAND…\n"
            "       quiet.py [--verbose] --gates GATE…",
            file=sys.stderr,
        )
        return 2

    if call.gates:
        return run_gates(call.gates, verbose=call.verbose)

    if not call.command:
        print("quiet.py: nothing to run", file=sys.stderr)
        return 2

    if env.get(WRAPPED):
        # Something outside is already reporting on this command. Adding a
        # second line about it would be noise, which is the thing being fixed.
        code = subprocess.call(call.command)
        return code

    outcome = run(call.label or call.command[0], call.command, call.tail, call.verbose)
    _emit(outcome.lines)
    return outcome.status


if __name__ == "__main__":
    sys.exit(main(sys.argv))
