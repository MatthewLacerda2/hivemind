# 0005. `rusqlite` with bundled SQLite for the index

- **Status:** accepted
- **Date:** 2026-09-17

## Context

The index is a local cache that answers unread counts, thread and peer filters,
and full-text search over subject and body ([0002](0002-files-are-source-of-truth.md)).
It is single-process, single-writer, local-only, and entirely rebuildable. The
hard constraint on it is latency: `hivemind hook check` runs on every Claude
Code `SessionStart` and `UserPromptSubmit` and must finish in under 100 ms
(SPEC §9.3), which rules out anything that has to open a network connection or
start a runtime to answer "how many unread".

SQLite is the obvious engine. The question is which Rust binding, and the
current landscape includes several rewrites and forks — `libsql`/Turso,
`sqlx` with its async story, and pure-Rust reimplementations — that are more
interesting than `rusqlite` and solve problems we do not have.

## Decision

`rusqlite` with the `bundled` feature, compiling SQLite from source into the
binary. FTS5 for search.

## Consequences

- No system SQLite dependency and no version skew: what we test is what ships,
  on every macOS and Linux machine and in CI. This matters for a Homebrew binary
  we do not want to see bug reports about from one particular OS version.
- Synchronous API, which is what `hivemind-core` needs — that crate is
  deliberately non-async (SPEC §3.1). The index is reached from async contexts
  through a blocking task, which is the right shape for a local file anyway.
- `rusqlite` is mature, widely deployed and unlikely to require attention. For a
  component whose job is to be boring and rebuildable, "unlikely to require
  attention" is the feature.
- Bundling adds a C compile to a clean build — tens of seconds, cached
  thereafter — and some binary size. Accepted.
- Writes are serialised through one connection. With one daemon process and a
  cache that is never the source of truth, there is no contention to solve.
- If the index ever needs to be replaced, the blast radius is one module and one
  file on disk. That is a direct consequence of
  [0002](0002-files-are-source-of-truth.md) and it is what makes committing to a
  boring choice here cheap.

## Alternatives considered

**Turso / `libsql`.** Offers embedded replicas and sync to a remote primary.
Rejected: those features exist to solve distribution, and our index is local,
derived and disposable — there is nothing to replicate. It would also pull a
larger, faster-moving dependency into the one place we want stability, and its
value proposition points at a cloud we have ruled out (SPEC §1).

**`sqlx`.** Compile-time-checked queries are genuinely attractive. Rejected: it
is async-first, which fights the no-async rule in `hivemind-core`, and its
offline query cache adds a build-time artifact to keep in sync for a schema that
is regenerated from scratch on every version bump.

**System SQLite via `rusqlite` without `bundled`.** Smaller binary, uses the
platform library. Rejected: FTS5 availability and version differences across
distributions turn into bug reports we cannot reproduce.

**No SQLite at all — an in-memory index rebuilt on daemon start.** Tempting,
since the index is already rebuildable. Rejected on SPEC §9.3: `hook check` is a
separate short-lived process that must not pay a rebuild, so the index has to
outlive the process that reads it.
