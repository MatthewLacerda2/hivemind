# 0007. `received_at` is not covered by the signature

- **Status:** accepted
- **Date:** 2026-09-17

## Context

SPEC §4.1 lists the fields of a message and describes the signature as:

> `signature: Signature,  // Ed25519 over the canonical encoding of all fields above`

Taken literally, "all fields above" includes `received_at`. But `received_at` is
described in the same struct as the time *this node* received the message, and
it is `None` on the sender's side by construction — there is nothing to record
yet when the message is written to `mail/out/`.

That makes the literal reading self-defeating. The sender signs with
`received_at: None`. The recipient verifies, stamps `received_at: Some(now)`,
and writes the message to `mail/new/`. From that moment the signature no longer
matches the bytes on disk.

This directly contradicts two other requirements:

- SPEC §4.1: the message is signed "so a stored message is verifiable
  independent of the transport".
- SPEC §6.3: "Every message is additionally signed so that a stored message is
  verifiable independent of the transport."

A signature that is valid only during the instant of delivery is a transport
checksum, not a message signature, and it would make `hivemind reindex` — which
rebuilds from `mail/` — unable to re-verify anything it reads.

## Decision

The canonical encoding covers the eleven sender-authored fields, in the
declaration order of `Message`:

`id`, `thread_id`, `in_reply_to`, `from`, `to`, `subject`, `body`, `kind`,
`sender_kind`, `attachments`, `sent_at`.

`received_at` and `signature` are excluded. SPEC §4.1's comment should read
"over the canonical encoding of the sender-authored fields"; the normative list
lives in `docs/protocol.md`.

## Consequences

- A message read back off disk verifies, months later, with no special handling
  and no need to reconstruct what it looked like in flight. This is the property
  the spec asked for and the literal reading would have removed.
- `received_at` is unauthenticated. It is local bookkeeping — "when did *my*
  daemon see this" — and it is never transmitted between nodes, so there is no
  claim for an attacker to forge. A peer cannot influence it at all.
- Excluding `signature` from the bytes it signs is not a decision so much as
  arithmetic, but it is written down here because it is the other half of the
  same question and a reader will ask.
- The exclusion is enforced structurally rather than by comment. The encoder
  destructures `Message` exhaustively, so adding a field to the struct fails to
  compile until somebody states whether it is signed. There are also two tests
  that would fail if either exclusion were dropped.
- `sent_at` is encoded as integer milliseconds since the epoch, not as the
  RFC 3339 string used in the JSON on disk. Two implementations have to agree on
  the signed bytes exactly, and "the shortest representation that round-trips"
  is not a property to rest a signature on. `sent_at` therefore carries
  millisecond precision, which matches the resolution of the ULID in `id`.

## Alternatives considered

**Sign `received_at` as `None` always, and strip it before verifying.** Keeps
the spec's field list literally intact. Rejected: it means the verifier
reconstructs a message that differs from the one in front of it, which is
exactly the kind of "normalise, then check" step that signature bugs live in.

**Keep `received_at` out of `Message` entirely, in a sidecar file.** Arguably
cleaner, since it is not part of the message. Rejected: it splits one message
across two files and breaks the "one JSON file per message" property that
[0002](0002-files-are-source-of-truth.md) relies on.

**Have the recipient re-sign on receipt.** Rejected immediately: the recipient
does not have, and must never have, the sender's key, and a recipient signature
would prove nothing about authorship.
