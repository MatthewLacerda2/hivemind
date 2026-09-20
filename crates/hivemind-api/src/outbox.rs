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
        let mut entries = self.service.pending_outbound().unwrap_or_else(|error| {
            // The disk is unreadable. There is nothing the worker can do about
            // it, and stopping would strand every message rather than this one.
            tracing::warn!(%error, "could not read the outbox");
            Vec::new()
        });

        // SPEC §5.5 and §8: a peer that has just said hello is demonstrably
        // up, so the wait it accumulated while it was off is about a machine
        // that no longer exists. Clearing the backoff here rather than
        // threading a "try these now" set through `attempt_all` keeps the
        // decision in one place — and because `pending` returns copies read
        // from disk, the stored attempt count is untouched. That matters: a
        // peer that says hello and then refuses the delivery must resume its
        // backoff where it was, not restart the sequence at two seconds.
        let woken = self.service.take_woken();
        if !woken.is_empty() {
            for entry in &mut entries {
                for recipient in &mut entry.recipients {
                    if woken.contains(&recipient.node) {
                        recipient.attempts = 0;
                        recipient.last_attempt = None;
                    }
                }
            }
        }
        entries
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

    fn unreachable(&self, node: NodeId) {
        self.service.mark_offline(node);
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
        // SPEC §13.1: every network operation carries a span naming the peer
        // and the message. A delivery that failed is looked up by one or the
        // other — "why has Ana not got it" and "where did 01JXT2 go" are the
        // two questions anybody asks — and a log line without both answers
        // neither.
        let span = tracing::info_span!(
            "deliver",
            peer = %node.short(),
            message = %message.id,
            %addr
        );
        let _entered = span.enter();

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
/// A known peer gets its address refreshed. An unknown one is remembered as
/// seen and, if this node is in a group, greeted: the two proofs are
/// exchanged, and a node in the same group is a peer by the time the
/// handshake returns (SPEC §6.2). Nobody is asked anything.
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

        self.service.record_seen(
            node.id,
            Some(node.name.clone()),
            node.owner.clone(),
            node.addr.clone(),
        );

        // With no key there is no proof to offer, and a greeting would only
        // be refused. Joining a group greets everything seen so far instead.
        if !self.service.in_group().unwrap_or(false) || !self.due_a_greeting(node.id) {
            return;
        }

        let service = Arc::clone(&self.service);
        let authority = node.addr.authority();
        let source = node.addr.source;
        tokio::spawn(async move {
            if let Err(error) = service.greet(&authority, source).await {
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
    use hivemind_core::store::Mailbox;
    use hivemind_net::discovery::Seen as _;

    use crate::service::NodeDescription;

    fn service() -> (tempfile::TempDir, Arc<MailService>) {
        let dir = tempfile::tempdir().expect("temp dir");
        // A real certificate and key, not a placeholder: without them the TLS
        // client cannot be built, so a greeting fails before it reaches the
        // network and no test can observe one being sent (#57).
        let identity = hivemind_core::identity::Identity::from_seed([1u8; 32]).expect("identity");
        let node = NodeDescription {
            id: identity.node_id(),
            certificate: identity.certificate_der().to_vec(),
            private_key: identity.private_key_pkcs8().expect("key"),
            name: "test".to_owned(),
            owner: None,
            callback_host: "127.0.0.1".to_owned(),
            peer_port: 8400,
            max_attachment_bytes: hivemind_core::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            inline_max_bytes: hivemind_core::config::DEFAULT_INLINE_MAX_BYTES,
            prefetch: false,
            presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
            tailscale: hivemind_core::config::Tailscale::Auto,
        };
        let service =
            MailService::open(dir.path(), node, identity.signing_key().clone()).expect("service");
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
            .admit(
                id,
                "friend",
                None,
                friend.certificate_der().to_vec(),
                hivemind_core::peerbook::PeerAddr::manual("10.0.0.1", 8400),
            )
            .expect("admit");

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

    fn stranger_on_the_lan(id: NodeId, port: u16) -> hivemind_net::discovery::Discovered {
        hivemind_net::discovery::Discovered {
            id,
            name: "their-laptop".to_owned(),
            owner: Some("ana".to_owned()),
            addr: hivemind_core::peerbook::PeerAddr {
                host: "127.0.0.1".to_owned(),
                port,
                source: hivemind_core::peerbook::AddrSource::Mdns,
                last_ok: None,
            },
        }
    }

    /// Port 1 on loopback: nothing listens, so a greeting fails at once
    /// rather than hanging a test on an address that never answers.
    const NOBODY_HOME: u16 = 1;

    #[tokio::test]
    async fn a_stranger_on_the_lan_is_listed_but_not_greeted_by_a_node_in_no_group() {
        // With no key there is no proof to offer, and the greeting would only
        // be refused. `hivemind peers` still shows it, so a person can see
        // there is somebody there to give the code to.
        let (_dir, service) = service();
        let sink = ServiceSink::new(Arc::clone(&service));

        sink.seen(stranger_on_the_lan(node(5), NOBODY_HOME));

        let seen = service.seen_nodes().expect("seen");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name.as_deref(), Some("their-laptop"));
        assert!(
            sink.greeted.lock().expect("lock").is_empty(),
            "a node with no group has nothing to greet with"
        );
    }

    #[tokio::test]
    async fn a_stranger_on_the_lan_is_greeted_by_a_node_in_a_group() {
        // The `greeted` map is not the greeting. `due_a_greeting` marks the
        // node as a side effect of being asked, so a sink that returns
        // without greeting leaves exactly the same entry behind as one that
        // greets — which is why dropping the `!` in front of it survived a
        // mutation sweep (#57). The greeting itself is a connection, so the
        // test answers the door.
        let (_dir, service) = service();
        service.create_group(false).expect("create");
        let door = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = door.local_addr().expect("local addr").port();
        let sink = ServiceSink::new(Arc::clone(&service));

        sink.seen(stranger_on_the_lan(node(6), port));

        tokio::time::timeout(std::time::Duration::from_secs(5), door.accept())
            .await
            .expect("a node in a group greets a stranger discovered on the LAN")
            .expect("accept the greeting");
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

    /// A service, and one message queued for a peer that will never answer.
    fn queued_for_an_offline_peer() -> (tempfile::TempDir, Arc<MailService>, NodeId) {
        let (dir, service) = service();
        let friend = hivemind_core::identity::Identity::from_seed([44u8; 32]).expect("identity");
        let id = friend.node_id();
        service
            .admit(
                id,
                "friend",
                None,
                friend.certificate_der().to_vec(),
                hivemind_core::peerbook::PeerAddr::manual("10.0.0.4", 8400),
            )
            .expect("admit");
        service
            .send(
                crate::service::Draft {
                    to: vec![hivemind_core::message::Recipient::Node(id)],
                    subject: "waiting".to_owned(),
                    body: "x".to_owned(),
                    kind: hivemind_core::message::Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                hivemind_core::message::SenderKind::Human,
            )
            .expect("send");
        (dir, service, id)
    }

    /// A service, and a friend queued mail that has already failed four times.
    fn backed_off_entry() -> (tempfile::TempDir, Arc<MailService>, NodeId) {
        let (dir, service, id) = queued_for_an_offline_peer();
        let mut entry = ServiceOutbox::new(Arc::clone(&service))
            .pending()
            .pop()
            .expect("one queued message");
        for _ in 0..4 {
            entry.mark_attempted(id, Utc::now(), "connection refused");
        }
        service.save_outbound(&entry).expect("save");
        (dir, service, id)
    }

    #[test]
    fn a_woken_peer_has_its_backoff_cleared_for_this_pass() {
        // SPEC §8's Monday morning: the peer has just said hello, so the wait
        // it accumulated while it was off is about a machine that is now
        // demonstrably up.
        let (_dir, service, id) = backed_off_entry();
        let outbox = ServiceOutbox::new(Arc::clone(&service));

        let waiting = outbox.pending().pop().expect("still queued");
        assert_eq!(
            waiting.recipients[0].attempts, 4,
            "it is inside a backoff before anybody says hello"
        );

        service.wake_delivery(id);
        let woken = outbox.pending().pop().expect("still queued");
        assert_eq!(
            woken.recipients[0].attempts, 0,
            "a woken recipient is due now"
        );
        assert!(woken.recipients[0].last_attempt.is_none());
    }

    #[test]
    fn waking_a_peer_does_not_forget_that_it_failed() {
        // The cleared backoff is for this pass only. Writing it back would
        // lose the attempt count, and the next failure would start the
        // sequence again from two seconds.
        let (_dir, service, id) = backed_off_entry();
        let outbox = ServiceOutbox::new(Arc::clone(&service));

        service.wake_delivery(id);
        let _ = outbox.pending();

        let on_disk = outbox.pending().pop().expect("still queued");
        assert_eq!(
            on_disk.recipients[0].attempts, 4,
            "the stored entry keeps its history"
        );
    }

    #[test]
    fn a_wake_is_spent_on_the_next_pass_and_not_the_one_after() {
        let (_dir, service, id) = backed_off_entry();
        let outbox = ServiceOutbox::new(Arc::clone(&service));

        service.wake_delivery(id);
        assert_eq!(outbox.pending()[0].recipients[0].attempts, 0);
        assert_eq!(
            outbox.pending()[0].recipients[0].attempts,
            4,
            "a wake is one instruction, not a standing exemption from backoff"
        );
    }

    #[test]
    fn a_peer_that_could_not_be_reached_stops_being_online() {
        // SPEC §5.5: a failed delivery is better evidence than the last
        // hello, and it is what stands in for the ping there is not.
        let (_dir, service, id) = backed_off_entry();
        let outbox = ServiceOutbox::new(Arc::clone(&service));
        service.mark_online(id, Vec::new());
        assert!(service.is_online(id));

        outbox.unreachable(id);

        assert!(!service.is_online(id));
    }

    /// A transport that never gets through, recording each message it was
    /// handed so the ids can be counted afterwards.
    #[derive(Default)]
    struct NeverReaches {
        seen: std::sync::Mutex<Vec<ulid::Ulid>>,
    }

    impl hivemind_net::delivery::Transport for NeverReaches {
        async fn deliver(
            &self,
            _node: NodeId,
            addr: &str,
            message: &hivemind_core::message::Message,
        ) -> Result<(), hivemind_net::client::ClientError> {
            self.seen.lock().expect("lock").push(message.id);
            Err(hivemind_net::client::ClientError::Connect {
                addr: addr.to_owned(),
                source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_hour_of_retries_against_a_peer_that_is_off_leaves_one_record() {
        // #33, the grave half: if a retry made a fresh message rather than
        // reusing the one in `out/`, a peer that stayed off would multiply the
        // sender's own copy — and each new ULID would arrive at the other end
        // as a new message, because idempotency there is on the id (SPEC §8).
        // Counting is the only honest way to ask: this runs the real worker
        // against the real store and counts what is on disk.
        let (dir, service, peer) = queued_for_an_offline_peer();
        let sent_id = service
            .pending_outbound()
            .expect("pending")
            .pop()
            .expect("one queued message")
            .message
            .id;

        let start = tokio::time::Instant::now();
        let clock = move || DateTime::from_timestamp(0, 0).expect("in range") + start.elapsed();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn({
            let outbox = ServiceOutbox::new(Arc::clone(&service));
            async move {
                let transport = NeverReaches::default();
                hivemind_net::delivery::run(&outbox, &transport, clock, async {
                    let _ = stopped.await;
                })
                .await;
                transport
            }
        });

        // Counted as it goes rather than only at the end. Under the bug this
        // is written to catch, every pass would write another envelope and the
        // next pass would retry all of them, so a single check an hour later
        // would take an age to arrive — and a test that hangs says less than
        // one that fails. Time is paused, so the hour costs microseconds.
        let out = dir.path().join("mail").join("out");
        let envelopes = || std::fs::read_dir(&out).expect("out/").count();

        // Seconds in, before the first minute: a record that multiplied per
        // pass would double every tick, and by a minute there would be more
        // files than the test could count.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert_eq!(envelopes(), 1, "one send, one envelope in out/");

        for minute in 1..=60 {
            tokio::time::sleep(std::time::Duration::from_mins(1)).await;
            assert_eq!(
                envelopes(),
                1,
                "one send should still be one envelope in out/ after {minute} min"
            );
        }
        let _ = stop.send(());
        let transport = worker.await.expect("the worker should not panic");

        let handed_over = transport.seen.lock().expect("lock").clone();
        assert!(
            handed_over.len() >= 8,
            "the test proves nothing unless it really retried: {} attempts",
            handed_over.len()
        );
        let distinct: std::collections::HashSet<ulid::Ulid> = handed_over.into_iter().collect();
        assert_eq!(
            distinct,
            std::iter::once(sent_id).collect(),
            "every attempt must carry the message that was sent, not a new one"
        );

        // Files are the source of truth (ADR 0002) and are counted above; the
        // index is asked separately because a duplicate visible only in
        // `hivemind sent` would look exactly like #33 too.
        let listed = service
            .list(&hivemind_core::index::Query {
                mailbox: Some(Mailbox::Out),
                ..hivemind_core::index::Query::default()
            })
            .expect("list");
        assert_eq!(listed.len(), 1, "and one row for it");
        assert_eq!(
            std::fs::read_dir(dir.path().join("mail").join("sent"))
                .expect("sent/")
                .count(),
            0,
            "nothing was delivered, so nothing should have been filed as sent"
        );
        assert!(
            service.pending_outbound().expect("pending")[0].recipients[0].attempts >= 8,
            "the attempts accumulate on the one entry"
        );
        assert!(!service.is_online(peer));
    }
}
