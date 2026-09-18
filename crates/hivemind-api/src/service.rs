//! The service layer shared by the HTTP routers and the MCP adapter.
//!
//! MCP tools call these functions directly rather than making HTTP calls back
//! into the local API, so that every operation has exactly one implementation
//! (SPEC §3).
//!
//! # Why this is synchronous
//!
//! Everything here is a local file read or a `SQLite` query on a single-user
//! machine — microseconds, not milliseconds — so the handlers call it directly
//! rather than through `spawn_blocking`. See
//! `docs/decisions/0008-service-layer-is-synchronous.md` for the reasoning and
//! the escape hatch if that ever stops being true.

use std::path::Path;
use std::sync::Mutex;

use chrono::{SubsecRound as _, Utc};
use hivemind_core::crypto::{Signature, SigningKey};
use hivemind_core::index::{Index, IndexError, Query, Summary};
use hivemind_core::message::{CanonicalError, Kind, Message, MessageError, Recipient, SenderKind};
use hivemind_core::peer::NodeId;
use hivemind_core::peerbook::{
    CertificateDer, Peer, PeerAddr, PeerBook, PeerBookError, PendingPair,
};
use hivemind_core::store::{MailStore, Mailbox, Outbound, RecipientState, StoreError};
use tokio::sync::broadcast;
use ulid::Ulid;

/// How many events a slow subscriber may fall behind before it is dropped.
const EVENT_BUFFER: usize = 256;

/// What the caller wants to send.
///
/// `sender_kind` is deliberately absent: it is decided by the entrypoint, not
/// by the caller (SPEC §4.1).
#[derive(Debug, Clone)]
pub struct Draft {
    /// Who it is for.
    pub to: Vec<Recipient>,
    /// The subject.
    pub subject: String,
    /// The body, as markdown.
    pub body: String,
    /// What it is for.
    pub kind: Kind,
    /// The message being replied to, if any.
    pub in_reply_to: Option<Ulid>,
}

/// Something that happened, for the SSE stream (SPEC §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A message arrived.
    MessageReceived {
        /// Which one.
        id: Ulid,
    },
    /// A message we sent reached all of its recipients.
    MessageDelivered {
        /// Which one.
        id: Ulid,
    },
    /// A message was marked read.
    MessageRead {
        /// Which one.
        id: Ulid,
    },
    /// A node offered to pair and is waiting on a confirmation.
    PairPending {
        /// Which node.
        id: NodeId,
    },
}

impl Event {
    /// The SSE event name (SPEC §7.1).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::MessageReceived { .. } => "message.received",
            Self::MessageDelivered { .. } => "message.delivered",
            Self::MessageRead { .. } => "message.read",
            Self::PairPending { .. } => "pair.pending",
        }
    }

    /// The id the event is about.
    #[must_use]
    pub fn id(&self) -> Ulid {
        match self {
            Self::MessageReceived { id }
            | Self::MessageDelivered { id }
            | Self::MessageRead { id } => *id,
            // A pairing event is not about a message.
            Self::PairPending { .. } => Ulid::nil(),
        }
    }
}

/// Why a service call failed.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// No such message.
    #[error("no message with id {id}")]
    NoSuchMessage {
        /// The id that was asked for.
        id: Ulid,
    },
    /// The draft was not acceptable.
    #[error(transparent)]
    Invalid(#[from] MessageError),
    /// A message must be addressed to somebody.
    #[error("a message needs at least one recipient")]
    NoRecipients,
    /// The store said no.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The index said no.
    #[error(transparent)]
    Index(#[from] IndexError),
    /// The message could not be signed.
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
    /// The address book said no.
    #[error(transparent)]
    PeerBook(#[from] PeerBookError),
    /// The message did not verify against the sender's key.
    #[error("the message signature does not verify")]
    BadSignature,
    /// No such peer.
    #[error("no peer {id}")]
    NoSuchPeer {
        /// The id that was asked for.
        id: String,
    },
    /// The index lock was poisoned by a panic in another thread.
    #[error("the index is unavailable after an earlier failure")]
    Unavailable,
}

/// Everything the local API and the MCP adapter can do.
#[derive(Debug)]
pub struct MailService {
    store: MailStore,
    index: Mutex<Index>,
    peers: Mutex<PeerBook>,
    identity: NodeId,
    certificate: Vec<u8>,
    name: String,
    owner: Option<String>,
    callback_host: String,
    peer_port: u16,
    signing_key: SigningKey,
    events: broadcast::Sender<Event>,
}

/// Who this node says it is when introducing itself (SPEC §7.2).
#[derive(Debug, Clone)]
pub struct NodeDescription {
    /// This node's fingerprint.
    pub id: NodeId,
    /// Its DER-encoded certificate.
    pub certificate: Vec<u8>,
    /// Its display name.
    pub name: String,
    /// The human who owns it.
    pub owner: Option<String>,
    /// The host peers should call back on.
    pub callback_host: String,
    /// The port its peer listener is on.
    pub peer_port: u16,
}

impl MailService {
    /// Open the store and index under `root`, rebuilding the index if it is
    /// missing or stale (SPEC §4.3).
    ///
    /// # Errors
    /// Returns [`ServiceError::Store`] or [`ServiceError::Index`] if the data
    /// directory cannot be opened.
    pub fn open(
        root: &Path,
        node: NodeDescription,
        signing_key: SigningKey,
    ) -> Result<Self, ServiceError> {
        let store = MailStore::open(root.join("mail"))?;
        let mut index = Index::open(&root.join("index.db"))?;
        // Cheap when the index was already current, because it is only the
        // files that exist; correct when it was not.
        index.rebuild_from(&store)?;
        let peers = PeerBook::load(root)?;

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Ok(Self {
            store,
            index: Mutex::new(index),
            peers: Mutex::new(peers),
            identity: node.id,
            certificate: node.certificate,
            name: node.name,
            owner: node.owner,
            callback_host: node.callback_host,
            peer_port: node.peer_port,
            signing_key,
            events,
        })
    }

    /// This node's display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The human who owns this machine, if they said.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// This node's certificate, which peers pin.
    #[must_use]
    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    fn peers(&self) -> Result<std::sync::MutexGuard<'_, PeerBook>, ServiceError> {
        self.peers.lock().map_err(|_| ServiceError::Unavailable)
    }

    /// This node's identity.
    #[must_use]
    pub fn identity(&self) -> NodeId {
        self.identity
    }

    /// Listen for events (SPEC §7.1).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn index(&self) -> Result<std::sync::MutexGuard<'_, Index>, ServiceError> {
        self.index.lock().map_err(|_| ServiceError::Unavailable)
    }

    /// Write and locally deliver a message.
    ///
    /// The message is written to `out/` before this returns, so sending never
    /// blocks on the network (SPEC §8). Recipients that are this node are
    /// delivered immediately; remote delivery arrives with peers in M3.
    ///
    /// # Errors
    /// [`ServiceError::NoRecipients`] for an unaddressed draft,
    /// [`ServiceError::Invalid`] if it breaks the limits in SPEC §4.1.
    pub fn send(&self, draft: Draft, sender_kind: SenderKind) -> Result<Message, ServiceError> {
        if draft.to.is_empty() {
            return Err(ServiceError::NoRecipients);
        }

        let id = Ulid::generate();
        let mut message = Message {
            id,
            // A reply joins the thread it answers; anything else starts one.
            thread_id: match draft.in_reply_to {
                Some(parent) => self.thread_id_of(parent)?,
                None => id,
            },
            in_reply_to: draft.in_reply_to,
            from: self.identity,
            to: draft.to,
            subject: draft.subject,
            body: draft.body,
            kind: draft.kind,
            sender_kind,
            attachments: Vec::new(),
            // Millisecond precision, because that is what the canonical
            // encoding signs (ADR 0007).
            sent_at: Utc::now().trunc_subsecs(3),
            received_at: None,
            signature: Signature::from_bytes([0u8; 64]),
        };

        message.validate()?;
        message.sign(&self.signing_key)?;

        // SPEC §8: recipients are expanded at send time and the expansion is
        // stored, so a peer that pairs tomorrow does not receive today's
        // message to `everyone`.
        let expanded = self.expand_recipients(&message.to)?;

        // Local delivery happens here rather than over a socket: a node does
        // not need to be paired with itself.
        if expanded.contains(&self.identity) {
            let mut received = message.clone();
            received.received_at = Some(Utc::now().trunc_subsecs(3));
            self.put(Mailbox::New, &received)?;
            let _ = self.events.send(Event::MessageReceived { id });
        }

        let outbound = Outbound {
            recipients: expanded
                .into_iter()
                .filter(|node| *node != self.identity)
                .map(RecipientState::pending)
                .collect(),
            message: message.clone(),
        };

        // Always through the outbox, even when there is nothing to deliver.
        // One path means a crash anywhere in it leaves the same recoverable
        // state, and the sender can see their own message either way.
        self.store.put_outbound(&outbound)?;
        self.index()?.upsert(Mailbox::Out, &message)?;

        if outbound.is_complete() {
            self.complete_delivery(&outbound)?;
        }

        Ok(message)
    }

    /// Reply to a message, inheriting its thread.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if the parent is unknown.
    pub fn reply(
        &self,
        parent: Ulid,
        body: String,
        sender_kind: SenderKind,
    ) -> Result<Message, ServiceError> {
        let (_, original) = self.get(parent)?;
        let subject = if original.subject.starts_with("Re: ") {
            original.subject.clone()
        } else {
            format!("Re: {}", original.subject)
        };

        self.send(
            Draft {
                to: vec![Recipient::Node(original.from)],
                subject,
                body,
                kind: Kind::Message,
                in_reply_to: Some(parent),
            },
            sender_kind,
        )
    }

    /// Run a listing query.
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn list(&self, query: &Query) -> Result<Vec<Summary>, ServiceError> {
        Ok(self.index()?.search(query)?)
    }

    /// Fetch one message, wherever it is.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if no mailbox holds it.
    pub fn get(&self, id: Ulid) -> Result<(Mailbox, Message), ServiceError> {
        // Prefer the received copy: a message addressed to its own sender
        // exists twice, and "read this" means the one in the inbox.
        for mailbox in [Mailbox::New, Mailbox::Cur, Mailbox::Sent, Mailbox::Out] {
            match self.store.get(mailbox, id) {
                Ok(message) => return Ok((mailbox, message)),
                Err(StoreError::NotFound { .. }) => {}
                Err(other) => return Err(other.into()),
            }
        }
        Err(ServiceError::NoSuchMessage { id })
    }

    /// Every message in a thread, oldest first.
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn thread(&self, thread_id: Ulid) -> Result<Vec<Summary>, ServiceError> {
        let mut found = self.index()?.search(&Query {
            thread: Some(thread_id),
            ..Query::default()
        })?;
        // A conversation reads forwards.
        found.reverse();
        Ok(found)
    }

    /// Move a message from `new` to `cur` (SPEC §7.1).
    ///
    /// Marking an already-read message read again is not an error: it is what
    /// a second click does.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if it is in neither mailbox.
    pub fn mark_read(&self, id: Ulid) -> Result<(), ServiceError> {
        match self.store.move_to(Mailbox::New, Mailbox::Cur, id) {
            Ok(()) => {
                self.index()?.set_mailbox(id, Mailbox::New, Mailbox::Cur)?;
                let _ = self.events.send(Event::MessageRead { id });
                Ok(())
            }
            Err(StoreError::NotFound { .. }) => {
                // Already read is success; never received is not.
                match self.store.get(Mailbox::Cur, id) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(ServiceError::NoSuchMessage { id }),
                }
            }
            Err(other) => Err(other.into()),
        }
    }

    /// How many unread messages there are (SPEC §9.3).
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn unread_count(&self) -> Result<u64, ServiceError> {
        Ok(self.index()?.unread_count()?)
    }

    /// Throw the index away and rebuild it from the mail files (SPEC §10).
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn reindex(&self) -> Result<(), ServiceError> {
        self.index()?.rebuild_from(&self.store)?;
        Ok(())
    }

    // ------------------------------------------------------------- peers ---

    /// How this node introduces itself (SPEC §7.2).
    #[must_use]
    pub fn own_handshake(&self) -> crate::peer::Handshake {
        crate::peer::Handshake {
            id: self.identity.to_string(),
            name: self.name.clone(),
            owner: self.owner.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            callback_host: self.callback_host.clone(),
            callback_port: self.peer_port,
            // Reserved for v2 (SPEC §12). Sending nothing today keeps the
            // field's meaning open.
            gossip: None,
        }
    }

    /// Is this node allowed to send us mail?
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn is_paired(&self, id: NodeId) -> Result<bool, ServiceError> {
        Ok(self.peers()?.is_paired(id))
    }

    /// Every paired peer.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn paired_peers(&self) -> Result<Vec<Peer>, ServiceError> {
        Ok(self.peers()?.peers().cloned().collect())
    }

    /// Everything waiting on a confirmation.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn pending_pairs(&self) -> Result<Vec<PendingPair>, ServiceError> {
        Ok(self.peers()?.pending().cloned().collect())
    }

    /// Record that a node introduced itself, without trusting it yet.
    ///
    /// # Errors
    /// [`ServiceError::PeerBook`] if the book cannot be written.
    pub fn record_pairing_offer(
        &self,
        id: NodeId,
        handshake: &crate::peer::Handshake,
        certificate: Vec<u8>,
        addr: PeerAddr,
    ) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;

        // Already paired: this is a peer saying hello again, not an offer.
        // Refresh where it can be reached and leave the trust decision alone.
        if let Some(peer) = peers.peer_mut(id) {
            peer.learn_addr(addr);
            peer.last_seen = Some(Utc::now());
            return peers.save().map_err(Into::into);
        }

        peers.insert_pending(PendingPair {
            id,
            name: handshake.name.clone(),
            owner: handshake.owner.clone(),
            certificate: CertificateDer::new(certificate),
            addr,
            first_seen: Utc::now(),
            confirmed_by_us: false,
        });
        peers.save()?;
        let _ = self.events.send(Event::PairPending { id });
        Ok(())
    }

    /// Confirm a pending pair from this side (SPEC §6.2).
    ///
    /// Promotes it to a real peer once *we* have agreed; the other side does
    /// the same independently, and neither will accept mail until it has.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if nothing is pending for that id.
    pub fn confirm_pair(&self, id: NodeId) -> Result<Peer, ServiceError> {
        let mut peers = self.peers()?;
        let Some(pending) = peers.remove_pending(id) else {
            return Err(ServiceError::NoSuchPeer { id: id.to_string() });
        };

        let peer = Peer {
            id: pending.id,
            name: pending.name,
            owner: pending.owner,
            certificate: pending.certificate,
            addrs: vec![pending.addr],
            paired_at: Utc::now(),
            last_seen: None,
        };
        peers.insert_peer(peer.clone());
        peers.save()?;
        Ok(peer)
    }

    /// Forget a peer.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if it was not there.
    pub fn remove_peer(&self, id: NodeId) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;
        if !peers.remove_peer(id) {
            peers.remove_pending(id);
        }
        peers.save().map_err(Into::into)
    }

    /// The certificates TLS should accept, for rebuilding the trust set.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn trusted_certificates(&self) -> Result<Vec<(NodeId, Vec<u8>)>, ServiceError> {
        Ok(self.peers()?.acceptable_certificates())
    }

    /// Accept a message delivered by a paired peer.
    ///
    /// Idempotent on message id, because redelivery is always safe (SPEC §8).
    ///
    /// # Errors
    /// [`ServiceError::BadSignature`] if it does not verify against the
    /// sender's key.
    pub fn receive(&self, from: NodeId, mut message: Message) -> Result<Ulid, ServiceError> {
        // The signature is checked against the certificate we pinned, not
        // against anything in the message: a peer cannot nominate its own key.
        let verifying_key = {
            let peers = self.peers()?;
            let peer = peers.peer(from).ok_or_else(|| ServiceError::NoSuchPeer {
                id: from.to_string(),
            })?;
            verifying_key_from_certificate(peer.certificate.as_bytes())
                .ok_or(ServiceError::BadSignature)?
        };
        message
            .verify(&verifying_key)
            .map_err(|_| ServiceError::BadSignature)?;

        let id = message.id;

        // Idempotent: a redelivery of something we already hold is a success,
        // and must not reset its read state by rewriting it into `new`.
        if self.store.get(Mailbox::New, id).is_ok() || self.store.get(Mailbox::Cur, id).is_ok() {
            return Ok(id);
        }

        message.received_at = Some(Utc::now().trunc_subsecs(3));
        self.put(Mailbox::New, &message)?;

        if let Ok(mut peers) = self.peers()
            && let Some(peer) = peers.peer_mut(from)
        {
            peer.last_seen = Some(Utc::now());
            let _ = peers.save();
        }

        let _ = self.events.send(Event::MessageReceived { id });
        Ok(id)
    }

    /// Which nodes a recipient list actually reaches, at this moment.
    ///
    /// Expanded at send time and stored with the message, so a peer paired
    /// tomorrow does not retroactively receive today's mail (SPEC §8).
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn expand_recipients(&self, to: &[Recipient]) -> Result<Vec<NodeId>, ServiceError> {
        let peers = self.peers()?;
        let mut out: Vec<NodeId> = Vec::new();

        for recipient in to {
            match recipient {
                Recipient::Node(id) => out.push(*id),
                Recipient::Owner(owner) => {
                    out.extend(peers.peers_owned_by(owner).map(|p| p.id));
                    // `everyone` and an owner name can both name this machine.
                    if self
                        .owner
                        .as_deref()
                        .is_some_and(|o| o.eq_ignore_ascii_case(owner))
                    {
                        out.push(self.identity);
                    }
                }
                Recipient::Everyone => {
                    out.extend(peers.peers().map(|p| p.id));
                    out.push(self.identity);
                }
            }
        }

        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn put(&self, mailbox: Mailbox, message: &Message) -> Result<(), ServiceError> {
        // File first, index second. A crash between them leaves a stale index,
        // which the next rebuild corrects; the other order would invent a
        // message that does not exist (ADR 0002).
        self.store.put(mailbox, message)?;
        self.index()?.upsert(mailbox, message)?;
        Ok(())
    }

    /// Every recipient has it: move `out/` → `sent/` and say so (SPEC §8).
    fn complete_delivery(&self, outbound: &Outbound) -> Result<(), ServiceError> {
        let id = outbound.message.id;
        self.store.promote_to_sent(outbound)?;
        self.index()?.set_mailbox(id, Mailbox::Out, Mailbox::Sent)?;
        let _ = self.events.send(Event::MessageDelivered { id });
        Ok(())
    }

    fn thread_id_of(&self, parent: Ulid) -> Result<Ulid, ServiceError> {
        let (_, message) = self.get(parent)?;
        Ok(message.thread_id)
    }
}

/// Pull the Ed25519 public key out of a DER certificate.
///
/// The `SubjectPublicKeyInfo` of an Ed25519 certificate ends with the 32-byte
/// key, and the OID that precedes it is fixed. Scanning for that OID avoids
/// pulling in a full X.509 parser for one field — and a wrong answer here
/// cannot forge anything, it can only fail to verify.
fn verifying_key_from_certificate(der: &[u8]) -> Option<hivemind_core::crypto::VerifyingKey> {
    /// `AlgorithmIdentifier` for Ed25519: SEQUENCE(6) { OID 1.3.101.112 }.
    const ED25519_SPKI_PREFIX: &[u8] =
        &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

    let start = der
        .windows(ED25519_SPKI_PREFIX.len())
        .position(|window| window == ED25519_SPKI_PREFIX)?
        + ED25519_SPKI_PREFIX.len();
    let bytes: [u8; 32] = der.get(start..start + 32)?.try_into().ok()?;
    hivemind_core::crypto::VerifyingKey::from_bytes(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bits of a node description the store tests do not care about.
    fn describe(id: NodeId) -> NodeDescription {
        NodeDescription {
            id,
            certificate: b"this node".to_vec(),
            name: "test".to_owned(),
            owner: None,
            callback_host: "127.0.0.1".to_owned(),
            peer_port: 8400,
        }
    }

    fn service() -> (tempfile::TempDir, MailService) {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let identity = NodeId::from_certificate_der(b"this node");
        let service =
            MailService::open(dir.path(), describe(identity), key).expect("service opens");
        (dir, service)
    }

    fn draft_to_self(service: &MailService, subject: &str, body: &str) -> Draft {
        Draft {
            to: vec![Recipient::Node(service.identity())],
            subject: subject.to_owned(),
            body: body.to_owned(),
            kind: Kind::Message,
            in_reply_to: None,
        }
    }

    #[test]
    fn a_message_sent_to_ourselves_arrives_in_the_inbox() {
        // SPEC §14's M1 round trip: one daemon, send to self, read it back.
        let (_dir, service) = service();
        let sent = service
            .send(
                draft_to_self(&service, "dashboard PR", "take a look"),
                SenderKind::Human,
            )
            .expect("send");

        assert_eq!(service.unread_count().expect("count"), 1);
        let (mailbox, received) = service.get(sent.id).expect("get");
        assert_eq!(mailbox, Mailbox::New);
        assert_eq!(received.subject, "dashboard PR");
    }

    #[test]
    fn a_sent_message_is_signed_by_this_node_and_verifies() {
        let (_dir, service) = service();
        let sent = service
            .send(draft_to_self(&service, "signed", "body"), SenderKind::Human)
            .expect("send");

        let key = SigningKey::from_bytes(&[11u8; 32]);
        assert!(sent.verify(&key.verifying_key()).is_ok());
        assert_eq!(sent.from, service.identity());
    }

    #[test]
    fn the_entrypoint_decides_sender_kind_not_the_caller() {
        // SPEC §4.1: the field is set by where the message came in, and a
        // caller cannot claim to be a human.
        let (_dir, service) = service();
        let agent = service
            .send(
                draft_to_self(&service, "from mcp", "body"),
                SenderKind::Agent,
            )
            .expect("send");
        assert_eq!(agent.sender_kind, SenderKind::Agent);

        let human = service
            .send(
                draft_to_self(&service, "from cli", "body"),
                SenderKind::Human,
            )
            .expect("send");
        assert_eq!(human.sender_kind, SenderKind::Human);
    }

    #[test]
    fn a_message_with_no_recipients_is_refused() {
        let (_dir, service) = service();
        let draft = Draft {
            to: Vec::new(),
            subject: "nobody".to_owned(),
            body: "body".to_owned(),
            kind: Kind::Message,
            in_reply_to: None,
        };
        assert!(matches!(
            service.send(draft, SenderKind::Human),
            Err(ServiceError::NoRecipients)
        ));
    }

    #[test]
    fn a_draft_that_breaks_the_subject_limit_is_refused_before_it_is_stored() {
        let (_dir, service) = service();
        let mut draft = draft_to_self(&service, "x", "body");
        draft.subject = "s".repeat(201);

        assert!(matches!(
            service.send(draft, SenderKind::Human),
            Err(ServiceError::Invalid(_))
        ));
        assert_eq!(
            service.list(&Query::default()).expect("list").len(),
            0,
            "a refused draft must leave nothing behind"
        );
    }

    #[test]
    fn marking_a_message_read_moves_it_out_of_the_unread_count() {
        let (_dir, service) = service();
        let sent = service
            .send(draft_to_self(&service, "unread", "body"), SenderKind::Human)
            .expect("send");
        assert_eq!(service.unread_count().expect("count"), 1);

        service.mark_read(sent.id).expect("mark read");
        assert_eq!(service.unread_count().expect("count"), 0);
        assert_eq!(service.get(sent.id).expect("get").0, Mailbox::Cur);
    }

    #[test]
    fn marking_an_already_read_message_read_again_is_not_an_error() {
        let (_dir, service) = service();
        let sent = service
            .send(draft_to_self(&service, "unread", "body"), SenderKind::Human)
            .expect("send");
        service.mark_read(sent.id).expect("first");
        service
            .mark_read(sent.id)
            .expect("a second click is not a failure");
    }

    #[test]
    fn marking_a_message_we_never_received_read_is_an_error() {
        let (_dir, service) = service();
        assert!(matches!(
            service.mark_read(Ulid::generate()),
            Err(ServiceError::NoSuchMessage { .. })
        ));
    }

    #[test]
    fn a_reply_joins_the_thread_it_answers() {
        let (_dir, service) = service();
        let root = service
            .send(
                draft_to_self(&service, "dashboard PR", "take a look"),
                SenderKind::Human,
            )
            .expect("send");

        let reply = service
            .reply(root.id, "on it".to_owned(), SenderKind::Human)
            .expect("reply");

        assert_eq!(reply.thread_id, root.thread_id);
        assert_eq!(reply.in_reply_to, Some(root.id));
        assert_eq!(reply.subject, "Re: dashboard PR");
    }

    #[test]
    fn replying_to_a_reply_does_not_stack_re_prefixes() {
        let (_dir, service) = service();
        let root = service
            .send(draft_to_self(&service, "lunch", "?"), SenderKind::Human)
            .expect("send");
        let first = service
            .reply(root.id, "yes".to_owned(), SenderKind::Human)
            .expect("reply");
        let second = service
            .reply(first.id, "1pm".to_owned(), SenderKind::Human)
            .expect("reply");

        assert_eq!(second.subject, "Re: lunch");
    }

    #[test]
    fn a_thread_reads_oldest_first() {
        let (_dir, service) = service();
        let root = service
            .send(draft_to_self(&service, "lunch", "?"), SenderKind::Human)
            .expect("send");
        service
            .reply(root.id, "yes".to_owned(), SenderKind::Human)
            .expect("reply");

        let thread = service.thread(root.thread_id).expect("thread");
        assert!(thread.len() >= 2);
        assert!(
            thread.windows(2).all(|w| w[0].sent_at <= w[1].sent_at),
            "a conversation reads forwards"
        );
    }

    #[test]
    fn replying_to_a_message_that_does_not_exist_is_an_error() {
        let (_dir, service) = service();
        assert!(matches!(
            service.reply(Ulid::generate(), "hello".to_owned(), SenderKind::Human),
            Err(ServiceError::NoSuchMessage { .. })
        ));
    }

    #[test]
    fn sending_emits_received_and_delivered_events() {
        let (_dir, service) = service();
        let mut events = service.subscribe();

        let sent = service
            .send(
                draft_to_self(&service, "watch me", "body"),
                SenderKind::Human,
            )
            .expect("send");

        let first = events.try_recv().expect("an event");
        let second = events.try_recv().expect("a second event");
        assert_eq!(first, Event::MessageReceived { id: sent.id });
        assert_eq!(second, Event::MessageDelivered { id: sent.id });
        assert_eq!(first.name(), "message.received");
    }

    #[test]
    fn the_index_survives_being_deleted_and_the_mail_does_not() {
        // ADR 0002: index.db is disposable. Reopening rebuilds it from mail/.
        let dir = tempfile::tempdir().expect("temp dir");
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let identity = NodeId::from_certificate_der(b"this node");

        let sent = {
            let service =
                MailService::open(dir.path(), describe(identity), key.clone()).expect("open");
            service
                .send(
                    Draft {
                        to: vec![Recipient::Node(identity)],
                        subject: "survives".to_owned(),
                        body: "body".to_owned(),
                        kind: Kind::Message,
                        in_reply_to: None,
                    },
                    SenderKind::Human,
                )
                .expect("send")
        };

        std::fs::remove_file(dir.path().join("index.db")).expect("delete the index");

        let service = MailService::open(dir.path(), describe(identity), key).expect("reopen");
        assert_eq!(service.unread_count().expect("count"), 1);
        assert_eq!(service.get(sent.id).expect("get").1.subject, "survives");
    }
}
