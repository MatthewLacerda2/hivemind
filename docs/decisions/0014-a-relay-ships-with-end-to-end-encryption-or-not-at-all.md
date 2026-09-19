# 0014. A relay ships with end-to-end encryption, or not at all

- **Status:** accepted
- **Date:** 2026-09-19
- **Amends:** SPEC §1, §12

## Context

SPEC §12 reserves a relay for v2: a member that holds mail for two peers that
are never online at the same time. Nothing about it is designed, which is the
point of §12.

Today mail is protected by two things (SPEC §6.3): mutual TLS between the two
endpoints, and a signature by the sender. The signature covers integrity and
origin. It does not cover confidentiality, and it does not need to, because
the only machines that ever hold a message are the sender and the recipient.

A relay is, by definition, a third machine that holds the message. The
signature still proves who wrote it; the relay can still read it. That is not
a property of any particular relay design — it is what "hold mail for
somebody" means, and it holds for a member acting as relay just as it would
for a server.

Whoever eventually builds a relay will have every incentive to build it
without encryption first, because the relay works without it and the
encryption is the harder half.

## Decision

A relay is two features, shipped together: the relay, and end-to-end
encryption of the message body and attachments to the recipient. The obvious
construction is a sealed box to the recipient's key, derived from the Ed25519
certificate it already has (Ed25519 → X25519), so nothing new has to be
distributed. The ADR that proposes a relay proposes the encryption in the same
record, and the milestone that ships one ships both.

This costs nothing now. It is written so that it cannot be forgotten later.

## Consequences

- The relay ADR, when it comes, is a larger piece of work than it would have
  looked, and everyone knows that before starting.
- `SECURITY.md`'s "what it does not defend against" stays true as written: no
  machine but the two endpoints holds a message, so nothing but those two can
  read one.
- Nothing about today's delivery changes. Direct delivery between paired
  members stays TLS plus signature; adding encryption there would be a second
  layer over a channel that already has one.
- The recipient's public key is in its certificate, which every member already
  pins (SPEC §4.2). Key distribution for the encryption is therefore already
  solved, which is what makes this a rule worth writing rather than a wish.

## Alternatives considered

**Leave it to the relay ADR.** Lost because the person writing that ADR is
the person with the incentive to defer it, and a rule that exists only when
somebody remembers it is not a rule.

**Encrypt everything now, relay or not.** Lost because it protects nothing
today — the endpoints are the only holders — and it would make every message
unreadable on disk to the tools that read `mail/` directly (ADR 0002 makes the
files the source of truth, and `hook check` reads the index built from them).
