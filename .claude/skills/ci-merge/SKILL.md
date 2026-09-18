---
name: ci-merge
description: Take a finished branch through to merged — the gates, proving CI actually ran, the traps that have cost work here, and the mutation signal. Use when a branch is ready, when a pull request has gone red, or when merging anything into `main`.
---

# Getting a branch merged

`CLAUDE.md` carries why these exist; this is how. Every trap below has cost
something in this repo, and each says what.

## Before marking a pull request ready

`just ci` must be green. It runs exactly what GitHub runs, in the same order —
that is what makes a green local run mean anything. `just --list` has the rest.

Not before every push. A checkpoint commit stays cheap; the pre-commit hook is
formatting only.

## The merge

1. `git fetch origin && git rebase origin/main`.
2. Push with `--force-with-lease`.
3. Wait for CI **on the rebased head**.
4. **`just mergeable PR=N`.** Not optional. See below.
5. `gh pr merge N --merge`. Never `--squash`.

### `just mergeable` is the gate, and its answer is final

`gh pr checks` is not a substitute: it blends runs, so a skipped run hides
behind a real one.

**Never hand-roll a "wait for CI" loop.** The one used for M3 through M6 was:

    until [ "$(gh pr view N --json statusCheckRollup --jq
        '[... select(.status != "COMPLETED")] | length')" = "0" ]

With zero checks recorded that is `0`. The loop exits on its first iteration
and the conclusions printed after it are an empty list, which reads exactly
like green. **Absent and passing are different states, and a loop counting
unfinished checks finds zero of each.** Four merges went through it and all
four happened to have checks, which is luck.

Three shapes it catches:

- **A run that concluded `success` having skipped every job.** An absent check
  wearing the costume of a passing one.
- **No run at all**, because the pull request changed state moments after a
  push. Force one with an empty commit.
- **No run at all, because the branch conflicts with `main`.** GitHub cannot
  build a merge ref for a conflicted branch, so it creates nothing — no run, no
  check, no error. This reads exactly like a broken workflow file.

Telling the last two apart: **an invalid workflow still produces a run**, a
`push`-event `startup_failure`. *No run whatsoever* means a conflict. `just
mergeable` says which, so read its output before editing any YAML.

## Merge commits, never squash

ADR 0009. Every commit on a branch must build and pass on its own — a history
where it does not only *looks* bisectable, and `git bisect` two weeks later is
the whole reason.

**Do not merge a stack bottom-up with `--delete-branch`.** Deleting a base
branch makes GitHub **close** the pull requests stacked on it rather than
retarget them. It happened to #3 and #5; no work was lost, but the recovery
cost a new pull request and an explanation on each closed one.

## Drafts get no CI at all

CI runs on ready pull requests and on `main`, nowhere else. Two consequences
worth holding onto:

- **`just mergeable` on a draft says so and refuses.** That is correct, not a
  bug in the tool: nothing has checked it.
- **Marking a pull request ready is what asks for a run.** Do it, then wait —
  and if no run appears, read `just mergeable`'s output before touching any
  workflow file. A conflicted branch produces no run either.

## When a pull request is red

Fix it in the next commit. It does not go back to draft — draft is for work
that is genuinely unfinished or handed over, not for work that failed once.

## Before restoring a file, look at what you are discarding

`git checkout <file>` reverts to the **committed** version. Anything
uncommitted in it is gone, including a test written minutes earlier. This has
destroyed work three times in one session: `discovery.rs`, `CHANGELOG.md` and
`config.rs`.

Copy the file to the scratchpad before sabotaging it, or commit first, and
restore from the copy. It is two more keystrokes and it has never once been the
thing that went wrong.

## The mutation signal

`just mutants` is a **signal, never a gate**. It cannot fail a build and it
does not hold a merge. It is scoped to the branch's diff, so its cost tracks
the size of the change rather than the size of the codebase.

Read the report when it lists survivors in code **this branch wrote**. Sort by
cost: fix what is cheap while the code is in hand; file a real bug and fix it
after this branch merges; or exclude it with a written reason.

**Establish equivalence by applying the mutation and running the suite**, never
by reasoning that it must be equivalent. Reasoning has been wrong; measurement
has not.

A survivor usually means one of two things, and both have been seen here:

- **A measurement that discards sign cannot test an operation that changes
  it.** Magnitudes, absolute values, counts and equality checks all discard it,
  and every one reads like a real assertion.
- **A boundary comparison surviving both `<=` and `>=` means neither end is
  exercised.** In this repo that shape is the backoff clamp, the inline-size
  limit and the attachment-name length check.

Do not run it while something else is compiling. It fans out.
