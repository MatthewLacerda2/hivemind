//! The bridge between the mail service and the delivery worker (SPEC §8).
//!
//! The worker lives in `hivemind-net`, which knows nothing about mailboxes or
//! address books; this is the seam it is written against. Keeping it here
//! rather than in the worker is what lets the retry logic be tested against an
//! outbox held in memory.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use hivemind_core::peer::NodeId;
use hivemind_core::store::Outbound;
use hivemind_net::delivery::Outbox;

use crate::service::MailService;

/// Gives the delivery worker what it needs from a [`MailService`].
#[derive(Debug, Clone)]
pub struct ServiceOutbox {
    service: Arc<MailService>,
}

impl ServiceOutbox {
    /// Wrap a service.
    #[must_use]
    pub fn new(service: Arc<MailService>) -> Self {
        Self { service }
    }
}

impl Outbox for ServiceOutbox {
    fn pending(&self) -> Vec<Outbound> {
        self.service.pending_outbound().unwrap_or_else(|error| {
            // The disk is unreadable. There is nothing the worker can do about
            // it, and stopping would strand every message rather than this one.
            tracing::warn!(%error, "could not read the outbox");
            Vec::new()
        })
    }

    fn addresses(&self, node: NodeId) -> Vec<String> {
        self.service.peer_addresses(node).unwrap_or_default()
    }

    fn store(&self, outbound: &Outbound) {
        if let Err(error) = self.service.save_outbound(outbound) {
            tracing::warn!(%error, id = %outbound.message.id, "could not save delivery progress");
        }
    }

    fn reached(&self, node: NodeId, addr: &str, at: DateTime<Utc>) {
        if let Err(error) = self.service.record_reached(node, addr, at) {
            tracing::warn!(%error, %node, %addr, "could not record a working address");
        }
    }
}

/// Delivers over mutual TLS to the peer's `/peer/v1/messages` (SPEC §7.2).
#[derive(Debug, Clone)]
pub struct PeerTransport {
    service: Arc<MailService>,
    identity: hivemind_net::tls::LocalIdentity,
}

impl PeerTransport {
    /// Deliver as `identity`, pinning each recipient from the address book.
    #[must_use]
    pub fn new(service: Arc<MailService>, identity: hivemind_net::tls::LocalIdentity) -> Self {
        Self { service, identity }
    }
}

impl hivemind_net::delivery::Transport for PeerTransport {
    async fn deliver(
        &self,
        node: NodeId,
        addr: &str,
        message: &hivemind_core::message::Message,
    ) -> Result<(), hivemind_net::client::ClientError> {
        // Pinned to this one recipient, rebuilt per attempt. A client trusting
        // every paired peer would let one of them collect another's mail by
        // answering on its address; a cached per-peer client would go on
        // trusting a certificate after the peer was removed.
        let trusted = self
            .service
            .certificate_of(node)
            .map_or_else(hivemind_net::tls::TrustedPeers::default, |certificate| {
                hivemind_net::tls::TrustedPeers::new(vec![(node, certificate)])
            });

        let client = hivemind_net::client::PeerClient::pinned(&self.identity, trusted)?;
        let _: hivemind_net::client::PeerResponse<crate::peer::Delivered> =
            client.post(addr, "/peer/v1/messages", message).await?;
        Ok(())
    }
}
