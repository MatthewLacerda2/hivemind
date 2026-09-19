#!/usr/bin/env python3
"""`mergeable.py N` — say whether pull request N has really been checked.

Exits 0 when CI genuinely ran on the head commit and passed, non-zero with a
reason otherwise. It is the last thing to run before `gh pr merge`, and it
exists because "the checks look green" and "the checks ran" are not the same
claim.

The idea and the failure modes come from `scorsese`'s script of the same name,
which found them in practice. What follows is why this repo needs it too.

**A loop that waits for checks to finish treats absent as settled.** The one
used to merge M3 through M6 here was:

    until [ "$(gh pr view N --json statusCheckRollup \\
        --jq '[.statusCheckRollup[] | select(.status != "COMPLETED")] | length')" = "0" ]
    do sleep 20; done

With zero checks recorded, `length` is `0`, the loop exits on its first
iteration, and the conclusions printed afterwards are an empty list. Nothing
distinguishes that from a green run. Four merges went through it; all four
happened to have checks, which is luck rather than a property.

**One commit can carry several runs.** A push followed by an edit to the pull
request can produce two runs seconds apart. Picking one by list position means
the verdict depends on an ordering the API never promised — and nothing about
"first in the list" prefers a green run over a red one.

So the rule is stated over the whole set: **no run for this commit failed, and
at least one succeeded having actually run its jobs.** A skipped run beside a
real one then decides nothing, and a failing run beside a green one refuses.

**A commit can have no run because the branch conflicts with `main`.** GitHub
cannot compute a merge commit for a conflicted branch, so it never evaluates
the workflow's triggers and creates no run, no check and no error. The symptom
is identical to a lost run, and the instinct is to go and read the workflow
YAML. The tell: an actually-invalid workflow *does* produce a run, a
`push`-event `startup_failure`. No run whatsoever means a conflict.

Run it against a live pull request:

    just mergeable 12
    just mergeable 12 --wait

**`--wait` polls until the answer settles**, which is not the same as polling
until it is green. A verdict and a forecast are different things, and every
hand-rolled loop this flag replaces collapsed them: an `until` over a single
non-zero code waits out a red pull request forever. [`unsettled`] is the
forecast, [`judge`] is the verdict, and they are separate functions on purpose.

Exit codes say which is which — `0` mergeable, `1` no, `3` not yet, `2` a
usage mistake — so a caller can tell "still building" from "it failed" without
reading the prose.

[`judge`] is the whole decision. It is pure and takes plain dictionaries, so
the tests beside this file need no network.
"""

from __future__ import annotations

import json
import subprocess
import sys
import time

# The workflow that gates a merge. `Release` runs on tags and is not the one
# being asked about; counting it would be the same mistake in a new costume.
WORKFLOW = "CI"

# What `--wait` does when nothing has settled. Twenty seconds because the
# thing being waited on takes minutes and the API has a rate limit; a poll a
# second would spend it for no sooner an answer.
POLL = 20

# How long `--wait` waits before giving up, in seconds. CI here takes a few
# minutes; half an hour is long enough that hitting it means something is
# wrong rather than slow, and short enough that a forgotten terminal does not
# poll all night.
TIMEOUT = 30 * 60

# Exit codes. `1` and `3` are both "not mergeable", and they are separate
# because a caller that cannot tell them apart is the bug this script was
# written for: a naive `until` loop over a single code spins forever on a red
# pull request, and gives up on a slow one.
OK = 0
NO = 1
USAGE = 2
PENDING = 3

# What GitHub calls a branch it cannot merge, in the two fields that say so.
# Both are read, because either alone can lag the other by a poll.
CONFLICTED = ("CONFLICTING", "DIRTY")

# Not "no": *not computed yet*. GitHub works this out in the background, so a
# fresh push reads UNKNOWN for a moment. Treated as its own answer rather than
# folded into either — asserting "not conflicted" from a field that means "ask
# again" is the confident wrong answer this script exists to refuse.
UNKNOWN = "UNKNOWN"


def conflicted(pull: dict) -> bool:
    """Does GitHub say this branch cannot be merged into `main`?

    Its own function because two questions need it and they must not drift:
    what to *say* when there is no run, and whether waiting for one could
    ever help. It cannot — GitHub never evaluates the workflow's triggers for
    a conflicted branch, so no amount of patience produces a run.
    """
    return (
        pull.get("mergeable") in CONFLICTED
        or pull.get("mergeStateStatus") in CONFLICTED
    )


def unsettled(pull: dict, runs: list[dict]) -> bool:
    """Could asking again give a different answer?

    The question `judge` deliberately does not answer, because a verdict and
    a forecast are different things — and collapsing them is what makes a
    naive `until` loop spin forever on a red pull request.

    Pure, and the whole of the waiting decision:

    - A **draft** is settled. CI does not run on drafts, so waiting is
      waiting for something nobody has asked for.
    - A **conflicted** branch is settled. There will never be a run.
    - **No runs yet**, on a branch that is neither, is unsettled: a run can
      appear moments after a push, and that gap is most of why anybody waits.
    - **A run still going** is unsettled, which is the ordinary case.
    - Everything else — failed, all-skipped, green — is settled. A red run
      does not go green by being asked twice.
    """
    if pull.get("isDraft") or conflicted(pull):
        return False
    if not runs:
        return True
    return bool(unfinished(runs))


def failed_runs(runs: list[dict]) -> list[dict]:
    """The runs in this set that concluded badly.

    `skipped` is not a failure — it is what CI does to a change it is
    configured to ignore — so the predicate is "completed, and not one of the
    two conclusions that mean nothing went wrong".
    """
    return [
        run
        for run in runs
        if run.get("status") == "completed"
        and run.get("conclusion") not in ("success", "skipped")
    ]


def unfinished(runs: list[dict]) -> list[dict]:
    """The runs in this set that have not concluded."""
    return [run for run in runs if run.get("status") != "completed"]


def ran_something(run: dict, jobs: dict[int, list[dict]]) -> bool:
    """Whether any job in this run actually concluded successfully.

    The question `conclusion == "success"` does not answer: a run whose every
    job was skipped still concludes successfully, and that is a run that
    compiled nothing.
    """
    return any(
        job.get("conclusion") == "success" for job in jobs.get(run.get("id"), [])
    )


def no_run(pull: dict, short: str) -> list[str]:
    """Why this commit has no run at all — and there is more than one why."""
    state, status = pull.get("mergeable"), pull.get("mergeStateStatus")
    head = f"no {WORKFLOW} run exists for the head commit {short}."

    if conflicted(pull):
        return [
            f"{head} The branch conflicts with `main`.",
            "That is the cause, not a coincidence: GitHub cannot compute a"
            " merge commit for a conflicted branch, so it never evaluates the"
            " workflow's triggers and creates no run, no check and no error."
            " The workflow file is fine — do not go and edit it.",
            "Rebase onto `main` and force-push.",
        ]

    lines = [
        head,
        "The checks are not green, they are absent. A run can go missing when a"
        " pull request changes state moments after a push.",
        "Force one with an empty commit, or push again.",
        "Before editing any workflow YAML: an invalid one still produces a run,"
        " a `push`-event `startup_failure`. No run whatsoever means a conflict"
        " with `main`, not a syntax error.",
    ]
    if state == UNKNOWN:
        lines.append(
            "GitHub has not finished computing whether this branch merges"
            " cleanly. Ask again in a moment before believing the rest."
        )
    elif status and status != "CLEAN":
        lines.append(f"GitHub reports this branch as {status}.")
    return lines


def judge(
    pull: dict, runs: list[dict], jobs: dict[int, list[dict]]
) -> tuple[bool, list[str]]:
    """Whether this pull request may be merged, and why not when it may not.

    Pure, and the whole of the decision. `pull` is `gh pr view --json`, `runs`
    is every [`WORKFLOW`] run recorded against the head commit, and `jobs` maps
    a run's id to its jobs.
    """
    short = pull.get("headRefOid", "")[:7]

    if pull.get("isDraft"):
        return False, [
            "the pull request is a draft.",
            "CI does not run on drafts, so nothing here has checked it. A draft"
            " makes no claim to pass; mark it ready and let CI answer.",
        ]

    if not runs:
        return False, no_run(pull, short)

    # A failure anywhere in the set refuses, and it is asked first. A commit
    # whose evidence disagrees with itself has one safe reading, and a green
    # run beside a red one does not make the red one go away.
    failed = failed_runs(runs)
    if failed:
        run = failed[0]
        return False, [
            f"a {WORKFLOW} run for {short} concluded {run.get('conclusion')}.",
            f"See {run.get('html_url', 'the run')}.",
        ]

    still_going = unfinished(runs)
    if still_going:
        run = still_going[0]
        return False, [
            f"a {WORKFLOW} run for {short} is still {run.get('status')}.",
            f"See {run.get('html_url', 'the run')}.",
        ]

    # Everything completed without failing. Now: did any of it build anything?
    if not any(ran_something(run, jobs) for run in runs):
        return False, [
            f"every {WORKFLOW} job for {short} was skipped.",
            "The run concluded successfully having compiled nothing. That is"
            " an absent check wearing the costume of a passing one.",
            "Push a commit that CI does not skip, or check the workflow's path"
            " filters against what this branch changed.",
        ]

    return True, [f"CI ran on {short} and passed."]


def look(number: str) -> tuple[dict, list[dict], dict[int, list[dict]]]:
    """Everything `judge` needs about pull request `number`, from GitHub."""
    pull = gh(
        "pr",
        "view",
        number,
        "--json",
        "headRefOid,isDraft,mergeable,mergeStateStatus,url",
    )
    sha = pull.get("headRefOid", "")

    repo = gh("repo", "view", "--json", "nameWithOwner")["nameWithOwner"]
    listed = gh(
        "api",
        f"repos/{repo}/actions/runs?head_sha={sha}&per_page=100",
        "--jq",
        ".workflow_runs",
    )
    runs = [run for run in listed if run.get("name") == WORKFLOW]

    jobs: dict[int, list[dict]] = {}
    for run in runs:
        jobs[run["id"]] = gh(
            "api", f"repos/{repo}/actions/runs/{run['id']}/jobs", "--jq", ".jobs"
        )
    return pull, runs, jobs


def report(ok: bool, why: list[str]) -> None:
    """Print the verdict the way a human reads it."""
    lead = "mergeable" if ok else "NOT mergeable"
    print(f"{lead}: {why[0]}")
    for line in why[1:]:
        print(f"  {line}")


def gh(*args: str) -> object:
    """`gh` with `--json`-shaped output, parsed. Fatal if `gh` itself fails."""
    done = subprocess.run(["gh", *args], capture_output=True, text=True, check=False)
    if done.returncode != 0:
        sys.exit(f"mergeable: gh {' '.join(args)}: {done.stderr.strip()}")
    return json.loads(done.stdout)


def main(argv: list[str]) -> int:
    # `just mergeable PR=12` was documented in five places from the first
    # commit and never ran: `PR` is a recipe parameter, so `just` hands the
    # whole string over as its value rather than binding a variable. The
    # generic usage line below leaves a reader looking for a broken script
    # instead of a wrong argument, which is the expensive half (#48).
    if len(argv) == 2 and argv[1].startswith("PR="):
        number = argv[1][len("PR=") :] or "12"
        print(
            f"mergeable: the number goes positionally — `just mergeable {number}`.",
            file=sys.stderr,
        )
        return 2

    rest = [arg for arg in argv[1:] if not arg.startswith("--")]
    flags = [arg for arg in argv[1:] if arg.startswith("--")]

    wait = "--wait" in flags
    timeout = TIMEOUT
    for flag in flags:
        if flag.startswith("--timeout="):
            value = flag[len("--timeout=") :]
            if not value.isdigit():
                print("mergeable: --timeout takes seconds", file=sys.stderr)
                return USAGE
            timeout = int(value)
        elif flag != "--wait":
            print(f"mergeable: unknown option {flag}", file=sys.stderr)
            return USAGE

    if len(rest) != 1 or not rest[0].isdigit():
        print(
            "usage: mergeable.py <pull-request-number> [--wait] [--timeout=SECONDS]",
            file=sys.stderr,
        )
        return USAGE

    number = rest[0]
    deadline = time.monotonic() + timeout
    said_waiting = False

    while True:
        pull, runs, jobs = look(number)
        ok, why = judge(pull, runs, jobs)

        if ok:
            report(True, why)
            return OK

        if not unsettled(pull, runs):
            # Settled and negative. Asking again would print the same thing,
            # so `--wait` stops here rather than sitting on a red run until
            # the timeout — which is the failure mode of every hand-rolled
            # loop this flag replaces.
            report(False, why)
            return NO

        if not wait:
            report(False, why)
            return PENDING

        if time.monotonic() >= deadline:
            report(False, why)
            print(f"  gave up waiting after {timeout}s.")
            return PENDING

        if not said_waiting:
            # Once, not per poll: this goes into a terminal somebody is
            # watching, and a line every twenty seconds is noise.
            print(f"waiting for CI on #{number}: {why[0]}", file=sys.stderr)
            said_waiting = True

        time.sleep(min(POLL, max(0, deadline - time.monotonic())))


if __name__ == "__main__":
    sys.exit(main(sys.argv))
