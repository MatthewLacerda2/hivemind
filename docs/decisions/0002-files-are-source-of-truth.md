# 0002. Files are the source of truth; the database is a cache

- **Status:** accepted
- **Date:** 2026-09-17

## Context

hivemind stores mail that people care about: a coworker's message, an
attachment, a thread that explains why something was built. That data has to
survive a crash mid-write, a daemon killed by the OS, a schema change in a
future version, and the user poking around in `~/.hivemind/` with `cat`.

It also has to be queried: unread counts on every Claude Code turn boundary in
under 100 ms (SPEC §9.3), filters by thread and by peer, and full-text search
over subject and body (SPEC §4.3). A directory of JSON files answers none of
those quickly once there are thousands of messages.

These two requirements pull in opposite directions, and the usual failure is to
let the database win and end up with mail that only one version of one program
can read.

## Decision

`~/.hivemind/mail/` is authoritative. One JSON file per message, in a
Maildir-style `new`/`cur`/`out`/`sent` layout, written to a temporary file and
moved into place with an atomic rename. No file is ever modified in place.

`index.db` (SQLite, via `rusqlite`) is a derived cache and nothing else. It is
deletable at any time. The daemon rebuilds it from `mail/` on startup when it is
missing or when its schema version does not match, and `hivemind reindex`
rebuilds it on demand (SPEC §4.3).

## Consequences

- A half-written message is impossible: rename is atomic, so a file either is
  not there or is complete. A crash costs at most the message being written.
- Recovery from a corrupt index is `rm index.db` and restart. No migration
  tooling, no repair mode, no support burden.
- Schema changes are free. Bumping the index schema version triggers a rebuild;
  it never touches the mail.
- Users can read, grep, back up and sync their mail with ordinary tools, and a
  future version of hivemind — or a different program entirely — can read mail
  written today.
- The cost is duplication: every write happens twice, to the file and to the
  index, and the two can disagree if we are careless. We hold this together with
  a property test (SPEC §13.2): for any sequence of mail operations, an index
  rebuilt from `mail/` must equal the index maintained live. That test is the
  thing that makes this decision safe, so it is not optional.
- Rebuild is O(number of messages) at startup in the worst case. For a mailbox
  of tens of thousands of messages this is seconds, and it only happens after a
  deletion or a version bump.
- Transactions do not span the two stores. The file write commits first and the
  index follows; a crash between them leaves the index stale, which the next
  rebuild corrects. The index is never the thing that decides whether a message
  exists.

## Alternatives considered

**SQLite as the only store.** One writer, real transactions, no duplication.
Rejected: it makes the mail hostage to one schema and one library, corruption
takes everything at once, and `~/.hivemind` stops being inspectable — which for
a tool that mediates between machines is a real loss of trust.

**Files only, no index.** Simplest possible thing, and correct. Rejected on the
100 ms hook budget (SPEC §9.3): `hivemind hook check` runs on every
`UserPromptSubmit` and cannot afford to stat and parse a directory of JSON.

**An embedded key-value store (sled, redb) as the cache.** Rejected: it buys
nothing over SQLite here and gives up full-text search, which we need for `q=`
(SPEC §7.1). See [0005](0005-rusqlite-over-turso.md).
