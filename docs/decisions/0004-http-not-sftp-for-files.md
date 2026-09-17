# 0004. Attachments move over the same HTTP/TLS channel, not SFTP

- **Status:** accepted
- **Date:** 2026-09-17

## Context

Messages carry attachments, and attachments are the part users notice when it
goes wrong: a 700 MB file whose transfer dies at 80% on a flaky office network
has to resume, not restart. The default size limit is 2 GiB (SPEC §6.3).

We already run one authenticated channel between daemons — mutual TLS on
port 8400, pinned by certificate fingerprint (SPEC §6.3). The question is
whether bulk transfer reuses it or gets its own mechanism. File transfer is a
solved problem with several off-the-shelf answers (SFTP, rsync), and reaching
for one is tempting.

## Decision

Blobs move over the peer HTTP API on the same mTLS connection as everything
else: `HEAD /peer/v1/blobs/{sha}` to ask whether the sender still has it and
`GET /peer/v1/blobs/{sha}` with byte-range support to fetch or resume it
(SPEC §7.2). Files are content-addressed by SHA-256 and stored under
`~/.hivemind/blobs/<sha256-hex>`, so identical attachments are stored and
transferred once.

Anything at or under `inline_max` (default 8 MiB) ships inside the delivery
multipart and needs no second round trip; larger files ship as a reference the
recipient fetches lazily on first access, or eagerly when `prefetch = true`
(SPEC §8).

## Consequences

- One port, one authentication mechanism, one set of firewall rules. A peer that
  can deliver mail can transfer files, with no second trust decision and no
  second thing to get wrong.
- Resume is `Range: bytes=N-`, which every HTTP stack implements and which we
  can test by killing a transfer mid-flight and asserting the retry sends a
  range request (SPEC §13.2).
- Content addressing gives integrity for free: the recipient hashes what it
  received and compares to the `sha256` in the signed `AttachmentRef`. A
  corrupted or substituted blob is detected without trusting the transport.
- Deduplication is a property of the naming scheme, not a feature we maintain.
  The same 200 MB build artifact broadcast to eight peers is stored once on each.
- We implement range handling, partial-file bookkeeping and cleanup of aborted
  transfers ourselves. SFTP would have given us that. This is the honest cost,
  and it is bounded: it is one handler and one client loop, both covered by the
  interrupted-transfer integration test.
- HTTP framing adds overhead an `rsync` delta transfer would avoid. For
  attachments — new files, not repeatedly-edited ones — there is no delta to
  exploit, so this costs us nothing real.
- No compression on the wire in v1. TLS 1.3 will not compress for us and we do
  not add it; most large attachments are already compressed.

## Alternatives considered

**SFTP or SSH-based transfer.** Battle-tested, resume included. Rejected: it
means a second listener, a second authentication system with its own key
material, and a dependency on an SSH server being present and configured. It
would also break the trust model — pairing establishes trust in a *certificate*,
and SFTP would need that trust re-expressed as authorized keys.

**BitTorrent-style multi-peer fetch.** Attractive for broadcast of large files.
Rejected as disproportionate: at team scale, sequential delivery to a handful of
peers is fast enough, and it would put peers in the position of serving blobs
they were not sent.

**Always inline, no lazy fetch.** Much simpler: no `HEAD`, no resume, no partial
state. Rejected on the 2 GiB limit — holding that in a delivery multipart, and
retrying the whole thing on failure, is exactly the behaviour users complain
about.
