//! Store-and-forward delivery.
//!
//! Sending writes to `mail/out/` and returns; a worker walks the outbox and
//! retries with jittered exponential backoff from 2 s to 5 min, forever
//! (SPEC §8). A laptop that comes to the office on Monday receives Friday's
//! mail.
//!
//! The pieces are split so the interesting parts can be tested without
//! sockets or clocks: [`backoff`] is a pure function, [`attempt_all`] makes one
//! pass over one message's outstanding recipients against any [`Transport`],
//! and only [`run`] sleeps.

use std::time::Duration;

use chrono::{DateTime, Utc};
use hivemind_core::message::Message;
use hivemind_core::peer::NodeId;
use hivemind_core::store::{MailStore, Mailbox, Outbound, RecipientState};

use crate::client::ClientError;

/// The shortest wait between attempts (SPEC §8).
pub const MIN_BACKOFF: Duration = Duration::from_secs(2);

/// The longest wait between attempts (SPEC §8).
///
/// Delivery never gives up, so this is the steady state for a peer that is
/// simply switched off — once every five minutes, indefinitely.
pub const MAX_BACKOFF: Duration = Duration::from_mins(5);

/// How far either side of the base delay jitter may move it.
///
/// Without this every node that failed at the same moment — which is what a
/// router reboot looks like — would retry at the same moment too, forever.
const JITTER: f64 = 0.25;

/// How long to wait after `attempts` consecutive failures.
///
/// `jitter` is a random number in `[0, 1)`; it is a parameter rather than
/// drawn here so the spread can be tested rather than hoped for. The result is
/// always within `[MIN_BACKOFF, MAX_BACKOFF]`, including at the cap — where
/// jitter still spreads *downward*, which is the case that matters, because
/// that is where a herd would otherwise form.
#[must_use]
pub fn backoff(attempts: u32, jitter: f64) -> Duration {
    // Saturating, because a message to a peer that has been off for a year has
    // a large attempt count and shifting by it would be undefined.
    let doublings = attempts.saturating_sub(1).min(31);
    let base = MIN_BACKOFF
        .saturating_mul(1u32 << doublings)
        .min(MAX_BACKOFF);

    // `f64::clamp` passes NaN straight through, and `Duration::mul_f64` panics
    // on it. A random source that returns NaN is broken, not a reason to take
    // the daemon down, so it is treated as "no jitter".
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let factor = (1.0 - JITTER) + jitter * (2.0 * JITTER);
    base.mul_f64(factor).clamp(MIN_BACKOFF, MAX_BACKOFF)
}

/// Has this recipient's backoff elapsed?
///
/// A recipient that has never been tried is always due. Otherwise it is due
/// once [`backoff`] has passed since the last attempt — which is per-recipient,
/// so one peer being unreachable does not hold back another on the same
/// message.
#[must_use]
pub fn is_due(state: &RecipientState, now: DateTime<Utc>, jitter: f64) -> bool {
    let Some(last) = state.last_attempt else {
        return true;
    };
    let waited = now.signed_duration_since(last).to_std().unwrap_or_default();
    waited >= backoff(state.attempts, jitter)
}

/// A jitter value in `[0, 1)`.
///
/// Drawn from the same source as everything else random here (SPEC §6.1) so
/// there is one place to look when asking where randomness comes from.
fn jitter() -> f64 {
    let mut bytes = [0u8; 4];
    // A failure here is not worth taking delivery down for; the midpoint is a
    // sound delay, just an unjittered one.
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn the_first_retry_waits_about_two_seconds() {
        assert_eq!(backoff(1, 0.5), MIN_BACKOFF);
        assert!(backoff(1, 0.0) >= MIN_BACKOFF, "the floor is a floor");
        assert!(backoff(1, 1.0) <= Duration::from_millis(2_500));
    }

    #[test]
    fn the_wait_doubles_until_it_reaches_five_minutes() {
        // Without jitter (0.5 is the midpoint) the sequence is exactly the one
        // SPEC §8 describes.
        let plain: Vec<u64> = (1..=10).map(|n| backoff(n, 0.5).as_secs()).collect();
        assert_eq!(plain, vec![2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    }

    #[test]
    fn it_never_leaves_the_range_however_many_attempts() {
        for attempts in [0, 1, 2, 30, 31, 32, 1_000, u32::MAX] {
            for jitter in [0.0, 0.5, 1.0] {
                let delay = backoff(attempts, jitter);
                assert!(
                    delay >= MIN_BACKOFF && delay <= MAX_BACKOFF,
                    "backoff({attempts}, {jitter}) = {delay:?} is outside the range"
                );
            }
        }
    }

    #[test]
    fn jitter_spreads_retries_so_peers_do_not_all_wake_together() {
        // At the cap especially: that is where every waiting node ends up.
        let earliest = backoff(u32::MAX, 0.0);
        let latest = backoff(u32::MAX, 1.0);
        let spread = latest.saturating_sub(earliest);
        assert!(
            spread >= Duration::from_mins(1),
            "the spread at the cap was only {spread:?}"
        );
    }

    #[test]
    fn an_out_of_range_jitter_cannot_push_the_delay_out_of_range() {
        // The caller supplies this; a bad random source must not become a
        // thirty-hour retry interval.
        assert!(backoff(5, -5.0) >= MIN_BACKOFF);
        assert!(backoff(5, 99.0) <= MAX_BACKOFF);
        assert!(backoff(5, f64::NAN) >= MIN_BACKOFF);
    }
}

/// How a pass reaches a peer.
///
/// A trait so the retry logic can be tested against a peer that fails twice
/// and then succeeds, which is tedious to arrange with real sockets and
/// impossible to arrange quickly.
pub trait Transport {
    /// Hand `message` to `node`, which is expected to be at `addr`.
    ///
    /// The node is named as well as the address because delivery pins the
    /// recipient's certificate. A client that merely trusted every paired peer
    /// would hand one peer's mail to another that happened to answer on the
    /// address we had for it.
    fn deliver(
        &self,
        node: NodeId,
        addr: &str,
        message: &Message,
    ) -> impl std::future::Future<Output = Result<(), ClientError>> + Send;
}

/// What one pass over an outbox entry achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// Recipients newly delivered, each with the address that worked.
    ///
    /// The caller records these against the address book so the next message
    /// tries the working address first.
    pub delivered: Vec<(NodeId, String)>,
    /// Recipients still owed a copy after this pass.
    pub remaining: usize,
    /// How many recipients were actually tried.
    ///
    /// Zero means every outstanding recipient is still inside its backoff, so
    /// nothing changed and there is nothing to write.
    pub attempted: usize,
    /// Recipients tried and not reached at any known address.
    ///
    /// Presence needs this (SPEC §5.5): a failed delivery is better evidence
    /// that a peer has gone than any hello is that it is still there, and it
    /// is why there is no ping.
    pub failed: Vec<NodeId>,
}

impl Pass {
    /// Nothing left to deliver.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.remaining == 0
    }
}

/// How one address failed, coarsely enough to compare two of the same peer's.
///
/// The words are what goes in the log, so they are the ones somebody reading it
/// has to make sense of unprompted: few, plain, and about the peer rather than
/// about the code that reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// Nothing answered: a closed laptop, or an address that has moved on.
    Unreachable,
    /// Something answered and it was not the peer we pinned (SPEC §6.1).
    Stranger,
    /// The peer answered and would not take the message. Somebody has to act.
    Refused,
    /// The exchange broke after connecting, or the peer said it had a problem.
    Broken,
}

impl Failure {
    fn of(error: &ClientError) -> Self {
        match error {
            ClientError::Connect { .. } | ClientError::Timeout { .. } => Self::Unreachable,
            ClientError::Handshake { .. } => Self::Stranger,
            // A 4xx will not pass on its own — `403 not_paired` stays a 403
            // until a human acts — and a 5xx is the peer's own trouble.
            ClientError::Status { status, .. } => {
                if *status >= 500 {
                    Self::Broken
                } else {
                    Self::Refused
                }
            }
            // A local TLS configuration failure is ours, not the peer's.
            ClientError::Tls(_) | ClientError::Http { .. } | ClientError::Decode { .. } => {
                Self::Broken
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::Stranger => "stranger",
            Self::Refused => "refused",
            Self::Broken => "broken",
        }
    }
}

/// What one peer's turn in a pass did, as the log has to describe it (#32).
///
/// A delivery that leaves no trace is a delivery nobody can reason about: the
/// incident this exists for was two minutes of polling the API and then the
/// wrong conclusion — "stuck" — about a message that was merely backing off.
struct Attempt<'a> {
    peer: NodeId,
    /// Every address tried that failed, in the order they were tried.
    failures: Vec<(String, Failure)>,
    /// The address that took the message, if one did.
    delivered_to: Option<&'a str>,
    /// Which attempt this was for this peer, counting from one.
    number: u32,
    /// The nominal wait before the next one — jitter moves the real one by up
    /// to a quarter either way. It is the field the incident turned on, so it
    /// is on every line that has a next attempt to describe, and on no other:
    /// a delivered message has no next attempt and this is not read.
    retry_in: Duration,
}

impl Attempt<'_> {
    /// The addresses tried and how each one failed, as one field.
    fn tried(&self) -> String {
        self.failures
            .iter()
            .map(|(addr, failure)| format!("{addr} {}", failure.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether this peer's addresses disagreed about what is wrong.
    ///
    /// One address failing the handshake while the others are merely unreachable
    /// is the symptom of #29 — something on the network is answering on an
    /// address we hold for this peer — and a line that names only the peer hides
    /// it. A peer that is off fails the same way everywhere and says nothing
    /// here, which is what keeps the warning worth reading.
    ///
    /// A success beside an ordinary failure is not disagreement either: a laptop
    /// with a stale Wi-Fi address and a working Tailscale one is the normal
    /// case. A success beside a *stranger* or a *refusal* is, because then two
    /// machines answered the same question differently.
    fn diverged(&self) -> bool {
        let mut kinds = self.failures.iter().map(|(_, failure)| *failure);
        let Some(first) = kinds.next() else {
            return false;
        };
        if kinds.any(|failure| failure != first) {
            return true;
        }
        self.delivered_to.is_some() && matches!(first, Failure::Stranger | Failure::Refused)
    }

    /// Say what happened, at a level the default filter passes.
    ///
    /// `info` for progress, because the log is the only place a human can watch
    /// the outbox from, and `warn` for a refusal, because that is a condition
    /// somebody has to resolve rather than one that passes on its own. Ids are
    /// short and addresses are addresses: nothing here is a secret.
    fn log(&self, why: &str) {
        let peer = self.peer.short();
        let retry_in = self.retry_in;
        let attempt = self.number;

        if let Some(addr) = self.delivered_to {
            tracing::info!(%peer, %addr, attempt, "delivered");
        } else if self.failures.is_empty() {
            tracing::info!(%peer, attempt, ?retry_in, "no address known for this peer yet");
        } else if self.failures.iter().any(|(_, f)| *f == Failure::Refused) {
            let tried = self.tried();
            tracing::warn!(
                %peer,
                %tried,
                %why,
                attempt,
                ?retry_in,
                "the peer refused the message; somebody has to resolve this"
            );
        } else {
            let tried = self.tried();
            tracing::info!(%peer, %tried, %why, attempt, ?retry_in, "not delivered yet");
        }

        if self.diverged() {
            tracing::warn!(
                %peer,
                tried = %self.tried(),
                "one address behaved differently from this peer's others; it may not be this peer"
            );
        }
    }
}

/// How many attempts this recipient already has recorded against it.
fn attempts_so_far(outbound: &Outbound, node: NodeId) -> u32 {
    outbound
        .recipients
        .iter()
        .find(|state| state.node == node)
        .map_or(0, |state| state.attempts)
}

/// Try every outstanding recipient once, updating `outbound` in place.
///
/// `addresses` returns where a peer might be, best guess first; each is tried
/// until one accepts. A recipient with no known address is recorded as
/// attempted, not dropped — the address may arrive from mDNS in a minute, and
/// SPEC §8 says delivery does not give up.
///
/// The caller is responsible for persisting `outbound` afterwards. This
/// function does no I/O beyond the transport so that it stays testable.
pub async fn attempt_all<T, A>(
    outbound: &mut Outbound,
    addresses: A,
    transport: &T,
    now: DateTime<Utc>,
) -> Pass
where
    T: Transport,
    A: Fn(NodeId) -> Vec<String>,
{
    let mut pass = Pass::default();

    let outstanding: Vec<NodeId> = outbound
        .outstanding()
        .filter(|state| is_due(state, now, jitter()))
        .map(|r| r.node)
        .collect();
    pass.attempted = outstanding.len();

    for node in outstanding {
        let mut failures = Vec::new();
        let mut last_error = None;
        let mut reached = None;

        for addr in addresses(node) {
            match transport.deliver(node, &addr, &outbound.message).await {
                Ok(()) => {
                    reached = Some(addr);
                    break;
                }
                Err(error) => {
                    // The whole error text, for the reader who has the summary
                    // line and wants what rustls or the socket actually said.
                    tracing::debug!(peer = %node.short(), %addr, %error, "an address failed");
                    failures.push((addr, Failure::of(&error)));
                    last_error = Some(error);
                }
            }
        }

        let why = if let Some(error) = last_error {
            error.to_string()
        } else if reached.is_some() {
            String::new()
        } else {
            // Not "no address could be tried": mDNS may supply one a minute
            // from now, and SPEC §8 says delivery does not give up.
            "no known address for this peer".to_owned()
        };

        if let Some(addr) = reached {
            // The failures recorded so far plus this, which worked.
            let number = attempts_so_far(outbound, node).saturating_add(1);
            outbound.mark_delivered(node, now);
            Attempt {
                peer: node,
                failures,
                delivered_to: Some(&addr),
                number,
                retry_in: Duration::ZERO,
            }
            .log(&why);
            pass.delivered.push((node, addr));
        } else {
            outbound.mark_attempted(node, now, &why);
            pass.failed.push(node);
            // After the mark, so the count and the interval are the ones the
            // next pass will use rather than the ones the last one did.
            let number = attempts_so_far(outbound, node);
            Attempt {
                peer: node,
                failures,
                delivered_to: None,
                number,
                retry_in: backoff(number, 0.5),
            }
            .log(&why);
        }
    }

    pass.remaining = outbound.outstanding().count();
    pass
}

/// Move a finished outbox entry to `sent/`, or save its progress.
///
/// SPEC §8: a message leaves `out/` only when every recipient has it. Until
/// then the updated attempt counts are written back so a restart resumes where
/// it left off rather than hammering a peer that is already backed off.
///
/// # Errors
/// Any [`hivemind_core::store::StoreError`] from writing or renaming.
pub fn persist(
    store: &MailStore,
    outbound: &Outbound,
) -> Result<(), hivemind_core::store::StoreError> {
    if outbound.is_complete() {
        // The envelope travels with it, so which recipient took the message —
        // and which has read it — outlives the queue (#31).
        store.promote_to_sent(outbound)
    } else {
        store.put_outbound(Mailbox::Out, outbound)
    }
}

/// What the delivery worker needs from the rest of the daemon.
///
/// A seam rather than a direct dependency on the mail service, which lives in
/// a crate above this one. It is also what lets the loop be tested against an
/// outbox held in memory, with no sockets and no clock.
pub trait Outbox: Send + Sync {
    /// Entries still awaiting delivery, oldest first.
    fn pending(&self) -> Vec<Outbound>;

    /// Where `node` might be reached, best guess first.
    fn addresses(&self, node: NodeId) -> Vec<String>;

    /// Save progress after a pass, and finish the entry if it is complete.
    ///
    /// Errors are the implementation's to log: a disk that will not take a
    /// write is not something the loop can do anything about, and stopping
    /// would strand every other message.
    fn store(&self, outbound: &Outbound);

    /// One recipient took the message at this address.
    ///
    /// The address book records it so the next message tries it first.
    fn reached(&self, node: NodeId, addr: &str, at: DateTime<Utc>);

    /// One recipient could not be reached at any address it is known at.
    ///
    /// Presence (SPEC §5.5) marks it offline at once: this is stronger and
    /// more recent evidence than the last hello, and it is what stands in for
    /// the ping there deliberately is not.
    ///
    /// Defaulted, because an outbox held in memory for a backoff test has no
    /// opinion about who is online.
    fn unreachable(&self, node: NodeId) {
        let _ = node;
    }
}

/// How often the loop looks for work.
///
/// Shorter than [`MIN_BACKOFF`] so a message that becomes due is picked up
/// promptly rather than up to a full backoff late.
pub const TICK: Duration = Duration::from_secs(1);

/// Deliver everything in the outbox, for as long as `shutdown` has not fired.
///
/// Never gives up on a recipient (SPEC §8) — a laptop that comes to the office
/// on Monday receives Friday's mail. Between passes it sleeps [`TICK`]; which
/// recipients an individual pass actually tries is decided by [`is_due`].
///
/// `clock` is a parameter rather than a call to `Utc::now`, because backoff is
/// the whole behaviour here and a test that had to wait real minutes to see it
/// would not be run. Production passes `Utc::now`.
pub async fn run<O, T, C, F>(outbox: &O, transport: &T, clock: C, shutdown: F)
where
    O: Outbox,
    T: Transport,
    C: Fn() -> DateTime<Utc>,
    F: std::future::Future<Output = ()> + Send,
{
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        for mut outbound in outbox.pending() {
            let now = clock();
            let pass =
                attempt_all(&mut outbound, |node| outbox.addresses(node), transport, now).await;

            for (node, addr) in &pass.delivered {
                outbox.reached(*node, addr, now);
            }
            for node in &pass.failed {
                outbox.unreachable(*node);
            }
            // Nothing tried means nothing changed: every outstanding
            // recipient is still inside its backoff.
            if pass.attempted > 0 {
                outbox.store(&outbound);
            }
        }

        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

#[cfg(test)]
mod pass_tests {
    use super::*;
    use hivemind_core::crypto::SigningKey;
    use hivemind_core::message::{Kind, Recipient, SenderKind};
    use hivemind_core::store::RecipientState;
    use std::collections::HashMap;
    use std::sync::Mutex;

    pub(super) fn node(seed: u8) -> NodeId {
        NodeId::from_certificate_der(&[seed; 16])
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    pub(super) fn outbound_to(recipients: &[NodeId]) -> Outbound {
        let from = node(0);
        let id = ulid::Ulid::generate();
        let mut message = Message {
            id,
            thread_id: id,
            in_reply_to: None,
            from,
            to: recipients.iter().copied().map(Recipient::Node).collect(),
            subject: "outbound".to_owned(),
            body: "body".to_owned(),
            kind: Kind::Message,
            sender_kind: SenderKind::Human,
            attachments: Vec::new(),
            sent_at: at(0),
            received_at: None,
            signature: hivemind_core::crypto::Signature::from_bytes([0u8; 64]),
        };
        message
            .sign(&SigningKey::from_bytes(&[9u8; 32]))
            .expect("sign");

        Outbound {
            message,
            recipients: recipients
                .iter()
                .copied()
                .map(RecipientState::pending)
                .collect(),
        }
    }

    /// A transport told in advance what each address should do.
    pub(super) struct Scripted {
        accepting: Vec<String>,
        seen: Mutex<Vec<(String, ulid::Ulid)>>,
    }

    impl Scripted {
        pub(super) fn accepting(addrs: &[&str]) -> Self {
            Self {
                accepting: addrs.iter().map(|a| (*a).to_owned()).collect(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn attempts(&self) -> Vec<(String, ulid::Ulid)> {
            self.seen.lock().expect("lock").clone()
        }
    }

    impl Transport for Scripted {
        async fn deliver(
            &self,
            _node: NodeId,
            addr: &str,
            message: &Message,
        ) -> Result<(), ClientError> {
            self.seen
                .lock()
                .expect("lock")
                .push((addr.to_owned(), message.id));
            if self.accepting.iter().any(|a| a == addr) {
                Ok(())
            } else {
                Err(ClientError::Connect {
                    addr: addr.to_owned(),
                    source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
                })
            }
        }
    }

    fn book(entries: &[(NodeId, &[&str])]) -> impl Fn(NodeId) -> Vec<String> + use<> {
        let map: HashMap<NodeId, Vec<String>> = entries
            .iter()
            .map(|(n, addrs)| (*n, addrs.iter().map(|a| (*a).to_owned()).collect()))
            .collect();
        move |node| map.get(&node).cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn every_recipient_that_accepts_is_marked_delivered() {
        let (a, b) = (node(1), node(2));
        let mut outbound = outbound_to(&[a, b]);
        let transport = Scripted::accepting(&["10.0.0.1:8400", "10.0.0.2:8400"]);

        let pass = attempt_all(
            &mut outbound,
            book(&[(a, &["10.0.0.1:8400"]), (b, &["10.0.0.2:8400"])]),
            &transport,
            at(100),
        )
        .await;

        assert!(pass.is_complete(), "both recipients took it");
        assert_eq!(pass.delivered.len(), 2);
        assert!(outbound.is_complete());
    }

    #[tokio::test]
    async fn one_unreachable_recipient_does_not_hold_up_the_others() {
        let (reachable, offline) = (node(1), node(2));
        let mut outbound = outbound_to(&[reachable, offline]);
        let transport = Scripted::accepting(&["10.0.0.1:8400"]);

        let pass = attempt_all(
            &mut outbound,
            book(&[
                (reachable, &["10.0.0.1:8400"]),
                (offline, &["10.0.0.2:8400"]),
            ]),
            &transport,
            at(100),
        )
        .await;

        assert_eq!(pass.remaining, 1, "the offline peer is still owed a copy");
        assert_eq!(
            pass.delivered,
            vec![(reachable, "10.0.0.1:8400".to_owned())]
        );
        assert!(!outbound.is_complete());
    }

    #[tokio::test]
    async fn a_recipient_that_already_took_it_is_not_tried_again() {
        // Idempotency is the recipient's job (SPEC §8), but there is no reason
        // to make it do it.
        let (done, pending) = (node(1), node(2));
        let mut outbound = outbound_to(&[done, pending]);
        outbound.mark_delivered(done, at(50));
        let transport = Scripted::accepting(&["10.0.0.2:8400"]);

        attempt_all(
            &mut outbound,
            book(&[(done, &["10.0.0.1:8400"]), (pending, &["10.0.0.2:8400"])]),
            &transport,
            at(100),
        )
        .await;

        let tried: Vec<String> = transport.attempts().into_iter().map(|(a, _)| a).collect();
        assert_eq!(tried, vec!["10.0.0.2:8400".to_owned()]);
    }

    #[tokio::test]
    async fn every_known_address_is_tried_before_the_peer_is_given_up_on() {
        // A laptop has a Wi-Fi address, an Ethernet address and a Tailscale
        // address, and which one works changes through the day.
        let peer = node(1);
        let mut outbound = outbound_to(&[peer]);
        let transport = Scripted::accepting(&["100.64.0.1:8400"]);

        let pass = attempt_all(
            &mut outbound,
            book(&[(
                peer,
                &["192.168.1.5:8400", "10.0.0.5:8400", "100.64.0.1:8400"],
            )]),
            &transport,
            at(100),
        )
        .await;

        assert!(pass.is_complete());
        assert_eq!(
            pass.delivered,
            vec![(peer, "100.64.0.1:8400".to_owned())],
            "the caller needs to know which address worked"
        );
        assert_eq!(
            transport.attempts().len(),
            3,
            "it should have tried in order"
        );
    }

    #[tokio::test]
    async fn a_peer_with_no_known_address_is_recorded_not_dropped() {
        // mDNS may supply one a minute from now. SPEC §8: delivery is forever.
        let peer = node(1);
        let mut outbound = outbound_to(&[peer]);
        let transport = Scripted::accepting(&[]);

        let pass = attempt_all(&mut outbound, book(&[]), &transport, at(100)).await;

        assert_eq!(pass.remaining, 1);
        let state = &outbound.recipients[0];
        assert_eq!(state.attempts, 1, "the attempt counts, so backoff advances");
        assert!(
            state
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("no known address")),
            "a human should be able to see why: {:?}",
            state.last_error
        );
        assert!(transport.attempts().is_empty());
    }

    #[tokio::test]
    async fn repeated_failures_accumulate_so_backoff_grows() {
        let peer = node(1);
        let mut outbound = outbound_to(&[peer]);
        let transport = Scripted::accepting(&[]);
        let addresses = book(&[(peer, &["10.0.0.9:8400"])]);

        for pass in 1..=3 {
            attempt_all(&mut outbound, &addresses, &transport, at(100 * pass)).await;
        }

        assert_eq!(outbound.recipients[0].attempts, 3);
        assert_eq!(outbound.recipients[0].last_attempt, Some(at(300)));
        assert!(backoff(3, 0.5) > backoff(1, 0.5));
    }

    #[test]
    fn a_finished_message_moves_to_sent_and_an_unfinished_one_stays() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = MailStore::open(dir.path().join("mail")).expect("store");
        let (a, b) = (node(1), node(2));

        let mut outbound = outbound_to(&[a, b]);
        let id = outbound.message.id;
        persist(&store, &outbound).expect("persist");
        assert!(
            store.get_outbound(Mailbox::Out, id).is_ok(),
            "still owed to somebody"
        );

        outbound.mark_delivered(a, at(1));
        persist(&store, &outbound).expect("persist");
        assert!(
            store.get_outbound(Mailbox::Out, id).is_ok(),
            "one of two is not everybody"
        );

        outbound.mark_delivered(b, at(2));
        persist(&store, &outbound).expect("persist");
        assert!(
            store.get_outbound(Mailbox::Out, id).is_err(),
            "out/ should be empty once everyone has it"
        );
        assert!(
            store.get(Mailbox::Sent, id).is_ok(),
            "it should be in sent/"
        );
    }
    #[tokio::test]
    async fn a_recipient_no_address_worked_for_is_reported_as_failed() {
        // Presence marks it offline on this (SPEC §5.5), so "tried and did not
        // answer" has to be distinguishable from "not tried".
        let (reachable, gone) = (node(1), node(2));
        let mut outbound = outbound_to(&[reachable, gone]);
        let transport = Scripted::accepting(&["10.0.0.1:8400"]);

        let pass = attempt_all(
            &mut outbound,
            book(&[(reachable, &["10.0.0.1:8400"]), (gone, &["10.0.0.2:8400"])]),
            &transport,
            at(100),
        )
        .await;

        assert_eq!(pass.failed, vec![gone]);
        assert!(
            !pass.failed.contains(&reachable),
            "the one that took it is not a failure"
        );
    }

    #[tokio::test]
    async fn a_recipient_with_no_address_at_all_counts_as_failed_too() {
        // It is the same fact from the peer's point of view — nothing got
        // there — and the alternative is a peer that stays "online" forever
        // because we never had anywhere to try.
        let nowhere = node(3);
        let mut outbound = outbound_to(&[nowhere]);
        let transport = Scripted::accepting(&[]);

        let pass = attempt_all(&mut outbound, book(&[]), &transport, at(100)).await;

        assert_eq!(pass.failed, vec![nowhere]);
    }

    #[tokio::test]
    async fn a_recipient_still_inside_its_backoff_is_not_reported_as_failed() {
        // It was not tried, so it is no evidence about anything. Reporting it
        // would mark a peer offline every second of a five-minute backoff.
        let backed_off = node(1);
        let mut outbound = outbound_to(&[backed_off]);
        outbound.recipients[0].attempts = 4;
        outbound.recipients[0].last_attempt = Some(at(1_000));

        let transport = Scripted::accepting(&[]);
        let pass = attempt_all(
            &mut outbound,
            book(&[(backed_off, &["10.0.0.1:8400"])]),
            &transport,
            at(1_001),
        )
        .await;

        assert_eq!(pass.attempted, 0);
        assert!(pass.failed.is_empty());
    }
}

#[cfg(test)]
mod worker_tests {
    use super::pass_tests::{Scripted, node, outbound_to};
    use super::*;
    use hivemind_core::store::RecipientState;
    use std::sync::Mutex;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    /// A clock that advances with tokio's, so `start_paused` fast-forwards the
    /// backoff as well as the sleeps.
    fn simulated_clock() -> impl Fn() -> DateTime<Utc> {
        let start = tokio::time::Instant::now();
        move || at(0) + start.elapsed()
    }

    #[test]
    fn a_recipient_that_has_never_been_tried_is_due_immediately() {
        // Sending should not wait two seconds for no reason.
        assert!(is_due(&RecipientState::pending(node(1)), at(0), 0.5));
    }

    #[test]
    fn a_recipient_inside_its_backoff_is_not_due() {
        let mut state = RecipientState::pending(node(1));
        state.attempts = 4;
        state.last_attempt = Some(at(1_000));

        // backoff(4) is 16s either side of jitter, so one second later is far
        // too soon and a minute later is comfortably past it.
        assert!(!is_due(&state, at(1_001), 0.5));
        assert!(is_due(&state, at(1_060), 0.5));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_make_a_recipient_due_forever() {
        // NTP correcting a sleeping laptop is entirely normal.
        let mut state = RecipientState::pending(node(1));
        state.attempts = 1;
        state.last_attempt = Some(at(1_000));
        assert!(
            !is_due(&state, at(900), 0.5),
            "a negative elapsed time is not a long one"
        );
    }

    #[tokio::test]
    async fn one_recipient_backing_off_does_not_hold_back_another() {
        let (slow, quick) = (node(1), node(2));
        let mut outbound = outbound_to(&[slow, quick]);

        // `slow` has just failed its fourth attempt; `quick` is untouched.
        outbound.recipients[0].attempts = 4;
        outbound.recipients[0].last_attempt = Some(at(1_000));

        let transport = Scripted::accepting(&["10.0.0.2:8400"]);
        let addresses = |n: NodeId| {
            if n == slow {
                vec!["10.0.0.1:8400".to_owned()]
            } else {
                vec!["10.0.0.2:8400".to_owned()]
            }
        };

        let pass = attempt_all(&mut outbound, addresses, &transport, at(1_001)).await;

        assert_eq!(pass.attempted, 1, "only the one that is due");
        assert_eq!(pass.delivered, vec![(quick, "10.0.0.2:8400".to_owned())]);
        assert_eq!(
            outbound.recipients[0].attempts, 4,
            "the backed-off recipient must not have its counter advanced"
        );
    }

    /// An outbox in memory, so the loop can be driven without a disk.
    #[derive(Default)]
    struct Fake {
        entries: Mutex<Vec<Outbound>>,
        finished: Mutex<Vec<ulid::Ulid>>,
        reached: Mutex<Vec<(NodeId, String)>>,
        unreachable: Mutex<Vec<NodeId>>,
    }

    impl Outbox for Fake {
        fn pending(&self) -> Vec<Outbound> {
            self.entries.lock().expect("lock").clone()
        }

        fn addresses(&self, _node: NodeId) -> Vec<String> {
            vec!["10.0.0.1:8400".to_owned()]
        }

        fn store(&self, outbound: &Outbound) {
            let mut entries = self.entries.lock().expect("lock");
            entries.retain(|e| e.message.id != outbound.message.id);
            if outbound.is_complete() {
                self.finished
                    .lock()
                    .expect("lock")
                    .push(outbound.message.id);
            } else {
                entries.push(outbound.clone());
            }
        }

        fn reached(&self, node: NodeId, addr: &str, _at: DateTime<Utc>) {
            self.reached
                .lock()
                .expect("lock")
                .push((node, addr.to_owned()));
        }

        fn unreachable(&self, node: NodeId) {
            self.unreachable.lock().expect("lock").push(node);
        }
    }

    /// A transport that refuses a given number of times and then accepts.
    struct FailsThenWorks {
        remaining: Mutex<u32>,
    }

    impl Transport for FailsThenWorks {
        async fn deliver(
            &self,
            _node: NodeId,
            addr: &str,
            _message: &Message,
        ) -> Result<(), ClientError> {
            let mut remaining = self.remaining.lock().expect("lock");
            if *remaining == 0 {
                return Ok(());
            }
            *remaining -= 1;
            Err(ClientError::Connect {
                addr: addr.to_owned(),
                source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_comes_back_on_monday_gets_fridays_mail() {
        // SPEC §8, and the reason delivery never gives up. Time is paused, so
        // this runs in microseconds rather than the minutes it models.
        let peer = node(1);
        let outbox = Fake {
            entries: Mutex::new(vec![outbound_to(&[peer])]),
            ..Fake::default()
        };
        let transport = FailsThenWorks {
            remaining: Mutex::new(6),
        };

        let (stop, stopped) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            // The future has to own the values; the loop borrows them.
            run(&outbox, &transport, simulated_clock(), async {
                let _ = stopped.await;
            })
            .await;
            outbox
        });

        // Six failures back off to roughly two minutes in total. An hour of
        // simulated time is comfortably past that, and costs nothing.
        tokio::time::sleep(Duration::from_hours(1)).await;
        let _ = stop.send(());
        let outbox = worker.await.expect("the worker should not panic");

        assert_eq!(
            outbox.finished.lock().expect("lock").len(),
            1,
            "the message should have been delivered and left the outbox"
        );
        assert!(outbox.entries.lock().expect("lock").is_empty());
        assert_eq!(
            outbox.reached.lock().expect("lock").as_slice(),
            [(peer, "10.0.0.1:8400".to_owned())],
            "the address that worked should have been recorded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_outbox_costs_nothing_and_stops_when_asked() {
        let outbox = Fake::default();
        let transport = Scripted::accepting(&[]);
        let (stop, stopped) = tokio::sync::oneshot::channel();

        let worker = tokio::spawn(async move {
            run(&outbox, &transport, simulated_clock(), async {
                let _ = stopped.await;
            })
            .await;
        });

        tokio::time::sleep(Duration::from_secs(10)).await;
        let _ = stop.send(());

        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("the worker should stop promptly")
            .expect("it should not panic");
    }
}
