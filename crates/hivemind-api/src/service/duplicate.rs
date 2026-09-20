//! Noticing that a message has just been sent again (#33).
//!
//! On the first day of real use the same message appeared twice, with two ids,
//! twice, on two machines. The queue was measured first and does not duplicate
//! — `an_hour_of_retries_against_a_peer_that_is_off_leaves_one_record` in
//! `outbox.rs` counts the records an hour of retries leaves — so what is left
//! is a person or an agent running the command twice, and nothing noticing that
//! the previous message was identical and seconds old.
//!
//! It **warns rather than refuses.** Asking somebody the same thing again is
//! legitimate — a nudge a minute later is a real message — and a mail service
//! that declines to carry mail is worse than one that carries a copy nobody
//! wanted. The notice travels with the send so that every surface can say it,
//! and the daemon log says it regardless.
//!
//! The receiver cannot do this job. Its idempotency is on the message id
//! (SPEC §8), and two presses produce two ids; by the time a copy arrives it is
//! as valid as the first and the far end has no way to tell them apart from two
//! deliberate messages.

use super::{DateTime, Draft, MailService, Mailbox, Message, Ulid, Utc};

/// How recently an identical message must have gone out to count as a repeat.
///
/// Measured rather than chosen: #33's two pairs were in the same second and
/// about a minute apart, so a window shorter than a minute would have missed
/// half the evidence it was filed on. Two minutes covers both with room, and
/// because this warns rather than refuses, being wrong costs one line.
pub const REPEAT_WINDOW: std::time::Duration = std::time::Duration::from_mins(2);

/// What a send produced.
///
/// A struct rather than a bare [`Message`] so the notice cannot be lost by a
/// surface that did not think to ask for it: naming the message is what every
/// caller already has to do.
#[derive(Debug, Clone)]
pub struct Queued {
    /// The message that was written and queued.
    pub message: Message,
    /// An identical message sent inside [`REPEAT_WINDOW`], if there was one.
    pub duplicate_of: Option<Ulid>,
}

impl MailService {
    /// The newest identical message sent inside [`REPEAT_WINDOW`], if any.
    ///
    /// Reads `out/` and `sent/` rather than the index, because the files are
    /// the source of truth (ADR 0002) and this must still work on the pass
    /// after somebody deleted `index.db`. Both hold only this node's own sends,
    /// and a ULID orders by the moment it was generated, so walking each
    /// backwards stops at the first message older than the window rather than
    /// reading a year of mail.
    pub(super) fn recent_duplicate(&self, draft: &Draft) -> Option<Ulid> {
        let cutoff = Utc::now() - chrono::Duration::from_std(REPEAT_WINDOW).ok()?;
        [Mailbox::Out, Mailbox::Sent]
            .into_iter()
            .find_map(|mailbox| {
                self.store
                    .list(mailbox)
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .take_while(|id| generated_at(*id).is_some_and(|at| at >= cutoff))
                    .find(|id| {
                        self.store
                            .get(mailbox, *id)
                            .is_ok_and(|message| repeats(&message, draft))
                    })
            })
    }
}

/// When this id was generated.
fn generated_at(id: Ulid) -> Option<DateTime<Utc>> {
    i64::try_from(id.timestamp_ms())
        .ok()
        .and_then(DateTime::from_timestamp_millis)
}

/// Whether `message` is this draft again.
///
/// Recipients, subject, body, kind and the message it answers, all exactly. The
/// case this is for is one request arriving twice, which differs in nothing —
/// anything looser would start warning about two people being told the same
/// news. `to` is compared as it was written rather than as it expanded, because
/// what somebody typed is what they typed twice.
fn repeats(message: &Message, draft: &Draft) -> bool {
    message.to == draft.to
        && message.subject == draft.subject
        && message.body == draft.body
        && message.kind == draft.kind
        && message.in_reply_to == draft.in_reply_to
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::service;
    use hivemind_core::crypto::Signature;
    use hivemind_core::message::{Kind, Recipient, SenderKind};

    /// A draft addressed to this node, so no peer has to exist for it to go.
    fn draft(service: &MailService, subject: &str, body: &str) -> Draft {
        Draft {
            to: vec![Recipient::Node(service.identity())],
            subject: subject.to_owned(),
            body: body.to_owned(),
            kind: Kind::Message,
            in_reply_to: None,
            attachments: Vec::new(),
        }
    }

    fn send(service: &MailService, draft: Draft) -> Queued {
        service.send(draft, SenderKind::Human).expect("send")
    }

    #[test]
    fn the_first_send_of_something_is_not_a_repeat() {
        let (_dir, service) = service();
        let queued = send(&service, draft(&service, "primeiro contato", "olá"));
        assert!(queued.duplicate_of.is_none());
    }

    #[test]
    fn the_same_message_again_names_the_one_before_it() {
        // #33 as reported: same recipient, same subject, same body, seconds
        // apart, and nothing said so until the other end had both.
        let (_dir, service) = service();
        let first = send(&service, draft(&service, "primeiro contato", "olá"));
        let second = send(&service, draft(&service, "primeiro contato", "olá"));

        assert_eq!(second.duplicate_of, Some(first.message.id));
        assert_ne!(second.message.id, first.message.id);
    }

    #[test]
    fn a_notice_is_not_a_refusal() {
        // Both copies exist and both are readable. hivemind carries the mail;
        // what it now also does is say that it has just carried this.
        let (_dir, service) = service();
        let first = send(&service, draft(&service, "ping", "still there?"));
        let second = send(&service, draft(&service, "ping", "still there?"));

        assert!(service.get(first.message.id).is_ok());
        assert!(service.get(second.message.id).is_ok());
    }

    #[test]
    fn a_different_subject_or_body_is_not_a_repeat() {
        // Two people being told the same news, or one person being told two
        // things, must not start warning about each other.
        let (_dir, service) = service();
        send(&service, draft(&service, "standup", "on my way"));

        assert!(
            send(&service, draft(&service, "standup", "five minutes"))
                .duplicate_of
                .is_none(),
            "a different body is a different message"
        );
        assert!(
            send(&service, draft(&service, "lunch", "on my way"))
                .duplicate_of
                .is_none(),
            "so is a different subject"
        );
    }

    #[test]
    fn the_same_words_to_somebody_else_are_not_a_repeat() {
        let (_dir, service) = service();
        send(&service, draft(&service, "standup", "on my way"));

        let elsewhere = Draft {
            to: vec![Recipient::Owner("ana".to_owned())],
            ..draft(&service, "standup", "on my way")
        };
        assert!(service.recent_duplicate(&elsewhere).is_none());
    }

    #[test]
    fn a_copy_still_waiting_in_the_outbox_counts_too() {
        // The case #33 was filed on has the recipient switched off, so the
        // first copy is still in `out/`. Looking only in `sent/` would find
        // nothing exactly when the repeat matters most.
        let (_dir, service) = service();
        let friend = hivemind_core::identity::Identity::from_seed([44u8; 32]).expect("identity");
        service
            .admit(
                friend.node_id(),
                "friend",
                None,
                friend.certificate_der().to_vec(),
                hivemind_core::peerbook::PeerAddr::manual("10.0.0.4", 8400),
            )
            .expect("admit");
        let to_friend = Draft {
            to: vec![Recipient::Node(friend.node_id())],
            ..draft(&service, "waiting", "x")
        };

        let first = send(&service, to_friend.clone());

        assert_eq!(service.recent_duplicate(&to_friend), Some(first.message.id));
    }

    /// A service holding one copy of a draft, filed under an id generated
    /// `age` ago.
    ///
    /// The age of a message is the age of its ULID, so the window can be
    /// tested from both sides without a test that waits two minutes.
    fn with_a_copy_aged(age: std::time::Duration) -> (tempfile::TempDir, MailService, Draft, Ulid) {
        let (dir, service) = service();
        let draft = draft(&service, "ping", "still there?");
        let then = Utc::now() - chrono::Duration::from_std(age).expect("in range");
        let id = Ulid::from_parts(
            u64::try_from(then.timestamp_millis()).expect("after 1970"),
            7,
        );
        let message = Message {
            id,
            thread_id: id,
            in_reply_to: None,
            from: service.identity(),
            to: draft.to.clone(),
            subject: draft.subject.clone(),
            body: draft.body.clone(),
            kind: draft.kind,
            sender_kind: SenderKind::Human,
            attachments: Vec::new(),
            sent_at: then,
            received_at: None,
            signature: Signature::from_bytes([0u8; 64]),
        };
        service
            .store
            .put_outbound(
                Mailbox::Sent,
                &hivemind_core::store::Outbound {
                    recipients: Vec::new(),
                    message,
                },
            )
            .expect("file it");
        (dir, service, draft, id)
    }

    #[test]
    fn a_copy_from_just_inside_the_window_is_a_repeat() {
        let (_dir, service, draft, id) =
            with_a_copy_aged(REPEAT_WINDOW.saturating_sub(std::time::Duration::from_secs(1)));
        assert_eq!(service.recent_duplicate(&draft), Some(id));
    }

    #[test]
    fn a_copy_from_just_outside_it_is_not() {
        // Asking again is a real message, and the longer ago the first one
        // went the more likely that is what this is.
        let (_dir, service, draft, _) =
            with_a_copy_aged(REPEAT_WINDOW + std::time::Duration::from_secs(1));
        assert!(service.recent_duplicate(&draft).is_none());
    }
}
