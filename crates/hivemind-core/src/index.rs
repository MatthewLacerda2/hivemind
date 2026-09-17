//! The derived `SQLite` query index.
//!
//! `index.db` answers "how many unread", "this thread", "search subject and
//! body" quickly. It holds no authoritative state: deleting it is always safe
//! and the daemon rebuilds it from `mail/` on startup when it is missing or its
//! schema version has moved (SPEC §4.3).
