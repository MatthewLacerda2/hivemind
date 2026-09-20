# Wire protocol

The canonical message encoding and the group proof below are **normative and
frozen**.

## Listeners

| Listener | Bind | Auth | Purpose |
|---|---|---|---|
| peer | `0.0.0.0:8400` | mutual TLS, peer must have proved the group key | daemon ↔ daemon delivery, read receipts, blob transfer, handshake |
| local | `127.0.0.1:8401` | none (loopback only) | humans, CLI, web UI, MCP |

## Peer endpoints

```
POST /peer/v1/handshake        exchange name/owner/version/id + proof of the group key
POST /peer/v1/hello            presence: fresh proof, addresses, peer list, sessions; answered in kind
POST /peer/v1/messages         deliver one signed message (+ inline blobs as multipart); idempotent on id
POST /peer/v1/receipts         "I have read these messages of yours"; members only, idempotent
HEAD /peer/v1/blobs/{sha}      does the sender still have it
GET  /peer/v1/blobs/{sha}      range requests supported (resume)
```

Delivery is push. Recipients never poll senders for mail — only for lazy blobs.

### `POST /peer/v1/handshake`

Both sides send the same shape. The certificate presented in the TLS handshake
is the identity; `id` is a claim, and a mismatch between the two is rejected
rather than resolved in either direction.

```json
{
  "id": "hm1:w2mq-xor2-…",
  "name": "laptop",
  "owner": "matheus",
  "version": "0.1.0",
  "callback_host": "10.0.0.5",
  "callback_port": 8400,
  "proof": { "sent_at": 1750000000000, "mac": "9631…781f" }
}
```

`callback_host` and `callback_port` are where the sender can be reached. The
receiver records the port, and the host only where the connection's own source
address does not contradict it (#23).

`proof` is the sender's proof of the group key, made for the receiver's
certificate — see [Group proof](#group-proof). It is absent when the sender is
in no group. The receiver checks it against the certificate the connection
presented:

- **It verifies** → the receiver pins the sender's certificate in `peers.toml`
  and answers `200` with its own handshake, whose `proof` is made for the
  sender. The sender checks that one the same way and pins the receiver. Both
  are now peers; nobody was asked anything (ADR 0013). A sender already pinned
  has its `name` and `owner` taken again rather than kept from the first
  meeting — a machine renamed by hand would otherwise read everywhere else
  under the name it had then (#37).
- **It is missing or does not verify**, or the receiver is in no group → `403
  not_paired`, with a `detail` saying which. The receiver remembers the sender
  as *seen*, in memory, so `hivemind peers` can show it.

`gossip` carries the sender's peer list, in the same shape the hello's `peers`
field uses. It is here as well as on the hello so that a node admitted this
second has the group now rather than at the next presence round.

The endpoint is open to anyone who completes a TLS handshake — see
`decisions/0010-tls-admits-strangers-the-application-rejects-them.md`. It
reveals nothing about the mailbox. Everything else requires a peer that has
proved the key, and answers `403 not_paired` otherwise.

### `POST /peer/v1/hello`

Presence (SPEC §5.5). Sent to every known peer on start, on wake, on a change
of network, and every `presence_interval` — sixty seconds by default. One
request each, none held open. The answer is a hello of the same shape, so a
single request tells both sides about the other.

```json
{
  "id": "hm1:w2mq-xor2-…",
  "name": "laptop",
  "owner": "matheus",
  "version": "0.1.0",
  "callback_host": "10.0.0.5",
  "callback_port": 8400,
  "proof": { "sent_at": 1750000000000, "mac": "9631…781f" },
  "peers": [
    {
      "id": "hm1:k4tp-9nc1-…",
      "name": "desktop",
      "owner": "ana",
      "addrs": ["10.0.0.7:8400", "desktop.tail1234.ts.net:8400"]
    }
  ],
  "sessions": [{ "id": "01JXT…", "label": "hivemind" }],
  "up": ["hm1:k4tp-9nc1-…"]
}
```

Unlike the handshake this is **not** open to strangers. `proof` is checked
afresh on every hello, against the current group key:

- **It verifies** → the sender is marked online, its addresses are recorded,
  the `name` and `owner` it reports are taken, its peer list is taken in, and
  its queued mail is sent now rather than at the end of its backoff. The answer
  is read the same way, so whichever direction of a pair works is the one both
  sides learn a new name from.
- **It is missing or does not verify** → `403 not_paired`, *and the sender is
  removed from* `peers.toml`. This is what gives `hivemind group create
  --replace` an effect on a node that is already pinned: a proof offered once
  at pairing time can never be withdrawn. SPEC §6.2.4 says a member whose key
  was rotated away stops receiving mail; it is listed as seen, and returns by
  the ordinary path if it pastes the new code.

`peers` is the gossip SPEC §5.4 describes. Addresses learned from it are
recorded with `source: "gossip"` and tried like any other. **Nothing in it is
believed beyond "try this address":** the certificate is what gets pinned and
the group key is what admits, so a member that lies here costs one connection
attempt. A node named in it that is not yet a peer is greeted like one seen by
mDNS — with a handshake, which the key decides.

`sessions` is what each open Claude Code session on the sending machine is
working on (SPEC §9.3). Presence, not address: mail goes to the node and any
session may read it.

`up` is a hint — nodes the sender has just heard from. It is **never
believed**: the receiver sends its own hello to each and marks it online on
the answer, or not at all. A hint costs one request and saves a peer an
interval of looking absent to everyone but the one node it happened to reach
first. Hints expire after one interval rather than being passed on forever,
and are never sent back to the node they are about.

A peer counts as online while its last hello is younger than **two** intervals
— one missed round is a dropped packet, two is a machine that has gone. A
failed delivery marks it offline at once, which is better evidence and more
recent than any hello. There is no ping.

### `POST /peer/v1/messages`

`multipart/form-data`. The first part is named `message` and carries the signed
message as JSON. Each further part is one inline attachment, named by its
SHA-256 in lowercase hex:

```
--boundary
Content-Disposition: form-data; name="message"
Content-Type: application/json

{"id":"01JXT2…","from":"hm1:…","attachments":[…],"signature":"…"}
--boundary
Content-Disposition: form-data; name="ab5aa970…"; filename="ab5aa970…"
Content-Type: text/markdown

# notes
--boundary--
```

A part is accepted only if the message declares an inline attachment with that
digest, **and** the bytes hash to it. Without the first check a paired peer
could write arbitrary files into the recipient's blob store; without the second
it could substitute different content for something it did declare.

Answers `202 Accepted` with `{"id": "<the message id>"}`. Idempotent: a
redelivery of something already held is a success and does not reset its read
state.

The recipient bounds the request at its own inline budget plus the message. A
sender configured more generously gets `413`.

### `POST /peer/v1/receipts`

`application/json`. A batch, because reads come in bursts — opening a
conversation marks every unread message in it — and a handshake per message for
a fact this size is the wrong trade:

```json
{
  "read": [
    { "id": "01JXT21Q00041061050R3GG28A", "read_at": "2026-09-20T10:14:00Z" }
  ]
}
```

`id` is the **sender's** id for the message, which is the only id either side
has for it. `read_at` is the reader's clock, as `sent_at` is the sender's
(SPEC §4.1); nothing is compared against it.

Answers `202 Accepted` with `{"recorded": <n>}`, counting the notes that named a
message the receiver had sent **to this caller**. Fewer than were offered is not
an error: the sender may have deleted the message, and a receipt for one it no
longer holds is a fact with nowhere to go rather than a failure to report.
Refusing it would make the sender's courier retry it for ever.

**The reader is the connection, never the body.** A note marks the delivery
entry for the authenticated caller and no other, so a member can only report on
its own reading. A caller that has not proved the group key gets `403
not_paired`, as delivery does — a receipt writes to the mailbox.

Idempotent: a note for something already recorded is a success and changes
nothing, and the time kept is the first one. Nothing runs because a receipt
arrived (SPEC §12).

Read receipts are **off by default** and sent only when the sending node has
`read_receipts = true` (ADR 0016). A node with them off never posts here;
receiving is not configurable.

### `HEAD /peer/v1/blobs/{sha}`

`200` with `Content-Length` if the sender still holds it, `404` otherwise. A
recipient asks before resuming, so a sender that deleted the file gives an
answer rather than a stalled download.

### `GET /peer/v1/blobs/{sha}`

Streams the blob. `Range: bytes=N-` resumes from byte `N` and is answered with
`206` and `Content-Range: bytes N-M/total`. Only that form is honoured — it is
the one resuming needs; any other range gets the whole blob.

A resume point past the end answers `416` with `Content-Range: bytes */total`,
which tells the caller to start over rather than leaving it waiting for bytes
that are not coming.

A client that asked to resume and received `200` must treat it as a failure:
the server started from zero, and appending that to what is already held would
corrupt the file.

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

## Group proof

Membership in the group is possession of its key (SPEC §6.2, ADR 0013). The key
is never sent; a node proves it has it.

### The code

The key is 16 random bytes. People see it as a **code**: `hm-`, then the bytes
in lowercase unpadded RFC 4648 base32 — the alphabet the node id uses — in
groups of four hyphen-separated characters. 128 bits is 26 characters, so the
last group has two:

```
hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4
```

A parser ignores case, the `hm-` prefix, hyphens and whitespace, and refuses
anything that is not exactly 128 bits with the unused low bits of the last
character zero, so each key has exactly one spelling.

### The proof

```
message = "hivemind group proof v1" 0x00
        ‖ u32be(len(sender_cert))   ‖ sender_cert
        ‖ u32be(len(receiver_cert)) ‖ receiver_cert
        ‖ i64be(sent_at)
proof   = HMAC-SHA256(key, message)
```

`sender_cert` and `receiver_cert` are the DER certificates the two nodes
present in TLS; `sent_at` is milliseconds since the Unix epoch. On the wire the
proof travels as

```json
{ "sent_at": 1750000000000, "mac": "<64 lowercase hex characters>" }
```

The receiver checks it against the certificate the connection actually
presented, never one the body names, so a proof is worth nothing between any
other two nodes. It accepts a `sent_at` up to an hour either side of its own
clock — the binding to both certificates already stops replay; the window only
bounds how long a proof that leaked into a log stays meaningful, and a laptop
whose clock drifted while it slept should still get in.

### Golden vector

Frozen in `crates/hivemind-core/src/group.rs`, and independently reproducible
with `docs/reference/group_proof_reference.py` (standard library only), which is
written from this section.

```
key            00 01 02 … 0f
code           hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4
sender_cert    "sender certificate"   (ASCII)
receiver_cert  "receiver certificate" (ASCII)
sent_at        1750000000000
proof          96317562f141081774fd826ac79b205f4f6afbdc112ab3b4106a51cf907a781f
```

---

## Errors

RFC 9457 `application/problem+json`, with stable `type` slugs enumerated in one
Rust enum so the documentation cannot drift from the code.

| `type` | Meaning | Status |
|---|---|---|
| `/problems/message-not-found` | No such message | 404 |
| `/problems/ambiguous-id` | Id matches more than one message | 422 |
| `/problems/invalid-query` | Query is not one this endpoint understands | 400 |
| `/problems/invalid-message` | Message is not acceptable | 422 |
| `/problems/no-recipients` | Message has no recipients | 422 |
| `/problems/not-paired` | Not paired | 403 |
| `/problems/already-in-group` | Already in a group | 409 |
| `/problems/invalid-group-code` | Not a group code | 422 |
| `/problems/identity-mismatch` | Identity does not match the certificate | 400 |
| `/problems/bad-signature` | Signature does not verify | 400 |
| `/problems/peer-unreachable` | Peer could not be reached | 502 |
| `/problems/blob-not-found` | No such attachment | 404 |
| `/problems/blob-too-large` | Attachment is too large | 413 |
| `/problems/unsafe-attachment-name` | Attachment name is not a file name | 422 |
| `/problems/addr-not-found` | No such address | 404 |
| `/problems/internal` | Internal error | 500 |

A test in `hivemind-api` reads this file and fails if a `ProblemType` variant
is missing from it, so a new failure cannot reach clients undocumented.

`docs/openapi.json` carries the same list, generated from the same enum.
