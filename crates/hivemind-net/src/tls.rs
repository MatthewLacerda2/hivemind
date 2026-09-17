//! TLS 1.3 with mutual authentication, pinned by certificate fingerprint.
//!
//! There is no CA and no hostname verification: a connection is accepted iff
//! the presented certificate is one listed in `peers.toml`, plus the pending
//! pairs that the handshake endpoint alone will talk to (SPEC §6.3).
