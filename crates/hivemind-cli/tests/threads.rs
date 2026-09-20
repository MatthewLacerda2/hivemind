//! `hivemind thread`: reading a conversation as a conversation (#34).
//!
//! Through the shipped binary, because what this is about is what somebody —
//! or a Claude resuming a session — sees on the screen, and an in-process test
//! of the service layer cannot see that.
//!
//! Two threads in every store here on purpose. With one thread, "the right
//! messages" and "all the messages" are the same list, which is how the
//! mailbox filter in #28 passed a test while returning everything.

mod daemon;

use daemon::{Daemon, json, pair};

/// The name the single-daemon tests here run under.
const NAME: &str = "solo";

/// The id of the message with this subject, as the inbox reports it.
fn id_of(daemon: &Daemon, subject: &str) -> String {
    let inbox = daemon.inbox();
    let found = inbox
        .as_array()
        .expect("an array")
        .iter()
        .find(|m| m["subject"] == subject)
        .unwrap_or_else(|| panic!("no {subject:?} in {inbox}"));
    found["id"].as_str().expect("an id").to_owned()
}

/// The tail of an id, which is what the CLI prints and what somebody copies.
fn short(id: &str) -> &str {
    &id[id.len() - 6..]
}

/// Two conversations on one machine: a thread of two, and a message alone.
fn two_threads(daemon: &Daemon) -> (String, String) {
    daemon.run(&["send", "everyone", "-s", "dashboard PR", "-b", "take a look"]);
    daemon.run(&["send", "everyone", "-s", "lunch", "-b", "1pm?"]);
    let root = id_of(daemon, "dashboard PR");
    daemon.run(&["reply", &root, "-b", "on it"]);
    (root, id_of(daemon, "Re: dashboard PR"))
}

#[test]
fn a_thread_prints_in_order_and_holds_nothing_from_the_other_conversation() {
    let daemon = Daemon::start(NAME);
    let (root, reply) = two_threads(&daemon);

    // The reply's short id, because nobody knows by heart which message was
    // first and the short form is what the inbox printed (#27, #34).
    let printed = daemon.run(&["thread", short(&reply)]);

    let first = printed.find("take a look").expect("the root's body");
    let second = printed.find("on it").expect("the reply's body");
    assert!(first < second, "a conversation reads forwards: {printed}");
    assert!(
        !printed.contains("lunch") && !printed.contains("1pm?"),
        "the other conversation is not part of this one: {printed}"
    );
    assert!(
        printed.contains("2 messages"),
        "say how big the conversation is: {printed}"
    );

    // The root's whole id resolves to the same conversation.
    let by_root = json(&daemon.run(&["thread", &root, "--json"]));
    let ids: Vec<&str> = by_root
        .as_array()
        .expect("an array")
        .iter()
        .map(|m| m["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, [root.as_str(), reply.as_str()]);
    assert_eq!(by_root[0]["body"], "take a look");
    assert_eq!(by_root[1]["body"], "on it");
}

#[test]
fn a_thread_between_two_machines_says_who_said_what() {
    // The case #34 was filed for: six messages with the machine next door,
    // read as six loose things in arrival order. Two participants, so "who
    // said what" is something the output can get wrong.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    pair(&alice, &bob);

    alice.run(&["send", &bob.node_id(), "-s", "a question", "-b", "what time?"]);
    let question = bob.wait_for("a question");
    let question_id = question["id"].as_str().expect("an id").to_owned();
    bob.run(&["reply", &question_id, "-b", "one o'clock"]);
    let answer = alice.wait_for("Re: a question");
    let answer_id = answer["id"].as_str().expect("an id").to_owned();

    // Alice asks for the conversation by the id of the reply she just got.
    let printed = alice.run(&["thread", short(&answer_id)]);

    // The four letters each machine is shown as, the same as `inbox` and
    // `read` print (#42 is about improving that, everywhere at once).
    let hers = alice.node_id()[4..8].to_owned();
    let his = bob.node_id()[4..8].to_owned();
    assert!(
        printed.contains(&hers) && printed.contains(&his),
        "both participants should be on it: {printed}"
    );

    let asked = printed.find("what time?").expect("her question");
    let answered = printed.find("one o'clock").expect("his answer");
    assert!(asked < answered, "oldest first: {printed}");
    assert!(
        printed.find(&hers).expect("her") < printed.find(&his).expect("him"),
        "and each body should follow the machine that sent it: {printed}"
    );
}

#[test]
fn reading_one_message_says_how_much_more_the_thread_holds() {
    let daemon = Daemon::start(NAME);
    let (root, _) = two_threads(&daemon);

    let printed = daemon.run(&["read", &root]);
    assert!(
        printed.contains("1 more in this thread"),
        "a message in a conversation should say so: {printed}"
    );
    assert!(
        printed.contains("hivemind thread"),
        "and how to read it: {printed}"
    );

    // The message on its own says nothing, or the line above proves nothing.
    let alone = daemon.run(&["read", &id_of(&daemon, "lunch")]);
    assert!(
        !alone.contains("in this thread"),
        "nothing to point at, so nothing to say: {alone}"
    );
}
