//! mDNS/DNS-SD and Tailscale discovery sources.
//!
//! Discovery only ever answers "this node exists at this address". It never
//! pairs: trust is established by the TOFU flow in SPEC §6.2. The backend is
//! behind a trait so the delivery tests do not need working multicast.
