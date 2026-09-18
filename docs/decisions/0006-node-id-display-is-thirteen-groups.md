# 0006. The node id display form is 13 groups, not 12

- **Status:** accepted
- **Date:** 2026-09-17

## Context

SPEC §6.1 specifies the human-readable form of a `NodeId` as:

> displayed as `hm1:<base32-lower, 12 groups>` with a short 8-char prefix for
> humans

A `NodeId` is a SHA-256 digest: 32 bytes, 256 bits. Unpadded base32 carries 5
bits per character, so it takes `ceil(256 / 5)` = **52 characters** to represent
one. 52 does not divide into 12 groups.

The spec is therefore not implementable as written. It admits three readings:

1. 13 groups of 4 characters — the full digest, 52 characters.
2. 12 groups of 4 characters — 48 characters, which is the digest **truncated
   to 240 bits**, discarding the last 16.
3. 12 groups of some other width, e.g. 12 × 5 with a short final group. This
   produces ragged output and no obvious benefit.

Reading 2 is the literal one, and 240 bits is still far beyond any collision
concern. But truncating a fingerprint is a security-relevant decision, and this
fingerprint is the *only* thing standing behind peer authentication: SPEC §6.3
pins TLS certificates by it, with no CA and no hostname check. A truncation
there deserves to be a decision somebody made on purpose, not a number inferred
from a formatting hint.

## Decision

Render the full digest as **13 groups of 4 lowercase base32 characters**:

```
hm1:w2mq-xor2-seiv-lghq-ybqb-36iv-i24o-n5vk-dpww-736k-eici-imu3-bnwq
```

The short form stays exactly as the spec says — the first 8 characters of the
base32 body, `w2mqxor2` — since that is what people type into `hivemind pair`
and compare by eye, and nothing authenticates on it.

Parsing accepts uppercase as well as lowercase, because someone retyping a
fingerprint from another screen should not be punished for their shift key.
Display is always lowercase.

## Consequences

- The displayed identity is the whole fingerprint. Comparing two of them by eye
  compares all 256 bits, and copying one from a terminal into `peers.toml`
  loses nothing.
- The display form is four characters longer than the spec implies. At this
  length it makes no practical difference to a line of terminal output.
- SPEC §6.1 said "12 groups" and has been amended to say 13 groups of 4, so the
  spec and the code now agree. This record is why.
- Nothing else depends on the display form. The wire protocol and `peers.toml`
  carry the raw 32 bytes; this is presentation only.

## Alternatives considered

**Truncate to 240 bits (12 groups).** Matches the spec literally. Rejected for
now because it trades away digest bits for a formatting detail, and the trade
was never stated as such.

**Groups of 13 characters (4 groups).** 52 divides evenly this way too.
Rejected: long groups are exactly what grouping is meant to prevent — the point
is to make a fingerprint scannable by eye, and four-character groups are what
every other tool that does this uses.

**No grouping at all.** Rejected: a 52-character run of base32 is unreadable and
undiffable by a human, which defeats the purpose of having a display form.
