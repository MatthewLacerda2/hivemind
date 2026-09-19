# 0013. Membership is possession of a group key

- **Status:** accepted
- **Date:** 2026-09-19
- **Amends:** SPEC §5.3, §5.4, §6.2, §7, §10, §12; the pairing consequence of
  [0003](0003-identity-is-per-machine.md)

## Context

SPEC §6.2 pairs machines one pair at a time: both users confirm the other's
fingerprint by hand, and ADR 0003 accepts N·(N−1)/2 confirmations as the cost of
per-machine identity. `--trust-network` is the escape hatch, and it trusts the
network segment rather than any group of people.

The first real use — three nodes, two of them the same person's — made the
requirement sharper than the spec had it:

- **There is one group.** Pairing with any member makes a node a member, and
  every member sees every other member. Two disjoint groups on one node is a
  feature for later, and it must not need a migration.
- **A node that comes online is visible to everyone**, after sleep, reboot or a
  change of network, with no step by a human after the first one.
- **Setup is one command per machine**, and nothing afterwards.

Per-pair trust cannot give the first property: a node paired with B is
invisible to C until somebody runs `join` and `pair` on both C and the node,
and that is true again for every machine that joins later. `--trust-network`
gives it by trusting whoever is on the LAN, which is the wrong thing to trust
and does not reach a tailnet at all.

What the requirement describes is a *group* with a *membership test*, and the
simplest membership test there is: knowing a secret.

## Decision

A group is identified by a **group key**: 128 random bits, generated once by
`hivemind group create`, shown to the human as a code they can paste, and
stored in `~/.hivemind/group.toml`. `hivemind pair <code>` stores the same
key. That is the whole of setup.

Trust follows from the key, not from a human confirming a fingerprint:

- **First contact.** When two nodes meet — by mDNS, by a Tailscale probe, by an
  address gossiped from a member, or by `hivemind join <host>` — each side's
  handshake carries a proof of possession: an HMAC under the group key over
  both certificates and the time. Both proofs verify → each side pins the
  other's certificate in `peers.toml` and the two are paired, with nobody
  asked anything. A proof that does not verify is `403 not_paired`, and the
  node shows in `hivemind peers` as seen but not in the group.
- **Every hello** (SPEC §5.5) carries a fresh proof under the *current* key. A
  member is therefore a node that keeps proving it, not a row that was written
  once.
- **Revocation is rotation.** `hivemind group create` on any member makes a
  new key; the humans paste it on the machines that stay. A node holding the
  old key fails its next hello and is dropped from delivery. There is no
  per-member revocation in v1; a signed revocation that propagates like gossip
  is the obvious v2 shape and the wire has room for it.
- **One group per node.** `pair` with a different code than the one held
  refuses and says why; `--replace` switches. Nothing else in the system
  carries a group id — not a peer, not a message — so a second group later is
  a field on `group.toml` and an assignment at index rebuild, not a migration.

Outbound pinning (SPEC §6.3) and ADR 0010 are unchanged: TLS still admits any
well-formed certificate inbound, the handshake is still the one open endpoint,
and the application still decides. What changes is what the application asks:
"does this node prove the key" instead of "did a human say yes".

The exact bytes under the HMAC are specified in `docs/protocol.md` when the
handshake is implemented, with a golden vector, as the message encoding has.

## Consequences

- **Onboarding is `hivemind init` and `hivemind pair <code>`.** No fingerprint
  to read out, no `y/N` on both sides, no second command when the third
  machine arrives. ADR 0003's N·(N−1)/2 becomes N.
- **Any member reaches any member.** Meeting anywhere — a LAN, a tailnet, a
  gossiped address — is enough. `peers.toml` becomes what its name says: an
  address book with certificate pins, not a list of decisions.
- **Retired:** `pending_pairs`, `hivemind pair <short-id>`, the `y/N` prompt,
  `--trust-network`, the `pair.pending` event, and the pair/confirm buttons in
  the web UI. `hivemind join <host>` stays for the case where discovery cannot
  find somebody, but it no longer asks the user to confirm anything: the key
  decides, as it does everywhere else.
- **The group is as trustworthy as its least careful member.** Anyone holding
  the key can admit any machine, and nothing distinguishes a member who was
  vouched for from one who was not. This is the stated model, in
  `SECURITY.md`, and it replaces per-peer TOFU deliberately: the people this
  is for are a team who already share a network and a repository.
- **The key is on disk.** A stolen laptop holds the group key as well as its
  own identity and all of its mail, so the response to a lost machine is
  rotation, not removal. The laptop already held everything that mattered; the
  key adds "can admit new machines" to what it can do until the group rotates.
- **Rotation is a paste on every remaining machine**, by a human. At team
  scale that is minutes; at any larger scale it is the reason a signed
  revocation exists as the v2 path.
- **The key does not find anybody.** Discovery is unchanged and still needed:
  mDNS on the LAN, a probe on a tailnet, gossip from a member, or an address
  typed into `join`. The key answers "may we talk", never "where are you".
- **The code is generated, never chosen.** 128 random bits are what make an
  HMAC sufficient: a proof captured by a stranger on the LAN — and the joiner
  probes every node it discovers — cannot be brute-forced offline. A code a
  human picks would need a PAKE instead (below).
- `not_paired` keeps its slug and its row in the problem table. Its meaning
  narrows to "not in the group", which is what it always rejected.

## Alternatives considered

**Per-pair TOFU, as specified.** Lost on the first requirement: a node paired
with one member is invisible to the others, and every later machine repeats
the cost on every existing one.

**`--trust-network`.** Trusts the network segment, not the group. Does not
cross a tailnet, and on a network the user does not control it means exactly
what it sounds like.

**Signed introductions, with the code never stored.** A member vouches for a
newcomer with a signed record; records propagate and are stored so anyone can
re-introduce anyone. It keeps the admission secret off disk, at the cost of a
record type, a merge rule, a separate notion of "member" beside "paired", and
storage for it all — to protect a disk that already holds the identity and
the mail. Lost on weight.

**A PAKE (SPAKE2 or CPace) on a human-chosen code.** One guess per interactive
attempt, rate-limitable, so `truss-2026` would be safe to type. Lost because a
generated code makes offline brute force moot with no new cryptography — the
HMAC comes from what is already in the tree — and the code is pasted once per
machine, so choosing it buys little. If human-chosen codes are ever wanted the
`spake2` crate is the candidate, and `curve25519-dalek` is already a
dependency, so the cost is measured and small.

**An epoch counter for revocation.** Members re-pair under a new epoch and a
hello with an old epoch is dropped. Proving the current key in every hello
does the same thing with no counter to keep in step.

**Creating the group when nobody answers within a timeout.** Two people
running `pair <code>` on a partitioned network, or a first member on a slow
tailnet, and there are two groups with one code that the one-group rule
refuses to merge. Creation is explicit instead.
