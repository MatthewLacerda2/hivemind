//! M3's integration tests: two and three real daemons, pairing and exchanging
//! mail over mutual TLS (SPEC §13.2, §14).
//!
//! Everything here goes through the shipped binary and a real socket. The
//! things this is for — a certificate that does not verify, a peer port that
//! is not served, an outbox that never empties — are all invisible to an
//! in-process test.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A daemon in a temporary home, stopped when the test ends.
struct Daemon {
    process: Child,
    port: u16,
    peer_port: u16,
    home: tempfile::TempDir,
    // Held open: the daemon prints several startup lines and would take a
    // SIGPIPE on the next one if this were dropped.
    _stdout: BufReader<std::process::ChildStdout>,
}

impl Daemon {
    fn start(name: &str) -> Self {
        let port = free_port();
        let peer_port = free_port();
        let home = tempfile::tempdir().expect("temp home");

        let mut process = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(["daemon", "--port", &port.to_string()])
            .env("HIVEMIND_HOME", home.path())
            .env("HIVEMIND_PEER_PORT", peer_port.to_string())
            .env("HIVEMIND_NAME", name)
            .env("HIVEMIND_OWNER", name)
            .env("HIVEMIND_NOTIFICATIONS", "false")
            // Off, or daemons on this machine would discover each other
            // and every other hivemind on the developer's LAN.
            .env("HIVEMIND_DISCOVERY", "false")
            .env("HIVEMIND_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon binary starts");

        let stdout = process.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the daemon says it is up");
        assert!(line.contains("listening"), "unexpected first line: {line}");

        let daemon = Self {
            process,
            port,
            peer_port,
            home,
            _stdout: reader,
        };
        daemon.wait_until_ready();
        daemon
    }

    /// The peer port is bound after the line we read above, so a join sent
    /// immediately can race it.
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.peer_port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the peer port never opened");
    }

    fn api(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(args)
            .env("HIVEMIND_HOME", self.home.path())
            .env("HIVEMIND_API", self.api())
            .env("NO_COLOR", "1")
            .output()
            .expect("the cli runs");

        assert!(
            output.status.success(),
            "`hivemind {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 output")
    }

    fn node_id(&self) -> String {
        let status = json(&self.run(&["status", "--json"]));
        status["id"].as_str().expect("an id").to_owned()
    }

    fn short_id(&self) -> String {
        let status = json(&self.run(&["status", "--json"]));
        status["short_id"].as_str().expect("a short id").to_owned()
    }

    fn inbox(&self) -> serde_json::Value {
        json(&self.run(&["inbox", "--json"]))
    }

    /// Poll the inbox until `subject` shows up, or give up.
    ///
    /// Delivery is asynchronous by design (SPEC §8), so there is nothing to
    /// await — but a test that slept a fixed second would be both slower and
    /// flakier than one that asks.
    fn wait_for(&self, subject: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let inbox = self.inbox();
            if let Some(found) = inbox
                .as_array()
                .expect("an array")
                .iter()
                .find(|m| m["subject"] == subject)
            {
                return found.clone();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{subject:?} never arrived; inbox is {}", self.inbox());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SIGTERM, not SIGKILL: the daemon handles it, and killing it would
        // lose the coverage profile it writes on the way out.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &self.process.id().to_string()])
                .status();
            for _ in 0..50 {
                if matches!(self.process.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid json {text:?}: {e}"))
}

/// Introduce two daemons and confirm from both sides (SPEC §6.2).
fn pair(a: &Daemon, b: &Daemon) {
    a.run(&["join", &format!("127.0.0.1:{}", b.peer_port)]);
    a.run(&["pair", &b.short_id(), "--yes"]);
    b.run(&["pair", &a.short_id(), "--yes"]);
}

#[test]
fn two_daemons_pair_and_exchange_mail() {
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");

    pair(&alice, &bob);

    let peers = json(&alice.run(&["peers", "--json"]));
    assert_eq!(peers.as_array().expect("array").len(), 1);
    assert_eq!(peers[0]["paired"], true);
    assert_eq!(peers[0]["name"], "bob");

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
fn mail_is_refused_until_both_sides_have_paired() {
    // SPEC §6.2 step 3: one side confirming is not enough.
    let alice = Daemon::start("alice");
    let bob = Daemon::start("bob");

    alice.run(&["join", &format!("127.0.0.1:{}", bob.peer_port)]);
    alice.run(&["pair", &bob.short_id(), "--yes"]);
    // Bob has *not* paired.

    alice.run(&[
        "send",
        &bob.node_id(),
        "-s",
        "too early",
        "--",
        "should not arrive",
    ]);

    // It should still be in the outbox, being refused with 403 not_paired,
    // rather than delivered or dropped.
    std::thread::sleep(Duration::from_secs(2));
    let bobs_inbox = bob.inbox();
    assert!(
        bobs_inbox.as_array().expect("array").is_empty(),
        "bob should have refused it: {bobs_inbox}"
    );

    let outbox = json(&alice.run(&["inbox", "--json"]));
    assert!(
        outbox.as_array().expect("array").is_empty(),
        "alice's own inbox should be empty too"
    );

    // And once Bob does pair, the message that was already sent arrives —
    // that is the whole point of the retry queue.
    bob.run(&["pair", &alice.short_id(), "--yes"]);
    bob.wait_for("too early");
}

#[test]
fn a_node_that_was_never_joined_cannot_send_us_mail() {
    // The peer port is reachable by anyone (ADR 0010); being reachable is not
    // being trusted.
    let alice = Daemon::start("alice");
    let stranger = Daemon::start("stranger");

    stranger.run(&["join", &format!("127.0.0.1:{}", alice.peer_port)]);
    // The stranger pairs from its side only, which is all it can do alone.
    stranger.run(&["pair", &alice.short_id(), "--yes"]);
    stranger.run(&["send", &alice.node_id(), "-s", "let me in", "--", "hello"]);

    std::thread::sleep(Duration::from_secs(2));
    assert!(
        alice.inbox().as_array().expect("array").is_empty(),
        "alice never agreed to anything"
    );

    // Alice sees it as a pending offer, not as a peer.
    let peers = json(&alice.run(&["peers", "--json"]));
    assert_eq!(peers.as_array().expect("array").len(), 1);
    assert_eq!(peers[0]["paired"], false);
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
    assert_eq!(names, vec!["alice"], "there is no gossip in v1 (SPEC §5.4)");
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
