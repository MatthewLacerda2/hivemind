//! The MCP server Claude Code talks to, served at `/mcp` on the loopback
//! listener.
//!
//! Eight tools, each a 1:1 mapping onto one service-layer function (SPEC §9.1).
//! Anything arriving through here is recorded with `sender_kind: Agent`; the
//! caller cannot set that field itself.

#![doc(html_root_url = "https://docs.rs/hivemind-mcp/0.1.0")]

pub mod server;
pub mod tools;

pub use server::{HivemindMcp, INBOX_URI, McpSessions, PEERS_URI, http_service};
