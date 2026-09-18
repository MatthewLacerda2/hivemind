#!/usr/bin/env python3
"""Write the seed corpus for the message-shaped fuzz targets.

Without these the fuzzer spends its whole budget discovering that random bytes
are not JSON, which tests serde and nothing of ours. Seeded with real messages
it mutates *valid* ones, which is what reaches `validate` and
`canonical_bytes` — the code an attacker actually gets to.

They live in `fuzz/seeds/`, which is checked in. `fuzz/corpus/` is
libFuzzer's own working directory — it grows to thousands of files as the
fuzzer finds new coverage — and is ignored.

Generated rather than checked in as opaque blobs so the shape stays readable
and can be regenerated when the model changes.

    python3 fuzz/seed.py
"""

import json
import pathlib

HERE = pathlib.Path(__file__).resolve().parent

# A minimal valid message, and the variations worth starting from. Each one is
# a different branch in `validate` or a different CBOR shape.
SEEDS = {
    "plain": {},
    "reply": {"in_reply_to": "01JXT200000000000000000001"},
    "task": {"kind": "task", "sender_kind": "agent"},
    "everyone": {"to": [{"everyone": None}]},
    "owner": {"to": [{"owner": "matheus"}]},
    "attachment": {
        "attachments": [
            {
                "name": "notes.md",
                "size": 12,
                "sha256": "ab" * 32,
                "mime": "text/markdown",
                "inline": True,
            }
        ]
    },
    "received": {"received_at": "2026-09-18T12:00:00Z"},
    "empty_subject": {"subject": ""},
    "unicode": {"subject": "relatório — ação", "body": "linha\ncom acentuação"},
}

# A ULID is 26 Crockford base32 characters. The first draft of these had 24,
# counted by eye, and every seed was rejected at the first field — which is
# exactly the failure the test in `message.rs` exists to catch.
BASE = {
    "id": "01JXT200000000000000000000",
    "thread_id": "01JXT200000000000000000000",
    "in_reply_to": None,
    "from": "0" * 64,
    "to": [{"node": "0" * 64}],
    "subject": "a subject",
    "body": "a body",
    "kind": "message",
    "sender_kind": "human",
    "attachments": [],
    "sent_at": "2026-09-18T12:00:00Z",
    "received_at": None,
    "signature": "0" * 128,
}


def main() -> None:
    for target in ("message_json", "canonical_encoding"):
        directory = HERE / "seeds" / target
        directory.mkdir(parents=True, exist_ok=True)
        for name, overrides in SEEDS.items():
            (directory / f"{name}.json").write_text(
                json.dumps({**BASE, **overrides}), encoding="utf-8"
            )
    print(f"wrote {len(SEEDS)} seeds to each of two corpora")


if __name__ == "__main__":
    main()
