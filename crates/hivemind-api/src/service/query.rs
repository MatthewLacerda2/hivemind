//! Reading what is here: listings, one message, a conversation, read state.
//!
//! Everything in here answers from the index and the mail files and changes
//! nothing, apart from `mark_read` and `reindex`, which are the two ways a
//! reader alters what a later read says.

use super::*;

impl MailService {
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
        for mailbox in [Mailbox::New, Mailbox::Cur, Mailbox::Sent] {
            match self.store.get(mailbox, id) {
                Ok(message) => return Ok((mailbox, message)),
                Err(StoreError::NotFound { .. }) => {}
                Err(other) => return Err(other.into()),
            }
        }

        // `out/` last, and through the envelope: the file there is an
        // `Outbound` — the signed message plus what is still owed each
        // recipient — so reading it as a plain message fails to parse. It was
        // in the loop above, which made every message still waiting for an
        // offline peer unreadable: the listing shows it, and `read` answered
        // "something went wrong on this node".
        match self.store.get_outbound(id) {
            Ok(outbound) => Ok((Mailbox::Out, outbound.message)),
            Err(StoreError::NotFound { .. }) => Err(ServiceError::NoSuchMessage { id }),
            Err(other) => Err(other.into()),
        }
    }

    /// Every message in a thread, oldest first, once each.
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

        // The index holds a row per id *and* mailbox, so a message addressed to
        // its own sender is in it twice: once as what arrived, once as what was
        // sent. A listing should show both, because both boxes hold it. A
        // conversation showing both reads as the same thing having been said
        // twice, so the arrived copy wins, as it does in `get`.
        let arrived: std::collections::HashSet<Ulid> = found
            .iter()
            .filter(|summary| matches!(summary.mailbox, Mailbox::New | Mailbox::Cur))
            .map(|summary| summary.id)
            .collect();
        found.retain(|summary| {
            matches!(summary.mailbox, Mailbox::New | Mailbox::Cur) || !arrived.contains(&summary.id)
        });
        Ok(found)
    }

    /// Every message in the conversation `id` belongs to, oldest first.
    ///
    /// The id of **any** message in the thread, not only its root: nobody knows
    /// by heart which one was first, and the id somebody has to hand is the one
    /// they were just reading (#34).
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if no mailbox holds `id`. A conversation
    /// nobody has is absent rather than empty, and an empty list would read as
    /// the second (#28).
    pub fn thread_of(&self, id: Ulid) -> Result<Vec<Summary>, ServiceError> {
        self.thread(self.thread_id_of(id)?)
    }

    /// Turn what somebody typed into a message id (SPEC §10).
    ///
    /// Accepts the whole ULID, or **any tail of one that names exactly one
    /// message** — which is what makes the short id the inbox prints usable.
    /// It was not: `hivemind inbox` showed `03VYRM`, `hivemind read 03VYRM`
    /// answered "not a message id", and only the 26-character form worked,
    /// which the CLI never showed anywhere (#27). A short form a program
    /// prints and then refuses is not informing anybody; it is misleading
    /// them, and copying what is on the screen is the obvious gesture.
    ///
    /// The same shape `resolve_peer` has had since #19, for the same reason.
    ///
    /// An ambiguous tail is an **error naming the candidates**, never a
    /// guess: reading the wrong message is worse than being asked again.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessageTail`] if nothing matches,
    /// [`ServiceError::AmbiguousMessage`] if more than one does, or
    /// [`ServiceError::Unavailable`] if the index lock is poisoned.
    pub fn resolve_message(&self, typed: &str) -> Result<Ulid, ServiceError> {
        let typed = typed.trim();
        if let Ok(id) = typed.parse::<Ulid>() {
            return Ok(id);
        }

        // Two is all the caller needs: "one" or "more than one". Fetching the
        // rest to count them would be work nobody reads.
        let found = self
            .index
            .lock()
            .map_err(|_| ServiceError::Unavailable)?
            .ids_ending_with(typed, 2)?;

        match found.as_slice() {
            [id] => Ok(*id),
            [] => Err(ServiceError::NoSuchMessageTail {
                typed: typed.to_owned(),
            }),
            _ => Err(ServiceError::AmbiguousMessage {
                typed: typed.to_owned(),
                candidates: found,
            }),
        }
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

    /// Every conversation, the one that moved last first (SPEC §7.1, #43).
    ///
    /// A conversation is a thread, so this is [`Self::thread`]'s list seen from
    /// the outside: one row per `thread_id`, with the subject it opened with,
    /// who it is with, and how much of it is unread.
    ///
    /// `participants` comes back as **the other machines** — who the
    /// conversation is with — rather than everybody in it, because that is the
    /// question a list of conversations answers. A conversation with nobody
    /// else keeps this machine, so a note to self is not a row with an empty
    /// space where a name goes.
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn conversations(
        &self,
        query: &ConversationQuery,
    ) -> Result<Vec<Conversation>, ServiceError> {
        let mut found = self.index()?.conversations(query)?;
        for conversation in &mut found {
            if conversation
                .participants
                .iter()
                .any(|id| *id != self.identity)
            {
                conversation.participants.retain(|id| *id != self.identity);
            }
        }
        Ok(found)
    }

    /// Throw the index away and rebuild it from the mail files (SPEC §10).
    ///
    /// # Errors
    /// Returns [`ServiceError::Index`] on failure.
    pub fn reindex(&self) -> Result<(), ServiceError> {
        self.index()?.rebuild_from(&self.store)?;
        Ok(())
    }
    pub(super) fn thread_id_of(&self, parent: Ulid) -> Result<Ulid, ServiceError> {
        let (_, message) = self.get(parent)?;
        Ok(message.thread_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::{draft_to_self, service};

    /// A member of this node's group, keys and all.
    fn member(service: &MailService, seed: u8, name: &str) -> hivemind_core::identity::Identity {
        let friend = hivemind_core::identity::Identity::from_seed([seed; 32]).expect("identity");
        service
            .admit(
                friend.node_id(),
                name,
                Some("ana"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.2", 8400),
            )
            .expect("admit");
        friend
    }

    /// One message from `friend`, as its machine would have delivered it.
    ///
    /// `millis` is the send time and the id is derived from it, so a test says
    /// which conversation moved last rather than hoping.
    fn arrives(
        service: &MailService,
        friend: &hivemind_core::identity::Identity,
        millis: u64,
        subject: &str,
    ) -> Ulid {
        let id = Ulid::from_parts(millis, 0);
        let mut message = Message {
            id,
            thread_id: id,
            in_reply_to: None,
            from: friend.node_id(),
            to: vec![Recipient::Node(service.identity())],
            subject: subject.to_owned(),
            body: "body".to_owned(),
            kind: Kind::Message,
            sender_kind: SenderKind::Human,
            attachments: Vec::new(),
            sent_at: DateTime::from_timestamp_millis(i64::try_from(millis).expect("in range"))
                .expect("a timestamp"),
            received_at: None,
            signature: Signature::from_bytes([0u8; 64]),
        };
        message.sign(friend.signing_key()).expect("sign");
        service.receive(friend.node_id(), message).expect("receive")
    }

    #[test]
    fn a_message_still_waiting_for_an_offline_peer_can_be_read() {
        // It sits in `out/` as a delivery envelope, and reading that as a
        // plain message fails to parse — so `read` on something this machine
        // had just sent answered "something went wrong on this node", while
        // `inbox --box out` listed it and printed its id.
        let (_dir, service) = service();
        let ana = member(&service, 53, "ana-mbp");
        let queued = service
            .send(
                Draft {
                    to: vec![Recipient::Node(ana.node_id())],
                    subject: "dashboard PR".to_owned(),
                    body: "take a look".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send")
            .message;

        let (mailbox, read) = service
            .get(queued.id)
            .expect("a message we sent is readable");

        assert_eq!(mailbox, Mailbox::Out, "it is still on its way");
        assert_eq!(read.subject, "dashboard PR");
        assert_eq!(read.id, queued.id);
    }

    #[test]
    fn a_conversation_says_which_machine_it_is_with_not_which_are_in_it() {
        // Two conversations and two machines, so "the right one" is
        // distinguishable from "all of them" — and this machine, which is in
        // both of them and is not who either is with.
        let (_dir, service) = service();
        let ana = member(&service, 51, "ana-mbp");
        let beto = member(&service, 52, "beto-air");
        let from_ana = arrives(&service, &ana, 100, "dashboard PR");
        arrives(&service, &beto, 200, "lunch?");
        service
            .reply(from_ana, "on it".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply");

        let found = service
            .conversations(&ConversationQuery::default())
            .expect("a list");

        assert_eq!(found.len(), 2, "two subjects, two conversations: {found:?}");
        let dashboard = found
            .iter()
            .find(|c| c.subject == "dashboard PR")
            .expect("the one we answered");
        assert_eq!(
            dashboard.participants,
            vec![ana.node_id()],
            "this machine is in it and is not who it is with"
        );
        assert_eq!(dashboard.messages, 2);

        let lunch = found
            .iter()
            .find(|c| c.subject == "lunch?")
            .expect("the other one");
        assert_eq!(lunch.participants, vec![beto.node_id()]);
    }

    #[test]
    fn a_conversation_with_nobody_else_still_names_this_machine() {
        // Dropping this node unconditionally would leave a note to self as a
        // row with an empty space where a name goes.
        let (_dir, service) = service();
        service
            .send(
                draft_to_self(&service, "note to self", "body"),
                SenderKind::Human,
            )
            .expect("send");

        let found = service
            .conversations(&ConversationQuery::default())
            .expect("a list");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].participants, vec![service.identity()]);
        assert_eq!(found[0].messages, 1, "one message, in two boxes");
    }

    #[test]
    fn marking_a_message_read_moves_it_out_of_the_unread_count() {
        let (_dir, service) = service();
        let sent = service
            .send(draft_to_self(&service, "unread", "body"), SenderKind::Human)
            .expect("send")
            .message;
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
            .expect("send")
            .message;
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
    fn a_thread_reads_oldest_first() {
        let (_dir, service) = service();
        let root = service
            .send(draft_to_self(&service, "lunch", "?"), SenderKind::Human)
            .expect("send")
            .message;
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
}
