# Wire protocol

The canonical message encoding below is **normative and frozen**. Everything
else on this page is a stub until the router lands later in M1.

## Listeners

| Listener | Bind | Auth | Purpose |
|---|---|---|---|
| peer | `0.0.0.0:8400` | mutual TLS, peer must be paired | daemon ↔ daemon delivery, blob transfer, handshake |
| local | `127.0.0.1:8401` | none (loopback only) | humans, CLI, web UI, MCP |

## Peer endpoints

```
POST /peer/v1/handshake        exchange name/owner/version/id; reserved `gossip` field
POST /peer/v1/messages         deliver one signed message (+ inline blobs as multipart); idempotent on id
HEAD /peer/v1/blobs/{sha}      does the sender still have it
GET  /peer/v1/blobs/{sha}      range requests supported (resume)
```

Delivery is push. Recipients never poll senders for mail — only for lazy blobs.

<!-- TODO(M3): request and response bodies, once the peer router exists. -->

---

## Canonical message encoding

A message is signed over a deterministic CBOR encoding of its sender-authored
fields, so that a message read back off disk is verifiable independently of the
transport that carried it (SPEC §4.1, §6.3).

### Shape

A CBOR **map** with 11 text keys, written in the order below. This is the
declaration order of `Message`, *not* RFC 8949 canonical map ordering — the
order is fixed by this table, and a decoder must not reorder it before
verifying.

| # | Key | CBOR type | Notes |
|---|---|---|---|
| 1 | `id` | text (26) | ULID, Crockford base32 |
| 2 | `thread_id` | text (26) | ULID; equal to `id` for a thread root |
| 3 | `in_reply_to` | text (26) or null | |
| 4 | `from` | bytes (32) | sender's `NodeId` |
| 5 | `to` | array | see **Recipients** |
| 6 | `subject` | text | ≤ 200 **characters** |
| 7 | `body` | text | Markdown, ≤ 1 MiB in **bytes** |
| 8 | `kind` | text | `message` \| `task` \| `notification` |
| 9 | `sender_kind` | text | `human` \| `agent` |
| 10 | `attachments` | array | see **Attachments** |
| 11 | `sent_at` | uint | **milliseconds** since the Unix epoch |

**Excluded from the signed bytes:** `received_at` and `signature`. See
[ADR 0007](decisions/0007-received-at-is-not-signed.md) — briefly, `received_at`
is stamped by the recipient after verifying, so signing it would mean a stored
message could never be verified again.

**`sent_at` is milliseconds, not RFC 3339.** The JSON on disk uses RFC 3339
because a human reads it; the signed encoding uses an integer because two
independent implementations must agree on the bytes exactly. Messages therefore
carry millisecond precision, matching the ULID in `id`.

### Recipients

Each element of `to` is one of:

| Variant | Encoding |
|---|---|
| a specific node | map(1) `{"node": bytes(32)}` |
| every machine of one owner | map(1) `{"owner": text}` |
| every paired machine | text `"everyone"` |

`owner` is unauthenticated free text and is never a security boundary
([ADR 0003](decisions/0003-identity-is-per-machine.md)). `owner` and `everyone`
are expanded to concrete nodes at send time and the expansion is stored
(SPEC §8).

### Attachments

Each element of `attachments` is a map(5), keys in this order:

| # | Key | CBOR type |
|---|---|---|
| 1 | `name` | text |
| 2 | `size` | uint |
| 3 | `sha256` | bytes (32) |
| 4 | `mime` | text |
| 5 | `inline` | bool |

`name` must be a plain file name: not empty, not `.` or `..`, containing no `/`,
no `\`, and no control characters. The recipient chooses the directory, so the
sender does not get to supply a path (SPEC §6.3).

### Signature

Ed25519 over exactly those bytes, by the sending node's key. Ed25519 signatures
are deterministic, so signing the same message with the same key twice gives the
same 64 bytes.

Verification needs the sender's public key, which comes from the peer book — the
`NodeId` is a certificate fingerprint, not a key, so it cannot be verified
against on its own.

### Golden vector

Frozen in `crates/hivemind-core/src/message.rs` as `GOLDEN_HEX`, and
independently reproducible with `docs/reference/canonical_reference.py`, which
is written from *this page* rather than from the Rust. The two derivations are
only worth having while they stay independent: if they disagree, the bug is in
whichever one stopped matching this table.

```
message id   01JXT21Q00041061050R3GG28A
length       402 bytes
sha256       c286597dacb56f2281ca4858d9595b50c9e7e7737835cd5da4f4d59985e08ab7
```

**Changing any of this is a wire-compatibility break.** It needs an ADR and a
version bump. The golden vector exists so it cannot happen quietly; if it fails,
the fix is almost never to update the vector.

---

## Errors

RFC 9457 `application/problem+json`, with stable `type` slugs enumerated in one
Rust enum so the documentation cannot drift from the code.

<!-- TODO(M1): the slug table, generated from that enum. -->
