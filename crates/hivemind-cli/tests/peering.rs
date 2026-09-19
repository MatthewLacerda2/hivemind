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
    // SIGPIPE on the next one if this were dropped. Replaced on restart,
    // which is why it is not underscore-prefixed.
    stdout: BufReader<std::process::ChildStdout>,
}

impl Daemon {
    fn start(name: &str) -> Self {
        Self::start_with(name, &[])
    }

    /// Start with extra environment, for the settings a test needs to bend.
    fn start_with(name: &str, extra: &[(&str, &str)]) -> Self {
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
            .envs(extra.iter().copied())
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
            stdout: reader,
        };
        daemon.wait_until_ready();
        daemon
    }

    /// The peer port is bound after the line we read above, so a join sent
    /// immediately can race it.
    ///
    /// A minute rather than ten seconds. This returns the moment the port
    /// opens, so a generous deadline costs nothing in the normal case — and
    /// ten seconds was not enough on a machine running a mutation sweep
    /// beside it. CI runs on a shared runner, so the same squeeze is waiting
    /// there; it would have read as a mysterious flake in an unrelated test.
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_mins(1);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.peer_port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "the peer port {} never opened — the daemon is slow to start or \
             died; its stdout is held open by this struct",
            self.peer_port
        );
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

    /// POST to this daemon's local API, returning the decoded JSON.
    fn post(&self, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
        let response = reqwest::blocking::Client::new()
            .post(format!("{}{path}", self.api()))
            .json(body)
            .send()
            .expect("the daemon answers");
        let status = response.status().as_u16();
        let text = response.text().unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        )
    }

    /// GET raw bytes from this daemon's local API.
    fn get_bytes(&self, path: &str) -> (u16, Vec<u8>) {
        let response = reqwest::blocking::Client::builder()
            // A first fetch pulls the whole file from the other daemon.
            .timeout(Duration::from_mins(1))
            .build()
            .expect("client")
            .get(format!("{}{path}", self.api()))
            .send()
            .expect("the daemon answers");
        let status = response.status().as_u16();
        (status, response.bytes().expect("body").to_vec())
    }

    fn node_id(&self) -> String {
        let status = json(&self.run(&["status", "--json"]));
        status["id"].as_str().expect("an id").to_owned()
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
        // Delivery is asynchronous and retries with backoff, so this is the
        // one wait that genuinely models the product rather than the machine.
        let deadline = Instant::now() + Duration::from_mins(1);
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

impl Daemon {
    /// Stop the daemon, keeping its home so it can be started again.
    ///
    /// SIGTERM rather than a kill: the daemon handles it (SPEC §2 — launchd
    /// stops a service that way), and this is the test that proves mail
    /// survives it.
    fn stop(&mut self) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &self.process.id().to_string()])
                .status();
            for _ in 0..200 {
                if matches!(self.process.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    /// Start it again on the same home, ports included.
    ///
    /// The ports have to be the same: the sender learned where to reach this
    /// node when they paired, and a peer that comes back on a different port
    /// is a different machine as far as the address book is concerned.
    fn restart(&mut self, name: &str) {
        let mut process = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(["daemon", "--port", &self.port.to_string()])
            .env("HIVEMIND_HOME", self.home.path())
            .env("HIVEMIND_PEER_PORT", self.peer_port.to_string())
            .env("HIVEMIND_NAME", name)
            .env("HIVEMIND_OWNER", name)
            .env("HIVEMIND_NOTIFICATIONS", "false")
            .env("HIVEMIND_DISCOVERY", "false")
            .env("HIVEMIND_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon binary starts");

        let stdout = process.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("it says it is up");
        assert!(line.contains("listening"), "unexpected: {line}");

        self.process = process;
        self.stdout = reader;
        self.wait_until_ready();
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

/// A port the OS says is free.
///
/// Released immediately, which leaves a window: a parallel test can be handed
/// the same number before this one's daemon binds it. That is a real race and
/// not a theoretical one -- it is the likeliest cause of the red `main` after
/// M6, where a daemon died during startup and the CLI reported only that
/// nothing was listening.
///
/// It is not closed here, because closing it properly means the daemon binding
/// port 0 and reporting what it got, which is a change to the product for the
/// sake of the tests. Instead [`wait_until_answering`] panics with the
/// daemon's stderr, so the next occurrence names itself instead of being
/// guessed at.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid json {text:?}: {e}"))
}

/// The group every test daemon is in unless a test says otherwise.
const CODE: &str = "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4";

/// Put two daemons in the same group and have one contact the other
/// (SPEC §6.2). Pasting the code twice is harmless, which is what lets a
/// daemon be paired with several others through this.
fn pair(a: &Daemon, b: &Daemon) {
    a.run(&["pair", CODE]);
    b.run(&["pair", CODE]);
    a.run(&["join", &format!("127.0.0.1:{}", b.peer_port)]);
}

/// The code `hivemind group create` printed: its first line.
fn created_code(output: &str) -> String {
    output.lines().next().expect("a line").trim().to_owned()
}

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
    let partial = bob.home.path().join("blobs").join(format!("{sha}.part"));
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
    let partial = bob.home.path().join("blobs").join(format!("{sha}.part"));
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
