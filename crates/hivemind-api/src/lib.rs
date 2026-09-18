//! The axum application: one service layer, two listeners.
//!
//! The loopback listener on `127.0.0.1:8401` serves humans, the CLI, the web UI
//! and MCP. The peer listener on `0.0.0.0:8400` serves other daemons over
//! mutual TLS. Both mount routers over the same [`service`] layer so that every
//! operation has exactly one implementation (SPEC §3).

#![doc(html_root_url = "https://docs.rs/hivemind-api/0.1.0")]

pub mod local;
pub mod openapi;
pub mod peer;
pub mod problem;
pub mod service;

pub use local::{router, serve};
pub use openapi::ApiDoc;
pub use service::{Draft, Event, MailService, ServiceError};
