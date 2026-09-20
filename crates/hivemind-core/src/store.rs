//! The maildir-style message store.
//!
//! Files under `~/.hivemind/mail/` are the source of truth (SPEC §4.3,
//! `docs/decisions/0002-files-are-source-of-truth.md`). Every write is
//! write-to-temp plus atomic rename; nothing is ever edited in place.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr as _;

use ulid::Ulid;

use crate::message::Message;
use crate::peer::NodeId;

/// Which of the four directories a message is sitting in (SPEC §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mailbox {
    /// Received and unread.
    New,
    /// Received and read.
    Cur,
    /// Written by us, not yet delivered to every recipient.
    Out,
    /// Written by us and delivered to everyone.
    Sent,
}

impl std::fmt::Display for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Mailbox {
    /// Every mailbox, in the order they appear in SPEC §4.3.
    pub const ALL: [Self; 4] = [Self::New, Self::Cur, Self::Out, Self::Sent];

    /// The directory name under `mail/`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Cur => "cur",
            Self::Out => "out",
            Self::Sent => "sent",
        }
    }
}

impl Mailbox {
    /// Parse [`Mailbox::as_str`].
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// Whether messages here are unread (SPEC §4.3).
    #[must_use]
    pub fn is_unread(self) -> bool {
        matches!(self, Self::New)
    }

    /// Whether the files here are [`Outbound`] envelopes rather than bare
    /// messages (SPEC §4.3).
    ///
    /// Both outgoing boxes are. `out/` always was; `sent/` became one with #31,
    /// because dropping the envelope on promotion threw away the only record of
    /// which recipient had taken the message and which had read it — and a
    /// message everybody has is exactly the one somebody asks about.
    #[must_use]
    pub fn holds_envelopes(self) -> bool {
        matches!(self, Self::Out | Self::Sent)
    }
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No message with that id in that mailbox.
    #[error("no message {id} in {mailbox}")]
    NotFound {
        /// The id that was looked for.
        id: Ulid,
        /// Where it was looked for.
        mailbox: &'static str,
    },
    /// The filesystem said no.
    #[error("{context}")]
    Io {
        /// What we were trying to do.
        context: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// An operation was asked for on a mailbox that cannot support it.
    ///
    /// `out/` and `sent/` hold [`Outbound`] envelopes rather than bare
    /// messages, so they take [`MailStore::put_outbound`] and `out/` leaves by
    /// [`MailStore::promote_to_sent`]; the generic operations refuse both.
    #[error("{mailbox}/ holds delivery envelopes, not plain messages")]
    WrongMailbox {
        /// The mailbox that was asked for.
        mailbox: &'static str,
    },
    /// A file in the store could not be parsed as a message.
    #[error("message file {path} is not readable as a message")]
    Corrupt {
        /// The offending file.
        path: PathBuf,
        /// What the parser said.
        #[source]
        source: serde_json::Error,
    },
}

/// How far one recipient's copy has got (SPEC §8, #31).
///
/// Three states and no more, because each is a different *kind* of claim.
/// `Queued` is what this node knows on its own; `Delivered` is what the far
/// node asserted when it accepted the bytes; `Read` is what the person at the
/// far end chose to tell us. A sender who cannot tell the three apart is left
/// with their own daemon's word for it, which is what #31 found to be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Delivery {
    /// Written here, and nobody has confirmed taking it.
    Queued,
    /// The recipient's node accepted it.
    Delivered,
    /// The recipient opened it, and their node said so (SPEC §8).
    Read,
}

impl Delivery {
    /// Every state, weakest claim first.
    pub const ALL: [Self; 3] = [Self::Queued, Self::Delivered, Self::Read];

    /// The stable string used by the API and the CLI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Read => "read",
        }
    }

    /// Parse [`Delivery::as_str`].
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == s)
    }
}

/// A message on its way out, with what each recipient has done with it.
///
/// SPEC §4.3 puts per-recipient delivery state "inside the file". It cannot go
/// inside `Message`: those fields are signed, and delivery state changes after
/// signing. So the outgoing file holds an envelope — the signed message exactly
/// as it will be sent, plus our own bookkeeping alongside it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Outbound {
    /// The message, byte-identical to what each recipient receives.
    pub message: Message,
    /// One entry per recipient the message was expanded to at send time.
    pub recipients: Vec<RecipientState>,
}

impl Outbound {
    /// Is there anyone left to deliver to?
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.recipients.iter().all(|r| r.delivered_at.is_some())
    }

    /// How far one recipient has got, or `None` if this is not for them.
    ///
    /// Absent rather than [`Delivery::Queued`]: "I am not on the list" and
    /// "nobody has taken it yet" are different answers, and a caller told the
    /// second would go looking for a delivery that was never owed.
    #[must_use]
    pub fn state_of(&self, node: NodeId) -> Option<Delivery> {
        self.recipients
            .iter()
            .find(|r| r.node == node)
            .map(RecipientState::state)
    }

    /// Recipients still owed a copy.
    pub fn outstanding(&self) -> impl Iterator<Item = &RecipientState> {
        self.recipients.iter().filter(|r| r.delivered_at.is_none())
    }

    /// Record a successful delivery. Returns whether anything changed.
    pub fn mark_delivered(&mut self, to: NodeId, at: chrono::DateTime<chrono::Utc>) -> bool {
        match self
            .recipients
            .iter_mut()
            .find(|r| r.node == to && r.delivered_at.is_none())
        {
            Some(state) => {
                state.delivered_at = Some(at);
                true
            }
            None => false,
        }
    }

    /// Record that a recipient read it. Returns whether anything changed.
    ///
    /// A receipt is also proof of delivery, so it stamps `delivered_at` if
    /// nothing else has: a recipient cannot read what never reached them, and
    /// our own record of the delivery can be lost between the far node's
    /// `202` and the write that follows it.
    ///
    /// The first receipt wins. A second one is not new information — that is
    /// still when they read it — and redelivery of a receipt is as safe as
    /// redelivery of mail (SPEC §8).
    pub fn mark_read(&mut self, by: NodeId, at: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(state) = self
            .recipients
            .iter_mut()
            .find(|r| r.node == by && r.read_at.is_none())
        else {
            return false;
        };
        state.read_at = Some(at);
        state.delivered_at = state.delivered_at.or(Some(at));
        true
    }

    /// Record a failed attempt, so backoff can be computed from it.
    pub fn mark_attempted(&mut self, to: NodeId, at: chrono::DateTime<chrono::Utc>, why: &str) {
        if let Some(state) = self.recipients.iter_mut().find(|r| r.node == to) {
            state.attempts = state.attempts.saturating_add(1);
            state.last_attempt = Some(at);
            // Kept for `hivemind status` and for a human wondering why their
            // message has not arrived.
            state.last_error = Some(why.to_owned());
        }
    }
}

/// What we owe one recipient.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecipientState {
    /// The node this copy is for.
    pub node: NodeId,
    /// When it was accepted, if it has been.
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When they read it, if their node said so (SPEC §8).
    ///
    /// Defaulted, so an envelope written before #31 reads back as one nobody
    /// has told us about rather than not reading back at all.
    #[serde(default)]
    pub read_at: Option<chrono::DateTime<chrono::Utc>>,
    /// How many times we have tried.
    pub attempts: u32,
    /// When we last tried.
    pub last_attempt: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
}

impl RecipientState {
    /// A recipient we have not tried yet.
    #[must_use]
    pub fn pending(node: NodeId) -> Self {
        Self {
            node,
            delivered_at: None,
            read_at: None,
            attempts: 0,
            last_attempt: None,
            last_error: None,
        }
    }

    /// How far this recipient's copy has got.
    #[must_use]
    pub fn state(&self) -> Delivery {
        if self.read_at.is_some() {
            Delivery::Read
        } else if self.delivered_at.is_some() {
            Delivery::Delivered
        } else {
            Delivery::Queued
        }
    }
}

/// Write a JSON body to `path` by writing a temporary file and renaming it.
///
/// The rename is the only step visible to a reader, and it is atomic: the file
/// is either absent or complete, never half-written (ADR 0002). The temporary
/// file is in the same directory so the rename cannot cross a filesystem
/// boundary and stop being atomic, and its suffix keeps it out of every
/// listing here, which only accept `.json`.
///
/// Shared with the receipt queue, which is a directory of small JSON files
/// with the same durability requirement and no reason to write them a second
/// way.
pub(crate) fn write_atomic<T: serde::Serialize>(
    path: &std::path::Path,
    body: &T,
) -> Result<(), StoreError> {
    let temp_path = path.with_extension("json.tmp");

    let json = serde_json::to_vec_pretty(body).map_err(|source| StoreError::Io {
        context: format!("could not encode {}", path.display()),
        source: std::io::Error::other(source),
    })?;

    fs::write(&temp_path, &json).map_err(|source| StoreError::Io {
        context: format!("could not write {}", temp_path.display()),
        source,
    })?;

    fs::rename(&temp_path, path).map_err(|source| StoreError::Io {
        context: format!("could not move {} into place", temp_path.display()),
        source,
    })
}

/// The on-disk mail store.
#[derive(Debug, Clone)]
pub struct MailStore {
    root: PathBuf,
}

impl MailStore {
    /// Open the store rooted at `mail/`, creating the four mailboxes if needed.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if a directory cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        for mailbox in Mailbox::ALL {
            let dir = root.join(mailbox.as_str());
            fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
                context: format!("could not create {}", dir.display()),
                source,
            })?;
        }
        Ok(Self { root })
    }

    /// Where a message would live.
    #[must_use]
    pub fn path_of(&self, mailbox: Mailbox, id: Ulid) -> PathBuf {
        self.root.join(mailbox.as_str()).join(format!("{id}.json"))
    }

    /// Write a message into a mailbox, replacing any message already there with
    /// the same id.
    ///
    /// # Errors
    /// Returns [`StoreError::WrongMailbox`] for `out/` and `sent/`, which hold
    /// [`Outbound`] envelopes — use [`MailStore::put_outbound`]. Returns
    /// [`StoreError::Io`] if the write or rename fails.
    pub fn put(&self, mailbox: Mailbox, message: &Message) -> Result<(), StoreError> {
        Self::refuse_envelopes(mailbox)?;
        self.write_json(mailbox, message.id, message)
    }

    /// Delete a message, if it is there.
    ///
    /// Not an error when it is already gone: the caller wanted it removed, and
    /// it is removed.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the file exists but cannot be deleted.
    pub fn remove(&self, mailbox: Mailbox, id: Ulid) -> Result<(), StoreError> {
        match fs::remove_file(self.path_of(mailbox, id)) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                context: format!("could not remove {id} from {}", mailbox.as_str()),
                source,
            }),
        }
    }

    /// The outgoing mailboxes' files are not bare messages.
    fn refuse_envelopes(mailbox: Mailbox) -> Result<(), StoreError> {
        if mailbox.holds_envelopes() {
            return Err(StoreError::WrongMailbox {
                mailbox: mailbox.as_str(),
            });
        }
        Ok(())
    }

    /// Write any JSON body into a mailbox, atomically.
    fn write_json<T: serde::Serialize>(
        &self,
        mailbox: Mailbox,
        id: Ulid,
        body: &T,
    ) -> Result<(), StoreError> {
        write_atomic(&self.path_of(mailbox, id), body)
    }

    /// Read a message out of a mailbox.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] if it is not there, [`StoreError::Corrupt`] if
    /// the file will not parse.
    pub fn get(&self, mailbox: Mailbox, id: Ulid) -> Result<Message, StoreError> {
        if mailbox.holds_envelopes() {
            return self.get_outbound(mailbox, id).map(|out| out.message);
        }
        self.read_json(mailbox, id)
    }

    /// Read any JSON body out of a mailbox.
    fn read_json<T: serde::de::DeserializeOwned>(
        &self,
        mailbox: Mailbox,
        id: Ulid,
    ) -> Result<T, StoreError> {
        let path = self.path_of(mailbox, id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound {
                    id,
                    mailbox: mailbox.as_str(),
                });
            }
            Err(source) => {
                return Err(StoreError::Io {
                    context: format!("could not read {}", path.display()),
                    source,
                });
            }
        };

        // A file that will not parse is a different problem from a file that is
        // not there, and saying "not found" would send someone looking in the
        // wrong place.
        serde_json::from_slice(&bytes).map_err(|source| StoreError::Corrupt { path, source })
    }

    /// Every message id in a mailbox, oldest first.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the directory cannot be read.
    pub fn list(&self, mailbox: Mailbox) -> Result<Vec<Ulid>, StoreError> {
        let dir = self.root.join(mailbox.as_str());
        let entries = fs::read_dir(&dir).map_err(|source| StoreError::Io {
            context: format!("could not list {}", dir.display()),
            source,
        })?;

        let mut ids: Vec<Ulid> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter_map(|name| {
                // Anything that is not `<ulid>.json` is somebody else's file:
                // an editor backup, a .DS_Store, a half-finished download.
                let name = name.to_str()?;
                let stem = name.strip_suffix(".json")?;
                Ulid::from_str(stem).ok()
            })
            .collect();

        // ULIDs sort lexicographically in time order, so this is oldest first.
        ids.sort_unstable();
        Ok(ids)
    }

    /// Move a message between mailboxes.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] if it is not in `from`.
    /// Write an outgoing entry, replacing any entry with the same id.
    ///
    /// # Errors
    /// Returns [`StoreError::WrongMailbox`] for a mailbox that holds received
    /// mail, or [`StoreError::Io`] if the write or rename fails.
    pub fn put_outbound(&self, mailbox: Mailbox, outbound: &Outbound) -> Result<(), StoreError> {
        if !mailbox.holds_envelopes() {
            return Err(StoreError::WrongMailbox {
                mailbox: mailbox.as_str(),
            });
        }
        self.write_json(mailbox, outbound.message.id, outbound)
    }

    /// Read an outgoing entry, from `out/` or `sent/`.
    ///
    /// A `sent/` file written before #31 is a bare message, so one that will
    /// not parse as an envelope is tried as a message and reported as an
    /// envelope nobody has told us anything about. The fallback is second, not
    /// first: a genuinely broken file must stay [`StoreError::Corrupt`] rather
    /// than become "nobody has it".
    ///
    /// # Errors
    /// [`StoreError::WrongMailbox`] for a mailbox that holds received mail,
    /// [`StoreError::NotFound`], or [`StoreError::Corrupt`].
    pub fn get_outbound(&self, mailbox: Mailbox, id: Ulid) -> Result<Outbound, StoreError> {
        if !mailbox.holds_envelopes() {
            return Err(StoreError::WrongMailbox {
                mailbox: mailbox.as_str(),
            });
        }
        match self.read_json::<Outbound>(mailbox, id) {
            Err(StoreError::Corrupt { path, source }) => {
                match self.read_json::<Message>(mailbox, id) {
                    Ok(message) => Ok(Outbound {
                        message,
                        recipients: Vec::new(),
                    }),
                    Err(_) => Err(StoreError::Corrupt { path, source }),
                }
            }
            other => other,
        }
    }

    /// Every outbox entry, oldest first.
    ///
    /// Entries that will not parse are skipped: one bad file must not stop
    /// delivery of everything behind it.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the directory cannot be read.
    pub fn list_outbound(&self) -> Result<Vec<Outbound>, StoreError> {
        Ok(self
            .list(Mailbox::Out)?
            .into_iter()
            .filter_map(|id| self.get_outbound(Mailbox::Out, id).ok())
            .collect())
    }

    /// Finish delivery: write the envelope to `sent/` and clear `out/`.
    ///
    /// The envelope travels rather than the bare message, so which recipient
    /// took it and which has read it outlives the queue (#31).
    ///
    /// The order is deliberate. `sent/` is written first and `out/` cleared
    /// after, so a crash between the two leaves the message in both — and
    /// redelivery is always safe (SPEC §8) where losing it is not.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if either step fails.
    pub fn promote_to_sent(&self, outbound: &Outbound) -> Result<(), StoreError> {
        self.put_outbound(Mailbox::Sent, outbound)?;
        self.remove(Mailbox::Out, outbound.message.id)
    }

    /// Move a message between mailboxes, leaving its bytes untouched.
    ///
    /// # Errors
    /// Returns [`StoreError::WrongMailbox`] if either side is an outgoing
    /// mailbox, whose files are envelopes rather than messages — delivery
    /// leaves `out/` through [`MailStore::promote_to_sent`]. Returns
    /// [`StoreError::NotFound`] if the message is not in `from`, or
    /// [`StoreError::Io`] if the rename fails.
    pub fn move_to(&self, from: Mailbox, to: Mailbox, id: Ulid) -> Result<(), StoreError> {
        Self::refuse_envelopes(from)?;
        Self::refuse_envelopes(to)?;
        if from == to {
            return Ok(());
        }

        let source_path = self.path_of(from, id);
        let target_path = self.path_of(to, id);

        // A rename, not a read-and-rewrite: the bytes are signed, and moving a
        // message between mailboxes must not be able to change them.
        match fs::rename(&source_path, &target_path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NotFound {
                    id,
                    mailbox: from.as_str(),
                })
            }
            Err(source) => Err(StoreError::Io {
                context: format!(
                    "could not move {} to {}",
                    source_path.display(),
                    target_path.display()
                ),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{fixture, test_signing_key};

    fn store() -> (tempfile::TempDir, MailStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = MailStore::open(dir.path().join("mail")).expect("store opens");
        (dir, store)
    }

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("in range")
    }

    #[test]
    fn opening_a_store_creates_the_four_mailboxes() {
        let (dir, _store) = store();
        for name in ["new", "cur", "out", "sent"] {
            assert!(
                dir.path().join("mail").join(name).is_dir(),
                "mail/{name} should exist"
            );
        }
    }

    #[test]
    fn opening_an_existing_store_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        MailStore::open(dir.path().join("mail")).expect("first open");
        MailStore::open(dir.path().join("mail")).expect("second open must succeed");
    }

    #[test]
    fn a_message_written_to_a_mailbox_reads_back_identical() {
        let (_dir, store) = store();
        let message = fixture();
        store.put(Mailbox::Cur, &message).expect("put");
        assert_eq!(store.get(Mailbox::Cur, message.id).expect("get"), message);
    }

    #[test]
    fn a_signed_message_still_verifies_after_a_round_trip_through_the_store() {
        // Files are the source of truth (ADR 0002) and a stored message must
        // stay verifiable (ADR 0007). This is both claims at once.
        let (_dir, store) = store();
        let mut message = fixture();
        message.sign(&test_signing_key()).expect("sign");
        store.put(Mailbox::New, &message).expect("put");

        let read = store.get(Mailbox::New, message.id).expect("get");
        assert!(read.verify(&test_signing_key().verifying_key()).is_ok());
    }

    #[test]
    fn writing_leaves_no_temporary_files_behind() {
        let (dir, store) = store();
        store.put(Mailbox::Cur, &fixture()).expect("put");

        let entries: Vec<_> = fs::read_dir(dir.path().join("mail").join("cur"))
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one file, got {entries:?}"
        );
        assert_eq!(
            std::path::Path::new(&entries[0]).extension(),
            Some("json".as_ref()),
            "a leftover .json.tmp would mean the rename never happened; got {entries:?}"
        );
    }

    #[test]
    fn writing_the_same_message_twice_is_idempotent() {
        // Redelivery is always safe: a recipient dedupes on message id (SPEC §8).
        let (_dir, store) = store();
        let message = fixture();
        store.put(Mailbox::New, &message).expect("first put");
        store.put(Mailbox::New, &message).expect("second put");
        assert_eq!(store.list(Mailbox::New).expect("list"), vec![message.id]);
    }

    #[test]
    fn listing_an_empty_mailbox_returns_nothing() {
        let (_dir, store) = store();
        assert!(store.list(Mailbox::Cur).expect("list").is_empty());
    }

    #[test]
    fn listing_returns_ids_oldest_first() {
        let (_dir, store) = store();
        let mut ids = Vec::new();
        for millis in [3_000_u64, 1_000, 2_000] {
            let mut message = fixture();
            message.id = Ulid::from_parts(millis, 0);
            store.put(Mailbox::New, &message).expect("put");
            ids.push(message.id);
        }
        ids.sort_unstable();
        assert_eq!(store.list(Mailbox::New).expect("list"), ids);
    }

    #[test]
    fn listing_ignores_files_that_are_not_messages() {
        // A stray editor backup or a half-finished download must not be
        // mistaken for mail.
        let (dir, store) = store();
        store.put(Mailbox::New, &fixture()).expect("put");
        let boxdir = dir.path().join("mail").join("new");
        fs::write(boxdir.join("notes.txt"), b"scratch").expect("write");
        fs::write(boxdir.join(".DS_Store"), b"junk").expect("write");
        fs::write(boxdir.join("not-a-ulid.json"), b"{}").expect("write");

        assert_eq!(store.list(Mailbox::New).expect("list"), vec![fixture().id]);
    }

    #[test]
    fn reading_a_message_that_is_not_there_reports_not_found() {
        let (_dir, store) = store();
        let missing = fixture().id;
        assert!(matches!(
            store.get(Mailbox::Cur, missing),
            Err(StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn reading_a_corrupt_message_file_says_so_rather_than_pretending_it_is_missing() {
        let (dir, store) = store();
        let message = fixture();
        store.put(Mailbox::New, &message).expect("put");
        fs::write(
            dir.path()
                .join("mail")
                .join("new")
                .join(format!("{}.json", message.id)),
            b"{ this is not json",
        )
        .expect("write");

        assert!(matches!(
            store.get(Mailbox::New, message.id),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn moving_a_message_removes_it_from_the_source_mailbox() {
        let (_dir, store) = store();
        let message = fixture();
        store.put(Mailbox::New, &message).expect("put");
        store
            .move_to(Mailbox::New, Mailbox::Cur, message.id)
            .expect("move");

        assert!(store.list(Mailbox::New).expect("list").is_empty());
        assert_eq!(store.list(Mailbox::Cur).expect("list"), vec![message.id]);
    }

    #[test]
    fn a_moved_message_is_unchanged() {
        let (_dir, store) = store();
        let mut message = fixture();
        message.sign(&test_signing_key()).expect("sign");
        store.put(Mailbox::New, &message).expect("put");
        store
            .move_to(Mailbox::New, Mailbox::Cur, message.id)
            .expect("move");

        assert_eq!(store.get(Mailbox::Cur, message.id).expect("get"), message);
    }

    #[test]
    fn moving_a_message_that_is_not_there_reports_not_found() {
        let (_dir, store) = store();
        assert!(matches!(
            store.move_to(Mailbox::New, Mailbox::Cur, fixture().id),
            Err(StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn the_outbox_refuses_a_bare_message_because_it_holds_envelopes() {
        // Writing one here produced a file nothing could read back: `get`
        // wanted an Outbound and the index skipped it, so the sender could not
        // see their own message. Refusing it is the only way that cannot
        // silently recur.
        let (_dir, store) = store();
        assert!(matches!(
            store.put(Mailbox::Out, &fixture()),
            Err(StoreError::WrongMailbox { .. })
        ));
    }

    #[test]
    fn a_delivered_message_keeps_its_per_recipient_state_in_sent() {
        // The bookkeeping used to be dropped here, which is why "did it
        // arrive, and to whom" had no answer once the last recipient took it
        // (#31). `sent/` holds the envelope too, and the message inside it is
        // still the signed one.
        let (_dir, store) = store();
        let message = fixture();
        let mut outbound = Outbound {
            recipients: vec![RecipientState::pending(message.from)],
            message: message.clone(),
        };
        store
            .put_outbound(Mailbox::Out, &outbound)
            .expect("put_outbound");
        assert!(outbound.mark_delivered(message.from, at(1_000)));

        store.promote_to_sent(&outbound).expect("promote");

        assert_eq!(
            store
                .get(Mailbox::Sent, message.id)
                .expect("read as a message"),
            message
        );
        assert_eq!(
            store
                .get_outbound(Mailbox::Sent, message.id)
                .expect("read as an envelope")
                .recipients,
            outbound.recipients,
            "what each recipient did outlives the outbox"
        );
        assert!(
            matches!(
                store.get_outbound(Mailbox::Out, message.id),
                Err(StoreError::NotFound { .. })
            ),
            "the outbox copy should be gone"
        );
    }

    #[test]
    fn a_read_receipt_names_one_recipient_and_leaves_the_others_alone() {
        // Two recipients, so "the right one" is distinguishable from "all of
        // them" — which is the whole point of per-recipient state.
        let one = NodeId::from_certificate_der(b"one");
        let two = NodeId::from_certificate_der(b"two");
        let mut outbound = Outbound {
            recipients: vec![RecipientState::pending(one), RecipientState::pending(two)],
            message: fixture(),
        };

        assert_eq!(outbound.state_of(one), Some(Delivery::Queued));
        assert!(outbound.mark_delivered(one, at(1_000)));
        assert!(outbound.mark_delivered(two, at(1_000)));
        assert!(outbound.mark_read(one, at(2_000)));

        assert_eq!(outbound.state_of(one), Some(Delivery::Read));
        assert_eq!(outbound.state_of(two), Some(Delivery::Delivered));
        assert_eq!(outbound.recipients[0].read_at, Some(at(2_000)));
        assert_eq!(outbound.recipients[1].read_at, None);
        assert_eq!(outbound.state_of(fixture().from), None, "not a recipient");
    }

    #[test]
    fn a_second_read_receipt_for_the_same_recipient_changes_nothing() {
        // Redelivery of a receipt is as safe as redelivery of mail (SPEC §8),
        // and the time kept is the first one: that is when they read it.
        let one = NodeId::from_certificate_der(b"one");
        let mut outbound = Outbound {
            recipients: vec![RecipientState::pending(one)],
            message: fixture(),
        };
        assert!(outbound.mark_read(one, at(2_000)));
        assert!(!outbound.mark_read(one, at(3_000)));
        assert_eq!(outbound.recipients[0].read_at, Some(at(2_000)));
    }

    #[test]
    fn a_read_receipt_is_itself_proof_of_delivery() {
        // A recipient cannot read what never reached it, and our own record of
        // the delivery can be lost — a crash between the accepted response and
        // the write is exactly what #31's third incident is about.
        let one = NodeId::from_certificate_der(b"one");
        let mut outbound = Outbound {
            recipients: vec![RecipientState::pending(one)],
            message: fixture(),
        };
        assert!(outbound.mark_read(one, at(2_000)));
        assert_eq!(outbound.recipients[0].delivered_at, Some(at(2_000)));
        assert!(outbound.is_complete());
    }

    #[test]
    fn a_sent_file_written_before_delivery_state_moved_there_still_reads() {
        // `sent/` held a bare message until #31, and that file is what
        // somebody upgrading brings with them. It reads, with nothing claimed
        // about who took it — the honest answer for a file that never said.
        let (dir, store) = store();
        let message = fixture();
        fs::write(
            dir.path()
                .join("mail")
                .join("sent")
                .join(format!("{}.json", message.id)),
            serde_json::to_vec_pretty(&message).expect("encode"),
        )
        .expect("write");

        assert_eq!(store.get(Mailbox::Sent, message.id).expect("get"), message);
        assert!(
            store
                .get_outbound(Mailbox::Sent, message.id)
                .expect("envelope")
                .recipients
                .is_empty()
        );
    }

    #[test]
    fn an_unreadable_envelope_is_corrupt_rather_than_an_empty_one() {
        // The fallback above must not swallow a genuinely broken file: that
        // would turn "this will not parse" into "nobody has it".
        let (dir, store) = store();
        fs::write(
            dir.path()
                .join("mail")
                .join("sent")
                .join(format!("{}.json", fixture().id)),
            b"{ this is not json",
        )
        .expect("write");

        assert!(matches!(
            store.get_outbound(Mailbox::Sent, fixture().id),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn only_the_two_outgoing_mailboxes_hold_envelopes() {
        assert!(!Mailbox::New.holds_envelopes());
        assert!(!Mailbox::Cur.holds_envelopes());
        assert!(Mailbox::Out.holds_envelopes());
        assert!(Mailbox::Sent.holds_envelopes());
    }

    #[test]
    fn promoting_writes_sent_before_removing_out_so_a_crash_redelivers() {
        // Redelivery is safe (SPEC §8); losing the message is not. If the
        // order were reversed, a crash between the two steps would drop it.
        let (_dir, store) = store();
        let message = fixture();
        let outbound = Outbound {
            recipients: vec![RecipientState::pending(message.from)],
            message: message.clone(),
        };
        store
            .put_outbound(Mailbox::Out, &outbound)
            .expect("put_outbound");
        store.promote_to_sent(&outbound).expect("promote");

        // Promoting again is a no-op rather than an error, because that is
        // what resuming after a crash looks like.
        store.promote_to_sent(&outbound).expect("promote again");
        assert!(store.get(Mailbox::Sent, message.id).is_ok());
    }

    #[test]
    fn moving_into_or_out_of_the_outbox_is_refused() {
        let (_dir, store) = store();
        let message = fixture();
        store.put(Mailbox::New, &message).expect("put");

        assert!(matches!(
            store.move_to(Mailbox::New, Mailbox::Out, message.id),
            Err(StoreError::WrongMailbox { .. })
        ));
        assert!(matches!(
            store.move_to(Mailbox::Out, Mailbox::Sent, message.id),
            Err(StoreError::WrongMailbox { .. })
        ));
    }

    #[test]
    fn removing_a_message_leaves_the_other_mailboxes_alone() {
        let (_dir, store) = store();
        let message = fixture();
        store.put(Mailbox::New, &message).expect("put");
        store.put(Mailbox::Cur, &message).expect("put");

        store.remove(Mailbox::New, message.id).expect("remove");

        assert!(store.get(Mailbox::New, message.id).is_err());
        assert!(store.get(Mailbox::Cur, message.id).is_ok());
    }

    #[test]
    fn removing_something_that_is_not_there_is_not_an_error() {
        // The caller wanted it gone and it is gone.
        let (_dir, store) = store();
        store.remove(Mailbox::New, fixture().id).expect("remove");
    }
}
