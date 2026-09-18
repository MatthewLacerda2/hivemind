//! A `Message` arriving over the wire (SPEC §13.2).
//!
//! `POST /peer/v1/messages` hands these bytes to serde before anything has
//! decided whether the sender is trustworthy. The target is narrow and
//! absolute: **it must not panic.** A refusal is the correct outcome for
//! almost every input here; a crash is a paired peer being able to stop the
//! daemon by sending it rubbish.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(message) = serde_json::from_slice::<hivemind_core::message::Message>(data) else {
        return;
    };

    // It parsed. Everything the receive path does before trusting it has to
    // survive the result, because parsing is not validating.
    let _ = message.validate();

    // The signature is checked by re-encoding the sender-authored fields and
    // comparing. That runs on every message that arrives, so it runs on this
    // one (SPEC §4.1).
    let _ = message.canonical_bytes();
});
