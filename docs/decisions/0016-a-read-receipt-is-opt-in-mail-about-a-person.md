# 0016. A read receipt is opt-in mail about a person

- **Status:** accepted
- **Date:** 2026-09-20
- **Amends:** SPEC §4.3, §7.2, §8, §10; extends
  [0015](0015-delivery-state-outlives-the-outbox.md)

## Context

ADR 0015 made `queued` → `delivered` per recipient answerable, and `delivered`
comes for free: the recipient's daemon already answers `202` to
`POST /peer/v1/messages`, so it is a fact this node is told rather than one it
assumes. `read` is not free. Nothing today tells a sender that a person opened
their message, and nothing on the wire could carry it.

Two constraints shape what that can be.

**The first is that it is not the same kind of fact.** That a node accepted the
bytes is information about a daemon; that somebody sat down and opened the
message is information about a person, and it is exactly what people turn off
in other messengers. hivemind's premise is machines belonging to different
people on a shared network (ADR 0003), so the person whose reading is being
described is not always the person who installed the daemon that would report
it.

**The second is that the other machine may be off for days.** That is the
premise of the whole application (SPEC §8), and it applies to the node owed a
receipt as much as to the node owed a message. A receipt attempted once and
dropped is a fact its sender never learns, which is the same blindness #31 was
filed about, one level down.

There is also a scope line to respect. SPEC §12 reserves anything that runs an
agent because mail arrived. A receipt is mail carrying a fact — the far node
writes a field and answers — and nothing is scheduled, spawned or decided by
its arrival.

## Decision

**`delivered` is always on. `read` receipts are off by default and turned on
per node with `read_receipts = true` in `config.toml`.**

The switch governs what **leaves** this machine. A receipt that arrives here is
always recorded, because that is a choice the other person has already made and
declining to write it down would only make this node's own display wrong.

The mechanism is the delivery queue's smaller sibling:

- Marking a message read here — by `hivemind read`, `hivemind thread`, the web
  UI or the MCP `read` tool, all of which go through one service call — queues
  a receipt for the node that sent it, once, on the `new` → `cur` transition.
- A receipt is one small JSON file under `~/.hivemind/receipts/<ulid>.json`,
  carrying the message id, the node owed it, when it was read here, and the
  attempt counters. Files are the source of truth (ADR 0002), and a queue held
  in memory cannot outlive the process, which is the case the feature is for.
- A courier walks the queue, **batches by peer** and posts
  `POST /peer/v1/receipts` over the same mutual TLS delivery uses, with the
  same jittered 2 s–5 min backoff, forever. A batch that is accepted is
  deleted; a batch that is not counts one attempt against each receipt in it,
  written back, so a restart resumes the backoff rather than hammering a peer
  that is already waiting.
- The receiving node records each note against the envelope in `out/` or
  `sent/`, **only ever against the entry for the authenticated caller**. The
  reader is the connection, never anything in the body: a node that could
  nominate the reader could report on somebody else's reading, which is the one
  thing this must not allow.
- A note naming a message this node does not hold, or one that was never
  addressed to the caller, is counted out and answered `202`. Refusing it would
  make the courier retry a fact with nowhere to go for ever.

`read_at` is the reader's clock, as `sent_at` is the sender's (SPEC §4.1).
Nothing compares it against anything; it is shown beside the recipient's name.

A receipt is also **proof of delivery**: recording one stamps `delivered_at` if
nothing else has, because a recipient cannot read what never reached them and
this node's own record of the delivery can be lost between the far node's `202`
and the write that follows it.

## Consequences

- **The default is the recoverable direction.** Somebody who wants receipts
  switches them on; somebody who did not want them has already been reported
  on. It also keeps the feature honest about what it promises: `delivered` is a
  fact the recipient's node asserts, `read` is a courtesy the far end chooses to
  extend, and the interface says which it is showing.
- **A message can read `✓✓ read by 1 of 2` for ever**, because the second
  machine's owner has receipts off. That is not a bug and the counts say so: the
  alternative is an interface that cannot distinguish "not read" from "not
  telling", and the first is the one people would assume.
- **A second retry loop.** Separate from delivery on purpose — what it carries
  is not mail, cannot be redelivered as a message, and one loop doing both would
  have to say which of the two every failure belonged to. The backoff is shared
  rather than written twice, which is the part that would otherwise drift.
- **A receipt for a peer that never accepts is retried for ever**, exactly as a
  message for one is. A node removed from the group answers `403` until somebody
  acts, and "forever" is the right interval for that (SPEC §8). It costs one
  request per peer per five minutes and one small file.
- **`~/.hivemind/receipts/` is a fifth directory** beside `mail/`, not inside
  it. Nothing in it carries a message, and a directory under `mail/` would join
  every walk of the mailboxes for the sake of sharing a parent.
- **A new wire endpoint.** `docs/protocol.md` specifies it and the `not_paired`
  problem type covers its one refusal, so no new slug and no new golden vector:
  the body is ordinary JSON with no signature over it, exactly as
  `/peer/v1/hello`'s peer list is.
- **Turning the default on later is a configuration line**, not a redesign. All
  of the mechanism is built either way.
- **Nothing runs because a receipt arrived.** A field is written and `202` is
  answered, which keeps SPEC §12 intact.

## Alternatives considered

**On by default.** What a chat client does, and what somebody comparing
hivemind to one would expect. Lost on the asymmetry of the mistake: a default
that quietly reports when a colleague read something is the kind of thing nobody
notices until they mind, and by then it has already happened. Switching it on is
a line of TOML.

**Piggyback the receipt on the presence hello** (SPEC §5.5), which already runs
every minute in both directions and already carries facts nobody signs. No new
endpoint, no queue, no retry loop. Lost on bounding it: the list of "messages of
yours I have read" grows without limit unless the sender acknowledges, and every
way of bounding it needs either an acknowledgement protocol on the hello or an
arbitrary window constant after which receipts are silently dropped. The
constant is the thing that would be wrong, and the acknowledgement is the queue
again with the hello wrapped round it.

**Best-effort: one attempt, no queue.** Much less code, and "a courtesy the far
end chooses to extend" arguably licenses it. Lost because the one case that
matters is the one it fails: the sender's laptop is shut, which is the whole
premise, and a receipt that is dropped then is never sent. The queue is one
directory and one loop.

**A `read_at` field on `Message`, stamped by the recipient and read back by the
sender.** There is no path for it: the sender has its own copy and never
receives the recipient's, so the field would have to travel — which is this
decision with the receipt hidden inside a message shape.

**Per-peer configuration — receipts for some machines and not others.** The
honest generalisation, and the shape a team would eventually want. Lost on
scope: there is nothing per-peer in `config.toml` today, and inventing a
per-peer settings table for one flag would be the design decision rather than a
consequence of one. A node-wide switch is the smallest thing that gives the
choice to the person it belongs to.
