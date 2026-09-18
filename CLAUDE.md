# CLAUDE.md

How to work in this repo. `SPEC.md` is the source of truth for *what* hivemind
is; this is *how* it gets built. `docs/decisions/` holds the arguments.

Most of what follows was learned by getting it wrong once. Where that is true it
says so, because a rule with its incident attached can be re-judged, and one
without can only be obeyed.

## The spec is the contract

`SPEC.md` is normative. When something in it turns out to be wrong or
impossible, **write an ADR proposing the change and say so — do not silently
diverge** (§16). Four have been written that way: the node id's group count
(0006), `received_at` not being signed (0007), TLS admitting strangers (0010),
and what adopting `dist` cost (0011). Each amended the spec in the same commit.

An amendment is cheap. A quiet divergence is a document that lies.

## Before a pull request

`just ci` must pass. It runs exactly what GitHub runs, in the same order —
that is the point of the file, so a green local run means a green remote one.

`just --list` has the rest. The gates worth knowing by name:

- `just boundaries` — the layering SPEC §3.1 and §10 describe, held to rather
  than hoped for.
- `just markers` — a `TODO(Mn)` whose milestone has shipped. Three were found
  at M6, one of them in the document the milestone was meant to have finished.
- `just scripts` — the Python under `.github/scripts`, stdlib `unittest`.
- `just dist-check` — `release.yml` is generated; this fails if it has drifted.

## Merging

**`just mergeable PR=N` before `gh pr merge`, always.** It asks GitHub whether
CI genuinely ran on the head commit, which is not the same question as whether
the checks look green.

The loop it replaces was used for M3 through M6:

    until [ "$(gh pr view N --json statusCheckRollup --jq
        '[... select(.status != "COMPLETED")] | length')" = "0" ]

With zero checks recorded that is `0`, so it exits immediately and the
conclusions printed afterwards are an empty list. **Absent and passing are
different states, and a loop counting unfinished checks finds zero of each.**
Four merges went through it; all four happened to have checks.

**Merge commits, never squash** (ADR 0009). Every commit on a branch must build
and pass on its own — a history where it does not only looks bisectable.

**Do not merge a stack bottom-up with `--delete-branch`.** Deleting a base
branch makes GitHub *close* the pull requests stacked on it rather than
retarget them. That happened on #3 and #5.

## Testing

TDD, and the important half is the middle step: **watch the test fail.** A test
written after the code passes immediately, which proves nothing about whether it
can catch anything.

**When a test passes the first time it is run, sabotage the code and check it
fails.** This has earned its keep repeatedly — and twice it found that a test
was passing for the wrong reason:

- `doctor`'s "optional tools are absent, not broken" rule had no failing case,
  because the development machine has Tailscale installed and the interesting
  branch never ran. Split into a judgement and a lookup so both branches are
  testable anywhere.
- `boundaries.py`'s network rule had a pattern anchored to the start of a line,
  as if scanning `Cargo.toml`. It matched `tokio = …` and never `use tokio::fs`.
  It would never have fired.

**Back the file up to the scratchpad before sabotaging it. Never `git checkout`
to undo a sabotage.** `git checkout <file>` reverts to the *committed* version,
which discards anything uncommitted — including the test just written. This has
destroyed work three times in one session: `discovery.rs`, `CHANGELOG.md` and
`config.rs`. Copy the file first, or commit first, and restore from the copy.

The 85% coverage floor is a gate because SPEC §13.2 says so. Coverage is a
smoke detector, not a target: when it drops, look at what is untested before
looking at the number.

## Documentation that cannot drift

Four documents are checked against the code rather than trusted:

| Document | Checked by |
|---|---|
| `docs/openapi.json` | `just openapi-check` — regenerates and diffs |
| `docs/protocol.md`'s problem table | a test in `hivemind-api` over `ProblemType::ALL` |
| `README.md`'s configuration table | a test in `hivemind-core` over `Config`'s fields |
| `.github/workflows/release.yml` | `just dist-check` — `dist` regenerates it |

When adding a document that restates something the code knows, add the check in
the same commit. The alternative is a second place to be wrong, and the second
place is always the one somebody reads.

## Dependencies

**Measure before adding, and measure before keeping.** `reqwest` was used to
speak plain HTTP to `127.0.0.1`. It brought 107 crates, about twenty of them
otherwise absent, including `aws-lc-rs` and its C library through
`hyper-rustls` — a second cryptography backend, compiled, for unencrypted
loopback requests.

The weight was the smaller half. Cargo unifies features across a build, so
`hyper-rustls` asking `rustls` for `aws-lc-rs` turned it on for `hivemind-net`
too, beside the `ring` this workspace chose. **That is what made rustls unable
to pick a default provider and panic during M3**, diagnosed at the time only as
"something in the workspace enables `aws-lc-rs`".

`cargo tree -i <crate>` answers "who pulls this in", and `cargo tree -e normal`
excludes dev-dependencies. Both were needed to see it.

HTTP clients here are written against `hyper` directly. There are two, and each
says in its module doc why it is not `reqwest`.

## Conventions

- **Comments say why, never what.** The code says what it does. A comment
  earns its place by carrying the argument, the measurement, or the incident.
- **`expect("…")`, not `unwrap()`** outside tests — clippy denies it, and
  `clippy.toml` steps aside inside test code where a failed unwrap *is* the
  assertion.
- **No `unsafe`, `forbid`-level.** An exception gets an ADR, not an
  `#[allow]` — and `forbid` refuses the `#[allow]` outright.
- **Conventional commits.** `just changelog` generates releases from them.
- Files are the source of truth; `index.db` is a cache that must survive being
  deleted (ADR 0002). There is a property test asserting a rebuilt index equals
  the one maintained along the way.

## Layout

| Crate | What it may do |
|---|---|
| `hivemind-core` | Domain, storage, index. Filesystem only — no network, no `async` |
| `hivemind-net` | Discovery, TLS transport, the delivery queue |
| `hivemind-api` | The service layer, the loopback API, the peer API, the web UI |
| `hivemind-mcp` | The MCP server. Depends on `-api`, never the reverse |
| `hivemind-cli` | The binary. Talks to the daemon over HTTP like any other client |

`just boundaries` enforces the parts cargo cannot: SQL only in `index.rs`, no
network types in `hivemind-core`, and the CLI reaching the store only where
SPEC §10 allows it.
