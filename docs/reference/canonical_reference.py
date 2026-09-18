"""Independent reference encoder for the hivemind canonical message encoding.

This exists so the golden vectors in `crates/hivemind-core` come from something
other than the code they test. It is written from `docs/protocol.md`, not from
the Rust, and it is the tie-breaker when the two disagree.

    python3 -m venv .venv && .venv/bin/pip install cbor2
    .venv/bin/python docs/reference/canonical_reference.py

The `canonical hex` it prints must equal `GOLDEN_HEX` in
`crates/hivemind-core/src/message.rs`. If you change this file to make them
agree, you are doing it backwards: the vector only means something while the
two derivations are independent.
"""
import hashlib
from collections import OrderedDict

import cbor2

CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def ulid(ms: int, rand: int) -> str:
    """Crockford base32 of 48 bits of timestamp followed by 80 bits of entropy."""
    value = (ms << 80) | rand
    out = ""
    for i in range(26):
        out = CROCKFORD[(value >> (5 * i)) & 0x1F] + out
    return out


def node_id(der: bytes) -> bytes:
    return hashlib.sha256(der).digest()


SENT_AT_MS = 1_750_000_000_000
MSG_ID = ulid(SENT_AT_MS, 0x0102030405060708090A)
FROM = node_id(b"hivemind test certificate")
TO_NODE = node_id(b"recipient certificate")
ATTACHMENT_SHA = hashlib.sha256(b"notes").digest()

# Field order is the declaration order of `Message` in SPEC §4.1, with
# `received_at` and `signature` excluded (ADR 0007).
message = OrderedDict([
    ("id", MSG_ID),
    ("thread_id", MSG_ID),
    ("in_reply_to", None),
    ("from", FROM),
    ("to", [
        OrderedDict([("node", TO_NODE)]),
        OrderedDict([("owner", "rafael")]),
        "everyone",
    ]),
    ("subject", "dashboard PR"),
    ("body", "Take a look when you get a chance."),
    ("kind", "message"),
    ("sender_kind", "human"),
    ("attachments", [
        OrderedDict([
            ("name", "notes.md"),
            ("size", 42),
            ("sha256", ATTACHMENT_SHA),
            ("mime", "text/markdown"),
            ("inline", True),
        ]),
    ]),
    ("sent_at", SENT_AT_MS),
])

encoded = cbor2.dumps(message, canonical=False)
print("ULID          :", MSG_ID)
print("from fp hex   :", FROM.hex())
print("to node fp hex:", TO_NODE.hex())
print("attach sha hex:", ATTACHMENT_SHA.hex())
print("canonical len :", len(encoded))
print("canonical hex :", encoded.hex())
print("sha256 of it  :", hashlib.sha256(encoded).hexdigest())
