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
    /// `out/` holds [`Outbound`] envelopes rather than bare messages, so it
    /// takes [`MailStore::put_outbound`] and leaves by
    /// [`MailStore::promote_to_sent`]; the generic operations refuse it.
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

/// A message in the outbox, with what we still owe each recipient.
///
/// SPEC §4.3 puts per-recipient delivery state "inside the file". It cannot go
/// inside `Message`: those fields are signed, and delivery state changes after
/// signing. So the outbox file holds an envelope — the signed message exactly
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
            attempts: 0,
            last_attempt: None,
            last_error: None,
        }
    }
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
    /// Returns [`StoreError::WrongMailbox`] for `out/`, which holds
    /// [`Outbound`] envelopes — use [`MailStore::put_outbound`]. Returns
    /// [`StoreError::Io`] if the write or rename fails.
    pub fn put(&self, mailbox: Mailbox, message: &Message) -> Result<(), StoreError> {
        Self::refuse_outbox(mailbox)?;
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

    /// `out/` is the one mailbox whose files are not bare messages.
    fn refuse_outbox(mailbox: Mailbox) -> Result<(), StoreError> {
        if mailbox == Mailbox::Out {
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
        let final_path = self.path_of(mailbox, id);
        // Same directory as the destination, so the rename below cannot cross a
        // filesystem boundary and stop being atomic. The suffix keeps it out of
        // `list`, which only accepts `.json`.
        let temp_path = final_path.with_extension("json.tmp");

        let json = serde_json::to_vec_pretty(body).map_err(|source| StoreError::Io {
            context: format!("could not encode {id}"),
            source: std::io::Error::other(source),
        })?;

        fs::write(&temp_path, &json).map_err(|source| StoreError::Io {
            context: format!("could not write {}", temp_path.display()),
            source,
        })?;

        // The only step that is visible to a reader, and it is atomic: the
        // message is either absent or complete, never half-written (ADR 0002).
        fs::rename(&temp_path, &final_path).map_err(|source| StoreError::Io {
            context: format!("could not move {} into place", temp_path.display()),
            source,
        })
    }

    /// Read a message out of a mailbox.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] if it is not there, [`StoreError::Corrupt`] if
    /// the file will not parse.
    pub fn get(&self, mailbox: Mailbox, id: Ulid) -> Result<Message, StoreError> {
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
    /// Write an outbox entry, replacing any entry with the same id.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the write or rename fails.
    pub fn put_outbound(&self, outbound: &Outbound) -> Result<(), StoreError> {
        self.write_json(Mailbox::Out, outbound.message.id, outbound)
    }

    /// Read an outbox entry.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] or [`StoreError::Corrupt`].
    pub fn get_outbound(&self, id: Ulid) -> Result<Outbound, StoreError> {
        self.read_json(Mailbox::Out, id)
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
            .filter_map(|id| self.get_outbound(id).ok())
            .collect())
    }

    /// Finish delivery: write the message to `sent/` and drop the envelope.
    ///
    /// `sent/` is read like any other mailbox, so the per-recipient delivery
    /// state is left behind rather than renamed along with the message.
    ///
    /// The order is deliberate. `sent/` is written first and `out/` cleared
    /// after, so a crash between the two leaves the message in both — and
    /// redelivery is always safe (SPEC §8) where losing it is not.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if either step fails.
    pub fn promote_to_sent(&self, outbound: &Outbound) -> Result<(), StoreError> {
        self.put(Mailbox::Sent, &outbound.message)?;
        self.remove(Mailbox::Out, outbound.message.id)
    }

    /// Move a message between mailboxes, leaving its bytes untouched.
    ///
    /// # Errors
    /// Returns [`StoreError::WrongMailbox`] if either side is `out/`, whose
    /// files are envelopes rather than messages — delivery leaves it through
    /// [`MailStore::promote_to_sent`]. Returns [`StoreError::NotFound`] if the
    /// message is not in `from`, or [`StoreError::Io`] if the rename fails.
    pub fn move_to(&self, from: Mailbox, to: Mailbox, id: Ulid) -> Result<(), StoreError> {
        Self::refuse_outbox(from)?;
        Self::refuse_outbox(to)?;
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
        store.put(Mailbox::Sent, &message).expect("put");
        assert_eq!(store.get(Mailbox::Sent, message.id).expect("get"), message);
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
        store.put(Mailbox::Sent, &fixture()).expect("put");

        let entries: Vec<_> = fs::read_dir(dir.path().join("mail").join("sent"))
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
    fn a_delivered_message_is_promoted_to_sent_as_a_plain_message() {
        // `sent/` is read like any other mailbox, so the delivery bookkeeping
        // must be dropped on the way rather than renamed along with it.
        let (_dir, store) = store();
        let message = fixture();
        let outbound = Outbound {
            recipients: vec![RecipientState::pending(message.from)],
            message: message.clone(),
        };
        store.put_outbound(&outbound).expect("put_outbound");

        store.promote_to_sent(&outbound).expect("promote");

        assert_eq!(
            store
                .get(Mailbox::Sent, message.id)
                .expect("read as a message"),
            message
        );
        assert!(
            matches!(
                store.get_outbound(message.id),
                Err(StoreError::NotFound { .. })
            ),
            "the outbox copy should be gone"
        );
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
        store.put_outbound(&outbound).expect("put_outbound");
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
        store.put(Mailbox::Sent, &message).expect("put");

        store.remove(Mailbox::New, message.id).expect("remove");

        assert!(store.get(Mailbox::New, message.id).is_err());
        assert!(store.get(Mailbox::Sent, message.id).is_ok());
    }

    #[test]
    fn removing_something_that_is_not_there_is_not_an_error() {
        // The caller wanted it gone and it is gone.
        let (_dir, store) = store();
        store.remove(Mailbox::New, fixture().id).expect("remove");
    }
}
