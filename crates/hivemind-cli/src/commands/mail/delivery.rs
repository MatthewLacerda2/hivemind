//! How far a message has got, put on a terminal (SPEC §10, #31).
//!
//! Two views of the same fact, because two questions are being asked. A
//! listing asks "did they get it" and answers with one mark per row; `read`
//! asks "who has it, and what is holding up the rest" and answers with a line
//! per recipient.
//!
//! The marks are the ones any chat client uses — `·` for queued, `✓` for
//! delivered, `✓✓` for read — because that is the vocabulary somebody already
//! has for this, and inventing another would make the listing need a legend.
//!
//! **A mark is the weakest claim the message supports.** One machine that is
//! off holds the whole row at `·`, which is the point: a row that showed the
//! best of its recipients would say a message had arrived when half of it had
//! not, and that is the shape of all three incidents in #31.

use std::fmt::Write as _;

use serde::Deserialize;

use crate::colour::Paint as _;

use super::short_node;

/// How a sent message's recipients are getting on, as the API reports it.
#[derive(Debug, Deserialize)]
pub(super) struct Delivery {
    /// `queued`, `delivered` or `read`.
    pub(super) state: String,
    /// How many machines it was addressed to.
    pub(super) recipients: usize,
    /// How many of them confirmed taking it.
    pub(super) delivered: usize,
    /// How many of them said they had read it.
    pub(super) read: usize,
}

/// What one recipient has done with it, as the API reports it.
#[derive(Debug, Deserialize)]
pub(super) struct Recipient {
    pub(super) node: String,
    pub(super) state: String,
    pub(super) delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(super) read_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(super) attempts: u32,
    pub(super) last_error: Option<String>,
}

/// The mark and the words for one row of a listing.
///
/// Returned as a plain string rather than printed, so the rule can be tested
/// without a terminal — the marks are the whole of what #31 asks a listing for.
pub(super) fn row(delivery: &Delivery) -> String {
    let Delivery {
        recipients,
        delivered,
        read,
        ..
    } = *delivery;

    if read == recipients {
        return "✓✓ read".to_owned();
    }
    if read > 0 {
        return format!("✓✓ read by {read} of {recipients}");
    }
    if delivered == recipients {
        return "✓ delivered".to_owned();
    }
    if delivered > 0 {
        return format!("✓ delivered to {delivered} of {recipients}");
    }
    "· queued".to_owned()
}

/// Whether a row's state is one that has arrived everywhere.
///
/// Decides the colour, and nothing else: green for a message that is somewhere
/// other than this machine, yellow for one that is not yet.
fn landed(delivery: &Delivery) -> bool {
    delivery.delivered == delivery.recipients
}

/// The coloured form of [`row`], for the listing.
pub(super) fn row_coloured(delivery: &Delivery) -> String {
    let words = row(delivery);
    if landed(delivery) {
        words.green()
    } else {
        words.yellow()
    }
}

/// One line of `hivemind read`'s delivery block.
///
/// The failure reason is on it because "why has this not arrived" is the
/// question, and the first time it was asked the answer was two minutes of
/// guessing at a message that was merely backing off (#31).
pub(super) fn line(recipient: &Recipient) -> String {
    let when = |at: Option<chrono::DateTime<chrono::Utc>>| {
        at.map(|at| format!(" {}", at.format("%Y-%m-%d %H:%M")))
            .unwrap_or_default()
    };

    let (mark, words) = match recipient.state.as_str() {
        "read" => ("✓✓", format!("read{}", when(recipient.read_at))),
        "delivered" => ("✓ ", format!("delivered{}", when(recipient.delivered_at))),
        _ => {
            let mut words = String::from("queued");
            if recipient.attempts > 0 {
                let attempts = recipient.attempts;
                let plural = if attempts == 1 { "attempt" } else { "attempts" };
                // INVARIANT: writing to a String cannot fail.
                let _ = write!(words, " · {attempts} {plural}");
            }
            if let Some(why) = &recipient.last_error {
                let _ = write!(words, " · {why}");
            }
            ("· ", words)
        }
    };

    format!("  {mark} {}  {words}", short_node(&recipient.node))
}

/// Print the delivery block of `hivemind read`, if there is one to print.
pub(super) fn block(recipients: &[Recipient]) {
    if recipients.is_empty() {
        return;
    }
    println!();
    println!("{}", "delivery".dimmed());
    for recipient in recipients {
        println!("{}", line(recipient));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery(recipients: usize, delivered: usize, read: usize) -> Delivery {
        // `state` is the API's own summary and this module does not read it;
        // the counts are what the marks are made of.
        Delivery {
            state: String::new(),
            recipients,
            delivered,
            read,
        }
    }

    #[test]
    fn one_machine_that_is_off_holds_the_whole_row_at_queued() {
        // Two recipients, so "the weakest" is distinguishable from "the first"
        // and from "all of them" — which is the only thing a mark has to get
        // right.
        assert_eq!(row(&delivery(2, 0, 0)), "· queued");
        assert_eq!(row(&delivery(2, 1, 0)), "✓ delivered to 1 of 2");
        assert_eq!(row(&delivery(2, 2, 0)), "✓ delivered");
        assert_eq!(row(&delivery(2, 2, 1)), "✓✓ read by 1 of 2");
        assert_eq!(row(&delivery(2, 2, 2)), "✓✓ read");
    }

    #[test]
    fn one_recipient_is_said_without_a_count() {
        // "delivered to 1 of 1" is a sentence nobody wants to read.
        assert_eq!(row(&delivery(1, 0, 0)), "· queued");
        assert_eq!(row(&delivery(1, 1, 0)), "✓ delivered");
        assert_eq!(row(&delivery(1, 1, 1)), "✓✓ read");
    }

    #[test]
    fn a_row_is_green_only_once_everybody_has_it() {
        assert!(landed(&delivery(2, 2, 0)));
        assert!(!landed(&delivery(2, 1, 1)), "one of them still has not");
        assert!(!landed(&delivery(1, 0, 0)));
    }

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("in range")
    }

    /// A recipient named the way the API names one, so the line is the line.
    fn recipient(state: &str) -> Recipient {
        Recipient {
            node: "hm1:abcd-ef01-2345-6789-abcd-ef01-2345".to_owned(),
            state: state.to_owned(),
            delivered_at: None,
            read_at: None,
            attempts: 0,
            last_error: None,
        }
    }

    #[test]
    fn a_read_line_says_when_they_read_it() {
        let mut them = recipient("read");
        them.delivered_at = Some(at(1_750_000_000));
        them.read_at = Some(at(1_750_003_600));
        assert_eq!(line(&them), "  ✓✓ abcd  read 2025-06-15 16:06");
    }

    #[test]
    fn a_queued_line_says_what_is_holding_it_up() {
        let mut them = recipient("queued");
        them.attempts = 4;
        them.last_error = Some("could not reach 10.0.0.9:8400".to_owned());
        assert_eq!(
            line(&them),
            "  ·  abcd  queued · 4 attempts · could not reach 10.0.0.9:8400"
        );

        them.attempts = 1;
        them.last_error = None;
        assert_eq!(line(&them), "  ·  abcd  queued · 1 attempt");
    }

    #[test]
    fn a_queued_line_with_nothing_to_report_says_only_queued() {
        // The first pass has not run yet. "0 attempts" would read as a failure.
        assert_eq!(line(&recipient("queued")), "  ·  abcd  queued");
    }
}
