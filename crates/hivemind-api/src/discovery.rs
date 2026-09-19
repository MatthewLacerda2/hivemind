//! The Tailscale discovery worker (SPEC §5.2).
//!
//! Beside the presence worker, and for the same reason: what a round does is
//! entirely a statement about the address book and the group key, and
//! `hivemind-net` — which owns the parsing and the probing — knows about
//! neither.
//!
//! # Why this exists at all
//!
//! On a LAN, mDNS announces a machine as it arrives and `ServiceSink` greets
//! whatever turns up. A tailnet has no multicast, so until now the equivalent
//! was a human running `hivemind peers refresh` — the opposite of "as soon as
//! it is online, everybody sees it".
//!
//! Presence (SPEC §5.5) handles nodes that are *already* peers, because it
//! knows where to find them. This handles the two cases presence cannot:
//! finding somebody who is not a peer yet, and finding a peer whose address
//! has changed out from under the address book.
//!
//! A round is not free — `tailscale status --json`, then a TCP connect to
//! each host it says is up — so it runs on a timer rather than a tick, and a
//! hello that failed brings the next one forward.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hivemind_core::config::Tailscale;

use crate::service::MailService;

/// How long between rounds when nothing has gone wrong (SPEC §5.2).
pub const ROUND: Duration = Duration::from_secs(30);

/// How often the loop looks at the clock.
///
/// The same shape as presence: short enough that a failed hello is acted on
/// in seconds, cheap enough that a tick which finds nothing due is two
/// comparisons.
const TICK: Duration = Duration::from_secs(5);

/// Look for peers on the tailnet until `shutdown` fires.
///
/// [`Tailscale::Off`] returns at once, and so does a mode that wants
/// Tailscale on a machine that does not have it — there is no point waking
/// every thirty seconds to run a binary that is not there. That check is made
/// once, at startup, which is the same moment `doctor` reports on.
pub async fn find_peers<F>(service: Arc<MailService>, mode: Tailscale, shutdown: F)
where
    F: std::future::Future<Output = ()> + Send,
{
    if !mode.wanted() {
        tracing::debug!("tailscale discovery is off");
        return;
    }

    let mut shutdown = std::pin::pin!(shutdown);
    // Never, so that starting up is itself a round: a daemon that has just
    // come back wants to know who is there now, not in thirty seconds.
    let mut last: Option<DateTime<Utc>> = None;

    loop {
        // A failed hello means an address stopped working, which is the best
        // possible moment to go looking for the one that replaced it.
        let asked = service.take_discovery_request();
        if asked || due(Utc::now(), last, ROUND) {
            match service.refresh_peers().await {
                Ok(0) => tracing::trace!(asked, "no tailnet peers answered"),
                Ok(found) => tracing::debug!(found, asked, "tailnet peers answered"),
                Err(error) => tracing::debug!(%error, "could not look for tailnet peers"),
            }
            last = Some(Utc::now());
        }

        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

/// Is a round due?
///
/// The same judgement presence makes, split out for the same reason: a clock
/// that went somewhere unexpected — a suspended laptop, an NTP correction —
/// is the interesting case and there is no way to test it by waiting.
fn due(now: DateTime<Utc>, last: Option<DateTime<Utc>>, every: Duration) -> bool {
    let Some(last) = last else {
        return true;
    };
    let since = now.signed_duration_since(last);
    since.to_std().is_ok_and(|since| since >= every) || since < chrono::TimeDelta::zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    #[test]
    fn starting_up_is_itself_a_round() {
        // A daemon that has just come back wants to know who is there now.
        assert!(due(at(1_000), None, ROUND));
    }

    #[test]
    fn a_round_is_due_the_moment_the_interval_passes_and_not_before() {
        assert!(!due(at(1_029), Some(at(1_000)), ROUND));
        assert!(due(at(1_030), Some(at(1_000)), ROUND));
    }

    #[test]
    fn a_laptop_that_was_shut_comes_back_overdue() {
        assert!(due(at(1_000 + 8 * 60 * 60), Some(at(1_000)), ROUND));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_stop_discovery_until_it_catches_up() {
        assert!(due(at(1_000), Some(at(9_999)), ROUND));
    }

    /// A `tailscale status --json` that records whether anybody asked.
    ///
    /// The point of the whole seam: against the real binary this test would
    /// pass on a machine with no Tailscale *and* on one where the setting
    /// was ignored, which is no test at all.
    #[derive(Default)]
    struct Spy {
        asked: std::sync::atomic::AtomicBool,
        answer: &'static str,
    }

    impl hivemind_net::discovery::Tailscale for Spy {
        fn status_json(&self) -> Result<String, String> {
            self.asked.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(self.answer.to_owned())
        }
    }

    impl Spy {
        fn was_asked(&self) -> bool {
            self.asked.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[tokio::test]
    async fn tailscale_off_means_the_binary_is_never_run() {
        // Not "run and ignored". `hivemind peers refresh` is "now rather
        // than in thirty seconds", never "despite the setting" — and a
        // refresh is the one path that could reach Tailscale behind the
        // worker's back.
        let (_dir, service) = crate::service::tests::service_with_tailscale(Tailscale::Off);
        let spy = Spy::default();

        let found = service.refresh_peers_from(&spy).await.expect("a refresh");

        assert_eq!(found, 0);
        assert!(
            !spy.was_asked(),
            "it should not have asked Tailscale at all"
        );
    }

    #[tokio::test]
    async fn tailscale_auto_and_on_both_ask() {
        // The other side of the same gate, so the test above cannot pass by
        // the refresh being broken for everybody.
        for mode in [Tailscale::Auto, Tailscale::On] {
            let (_dir, service) = crate::service::tests::service_with_tailscale(mode);
            let spy = Spy::default();

            service.refresh_peers_from(&spy).await.expect("a refresh");

            assert!(spy.was_asked(), "{mode:?} should have asked Tailscale");
        }
    }

    #[tokio::test]
    async fn discovery_turned_off_does_not_run_at_all() {
        // Not "runs and finds nothing": a machine whose owner said `false`
        // should never execute `tailscale`, however installed it is. The test
        // is that this returns rather than looping until the shutdown.
        let (_dir, service) = crate::service::tests::service();
        let service = Arc::new(service);

        let ran = tokio::time::timeout(
            Duration::from_secs(5),
            find_peers(service, Tailscale::Off, std::future::pending()),
        )
        .await;

        assert!(ran.is_ok(), "it should have returned without being told to");
    }
}
