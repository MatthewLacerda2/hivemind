# 0001. No central hub: peers talk directly

- **Status:** accepted
- **Date:** 2026-09-17

## Context

hivemind exists because several Claude Code sessions across several machines
currently use a human as the message bus (SPEC §1). Every existing answer to
this problem asks for something the user has to keep alive: a broker at a known
address, a cloud account, or a single machine everyone else depends on.

The people we are building for are developers on a shared LAN or tailnet who
will not read documentation (SPEC §2). The whole onboarding budget is three
commands. Anything that requires "and someone stands up the server" spends that
budget before the tool has done anything useful.

There is also a trust argument. A hub sees every message that passes through it.
For mail between coworkers' machines, adding a component that must be trusted
with all of it — and must be operated, patched and backed up by someone — is a
cost with no matching benefit, because the machines can already reach each
other directly.

## Decision

Every machine runs one daemon and there is no server. Peers find each other by
mDNS on the LAN or by address on Tailscale, and deliver mail to each other
directly over mutually authenticated TLS (SPEC §3, §5).

Delivery is push: the sender's daemon posts to the recipient's daemon. Nobody
polls anybody for mail.

## Consequences

- Onboarding is `brew install`, `hivemind init`, and at worst one `join` with an
  IP. There is nothing to provision and nothing to keep running.
- No component in the system can read everybody's mail. Compromising one machine
  gets that machine's mailbox, not the network's.
- Both machines must be reachable from each other at some point. Two peers that
  are never online at the same time and never share a network cannot exchange
  mail at all. This is the real cost, and it is why store-and-forward with
  indefinite retry (SPEC §8) is not optional: the laptop that was closed on
  Friday must receive Friday's mail when it opens on Monday.
- Peers behind different NATs with no tailnet are out of scope. A relay is
  explicitly deferred to v2 (SPEC §12) rather than designed away now.
- Every node carries the full delivery state machine, including retry and
  backoff. That logic would have lived in one place with a hub; now it ships
  everywhere and has to be correct everywhere.
- Broadcast is fan-out at the sender, so `to: everyone` costs one delivery per
  peer. At the scale we target — a team, not an organisation — that is fine.

## Alternatives considered

**A central broker (MQTT, NATS, Redis).** Fastest to build and the delivery
semantics come for free. Rejected: it reintroduces the "known broker address"
problem that makes the existing tools unpleasant, and someone has to run it.

**A cloud relay we operate.** Solves reachability everywhere. Rejected: it means
accounts, telemetry and a privacy story, all of which SPEC §1 rules out, and it
makes an offline LAN — the common case — depend on the internet.

**Leader election among peers.** A hub that nobody has to provision. Rejected:
consensus is a large amount of machinery to get wrong for a mail service, and a
changing leader makes the "who has my mail right now" question harder to answer
than direct delivery does.
