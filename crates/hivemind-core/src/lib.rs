//! Domain model for hivemind: messages, peers, the on-disk mail store and the
//! query index.
//!
//! This crate is deliberately the boring one. It performs no network I/O and
//! exposes no `async` API — filesystem access is the only side effect it is
//! allowed (SPEC §3.1). Everything else in the workspace depends on it, so
//! keeping it synchronous and dependency-light keeps the rest testable.
//!
//! # Layout
//!
//! - [`message`] — the `Message` type, its canonical encoding and signature.
//! - [`blobs`] — content-addressed attachment storage (SPEC §4.3).
//! - [`peer`] — node identity and fingerprints.
//! - [`peerbook`] — the address book: who we have paired with (SPEC §4.2).
//! - [`store`] — the maildir-style store; files are the source of truth (SPEC §4.3).
//! - [`index`] — the derived `SQLite` cache and its rebuild logic.
//! - [`crypto`] — Ed25519 key handling, fingerprints and signing.
//! - [`identity`] — this node's keypair and self-signed certificate.
//! - [`config`] — `config.toml` parsing and validation.

#![doc(html_root_url = "https://docs.rs/hivemind-core/0.1.0")]

pub mod blobs;
pub mod config;
pub mod crypto;
pub mod identity;
pub mod index;
pub mod message;
pub mod peer;
pub mod peerbook;
pub mod store;
