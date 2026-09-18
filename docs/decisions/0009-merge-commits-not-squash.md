# 0009. Merge commits, not squash merges

- **Status:** accepted
- **Date:** 2026-09-17
- **Amends:** SPEC §13.4

## Context

SPEC §13.4 said branch protection should require "squash merge". That is the
common default, and it produces a tidy one-commit-per-PR history.

It also destroys the information you need when something breaks. A squashed PR
is a single commit containing everything from "add the message model" to
"complete the local API". Two weeks later, `git bisect` can tell you that commit
is the culprit and nothing more — which, for a PR touching five files across
three crates, is barely narrower than "somewhere in the last fortnight".

The project owner raised this directly, and it is their history to live with:

> I'm not a fan of finding out I can't tell which of 40 commits broke something
> 2 weeks later, hence why I don't squash if I can avoid it.

## Decision

Merge pull requests with **merge commits**. Do not squash, and do not rebase
into the base branch.

This puts a real constraint on how PRs are written: **every commit on a branch
must build and pass its own tests on its own.** A bisectable history is the
entire point, and a history with commits that do not compile is not bisectable —
it just looks like one. "Fix the thing I broke two commits ago" is not an
acceptable commit; fix it in place before the branch is merged.

Branch protection therefore requires: CI passes, one approving review, merge
commit. Conventional commit messages stay as they were, since `git-cliff`
generates `CHANGELOG.md` from them (SPEC §13.5) and it now sees every commit
rather than one squashed summary per PR.

## Consequences

- `git bisect` lands on the commit that actually broke something, not on the
  PR that contained it.
- `CHANGELOG.md` gets finer-grained entries, because `git-cliff` sees each
  `feat:` and `fix:` rather than one per PR.
- The graph has merge commits in it and is not a straight line. `git log
  --first-parent` gives the one-entry-per-PR view for anyone who wants it.
- Writing branches is more work: commits have to be coherent in order, and
  a mistake made early has to be amended or rebased away before merge rather
  than patched later on the same branch. That is the cost, and it is the cost
  that buys the bisect.
- **Stacked PRs need care.** Merging a stack with `--delete-branch` closes the
  PRs stacked on the deleted base rather than retargeting them. Merge from the
  bottom up, let each child retarget, and delete branches afterwards — or avoid
  stacks and branch from `main`.

## Alternatives considered

**Squash merge**, as §13.4 originally said. Rejected for the reason above: it
optimises for a tidy log over the ability to find a regression, and the log is
read far less often than a bisect is run.

**Rebase merge.** Keeps every commit *and* a linear history, which sounds like
the best of both. Rejected: it rewrites every commit's hash on merge, so a SHA
quoted in an issue, a review comment or a CI run stops resolving, and it loses
the record of which PR a commit arrived through.
