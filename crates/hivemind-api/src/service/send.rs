//! Writing a message and getting it out of this node (SPEC §8).
//!
//! Sending is a file write: the message lands in `out/` before the call
//! returns, and the delivery queue carries it from there. The rest of this
//! module is the other end of that arrangement — what the queue reads, and
//! how progress is written back until every recipient has it.

use super::*;

impl MailService {
    /// Write and locally deliver a message.
    ///
    /// The message is written to `out/` before this returns, so sending never
    /// blocks on the network (SPEC §8). Recipients that are this node are
    /// delivered immediately; every other recipient goes through the outbox,
    /// which retries until it lands.
    ///
    /// # Errors
    /// [`ServiceError::NoRecipients`] for an unaddressed draft,
    /// [`ServiceError::Invalid`] if it breaks the limits in SPEC §4.1.
    pub fn send(&self, draft: Draft, sender_kind: SenderKind) -> Result<Queued, ServiceError> {
        if draft.to.is_empty() {
            return Err(ServiceError::NoRecipients);
        }

        // Asked before the draft is taken apart, and answered by the files
        // rather than by memory of this process: two presses can land in two
        // daemons' lifetimes (#33).
        let duplicate_of = self.recent_duplicate(&draft);

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
        self.store.put_outbound(Mailbox::Out, &outbound)?;
        self.index()?.upsert(Mailbox::Out, &message)?;

        if outbound.is_complete() {
            self.complete_delivery(&outbound)?;
        }

        if let Some(previous) = duplicate_of {
            // Both ids, because the question a person asks next is which of the
            // two the other end got, and the answer is both.
            tracing::warn!(
                id = %message.id,
                %previous,
                "this is the same message as one sent moments ago"
            );
        }

        Ok(Queued {
            message,
            duplicate_of,
        })
    }

    /// Answer a message, or the conversation `target` names, inheriting its
    /// thread (SPEC §10, #43).
    ///
    /// **A thread id answers the conversation's most recent message.** A
    /// thread id *is* the id of the message that opened the conversation, so
    /// that is the only way the two can be told apart — and "answer the first
    /// message of a six-message thread" is not a thing anybody means by
    /// handing over a conversation's id. Continuing a subject should not mean
    /// hunting for the id of its latest message.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchMessage`] if neither a message nor a conversation
    /// here is called that.
    pub fn reply(
        &self,
        target: Ulid,
        body: String,
        attachments: Vec<std::path::PathBuf>,
        sender_kind: SenderKind,
    ) -> Result<Queued, ServiceError> {
        let parent = self.reply_target(target)?;
        let (_, original) = self.get(parent)?;
        let subject = if original.subject.starts_with("Re: ") {
            original.subject.clone()
        } else {
            format!("Re: {}", original.subject)
        };

        // Answering our own message — which is what continuing a conversation
        // nobody has answered yet comes down to — goes to whoever it was sent
        // to. Replying to the sender would address this machine, so the answer
        // would land in its own inbox and reach nobody: a message that went
        // nowhere and said it was sent, which is the shape of #19.
        let to = if original.from == self.identity {
            original.to.clone()
        } else {
            vec![Recipient::Node(original.from)]
        };

        self.send(
            Draft {
                to,
                subject,
                body,
                kind: Kind::Message,
                in_reply_to: Some(parent),
                attachments,
            },
            sender_kind,
        )
    }

    /// Which message a reply to `target` answers.
    ///
    /// The id of a message that is not the one a conversation opened with
    /// answers exactly that message. Anything else — the id of a conversation,
    /// or of a conversation whose opening message this node never received —
    /// answers the most recent message in it.
    fn reply_target(&self, target: Ulid) -> Result<Ulid, ServiceError> {
        match self.get(target) {
            Ok((_, named)) if named.thread_id != target => Ok(target),
            Ok(_) | Err(ServiceError::NoSuchMessage { .. }) => self
                .thread(target)?
                .last()
                .map(|last| last.id)
                .ok_or(ServiceError::NoSuchMessage { id: target }),
            Err(other) => Err(other),
        }
    }

    /// Everything still awaiting delivery, oldest first (SPEC §8).
    ///
    /// # Errors
    /// [`ServiceError::Store`] if the outbox cannot be read.
    pub fn pending_outbound(&self) -> Result<Vec<Outbound>, ServiceError> {
        Ok(self.store.list_outbound()?)
    }
    /// Write delivery progress back, finishing the message if it is complete.
    ///
    /// # Errors
    /// [`ServiceError::Store`] or [`ServiceError::Index`].
    pub fn save_outbound(&self, outbound: &Outbound) -> Result<(), ServiceError> {
        if outbound.is_complete() {
            self.complete_delivery(outbound)
        } else {
            self.store.put_outbound(Mailbox::Out, outbound)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::{draft_to_self, service};

    #[test]
    fn a_message_sent_to_ourselves_arrives_in_the_inbox() {
        // SPEC §14's M1 round trip: one daemon, send to self, read it back.
        let (_dir, service) = service();
        let sent = service
            .send(
                draft_to_self(&service, "dashboard PR", "take a look"),
                SenderKind::Human,
            )
            .expect("send")
            .message;

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
            .expect("send")
            .message;

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
            .expect("send")
            .message;
        assert_eq!(agent.sender_kind, SenderKind::Agent);

        let human = service
            .send(
                draft_to_self(&service, "from cli", "body"),
                SenderKind::Human,
            )
            .expect("send")
            .message;
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
    fn a_reply_joins_the_thread_it_answers() {
        let (_dir, service) = service();
        let root = service
            .send(
                draft_to_self(&service, "dashboard PR", "take a look"),
                SenderKind::Human,
            )
            .expect("send")
            .message;

        let reply = service
            .reply(root.id, "on it".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply")
            .message;

        assert_eq!(reply.thread_id, root.thread_id);
        assert_eq!(reply.in_reply_to, Some(root.id));
        assert_eq!(reply.subject, "Re: dashboard PR");
    }

    #[test]
    fn replying_to_a_reply_does_not_stack_re_prefixes() {
        let (_dir, service) = service();
        let root = service
            .send(draft_to_self(&service, "lunch", "?"), SenderKind::Human)
            .expect("send")
            .message;
        let first = service
            .reply(root.id, "yes".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply")
            .message;
        let second = service
            .reply(first.id, "1pm".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply")
            .message;

        assert_eq!(second.subject, "Re: lunch");
    }
    /// A member of this node's group, keys and all.
    fn member(service: &MailService, seed: u8) -> hivemind_core::identity::Identity {
        let friend = hivemind_core::identity::Identity::from_seed([seed; 32]).expect("identity");
        service
            .admit(
                friend.node_id(),
                "ana-mbp",
                Some("ana"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.2", 8400),
            )
            .expect("admit");
        friend
    }

    /// One message from `friend`, in `thread` when it is a reply.
    fn arrives(
        service: &MailService,
        friend: &hivemind_core::identity::Identity,
        millis: u64,
        thread: Option<Ulid>,
        subject: &str,
    ) -> Ulid {
        let id = Ulid::from_parts(millis, 0);
        let mut message = Message {
            id,
            thread_id: thread.unwrap_or(id),
            in_reply_to: thread,
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
    fn replying_to_a_conversation_answers_its_most_recent_message() {
        // What #43 asks for: continuing a subject should not mean hunting for
        // the id of the message that happens to be last in it.
        let (_dir, service) = service();
        let ana = member(&service, 63);
        let opened = arrives(&service, &ana, 100, None, "dashboard PR");
        let latest = arrives(&service, &ana, 200, Some(opened), "Re: dashboard PR");

        // `opened` is the conversation's id as well as a message's: a thread
        // id is the id of the message that opened it.
        let answer = service
            .reply(opened, "on it".to_owned(), Vec::new(), SenderKind::Human)
            .expect("reply")
            .message;

        assert_eq!(
            answer.in_reply_to,
            Some(latest),
            "the conversation's id answers where the conversation got to"
        );
        assert_eq!(answer.thread_id, opened);
        assert_eq!(answer.to, vec![Recipient::Node(ana.node_id())]);
    }

    #[test]
    fn replying_to_one_message_in_a_conversation_still_answers_that_message() {
        // The other half: an id that names a message and not a conversation
        // means that message, which is what `thread` tells a Claude to do
        // when it wants to answer one particular turn.
        let (_dir, service) = service();
        let ana = member(&service, 64);
        let opened = arrives(&service, &ana, 100, None, "dashboard PR");
        let middle = arrives(&service, &ana, 200, Some(opened), "Re: dashboard PR");
        arrives(&service, &ana, 300, Some(opened), "Re: dashboard PR");

        let answer = service
            .reply(
                middle,
                "about that".to_owned(),
                Vec::new(),
                SenderKind::Human,
            )
            .expect("reply")
            .message;

        assert_eq!(answer.in_reply_to, Some(middle));
    }

    #[test]
    fn answering_our_own_message_goes_to_whoever_it_was_sent_to() {
        // Continuing a conversation nobody has answered yet comes down to
        // replying to ourselves. Addressing the sender would address this
        // machine, and the answer would land in its own inbox and reach
        // nobody — sent, and gone nowhere (#19).
        let (_dir, service) = service();
        let ana = member(&service, 65);
        let opened = service
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

        let answer = service
            .reply(
                opened.thread_id,
                "and one more thing".to_owned(),
                Vec::new(),
                SenderKind::Human,
            )
            .expect("reply")
            .message;

        assert_eq!(
            answer.to,
            vec![Recipient::Node(ana.node_id())],
            "the conversation is with her, whoever spoke last"
        );
        assert_eq!(answer.thread_id, opened.thread_id);
        assert_eq!(answer.in_reply_to, Some(opened.id));
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
            .expect("send")
            .message;

        let first = events.try_recv().expect("an event");
        let second = events.try_recv().expect("a second event");
        assert_eq!(first, Event::MessageReceived { id: sent.id });
        assert_eq!(second, Event::MessageDelivered { id: sent.id });
        assert_eq!(first.name(), "message.received");
        assert_eq!(first.data(), sent.id.to_string());
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
}
