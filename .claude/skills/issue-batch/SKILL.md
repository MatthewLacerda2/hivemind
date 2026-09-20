---
name: issue-batch
description: Run a set of issues from the board through to merged — how many branches at once, which ones can safely run together, worktrees, briefing subagents, and re-reading the board. Use when starting work on one or more issues, when deciding what to start next, or when told to "do the issues".
---

# Working a batch of issues

The user rarely has one issue. An idea becomes several, and more appear as
coding starts. This is how a set of them gets worked without the batch costing
more than the work.

`CLAUDE.md` carries why these exist; this is how. The `ci-merge` skill takes a
single branch from finished to merged, and this skill does not restate it.

## Two branches in flight, pipelined

Coding parallelises. **Merging does not** — Rust is compiled, so merges are
serialized and the queue is the bottleneck.

Every branch that is not first pays a rebase for each merge ahead of it. Over
shared code that is **N(N−1)/2 rebases**: two branches cost one, three cost
three, four cost six. A rebase buys no correctness.

So: **one in the merge queue, one being written.** Nothing idles through a CI
run, and nothing rebases twice.

## The real limit is file collision, not count

Two branches adding a variant to the same enum cost more than four branches in
genuinely separate areas. A clean rebase is seconds of `git`; a colliding one is
a whole session.

**Before starting a second branch, ask which files it will open.** The ones
where everything collides here, measured over the last forty commits rather than
guessed:

- `crates/hivemind-api/src/service/` and `crates/hivemind-cli/src/commands/`,
  both of which were single files until #100 and #34 split them. Splitting
  helped: two branches now meet only if they want the same concern rather than
  the same file. Two branches in one submodule is still the case to avoid above
  all others, and `service.rs` itself still holds the type every child touches.
- `crates/hivemind-api/src/local.rs`, the largest file left and the one
  `just size --report` now names, and `peer.rs` beside it. A CLI change and a
  service change usually meet in one of these.
- `justfile`, `Cargo.toml`, `Cargo.lock` — appended to by almost everything.
- The four documents checked against the code: `docs/openapi.json`,
  `docs/protocol.md`'s problem table, `README.md`'s configuration table,
  `.github/workflows/release.yml`. These collide *and* the regenerated result
  differs from either side, so the conflict cannot be resolved by picking one.
- `SPEC.md` and `CHANGELOG.md`, for the same reason.

Two branches in `hivemind-core` storage and in the web UI barely touch each
other. Two branches both adding a problem type will collide every time.

## Group the work before splitting it

**Split by responsibility, not by parallelism.** If a parent's sub-issues all
touch the same type, they are **one branch**, not one each.

Splitting an issue so several agents can run at once optimises the half that was
never scarce, and manufactures collisions: four sub-issues each adding a variant
to one enum is four rebases, four CI cycles and four mutation reports for one
coherent change.

Sub-issues are for work that is genuinely separable *in the code*, not for work
that is merely listable.

## Each branch gets its own worktree

One checkout per branch, never two branches taking turns in one. A shared
checkout mixes another issue's edits into `just ci` and thrashes `target/`.

**Never set `CARGO_TARGET_DIR`.** Cargo's default already gives every worktree
its own `target/`; an override makes worktrees overwrite each other's artifacts
and produces a false green. `just workspace` refuses it and `just ci` runs that
first, so this one is enforced rather than remembered.

**Remove a worktree the moment its branch merges**, and `just reap` frees the
`target/` of any you forgot. `target/` in this repo measured **32 GB** in
September 2026, and each worktree carries its own. Disposal is what keeps disk
from becoming the overnight failure, and it arrives as a confusing build error
rather than as "no disk" — `just workspace` warns below 10 GB.

## Starting

- **Assign the user the moment work begins** — unassigned means fair game.
  **Unassign** if it turns out the issue was never started.
- Branch off the latest `main` with a readable slug carrying the issue number:
  `fix-20-hostname` is the precedent on this repo. An issue-less pull request
  uses a readable slug alone — `adr-naming`, `ci-drafts-and-size`.
- Open a **draft** pull request on the first commit. Draft is how work survives
  a session that ends badly, and it costs nothing because CI does not run on
  drafts anyway. The issue is the durable context; a hand-back comment may never
  get written.

## Waiting for CI is the expensive step, and it is manual here

`just ci` locally, then ready, then wait for the run on the rebased head, then
**`just mergeable N`** — the number positionally. `PR=N` reads like a named
argument and is not one: `PR` is a recipe parameter, so `just` hands the whole
string over as its value. It is refused by name rather than by a generic usage
line (#48).

The waiting is minutes per branch, and `just mergeable N --wait` is what does
it: it stops on a red run rather than sitting on it, and gives up after half an
hour rather than never. A batch still pays the wait once per merge, which is
the whole argument for having a second branch being written while the first
waits.

**Never hand-roll a wait loop to fill the gap.** The `ci-merge` skill has the
one that read an empty check list as green, and why absent and passing are
different states.

## Re-read the board after every merge

A merge changes the graph. Whatever the merged issue blocked is fair game the
moment it lands, so the decision is one merge wide, not one batch wide.

Re-reading is not a licence to start everything: **start the next one, and keep
the second slot for whatever is furthest along.** Priority orders what gets
merged, never what gets worked. An unblocked issue left unstarted is not wasted
capacity; it is a rebase not yet paid for.

**A stage label is the only absolute stop.** `idea`, `planning` and `human` mean
*not yet*, and no amount of the issue looking ready overrides that. The user may
say to remove a label and then do the work — never to do it with the label still
on. Everything else is startable the moment it exists, including an issue filed
a minute ago.

## Briefing a subagent

Point it at `CLAUDE.md` first, then the issue — issues here are written to be
read cold. Beyond that:

- Name the **base commit** and what has landed recently that it must respect.
- Name the **siblings** and which files they are touching, from the collision
  list above.
- Tell it to invoke the **`ci-merge` skill** rather than restating that
  protocol.
- Tell it **not to merge**. Merging is serialized and belongs to the session
  running the batch.
- Tell it not to run `just mutants` while a sibling is compiling. It fans out.
- **Scratch filenames must carry the issue number.** The scratchpad is shared
  between sibling agents, and a collision has already swapped one pull request's
  description for another's.

## Model, as a hint

Judgement work — design, implementation, triage — wants the strongest model. A
rebase, a module-list conflict, an attribute moved between files does not. Most
sessions on a branch are the second kind. The line is not crisp, so err upwards.

## When to hand back to the user

- Roughly three attempts at the same failure.
- A decision that is genuinely theirs: a format change, a name people will type,
  anything a `planning` label would have carried.
- Leave the pull request **draft**, say why in a comment, and stop. Do not
  thrash.

Working unattended, prefer leaving a comment on the issue and carrying on over
stalling the night on a question.

## Reporting back

The user is not reading the transcript of a batch. Batches take hours, often
overnight, and the transcript is at best notes for Claude itself.

**When things go well, say what the result was.** When they did not go as one
would expect, say what the surprise was. That does not necessarily mean things
went badly — it is written down because the more that can be predicted, the
better the next batch gets.

Two things still interrupt, because they are the ones the user would want to
overrule and overruling is only possible while the batch is still running: **a
change to the user's own files** outside the repo, and **a decision reversed**,
where the issue said one thing and the branch did another.
