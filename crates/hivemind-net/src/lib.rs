//! Discovery, transport and delivery: everything hivemind does over a wire.
//!
//! - [`discovery`] — mDNS/DNS-SD browsing and the Tailscale peer probe (SPEC §5).
//! - [`tls`] — rustls configuration with fingerprint-pinned mutual auth (SPEC §6.3).
//! - [`listener`] — the peer listener, which hands each caller's identity to the router.
//! - [`client`] — the peer HTTP client used to deliver mail and fetch blobs.
//! - [`delivery`] — the per-recipient retry queue that makes delivery survive
//!   a laptop being closed on Friday (SPEC §8).

#![doc(html_root_url = "https://docs.rs/hivemind-net/0.1.0")]

pub mod client;
pub mod delivery;
pub mod discovery;
pub mod listener;
pub mod tls;
