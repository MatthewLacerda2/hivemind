# Contributing to hivemind

## Setup

You need Rust (the version is pinned in `rust-toolchain.toml`; rustup installs
it for you) and [`just`](https://github.com/casey/just).

```
brew install just
cargo install cargo-nextest cargo-deny cargo-audit cargo-llvm-cov

pip install pre-commit
pre-commit install --install-hooks
pre-commit install --hook-type commit-msg

just ci     # should be green before you change anything
```

`just ci` runs exactly what GitHub Actions runs, in the same order. If CI fails
on something `just ci` passes, that gap is itself a bug worth reporting.

It prints one line per gate while everything passes, and the whole output of
whatever fails. `just ci --verbose` streams all of it instead, and so does
`--verbose` on `just test`, `just lint` and `just cov-gate`.

## The rules that are not negotiable

These come from [SPEC.md](SPEC.md) §13, which is worth reading in full before
your first PR.

- **No `unsafe`.** The workspace denies it. An exception needs an ADR, not an
  `#[allow]`.
- **No `unwrap()` or `expect()`** outside tests without an `// INVARIANT:`
  comment saying why it cannot fire.
- **Clippy pedantic, warnings are errors.** The allow-list lives in the root
  `Cargo.toml` and every entry has a reason next to it. Adding one is a review
  conversation.
- **Test names describe behaviour.** `unpaired_peer_delivery_is_rejected_with_403`,
  not `test_delivery_2`. If you cannot name the behaviour, you do not yet know
  what you are testing.
- **`hivemind-core` does no I/O beyond the filesystem and has no `async`.**
  Everything depends on it; keeping it synchronous keeps the rest testable.
- **Public API of `hivemind-core` is documented.** `missing_docs` is on.
- **Coverage floor of 85%** on `hivemind-core` and `hivemind-net`. Coverage is a
  smoke detector, not a goal — a PR that games it is worse than one that misses
  it.

## Commits and PRs

Conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`),
enforced by a `commit-msg` hook. `CHANGELOG.md` is generated from them with
`git-cliff`, so an unparseable commit is a hole in the changelog.

Branch protection on `main`: CI must pass, one approving review, **merge commit
— not squash** ([ADR 0009](docs/decisions/0009-merge-commits-not-squash.md)).

That last one has teeth: **every commit on your branch must build and pass its
own tests on its own.** The whole reason we keep individual commits is so
`git bisect` can name the one that broke something, and a history containing
commits that do not compile only looks bisectable. If you need to fix something
you did three commits ago, amend or rebase it away before you open the PR —
do not add a "fix the thing I broke earlier" commit.

Your PR description says which sections of the spec it implements. Reviewers
check the code against the spec, so make that easy.

## Changing the spec

SPEC.md is the source of truth. When it turns out to be wrong or impossible —
which will happen — **stop and write an ADR proposing the change, then ask.**
Do not silently diverge. A PR that quietly does something other than what the
spec says is the one thing that will get turned down on sight, however good the
code is.

Copy `docs/decisions/0000-template.md`, take the next number, and be honest in
the Consequences section. A record that lists no downside is a record nobody
thought about.

## Working through the milestones

Work is organised as the milestones in SPEC.md §14, and each one is a mergeable
set of PRs with green CI. Do not start M(n+1) until M(n) is green — the point of
the ordering is that each milestone is independently usable and independently
reviewable.

## Tests that need a real network

mDNS is unreliable inside CI containers, so discovery tests are `#[ignore]`d and
run with `just test-network` on a real machine. The discovery backend is behind
a trait, so everything *except* multicast itself is covered by the normal suite.
If you touch discovery, run `just test-network` and say so in the PR.
