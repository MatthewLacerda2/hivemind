//! M3's integration tests: two and three real daemons, pairing and exchanging
//! mail over mutual TLS (SPEC §13.2, §14).
//!
//! Everything here goes through the shipped binary and a real socket. The
//! things this is for — a certificate that does not verify, a peer port that
//! is not served, an outbox that never empties — are all invisible to an
//! in-process test.

mod daemon;

use std::time::{Duration, Instant};

use daemon::{CODE, Daemon, created_code, json, pair};

#[test]
fn two_daemons_pair_and_exchange_mail() {
    // ADR 0013 end to end: one machine makes the group, the other pastes its
    // code, one contacts the other — and both are peers, with nobody asked to
    // confirm anything on either side.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");

    let code = created_code(&alice.run(&["group", "create"]));
    bob.run(&["pair", &code]);
    bob.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);

    for (daemon, other) in [(&alice, "bob"), (&bob, "alice")] {
        let peers = json(&daemon.run(&["peers", "--json"]));
        assert_eq!(peers.as_array().expect("array").len(), 1);
        assert_eq!(peers[0]["paired"], true, "{other} should be a peer");
        assert_eq!(peers[0]["name"], other);
    }

    alice.run(&[
        "send",
        &bob.node_id(),
        "-s",
        "over the wire",
        "--",
        "this went through TLS",
    ]);

    let arrived = bob.wait_for("over the wire");
    assert_eq!(arrived["from"], alice.node_id());
    assert!(
        bob.run(&["read", arrived["id"].as_str().expect("an id")])
            .contains("this went through TLS"),
        "the body should have survived the trip"
    );
}

#[test]
fn an_address_pointing_at_this_machine_does_not_come_back_out_of_the_file() {
    // #29, over the thing that actually went wrong: two machines paired before
    // #23 each wrote the other down at its *own* loopback, and delivery tries
    // addresses in order, so the first attempt went to the local peer port.
    // The fix on the way in does nothing for a file that already says it, and
    // the file is what a person upgrading brings with them.
    let alice = Daemon::start("alice");
    let mut bob = Daemon::start("bob");
    pair(&alice, &bob);

    let peers = json(&bob.run(&["peers", "--json"]));
    let alices_id = peers[0]["id"].as_str().expect("alice's id").to_owned();
    let poison = format!("127.0.0.1:{}", bob.peer_port);

    // By hand, into the file, exactly as a daemon from before #23 left it.
    bob.stop();
    let path = bob.home().join("peers.toml");
    let book = std::fs::read_to_string(&path).expect("bob's address book");
    let poisoned = format!(
        "{book}\n[[peers.\"{alices_id}\".addrs]]\n\
         host = \"127.0.0.1\"\nport = {}\nsource = \"manual\"\n",
        bob.peer_port
    );
    std::fs::write(&path, &poisoned).expect("poison it");

    bob.restart("bob");
    let peers = json(&bob.run(&["peers", "--json"]));
    let addrs: Vec<String> = peers[0]["addrs"]
        .as_array()
        .expect("addresses")
        .iter()
        .map(|a| a.as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        !addrs.contains(&poison),
        "bob's own peer port must not be a way to reach alice: {addrs:?}"
    );
    assert!(
        addrs.contains(&format!("127.0.0.1:{}", alice.peer_port)),
        "and alice's real address must survive it: {addrs:?}"
    );
    // Mail still flows, which is the half `hivemind peers remove` used to cost.
    alice.run(&[
        "send",
        &bob.node_id(),
        "-s",
        "after the repair",
        "--",
        "hello",
    ]);
    bob.wait_for("after the repair");
}

#[test]
fn one_address_can_be_forgotten_without_forgetting_the_peer() {
    // The escape hatch #29 asks for. `join` is how a second address arrives:
    // the same machine named differently, which is what a peer that moved
    // networks leaves behind. Forgetting the peer to be rid of one of them
    // would spend the whole trust relationship on it.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    pair(&alice, &bob);
    bob.run(&["join", &format!("localhost:{}", alice.peer_port)]);

    let peers = json(&bob.run(&["peers", "--json"]));
    let alices_short = peers[0]["short_id"].as_str().expect("short").to_owned();
    let stale = format!("localhost:{}", alice.peer_port);
    assert!(
        peers[0]["addrs"]
            .as_array()
            .expect("addresses")
            .iter()
            .any(|a| a.as_str() == Some(stale.as_str())),
        "the second name should be in the book to start with: {}",
        peers[0]["addrs"]
    );

    let said = bob.run(&["peers", "forget-addr", &alices_short, &stale]);
    assert!(said.contains(&alices_short), "it should say whose: {said}");

    let peers = json(&bob.run(&["peers", "--json"]));
    let addrs = peers[0]["addrs"].to_string();
    assert!(!addrs.contains("localhost"), "the address goes: {addrs}");
    assert!(
        addrs.contains(&format!("127.0.0.1:{}", alice.peer_port)),
        "the other one stays: {addrs}"
    );
    assert_eq!(peers[0]["paired"], true, "and so does the peer");
    assert!(
        !std::fs::read_to_string(bob.home().join("peers.toml"))
            .expect("read the book")
            .contains("localhost"),
        "and it reaches the file, or the next start has it again"
    );
}

#[test]
fn a_machine_with_another_groups_code_is_seen_and_never_admitted() {
    // The peer port is reachable by anyone (ADR 0010); being reachable is not
    // being trusted. Both sides see the other, and neither can mail it.
    let alice = Daemon::start("alice");
    let stranger = Daemon::start("stranger");
    alice.run(&["pair", CODE]);
    stranger.run(&["group", "create"]);

    let said = stranger.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);
    assert!(said.contains("not in this node's group"), "{said}");

    for daemon in [&alice, &stranger] {
        let peers = json(&daemon.run(&["peers", "--json"]));
        assert_eq!(peers.as_array().expect("array").len(), 1);
        assert_eq!(peers[0]["paired"], false, "seen, never admitted");
    }
}

#[test]
fn a_machine_in_no_group_is_told_so_and_can_then_join() {
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    alice.run(&["pair", CODE]);

    // Bob has no group yet: alice refuses him, and he remembers her as seen.
    bob.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);
    assert_eq!(json(&bob.run(&["peers", "--json"]))[0]["paired"], false);
    assert!(bob.run(&["group"]).contains("not in a group"));

    // Pasting the code greets what was seen, so nothing more is needed: no
    // second `join`, no confirmation on either side.
    bob.run(&["pair", CODE]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while json(&bob.run(&["peers", "--json"]))[0]["paired"] != true {
        assert!(
            Instant::now() < deadline,
            "bob never met alice after pairing"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(json(&alice.run(&["peers", "--json"]))[0]["paired"], true);
}

#[test]
fn everyone_reaches_every_paired_peer_and_nobody_else() {
    // SPEC §8: `everyone` expands at send time to the peers paired *then*.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    let carol = Daemon::start("carol");

    pair(&alice, &bob);
    pair(&alice, &carol);
    // Bob and Carol have never met. Alice is the only one paired with both.

    alice.run(&["send", "everyone", "-s", "standup", "--", "in five"]);

    bob.wait_for("standup");
    carol.wait_for("standup");

    // And Bob still does not know Carol exists.
    let bobs_peers = json(&bob.run(&["peers", "--json"]));
    let names: Vec<&str> = bobs_peers
        .as_array()
        .expect("array")
        .iter()
        .map(|p| p["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        vec!["alice"],
        "the peer list rides on the hello, which is #51; a handshake carries none"
    );
}

#[test]
fn a_late_joiner_does_not_receive_what_everyone_meant_before_it_arrived() {
    // The expansion is stored, so this is decided at send time and cannot
    // change afterwards (SPEC §8).
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    let carol = Daemon::start("carol");

    pair(&alice, &bob);
    alice.run(&["send", "everyone", "-s", "early", "--", "before carol"]);
    bob.wait_for("early");

    pair(&alice, &carol);
    alice.run(&["send", "everyone", "-s", "late", "--", "after carol"]);
    carol.wait_for("late");

    let carols_inbox = carol.inbox();
    let subjects: Vec<&str> = carols_inbox
        .as_array()
        .expect("array")
        .iter()
        .map(|m| m["subject"].as_str().expect("a subject"))
        .collect();
    assert_eq!(
        subjects,
        vec!["late"],
        "carol should not receive mail sent before she was paired"
    );
}

#[test]
fn mail_addressed_to_an_owner_reaches_every_machine_they_have() {
    // SPEC §8: `to: <owner>` expands to all paired peers with that owner.
    let alice = Daemon::start("alice");
    let laptop = Daemon::start("bob");
    let desktop = Daemon::start("bob");

    pair(&alice, &laptop);
    pair(&alice, &desktop);

    alice.run(&[
        "send",
        "bob",
        "-s",
        "both machines",
        "--",
        "wherever you are",
    ]);

    laptop.wait_for("both machines");
    desktop.wait_for("both machines");
}

/// Where a test can leave a file to attach.
fn scratch_file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).expect("write");
    path.to_string_lossy().into_owned()
}

#[test]
fn a_small_attachment_arrives_with_its_message() {
    // SPEC §8: at or below inline_max it ships in the delivery, so it is
    // readable the moment the message is.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    pair(&alice, &bob);

    let files = tempfile::tempdir().expect("temp dir");
    let path = scratch_file(&files, "notes.md", b"# notes\n\nsmall enough to travel");

    let (status, accepted) = alice.post(
        "/api/v1/messages",
        &serde_json::json!({
            "to": [bob.node_id()],
            "subject": "with notes",
            "body": "see attached",
            "attachments": [path],
        }),
    );
    assert_eq!(
        status, 202,
        "sending is accepted, not delivered: {accepted}"
    );

    let arrived = bob.wait_for("with notes");
    let id = arrived["id"].as_str().expect("an id");

    let full = json(&bob.run(&["read", id, "--json"]));
    let sha = full["attachments"][0]["sha256"].as_str().expect("a digest");

    let (status, bytes) = bob.get_bytes(&format!("/api/v1/messages/{id}/attachments/{sha}"));
    assert_eq!(status, 200);
    assert_eq!(bytes, b"# notes\n\nsmall enough to travel");
}

#[test]
fn a_large_attachment_is_fetched_on_first_access_and_resumes_after_an_interruption() {
    // SPEC §13.2 asks for exactly this: kill the blob transfer mid-way and
    // assert it resumes with a range request rather than starting again.
    // Alice's inline limit is below the file, so it ships as a ref and bob
    // fetches it on first access.
    let alice = Daemon::start_with("alice", &[("HIVEMIND_INLINE_MAX_BYTES", "1024")]);
    let bob = Daemon::start("bob");
    pair(&alice, &bob);

    let files = tempfile::tempdir().expect("temp dir");
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let path = scratch_file(&files, "big.bin", &content);

    let (status, _) = alice.post(
        "/api/v1/messages",
        &serde_json::json!({
            "to": [bob.node_id()],
            "subject": "something large",
            "body": "fetch it when you want it",
            "attachments": [path],
        }),
    );
    assert_eq!(status, 202);

    let arrived = bob.wait_for("something large");
    let id = arrived["id"].as_str().expect("an id").to_owned();
    let full = json(&bob.run(&["read", &id, "--json"]));
    let attachment = &full["attachments"][0];
    let sha = attachment["sha256"].as_str().expect("a digest").to_owned();

    assert_eq!(attachment["inline"], false, "too large to have travelled");
    assert_eq!(
        attachment["size"].as_u64().expect("a size"),
        content.len() as u64
    );

    // Interrupt it: put most of the file in place as a partial, exactly as a
    // transfer killed part-way would leave it, then let the fetch finish.
    let partial = bob.home().join("blobs").join(format!("{sha}.part"));
    std::fs::create_dir_all(partial.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&partial, &content[..200_000]).expect("write a partial");

    let (status, bytes) = bob.get_bytes(&format!("/api/v1/messages/{id}/attachments/{sha}"));

    assert_eq!(status, 200);
    assert_eq!(
        bytes.len(),
        content.len(),
        "the whole file should be there after resuming"
    );
    assert_eq!(bytes, content, "and it should be the right bytes");
    assert!(
        !partial.exists(),
        "the partial should have been promoted, not left behind"
    );
}

#[test]
fn a_partial_that_does_not_match_is_not_served_as_if_it_did() {
    // Resuming from a corrupt prefix would produce a file that is not what the
    // signed message names, so the fetch must fail rather than hand it over.
    let alice = Daemon::start_with("alice", &[("HIVEMIND_INLINE_MAX_BYTES", "1024")]);
    let bob = Daemon::start("bob");
    pair(&alice, &bob);

    let files = tempfile::tempdir().expect("temp dir");
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let path = scratch_file(&files, "big.bin", &content);

    alice.post(
        "/api/v1/messages",
        &serde_json::json!({
            "to": [bob.node_id()],
            "subject": "verify me",
            "body": "x",
            "attachments": [path],
        }),
    );

    let arrived = bob.wait_for("verify me");
    let id = arrived["id"].as_str().expect("an id").to_owned();
    let full = json(&bob.run(&["read", &id, "--json"]));
    let sha = full["attachments"][0]["sha256"]
        .as_str()
        .expect("a digest")
        .to_owned();

    // A prefix of the wrong length, so resuming lands at the wrong offset.
    let partial = bob.home().join("blobs").join(format!("{sha}.part"));
    std::fs::create_dir_all(partial.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&partial, vec![0u8; 200_000]).expect("write a bad partial");

    let (status, _) = bob.get_bytes(&format!("/api/v1/messages/{id}/attachments/{sha}"));
    assert_eq!(
        status, 500,
        "a file that does not hash to what was signed is not served"
    );
    assert!(
        !partial.exists(),
        "and the bad partial must not be left to be resumed"
    );
}

#[test]
fn a_laptop_that_was_closed_receives_what_was_sent_while_it_slept() {
    // SPEC §8, and the sentence the whole project rests on: "A laptop that
    // comes to the office on Monday receives Friday's mail." Every other test
    // here has both daemons up the whole time, so nothing was checking it.
    let alice = Daemon::start("alice");
    let mut bob = Daemon::start("bob");
    pair(&alice, &bob);

    let bobs_id = bob.node_id();
    bob.stop();

    alice.run(&[
        "send",
        &bobs_id,
        "-s",
        "friday afternoon",
        "--",
        "read this on monday",
    ]);

    // It should be waiting, not lost and not delivered. `status` reports the
    // outbox depth, which is the daemon's own account of what it still owes.
    let mut waited = 0;
    let outstanding = loop {
        let status = json(&alice.run(&["status", "--json"]));
        let outbox = status["outbox"].as_u64().expect("an outbox count");
        if outbox > 0 || waited > 50 {
            break outbox;
        }
        std::thread::sleep(Duration::from_millis(100));
        waited += 1;
    };
    assert_eq!(outstanding, 1, "alice should still owe bob one message");

    // Monday.
    bob.restart("bob");
    let arrived = bob.wait_for("friday afternoon");
    assert_eq!(arrived["from"], alice.node_id());

    // And alice should stop owing it, which is the other half: a message that
    // arrives but never leaves the outbox is a message that will be delivered
    // again for ever.
    let deadline = Instant::now() + Duration::from_mins(1);
    while Instant::now() < deadline {
        let status = json(&alice.run(&["status", "--json"]));
        if status["outbox"].as_u64() == Some(0) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "the message arrived but alice still has it in her outbox: {}",
        alice.run(&["status", "--json"])
    );
}

#[test]
fn a_reply_to_another_machine_stays_in_the_same_thread() {
    // Threading is tested on one daemon; this is the half that crosses a wire,
    // where `in_reply_to` has to survive being signed, delivered and re-read.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");
    pair(&alice, &bob);

    alice.run(&[
        "send",
        &bob.node_id(),
        "-s",
        "a question",
        "--",
        "what time?",
    ]);
    let question = bob.wait_for("a question");
    let question_id = question["id"].as_str().expect("an id").to_owned();

    bob.run(&["reply", &question_id, "--", "one o'clock"]);
    let answer = alice.wait_for("Re: a question");

    assert_eq!(
        answer["thread_id"], question["thread_id"],
        "the reply should have joined the thread it answers"
    );
    assert_eq!(answer["from"], bob.node_id());
}

#[test]
fn a_message_to_a_machine_that_is_off_waits_in_out_and_then_shows_as_sent() {
    // The question #26 was filed for: "I sent it, did it arrive?". The answer
    // lives in `out/` while the other machine is off, and until now the only
    // way to look at it was `curl`.
    let alice = Daemon::start("alice");
    let mut bob = Daemon::start("bob");
    pair(&alice, &bob);

    let bobs_id = bob.node_id();
    bob.stop();

    alice.run(&[
        "send",
        &bobs_id,
        "-s",
        "did it arrive",
        "--",
        "asking for a friend",
    ]);

    // Waiting: in `out`, and said to be waiting rather than listed as if it
    // had landed.
    let waiting = alice.wait_for_sent("did it arrive", "out");
    assert_eq!(waiting["mailbox"], "out");
    let prose = alice.run(&["sent"]);
    assert!(
        prose.contains("waiting"),
        "a message still going out should say so: {prose}"
    );

    // `status` is the shorter question, and the depth of `out/` is the part of
    // it somebody is looking for when mail seems stuck.
    let standing = alice.run(&["status"]);
    assert!(
        standing.contains("1 message still going out"),
        "status should account for what is owed: {standing}"
    );

    // And it is not mail that arrived here, which is the other half of having
    // four boxes at all.
    assert_eq!(
        alice.inbox().as_array().expect("an array").len(),
        0,
        "what we sent is not what arrived"
    );
    assert_eq!(
        json(&alice.run(&["inbox", "--box", "out", "--json"]))[0]["subject"],
        "did it arrive",
        "`--box out` is the same message the shorthand shows"
    );

    // Monday.
    bob.restart("bob");
    bob.wait_for("did it arrive");

    let delivered = alice.wait_for_sent("did it arrive", "sent");
    assert_eq!(
        delivered["mailbox"], "sent",
        "delivered to everyone means it has left `out`"
    );
}
