# CLAUDE.md

For working **on** hivemind. Somebody being asked to *run* it wants
[`docs/using.md`](docs/using.md), which the README points at.

How to work in this repo. `SPEC.md` is the source of truth for *what* hivemind
is and stays until it is all built; this is *how* it gets built;
`docs/decisions/` holds the arguments.

Much of what follows was learned by getting it wrong once. Where that is true it
says so, because a rule with its incident attached can be re-judged, and one
without can only be obeyed or ignored.

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
diverge** (§16).

Seven have been written that way, and each amended the spec in the same commit:
the node id's group count (0006), `received_at` not being signed (0007), the
service layer staying synchronous (0008), TLS admitting strangers (0010), what
adopting `dist` cost (0011), membership being a group key rather than pairwise
confirmation (0013), and a relay needing end-to-end encryption (0014).

An amendment is cheap. A quiet divergence is a document that lies, and the next
person to read it cannot tell which half to trust.

## Where this runs

**macOS is the primary target; Linux builds and is tested in CI. Windows is not
supported** and nothing here should grow a `#[cfg(windows)]` without a
conversation first.

macOS is not Linux underneath — Darwin is Mach plus BSD, not a Linux kernel.
The two are POSIX-ish together, which is why `std::os::unix` works on both, and
that is where the similarity stops: `launchd` rather than systemd, no `/proc`,
different filesystem case sensitivity by default. Anything reaching for a
platform service gets tested on both or guarded.

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
on, never blocking a merge. `just mutants` is one. Coverage is the exception
that proves the rule — it is a gate here only because SPEC §13.2 says so, and
even then it is a smoke detector: when it drops, look at what is untested
before looking at the number.

**Don't reach for a gate where a signal does the job.** A gate people route
around teaches everyone to route around gates.

### Architecture first, then infrastructure, then features

When something bites — or will bite more than once — **document it and fix it
before continuing.** Architecture and infrastructure problems halt feature work.

This is not theory here. M6 turned up a `TODO(M3)` in a normative document, four
milestones after M3 shipped; the fix was `just markers`, and it found two more.

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

One branch merging, one being written, is the shape that keeps the queue moving
without paying for it twice. **The binding constraint is file collision, not
branch count** — two branches adding a variant to the same enum cost more than
four in genuinely separate crates.

**One worktree, one `target/`. Never a shared `CARGO_TARGET_DIR`.** Cargo keys
artifacts by package, version, features and profile — never by source path — so
worktrees sharing a target directory overwrite each other's output for anything
a given build did not itself rebuild. The phantom failures waste an hour; the
false *green* is the reason it is a rule, because it claims the gates passed on
code that was never compiled.

### A ready pull request claims it passes; a draft makes no such claim

CI runs on ready pull requests and on `main`, nowhere else. So a red run always
means a claim was broken, which is worth a notification every time.

A draft is not decoration on unfinished work — it is **how work survives** a
question that needs the user, a failure that could not be resolved, or a session
that ended badly. None of those asserts anything, so there is nothing to check.
The durable context is the issue, written to be read cold; never rely on a
hand-back comment existing.

**A red ready pull request stays ready** and is fixed forward. Draft is not
where something goes because it failed once.

### Unattended work

If in doubt mid-branch, leave a comment on the issue and carry on rather than
stalling the night on a question. Questions during a *planning* conversation are
asked immediately.

**Agent velocity is first-class.** Write code that is readable by design and
lean; keep CI fast. That is part of Craft, not a trade against it.

## Before a pull request

`just ci` must pass. `just --list` has the rest. The gates worth knowing:

| Recipe | What it holds |
|---|---|
| `just size` | No file over its line-of-code limit. A ratchet — see below |
| `just boundaries` | The layering SPEC §3.1 and §10 describe |
| `just markers` | A `TODO(Mn)` whose milestone has shipped |
| `just scripts` | The Python under `.github/scripts`, stdlib `unittest` |
| `just openapi-check` | `docs/openapi.json` against the code |
| `just dist-check` | `release.yml`, which `dist` generates |
| `just cov-gate` | The 85% floor SPEC §13.2 asks for |

Signals, opt-in and never part of passing: `just mutants`, `just cov`.

## Merging

**`just mergeable N` before `gh pr merge`, always.** It asks GitHub whether
CI genuinely ran on the head commit, which is a different question from whether
the checks look green. `gh pr checks` is not a substitute — it blends runs, so a
skipped one hides behind a real one.

**Never hand-roll a "wait for CI" loop.** The one used for M3 through M6 was:

    until [ "$(gh pr view N --json statusCheckRollup --jq
        '[... select(.status != "COMPLETED")] | length')" = "0" ]

With zero checks recorded that is `0`, so it exits on its first iteration and
the conclusions printed afterwards are an empty list — which reads exactly like
green. **Absent and passing are different states, and a loop counting
unfinished checks finds zero of each.** Four merges went through it; all four
happened to have checks.

**Merge commits, never squash** (ADR 0009). Every commit on a branch must build
and pass on its own; a history where it does not only *looks* bisectable, and
`git bisect` two weeks later is the whole point.

**Do not merge a stack bottom-up with `--delete-branch`.** Deleting a base
branch makes GitHub **close** the pull requests stacked on it rather than
retarget them. It happened to #3 and #5.

The `ci-merge` skill has the rest, including how to triage a mutation report.

## Testing

TDD, and the middle step is the one that matters: **watch the test fail.** A
test written after the code passes immediately, which proves nothing about
whether it can catch anything.

**When a test passes the first time it is run, sabotage the code and check it
fails.** `just mutants` is the automatic form of this and is scoped to the
branch's diff. Both have earned their keep — twice by finding a test that was
passing for the wrong reason:

- `doctor`'s "optional tools are absent, not broken" rule had no failing case,
  because this machine has Tailscale installed and the interesting branch never
  ran. Split into a judgement and a lookup so both branches are testable
  anywhere.
- `boundaries.py`'s network rule was anchored to the start of a line, as if
  scanning `Cargo.toml`. It matched `tokio = …` and never `use tokio::fs`. It
  would never have fired.

**Establish equivalence by applying a mutation and running the suite**, never by
reasoning that it must be equivalent. Reasoning has been wrong; measurement has
not.

**Coverage and mutation measure different things, and the gap between them is
where bugs live.** `outbox.rs` — the bridge between the mail service and the
delivery worker — read 85–90% covered and had **19 surviving mutants**: every
method could be replaced with a no-op and nothing failed. Its lines execute
during the integration tests; nothing asserted on them. The sharpest case was
`due_a_greeting`, which survived `true`, `false` and both sides of its
comparison — the limit that stops an mDNS browse result becoming a TLS
handshake every minute was entirely unasserted.

A whole module with nothing asserting its mechanism is **one finding about the
tests**, not a list of survivors. Stop and fix that; a scattered survivor in
code this branch did not write can wait.

**Back the file up to the scratchpad before sabotaging it. Never `git checkout`
to undo a sabotage.** It reverts to the **committed** version, discarding
anything uncommitted — including the test written minutes earlier. This has
destroyed work three times in one session: `discovery.rs`, `CHANGELOG.md` and
`config.rs`. Copy first, or commit first, and restore from the copy.

## File size is a ratchet

`just size` caps lines of code per file: **800 source, 600 test**, counted
separately because most tests here live in the file they test.

Blank lines and comments are free. `missing_docs` is a merge gate and the house
style is to explain *why*, so a cap that counted prose would put those two
rules in opposition and split files whose code was never the problem.

**The numbers are measured, not chosen.** They started just *under* the largest
file of each kind, so landing the gate cost two small splits — the peer
commands out of `commands.rs`, and the blob tests out of `peer.rs` — rather
than a refactor. A gate that passes on the day it arrives is a gate nobody
knows works.

**They only ever come down.** Lower them as files are split; never raise them
to admit growth. `just size --report` lists what to split next, largest first —
`service.rs` at 667 source lines is the standing answer.

Split by concern and **group into a subfolder rather than adding a filename
prefix**. A shared prefix on sibling files is a subfolder waiting to happen.

## Documentation that cannot drift

Four documents are checked against the code rather than trusted:

| Document | Checked by |
|---|---|
| `docs/openapi.json` | `just openapi-check` — regenerates and diffs |
| `docs/protocol.md`'s problem table | a test over `ProblemType::ALL`, both directions |
| `README.md`'s configuration table | a test reading `Config`'s fields from the source |
| `.github/workflows/release.yml` | `just dist-check` — `dist` regenerates it |

**When adding a document that restates something the code knows, add the check
in the same commit.** Otherwise it is a second place to be wrong, and the second
place is always the one somebody reads.

## Dependencies

**Measure before adding, and measure before keeping.** `reqwest` was here to
speak plain HTTP to `127.0.0.1`. It brought 107 crates, about twenty otherwise
absent, including `aws-lc-rs` and its C library through `hyper-rustls` — a
second cryptography backend, compiled, for unencrypted loopback requests.

The weight was the smaller half. Cargo unifies features across a build, so
`hyper-rustls` asking `rustls` for `aws-lc-rs` turned it on for `hivemind-net`
too, beside the `ring` this workspace chose. **That is what made rustls unable
to pick a default provider and panic during M3**, diagnosed at the time only as
"something in the workspace enables `aws-lc-rs`".

`cargo tree -i <crate>` answers "who pulls this in"; `cargo tree -e normal`
excludes dev-dependencies. Both were needed to see it.

**HTTP clients here are written against `hyper` directly.** There are two, and
each says in its module doc why it is not `reqwest`.

## Layout

| Crate | What it may do |
|---|---|
| `hivemind-core` | Domain, storage, index. Filesystem only — no network, no `async` |
| `hivemind-net` | Discovery, TLS transport, the delivery queue |
| `hivemind-api` | Service layer, loopback API, peer API, web UI |
| `hivemind-mcp` | The MCP server. Depends on `-api`, never the reverse |
| `hivemind-cli` | The binary. Talks to the daemon over HTTP like any other client |

`just boundaries` enforces what cargo cannot: SQL only in `index.rs`, no network
types in `hivemind-core`, and the CLI reaching the store only where SPEC §10
allows it. Adding a rule there is one entry with a reason.

## Conventions

- **Comments say why, never what.** The code says what it does. A comment earns
  its place by carrying the argument, the measurement, or the incident.
- **`expect("…")`, not `unwrap()`** outside tests. Clippy denies it, and
  `clippy.toml` steps aside inside test code where a failed unwrap *is* the
  assertion.
- **No `unsafe`, at `forbid` level.** An exception gets an ADR, not an
  `#[allow]` — and `forbid` refuses the `#[allow]` outright.
- **Conventional commits.** `just changelog` generates releases from them.
- Prose in comments and docs is British-ish and plain. No exclamation marks, no
  "simply", no "just" meaning "merely".

## Issues, labels and priority

**The unit of work is a well-specified issue**: the **what**, the **why it
belongs**, and the **roadmap — not the implementation intrinsics**. A future
Claude reads it cold and says *"I understand the assignment, I know how to
proceed."* That is what lets one run unattended.

**File what you notice.** Claude may open an issue on its own for anything that
will recur, or that a tool would solve more than once, when the benefit
outweighs the cost of building it. The strongest issues come out of doing the
work — a mutation survivor that turned out to be a real gap, a claim in a doc
that quietly became false.

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
