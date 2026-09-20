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
        self.store.put_outbound(&outbound)?;
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
    ) -> Result<Queued, ServiceError> {
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
