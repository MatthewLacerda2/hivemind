//! Per-recipient delivery state, as the local API reports it (SPEC §8, #31).
//!
//! Two shapes, because a listing and one message want different amounts of it:
//! a row needs a mark and a count, and an open message needs the line per
//! recipient that says who has it and why the others do not.
//!
//! Both are `None`/empty for a message this node did not send, and for one whose
//! only recipient was this machine — a message with nothing to confirm is not
//! the same as one nobody has confirmed, and reporting `0 of 0 delivered` would
//! put a mark on a note to self.

use chrono::{DateTime, Utc};
use hivemind_core::store::{Delivery, RecipientState};
use serde::Serialize;
use utoipa::ToSchema;

/// How a message's recipients are getting on, for one row of a listing.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeliverySummary {
    /// How far the message as a whole has got: the **weakest** claim any of its
    /// recipients supports. One machine that is off holds the whole message at
    /// `queued`, which is what makes a mark in a listing worth reading.
    pub state: String,
    /// How many machines it was addressed to, after expansion at send time.
    pub recipients: usize,
    /// How many of them confirmed taking it.
    pub delivered: usize,
    /// How many of them said they read it.
    pub read: usize,
}

impl DeliverySummary {
    /// Summarise what each recipient has done, or `None` if there is nothing to
    /// confirm.
    #[must_use]
    pub fn of(recipients: &[RecipientState]) -> Option<Self> {
        let weakest = recipients.iter().map(RecipientState::state).min()?;
        Some(Self {
            state: weakest.as_str().to_owned(),
            recipients: recipients.len(),
            delivered: Self::at_least(recipients, Delivery::Delivered),
            read: Self::at_least(recipients, Delivery::Read),
        })
    }

    /// How many recipients have got at least this far.
    ///
    /// `Delivery` orders weakest first, so "delivered" counts the ones that have
    /// read it too — otherwise a message everybody read would report nobody
    /// delivered.
    fn at_least(recipients: &[RecipientState], state: Delivery) -> usize {
        recipients.iter().filter(|r| r.state() >= state).count()
    }
}

/// What one recipient has done with a message, and what is holding it up.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecipientDelivery {
    /// The machine this copy is for.
    pub node: String,
    /// `queued`, `delivered` or `read`.
    pub state: String,
    /// When that machine's daemon confirmed taking it.
    pub delivered_at: Option<DateTime<Utc>>,
    /// When it said the message had been read (SPEC §8).
    pub read_at: Option<DateTime<Utc>>,
    /// How many times delivery has been attempted.
    pub attempts: u32,
    /// When the last attempt was made.
    pub last_attempt: Option<DateTime<Utc>>,
    /// Why the last attempt failed. The answer to "why has this not arrived",
    /// which was two minutes of guessing the first time it was asked (#31).
    pub last_error: Option<String>,
}

impl From<RecipientState> for RecipientDelivery {
    fn from(state: RecipientState) -> Self {
        let named = state.state().as_str().to_owned();
        Self {
            node: state.node.to_string(),
            state: named,
            delivered_at: state.delivered_at,
            read_at: state.read_at,
            attempts: state.attempts,
            last_attempt: state.last_attempt,
            last_error: state.last_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivemind_core::peer::NodeId;

    fn node(seed: u8) -> NodeId {
        NodeId::from_certificate_der(&[seed; 8])
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    #[test]
    fn nothing_to_confirm_is_reported_as_nothing() {
        assert!(DeliverySummary::of(&[]).is_none());
    }

    #[test]
    fn one_machine_that_is_off_holds_the_whole_message_at_queued() {
        // Two recipients, one of them never reached, so "the weakest" is
        // distinguishable from "the first" and from "all of them".
        let mut read = RecipientState::pending(node(1));
        read.delivered_at = Some(at(1_000));
        read.read_at = Some(at(2_000));

        let summary =
            DeliverySummary::of(&[read, RecipientState::pending(node(2))]).expect("two recipients");

        assert_eq!(summary.state, "queued");
        assert_eq!(summary.recipients, 2);
        assert_eq!(summary.delivered, 1, "a read message is a delivered one");
        assert_eq!(summary.read, 1);
    }

    #[test]
    fn everybody_having_read_it_is_the_only_way_a_message_reads_read() {
        let mut one = RecipientState::pending(node(1));
        one.delivered_at = Some(at(1_000));
        one.read_at = Some(at(2_000));
        let mut two = one.clone();
        two.node = node(2);

        let both = DeliverySummary::of(&[one.clone(), two.clone()]).expect("two");
        assert_eq!(both.state, "read");

        let mut unread = two;
        unread.read_at = None;
        let mixed = DeliverySummary::of(&[one, unread]).expect("two");
        assert_eq!(mixed.state, "delivered");
        assert_eq!(mixed.read, 1);
    }

    #[test]
    fn a_recipient_line_carries_why_it_has_not_arrived() {
        let mut waiting = RecipientState::pending(node(3));
        waiting.attempts = 4;
        waiting.last_attempt = Some(at(3_000));
        waiting.last_error = Some("could not reach 10.0.0.5:8400".to_owned());

        let line = RecipientDelivery::from(waiting);
        assert_eq!(line.state, "queued");
        assert_eq!(line.attempts, 4);
        assert_eq!(
            line.last_error.as_deref(),
            Some("could not reach 10.0.0.5:8400")
        );
        assert_eq!(line.node, node(3).to_string());
    }
}
