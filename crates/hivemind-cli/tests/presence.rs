//! M7's presence tests: real daemons saying hello to each other (SPEC §5.5).
//!
//! These are the claims that only real sockets can settle — a node becoming
//! visible without anybody running a command, a peer list travelling between
//! machines that never met, and a rotated key actually taking a member out.
//! Every one of them is invisible to an in-process test, because in-process
//! there is no round and no clock.

mod daemon;

use std::time::{Duration, Instant};

use daemon::{Daemon, created_code, json};

/// Five seconds is the shortest interval the configuration allows, and these
/// tests wait for two of them. Anything longer would make the file the slowest
/// thing in `just ci` for no extra confidence.
const INTERVAL: &str = "5";

/// A daemon with presence running at [`INTERVAL`].
fn daemon(name: &str) -> Daemon {
    Daemon::start_with(name, &[("HIVEMIND_PRESENCE_INTERVAL", INTERVAL)])
}

/// Wait until `check` holds of this daemon's peer list, or give up.
///
/// Presence is periodic by design, so there is nothing to await — but a test
/// that slept a fixed number of intervals would be both slower and flakier
/// than one that asks.
fn wait_for_peers(
    daemon: &Daemon,
    what: &str,
    check: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_mins(1);
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        last = json(&daemon.run(&["peers", "--json"]));
        if check(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what} never happened; the peer list is {last}");
}

/// Is `id` listed as a member that is online?
fn online(peers: &serde_json::Value, id: &str) -> bool {
    peers.as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["id"] == id && row["paired"] == true && row["online"] == true)
    })
}

/// Is `id` a member at all?
fn member(peers: &serde_json::Value, id: &str) -> bool {
    peers.as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["id"] == id && row["paired"] == true)
    })
}

#[test]
fn a_node_that_is_up_is_seen_as_up_without_anybody_running_anything() {
    // The complaint presence answers. Before this, `last_seen` moved only
    // during a handshake, and a handshake happens once per peer — so on a
    // tailnet, where there is no multicast to re-announce, a machine that
    // came back was invisible until somebody tried to deliver to it.
    let alice = daemon("alice");
    let bob = daemon("bob");

    let code = created_code(&alice.run(&["group", "create"]));
    bob.run(&["pair", &code]);
    bob.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);

    let bob_id = bob.node_id();
    let alice_id = alice.node_id();

    wait_for_peers(&alice, "bob shows up as online for alice", |peers| {
        online(peers, &bob_id)
    });
    wait_for_peers(&bob, "alice shows up as online for bob", |peers| {
        online(peers, &alice_id)
    });
}

#[test]
fn the_group_travels_on_the_hello_so_a_third_machine_needs_no_introduction() {
    // SPEC §5.4's gossip. Alice contacts bob and carol separately; bob and
    // carol are never told about each other by anyone, and mDNS is off, so
    // the only way they can meet is alice's peer list riding on a hello.
    let alice = daemon("alice");
    let bob = daemon("bob");
    let carol = daemon("carol");

    let code = created_code(&alice.run(&["group", "create"]));
    for node in [&bob, &carol] {
        node.run(&["pair", &code]);
        node.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);
    }

    let carol_id = carol.node_id();
    let bob_id = bob.node_id();

    wait_for_peers(&bob, "bob learns carol from alice's peer list", |peers| {
        member(peers, &carol_id)
    });
    wait_for_peers(&carol, "carol learns bob from alice's peer list", |peers| {
        member(peers, &bob_id)
    });
}

#[test]
fn rotating_the_key_drops_the_member_that_kept_the_old_one() {
    // What #50 promised and only presence can deliver: the proof goes in the
    // handshake, which runs once per peer, so nothing ever asked a pinned
    // node for it again. Alice rotates; bob pastes the new code; carol does
    // not. At the next hello carol is refused and drops out.
    let alice = daemon("alice");
    let bob = daemon("bob");
    let carol = daemon("carol");

    let code = created_code(&alice.run(&["group", "create"]));
    for node in [&bob, &carol] {
        node.run(&["pair", &code]);
        node.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);
    }

    let carol_id = carol.node_id();
    let bob_id = bob.node_id();
    wait_for_peers(&alice, "both are online before the rotation", |peers| {
        online(peers, &carol_id) && online(peers, &bob_id)
    });

    // Alice makes a new key. Bob pastes it; carol is left holding the old.
    let rotated = created_code(&alice.run(&["group", "create", "--replace"]));
    bob.run(&["pair", &rotated, "--replace"]);

    wait_for_peers(&alice, "carol is dropped for failing the proof", |peers| {
        !member(peers, &carol_id)
    });

    let peers = json(&alice.run(&["peers", "--json"]));
    assert!(
        member(&peers, &bob_id),
        "bob pasted the new code and stays: {peers}"
    );
    assert!(
        peers
            .as_array()
            .expect("array")
            .iter()
            .any(|row| row["id"] == carol_id && row["paired"] == false),
        "carol is still a node we have met, listed as seen: {peers}"
    );

    // And the point of it all: mail addressed to carol does not leave.
    // A full interval plus change, which is several passes of the delivery
    // worker — it ticks every second — so this is not merely "not yet".
    alice.run(&[
        "send",
        "--subject",
        "after the rotation",
        &carol_id,
        "--",
        "no",
    ]);
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let inbox = json(&carol.run(&["inbox", "--json"]));
        assert!(
            !inbox
                .as_array()
                .expect("array")
                .iter()
                .any(|m| m["subject"] == "after the rotation"),
            "a node outside the group must not receive mail: {inbox}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}
