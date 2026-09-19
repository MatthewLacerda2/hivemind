//! Sessions end to end: a real hook, a real daemon, a real peer (SPEC §9.3).
//!
//! The parsing and the register have unit tests. What only a process can
//! settle is whether the hook reads its stdin at all, whether it finishes
//! inside its budget, and whether what it registered reaches the machine
//! next door — which is the whole point of the feature.

mod daemon;

use std::time::Duration;

use daemon::{Daemon, created_code, json};

/// Presence fast enough that a test does not wait a minute for a hello.
const INTERVAL: &str = "5";

fn daemon(name: &str) -> Daemon {
    Daemon::start_with(name, &[("HIVEMIND_PRESENCE_INTERVAL", INTERVAL)])
}

/// What Claude Code writes to a hook's stdin.
fn payload(session: &str, cwd: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session,
        "cwd": cwd,
        "hook_event_name": event,
        "transcript_path": "/tmp/whatever.jsonl",
    })
}

#[test]
fn a_hook_registers_a_session_renews_it_and_closes_it() {
    let alice = daemon("alice");

    let (status, sessions) = alice.get_json("/api/v1/sessions");
    assert_eq!(status, 200);
    assert_eq!(sessions, serde_json::json!([]), "none before a turn");

    alice.hook(&payload(
        "01JXT-a",
        "/Users/ana/repos/hivemind",
        "SessionStart",
    ));

    let (_, sessions) = alice.get_json("/api/v1/sessions");
    assert_eq!(sessions.as_array().expect("array").len(), 1);
    assert_eq!(
        sessions[0]["label"], "hivemind",
        "the basename of the working directory, not the path"
    );

    // Every turn renews. Without that a long conversation would look like
    // forty Claudes in one repo.
    for _ in 0..5 {
        alice.hook(&payload(
            "01JXT-a",
            "/Users/ana/repos/hivemind",
            "UserPromptSubmit",
        ));
    }
    let (_, sessions) = alice.get_json("/api/v1/sessions");
    assert_eq!(sessions.as_array().expect("array").len(), 1);

    // A second terminal in another repo is a second session.
    alice.hook(&payload(
        "01JXT-b",
        "/Users/ana/repos/scorsese",
        "SessionStart",
    ));
    let (_, sessions) = alice.get_json("/api/v1/sessions");
    assert_eq!(sessions.as_array().expect("array").len(), 2);

    alice.hook(&payload(
        "01JXT-a",
        "/Users/ana/repos/hivemind",
        "SessionEnd",
    ));
    let (_, sessions) = alice.get_json("/api/v1/sessions");
    let labels: Vec<&str> = sessions
        .as_array()
        .expect("array")
        .iter()
        .map(|s| s["label"].as_str().expect("a label"))
        .collect();
    assert_eq!(labels, vec!["scorsese"], "the one that ended is gone");
}

#[test]
fn a_hook_finishes_inside_its_budget_even_with_no_daemon_to_talk_to() {
    // SPEC §9.3 gives the hook 100 ms, and `hook check` is installed once
    // while the daemon is stopped and started freely. Waiting on a socket
    // nobody is listening on is the one way this could stop somebody working.
    let alice = daemon("alice");
    let api = alice.api();
    drop(alice);

    let elapsed = Daemon::hook_against(&api, &payload("01JXT-a", "/tmp/work", "SessionStart"));

    assert!(
        elapsed < Duration::from_secs(2),
        "a hook with no daemon took {elapsed:?}; it must give up, not wait"
    );
}

#[test]
fn the_machine_next_door_sees_what_this_one_is_working_on() {
    // The point of the feature. "Ana's machine is on" is worth less than
    // "there is a Claude in the repo I am asking about", and only the hello
    // can carry the second (SPEC §5.5, §9.3).
    let alice = daemon("alice");
    let bob = daemon("bob");

    let code = created_code(&alice.run(&["group", "create"]));
    bob.run(&["pair", &code]);
    bob.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);

    alice.hook(&payload(
        "01JXT-a",
        "/Users/ana/repos/hivemind",
        "SessionStart",
    ));
    alice.hook(&payload(
        "01JXT-b",
        "/Users/ana/repos/scorsese",
        "SessionStart",
    ));

    let alice_id = alice.node_id();
    let deadline = std::time::Instant::now() + Duration::from_mins(1);
    loop {
        let peers = json(&bob.run(&["peers", "--json"]));
        let row = peers
            .as_array()
            .expect("array")
            .iter()
            .find(|row| row["id"] == alice_id)
            .cloned();

        if let Some(row) = row
            && row["online"] == true
        {
            let mut sessions: Vec<&str> = row["sessions"]
                .as_array()
                .expect("an array of labels")
                .iter()
                .map(|s| s.as_str().expect("a label"))
                .collect();
            sessions.sort_unstable();
            if sessions == ["hivemind", "scorsese"] {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bob never saw alice's sessions; last saw {sessions:?}"
            );
        }

        assert!(
            std::time::Instant::now() < deadline,
            "bob never saw alice online at all"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_session_never_lets_its_id_off_the_machine() {
    // The id is what the hooks here use to renew and close a registration.
    // Another node has nothing it could do with one, and a field that
    // travels is a field that has to keep meaning something.
    let alice = daemon("alice");
    let bob = daemon("bob");

    let code = created_code(&alice.run(&["group", "create"]));
    bob.run(&["pair", &code]);
    bob.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);

    alice.hook(&payload(
        "01JXT-this-must-not-travel",
        "/Users/ana/repos/hivemind",
        "SessionStart",
    ));

    let deadline = std::time::Instant::now() + Duration::from_mins(1);
    while std::time::Instant::now() < deadline {
        let peers = bob.run(&["peers", "--json"]);
        if peers.contains("hivemind") {
            assert!(
                !peers.contains("01JXT-this-must-not-travel"),
                "the session id reached another machine: {peers}"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("bob never saw alice's session at all");
}
