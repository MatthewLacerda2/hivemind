//! Node identity, the peer address book and recipient expansion.
//!
//! A `NodeId` is the SHA-256 fingerprint of a node's DER-encoded certificate
//! (SPEC §6.1). Identity is per machine; the free-text `owner` label is what
//! makes `to: matthew` fan out across all of Matthew's machines (SPEC §8).
