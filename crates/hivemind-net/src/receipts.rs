//! Carrying read receipts back to the nodes that are owed them (ADR 0016).
//!
//! The delivery worker's smaller sibling, and deliberately separate: what it
//! carries is not mail, it cannot be redelivered as a message, and one loop
//! doing both would have to say which of the two every failure belonged to.
//! What the two share is the backoff, which lives in [`crate::delivery`] and is
//! used from here rather than written again.
//!
//! Receipts are **batched per peer**, because reads come in bursts — opening a
//! conversation marks every unread message in it — and a handshake per message
//! for a fact that fits in forty bytes is the wrong trade. A batch is attempted
//! when the oldest receipt in it is due, and a batch that fails counts one
//! attempt against every receipt in it, so the whole peer backs off together.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hivemind_core::peer::NodeId;
use hivemind_core::receipts::{OwedReceipt, ReadNote};

use crate::client::ClientError;
use crate::delivery::{TICK, backoff, due_after, jitter};

/// How a batch of receipts reaches a peer.
///
/// A trait for the same reason [`crate::delivery::Transport`] is one: the
/// interesting cases are a peer that is off and a peer that refuses, and
/// neither is quick to arrange with real sockets.
pub trait Transport {
    /// Tell `node`, at `addr`, that these messages of its have been read.
    fn confirm_read(
        &self,
        node: NodeId,
        addr: &str,
        read: &[ReadNote],
    ) -> impl std::future::Future<Output = Result<(), ClientError>> + Send;
}

/// What the courier needs from the rest of the daemon.
pub trait Courier: Send + Sync {
    /// Every receipt still owed, oldest message first.
    fn owed(&self) -> Vec<OwedReceipt>;

    /// Where `node` might be reached, best guess first.
    fn addresses(&self, node: NodeId) -> Vec<String>;

    /// These receipts have been taken and can be forgotten.
    fn settled(&self, receipts: &[OwedReceipt]);

    /// These receipts were not taken; their counters have been updated.
    ///
    /// Written back rather than held in memory so a restart resumes the
    /// backoff rather than hammering a peer that is already waiting.
    fn failed(&self, receipts: &[OwedReceipt]);
}

/// What one pass over the queue achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// How many receipts were accepted.
    pub settled: usize,
    /// How many were attempted and not accepted.
    pub failed: usize,
    /// How many peers were tried. Zero means every batch is still backing off.
    pub peers: usize,
}

/// Group what is owed by the peer it is owed to, oldest first within each.
fn by_peer(owed: Vec<OwedReceipt>) -> BTreeMap<NodeId, Vec<OwedReceipt>> {
    let mut batches: BTreeMap<NodeId, Vec<OwedReceipt>> = BTreeMap::new();
    for receipt in owed {
        batches.entry(receipt.to).or_default().push(receipt);
    }
    batches
}

/// Whether a peer's batch is out of its backoff.
///
/// The **oldest** receipt decides, not every one of them: a receipt queued a
/// second ago must not hold back one that has been waiting five minutes, and a
/// batch is one request either way.
fn batch_is_due(batch: &[OwedReceipt], now: DateTime<Utc>, jitter: f64) -> bool {
    batch
        .iter()
        .any(|receipt| due_after(receipt.attempts, receipt.last_attempt, now, jitter))
}

/// Try every peer that is owed receipts and out of its backoff, once.
///
/// Does no I/O beyond the transport, so the retry behaviour is testable.
pub async fn attempt_all<C, T>(courier: &C, transport: &T, now: DateTime<Utc>) -> Pass
where
    C: Courier,
    T: Transport,
{
    let mut pass = Pass::default();

    for (peer, mut batch) in by_peer(courier.owed()) {
        if !batch_is_due(&batch, now, jitter()) {
            continue;
        }
        pass.peers += 1;

        let notes: Vec<ReadNote> = batch.iter().map(OwedReceipt::note).collect();
        let mut taken = false;
        let mut why = "no known address for this peer".to_owned();

        for addr in courier.addresses(peer) {
            match transport.confirm_read(peer, &addr, &notes).await {
                Ok(()) => {
                    taken = true;
                    break;
                }
                Err(error) => {
                    tracing::debug!(peer = %peer.short(), %addr, %error, "a receipt address failed");
                    why = error.to_string();
                }
            }
        }

        if taken {
            pass.settled += batch.len();
            tracing::info!(
                peer = %peer.short(),
                receipts = batch.len(),
                "read receipts accepted"
            );
            courier.settled(&batch);
        } else {
            for receipt in &mut batch {
                receipt.attempted(now, &why);
            }
            pass.failed += batch.len();
            // At `info`, and once per peer rather than once per receipt: the
            // same reason delivery logs every attempt (#32), at the same
            // volume as one delivery rather than at the volume of a mailbox.
            let retry_in = batch.first().map_or(crate::delivery::MIN_BACKOFF, |first| {
                backoff(first.attempts, 0.5)
            });
            tracing::info!(
                peer = %peer.short(),
                receipts = batch.len(),
                %why,
                ?retry_in,
                "read receipts not accepted yet"
            );
            courier.failed(&batch);
        }
    }

    pass
}

/// Carry receipts until `shutdown` fires.
///
/// Never gives up, for the same reason delivery never gives up (SPEC §8): the
/// node owed a receipt may be off for days, and a receipt attempted once and
/// dropped is a fact its sender never learns.
pub async fn run<C, T, K, F>(courier: &C, transport: &T, clock: K, shutdown: F)
where
    C: Courier,
    T: Transport,
    K: Fn() -> DateTime<Utc>,
    F: std::future::Future<Output = ()> + Send,
{
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        attempt_all(courier, transport, clock()).await;

        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use ulid::Ulid;

    fn node(seed: u8) -> NodeId {
        NodeId::from_certificate_der(&[seed; 16])
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    fn owed(message: u64, to: NodeId) -> OwedReceipt {
        OwedReceipt::new(Ulid::from_parts(message, 0), to, at(1_000))
    }

    /// A queue held in memory, recording what the courier did to it.
    #[derive(Default)]
    struct Book {
        owed: Mutex<Vec<OwedReceipt>>,
        addresses: BTreeMap<NodeId, Vec<String>>,
        settled: Mutex<Vec<Ulid>>,
    }

    impl Courier for Book {
        fn owed(&self) -> Vec<OwedReceipt> {
            self.owed.lock().expect("lock").clone()
        }

        fn addresses(&self, node: NodeId) -> Vec<String> {
            self.addresses.get(&node).cloned().unwrap_or_default()
        }

        fn settled(&self, receipts: &[OwedReceipt]) {
            let taken: Vec<Ulid> = receipts.iter().map(|receipt| receipt.message).collect();
            self.settled.lock().expect("lock").extend_from_slice(&taken);
            self.owed
                .lock()
                .expect("lock")
                .retain(|receipt| !taken.contains(&receipt.message));
        }

        fn failed(&self, receipts: &[OwedReceipt]) {
            let mut owed = self.owed.lock().expect("lock");
            for updated in receipts {
                if let Some(held) = owed.iter_mut().find(|held| held.message == updated.message) {
                    *held = updated.clone();
                }
            }
        }
    }

    /// A transport that takes receipts at some addresses and not others.
    struct Scripted {
        accepting: Vec<String>,
        seen: Mutex<Vec<(NodeId, Vec<Ulid>)>>,
    }

    impl Scripted {
        fn accepting(addrs: &[&str]) -> Self {
            Self {
                accepting: addrs.iter().map(|a| (*a).to_owned()).collect(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn batches(&self) -> Vec<(NodeId, Vec<Ulid>)> {
            self.seen.lock().expect("lock").clone()
        }
    }

    impl Transport for Scripted {
        async fn confirm_read(
            &self,
            node: NodeId,
            addr: &str,
            read: &[ReadNote],
        ) -> Result<(), ClientError> {
            if self.accepting.iter().any(|a| a == addr) {
                self.seen
                    .lock()
                    .expect("lock")
                    .push((node, read.iter().map(|note| note.id).collect()));
                return Ok(());
            }
            Err(ClientError::Connect {
                addr: addr.to_owned(),
                source: std::io::Error::other("nothing there"),
            })
        }
    }

    #[tokio::test]
    async fn every_receipt_for_one_peer_travels_in_one_request() {
        // Opening a conversation marks every unread message in it, and a
        // handshake per message for a forty-byte fact is the wrong trade.
        let peer = node(1);
        let book = Book {
            owed: Mutex::new(vec![owed(100, peer), owed(200, peer), owed(300, peer)]),
            addresses: [(peer, vec!["10.0.0.2:8400".to_owned()])].into(),
            ..Book::default()
        };
        let transport = Scripted::accepting(&["10.0.0.2:8400"]);

        let pass = attempt_all(&book, &transport, at(2_000)).await;

        assert_eq!(pass.settled, 3);
        assert_eq!(pass.peers, 1);
        assert_eq!(transport.batches().len(), 1, "one request, not three");
        assert_eq!(transport.batches()[0].1.len(), 3);
        assert!(book.owed().is_empty(), "all of them are settled");
    }

    #[tokio::test]
    async fn each_peer_is_told_only_about_its_own_messages() {
        // Two peers, so "the right peer's receipts" is distinguishable from
        // "all of them" — which is the thing that must never be wrong here.
        let one = node(1);
        let two = node(2);
        let book = Book {
            owed: Mutex::new(vec![owed(100, one), owed(200, two), owed(300, one)]),
            addresses: [
                (one, vec!["10.0.0.1:8400".to_owned()]),
                (two, vec!["10.0.0.2:8400".to_owned()]),
            ]
            .into(),
            ..Book::default()
        };
        let transport = Scripted::accepting(&["10.0.0.1:8400", "10.0.0.2:8400"]);

        attempt_all(&book, &transport, at(2_000)).await;

        for (peer, messages) in transport.batches() {
            let expected: Vec<u64> = if peer == one {
                vec![100, 300]
            } else {
                vec![200]
            };
            let got: Vec<u64> = messages.iter().map(Ulid::timestamp_ms).collect();
            assert_eq!(got, expected, "{} got the wrong batch", peer.short());
        }
    }

    #[tokio::test]
    async fn a_peer_that_is_off_keeps_its_receipts_and_backs_off() {
        let peer = node(1);
        let book = Book {
            owed: Mutex::new(vec![owed(100, peer)]),
            addresses: [(peer, vec!["10.0.0.2:8400".to_owned()])].into(),
            ..Book::default()
        };
        let transport = Scripted::accepting(&[]);

        let pass = attempt_all(&book, &transport, at(2_000)).await;

        assert_eq!(pass.failed, 1);
        assert_eq!(pass.settled, 0);
        let held = book.owed();
        assert_eq!(held.len(), 1, "nothing is dropped");
        assert_eq!(held[0].attempts, 1);
        assert_eq!(held[0].last_attempt, Some(at(2_000)));
        assert!(held[0].last_error.is_some(), "it should say why");

        // And the next pass, a second later, leaves it alone.
        let pass = attempt_all(&book, &transport, at(2_001)).await;
        assert_eq!(pass.peers, 0, "still inside its backoff");
        assert_eq!(book.owed()[0].attempts, 1);
    }

    #[tokio::test]
    async fn a_peer_with_no_address_is_kept_rather_than_dropped() {
        // mDNS may supply one in a minute, and a receipt is owed until it is
        // taken (ADR 0016).
        let peer = node(1);
        let book = Book {
            owed: Mutex::new(vec![owed(100, peer)]),
            ..Book::default()
        };
        let transport = Scripted::accepting(&["10.0.0.2:8400"]);

        let pass = attempt_all(&book, &transport, at(2_000)).await;

        assert_eq!(pass.failed, 1);
        assert_eq!(book.owed().len(), 1);
        assert_eq!(
            book.owed()[0].last_error.as_deref(),
            Some("no known address for this peer")
        );
    }

    #[tokio::test]
    async fn a_second_address_is_tried_when_the_first_fails() {
        let peer = node(1);
        let book = Book {
            owed: Mutex::new(vec![owed(100, peer)]),
            addresses: [(
                peer,
                vec!["stale:8400".to_owned(), "10.0.0.2:8400".to_owned()],
            )]
            .into(),
            ..Book::default()
        };
        let transport = Scripted::accepting(&["10.0.0.2:8400"]);

        let pass = attempt_all(&book, &transport, at(2_000)).await;

        assert_eq!(pass.settled, 1);
        assert!(book.owed().is_empty());
    }

    #[tokio::test]
    async fn the_oldest_receipt_decides_whether_a_batch_is_tried() {
        // One queued a second ago must not hold back one that has been waiting
        // five minutes, and a batch is one request either way.
        let peer = node(1);
        let mut waiting = owed(100, peer);
        waiting.attempted(at(1_000), "unreachable");
        let book = Book {
            owed: Mutex::new(vec![waiting, owed(200, peer)]),
            addresses: [(peer, vec!["10.0.0.2:8400".to_owned()])].into(),
            ..Book::default()
        };
        let transport = Scripted::accepting(&["10.0.0.2:8400"]);

        // A second later, the attempted one is inside its two-second backoff
        // and the fresh one has never been tried, so the batch goes.
        let pass = attempt_all(&book, &transport, at(1_001)).await;

        assert_eq!(pass.settled, 2);
    }

    #[test]
    fn a_batch_with_nothing_due_is_not_tried() {
        let peer = node(1);
        let mut receipt = owed(100, peer);
        receipt.attempted(at(1_000), "unreachable");
        assert!(!batch_is_due(&[receipt.clone()], at(1_001), 0.5));
        assert!(batch_is_due(&[receipt], at(1_010), 0.5));
    }

    #[tokio::test]
    async fn the_loop_stops_when_it_is_told_to() {
        let book = Book::default();
        let transport = Scripted::accepting(&[]);
        let (stop, wait) = tokio::sync::oneshot::channel::<()>();
        drop(stop);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run(&book, &transport, || at(2_000), async {
                let _ = wait.await;
            }),
        )
        .await
        .expect("a courier told to stop must stop");
    }
}
