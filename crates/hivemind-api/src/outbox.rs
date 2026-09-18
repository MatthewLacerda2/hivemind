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

    /// The attachments that travel with the message (SPEC §8).
    ///
    /// A blob we no longer hold is left out rather than failing the delivery:
    /// the recipient can still fetch it lazily, and a missing file is a worse
    /// reason to stop delivering mail than to deliver it without the file.
    fn inline_parts(
        &self,
        message: &hivemind_core::message::Message,
    ) -> Vec<hivemind_net::client::Part> {
        message
            .attachments
            .iter()
            .filter(|attachment| attachment.inline)
            .filter_map(|attachment| {
                let path = self.service.blobs().path_of(&attachment.sha256);
                match std::fs::read(&path) {
                    Ok(bytes) => Some(hivemind_net::client::Part {
                        name: attachment.sha256.to_hex(),
                        content_type: attachment.mime.clone(),
                        bytes,
                    }),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            name = %attachment.name,
                            "an inline attachment is missing; sending without it"
                        );
                        None
                    }
                }
            })
            .collect()
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
        let parts = self.inline_parts(message);
        let _: hivemind_net::client::PeerResponse<crate::peer::Delivered> = client
            .post_multipart(addr, "/peer/v1/messages", message, &parts)
            .await?;
        Ok(())
    }
}

/// Feeds mDNS results into the address book (SPEC §5.4).
///
/// A known peer gets its address refreshed. An unknown one is greeted, which
/// records a pending offer — an introduction, not an agreement, still needing
/// a human on both sides (SPEC §6.2). Without that there would be nothing for
/// `hivemind peers` to list and nothing for anyone to confirm.
#[derive(Debug, Clone)]
pub struct ServiceSink {
    service: Arc<MailService>,
    /// Nodes greeted recently, so a browse result arriving several times a
    /// minute does not become a handshake several times a minute.
    greeted: Arc<std::sync::Mutex<std::collections::HashMap<NodeId, std::time::Instant>>>,
}

/// How long a greeting counts for before the same node is greeted again.
const GREETING_INTERVAL: std::time::Duration = std::time::Duration::from_mins(5);

impl ServiceSink {
    /// Wrap a service.
    #[must_use]
    pub fn new(service: Arc<MailService>) -> Self {
        Self {
            service,
            greeted: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Whether this node is due a greeting, marking it greeted if so.
    fn due_a_greeting(&self, node: NodeId) -> bool {
        let Ok(mut greeted) = self.greeted.lock() else {
            // A poisoned lock here would mean never greeting anyone again.
            return false;
        };
        let now = std::time::Instant::now();
        match greeted.get(&node) {
            Some(last) if now.duration_since(*last) < GREETING_INTERVAL => false,
            _ => {
                greeted.insert(node, now);
                true
            }
        }
    }
}

impl hivemind_net::discovery::Seen for ServiceSink {
    fn seen(&self, node: hivemind_net::discovery::Discovered) {
        match self
            .service
            .learn_discovered_addr(node.id, node.addr.clone())
        {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, id = %node.id, "could not record a discovered address");
                return;
            }
        }

        if !self.due_a_greeting(node.id) {
            return;
        }

        let service = Arc::clone(&self.service);
        let authority = node.addr.authority();
        tokio::spawn(async move {
            if let Err(error) = service.join(&authority).await {
                tracing::debug!(%authority, %error, "could not greet a node seen on the LAN");
            }
        });
    }
}
