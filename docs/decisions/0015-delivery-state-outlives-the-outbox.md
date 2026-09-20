# 0015. Delivery state is per recipient and outlives the outbox

- **Status:** accepted
- **Date:** 2026-09-20
- **Amends:** SPEC §4.3, §8

## Context

A message has had exactly one state since M2, and it belongs to the whole
batch: the file sits in `mail/out/` while anybody is still missing it and moves
to `mail/sent/` when everybody has it. The sender sees `outbox: 1`, which says
that *something* is outstanding and never what or to whom.

The model already knew almost everything. `Outbound` — the envelope in `out/` —
carries a `RecipientState` per recipient with `delivered_at`, the attempt count,
the last attempt and the last error, and that per-recipient record is exactly
what decides when `out/` becomes `sent/`. Two things kept it from being
answerable:

- **It never left the worker.** Nothing above `hivemind-net` read it, so no
  API, no CLI and no web page could say which recipient was still owed a copy.
- **It was thrown away at the finish line.** `promote_to_sent` wrote the bare
  signed message into `sent/` and deleted the envelope, so the moment the last
  recipient took the message, the record of who took it and when ceased to
  exist. A message everybody has is precisely the one somebody asks about a
  week later.

Three incidents in one day of real use between three machines (#31) are the
same blindness three times: a message queued behind a peer that had not yet
admitted the sender, and two minutes spent suspecting loss; four messages
believed sent against two that arrived, noticed only by comparing lists by
hand; and seven sent of which exactly one could be *proved* to have arrived —
the one that was answered. In an application whose premise is that the other
machine may be off for days, "did it arrive?" is *the* question.

There is a second constraint. Files are the source of truth and `index.db` is a
cache that must survive being deleted (ADR 0002), so whatever answers "did it
arrive" has to be recoverable from `mail/` alone. The index schema already
records the cost of getting this wrong: its `to_nodes` column holds node
recipients only, because `everyone` and an owner name are expanded at send time
into the envelope and `sent/` kept the signed message alone — so a rebuild could
not recover the expansion.

## Decision

**Per-recipient state is `queued` → `delivered` → `read`, with the time of each
transition, and it lives in the outgoing file for as long as the message does.**

- `RecipientState` gains `read_at`, beside the `delivered_at` it already had.
  `Delivery` names the three states, derived from those two stamps rather than
  stored as a fourth field, so the timestamps and the state cannot disagree.
- **`sent/` holds an `Outbound` envelope**, exactly as `out/` does.
  `promote_to_sent` moves the envelope rather than unwrapping it, so what each
  recipient did survives the queue. `Mailbox::holds_envelopes` is the one place
  that says which boxes those are, and `MailStore::get` unwraps the envelope so
  that a caller wanting the message does not have to know.
- **`delivered` is what the far node confirms**, never what this one assumes. It
  is set when the recipient's daemon answers `202` to `POST /peer/v1/messages`
  and at no other moment.
- **Nothing per-recipient enters the index.** It is read from the envelope in
  `out/` or `sent/`, which is one small file read per message and is what makes
  a rebuild from `mail/` lossless without a column for the live path to fill.

A `sent/` file written before this change is a bare message. Reading one falls
back to parsing it as a `Message` and reports an envelope with no recipients —
nothing claimed about who took it, which is the honest answer for a file that
never said. The fallback is tried second, so a genuinely unreadable file stays
`Corrupt` rather than becoming "nobody has it".

## Consequences

- **"Did it arrive, and to whom" has an answer**, per message, for as long as
  the message is kept, and the answer is the far node's assertion rather than
  this one's. That is the half of #31 that needs no new protocol and no
  configuration: it was already being computed.
- **`sent/` is no longer readable as a message with `jq .subject`.** Somebody
  reading `~/.hivemind/mail` by hand (ADR 0002 invites them to) now needs
  `jq .message.subject` for the two outgoing boxes. That is a real cost, paid
  for consistency: `out/` has always been an envelope, and two shapes in two
  sibling directories is worse than one shape in both.
- **`put` and `move_to` refuse both outgoing boxes**, where they previously
  refused only `out/`. That is a compile-time error at every call site rather
  than a file nothing can read back, which is how the same mistake in `out/`
  was found (#43): a bare message written there made the sender's own message
  unreadable, and only a listing noticed.
- **The index's `to_nodes` caveat could now be lifted**, because the expansion
  in `sent/` is recoverable again. It is not lifted here — the conversation
  list's notion of who a conversation is with would change with it, and that is
  a separate decision — and the schema comment says so rather than going stale.
- **One file read per outgoing row in a listing.** A page of `hivemind sent` is
  fifty small reads from the page cache, which is what the index was avoiding.
  Measured against the alternative of three columns the rebuild would have to
  fill: the reads cost microseconds and the columns cost a class of bug ADR
  0002 exists to prevent.
- `read_at` is `#[serde(default)]`, so an envelope written by an older build
  reads back as one nobody has told us about.

## Alternatives considered

**Three columns in `index.db` — `recipients`, `delivered`, `read`.** One query
answers a whole listing, which is what an index is for. Lost on ADR 0002: the
columns would be filled by the live delivery path and would have to be
reconstructed by `rebuild_from`, which can only read them back out of the
envelope — so the envelope has to be in `sent/` anyway, and then the columns are
a second copy of a fact, in the place that is allowed to be deleted. The
rebuild property test would have to grow a comparison for each of them.

**A fifth directory, `mail/receipts/<ulid>.json`, holding the state beside the
message.** Keeps `sent/` readable as a plain message. Lost because two files
per message is two writes to keep atomic with respect to each other, and the
state SPEC §4.3 already says lives "inside the file" would then live inside a
different file.

**Leaving the message in `out/` forever and marking it complete there.** No
promotion, no second shape, and `sent/` becomes a view rather than a directory.
Lost because `out/` is what the delivery worker walks on every tick: a year of
delivered mail in it turns an O(outstanding) loop into an O(everything) loop,
and SPEC §4.3 names four directories with `sent/` among them.

**Keeping the state only in memory in the worker.** What the code did. It
cannot survive a restart, which is the case the whole feature is for: a peer
that is off for days outlives every daemon process on this side.
