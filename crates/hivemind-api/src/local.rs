//! The loopback router (SPEC §7.1).
//!
//! Binds to `127.0.0.1` and fails closed if configured otherwise: it carries no
//! authentication, so reachability *is* the authorization (SPEC §6.3).
