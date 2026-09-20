//! What [`Index::conversations`] must answer (#43).
//!
//! Every test here starts from **two** conversations and, where a filter is
//! under test, two machines. An assertion against one conversation cannot tell
//! "the right ones" from "all of them", which is exactly how
//! `an_unknown_mailbox_filter_returns_nothing` passed while the code returned
//! everything (#28).
//!
//! In its own file because `index.rs` is at its test limit, and the SQL stays
//! where `just boundaries` requires it.

use chrono::DateTime;
use ulid::Ulid;

use super::{Conversation, ConversationQuery, Index};
use crate::message::{Message, Recipient, SenderKind, fixture};
use crate::peer::NodeId;
use crate::store::Mailbox;

/// This machine, and the two it talks to.
fn me() -> NodeId {
    NodeId::from_certificate_der(b"this node")
}

fn ana() -> NodeId {
    NodeId::from_certificate_der(b"ana's laptop")
}

fn beto() -> NodeId {
    NodeId::from_certificate_der(b"beto's desktop")
}

/// One message, placed in a thread at a moment.
///
/// `millis` is both the id's timestamp and `sent_at`, so a test says which
/// message is newest rather than hoping.
fn message(millis: u64, thread: Ulid, from: NodeId, to: NodeId, subject: &str) -> Message {
    let mut message = fixture();
    message.id = Ulid::from_parts(millis, 0);
    message.thread_id = thread;
    message.in_reply_to = (thread != message.id).then_some(thread);
    message.from = from;
    message.to = vec![Recipient::Node(to)];
    message.subject = subject.to_owned();
    message.sent_at =
        DateTime::from_timestamp_millis(i64::try_from(millis).expect("in range")).expect("a time");
    message
}

/// A message that opens a thread of its own.
fn opening(millis: u64, from: NodeId, to: NodeId, subject: &str) -> Message {
    let id = Ulid::from_parts(millis, 0);
    message(millis, id, from, to, subject)
}

fn index_with(rows: &[(Mailbox, Message)]) -> Index {
    let index = Index::in_memory().expect("in-memory index");
    for (mailbox, message) in rows {
        index.upsert(*mailbox, message).expect("upsert");
    }
    index
}

fn listed(index: &Index, query: &ConversationQuery) -> Vec<Conversation> {
    index.conversations(query).expect("a conversation list")
}

fn subjects(found: &[Conversation]) -> Vec<&str> {
    found.iter().map(|c| c.subject.as_str()).collect()
}

#[test]
fn two_conversations_with_the_same_machine_stay_separate() {
    // The whole complaint in #43: six messages about one subject and three
    // about another, with the same machine, are nine mixed lines in `inbox`.
    let first = opening(100, me(), ana(), "dashboard PR");
    let second = opening(300, me(), ana(), "lunch?");
    let index = index_with(&[
        (Mailbox::Sent, first.clone()),
        (
            Mailbox::New,
            message(200, first.thread_id, ana(), me(), "Re: dashboard PR"),
        ),
        (Mailbox::Sent, second.clone()),
    ]);

    let found = listed(&index, &ConversationQuery::default());

    assert_eq!(
        subjects(&found),
        ["lunch?", "dashboard PR"],
        "two conversations, newest first, not five loose messages"
    );
    assert_eq!(found[0].thread_id, second.thread_id);
    assert_eq!(found[1].thread_id, first.thread_id);
    assert_eq!(found[0].messages, 1);
    assert_eq!(found[1].messages, 2, "the reply belongs to the first one");
}

#[test]
fn a_new_message_lifts_its_conversation_to_the_top() {
    let older = opening(100, me(), ana(), "dashboard PR");
    let newer = opening(200, me(), beto(), "lunch?");
    let rows = [
        (Mailbox::Sent, older.clone()),
        (Mailbox::Sent, newer.clone()),
    ];

    let index = index_with(&rows);
    assert_eq!(
        subjects(&listed(&index, &ConversationQuery::default())),
        ["lunch?", "dashboard PR"]
    );

    // Ana answers the older one, which is what a chat list is for.
    index
        .upsert(
            Mailbox::New,
            &message(300, older.thread_id, ana(), me(), "Re: dashboard PR"),
        )
        .expect("upsert");

    let found = listed(&index, &ConversationQuery::default());
    assert_eq!(
        subjects(&found),
        ["dashboard PR", "lunch?"],
        "the conversation that just moved is the one at the top"
    );
    assert_eq!(
        found[0].last_from,
        ana(),
        "and it is her message that is last"
    );
    assert_eq!(
        found[0].last_at.timestamp_millis(),
        300,
        "the list is ordered by when the last message was sent"
    );
}

#[test]
fn a_message_addressed_to_its_own_sender_is_counted_once() {
    // The index is keyed (id, mailbox), so a message sent to this machine is
    // in it twice: once as what was sent, once as what arrived. A thread
    // listed every one of them twice until #34, and a count that trusted the
    // rows would say this conversation has two messages in it.
    let note = opening(100, me(), me(), "note to self");
    let index = index_with(&[
        (Mailbox::Sent, note.clone()),
        (Mailbox::New, note.clone()),
        (Mailbox::Sent, opening(200, me(), ana(), "dashboard PR")),
    ]);

    let found = listed(&index, &ConversationQuery::default());

    let to_self = found
        .iter()
        .find(|c| c.thread_id == note.thread_id)
        .expect("the note to self is a conversation");
    assert_eq!(to_self.messages, 1, "one message, in two boxes");
    assert_eq!(to_self.unread, 1, "and unread once, not twice");
    assert_eq!(
        to_self.participants,
        vec![me()],
        "a conversation with oneself has one participant"
    );
}

#[test]
fn unread_counts_the_unread_and_not_the_rest() {
    let root = opening(100, ana(), me(), "dashboard PR");
    let index = index_with(&[
        // Arrived and read, arrived and not.
        (Mailbox::Cur, root.clone()),
        (
            Mailbox::New,
            message(200, root.thread_id, ana(), me(), "Re: dashboard PR"),
        ),
        (Mailbox::New, opening(300, beto(), me(), "lunch?")),
    ]);

    let found = listed(&index, &ConversationQuery::default());

    let dashboard = found
        .iter()
        .find(|c| c.thread_id == root.thread_id)
        .expect("the conversation is listed");
    assert_eq!(dashboard.messages, 2);
    assert_eq!(dashboard.unread, 1, "one of the two has been read");
}

#[test]
fn the_subject_is_the_one_the_conversation_opened_with() {
    // The replies are all `Re:` it, and a conversation has one subject by
    // construction. Showing the newest message's would show the `Re:`.
    let root = opening(100, ana(), me(), "dashboard PR");
    let reply = message(200, root.thread_id, me(), ana(), "Re: dashboard PR");
    let index = index_with(&[(Mailbox::New, root.clone()), (Mailbox::Sent, reply.clone())]);

    let found = listed(&index, &ConversationQuery::default());

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].subject, "dashboard PR");
    assert_eq!(
        found[0].last_id, reply.id,
        "and the last message is the reply"
    );
    assert_eq!(found[0].last_from, me());
    assert_eq!(found[0].last_sender_kind, SenderKind::Human);
}

#[test]
fn a_conversation_names_the_machine_it_is_with_before_that_machine_answers() {
    // "A new subject with the same person" is a `send`, and until it is
    // answered nobody but this machine has spoken in it. A list that read
    // participants off the senders would show a conversation with nobody.
    let index = index_with(&[
        (Mailbox::Out, opening(100, me(), ana(), "dashboard PR")),
        (Mailbox::Out, opening(200, me(), beto(), "lunch?")),
    ]);

    let found = listed(&index, &ConversationQuery::default());

    assert_eq!(subjects(&found), ["lunch?", "dashboard PR"]);
    assert_eq!(found[0].participants, sorted(&[me(), beto()]));
    assert_eq!(found[1].participants, sorted(&[me(), ana()]));
}

#[test]
fn participants_are_every_machine_in_the_conversation_once_each() {
    let root = opening(100, ana(), me(), "dashboard PR");
    let index = index_with(&[
        (Mailbox::New, root.clone()),
        (
            Mailbox::Sent,
            message(200, root.thread_id, me(), ana(), "Re: dashboard PR"),
        ),
        (
            Mailbox::New,
            message(300, root.thread_id, ana(), me(), "Re: dashboard PR"),
        ),
    ]);

    let found = listed(&index, &ConversationQuery::default());

    assert_eq!(
        found[0].participants,
        sorted(&[me(), ana()]),
        "two machines, whichever order the rows came back in"
    );
}

#[test]
fn with_narrows_the_list_to_the_conversations_one_machine_is_in() {
    let with_ana = opening(100, me(), ana(), "dashboard PR");
    let with_beto = opening(200, beto(), me(), "lunch?");
    let answered_by_ana = opening(300, me(), beto(), "the release");
    let index = index_with(&[
        (Mailbox::Sent, with_ana.clone()),
        (Mailbox::New, with_beto.clone()),
        (Mailbox::Sent, answered_by_ana.clone()),
        (
            Mailbox::New,
            message(
                400,
                answered_by_ana.thread_id,
                ana(),
                me(),
                "Re: the release",
            ),
        ),
    ]);

    let only_ana = listed(
        &index,
        &ConversationQuery {
            with: Some(ana()),
            ..ConversationQuery::default()
        },
    );

    assert_eq!(
        subjects(&only_ana),
        ["the release", "dashboard PR"],
        "the one she was written to and the one she answered, and not beto's"
    );

    let only_beto = listed(
        &index,
        &ConversationQuery {
            with: Some(beto()),
            ..ConversationQuery::default()
        },
    );
    assert_eq!(subjects(&only_beto), ["the release", "lunch?"]);
}

#[test]
fn a_machine_this_node_has_never_written_to_is_in_no_conversation() {
    // Not "everything", which is what a filter that fell through would give.
    let index = index_with(&[(Mailbox::Sent, opening(100, me(), ana(), "dashboard PR"))]);

    let found = listed(
        &index,
        &ConversationQuery {
            with: Some(beto()),
            ..ConversationQuery::default()
        },
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn the_limit_takes_the_conversations_that_moved_last() {
    let index = index_with(&[
        (Mailbox::Sent, opening(100, me(), ana(), "oldest")),
        (Mailbox::Sent, opening(200, me(), ana(), "middle")),
        (Mailbox::Sent, opening(300, me(), beto(), "newest")),
    ]);

    let found = listed(
        &index,
        &ConversationQuery {
            limit: Some(2),
            ..ConversationQuery::default()
        },
    );
    assert_eq!(subjects(&found), ["newest", "middle"]);
}

#[test]
fn nothing_here_is_no_conversations() {
    let index = Index::in_memory().expect("index");
    assert!(
        listed(&index, &ConversationQuery::default()).is_empty(),
        "an empty index has nothing to talk about"
    );
}

/// The machines in the order a [`Conversation`] promises.
fn sorted(nodes: &[NodeId]) -> Vec<NodeId> {
    let mut nodes = nodes.to_vec();
    nodes.sort_unstable();
    nodes
}
