//! The maildir-style message store.
//!
//! Files under `~/.hivemind/mail/` are the source of truth (SPEC §4.3). Every
//! write is write-to-temp plus atomic rename; nothing is ever edited in place.
