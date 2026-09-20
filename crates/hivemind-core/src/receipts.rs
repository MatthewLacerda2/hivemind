//! Read receipts this node owes other nodes (SPEC §4.3, §8, ADR 0016).
//!
//! A receipt is a small piece of mail carrying one fact: *I opened the message
//! you sent me, at this time*. It is queued on disk exactly as outgoing mail
//! is, one file per receipt under `~/.hivemind/receipts/`, and it stays there
//! until the node it is for accepts it. The premise of the whole application
//! is that the other machine may be off for days, and a receipt that is
//! attempted once and dropped would be a fact the sender never learns.
//!
//! **Not a fifth mailbox.** These files carry no message, and nothing lists,
//! reads, replies to or searches them; a directory under `mail/` would put
//! them in every walk of it for the sake of sharing a parent.
//!
//! **Nothing is queued unless `read_receipts` is on** (SPEC §8). A receipt is
//! information about a person rather than about a daemon, so the person whose
//! reading it describes is the one who decides, and the default is off.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::peer::NodeId;
use crate::store::{StoreError, write_atomic};

/// One message a reader has opened, as it travels back (`docs/protocol.md`).
///
/// `read_at` is the reader's clock, not ours, exactly as `sent_at` is the
/// sender's (SPEC §4.1). Nothing depends on it being right: it is shown beside
/// the recipient's name and never compared against anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadNote {
    /// The message that was read. The sender's own id for it.
    pub id: Ulid,
    /// When the reader opened it.
    pub read_at: DateTime<Utc>,
}

/// The body of `POST /peer/v1/receipts` (SPEC §7.2).
///
/// A batch, because reads come in bursts: opening a conversation marks every
/// unread message in it, and one request per message would be a handshake per
/// message for a fact that fits in forty bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReceipts {
    /// The messages this node has read, in whatever order they were queued.
    #[serde(default)]
    pub read: Vec<ReadNote>,
}

/// What this node answers a batch of receipts with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recorded {
    /// How many of them named a message this node had sent to the caller.
    ///
    /// Fewer than were offered is not an error: the sender may have deleted
    /// the message, and a receipt for one it no longer holds is a fact with
    /// nowhere to go rather than a failure to report.
    pub recorded: usize,
}

/// One receipt still owed to one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwedReceipt {
    /// The message that was read.
    pub message: Ulid,
    /// The node that sent it, and so the node owed the receipt.
    pub to: NodeId,
    /// When it was read here.
    pub read_at: DateTime<Utc>,
    /// How many times delivering this receipt has been attempted.
    #[serde(default)]
    pub attempts: u32,
    /// When the last attempt was made.
    #[serde(default)]
    pub last_attempt: Option<DateTime<Utc>>,
    /// What the last attempt said.
    #[serde(default)]
    pub last_error: Option<String>,
}

impl OwedReceipt {
    /// A receipt that has not been attempted yet.
    #[must_use]
    pub fn new(message: Ulid, to: NodeId, read_at: DateTime<Utc>) -> Self {
        Self {
            message,
            to,
            read_at,
            attempts: 0,
            last_attempt: None,
            last_error: None,
        }
    }

    /// What travels on the wire.
    #[must_use]
    pub fn note(&self) -> ReadNote {
        ReadNote {
            id: self.message,
            read_at: self.read_at,
        }
    }

    /// Record a failed attempt, so backoff can be computed from it.
    pub fn attempted(&mut self, at: DateTime<Utc>, why: &str) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_attempt = Some(at);
        self.last_error = Some(why.to_owned());
    }
}

/// Whether a removal counts as settled.
///
/// A judgement over an `io::Result` rather than a match inside `settle`, so
/// both branches are testable anywhere: provoking a real permission failure
/// needs a directory nobody can write to, which is not the same test on every
/// machine and is not a test at all as root. The same split `doctor`'s
/// optional-tool check was given, for the same reason.
fn settled(removed: std::io::Result<()>, message: Ulid) -> Result<(), StoreError> {
    match removed {
        Ok(()) => Ok(()),
        // Already gone is what the caller asked for.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Anything else is a receipt still on disk that we believe is settled,
        // and a queue that quietly disagrees with itself is worth a word.
        Err(source) => Err(StoreError::Io {
            context: format!("could not settle the receipt for {message}"),
            source,
        }),
    }
}

/// The queue of receipts this node owes.
#[derive(Debug, Clone)]
pub struct ReceiptBook {
    root: PathBuf,
}

impl ReceiptBook {
    /// Open the queue rooted at `receipts/`, creating it if needed.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the directory cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| StoreError::Io {
            context: format!("could not create {}", root.display()),
            source,
        })?;
        Ok(Self { root })
    }

    fn path_of(&self, message: Ulid) -> PathBuf {
        self.root.join(format!("{message}.json"))
    }

    /// Start owing a receipt, unless one for this message is already owed.
    ///
    /// Returns whether anything was written. The first read is the one that
    /// counts: a message cannot be opened for the first time twice, and a
    /// second call would restart a backoff the first one has accumulated.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the write fails.
    pub fn owe(&self, receipt: &OwedReceipt) -> Result<bool, StoreError> {
        if self.path_of(receipt.message).exists() {
            return Ok(false);
        }
        write_atomic(&self.path_of(receipt.message), receipt)?;
        Ok(true)
    }

    /// Write a receipt back after an attempt, keeping its counters.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the write fails.
    pub fn save(&self, receipt: &OwedReceipt) -> Result<(), StoreError> {
        write_atomic(&self.path_of(receipt.message), receipt)
    }

    /// Stop owing one, because the node it was for has taken it.
    ///
    /// Not an error when it is already gone: the caller wanted it settled and
    /// it is settled.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the file exists but cannot be deleted.
    pub fn settle(&self, message: Ulid) -> Result<(), StoreError> {
        settled(fs::remove_file(self.path_of(message)), message)
    }

    /// Every receipt still owed, oldest first.
    ///
    /// One that will not parse is skipped rather than fatal, as the outbox
    /// does: a single bad file must not stop the rest.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the directory cannot be read.
    pub fn owed(&self) -> Result<Vec<OwedReceipt>, StoreError> {
        let entries = fs::read_dir(&self.root).map_err(|source| StoreError::Io {
            context: format!("could not list {}", self.root.display()),
            source,
        })?;

        let mut ids: Vec<Ulid> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter_map(|name| {
                let name = name.to_str()?;
                Ulid::from_str(name.strip_suffix(".json")?).ok()
            })
            .collect();
        // ULIDs sort in time order, so this is oldest message first.
        ids.sort_unstable();

        Ok(ids
            .into_iter()
            .filter_map(|id| {
                let bytes = fs::read(self.path_of(id)).ok()?;
                serde_json::from_slice(&bytes).ok()
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> (tempfile::TempDir, ReceiptBook) {
        let dir = tempfile::tempdir().expect("temp dir");
        let book = ReceiptBook::open(dir.path().join("receipts")).expect("opens");
        (dir, book)
    }

    fn node(seed: u8) -> NodeId {
        NodeId::from_certificate_der(&[seed; 8])
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    #[test]
    fn a_receipt_written_reads_back_identical() {
        let (_dir, book) = book();
        let owed = OwedReceipt::new(Ulid::from_parts(100, 1), node(1), at(1_000));
        assert!(book.owe(&owed).expect("owe"));
        assert_eq!(book.owed().expect("owed"), vec![owed]);
    }

    #[test]
    fn the_first_read_is_the_one_that_counts() {
        // A second `owe` would restart a backoff the first has accumulated,
        // and a message cannot be opened for the first time twice.
        let (_dir, book) = book();
        let id = Ulid::from_parts(100, 1);
        let mut owed = OwedReceipt::new(id, node(1), at(1_000));
        assert!(book.owe(&owed).expect("owe"));
        owed.attempted(at(2_000), "unreachable");
        book.save(&owed).expect("save");

        let again = OwedReceipt::new(id, node(1), at(9_000));
        assert!(!book.owe(&again).expect("owe"), "nothing to write");

        let held = book.owed().expect("owed");
        assert_eq!(held[0].read_at, at(1_000));
        assert_eq!(held[0].attempts, 1);
    }

    #[test]
    fn settling_one_leaves_the_others() {
        // Two peers owed a receipt each, so "the right one" is distinguishable
        // from "all of them".
        let (_dir, book) = book();
        let one = Ulid::from_parts(100, 1);
        let two = Ulid::from_parts(200, 2);
        book.owe(&OwedReceipt::new(one, node(1), at(1_000)))
            .expect("owe");
        book.owe(&OwedReceipt::new(two, node(2), at(1_000)))
            .expect("owe");

        book.settle(one).expect("settle");

        let left = book.owed().expect("owed");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, two);
        assert_eq!(left[0].to, node(2));
    }

    #[test]
    fn settling_something_already_settled_is_not_an_error() {
        let (_dir, book) = book();
        book.settle(Ulid::from_parts(100, 1))
            .expect("the caller wanted it gone and it is gone");
    }

    #[test]
    fn a_removal_that_failed_for_any_other_reason_is_reported() {
        // The `NotFound` arm must not swallow the rest: anything else leaves a
        // receipt on disk that we believe is settled, and it will be delivered
        // a second time with nothing saying why.
        let id = Ulid::from_parts(100, 1);
        assert!(settled(Ok(()), id).is_ok());
        assert!(settled(Err(std::io::Error::from(std::io::ErrorKind::NotFound)), id).is_ok());
        let error = settled(
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            id,
        )
        .expect_err("a receipt we cannot remove is not settled");
        assert!(
            error.to_string().contains(&id.to_string()),
            "it should say which one: {error}"
        );
    }

    #[test]
    fn receipts_come_back_oldest_first() {
        let (_dir, book) = book();
        for millis in [300_u64, 100, 200] {
            book.owe(&OwedReceipt::new(
                Ulid::from_parts(millis, 0),
                node(1),
                at(1_000),
            ))
            .expect("owe");
        }
        let order: Vec<u64> = book
            .owed()
            .expect("owed")
            .iter()
            .map(|r| r.message.timestamp_ms())
            .collect();
        assert_eq!(order, vec![100, 200, 300]);
    }

    #[test]
    fn a_file_that_will_not_parse_is_skipped_rather_than_fatal() {
        // One bad file must not stop every other receipt, exactly as one bad
        // outbox entry must not stop delivery.
        let (dir, book) = book();
        let good = Ulid::from_parts(200, 0);
        book.owe(&OwedReceipt::new(good, node(1), at(1_000)))
            .expect("owe");
        fs::write(
            dir.path()
                .join("receipts")
                .join(format!("{}.json", Ulid::from_parts(100, 0))),
            b"{ not json",
        )
        .expect("write");

        let owed = book.owed().expect("owed");
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].message, good);
    }

    #[test]
    fn a_failed_attempt_is_counted_and_explained() {
        let mut owed = OwedReceipt::new(Ulid::from_parts(100, 1), node(1), at(1_000));
        owed.attempted(at(2_000), "could not reach 10.0.0.5:8400");
        owed.attempted(at(3_000), "could not reach 10.0.0.5:8400");
        assert_eq!(owed.attempts, 2);
        assert_eq!(owed.last_attempt, Some(at(3_000)));
        assert_eq!(
            owed.last_error.as_deref(),
            Some("could not reach 10.0.0.5:8400")
        );
    }

    #[test]
    fn what_travels_is_the_message_and_when_it_was_read() {
        // Not who it is for: the connection says that, and a node that could
        // nominate the reader could report on somebody else's reading.
        let id = Ulid::from_parts(100, 1);
        let owed = OwedReceipt::new(id, node(1), at(1_000));
        assert_eq!(
            owed.note(),
            ReadNote {
                id,
                read_at: at(1_000)
            }
        );
    }

    #[test]
    fn a_batch_round_trips_through_its_json() {
        let batch = ReadReceipts {
            read: vec![ReadNote {
                id: Ulid::from_parts(100, 1),
                read_at: at(1_000),
            }],
        };
        let json = serde_json::to_string(&batch).expect("encode");
        assert_eq!(
            serde_json::from_str::<ReadReceipts>(&json).expect("decode"),
            batch
        );
        assert_eq!(
            serde_json::from_str::<ReadReceipts>("{}").expect("an absent list is an empty one"),
            ReadReceipts::default()
        );
    }
}
