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

/// Fetches lazy attachments as soon as their message arrives (SPEC §8).
///
/// Only when `prefetch = true`. The default is to wait until somebody asks,
/// because the common case is a laptop on a metered connection that will never
/// open most of what it receives.
///
/// Failures are logged and dropped: a fetch that did not work is exactly the
/// situation the lazy path already handles, so the message is still readable
/// and the attachment is still fetchable on first access.
pub async fn prefetch_attachments<F>(service: Arc<MailService>, shutdown: F)
where
    F: std::future::Future<Output = ()> + Send,
{
    if !service.prefetches() {
        return;
    }

    let mut events = service.subscribe();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let event = tokio::select! {
            () = &mut shutdown => break,
            event = events.recv() => event,
        };

        let Ok(crate::service::Event::MessageReceived { id }) = event else {
            // A lagged receiver has missed messages; their attachments will be
            // fetched on first access like any other. Not worth stopping for.
            continue;
        };

        let Ok((_, message)) = service.get(id) else {
            continue;
        };

        for digest in service.missing_attachments(&message) {
            if let Err(error) = service.fetch_attachment(id, digest).await {
                tracing::debug!(%error, %id, "could not prefetch an attachment");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivemind_core::crypto::SigningKey;
    use hivemind_net::discovery::Seen as _;

    use crate::service::NodeDescription;

    fn service() -> (tempfile::TempDir, Arc<MailService>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let id = NodeId::from_certificate_der(b"this node");
        let node = NodeDescription {
            id,
            certificate: b"this node".to_vec(),
            private_key: Vec::new(),
            name: "test".to_owned(),
            owner: None,
            callback_host: "127.0.0.1".to_owned(),
            peer_port: 8400,
            max_attachment_bytes: hivemind_core::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            inline_max_bytes: hivemind_core::config::DEFAULT_INLINE_MAX_BYTES,
            prefetch: false,
        };
        let service = MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
            .expect("service");
        (dir, Arc::new(service))
    }

    fn node(seed: u8) -> NodeId {
        NodeId::from_certificate_der(&[seed; 16])
    }

    #[test]
    fn a_node_seen_for_the_first_time_is_due_a_greeting() {
        let (_dir, service) = service();
        let sink = ServiceSink::new(service);
        assert!(sink.due_a_greeting(node(1)));
    }

    #[test]
    fn the_same_node_seen_again_immediately_is_not() {
        // mDNS repeats a browse result several times a minute. Without this,
        // each repeat would be a TLS handshake.
        let (_dir, service) = service();
        let sink = ServiceSink::new(service);

        assert!(sink.due_a_greeting(node(1)));
        assert!(!sink.due_a_greeting(node(1)));
        assert!(!sink.due_a_greeting(node(1)));
    }

    #[test]
    fn a_different_node_is_due_one_of_its_own() {
        // Greeting one machine must not silence the next one discovered.
        let (_dir, service) = service();
        let sink = ServiceSink::new(service);

        assert!(sink.due_a_greeting(node(1)));
        assert!(sink.due_a_greeting(node(2)));
    }

    #[test]
    fn a_node_greeted_longer_ago_than_the_interval_is_due_again() {
        // The boundary, from both sides. `Instant` cannot be moved, so the
        // record is aged directly — which is what the passage of time does to
        // it anyway.
        let (_dir, service) = service();
        let sink = ServiceSink::new(service);
        let peer = node(1);

        assert!(sink.due_a_greeting(peer));

        // `checked_sub` rather than `-`: an `Instant` taken early in a
        // process's life can be younger than the interval, and that panic
        // would look like a bug in the code under test.
        let second = std::time::Duration::from_secs(1);
        let now = std::time::Instant::now();
        let ago = |how_long| {
            now.checked_sub(how_long)
                .expect("the process has run for longer than the interval")
        };

        sink.greeted
            .lock()
            .expect("lock")
            .insert(peer, ago(GREETING_INTERVAL.saturating_sub(second)));
        assert!(
            !sink.due_a_greeting(peer),
            "one second short of the interval is still too soon"
        );

        sink.greeted
            .lock()
            .expect("lock")
            .insert(peer, ago(GREETING_INTERVAL + second));
        assert!(sink.due_a_greeting(peer), "one second past it is due again");
    }

    #[test]
    fn discovering_a_peer_we_already_know_records_the_address_and_greets_nobody() {
        // SPEC §5.4: discovery keeps the address book current for known peers
        // and never creates trust.
        let (_dir, service) = service();
        let friend = hivemind_core::identity::Identity::from_seed([7u8; 32]).expect("identity");
        let id = friend.node_id();

        service
            .record_pairing_offer(
                id,
                &crate::peer::Handshake {
                    id: id.to_string(),
                    name: "friend".to_owned(),
                    owner: None,
                    version: "0.1.0".to_owned(),
                    callback_host: "10.0.0.1".to_owned(),
                    callback_port: 8400,
                    gossip: None,
                },
                friend.certificate_der().to_vec(),
                hivemind_core::peerbook::PeerAddr::manual("10.0.0.1", 8400),
            )
            .expect("offer");
        service.confirm_pair(id).expect("confirm");

        let sink = ServiceSink::new(Arc::clone(&service));
        sink.seen(hivemind_net::discovery::Discovered {
            id,
            name: "friend".to_owned(),
            owner: None,
            addr: hivemind_core::peerbook::PeerAddr {
                host: "10.0.0.9".to_owned(),
                port: 8400,
                source: hivemind_core::peerbook::AddrSource::Mdns,
                last_ok: None,
            },
        });

        let addresses = service.peer_addresses(id).expect("addresses");
        assert!(
            addresses.iter().any(|a| a == "10.0.0.9:8400"),
            "the discovered address should have been learned: {addresses:?}"
        );
        assert!(
            sink.greeted.lock().expect("lock").is_empty(),
            "a peer we already know needs no greeting"
        );
    }

    #[test]
    fn the_outbox_reports_what_is_waiting_and_where_to_send_it() {
        let (_dir, service) = service();
        let outbox = ServiceOutbox::new(Arc::clone(&service));

        assert!(outbox.pending().is_empty(), "nothing sent yet");

        // A message to ourselves completes at once, so it never sits in out/.
        service
            .send(
                crate::service::Draft {
                    to: vec![hivemind_core::message::Recipient::Node(service.identity())],
                    subject: "to myself".to_owned(),
                    body: "x".to_owned(),
                    kind: hivemind_core::message::Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                hivemind_core::message::SenderKind::Human,
            )
            .expect("send");
        assert!(
            outbox.pending().is_empty(),
            "a message with no remote recipient is already delivered"
        );

        // An unknown node has no address to try, which is not the same as
        // having one that does not answer.
        assert!(outbox.addresses(node(3)).is_empty());
    }
}
