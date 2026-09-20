//! Read receipts, both directions (SPEC §8, ADR 0016).
//!
//! Outward: when a message from a peer is marked read here and
//! `read_receipts` is on, a receipt is queued for the node that sent it and
//! retried until that node takes it. Inward: a receipt a peer delivers is
//! recorded against the envelope in `out/` or `sent/`, and only ever against
//! the entry for the node that delivered it.
//!
//! **Receiving is not configurable.** The switch governs what leaves this
//! machine, because that is the half that is somebody's business but their
//! own. A receipt that arrives here is a choice the other person already made.

use super::*;

use hivemind_core::receipts::{OwedReceipt, ReadNote};

impl MailService {
    /// Start owing the sender of `message` a read receipt.
    ///
    /// Silent when receipts are off, when the message is our own, and when the
    /// sender is not a peer — in the last case there is nowhere to deliver it
    /// and a file nothing can settle would be owed for ever.
    pub(super) fn owe_read_receipt(&self, message: &Message) {
        if !self.read_receipts || message.from == self.identity {
            return;
        }
        if !self.is_paired(message.from).unwrap_or(false) {
            return;
        }

        let owed = OwedReceipt::new(message.id, message.from, Utc::now().trunc_subsecs(3));
        match self.receipts.owe(&owed) {
            // Delivery is not attempted here: this is called from a read, and
            // a read must not wait on a socket.
            Ok(true) => tracing::debug!(
                id = %message.id,
                peer = %message.from.short(),
                "a read receipt is owed"
            ),
            Ok(false) => {}
            // A receipt that cannot be written is a courtesy not extended, and
            // failing the read it came from would be much worse.
            Err(error) => {
                tracing::warn!(%error, id = %message.id, "could not queue a read receipt");
            }
        }
    }

    /// Every read receipt this node still owes, oldest message first.
    ///
    /// # Errors
    /// [`ServiceError::Store`] if the queue cannot be read.
    pub fn owed_receipts(&self) -> Result<Vec<OwedReceipt>, ServiceError> {
        Ok(self.receipts.owed()?)
    }

    /// Stop owing these, because the node they were for has taken them.
    pub fn settle_receipts(&self, messages: &[Ulid]) {
        for message in messages {
            if let Err(error) = self.receipts.settle(*message) {
                tracing::warn!(%error, id = %message, "could not settle a read receipt");
            }
        }
    }

    /// Write receipts back after a failed attempt, keeping what they learned.
    pub fn save_receipts(&self, receipts: &[OwedReceipt]) {
        for receipt in receipts {
            if let Err(error) = self.receipts.save(receipt) {
                tracing::warn!(%error, id = %receipt.message, "could not save a read receipt");
            }
        }
    }

    /// Record that `reader` has read messages this node sent them (SPEC §8).
    ///
    /// Returns how many named a message this node has and had addressed to
    /// them. Fewer than were offered is not an error: the sender may have
    /// deleted the message, and a receipt for one it no longer holds is a fact
    /// with nowhere to go.
    ///
    /// The reader is the authenticated caller, never anything in the body. A
    /// node that could nominate the reader could report on somebody else's
    /// reading, which is the one thing this feature must not allow.
    ///
    /// # Errors
    /// [`ServiceError::Store`] if an envelope exists and cannot be written.
    pub fn record_read_receipts(
        &self,
        reader: NodeId,
        notes: &[ReadNote],
    ) -> Result<usize, ServiceError> {
        let mut recorded = 0;
        for note in notes {
            let Some(mailbox) = self.outgoing_mailbox_of(note.id)? else {
                continue;
            };
            let mut outbound = self.store.get_outbound(mailbox, note.id)?;
            if !outbound.mark_read(reader, note.read_at) {
                // Either they are not a recipient, or they have told us
                // already. Redelivery of a receipt is as safe as redelivery of
                // mail (SPEC §8), so neither is a failure.
                continue;
            }

            if mailbox == Mailbox::Out {
                // Through `save_outbound`, because a receipt is also proof of
                // delivery and can be the thing that completes a message and
                // moves it out of `out/`.
                self.save_outbound(&outbound)?;
            } else {
                // Already finished. Writing the envelope back directly rather
                // than through `save_outbound`, which would announce
                // `message.delivered` again for something delivered days ago.
                self.store.put_outbound(Mailbox::Sent, &outbound)?;
            }
            recorded += 1;
            tracing::info!(
                id = %note.id,
                peer = %reader.short(),
                "they have read it"
            );
        }
        Ok(recorded)
    }

    /// Which outgoing mailbox holds `id`, if either does.
    fn outgoing_mailbox_of(&self, id: Ulid) -> Result<Option<Mailbox>, ServiceError> {
        for mailbox in [Mailbox::Out, Mailbox::Sent] {
            match self.store.get_outbound(mailbox, id) {
                Ok(_) => return Ok(Some(mailbox)),
                Err(StoreError::NotFound { .. }) => {}
                Err(other) => return Err(other.into()),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::service;

    /// A service whose owner has turned receipts on, and a member of its group.
    fn pair_with_receipts(on: bool) -> (tempfile::TempDir, MailService, NodeId) {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity = NodeId::from_certificate_der(b"this node");
        let service = MailService::open(
            dir.path(),
            NodeDescription {
                read_receipts: on,
                ..crate::service::tests::describe(identity)
            },
            SigningKey::from_bytes(&[11u8; 32]),
        )
        .expect("service");

        let ana = hivemind_core::identity::Identity::from_seed([41u8; 32]).expect("identity");
        service
            .admit(
                ana.node_id(),
                "ana-mbp",
                Some("ana"),
                ana.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.2", 8400),
            )
            .expect("admit");
        (dir, service, ana.node_id())
    }

    /// One message from `from`, already in `new/`.
    fn arrives(service: &MailService, from: NodeId, millis: u64) -> Ulid {
        let ana = hivemind_core::identity::Identity::from_seed([41u8; 32]).expect("identity");
        let id = Ulid::from_parts(millis, 0);
        let mut message = Message {
            id,
            thread_id: id,
            in_reply_to: None,
            from,
            to: vec![Recipient::Node(service.identity())],
            subject: "read me".to_owned(),
            body: "body".to_owned(),
            kind: Kind::Message,
            sender_kind: SenderKind::Human,
            attachments: Vec::new(),
            sent_at: DateTime::from_timestamp_millis(i64::try_from(millis).expect("in range"))
                .expect("a timestamp"),
            received_at: None,
            signature: Signature::from_bytes([0u8; 64]),
        };
        message.sign(ana.signing_key()).expect("sign");
        service.receive(from, message).expect("receive")
    }

    #[test]
    fn nothing_is_owed_while_read_receipts_are_off() {
        // The default, and the half of #31 that is somebody's own business.
        let (_dir, service, ana) = pair_with_receipts(false);
        let id = arrives(&service, ana, 100);

        service.mark_read(id).expect("mark read");

        assert!(service.owed_receipts().expect("owed").is_empty());
    }

    #[test]
    fn reading_a_peers_message_owes_them_a_receipt_when_they_are_on() {
        let (_dir, service, ana) = pair_with_receipts(true);
        let id = arrives(&service, ana, 100);
        assert!(
            service.owed_receipts().expect("owed").is_empty(),
            "arriving is not reading"
        );

        service.mark_read(id).expect("mark read");

        let owed = service.owed_receipts().expect("owed");
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].message, id);
        assert_eq!(owed[0].to, ana);
    }

    #[test]
    fn reading_the_same_message_twice_owes_one_receipt() {
        let (_dir, service, ana) = pair_with_receipts(true);
        let id = arrives(&service, ana, 100);
        service.mark_read(id).expect("first");
        service
            .mark_read(id)
            .expect("a second click is not a failure");
        assert_eq!(service.owed_receipts().expect("owed").len(), 1);
    }

    #[test]
    fn our_own_mail_owes_nobody_a_receipt() {
        // Reading something this machine sent is reading it (#31), and a
        // receipt addressed to ourselves would be owed for ever.
        let (_dir, service, _ana) = pair_with_receipts(true);
        let sent = service
            .send(
                crate::service::tests::draft_to_self(&service, "note to self", "body"),
                SenderKind::Human,
            )
            .expect("send")
            .message;

        service.mark_read(sent.id).expect("mark read");

        assert!(service.owed_receipts().expect("owed").is_empty());
    }

    #[test]
    fn a_receipt_marks_the_reader_and_leaves_the_other_recipients_alone() {
        // Two recipients, one of which never says anything, so "the right
        // one's state" is distinguishable from "all of them".
        let (_dir, service) = service();
        let ana = hivemind_core::identity::Identity::from_seed([42u8; 32]).expect("identity");
        let beto = hivemind_core::identity::Identity::from_seed([43u8; 32]).expect("identity");
        for who in [&ana, &beto] {
            service
                .admit(
                    who.node_id(),
                    "machine",
                    None,
                    who.certificate_der().to_vec(),
                    PeerAddr::manual("10.0.0.2", 8400),
                )
                .expect("admit");
        }
        let sent = service
            .send(
                Draft {
                    to: vec![
                        Recipient::Node(ana.node_id()),
                        Recipient::Node(beto.node_id()),
                    ],
                    subject: "two of you".to_owned(),
                    body: "body".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send")
            .message;

        let read_at = Utc::now().trunc_subsecs(3);
        let recorded = service
            .record_read_receipts(
                ana.node_id(),
                &[ReadNote {
                    id: sent.id,
                    read_at,
                }],
            )
            .expect("record");

        assert_eq!(recorded, 1);
        let recipients = service.delivery_of(sent.id).expect("ours").expect("sent");
        let state_of = |who: NodeId| {
            recipients
                .iter()
                .find(|r| r.node == who)
                .map(RecipientState::state)
        };
        assert_eq!(
            state_of(ana.node_id()),
            Some(hivemind_core::store::Delivery::Read)
        );
        assert_eq!(
            state_of(beto.node_id()),
            Some(hivemind_core::store::Delivery::Queued),
            "the one who said nothing is untouched"
        );
    }

    #[test]
    fn a_receipt_from_somebody_who_was_not_a_recipient_records_nothing() {
        // The caller is authenticated, so this is not a forgery route — but a
        // node vouching for a message it was not sent must still change
        // nothing, or a member could mark any message read.
        let (_dir, service, ana) = pair_with_receipts(true);
        let sent = service
            .send(
                crate::service::tests::draft_to_self(&service, "not for ana", "body"),
                SenderKind::Human,
            )
            .expect("send")
            .message;

        let recorded = service
            .record_read_receipts(
                ana,
                &[ReadNote {
                    id: sent.id,
                    read_at: Utc::now(),
                }],
            )
            .expect("record");

        assert_eq!(recorded, 0);
    }

    #[test]
    fn a_receipt_for_a_message_we_do_not_have_is_ignored_rather_than_an_error() {
        let (_dir, service, ana) = pair_with_receipts(true);
        assert_eq!(
            service
                .record_read_receipts(
                    ana,
                    &[ReadNote {
                        id: Ulid::generate(),
                        read_at: Utc::now()
                    }]
                )
                .expect("not an error"),
            0
        );
    }

    #[test]
    fn a_receipt_for_something_long_delivered_does_not_announce_it_again() {
        // `message.delivered` means "it reached everybody", and a receipt
        // arriving a week later is not that happening a second time.
        let (_dir, service, ana) = pair_with_receipts(true);
        let sent = service
            .send(
                Draft {
                    to: vec![Recipient::Node(ana)],
                    subject: "long gone".to_owned(),
                    body: "body".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send")
            .message;
        let mut outbound = service
            .store
            .get_outbound(Mailbox::Out, sent.id)
            .expect("envelope");
        assert!(outbound.mark_delivered(ana, Utc::now()));
        service.save_outbound(&outbound).expect("save");
        assert_eq!(service.get(sent.id).expect("get").0, Mailbox::Sent);

        let mut events = service.subscribe();
        let read_at = Utc::now().trunc_subsecs(3);
        assert_eq!(
            service
                .record_read_receipts(
                    ana,
                    &[ReadNote {
                        id: sent.id,
                        read_at
                    }]
                )
                .expect("record"),
            1
        );

        assert!(
            events.try_recv().is_err(),
            "nothing happened that a client has to hear about"
        );
        assert_eq!(
            service.delivery_of(sent.id).expect("ours").expect("sent")[0].read_at,
            Some(read_at)
        );
    }

    #[test]
    fn a_receipt_settles_and_stays_settled() {
        let (_dir, service, ana) = pair_with_receipts(true);
        let first = arrives(&service, ana, 100);
        let second = arrives(&service, ana, 200);
        service.mark_read(first).expect("read");
        service.mark_read(second).expect("read");

        service.settle_receipts(&[first]);

        let left = service.owed_receipts().expect("owed");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, second);
    }

    #[test]
    fn a_failed_attempt_is_written_back_so_backoff_survives_a_restart() {
        let (_dir, service, ana) = pair_with_receipts(true);
        let id = arrives(&service, ana, 100);
        service.mark_read(id).expect("read");

        let mut owed = service.owed_receipts().expect("owed");
        owed[0].attempted(Utc::now(), "could not reach 10.0.0.2:8400");
        service.save_receipts(&owed);

        let back = service.owed_receipts().expect("owed");
        assert_eq!(back[0].attempts, 1);
        assert_eq!(
            back[0].last_error.as_deref(),
            Some("could not reach 10.0.0.2:8400")
        );
    }
}
