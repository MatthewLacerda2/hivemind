//! The presence worker (SPEC §5.5).
//!
//! Beside the delivery worker rather than inside `hivemind-net`, for the same
//! reason `prefetch_attachments` is: what a round does is entirely a statement
//! about the address book and the group key, and the crate below this one
//! knows about neither.
//!
//! # Why there is no wake notification
//!
//! SPEC §5.5 asks for a round on start, on wake, and on a change of network.
//! All three are the same event as far as this loop is concerned — a stretch
//! of wall-clock time passed in which no round happened — and all three fall
//! out of checking the clock rather than asking the operating system.
//!
//! That works because `tokio::time::sleep` is on a monotonic clock, and on
//! Darwin the monotonic clock does not advance while the machine is suspended.
//! A laptop closed for eight hours resumes its pending sleep and finishes it
//! within a tick; by then the wall clock has moved eight hours, the round is
//! overdue, and the loop does one. No `IOKit`, no `#[cfg]`, and nothing to test
//! on a machine that cannot be suspended.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::service::MailService;

/// How often the loop looks at the clock.
///
/// Short enough that coming back from a closed lid is noticed in seconds
/// rather than at the end of an interval, and cheap enough to be free: a tick
/// that finds nothing due does one comparison.
const TICK: Duration = Duration::from_secs(5);

/// Say hello to every peer, for as long as `shutdown` has not fired.
///
/// `interval` of zero turns presence off entirely and this returns at once —
/// which is what `presence_interval = 0` means, and what the integration
/// tests that are not about presence use.
pub async fn say_hello<F>(service: Arc<MailService>, interval: Duration, shutdown: F)
where
    F: std::future::Future<Output = ()> + Send,
{
    if interval.is_zero() {
        tracing::debug!("presence is off");
        return;
    }

    let tick = TICK.min(interval);
    let mut shutdown = std::pin::pin!(shutdown);
    // Never, so that starting up is itself a round — SPEC §5.5's "on start".
    let mut last: Option<DateTime<Utc>> = None;

    loop {
        if round_due(Utc::now(), last, interval) {
            service.presence_round().await;
            last = Some(Utc::now());
        }

        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(tick) => {}
        }
    }
}

/// Is a presence round overdue?
///
/// Split from the loop so that the three cases can be tested without a clock
/// to move: never having run, having run recently, and a wall clock that went
/// somewhere unexpected.
fn round_due(now: DateTime<Utc>, last: Option<DateTime<Utc>>, interval: Duration) -> bool {
    let Some(last) = last else {
        return true;
    };
    let since = now.signed_duration_since(last);
    // A negative gap is the clock going backwards — an NTP correction, or a
    // machine that woke up believing it is last Tuesday. Waiting for it to
    // catch up would mean no presence until it did, so the round happens now.
    since.to_std().is_ok_and(|since| since >= interval) || since < chrono::TimeDelta::zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    #[test]
    fn starting_up_is_itself_a_round() {
        // SPEC §5.5: on start. A daemon that waited a full interval before
        // saying anything would be invisible for that minute, which is the
        // complaint presence exists to answer.
        assert!(round_due(at(1_000), None, Duration::from_mins(1)));
    }

    #[test]
    fn a_round_just_done_is_not_due_again() {
        assert!(!round_due(
            at(1_030),
            Some(at(1_000)),
            Duration::from_mins(1)
        ));
    }

    #[test]
    fn a_round_is_due_the_moment_the_interval_has_passed_and_not_before() {
        let interval = Duration::from_mins(1);
        assert!(!round_due(at(1_059), Some(at(1_000)), interval));
        assert!(round_due(at(1_060), Some(at(1_000)), interval));
    }

    #[test]
    fn a_laptop_that_was_shut_for_eight_hours_is_overdue_when_it_opens() {
        // This is all "on wake" amounts to. The monotonic clock the sleep runs
        // on did not advance while the machine was suspended, so the tick
        // finishes shortly after the lid opens — and the wall clock has moved.
        assert!(round_due(
            at(1_000 + 8 * 60 * 60),
            Some(at(1_000)),
            Duration::from_mins(1)
        ));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_stop_presence_until_it_catches_up() {
        // An NTP correction after a machine boots with a dead battery clock
        // would otherwise hold the next round until real time caught up with
        // the future timestamp — which can be days.
        assert!(round_due(
            at(1_000),
            Some(at(9_999)),
            Duration::from_mins(1)
        ));
    }
}
