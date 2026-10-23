# CLAUDE.md

For working **on** hivemind. Somebody being asked to *run* it wants
[`docs/using.md`](docs/using.md), which the README points at.

How to work in this repo. `SPEC.md` is the source of truth for *what* hivemind
is and stays until it is all built; this is *how* it gets built;
`docs/decisions/` holds the arguments. The procedure lives in skills, which load
when they are used: `ci-merge` takes a branch to merged, `issue-batch` runs a
set of issues, `run-hivemind` installs it.

Much of what follows was learned by getting it wrong once. Where that is true it
says so, because a rule with its incident attached can be re-judged, and one
without can only be obeyed or ignored.

**Capped at 300 lines** (`just limits`): every session reads all of it. A rule
a tool enforces belongs in that tool's failure message, not here.

## North star

A small, always-on daemon that lets developers on the same LAN or Tailscale
network — and the Claude Code instances on their machines — send each other
messages and files.

**It is a mail service, not an orchestrator.** Store-and-forward, inbox and
outbox, attachments, threads. The intelligence stays in each Claude; hivemind
only carries the mail. A feature that starts scheduling work across machines,
or runs an agent because a message arrived, is out of scope and says so in
SPEC §12.

Three properties are load-bearing, and a change that costs one of them needs an
argument, not a shrug:

- **No central anything.** No broker, no cloud, no account (ADR 0001).
- **Files are the source of truth.** `index.db` is a cache that must survive
  being deleted (ADR 0002).
- **Delivery survives a closed laptop.** Retry forever; a laptop that comes to
  the office on Monday gets Friday's mail (SPEC §8).

## The spec is the contract

`SPEC.md` is normative. When something in it turns out to be wrong or
impossible, **write an ADR proposing the change and say so — never silently
diverge** (§16). The ADR amends the spec in the same commit, and most of
`docs/decisions/` was written exactly that way.

An amendment is cheap. A quiet divergence is a document that lies, and the next
person to read it cannot tell which half to trust.

## Where this runs

**macOS is the primary target; Linux builds and is tested in CI. Windows is not
supported** and nothing here should grow a `#[cfg(windows)]` without a
conversation first.

macOS is Darwin, not Linux: `std::os::unix` works on both, but `launchd`
rather than systemd, no `/proc`, case-insensitive filesystems by default.
Anything reaching for a platform service gets tested on both or guarded.

**There is no GPU and nothing here should want one.**

**CI is a different computer.** Cold, and both platforms in the matrix. "Works
here" and "passes CI" are separate claims — `just ci` runs exactly what GitHub
runs, in the same order, which is what makes a green local run mean something.

## How we work

### Gates block on correctness; signals inform on quality

A check that proves **correctness** — build, `clippy -D warnings`, the tests,
the golden vectors, the boundary linter — is a **hard gate**: green to merge, no
exceptions.

A check that **audits quality** is a **signal**: surfaced where it can be acted
on, never blocking a merge, like `just mutants`. Coverage is a gate only because
SPEC §13.2 says so, and even then a smoke detector: when it drops, look at what
is untested before looking at the number.

**Don't reach for a gate where a signal does the job.** A gate people route
around teaches everyone to route around gates.

### Architecture first, then infrastructure, then features

When something bites — or will bite more than once — **document it and fix it
before continuing.** Architecture and infrastructure problems halt feature work.

M6 turned up a `TODO(M3)` in a normative document, four milestones after M3
shipped; the fix was `just markers`, and it found two more.

### The flow

idea → issue → branch → PR → `just ci` green → `just mergeable` → merge.

Work starts as an issue, not a surprise diff, and the pull request references
the issue it closes. **Issue-less pull requests are fine** for documentation and
bug fixes; the description still has to clear the three gates below.

### The three gates, before building anything

An idea becomes an issue only when all three hold. If one fails, **push back
rather than comply**:

1. **Understanding** — the intent is clear and can be restated. If unsure,
   restate it and confirm. Do not guess.
2. **Value** — real value to the project. No busywork, no features for their
   own sake.
3. **Craft** — Rust good practice and the decided architecture. If the idea
   violates a settled decision, say so and propose the right shape.

### Merging is serialized

Two pull requests can each be green alone and break `main` together, because
Rust type-checks across crate boundaries: a changed signature in one crate and a
new caller in another compile apart and not together. No tooling repeals that.

One branch merging, one being written. **The binding constraint is file
collision, not branch count.** One worktree per branch, each with its own
`target/`; `just workspace` refuses a shared `CARGO_TARGET_DIR`, because it
produces a false green. `issue-batch` has the rest.

**`just mergeable N` before `gh pr merge`, always**, and merge commits, never
squash (ADR 0009): every commit on a branch builds and passes on its own. Never
hand-roll a "wait for CI" loop — `just mergeable N --wait` exists because one
read zero checks as green. `ci-merge` has the traps, each with what it cost.

### A ready pull request claims it passes; a draft makes no such claim

CI runs on ready pull requests and on `main`, nowhere else. So a red run always
means a claim was broken, which is worth a notification every time.

A draft is **how work survives** a question for the user, a failure that could
not be resolved, or a session that ended badly — none of which asserts anything.
The durable context is the issue, written to be read cold. **A red ready pull
request stays ready** and is fixed forward; draft is not for failing once.

### Unattended work

If in doubt mid-branch, comment on the issue and carry on rather than stall the
night on a question. In a *planning* conversation, ask immediately.

**Agent velocity is first-class.** Readable, lean code and a fast CI are part of
Craft, not a trade against it.

## Before a pull request

`just ci` must pass; `just --list` says what each gate holds.

### A gate with nothing to say says one line

`just ci`, `just test`, `just lint` and `just cov-gate` print **one line per
gate** when they pass, and the **whole output** of whatever fails. `--verbose`
on any of them streams the lot. A session once spent a quarter of its context
on output that had already answered its question (#71).

**For a long command no recipe covers**, use the same wrapper rather than
inventing a filter:

    python3 .github/scripts/quiet.py --label audit -- cargo audit --deny warnings

`--tail 1` keeps the command's own last line, which for a test runner is the
count of what ran. **Do not pipe a gate through `tail` or `grep`**: a command
that dies before printing anything has no error string to match, so a filter
looking for one finds a clean run. `ok`, `FAILED` and `skip` (an optional tool
is absent) are three different states.

## Testing

TDD, and the middle step is the one that matters: **watch the test fail.** A
test written after the code passes immediately, which proves nothing about
whether it can catch anything.

**When a test passes the first time it is run, sabotage the code and check it
fails.** Back the file up, break the thing the test is about, run the test,
restore. It costs seconds and it is the single highest-return habit here. One
session of it found three tests that asserted nothing and one that **hung**:

- An empty-list assertion against a service with no messages in it passed
  whether the filter worked or not, and the code returned *everything* (#28).
- `mergeable`'s wait test mocked `sleep` and left `monotonic` frozen, so a
  bounded loop ran forever. Nothing tells a hang from slowness.
- `refresh_peers`'s "Tailscale is off" test passed with the gate deleted,
  because this machine has no tailnet either way. Code whose interesting branch
  depends on the machine splits into a judgement and a lookup, so both branches
  are testable anywhere.

**Establish equivalence by applying a mutation and running the suite**, never by
reasoning that it must be equivalent. Reasoning has been wrong; measurement has
not.

**Back the file up to the scratchpad before sabotaging it. Never `git checkout`
to undo a sabotage.** It reverts to the **committed** version, discarding
anything uncommitted — including the test written minutes earlier. This has
destroyed work three times in one session.

`just mutants` is for one job: a whole module that reads well-covered when you
suspect nothing asserts its mechanism. Not every branch, out of habit. `ci-merge`
says how to read its report.

## File size is a ratchet

`just size` caps the lines of code in each Rust file, source and test counted
separately; comments and blanks are free, because `missing_docs` asks for them.
The caps live in `.github/scripts/size.py` and only ever come down, lowered by
the branch that split something. **`just size --report` lists what to split
next.** The branch that hits the cap pays for the split, as an issue of its
own (#100). Split by concern into a subfolder rather than adding a filename
prefix.

`just limits` holds the rest at fixed caps, prose included: `CLAUDE.md`, the
skills' Markdown and headers, and the Python and TypeScript.

## Documentation that cannot drift

`docs/openapi.json`, `docs/protocol.md`'s problem table, `README.md`'s
configuration table and `release.yml` are each checked against the code rather
than trusted. **When adding a document that restates something the code knows,
add the check in the same commit.** Otherwise it is a second place to be wrong,
and the second place is always the one somebody reads.

## Dependencies

**Measure before adding, and measure before keeping.** `reqwest` was here to
speak plain HTTP to `127.0.0.1`. It brought 107 crates and a second
cryptography backend, and Cargo's feature unification turned that backend on
for `hivemind-net` too: that is what made rustls panic in M3. `deny.toml` now
bans both. `cargo tree -i <crate>` answers "who pulls this in"; `-e normal`
leaves out dev-dependencies.

## Layout

| Crate | What it may do |
|---|---|
| `hivemind-core` | Domain, storage, index. Filesystem only — no network, no `async` |
| `hivemind-net` | Discovery, TLS transport, the delivery queue |
| `hivemind-api` | Service layer, loopback API, peer API, web UI |
| `hivemind-mcp` | The MCP server. Depends on `-api`, never the reverse |
| `hivemind-cli` | The binary. Talks to the daemon over HTTP like any other client |

`just boundaries` enforces what cargo cannot. Adding a rule there is one entry
with a reason.

## Conventions

- **Comments say why, never what.** The code says what it does. A comment earns
  its place by carrying the argument, the measurement, or the incident.
- **Conventional commits.** `just changelog` generates releases from them, and
  the `commit-msg` hook in `.pre-commit-config.yaml` refuses one that does not
  parse.
- Prose in comments and docs is British-ish and plain. No exclamation marks, no
  "simply", no "just" meaning "merely".

## Issues, labels and priority

**The unit of work is a well-specified issue**: the **what**, the **why it
belongs**, and the **roadmap — not the implementation intrinsics**. A future
Claude reads it cold and says *"I understand the assignment, I know how to
proceed."* That is what lets one run unattended.

**File what you notice.** Claude may open an issue on its own for anything that
will recur, or that a tool would solve more than once, when the benefit
outweighs the cost. The strongest come out of doing the work: a survivor that
was a real gap, a claim in a doc that quietly became false.

**File rather than fix when the finding is outside the branch in hand.** A
branch that grows to cover everything it noticed is a branch nobody can review.

**A bug is always filable.** The test above is about whether something is worth
*building*, never about whether a defect is worth *recording*. If the bug
questions a decision or exposes a foundational problem, say so to the user —
that is a judgement call. Otherwise keep it brief and carry on.

**Priority: architecture → infrastructure → bug → foundation → feature.**
`documentation` never waits its turn. Priority orders what gets **merged**,
never what gets **worked**.

**Stage labels — at most one, and absence means ready.** `idea` (might not add
value; parked until the user decides), `planning` (has value, approach still
being discussed), `human` (needs a person end to end). All three mean **do not
start**. A Claude-written issue must carry one if it is a breaking change,
changes human-facing behaviour, needs a judgement call, or proposes a structural
change. A `bug` usually should not — the deciding happened when the code broke.

**Reference the issue from the pull request that closes it, and check the
number.** A typo'd `Closes #N` closes the wrong issue or none, silently.

## Overrides

Any rule here may be overridden by the user's explicit say-so, in this prompt or
an earlier one. **One exception:** an issue carrying `idea` or `planning` is
never started while the label is on it. The user may say to remove the label and
then do it — never to do it with the label still on.
