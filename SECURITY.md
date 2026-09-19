# Security

## Reporting a vulnerability

Please report privately through
[GitHub's advisory form](https://github.com/MatthewLacerda2/hivemind/security/advisories/new)
rather than opening an issue.

Include what an attacker can do, not just what is wrong. We will acknowledge
within a few days and keep you updated; if you want credit in the advisory, say
so and we will include it.

## Threat model

hivemind carries mail between machines on a LAN or tailnet. Being explicit about
what that does and does not protect:

### What hivemind defends against

- **A machine on your network that is not in your group.** Discovery tells
  peers that a node exists; it never grants trust. A node that cannot prove
  the group key — in the handshake and in every hello afterwards — is
  rejected with `403 not_paired`, and the key is 128 random bits, so a proof
  captured off the wire cannot be brute-forced (ADR 0013).
- **Interception or tampering in transit.** All peer traffic is TLS 1.3 with
  mutual authentication. Certificates are pinned by SHA-256 fingerprint against
  `peers.toml` — there is no CA to mis-issue and no hostname to spoof.
- **Tampering at rest, and a compromised transport.** Every message is signed
  with the sender's Ed25519 key over a canonical CBOR encoding, independently of
  the channel that carried it. A message read back off disk is still verifiable.
- **Corrupted or substituted attachments.** Blobs are content-addressed; the
  recipient hashes what arrived and checks it against the hash in the signed
  message.
- **Exposure of the unauthenticated local API.** The loopback listener on
  `127.0.0.1:8401` has no authentication, so it refuses to bind to any other
  address even if configured to. It fails closed.
- **Path traversal through attachment names.** Names are validated; blobs are
  stored by hash, never by the name the sender chose.

### What it does not defend against

- **A member of your group.** The group is as trustworthy as its least
  careful member: anyone holding the key can admit any machine, and a member
  can send you anything and claim any `owner` label — `owner` is a free-text
  convenience for fan-out and is never a security boundary. This is the
  stated model (ADR 0013): the people it is for already share a network and a
  repository. The response to a member you no longer trust, or a machine you
  have lost, is to rotate the key — `hivemind group create` — and paste the
  new code on the machines that stay.
- **Anyone with access to your user account.** Mail, blobs and your private key
  live under `~/.hivemind/` with the key at mode `0600`. They are not encrypted
  at rest: a process running as you can read your mail, as it can read your SSH
  keys.
- **Traffic analysis.** Sizes and timings of deliveries are visible to anyone
  who can watch the network, and mDNS advertises your node's name and
  fingerprint to the local segment by design.
- **A group key that has leaked.** It lives in `~/.hivemind/group.toml` at
  mode `0600`, beside the private key and the mail; a process running as you,
  or whoever has your laptop, has it. Until the group rotates, it admits new
  machines. Rotation is the remedy, and it is one command plus a paste.
- **Malicious content inside a message.** hivemind delivers what it is given. A
  message body is untrusted input, including — especially — when a Claude reads
  it. Treat mail from another machine the way you would treat any other external
  content.

### Not in v1

There is no autoreply: hivemind never executes anything on receipt. Unattended
`claude -p` execution is reserved for v2 precisely because it needs a per-peer
allow-list, tool restrictions and quota awareness before it would be responsible
to ship (SPEC.md §12).

## Supported versions

Pre-1.0: only the latest release gets fixes.
