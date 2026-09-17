# Wire protocol

> **Status: stub.** The canonical encoding is specified here as SPEC §4.1
> describes it; the normative version — with golden vectors — lands with the
> message type in M1 (SPEC §14).

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

## Canonical encoding

A message is signed over a deterministic CBOR encoding of its fields, so that a
message read back off disk is verifiable independently of the transport that
carried it.

- Encoder: `ciborium`, with a fixed struct field order — **not** a map keyed by
  name, and never derived from serialisation order of a `HashMap`.
- The `signature` field is excluded from the bytes being signed. Everything
  else in SPEC §4.1, in declaration order, is included.
- Signature: Ed25519 over those bytes, by the sending node's key.

This encoding is frozen by golden test vectors checked into the repository. A
change to it is a wire-compatibility break and needs an ADR and a version bump —
the golden tests exist so that it can never happen silently.

<!-- TODO(M1): golden vectors, the exact field order table, and the generated
     openapi.json alongside this file. -->

## Errors

RFC 9457 `application/problem+json`, with stable `type` slugs enumerated in one
Rust enum so the documentation cannot drift from the code.

<!-- TODO(M1): the slug table, generated from that enum. -->
