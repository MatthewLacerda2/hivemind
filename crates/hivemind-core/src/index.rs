//! The derived `SQLite` query index.
//!
//! `index.db` answers "how many unread", "this thread", "search subject and
//! body" quickly. It holds no authoritative state: deleting it is always safe
//! and the daemon rebuilds it from `mail/` on startup when it is missing or its
//! schema version has moved (SPEC §4.3,
//! `docs/decisions/0002-files-are-source-of-truth.md`).

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use ulid::Ulid;

use crate::message::{Kind, Message, SenderKind};
use crate::peer::NodeId;
use crate::store::{MailStore, Mailbox};

/// Bumping this throws the index away and rebuilds it. That is the whole
/// migration story, and it is why the index must never hold anything the mail
/// files do not (SPEC §4.3).
pub const SCHEMA_VERSION: u32 = 2;

/// What a listing shows without opening the message (SPEC §9.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The message id.
    pub id: Ulid,
    /// The thread it belongs to.
    pub thread_id: Ulid,
    /// Who sent it.
    pub from: NodeId,
    /// Its subject.
    pub subject: String,
    /// What it is for.
    pub kind: Kind,
    /// Whether a person or an agent wrote it.
    pub sender_kind: SenderKind,
    /// When the sender sent it.
    pub sent_at: DateTime<Utc>,
    /// Which mailbox it is in.
    pub mailbox: Mailbox,
    /// The names of its attachments, in order.
    pub attachment_names: Vec<String>,
}

impl Summary {
    /// Whether this message is still unread.
    #[must_use]
    pub fn is_unread(&self) -> bool {
        self.mailbox.is_unread()
    }
}

/// What to look for (SPEC §7.1).
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Restrict to one mailbox.
    pub mailbox: Option<Mailbox>,
    /// Restrict to one thread.
    pub thread: Option<Ulid>,
    /// Restrict to one sender.
    pub from: Option<NodeId>,
    /// Only messages that have not been read.
    pub unread_only: bool,
    /// Full-text search over subject and body.
    pub text: Option<String>,
    /// How many to return. `None` means everything.
    pub limit: Option<usize>,
}

/// Why an index operation failed.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// `SQLite` said no.
    #[error("{context}")]
    Sqlite {
        /// What we were trying to do.
        context: String,
        /// What went wrong.
        #[source]
        source: rusqlite::Error,
    },
    /// A row in the index could not be read back into a [`Summary`].
    ///
    /// The index is derived, so the response to this is to rebuild it.
    #[error("index row for {id} is not readable: {detail}")]
    CorruptRow {
        /// The offending message id.
        id: String,
        /// What was wrong with it.
        detail: String,
    },
    /// The store could not be read while rebuilding.
    #[error("could not read the mail store while rebuilding the index")]
    Store(#[from] crate::store::StoreError),
}

/// The derived query index.
#[derive(Debug)]
pub struct Index {
    conn: Connection,
}

/// The whole schema. There is no migration path on purpose: a version bump
/// drops this and rebuilds from `mail/` (SPEC §4.3).
const SCHEMA: &str = "
CREATE TABLE messages (
    id               TEXT NOT NULL,
    thread_id        TEXT NOT NULL,
    in_reply_to      TEXT,
    from_node        BLOB NOT NULL,
    subject          TEXT NOT NULL,
    kind             TEXT NOT NULL,
    sender_kind      TEXT NOT NULL,
    sent_at          INTEGER NOT NULL,
    mailbox          TEXT NOT NULL,
    attachment_names TEXT NOT NULL,
    -- A message addressed to its own sender genuinely exists twice: once in
    -- `sent` as our copy, once in `new` as the one we received. The pair is
    -- the identity, not the id alone.
    PRIMARY KEY (id, mailbox)
) STRICT;

CREATE INDEX messages_by_sent_at ON messages(sent_at DESC);
CREATE INDEX messages_by_thread  ON messages(thread_id);
CREATE INDEX messages_by_sender  ON messages(from_node);
CREATE INDEX messages_by_mailbox ON messages(mailbox);

-- Standalone rather than an external-content table: the index is rebuilt
-- wholesale rather than migrated, so the bookkeeping an external-content
-- table needs would buy nothing.
CREATE VIRTUAL TABLE messages_fts USING fts5(id UNINDEXED, subject, body);
";

fn sqlite<T>(context: &str, r: Result<T, rusqlite::Error>) -> Result<T, IndexError> {
    r.map_err(|source| IndexError::Sqlite {
        context: context.to_owned(),
        source,
    })
}

impl Index {
    fn create(conn: Connection) -> Result<Self, IndexError> {
        sqlite(
            "could not create the index schema",
            conn.execute_batch(SCHEMA),
        )?;
        sqlite(
            "could not stamp the schema version",
            conn.pragma_update(None, "user_version", SCHEMA_VERSION),
        )?;
        Ok(Self { conn })
    }

    /// Does this connection hold a usable index at the current version?
    fn is_current(conn: &Connection) -> bool {
        let version: Result<u32, _> = conn.query_row("PRAGMA user_version", [], |row| row.get(0));
        // The table check matters as well as the version: a zero-length file
        // reads as a valid empty database with user_version 0.
        version.is_ok_and(|v| v == SCHEMA_VERSION)
            && conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .is_ok()
    }
}

impl Index {
    /// Open (or create) the index at `path`.
    ///
    /// If the file is missing, unreadable, or carries a different schema
    /// version, it is discarded and recreated empty — the caller is expected to
    /// follow with [`Index::rebuild_from`].
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] if the database cannot be opened.
    pub fn open(path: &std::path::Path) -> Result<Self, IndexError> {
        if let Ok(conn) = Connection::open(path)
            && Self::is_current(&conn)
        {
            return Ok(Self { conn });
        }

        // Missing, stale, or not a database at all. All three mean the same
        // thing for a derived cache: start over. The caller rebuilds.
        let _ = std::fs::remove_file(path);
        let conn = sqlite(
            "could not create the index database",
            Connection::open(path),
        )?;
        Self::create(conn)
    }

    /// An index that lives only in memory, for tests and one-shot queries.
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] if `SQLite` cannot create it.
    pub fn in_memory() -> Result<Self, IndexError> {
        let conn = sqlite(
            "could not create an in-memory index",
            Connection::open_in_memory(),
        )?;
        Self::create(conn)
    }

    /// The schema version this index was created with.
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] if the pragma cannot be read.
    pub fn schema_version(&self) -> Result<u32, IndexError> {
        sqlite(
            "could not read the schema version",
            self.conn
                .query_row("PRAGMA user_version", [], |row| row.get(0)),
        )
    }

    /// Add or replace a message.
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] on failure.
    pub fn upsert(&self, mailbox: Mailbox, message: &Message) -> Result<(), IndexError> {
        let id = message.id.to_string();
        let names: Vec<&str> = message
            .attachments
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let names = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_owned());

        sqlite(
            "could not index a message",
            self.conn.execute(
                "INSERT INTO messages
                     (id, thread_id, in_reply_to, from_node, subject, kind,
                      sender_kind, sent_at, mailbox, attachment_names)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id, mailbox) DO UPDATE SET
                     thread_id = excluded.thread_id,
                     in_reply_to = excluded.in_reply_to,
                     from_node = excluded.from_node,
                     subject = excluded.subject,
                     kind = excluded.kind,
                     sender_kind = excluded.sender_kind,
                     sent_at = excluded.sent_at,
                     mailbox = excluded.mailbox,
                     attachment_names = excluded.attachment_names",
                rusqlite::params![
                    &id,
                    message.thread_id.to_string(),
                    message.in_reply_to.map(|u| u.to_string()),
                    message.from.as_bytes().as_slice(),
                    &message.subject,
                    message.kind.as_str(),
                    message.sender_kind.as_str(),
                    message.sent_at.timestamp_millis(),
                    mailbox.as_str(),
                    &names,
                ],
            ),
        )?;

        // One shared full-text row per message id. FTS5 has no upsert, so it is
        // replaced outright.
        sqlite(
            "could not clear the previous full-text row",
            self.conn
                .execute("DELETE FROM messages_fts WHERE id = ?1", [&id]),
        )?;
        sqlite(
            "could not index a message for full-text search",
            self.conn.execute(
                "INSERT INTO messages_fts (id, subject, body) VALUES (?1, ?2, ?3)",
                rusqlite::params![&id, &message.subject, &message.body],
            ),
        )?;
        Ok(())
    }

    /// Record that a message has moved between mailboxes.
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] on failure.
    pub fn set_mailbox(&self, id: Ulid, from: Mailbox, to: Mailbox) -> Result<(), IndexError> {
        sqlite(
            "could not update a message's mailbox",
            self.conn.execute(
                "UPDATE messages SET mailbox = ?3 WHERE id = ?1 AND mailbox = ?2",
                rusqlite::params![id.to_string(), from.as_str(), to.as_str()],
            ),
        )?;
        Ok(())
    }

    /// Forget a message.
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] on failure.
    pub fn remove(&self, id: Ulid, mailbox: Mailbox) -> Result<(), IndexError> {
        let id = id.to_string();
        sqlite(
            "could not remove a message from the index",
            self.conn.execute(
                "DELETE FROM messages WHERE id = ?1 AND mailbox = ?2",
                rusqlite::params![&id, mailbox.as_str()],
            ),
        )?;

        // One full-text row per message, shared by its copies: drop it only
        // once the last copy is gone.
        let remaining: i64 = sqlite(
            "could not count remaining copies",
            self.conn
                .query_row("SELECT COUNT(*) FROM messages WHERE id = ?1", [&id], |r| {
                    r.get(0)
                }),
        )?;
        if remaining == 0 {
            sqlite(
                "could not remove a message from full-text search",
                self.conn
                    .execute("DELETE FROM messages_fts WHERE id = ?1", [&id]),
            )?;
        }
        Ok(())
    }

    /// How many unread messages there are.
    ///
    /// This is what `hivemind hook check` asks on every Claude turn boundary,
    /// so it must stay a single indexed count (SPEC §9.3).
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] on failure.
    pub fn unread_count(&self) -> Result<u64, IndexError> {
        // SQLite counts are signed; a negative one is not a thing COUNT(*) can
        // produce, so saturating is the honest conversion.
        let count: i64 = sqlite(
            "could not count unread messages",
            self.conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE mailbox = ?1",
                [Mailbox::New.as_str()],
                |row| row.get(0),
            ),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Run a query, newest first.
    ///
    /// # Errors
    /// Returns [`IndexError::Sqlite`] on failure, or [`IndexError::CorruptRow`]
    /// if a stored row cannot be read back.
    pub fn search(&self, query: &Query) -> Result<Vec<Summary>, IndexError> {
        let mut sql = String::from(
            "SELECT id, thread_id, from_node, subject, kind, sender_kind, sent_at,
                    mailbox, attachment_names
             FROM messages WHERE 1 = 1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(mailbox) = query.mailbox {
            params.push(Box::new(mailbox.as_str()));
            let _ = write!(sql, " AND mailbox = ?{}", params.len());
        }
        if query.unread_only {
            params.push(Box::new(Mailbox::New.as_str()));
            let _ = write!(sql, " AND mailbox = ?{}", params.len());
        }
        if let Some(thread) = query.thread {
            params.push(Box::new(thread.to_string()));
            let _ = write!(sql, " AND thread_id = ?{}", params.len());
        }
        if let Some(from) = query.from {
            params.push(Box::new(from.as_bytes().to_vec()));
            let _ = write!(sql, " AND from_node = ?{}", params.len());
        }
        if let Some(text) = &query.text {
            params.push(Box::new(fts_query(text)));
            let _ = write!(
                sql,
                " AND id IN (SELECT id FROM messages_fts WHERE messages_fts MATCH ?{})",
                params.len()
            );
        }

        sql.push_str(" ORDER BY sent_at DESC, id DESC");
        if let Some(limit) = query.limit {
            let _ = write!(sql, " LIMIT {limit}");
        }

        let mut stmt = sqlite("could not prepare the query", self.conn.prepare(&sql))?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let rows = sqlite(
            "could not run the query",
            stmt.query_map(refs.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            }),
        )?;

        let mut out = Vec::new();
        for row in rows {
            let row = sqlite("could not read a result row", row)?;
            out.push(summary_from_row(row)?);
        }
        Ok(out)
    }

    /// Throw away everything and rebuild from the mail files.
    ///
    /// # Errors
    /// Returns [`IndexError::Store`] if the store cannot be read.
    pub fn rebuild_from(&mut self, store: &MailStore) -> Result<(), IndexError> {
        let tx = sqlite("could not start the rebuild", self.conn.transaction())?;
        sqlite(
            "could not clear the index",
            tx.execute_batch("DELETE FROM messages; DELETE FROM messages_fts;"),
        )?;
        sqlite("could not commit the clear", tx.commit())?;

        for mailbox in Mailbox::ALL {
            for id in store.list(mailbox)? {
                // `out/` holds an Outbound envelope: the signed message plus
                // the per-recipient delivery state, which cannot live inside
                // the message because it changes after signing.
                let message = if mailbox == Mailbox::Out {
                    store.get_outbound(id).map(|o| o.message)
                } else {
                    store.get(mailbox, id)
                };

                // A message that will not parse is skipped rather than fatal:
                // the index is a cache, and refusing to start because of one
                // bad file would take the other thousand with it.
                if let Ok(message) = message {
                    self.upsert(mailbox, &message)?;
                }
            }
        }
        Ok(())
    }
}

/// Turn whatever a human typed into a safe FTS5 query.
///
/// FTS5 has an operator language, and a user typing an unbalanced quote or a
/// bare `NEAR(` should get no results rather than a syntax error. Every run of
/// alphanumerics becomes a quoted term; everything else is dropped.
fn fts_query(text: &str) -> String {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if terms.is_empty() {
        // Matches nothing, which is the honest answer to a query with no terms.
        return "\"\"".to_owned();
    }
    terms.join(" ")
}

type Row = (
    String,
    String,
    Vec<u8>,
    String,
    String,
    String,
    i64,
    String,
    String,
);

fn summary_from_row(row: Row) -> Result<Summary, IndexError> {
    let (id, thread_id, from, subject, kind, sender_kind, sent_at, mailbox, names) = row;

    let corrupt = |detail: &str| IndexError::CorruptRow {
        id: id.clone(),
        detail: detail.to_owned(),
    };

    Ok(Summary {
        id: id.parse().map_err(|_| corrupt("id is not a ULID"))?,
        thread_id: thread_id
            .parse()
            .map_err(|_| corrupt("thread_id is not a ULID"))?,
        from: NodeId::from_bytes(
            <[u8; 32]>::try_from(from.as_slice())
                .map_err(|_| corrupt("from_node is not 32 bytes"))?,
        ),
        subject,
        kind: Kind::from_str_opt(&kind).ok_or_else(|| corrupt("unknown kind"))?,
        sender_kind: SenderKind::from_str_opt(&sender_kind)
            .ok_or_else(|| corrupt("unknown sender_kind"))?,
        sent_at: DateTime::from_timestamp_millis(sent_at)
            .ok_or_else(|| corrupt("sent_at is out of range"))?,
        mailbox: Mailbox::from_str_opt(&mailbox).ok_or_else(|| corrupt("unknown mailbox"))?,
        attachment_names: serde_json::from_str(&names)
            .map_err(|_| corrupt("attachment_names is not a JSON array"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::fixture;

    fn message_at(millis: u64, subject: &str, body: &str) -> Message {
        let mut message = fixture();
        message.id = Ulid::from_parts(millis, 0);
        message.thread_id = message.id;
        message.subject = subject.to_owned();
        message.body = body.to_owned();
        message.sent_at =
            DateTime::from_timestamp_millis(i64::try_from(millis).expect("in range")).expect("ts");
        message
    }

    fn index_with(messages: &[(Mailbox, Message)]) -> Index {
        let index = Index::in_memory().expect("in-memory index");
        for (mailbox, message) in messages {
            index.upsert(*mailbox, message).expect("upsert");
        }
        index
    }

    #[test]
    fn a_new_index_carries_the_current_schema_version() {
        let index = Index::in_memory().expect("index");
        assert_eq!(index.schema_version().expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn an_index_from_an_older_schema_version_is_discarded_and_recreated() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("index.db");

        // An index written by a previous version, with a row in it.
        {
            let index = Index::open(&path).expect("open");
            index
                .upsert(Mailbox::New, &message_at(1, "old", "old"))
                .expect("upsert");
            index
                .conn
                .pragma_update(None, "user_version", SCHEMA_VERSION - 1)
                .expect("downgrade");
        }

        let index = Index::open(&path).expect("reopen");
        assert_eq!(index.schema_version().expect("version"), SCHEMA_VERSION);
        assert_eq!(
            index.search(&Query::default()).expect("search").len(),
            0,
            "a stale index must be emptied, not carried forward"
        );
    }

    #[test]
    fn a_file_that_is_not_a_database_is_discarded_rather_than_fatal() {
        // The index is derived. Anything unreadable is a reason to rebuild, not
        // a reason to refuse to start (ADR 0002).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("index.db");
        std::fs::write(&path, b"this is not a sqlite database").expect("write");

        let index = Index::open(&path).expect("a corrupt index must not be fatal");
        assert_eq!(index.schema_version().expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn an_indexed_message_is_found_by_an_empty_query() {
        let message = message_at(1_000, "dashboard PR", "take a look");
        let index = index_with(&[(Mailbox::New, message.clone())]);

        let found = index.search(&Query::default()).expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, message.id);
        assert_eq!(found[0].subject, "dashboard PR");
        assert_eq!(found[0].mailbox, Mailbox::New);
        assert_eq!(found[0].attachment_names, vec!["notes.md".to_owned()]);
    }

    #[test]
    fn a_message_addressed_to_its_own_sender_exists_in_two_mailboxes() {
        // Our copy in `sent`, and the one we received in `new`. Both are real
        // and a listing should show each in its own place.
        let message = message_at(1_000, "note to self", "remember this");
        let index = index_with(&[
            (Mailbox::Sent, message.clone()),
            (Mailbox::New, message.clone()),
        ]);

        let found = index.search(&Query::default()).expect("search");
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|s| s.id == message.id));
        assert_eq!(index.unread_count().expect("count"), 1);
    }

    #[test]
    fn removing_one_copy_leaves_the_other_searchable() {
        let message = message_at(1_000, "dashboard PR", "body");
        let index = index_with(&[
            (Mailbox::Sent, message.clone()),
            (Mailbox::New, message.clone()),
        ]);
        index.remove(message.id, Mailbox::New).expect("remove");

        let by_text = index
            .search(&Query {
                text: Some("dashboard".to_owned()),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(by_text.len(), 1, "the surviving copy must stay searchable");
        assert_eq!(by_text[0].mailbox, Mailbox::Sent);
    }

    #[test]
    fn indexing_the_same_message_twice_leaves_one_row() {
        let message = message_at(1_000, "once", "body");
        let index = index_with(&[(Mailbox::New, message.clone()), (Mailbox::New, message)]);
        assert_eq!(index.search(&Query::default()).expect("search").len(), 1);
    }

    #[test]
    fn results_come_back_newest_first() {
        let index = index_with(&[
            (Mailbox::New, message_at(1_000, "oldest", "a")),
            (Mailbox::New, message_at(3_000, "newest", "b")),
            (Mailbox::New, message_at(2_000, "middle", "c")),
        ]);
        let subjects: Vec<String> = index
            .search(&Query::default())
            .expect("search")
            .into_iter()
            .map(|s| s.subject)
            .collect();
        assert_eq!(subjects, ["newest", "middle", "oldest"]);
    }

    #[test]
    fn unread_count_counts_only_the_new_mailbox() {
        let index = index_with(&[
            (Mailbox::New, message_at(1_000, "unread one", "a")),
            (Mailbox::New, message_at(2_000, "unread two", "b")),
            (Mailbox::Cur, message_at(3_000, "already read", "c")),
            (Mailbox::Sent, message_at(4_000, "mine", "d")),
        ]);
        assert_eq!(index.unread_count().expect("count"), 2);
    }

    #[test]
    fn marking_a_message_read_moves_it_out_of_the_unread_count() {
        let message = message_at(1_000, "dashboard PR", "a");
        let index = index_with(&[(Mailbox::New, message.clone())]);
        assert_eq!(index.unread_count().expect("count"), 1);

        index
            .set_mailbox(message.id, Mailbox::New, Mailbox::Cur)
            .expect("set mailbox");
        assert_eq!(index.unread_count().expect("count"), 0);
        assert_eq!(
            index.search(&Query::default()).expect("search")[0].mailbox,
            Mailbox::Cur
        );
    }

    #[test]
    fn filtering_by_mailbox_excludes_the_others() {
        let index = index_with(&[
            (Mailbox::New, message_at(1_000, "incoming", "a")),
            (Mailbox::Sent, message_at(2_000, "outgoing", "b")),
        ]);
        let found = index
            .search(&Query {
                mailbox: Some(Mailbox::Sent),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "outgoing");
    }

    #[test]
    fn filtering_by_thread_returns_only_that_thread() {
        let root = message_at(1_000, "root", "a");
        let mut reply = message_at(2_000, "reply", "b");
        reply.thread_id = root.thread_id;
        reply.in_reply_to = Some(root.id);
        let other = message_at(3_000, "unrelated", "c");

        let index = index_with(&[
            (Mailbox::New, root.clone()),
            (Mailbox::New, reply),
            (Mailbox::New, other),
        ]);

        let found = index
            .search(&Query {
                thread: Some(root.thread_id),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|s| s.thread_id == root.thread_id));
    }

    #[test]
    fn filtering_by_sender_returns_only_that_senders_mail() {
        let mine = message_at(1_000, "from me", "a");
        let mut theirs = message_at(2_000, "from them", "b");
        theirs.from = NodeId::from_certificate_der(b"somebody else");

        let index = index_with(&[(Mailbox::New, mine.clone()), (Mailbox::New, theirs)]);

        let found = index
            .search(&Query {
                from: Some(mine.from),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "from me");
    }

    #[test]
    fn unread_only_excludes_read_and_sent_mail() {
        let index = index_with(&[
            (Mailbox::New, message_at(1_000, "unread", "a")),
            (Mailbox::Cur, message_at(2_000, "read", "b")),
            (Mailbox::Sent, message_at(3_000, "sent", "c")),
        ]);
        let found = index
            .search(&Query {
                unread_only: true,
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "unread");
    }

    #[test]
    fn full_text_search_matches_a_word_in_the_subject() {
        let index = index_with(&[
            (Mailbox::New, message_at(1_000, "dashboard PR", "unrelated")),
            (Mailbox::New, message_at(2_000, "lunch", "unrelated")),
        ]);
        let found = index
            .search(&Query {
                text: Some("dashboard".to_owned()),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "dashboard PR");
    }

    #[test]
    fn full_text_search_matches_a_word_in_the_body() {
        let index = index_with(&[
            (
                Mailbox::New,
                message_at(1_000, "a", "the migration is ready"),
            ),
            (Mailbox::New, message_at(2_000, "b", "nothing to see")),
        ]);
        let found = index
            .search(&Query {
                text: Some("migration".to_owned()),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "a");
    }

    #[test]
    fn full_text_search_returns_nothing_when_nothing_matches() {
        let index = index_with(&[(Mailbox::New, message_at(1_000, "a", "b"))]);
        let found = index
            .search(&Query {
                text: Some("absent".to_owned()),
                ..Query::default()
            })
            .expect("search");
        assert!(found.is_empty());
    }

    #[test]
    fn full_text_search_does_not_treat_user_input_as_query_syntax() {
        // FTS5 has an operator language. A user typing a quote or a NEAR should
        // get no results, not a syntax error and not somebody else's mail.
        let index = index_with(&[(Mailbox::New, message_at(1_000, "a", "b"))]);
        for text in ["\"unbalanced", "NEAR(", "a OR b", "*", ""] {
            let found = index.search(&Query {
                text: Some(text.to_owned()),
                ..Query::default()
            });
            assert!(found.is_ok(), "{text:?} should not be an error");
        }
    }

    #[test]
    fn a_removed_message_disappears_from_search_and_from_full_text() {
        let message = message_at(1_000, "dashboard PR", "a");
        let index = index_with(&[(Mailbox::New, message.clone())]);
        index.remove(message.id, Mailbox::New).expect("remove");

        assert!(index.search(&Query::default()).expect("search").is_empty());
        let by_text = index
            .search(&Query {
                text: Some("dashboard".to_owned()),
                ..Query::default()
            })
            .expect("search");
        assert!(by_text.is_empty(), "the fts row must go too");
    }

    #[test]
    fn limit_caps_the_number_of_results() {
        let index = index_with(&[
            (Mailbox::New, message_at(1_000, "a", "x")),
            (Mailbox::New, message_at(2_000, "b", "x")),
            (Mailbox::New, message_at(3_000, "c", "x")),
        ]);
        let found = index
            .search(&Query {
                limit: Some(2),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].subject, "c", "limit keeps the newest");
    }

    #[test]
    fn a_message_still_being_delivered_is_indexed_from_its_outbox_envelope() {
        // `out/` holds an Outbound, not a bare Message, because per-recipient
        // delivery state cannot live inside the signed message. Reading it as
        // a Message fails, and the index would silently skip it — so a sender
        // could not see their own message until the last recipient took it.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = MailStore::open(dir.path().join("mail")).expect("store");

        let message = message_at(1_000, "still going out", "body");
        store
            .put_outbound(&crate::store::Outbound {
                recipients: vec![crate::store::RecipientState::pending(message.from)],
                message: message.clone(),
            })
            .expect("put_outbound");

        let mut index = Index::in_memory().expect("index");
        index.rebuild_from(&store).expect("rebuild");

        let found = index
            .search(&Query {
                mailbox: Some(Mailbox::Out),
                ..Query::default()
            })
            .expect("search");
        assert_eq!(
            found.len(),
            1,
            "a message in the outbox should be visible to its sender"
        );
        assert_eq!(found[0].id, message.id);
    }

    #[test]
    fn rebuilding_from_the_store_reproduces_the_live_index() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = MailStore::open(dir.path().join("mail")).expect("store");

        let live = Index::in_memory().expect("index");
        for (millis, mailbox) in [
            (1_000_u64, Mailbox::New),
            (2_000, Mailbox::Cur),
            (3_000, Mailbox::Out),
            (4_000, Mailbox::Sent),
        ] {
            let message = message_at(millis, &format!("subject {millis}"), "body");
            if mailbox == Mailbox::Out {
                store
                    .put_outbound(&crate::store::Outbound {
                        recipients: vec![crate::store::RecipientState::pending(message.from)],
                        message: message.clone(),
                    })
                    .expect("put_outbound");
            } else {
                store.put(mailbox, &message).expect("put");
            }
            live.upsert(mailbox, &message).expect("upsert");
        }

        let mut rebuilt = Index::in_memory().expect("index");
        rebuilt.rebuild_from(&store).expect("rebuild");

        assert_eq!(
            rebuilt.search(&Query::default()).expect("search"),
            live.search(&Query::default()).expect("search")
        );
        assert_eq!(
            rebuilt.unread_count().expect("count"),
            live.unread_count().expect("count")
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// SPEC §13.2 asks for exactly this: any sequence of mail operations,
        /// and the index rebuilt from `mail/` must equal the index that was
        /// maintained as it went. It is the test that makes ADR 0002 safe —
        /// keeping two stores in step is only sound if drift is detectable.
        ///
        /// The operations are the ones the daemon actually performs, not any
        /// pairing of mailboxes: `out/` is only ever entered by sending and
        /// only ever left by finishing delivery.
        #[test]
        fn a_rebuilt_index_equals_the_index_maintained_along_the_way(
            ops in proptest::collection::vec((0_usize..6, 0_usize..4), 0..40)
        ) {
            let dir = tempfile::tempdir().expect("temp dir");
            let store = MailStore::open(dir.path().join("mail")).expect("store");
            let live = Index::in_memory().expect("index");

            // Where each message currently sits, so a repeat operation is a
            // move rather than a second copy.
            let mut placed: std::collections::HashMap<usize, Mailbox> =
                std::collections::HashMap::new();

            for (which, action) in ops {
                let message = message_at(
                    1_000 + which as u64,
                    &format!("subject {which}"),
                    &format!("body {which}"),
                );
                let outbound = crate::store::Outbound {
                    recipients: vec![crate::store::RecipientState::pending(message.from)],
                    message: message.clone(),
                };

                match (action, placed.get(&which).copied()) {
                    // Arrive from a peer.
                    (0, None) => {
                        store.put(Mailbox::New, &message).expect("put");
                        live.upsert(Mailbox::New, &message).expect("upsert");
                        placed.insert(which, Mailbox::New);
                    }
                    // Read it.
                    (1, Some(Mailbox::New)) => {
                        store.move_to(Mailbox::New, Mailbox::Cur, message.id).expect("move");
                        live.set_mailbox(message.id, Mailbox::New, Mailbox::Cur)
                            .expect("set mailbox");
                        placed.insert(which, Mailbox::Cur);
                    }
                    // Send it.
                    (2, None) => {
                        store.put_outbound(&outbound).expect("put_outbound");
                        live.upsert(Mailbox::Out, &message).expect("upsert");
                        placed.insert(which, Mailbox::Out);
                    }
                    // Every recipient took it.
                    (3, Some(Mailbox::Out)) => {
                        store.promote_to_sent(&outbound).expect("promote");
                        live.set_mailbox(message.id, Mailbox::Out, Mailbox::Sent)
                            .expect("set mailbox");
                        placed.insert(which, Mailbox::Sent);
                    }
                    // Anything else is not a thing the daemon can do from here.
                    _ => {}
                }
            }

            let mut rebuilt = Index::in_memory().expect("index");
            rebuilt.rebuild_from(&store).expect("rebuild");

            proptest::prop_assert_eq!(
                rebuilt.search(&Query::default()).expect("search"),
                live.search(&Query::default()).expect("search")
            );
            proptest::prop_assert_eq!(
                rebuilt.unread_count().expect("count"),
                live.unread_count().expect("count")
            );
        }
    }

    #[test]
    fn rebuilding_discards_rows_that_are_no_longer_in_the_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = MailStore::open(dir.path().join("mail")).expect("store");

        let mut index = Index::in_memory().expect("index");
        index
            .upsert(Mailbox::New, &message_at(1_000, "ghost", "gone"))
            .expect("upsert");

        index.rebuild_from(&store).expect("rebuild");
        assert!(index.search(&Query::default()).expect("search").is_empty());
    }
}
