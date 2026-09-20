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
    let reader = Daemon::start("reader");
    let mut away = Daemon::start("away");
    pair(&sender, &reader);
    pair(&sender, &away);

    let reader_id = reader.node_id();
    let away_id = away.node_id();
    away.stop();

    sender.run(&[
        "send",
        &reader_id,
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
        .find(|line| line["node"] == reader_id)
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

#[test]
fn a_read_receipt_travels_back_and_only_for_the_machine_that_read_it() {
    // The test #31's roadmap asks for, with a second recipient so that "the
    // right machine's state" is distinguishable from "all of them": send to a
    // peer that is off, see `queued`, bring the peer up, see `delivered`, read
    // at the far end and see `read`.
    let sender = Daemon::start("sender");
    let reader = Daemon::start_with("reader", &[("HIVEMIND_READ_RECEIPTS", "true")]);
    let mut away = Daemon::start_with("away", &[("HIVEMIND_READ_RECEIPTS", "true")]);
    pair(&sender, &reader);
    pair(&sender, &away);

    let reader_id = reader.node_id();
    let away_id = away.node_id();
    away.stop();

    sender.run(&[
        "send",
        &reader_id,
        &away_id,
        "-s",
        "tell me when you read it",
        "--",
        "no hurry",
    ]);

    // One machine has it and the other cannot, so nothing is read yet.
    let waiting = until(
        &sender,
        "tell me when you read it",
        "delivered to exactly one of the two",
        |state| state["delivered"] == 1,
    );
    assert_eq!(waiting["read"], 0);

    // She opens it. The receipt comes back over the same mutual TLS the
    // message went out on, and marks her entry and nobody else's.
    let arrived = reader.wait_for("tell me when you read it");
    reader.run(&["read", arrived["id"].as_str().expect("an id")]);

    let one_read = until(
        &sender,
        "tell me when you read it",
        "read by one of the two",
        |state| state["read"] == 1,
    );
    assert_eq!(
        one_read["state"], "queued",
        "the machine that is off still has not had it: {one_read}"
    );

    let id = json(&sender.run(&["sent", "--json"]))[0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let opened = json(&sender.run(&["read", &id, "--json"]));
    let lines = opened["recipients"].as_array().expect("a line each");
    let hers = lines
        .iter()
        .find(|line| line["node"] == reader_id)
        .expect("the machine that read it");
    assert_eq!(hers["state"], "read");
    assert!(hers["read_at"].is_string(), "with the time she read it");
    assert_eq!(
        lines
            .iter()
            .find(|line| line["node"] == away_id)
            .expect("the machine that is off")["state"],
        "queued"
    );

    // Monday morning: the other laptop comes back, takes the message and is
    // read too. Only then is the message as a whole read.
    away.restart("away");
    let arrived = away.wait_for("tell me when you read it");
    away.run(&["read", arrived["id"].as_str().expect("an id")]);

    let both = until(
        &sender,
        "tell me when you read it",
        "read by both",
        |state| state["state"] == "read",
    );
    assert_eq!(both["read"], 2, "{both}");
    assert_eq!(both["delivered"], 2);
}

#[test]
fn nothing_comes_back_from_a_machine_that_has_not_turned_receipts_on() {
    // The default, and ADR 0016's whole argument: that a machine took a
    // message is a fact about a daemon, and that somebody opened it is a fact
    // about a person. The message is still reported delivered.
    let sender = Daemon::start("sender");
    let quiet = Daemon::start("quiet");
    pair(&sender, &quiet);

    sender.run(&[
        "send",
        &quiet.node_id(),
        "-s",
        "no receipt for this",
        "--",
        "hello",
    ]);

    let arrived = quiet.wait_for("no receipt for this");
    quiet.run(&["read", arrived["id"].as_str().expect("an id")]);

    let delivered = until(&sender, "no receipt for this", "delivered", |state| {
        state["state"] == "delivered"
    });
    assert_eq!(delivered["read"], 0, "{delivered}");

    // Long enough for a receipt to have been carried if one had been queued:
    // the courier ticks every second and the peer is up and reachable.
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(
        state_of(&sender, "no receipt for this")["read"],
        0,
        "reading it must not have been reported"
    );
}

/// The receipts this machine still owes, read off its disk.
fn owed_receipts(machine: &Daemon) -> Vec<serde_json::Value> {
    let dir = machine.home().join("receipts");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect()
}

#[test]
fn a_receipt_waits_on_disk_while_the_sender_is_off_and_clears_when_it_lands() {
    // ADR 0016's reason for a queue rather than one attempt: the node owed a
    // receipt may be off for days, exactly as the node owed a message may be.
    // Asserted from the files, because "it was retried" and "it was sent once
    // and lost" look identical from the sender's side.
    let mut sender = Daemon::start("sender");
    let reader = Daemon::start_with("reader", &[("HIVEMIND_READ_RECEIPTS", "true")]);
    pair(&sender, &reader);

    sender.run(&[
        "send",
        &reader.node_id(),
        "-s",
        "read it whenever",
        "--",
        "no hurry",
    ]);
    let arrived = reader.wait_for("read it whenever");
    let id = arrived["id"].as_str().expect("an id").to_owned();

    // She reads it with the sender's laptop shut.
    sender.stop();
    reader.run(&["read", &id]);

    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    let waiting = loop {
        let owed = owed_receipts(&reader);
        if owed
            .first()
            .is_some_and(|r| r["attempts"].as_u64() > Some(0))
        {
            break owed;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the receipt should be queued and retried: {owed:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0]["message"], id);
    assert!(
        waiting[0]["last_error"].is_string(),
        "and it should say what happened: {waiting:?}"
    );

    // The laptop comes back. The receipt goes, and stops being owed.
    sender.restart("sender");
    until(&sender, "read it whenever", "read", |state| {
        state["read"] == 1
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        let owed = owed_receipts(&reader);
        if owed.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a receipt that landed should stop being owed: {owed:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
