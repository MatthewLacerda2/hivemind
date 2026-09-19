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

use chrono::{DateTime, SubsecRound as _, Utc};
use hivemind_core::blobs::{BlobError, BlobStore, check_attachment_name};
use hivemind_core::config::DEFAULT_PEER_PORT;
use hivemind_core::crypto::{Sha256Digest, Signature, SigningKey};
use hivemind_core::index::{Index, IndexError, Query, Summary};
use hivemind_core::message::{
    AttachmentRef, CanonicalError, Kind, Message, MessageError, Recipient, SenderKind,
};
use hivemind_core::peer::NodeId;
use hivemind_core::peerbook::{
    AddrSource, CertificateDer, Peer, PeerAddr, PeerBook, PeerBookError,
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
    /// Local files to send with it. Each is copied into the blob store, and
    /// only its name travels (SPEC §9.1).
    pub attachments: Vec<std::path::PathBuf>,
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
    /// A node outside the group was seen for the first time (SPEC §5.4).
    PeerSeen {
        /// Which node.
        id: NodeId,
    },
    /// A peer said hello after being absent (SPEC §5.5).
    PeerOnline {
        /// Which node.
        id: NodeId,
    },
    /// A peer stopped answering, or could not be delivered to (SPEC §5.5).
    PeerOffline {
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
            Self::PeerSeen { .. } => "peer.seen",
            Self::PeerOnline { .. } => "peer.online",
            Self::PeerOffline { .. } => "peer.offline",
        }
    }

    /// The id the event is about.
    #[must_use]
    pub fn id(&self) -> Ulid {
        match self {
            Self::MessageReceived { id }
            | Self::MessageDelivered { id }
            | Self::MessageRead { id } => *id,
            // A peer event is not about a message.
            Self::PeerSeen { .. } | Self::PeerOnline { .. } | Self::PeerOffline { .. } => {
                Ulid::nil()
            }
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
    /// The blob store said no.
    #[error(transparent)]
    Blob(#[from] BlobError),
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
    /// The host could not be reached, or refused the handshake.
    #[error(transparent)]
    Peer(#[from] hivemind_net::client::ClientError),
    /// The host answered, but not as the node it claims to be.
    #[error("{0}")]
    IdentityMismatch(String),
    /// The other node could not prove the group key, or this node has none
    /// (SPEC §6.2). Says which.
    #[error("{0}")]
    NotInGroup(String),
    /// This node is already in a group, and the caller did not ask to replace
    /// it (SPEC §6.2).
    #[error("this node is already in a group; pass `replace` to leave it for this one")]
    AlreadyInGroup,
    /// The group code or `group.toml` said no.
    #[error(transparent)]
    Group(hivemind_core::group::GroupError),
    /// The index lock was poisoned by a panic in another thread.
    #[error("the index is unavailable after an earlier failure")]
    Unavailable,
}

mod group;
mod peering;
mod presence;

pub use group::{GroupStatus, MAX_SEEN, SeenNode};
pub use peering::Met;
pub use presence::Presence;

/// Everything the local API and the MCP adapter can do.
#[derive(Debug)]
pub struct MailService {
    store: MailStore,
    blobs: BlobStore,
    index: Mutex<Index>,
    peers: Mutex<PeerBook>,
    /// The group key, if this node is in a group (ADR 0013).
    group: Mutex<Option<hivemind_core::group::Group>>,
    /// Nodes outside the group, for `hivemind peers`. In memory only.
    seen: Mutex<std::collections::BTreeMap<NodeId, SeenNode>>,
    /// Who has said hello lately (SPEC §5.5). In memory only: online is not a
    /// fact about a peer, it is a statement about this minute.
    presence: Mutex<std::collections::HashMap<NodeId, Presence>>,
    /// Peers the delivery worker should try now rather than at the end of
    /// their backoff, because one of them has just been heard from.
    woken: Mutex<std::collections::HashSet<NodeId>>,
    /// Addresses worth a hello next round, from gossip and from hints.
    candidates: Mutex<std::collections::BTreeSet<String>>,
    /// Nodes to pass on as "X is up", and when we heard from each. They age
    /// out after one interval rather than being consumed, so that every peer
    /// greeted in the next round hears the news once.
    hints: Mutex<std::collections::BTreeMap<NodeId, DateTime<Utc>>>,
    /// How often presence runs, which is also how long a hello counts for —
    /// a peer is online while its last one is younger than two of these.
    presence_interval: std::time::Duration,
    /// Where `group.toml` lives.
    home: std::path::PathBuf,
    identity: NodeId,
    certificate: Vec<u8>,
    tls: hivemind_net::tls::LocalIdentity,
    name: String,
    owner: Option<String>,
    callback_host: String,
    peer_port: u16,
    max_attachment_bytes: u64,
    inline_max_bytes: u64,
    prefetch: bool,
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
    /// Its PKCS#8 private key, used to present that certificate when dialling
    /// a peer. Never leaves this process.
    pub private_key: Vec<u8>,
    /// Its display name.
    pub name: String,
    /// The human who owns it.
    pub owner: Option<String>,
    /// The host peers should call back on.
    pub callback_host: String,
    /// The port its peer listener is on.
    pub peer_port: u16,
    /// The largest attachment this node accepts (SPEC §6.3).
    pub max_attachment_bytes: u64,
    /// Attachments at or below this size travel with the message (SPEC §8).
    pub inline_max_bytes: u64,
    /// Fetch lazy attachments as soon as a message arrives, rather than on
    /// first access (SPEC §8).
    pub prefetch: bool,
    /// Seconds between presence rounds (SPEC §5.5), and half the window a
    /// hello keeps a peer looking online for.
    pub presence_interval: u64,
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
        let blobs = BlobStore::open(root.join("blobs"))?;
        let mut index = Index::open(&root.join("index.db"))?;
        // Cheap when the index was already current, because it is only the
        // files that exist; correct when it was not.
        index.rebuild_from(&store)?;
        let peers = PeerBook::load(root)?;
        let group = hivemind_core::group::Group::load(root)?;

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Ok(Self {
            store,
            blobs,
            index: Mutex::new(index),
            peers: Mutex::new(peers),
            group: Mutex::new(group),
            seen: Mutex::new(std::collections::BTreeMap::new()),
            presence: Mutex::new(std::collections::HashMap::new()),
            woken: Mutex::new(std::collections::HashSet::new()),
            candidates: Mutex::new(std::collections::BTreeSet::new()),
            hints: Mutex::new(std::collections::BTreeMap::new()),
            presence_interval: std::time::Duration::from_secs(node.presence_interval),
            home: root.to_path_buf(),
            identity: node.id,
            tls: hivemind_net::tls::LocalIdentity::new(node.certificate.clone(), node.private_key),
            certificate: node.certificate,
            name: node.name,
            owner: node.owner,
            callback_host: node.callback_host,
            peer_port: node.peer_port,
            max_attachment_bytes: node.max_attachment_bytes,
            inline_max_bytes: node.inline_max_bytes,
            prefetch: node.prefetch,
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

    /// The port this node's peer listener is on.
    #[must_use]
    pub fn peer_port(&self) -> u16 {
        self.peer_port
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
            attachments: self.take_attachments(&draft.attachments)?,
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

        // A `to` that resolves to nobody is an empty `to` with a better
        // disguise, and used to be filed straight into `sent/` — the message
        // went nowhere and reported success (#19). One typo in a person's name
        // was enough.
        if expanded.is_empty() {
            return Err(ServiceError::NoRecipients);
        }

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
        attachments: Vec<std::path::PathBuf>,
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
                attachments,
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

    /// Turn what somebody typed in a `To` box into a recipient.
    ///
    /// Three forms, in this order:
    ///
    /// 1. the full `hm1:` fingerprint,
    /// 2. `everyone`,
    /// 3. a **short id** of a peer we know, which is the form the interface
    ///    teaches — `peers`, `status` and `init` all print it and `pair` takes
    ///    it — and which used to be read as a person's name, delivering the
    ///    message to nobody and saying it was sent (#19),
    /// 4. anything else: a person's name.
    ///
    /// An owner's name wins over a short id that looks like it. That ordering
    /// is decided rather than discovered: what somebody called themselves is
    /// what they meant by it.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] for a short id that matches more than one
    /// peer, or for an empty string. Guessing which peer gets somebody's mail
    /// is not a thing to do by accident.
    pub fn parse_recipient(&self, typed: &str) -> Result<Recipient, ServiceError> {
        let typed = typed.trim();
        if typed.is_empty() {
            return Err(ServiceError::NoSuchPeer { id: String::new() });
        }

        if let Ok(id) = typed.parse::<NodeId>() {
            return Ok(Recipient::Node(id));
        }
        if typed.eq_ignore_ascii_case("everyone") {
            return Ok(Recipient::Everyone);
        }

        let peers = self.peers()?;

        // A person's name first, including this machine's own owner.
        let names_somebody = peers.peers().any(|p| {
            p.owner
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(typed))
        }) || self
            .owner
            .as_deref()
            .is_some_and(|o| o.eq_ignore_ascii_case(typed));
        if names_somebody {
            return Ok(Recipient::Owner(typed.to_owned()));
        }

        let mut matches = peers
            .peers()
            .map(|p| p.id)
            .chain(std::iter::once(self.identity))
            .filter(|id| id.short().eq_ignore_ascii_case(typed));

        match (matches.next(), matches.next()) {
            (Some(id), None) => Ok(Recipient::Node(id)),
            (Some(_), Some(_)) => Err(ServiceError::NoSuchPeer {
                id: typed.to_owned(),
            }),
            // Not a fingerprint, not `everyone`, not a short id we know: a
            // person we have not met. Expansion will then find nobody, and
            // `send` refuses rather than filing it as sent.
            (None, _) => Ok(Recipient::Owner(typed.to_owned())),
        }
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

    /// The certificate pinned for one peer, if we are paired with it.
    ///
    /// `None` also when the address book is unreadable, because "do not trust
    /// this certificate" is the safe answer to "I cannot tell".
    #[must_use]
    pub fn certificate_of(&self, node: NodeId) -> Option<Vec<u8>> {
        let peers = self.peers().ok()?;
        peers
            .peer(node)
            .map(|peer| peer.certificate.as_bytes().to_vec())
    }

    /// Copy local files into the blob store and describe them (SPEC §8).
    ///
    /// Each file is hashed as it is copied, so an attachment sent twice — or
    /// sent by two people — is stored once. Whether it travels with the
    /// message or is fetched on demand is decided here and recorded in the
    /// signed message, so the recipient knows which to expect.
    fn take_attachments(
        &self,
        paths: &[std::path::PathBuf],
    ) -> Result<Vec<AttachmentRef>, ServiceError> {
        let mut refs = Vec::with_capacity(paths.len());
        // Bounded so that one message cannot become an unboundedly large
        // delivery. Past the budget, otherwise-inline files ship as refs and
        // the recipient fetches them; nothing is refused for being numerous.
        let mut inline_budget = self.inline_max_bytes.saturating_mul(INLINE_BUDGET_MULTIPLE);

        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or(BlobError::UnsafeName("an attachment needs a file name"))?;
            check_attachment_name(&name)?;

            let (sha256, size) = self.blobs.put_file(path, self.max_attachment_bytes)?;

            let inline = size <= self.inline_max_bytes && size <= inline_budget;
            if inline {
                inline_budget -= size;
            }

            refs.push(AttachmentRef {
                name,
                size,
                sha256,
                // Guessed from the extension. Advisory only: a recipient that
                // acts on it rather than on the bytes is trusting the sender.
                mime: mime_for(path),
                inline,
            });
        }
        Ok(refs)
    }

    /// Whether this node fetches lazy attachments before they are asked for.
    #[must_use]
    pub fn prefetches(&self) -> bool {
        self.prefetch
    }

    /// Attachments of `message` that are not here yet (SPEC §8).
    ///
    /// What `prefetch = true` acts on, and what `hivemind status` would report
    /// as still outstanding.
    #[must_use]
    pub fn missing_attachments(&self, message: &Message) -> Vec<Sha256Digest> {
        message
            .attachments
            .iter()
            .map(|a| a.sha256)
            .filter(|digest| !self.blobs.has(digest))
            .collect()
    }

    /// Get an attachment onto local disk, fetching it if we do not hold it.
    ///
    /// SPEC §8: a file too large to travel with its message is fetched on
    /// first access. The fetch resumes from whatever an interrupted attempt
    /// left behind, so a laptop closed mid-transfer costs the bytes not yet
    /// received rather than all of them.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if the message is unknown,
    /// [`ServiceError::Blob`] if the message declares no such attachment,
    /// [`ServiceError::NoSuchPeer`] if the sender is no longer paired, or
    /// [`ServiceError::Peer`] if the fetch fails.
    pub async fn fetch_attachment(
        &self,
        message_id: Ulid,
        digest: Sha256Digest,
    ) -> Result<std::path::PathBuf, ServiceError> {
        if self.blobs.has(&digest) {
            return Ok(self.blobs.path_of(&digest));
        }

        let (_, message) = self.get(message_id)?;
        // Only an attachment this message declares. Otherwise the endpoint
        // would be a way to ask a peer for any file it happens to hold.
        if !message.attachments.iter().any(|a| a.sha256 == digest) {
            return Err(BlobError::NotFound {
                digest: digest.to_hex(),
            }
            .into());
        }

        // Our own outgoing mail: if the blob is gone it is gone, and there is
        // nobody to ask for it.
        if message.from == self.identity {
            return Err(BlobError::NotFound {
                digest: digest.to_hex(),
            }
            .into());
        }

        self.download_from(message.from, &digest).await?;
        Ok(self.blobs.path_of(&digest))
    }

    /// Fetch one blob from the peer that sent it, resuming if we can.
    async fn download_from(&self, from: NodeId, digest: &Sha256Digest) -> Result<(), ServiceError> {
        // SPEC §13.1. The blob's digest stands in for a message id here: an
        // attachment is fetched by content, and the same bytes can belong to
        // several messages.
        let span = tracing::info_span!(
            "fetch_blob",
            peer = %from.short(),
            blob = %digest.to_hex()
        );
        let _entered = span.enter();

        let certificate = self
            .certificate_of(from)
            .ok_or_else(|| ServiceError::NoSuchPeer {
                id: from.to_string(),
            })?;
        let addresses = self.peer_addresses(from)?;
        if addresses.is_empty() {
            return Err(ServiceError::NoSuchPeer {
                id: from.to_string(),
            });
        }

        let client = hivemind_net::client::PeerClient::pinned(
            &self.tls,
            hivemind_net::tls::TrustedPeers::new(vec![(from, certificate)]),
        )?;
        let path = format!("/peer/v1/blobs/{}", digest.to_hex());

        let mut last = None;
        for addr in addresses {
            let from_byte = self.blobs.partial_len(digest);
            let result = client
                .download(&addr, &path, from_byte, |chunk| {
                    self.blobs
                        .append_partial(digest, chunk)
                        .map(|_| ())
                        .map_err(|e| hivemind_net::client::ClientError::Http {
                            addr: String::new(),
                            reason: e.to_string(),
                        })
                })
                .await;

            match result {
                Ok(_) => {
                    // Verified here rather than trusted: the bytes are thrown
                    // away if they are not what the signed message named.
                    self.blobs.finish_partial(digest)?;
                    return Ok(());
                }
                Err(error) => last = Some(error),
            }
        }

        Err(last.map_or(
            ServiceError::NoSuchPeer {
                id: from.to_string(),
            },
            ServiceError::Peer,
        ))
    }

    /// Store the blobs that arrived with a message (SPEC §8).
    ///
    /// Each is matched against an attachment the *signed* message declares, so
    /// a sender cannot use a delivery to push arbitrary files into the blob
    /// store — and the bytes must hash to the digest that attachment names, so
    /// it cannot substitute different content for one it did declare.
    ///
    /// # Errors
    /// [`ServiceError::Blob`] if a part does not match what was declared.
    pub fn accept_inline_blobs(
        &self,
        message: &Message,
        blobs: Vec<(String, Vec<u8>)>,
    ) -> Result<(), ServiceError> {
        for (name, bytes) in blobs {
            let declared = message
                .attachments
                .iter()
                .find(|a| a.inline && a.sha256.to_hex() == name)
                .ok_or(BlobError::UnsafeName(
                    "a delivery carried a file the message does not declare",
                ))?;

            // Hashed before it is stored, not after: writing it first would
            // leave a file named after the wrong digest behind on every
            // rejection.
            let actual = hivemind_core::crypto::Sha256Digest::of(&bytes);
            if actual != declared.sha256 {
                return Err(BlobError::DigestMismatch {
                    expected: declared.sha256.to_hex(),
                    actual: actual.to_hex(),
                }
                .into());
            }
            self.blobs.put_bytes(&bytes)?;
        }
        Ok(())
    }

    /// The blob store, for handlers that stream attachments.
    #[must_use]
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Everything still awaiting delivery, oldest first (SPEC §8).
    ///
    /// # Errors
    /// [`ServiceError::Store`] if the outbox cannot be read.
    pub fn pending_outbound(&self) -> Result<Vec<Outbound>, ServiceError> {
        Ok(self.store.list_outbound()?)
    }

    /// Where a peer might be reached, best guess first.
    ///
    /// Empty for a peer we are not paired with: an address without a pinned
    /// certificate is not somewhere we will send mail.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book is poisoned.
    pub fn peer_addresses(&self, node: NodeId) -> Result<Vec<String>, ServiceError> {
        Ok(self.peers()?.peer(node).map_or_else(Vec::new, |peer| {
            peer.addrs_by_preference()
                .into_iter()
                .map(PeerAddr::authority)
                .collect()
        }))
    }

    /// Write delivery progress back, finishing the message if it is complete.
    ///
    /// # Errors
    /// [`ServiceError::Store`] or [`ServiceError::Index`].
    pub fn save_outbound(&self, outbound: &Outbound) -> Result<(), ServiceError> {
        if outbound.is_complete() {
            self.complete_delivery(outbound)
        } else {
            self.store.put_outbound(outbound)?;
            Ok(())
        }
    }

    /// Record that a peer answered at this address, so it is tried first next
    /// time (SPEC §5).
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book is poisoned.
    pub fn record_reached(
        &self,
        node: NodeId,
        addr: &str,
        at: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;
        let Some(peer) = peers.peer_mut(node) else {
            return Ok(());
        };
        peer.mark_reached(addr, at);
        // Best effort: losing the preference ordering costs a slow first
        // attempt next time, and is not worth failing a delivery over.
        if let Err(error) = peers.save() {
            tracing::warn!(%error, "could not save the address book");
        }
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

impl MailService {
    /// The largest delivery this node will read (SPEC §8).
    ///
    /// The inline budget, plus room for the message itself. A peer configured
    /// more generously than this one gets a `413` rather than being allowed to
    /// decide how much memory this machine spends.
    #[must_use]
    pub fn max_delivery_bytes(&self) -> usize {
        let budget = self
            .inline_max_bytes
            .saturating_mul(INLINE_BUDGET_MULTIPLE)
            .saturating_add(hivemind_core::message::BODY_MAX_BYTES as u64)
            // Multipart framing, and the subject and headers alongside it.
            .saturating_add(64 * 1024);
        usize::try_from(budget).unwrap_or(usize::MAX)
    }
}

/// How many times `inline_max` one message's inline attachments may total.
///
/// SPEC §8 sets the per-file rule; this bounds the whole delivery, which the
/// recipient has to be willing to buffer.
const INLINE_BUDGET_MULTIPLE: u64 = 4;

/// A guess at a media type from the file extension.
///
/// Advisory only (SPEC §4.1). A recipient that acts on this rather than on the
/// bytes is trusting the sender, so the list is short and boring on purpose.
fn mime_for(path: &std::path::Path) -> String {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "txt" | "log" | "toml" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    }
    .to_owned()
}

/// Split `host[:port]` into its parts, bracketed IPv6 included.
///
/// A bare IPv6 address is full of colons and cannot be told from `host:port`
/// without brackets, so an unbracketed one keeps the default port rather than
/// having its last group read as one.
fn split_host(host: &str, default_port: u16) -> (String, u16) {
    if let Some(rest) = host.strip_prefix('[') {
        // Unbalanced brackets: nothing sensible to do but keep it whole.
        let Some((inside, after)) = rest.split_once(']') else {
            return (host.to_owned(), default_port);
        };
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return (inside.to_owned(), port);
    }

    match host.rsplit_once(':') {
        // More than one colon and no brackets: a bare IPv6 address.
        Some(_) if host.matches(':').count() > 1 => (host.to_owned(), default_port),
        Some((name, port)) => match port.parse() {
            Ok(port) => (name.to_owned(), port),
            Err(_) => (host.to_owned(), default_port),
        },
        None => (host.to_owned(), default_port),
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

    #[test]
    fn a_host_without_a_port_gets_the_peer_port() {
        assert_eq!(
            split_host("laptop.local", 8400),
            ("laptop.local".into(), 8400)
        );
    }

    #[test]
    fn a_host_with_a_port_keeps_it() {
        assert_eq!(split_host("10.0.0.5:9001", 8400), ("10.0.0.5".into(), 9001));
    }

    #[test]
    fn a_bracketed_ipv6_address_is_unwrapped() {
        assert_eq!(split_host("[::1]:9001", 8400), ("::1".into(), 9001));
        assert_eq!(split_host("[fe80::1]", 8400), ("fe80::1".into(), 8400));
    }

    #[test]
    fn a_bare_ipv6_address_does_not_lose_its_last_group_to_a_port() {
        // `fe80::1` has a colon but no port; reading `1` as one would dial the
        // wrong place and drop part of the address.
        assert_eq!(split_host("fe80::1", 8400), ("fe80::1".into(), 8400));
    }

    #[test]
    fn a_port_that_is_not_a_number_leaves_the_host_alone() {
        // `machine:name` is likelier a typo than a request to dial port NaN.
        assert_eq!(
            split_host("machine:name", 8400),
            ("machine:name".into(), 8400)
        );
    }

    /// The bits of a node description the store tests do not care about.
    pub(super) fn describe(id: NodeId) -> NodeDescription {
        NodeDescription {
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
            presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
        }
    }

    pub(super) fn service() -> (tempfile::TempDir, MailService) {
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
            attachments: Vec::new(),
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
            attachments: Vec::new(),
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
            .reply(root.id, "on it".to_owned(), Vec::new(), SenderKind::Human)
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
            .reply(root.id, "yes".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply");
        let second = service
            .reply(first.id, "1pm".to_owned(), Vec::new(), SenderKind::Human)
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
            .reply(root.id, "yes".to_owned(), Vec::new(), SenderKind::Human)
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
            service.reply(
                Ulid::generate(),
                "hello".to_owned(),
                Vec::new(),
                SenderKind::Human
            ),
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

    /// A service whose limits are small enough to test the boundaries of.
    fn service_with_limits(
        max_attachment: u64,
        inline_max: u64,
    ) -> (tempfile::TempDir, MailService) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut node = describe(NodeId::from_certificate_der(b"this node"));
        node.max_attachment_bytes = max_attachment;
        node.inline_max_bytes = inline_max;
        let service = MailService::open(dir.path(), node, SigningKey::from_bytes(&[11u8; 32]))
            .expect("service opens");
        (dir, service)
    }

    fn file_of(dir: &tempfile::TempDir, name: &str, bytes: usize) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, vec![b'x'; bytes]).expect("write");
        path
    }

    fn draft_with(service: &MailService, attachments: Vec<std::path::PathBuf>) -> Draft {
        Draft {
            to: vec![Recipient::Node(service.identity())],
            subject: "with files".to_owned(),
            body: "see attached".to_owned(),
            kind: Kind::Message,
            in_reply_to: None,
            attachments,
        }
    }

    /// A service that has already paired with `friend`.
    fn service_knowing(
        friend: &hivemind_core::identity::Identity,
    ) -> (tempfile::TempDir, MailService) {
        let (dir, service) = service();
        service
            .admit(
                friend.node_id(),
                "their-laptop",
                Some("ana"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.2", 8400),
            )
            .expect("admit");
        (dir, service)
    }

    #[test]
    fn a_short_id_names_the_peer_it_belongs_to() {
        // It is the form the interface teaches: `peers`, `status` and `init`
        // all print it, and `pair` takes it. Issue #19 — sending to one
        // silently delivered to nobody, and fooled an agent on another
        // machine into reporting it had made contact.
        let friend = hivemind_core::identity::Identity::from_seed([5u8; 32]).expect("identity");
        let (_dir, service) = service_knowing(&friend);

        assert_eq!(
            service
                .parse_recipient(&friend.node_id().short())
                .expect("resolves"),
            Recipient::Node(friend.node_id())
        );
        // And case does not matter, since people copy it by eye.
        assert_eq!(
            service
                .parse_recipient(&friend.node_id().short().to_uppercase())
                .expect("resolves"),
            Recipient::Node(friend.node_id())
        );
    }

    #[test]
    fn the_full_form_and_everyone_still_mean_what_they_did() {
        let friend = hivemind_core::identity::Identity::from_seed([6u8; 32]).expect("identity");
        let (_dir, service) = service_knowing(&friend);

        assert_eq!(
            service
                .parse_recipient(&friend.node_id().to_string())
                .expect("resolves"),
            Recipient::Node(friend.node_id())
        );
        assert_eq!(
            service.parse_recipient("EVERYONE").expect("resolves"),
            Recipient::Everyone
        );
        assert_eq!(
            service.parse_recipient("ana").expect("resolves"),
            Recipient::Owner("ana".to_owned()),
            "a name that is not a short id is still a person"
        );
    }

    #[test]
    fn an_owner_name_wins_over_a_short_id_that_looks_like_it() {
        // Vanishingly unlikely, and the order has to be decided rather than
        // discovered: what somebody called themselves is what they meant.
        let friend = hivemind_core::identity::Identity::from_seed([7u8; 32]).expect("identity");
        let short = friend.node_id().short();
        let (_dir, service) = service();
        service
            .admit(
                friend.node_id(),
                "odd",
                Some(&short),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.3", 8400),
            )
            .expect("admit");

        assert_eq!(
            service.parse_recipient(&short).expect("resolves"),
            Recipient::Owner(short),
            "somebody's name is what they meant by it"
        );
    }

    #[test]
    fn a_message_that_reaches_nobody_is_refused_rather_than_filed_as_sent() {
        // The other half of #19, and the half that closes the class. A `to`
        // that resolves to nothing is the same outcome as an empty `to` with
        // a better disguise: the message went nowhere and said it was sent.
        let (_dir, service) = service();

        let error = service
            .send(
                Draft {
                    to: vec![Recipient::Owner("nobody-by-that-name".to_owned())],
                    subject: "into the void".to_owned(),
                    body: "x".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect_err("it reaches nobody");

        assert!(
            matches!(error, ServiceError::NoRecipients),
            "expected NoRecipients, got {error:?}"
        );
        assert_eq!(
            service.list(&Query::default()).expect("list").len(),
            0,
            "and nothing should have been written anywhere"
        );
    }

    #[test]
    fn an_ambiguous_short_id_is_refused_rather_than_guessed() {
        // Two peers cannot share a short id in practice — it is 40 bits of a
        // hash — but guessing which one gets the mail is not a thing to do by
        // accident, so the code says so rather than relying on that.
        let (_dir, service) = service();
        assert!(matches!(
            service.parse_recipient(""),
            Err(ServiceError::NoSuchPeer { .. })
        ));
    }

    #[test]
    fn a_small_attachment_travels_with_the_message_and_a_large_one_does_not() {
        // SPEC §8: any single file at or below inline_max ships in the
        // delivery; larger ones ship as refs and are fetched on demand.
        let (dir, service) = service_with_limits(1_000_000, 100);
        let small = file_of(&dir, "small.txt", 100);
        let large = file_of(&dir, "large.txt", 101);

        let sent = service
            .send(draft_with(&service, vec![small, large]), SenderKind::Human)
            .expect("send");

        assert_eq!(sent.attachments.len(), 2);
        assert!(sent.attachments[0].inline, "100 bytes is at the limit");
        assert!(!sent.attachments[1].inline, "101 is over it");
        assert_eq!(sent.attachments[0].name, "small.txt");
        assert_eq!(sent.attachments[0].size, 100);
    }

    #[test]
    fn the_inline_budget_bounds_one_delivery_without_refusing_anything() {
        // Otherwise a message with a hundred small files becomes an
        // unboundedly large request the recipient has to buffer.
        let (dir, service) = service_with_limits(1_000_000, 100);
        let budget = 100 * INLINE_BUDGET_MULTIPLE;

        // Five files of 100 bytes: four fit the budget, the fifth does not.
        let files: Vec<_> = (0..5)
            .map(|i| file_of(&dir, &format!("part{i}.bin"), 100))
            .collect();

        let sent = service
            .send(draft_with(&service, files), SenderKind::Human)
            .expect("send");

        let inline: Vec<_> = sent.attachments.iter().filter(|a| a.inline).collect();
        assert_eq!(inline.len(), usize::try_from(budget / 100).expect("fits"));
        assert!(
            sent.attachments.iter().any(|a| !a.inline),
            "the rest should ship as refs rather than be refused"
        );
        assert_eq!(sent.attachments.len(), 5, "nothing is dropped");
    }

    #[test]
    fn an_attachment_over_the_hard_limit_is_refused() {
        let (dir, service) = service_with_limits(50, 10);
        let too_big = file_of(&dir, "huge.bin", 51);

        let error = service
            .send(draft_with(&service, vec![too_big]), SenderKind::Human)
            .expect_err("over the limit");

        assert!(matches!(
            error,
            ServiceError::Blob(BlobError::TooLarge { .. })
        ));
    }

    #[test]
    fn the_same_file_attached_twice_is_stored_once() {
        let (dir, service) = service_with_limits(1_000_000, 1_000_000);
        let path = file_of(&dir, "shared.bin", 512);

        let sent = service
            .send(
                draft_with(&service, vec![path.clone(), path]),
                SenderKind::Human,
            )
            .expect("send");

        assert_eq!(sent.attachments.len(), 2, "both are listed");
        assert_eq!(
            sent.attachments[0].sha256, sent.attachments[1].sha256,
            "and both point at one blob"
        );
        assert!(service.blobs().has(&sent.attachments[0].sha256));
    }

    #[test]
    fn a_media_type_is_guessed_from_the_name_and_never_from_the_contents() {
        let (dir, service) = service_with_limits(1_000_000, 1_000_000);
        let png = file_of(&dir, "not-really.png", 8);

        let sent = service
            .send(draft_with(&service, vec![png]), SenderKind::Human)
            .expect("send");

        assert_eq!(
            sent.attachments[0].mime, "image/png",
            "advisory only: acting on this rather than on the bytes is \
             trusting the sender"
        );
    }

    #[test]
    fn an_attachment_that_does_not_exist_says_so_rather_than_sending_nothing() {
        let (dir, service) = service_with_limits(1_000_000, 1_000_000);
        let missing = dir.path().join("never-written.txt");

        let error = service
            .send(draft_with(&service, vec![missing]), SenderKind::Human)
            .expect_err("no such file");

        assert!(matches!(error, ServiceError::Blob(BlobError::Io { .. })));
        assert_eq!(
            service.unread_count().expect("count"),
            0,
            "and nothing should have been sent"
        );
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
                        attachments: Vec::new(),
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
