"""Independent reference for the group-key code and proof (ADR 0013).

Written from `docs/protocol.md`, not from the Rust, so the golden vector in
`crates/hivemind-core/src/group.rs` comes from two derivations rather than one.
Standard library only:

    python3 docs/reference/group_proof_reference.py

The `code` and `proof hex` it prints must equal `GOLDEN_CODE` and
`GOLDEN_PROOF_HEX` in `group.rs`. If you change this file to make them agree,
you are doing it backwards: the vector only means something while the two
derivations are independent.
"""
import base64
import hashlib
import hmac
import struct

DOMAIN = b"hivemind group proof v1\x00"

# 16 bytes, 0x00..0x0f: easy to recognise in a hex dump.
KEY = bytes(range(16))
SENDER_CERT = b"sender certificate"
RECEIVER_CERT = b"receiver certificate"
SENT_AT_MS = 1_750_000_000_000


def code(key: bytes) -> str:
    """`hm-` then the key in lowercase unpadded base32, in groups of four."""
    body = base64.b32encode(key).decode().rstrip("=").lower()
    groups = [body[i:i + 4] for i in range(0, len(body), 4)]
    return "hm-" + "-".join(groups)


def proof(key: bytes, sender: bytes, receiver: bytes, sent_at_ms: int) -> bytes:
    message = (
        DOMAIN
        + struct.pack(">I", len(sender)) + sender
        + struct.pack(">I", len(receiver)) + receiver
        + struct.pack(">q", sent_at_ms)
    )
    return hmac.new(key, message, hashlib.sha256).digest()


print("code      :", code(KEY))
print("proof hex :", proof(KEY, SENDER_CERT, RECEIVER_CERT, SENT_AT_MS).hex())
