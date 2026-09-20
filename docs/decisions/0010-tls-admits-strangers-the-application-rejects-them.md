# 0010. TLS admits strangers; the application is what rejects them

- **Status:** accepted
- **Date:** 2026-09-17
- **Amends:** SPEC §6.3
- **Addendum:** 2026-09-20, "Observed in use" below

## Context

SPEC §6.3 describes the transport as:

> Custom `ClientCertVerifier`/`ServerCertVerifier` that accepts exactly the
> certificates in `peers.toml` (plus `pending_pairs` for the handshake endpoint
> only).

SPEC §6.2 describes pairing as trust-on-first-use:

> 1. A calls `POST /peer/v1/handshake` on B over TLS, presenting its client
>    cert. B does the same in response. Both sides now have each other's cert.
> 2. Both daemons store the candidate in `pending_pairs`. […]
> 3. Only after **both** sides confirm is the peer written to `peers.toml` […]
>    Until then, B rejects mail from A with `403 not_paired`.

These cannot both hold. For A's very first handshake, A is in neither B's
`peers.toml` nor B's `pending_pairs` — it becomes pending *as a result of* that
handshake. If TLS rejected it, step 1 could never happen and nothing would ever
pair.

The parenthetical "for the handshake endpoint only" also asks for something TLS
cannot do. The client certificate is verified during the handshake, before any
HTTP request has been parsed, so the verifier cannot know which endpoint the
connection is about to address.

§6.2's own answer is the tell: the rejection it specifies is **`403 not_paired`**
— an HTTP status. An unpaired sender is supposed to reach the application and be
turned away there.

## Decision

Split the two directions, because they are not the same question.

**Inbound (we are the server).** Accept any well-formed client certificate at
the TLS layer, and record which node presented it. Authorisation is then the
application's job:

- `POST /peer/v1/handshake` — open to anyone. This is what creates a pending pair.
- Everything else — requires a peer in `peers.toml`, else `403 not_paired`.

**Outbound (we are the client).** Pin. Delivering mail to a known peer requires
that peer's exact certificate, so a node that answers on the right address with
the wrong key gets nothing. There is one exception, and it is the "first use" in
trust-on-first-use: `hivemind join <host>` accepts whatever certificate the host
presents, shows its fingerprint, and asks. Nothing is trusted until a human says
so on both sides.

SPEC §6.3 should read: certificates in `peers.toml` are pinned for outbound
connections; inbound connections are admitted by TLS and authorised by the
application.

## Consequences

- Pairing can actually happen, which the literal reading prevented.
- The security boundary is exactly where §6.2 already put it: `403 not_paired`.
  An unpaired node can complete a TLS handshake and call one endpoint. It cannot
  deliver mail, fetch a blob, or learn anything about the mailbox.
- **An unpaired node can create a pending entry.** That is a real cost. Anyone
  who can reach port 8400 can add a row to `pending_pairs` and make somebody's
  `hivemind peers` output longer. It is not a foothold — nothing about a pending
  entry grants access, and the user confirms fingerprints by hand — but it is
  unauthenticated write-ish access to a small amount of local state, so the list
  is bounded and evicts the oldest unconfirmed entry.
- Outbound pinning is what actually protects mail in flight, and it is
  unweakened: we never send a message to a certificate we did not confirm.
- `--trust-network` (SPEC §6.2) becomes a policy on the *confirmation* step, not
  on TLS, which is where it belongs.
- The handshake endpoint is the one piece of attack surface an unpaired node can
  reach, so it stays small: it parses a fixed, small JSON body, does no I/O
  beyond appending a pending entry, and never returns anything about the
  mailbox.

## Alternatives considered

**Accept only `peers.toml` and `pending_pairs`, with pending seeded some other
way.** The only way to seed it would be an out-of-band channel — a plaintext
port serving the certificate, or the user copying a fingerprint by hand before
first contact. The first is a second listener with its own security story; the
second destroys the three-command onboarding in SPEC §2.

**Verify per-endpoint at the TLS layer.** Not possible: certificate
verification happens before the request line is read.

**Pin inbound too, and pair entirely by hand.** Genuinely more restrictive, and
it is what `--trust-network` is the opposite of. Rejected because it makes the
common case — two laptops on one network — require copying a fingerprint over
another medium before anything can happen at all.

## Observed in use

*Added 2026-09-20, from the first use outside tests, on 2026-09-18. It records
evidence and changes nothing above.*

Three machines, two operating systems, a local wifi and a tailnet, and for the
first time a peer that belonged to somebody else. Matheus, who pays the cost of
the extra step, on whether it earned its place — translated from the Portuguese:

> the `hivemind pair key123` helped a lot, I wanted it not to be needed but it
> was useful and ended up being more gain than headache

Both sides of it turned up on the same day.

**What it bought.** Leonardo's node appeared over mDNS on its own, with nobody
doing anything. Without an admission step, every machine on an office wifi would
take mail from every other at first contact. The extra step is what makes
discovery that automatic acceptable at all, and it is the step this record
argues belongs in the application rather than in TLS.

**What it cost, measured.** A message sent to his node before it had admitted
this one sat in the queue, was refused, and went through two minutes later once
admission happened. That delay is the price, and it cleared itself.

The useful finding is not that admission is worth having. It is **where it
hurts**: the delay was invisible. The sender saw a message in the outbox and no
reason for it being there — not "waiting to be admitted", not "refused, will
retry". The fix for that is not in this record; it is per-recipient delivery
state, which is #31.

[0013](0013-membership-is-possession-of-a-group-key.md), written the day after,
replaced the per-pair fingerprint confirmation that was running here with a
group key, so the two-minute wait for a human in particular is gone. What was
measured survives the change, because the boundary this record draws does not
move: admission is still the application's decision rather than the transport's,
`403 not_paired` is still the refusal, and a sender still cannot see that being
refused is why its mail is waiting.
