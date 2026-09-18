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
use hivemind_core::store::{MailStore, Mailbox, StoreError};
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
}

impl Event {
    /// The SSE event name (SPEC §7.1).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::MessageReceived { .. } => "message.received",
            Self::MessageDelivered { .. } => "message.delivered",
            Self::MessageRead { .. } => "message.read",
        }
    }

    /// The id the event is about.
    #[must_use]
    pub fn id(&self) -> Ulid {
        match self {
            Self::MessageReceived { id }
            | Self::MessageDelivered { id }
            | Self::MessageRead { id } => *id,
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
    /// The index lock was poisoned by a panic in another thread.
    #[error("the index is unavailable after an earlier failure")]
    Unavailable,
}

/// Everything the local API and the MCP adapter can do.
#[derive(Debug)]
pub struct MailService {
    store: MailStore,
    index: Mutex<Index>,
    identity: NodeId,
    signing_key: SigningKey,
    events: broadcast::Sender<Event>,
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
        identity: NodeId,
        signing_key: SigningKey,
    ) -> Result<Self, ServiceError> {
        let store = MailStore::open(root.join("mail"))?;
        let mut index = Index::open(&root.join("index.db"))?;
        // Cheap when the index was already current, because it is only the
        // files that exist; correct when it was not.
        index.rebuild_from(&store)?;

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Ok(Self {
            store,
            index: Mutex::new(index),
            identity,
            signing_key,
            events,
        })
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

        self.put(Mailbox::Out, &message)?;

        // Local delivery. With no peers yet, a message addressed to us is the
        // whole of M1's round trip (SPEC §14).
        if self.addresses_us(&message) {
            let mut received = message.clone();
            received.received_at = Some(Utc::now().trunc_subsecs(3));
            self.put(Mailbox::New, &received)?;
            let _ = self.events.send(Event::MessageReceived { id });
        }

        // Nothing is left to deliver remotely until M3, so the outbox copy is
        // already done.
        self.move_message(Mailbox::Out, Mailbox::Sent, id)?;
        let _ = self.events.send(Event::MessageDelivered { id });

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

    fn put(&self, mailbox: Mailbox, message: &Message) -> Result<(), ServiceError> {
        // File first, index second. A crash between them leaves a stale index,
        // which the next rebuild corrects; the other order would invent a
        // message that does not exist (ADR 0002).
        self.store.put(mailbox, message)?;
        self.index()?.upsert(mailbox, message)?;
        Ok(())
    }

    fn move_message(&self, from: Mailbox, to: Mailbox, id: Ulid) -> Result<(), ServiceError> {
        self.store.move_to(from, to, id)?;
        self.index()?.set_mailbox(id, from, to)?;
        Ok(())
    }

    fn addresses_us(&self, message: &Message) -> bool {
        message.to.iter().any(|recipient| match recipient {
            Recipient::Node(node) => *node == self.identity,
            // No peer book yet, so we are the only machine `everyone` reaches.
            Recipient::Everyone => true,
            // `owner` needs the peer book to resolve; it arrives with M3.
            Recipient::Owner(_) => false,
        })
    }

    fn thread_id_of(&self, parent: Ulid) -> Result<Ulid, ServiceError> {
        let (_, message) = self.get(parent)?;
        Ok(message.thread_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, MailService) {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let identity = NodeId::from_certificate_der(b"this node");
        let service = MailService::open(dir.path(), identity, key).expect("service opens");
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
            let service = MailService::open(dir.path(), identity, key.clone()).expect("open");
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

        let service = MailService::open(dir.path(), identity, key).expect("reopen");
        assert_eq!(service.unread_count().expect("count"), 1);
        assert_eq!(service.get(sent.id).expect("get").1.subject, "survives");
    }
}
