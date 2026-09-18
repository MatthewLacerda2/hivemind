//! The canonical encoding, on values that came from somewhere else
//! (SPEC §13.2).
//!
//! `canonical_bytes` is where a message meets its signature, and it runs on
//! every message the daemon receives — verification re-encodes the
//! sender-authored fields and compares. So it is reachable by anyone who can
//! complete a handshake, with whatever a `Message` can be made to hold.
//!
//! Two things are asserted beyond "does not panic", and both are what the
//! golden vectors rest on:
//!
//! - encoding is **deterministic**: the same message twice is the same bytes.
//!   A signature is only meaningful if this holds.
//! - the fields excluded from the signature are **actually excluded**
//!   (ADR 0007): changing `received_at` must not change the bytes, or a
//!   message could not be verified after being stored.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(message) = serde_json::from_slice::<hivemind_core::message::Message>(data) else {
        return;
    };

    let Ok(once) = message.canonical_bytes() else {
        return;
    };
    let twice = message.canonical_bytes().expect("it encoded a moment ago");
    assert_eq!(once, twice, "the canonical encoding is not deterministic");

    let mut stamped = message.clone();
    stamped.received_at = Some(chrono::Utc::now());
    if let Ok(after) = stamped.canonical_bytes() {
        assert_eq!(
            once, after,
            "`received_at` changed the signed bytes; a stored message could \
             never verify (ADR 0007)"
        );
    }
});
