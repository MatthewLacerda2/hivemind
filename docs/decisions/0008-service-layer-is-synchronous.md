# 0008. The service layer is synchronous, and handlers call it directly

- **Status:** accepted
- **Date:** 2026-09-17

## Context

`hivemind-core` is deliberately synchronous: SPEC §3.1 forbids async in it, and
that is what lets the store and index be tested without a runtime. The API crate
above it is axum on tokio, which is not.

Somewhere between the two, a synchronous filesystem read and a `SQLite` query
happen inside an async handler. The textbook answer is `tokio::task::spawn_blocking`,
because blocking a runtime worker stalls every other task scheduled on it.

The textbook answer is aimed at servers doing seconds of work per request, or
sharing a runtime with latency-sensitive traffic. Neither describes this. The
local listener serves one person and their Claude on their own machine
(SPEC §7.1). A service call reads one JSON file of at most 1 MiB, or runs one
indexed `SQLite` query against a local file. That is microseconds to low
milliseconds, on a multi-threaded runtime with a worker per core.

## Decision

`MailService` is synchronous. Handlers call it directly, without
`spawn_blocking`.

The index lives behind a `std::sync::Mutex`, and **no `.await` happens while
that lock is held** — every critical section is a single service call that
returns before the handler yields.

## Consequences

- One implementation of every operation, callable from an axum handler, from an
  MCP tool, and from a test, with no runtime needed. SPEC §3 requires that
  single implementation, and making it async would push a runtime requirement
  down into everything that wants to reuse it.
- Tests stay plain `#[test]` functions. The 14 service tests need no `#[tokio::test]`
  and no runtime, which is why they run in about a tenth of a second.
- A `std::sync::Mutex` held across an `.await` would be a deadlock waiting to
  happen. It is safe here only because of the rule above, and that rule is a
  real constraint on future code, not a description of an accident. Anything
  that needs to await while holding index state must restructure rather than
  reach for `tokio::sync::Mutex`, which would make every call site async and
  undo the first bullet.
- If a handler ever does genuinely slow work — a 2 GiB blob hashed inline, a
  rebuild of a very large mailbox on a request path — this stops being true. The
  escape hatch is per call site, not architectural: wrap that one call in
  `spawn_blocking`. `MailService` is `Send + Sync`, so that works without
  changing its shape.
- **Blob streaming in M4 does not get this exemption.** Reading up to 2 GiB with
  range requests is exactly the case this decision does not cover, and it should
  be async from the start.

## Alternatives considered

**`spawn_blocking` around every service call.** Correct by construction and the
advice anyone would give. Rejected as disproportionate: it puts a thread hop and
an `Arc` clone in front of a 50-microsecond `SQLite` query, and it makes every
handler noisier for a stall that cannot be observed on a single-user loopback
listener.

**Make the service async and the core async with it.** Rejected outright: SPEC
§3.1 makes `hivemind-core` synchronous on purpose, and the reason — that the
domain logic is testable without a runtime — is worth more than uniformity.

**`tokio::sync::Mutex` for the index.** Would make holding the lock across an
await safe. Rejected: it makes acquiring the lock async, which makes every
service method async, which is the previous alternative by another route.
