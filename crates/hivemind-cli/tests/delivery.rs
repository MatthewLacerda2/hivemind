//! Delivery and read confirmation, per recipient, through the shipped binary
//! (SPEC §8, §10, #31).
//!
//! Real daemons, real sockets and a machine that is genuinely off, because the
//! thing being tested is what a sender can *prove*: "queued" and "delivered"
//! are indistinguishable from inside one process, and #31's third incident is
//! that this daemon's own assertion turned out to be wrong.

mod daemon;

use daemon::{Daemon, json, pair};

/// The delivery state of one of this machine's sends, as the CLI reports it.
fn state_of(machine: &Daemon, subject: &str) -> serde_json::Value {
    let sent = json(&machine.run(&["sent", "--json"]));
    sent.as_array()
        .expect("an array")
        .iter()
        .find(|m| m["subject"] == subject)
        .unwrap_or_else(|| panic!("{subject:?} is not in what we sent: {sent}"))
        .clone()["delivery"]
        .clone()
}

/// Poll a message's delivery state until `wanted` holds, or say what it was.
///
/// A predicate rather than a state name, because "queued" is true the instant
/// the message is written and the interesting moment is the one after the
/// first pass — a wait that stopped at the name would stop before anything
/// had happened.
fn until(
    machine: &Daemon,
    subject: &str,
    described: &str,
    wanted: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    let mut last = serde_json::Value::Null;
    while std::time::Instant::now() < deadline {
        last = state_of(machine, subject);
        if wanted(&last) {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{subject:?} never became {described}; it was {last}");
}

#[test]
fn a_message_says_which_recipient_has_it_and_which_is_still_owed_one() {
    // Two recipients and one of them off, because a test against one recipient
    // proves very little: "the right one's state" has to be distinguishable
    // from "all of them".
    let sender = Daemon::start("sender");
    let here = Daemon::start("here");
    let mut away = Daemon::start("away");
    pair(&sender, &here);
    pair(&sender, &away);

    let here_id = here.node_id();
    let away_id = away.node_id();
    away.stop();

    sender.run(&[
        "send",
        &here_id,
        &away_id,
        "-s",
        "two machines",
        "--",
        "one of you is asleep",
    ]);

    // One took it; the other cannot, so the message as a whole is still queued.
    let waiting = until(
        &sender,
        "two machines",
        "delivered to exactly one of the two",
        |state| state["delivered"] == 1,
    );
    assert_eq!(waiting["recipients"], 2);
    assert_eq!(
        waiting["state"], "queued",
        "one machine that is off holds the whole message: {waiting}"
    );

    // And `read` says which is which, by name, with a reason for the one that
    // has not — the two minutes of suspecting loss in #31's first incident.
    let id = json(&sender.run(&["sent", "--json"]))[0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let opened = json(&sender.run(&["read", &id, "--json"]));
    let lines = opened["recipients"].as_array().expect("a line each");
    assert_eq!(lines.len(), 2);

    let took_it = lines
        .iter()
        .find(|line| line["node"] == here_id)
        .expect("the machine that is up");
    assert_eq!(took_it["state"], "delivered");
    assert!(took_it["delivered_at"].is_string());

    let asleep = lines
        .iter()
        .find(|line| line["node"] == away_id)
        .expect("the machine that is off");
    assert_eq!(asleep["state"], "queued");
    assert!(
        asleep["last_error"].is_string(),
        "it should say why, not merely that: {asleep}"
    );

    // Monday morning. The laptop comes back and the message goes through
    // (SPEC §8), and the state follows it rather than staying where it was.
    away.restart("away");
    let arrived = until(&sender, "two machines", "delivered to both", |state| {
        state["state"] == "delivered"
    });
    assert_eq!(arrived["delivered"], 2, "{arrived}");
    assert_eq!(arrived["read"], 0, "nobody has opened it");

    away.wait_for("two machines");
}

#[test]
fn the_listing_marks_a_message_nobody_has_taken_yet() {
    // The mark is what somebody sees without asking anything else, and it is
    // the weakest claim the message supports.
    let sender = Daemon::start("sender");
    let mut away = Daemon::start("away");
    pair(&sender, &away);
    let away_id = away.node_id();
    away.stop();

    sender.run(&["send", &away_id, "-s", "into the night", "--", "hello"]);

    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        let listed = sender.run(&["sent"]);
        if listed.contains("· queued") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the listing never marked it queued: {listed}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
