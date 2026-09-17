//! The `Message` type and its canonical encoding.
//!
//! A message is signed over a deterministic CBOR encoding of its fields so that
//! a message read back off disk is verifiable independently of the transport
//! that carried it (SPEC §4.1). The encoding is frozen by golden tests; see
//! `docs/protocol.md`.
