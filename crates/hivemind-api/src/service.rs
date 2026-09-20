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
use hivemind_core::index::{Conversation, ConversationQuery, Index, IndexError, Query, Summary};
use hivemind_core::message::{
    AttachmentRef, CanonicalError, Kind, Message, MessageError, Recipient, SenderKind,
};
use hivemind_core::peer::NodeId;
use hivemind_core::peerbook::{
    AddrSource, CertificateDer, Peer, PeerAddr, PeerBook, PeerBookError,
};
use hivemind_core::store::{MailStore, Mailbox, Outbound, RecipientState, StoreError};
use tokio::sync::{broadcast, watch};
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

    /// The message this event is about, if it is about one.
    #[must_use]
    pub fn message(&self) -> Option<Ulid> {
        match self {
            Self::MessageReceived { id }
            | Self::MessageDelivered { id }
            | Self::MessageRead { id } => Some(*id),
            Self::PeerSeen { .. } | Self::PeerOnline { .. } | Self::PeerOffline { .. } => None,
        }
    }

    /// What the SSE stream carries as this event's data (SPEC §7.1).
    ///
    /// The thing the event is about, whatever kind of thing that is: a message
    /// id, or a node id. A peer event used to send a nil ULID, which told a
    /// client that *somebody* had come online and left it to re-read the whole
    /// peer list to find out who.
    #[must_use]
    pub fn data(&self) -> String {
        match self {
            Self::MessageReceived { id }
            | Self::MessageDelivered { id }
            | Self::MessageRead { id } => id.to_string(),
            Self::PeerSeen { id } | Self::PeerOnline { id } | Self::PeerOffline { id } => {
                id.to_string()
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
    /// Nothing here has an id ending that way (SPEC §10, #27).
    #[error("no message whose id ends with `{typed}`")]
    NoSuchMessageTail {
        /// What the caller typed.
        typed: String,
    },
    /// More than one message ends that way, and guessing is not an option.
    #[error("`{typed}` matches {} messages: {}", .candidates.len(), .candidates.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))]
    AmbiguousMessage {
        /// What the caller typed.
        typed: String,
        /// Every message it could have meant.
        candidates: Vec<Ulid>,
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
    /// The peer is known; that address is not one of the ways to reach it.
    #[error("{peer} has no address {addr}")]
    NoSuchAddr {
        /// The peer, in its short form.
        peer: String,
        /// The `host:port` that was asked for.
        addr: String,
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

mod attachments;
mod duplicate;
mod group;
mod peering;
mod presence;
mod query;
mod receive;
mod recipients;
mod send;
mod sessions;

pub use duplicate::{Queued, REPEAT_WINDOW};
pub use group::{GroupStatus, MAX_SEEN, SeenNode};
pub use peering::Met;
pub use presence::Presence;
pub use sessions::{SESSION_TTL, Session};

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
    /// Set when a hello could not be delivered, so discovery looks sooner
    /// than its next scheduled round (SPEC §5.2). An address that stopped
    /// working is the best moment to go looking for the one that replaced it.
    rediscover: std::sync::atomic::AtomicBool,
    /// The Claude Code sessions open on this machine (SPEC §9.3). In memory
    /// only: a daemon that restarts hears about each again at its next turn,
    /// which is more honest than a file claiming something about a process
    /// that may be gone.
    sessions: Mutex<sessions::Sessions>,
    /// Nodes to pass on as "X is up", and when we heard from each. They age
    /// out after one interval rather than being consumed, so that every peer
    /// greeted in the next round hears the news once.
    hints: Mutex<std::collections::BTreeMap<NodeId, DateTime<Utc>>>,
    /// How often presence runs, which is also how long a hello counts for —
    /// a peer is online while its last one is younger than two of these.
    presence_interval: std::time::Duration,
    /// Whether Tailscale is a discovery source here (SPEC §5.2). `Off` means
    /// the binary is never run, not that its results are ignored.
    tailscale: hivemind_core::config::Tailscale,
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
    closing: watch::Sender<bool>,
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
    /// Whether to find peers through Tailscale (SPEC §5.2).
    pub tailscale: hivemind_core::config::Tailscale,
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
        let peers = PeerBook::load(root, node.peer_port)?;
        // Said out loud, once, at start: a peers.toml written before #23 holds
        // this node's own loopback as the way to reach somebody else, and an
        // address quietly vanishing is worse than the address.
        for discarded in peers.discarded() {
            tracing::warn!(
                peer = %discarded.peer.short(),
                addr = %discarded.authority,
                "ignoring an address in peers.toml that points at this node; \
                 `hivemind peers forget-addr` takes it out of the file"
            );
        }
        let group = hivemind_core::group::Group::load(root)?;

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (closing, _) = watch::channel(false);
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
            rediscover: std::sync::atomic::AtomicBool::new(false),
            sessions: Mutex::new(sessions::Sessions::new()),
            hints: Mutex::new(std::collections::BTreeMap::new()),
            presence_interval: std::time::Duration::from_secs(node.presence_interval),
            tailscale: node.tailscale,
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
            closing,
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

    /// Tell every open event stream that the daemon is going away (#108).
    ///
    /// An SSE subscription is in flight for as long as it is open, and
    /// `/api/v1/events` is open by design — so a graceful shutdown that waits
    /// for in-flight connections waits for ever, and launchd's SIGKILL is
    /// what actually ends the process. The stream needs something to end on.
    ///
    /// A signal of its own rather than dropping the event sender, which was
    /// the other shape on offer: the sender lives in this struct behind an
    /// `Arc` that every task holds, so dropping it means an `Option` and a
    /// lock on the path every published event takes — and a `Closed` that
    /// readers cannot tell from the service having gone away by accident.
    pub fn close_event_streams(&self) {
        let _ = self.closing.send(true);
    }

    /// Watch for [`Self::close_event_streams`].
    ///
    /// A `watch` and not a `Notify`, because the value is sticky: a stream
    /// opened after the daemon began shutting down has to end at once rather
    /// than wait for a signal that has already been sent.
    #[must_use]
    pub fn closing(&self) -> watch::Receiver<bool> {
        self.closing.subscribe()
    }

    fn index(&self) -> Result<std::sync::MutexGuard<'_, Index>, ServiceError> {
        self.index.lock().map_err(|_| ServiceError::Unavailable)
    }

    fn put(&self, mailbox: Mailbox, message: &Message) -> Result<(), ServiceError> {
        // File first, index second. A crash between them leaves a stale index,
        // which the next rebuild corrects; the other order would invent a
        // message that does not exist (ADR 0002).
        self.store.put(mailbox, message)?;
        self.index()?.upsert(mailbox, message)?;
        Ok(())
    }
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

#[cfg(test)]
pub(crate) mod tests {
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
            tailscale: hivemind_core::config::Tailscale::Auto,
        }
    }

    /// A service whose Tailscale mode is not the default.
    pub(crate) fn service_with_tailscale(
        mode: hivemind_core::config::Tailscale,
    ) -> (tempfile::TempDir, std::sync::Arc<MailService>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let id = NodeId::from_certificate_der(b"this node");
        let service = MailService::open(
            dir.path(),
            NodeDescription {
                tailscale: mode,
                ..describe(id)
            },
            SigningKey::from_bytes(&[11u8; 32]),
        )
        .expect("service");
        (dir, std::sync::Arc::new(service))
    }

    pub(crate) fn service() -> (tempfile::TempDir, MailService) {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let identity = NodeId::from_certificate_der(b"this node");
        let service =
            MailService::open(dir.path(), describe(identity), key).expect("service opens");
        (dir, service)
    }

    pub(super) fn draft_to_self(service: &MailService, subject: &str, body: &str) -> Draft {
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
    fn a_peer_event_carries_the_node_it_is_about() {
        // It used to carry a nil ULID, which told a client that *somebody*
        // had come online and left it to re-read the whole peer list to find
        // out who. SPEC §7.1 names these events; a name with no subject is
        // half an event.
        let id = NodeId::from_certificate_der(b"somebody");

        for event in [
            Event::PeerSeen { id },
            Event::PeerOnline { id },
            Event::PeerOffline { id },
        ] {
            assert_eq!(event.data(), id.to_string(), "{}", event.name());
            assert!(
                event.message().is_none(),
                "a peer event is not about a message"
            );
        }

        assert_eq!(Event::PeerOnline { id }.name(), "peer.online");
        assert_eq!(Event::PeerOffline { id }.name(), "peer.offline");
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
                .message
        };

        std::fs::remove_file(dir.path().join("index.db")).expect("delete the index");

        let service = MailService::open(dir.path(), describe(identity), key).expect("reopen");
        assert_eq!(service.unread_count().expect("count"), 1);
        assert_eq!(service.get(sent.id).expect("get").1.subject, "survives");
    }
}
