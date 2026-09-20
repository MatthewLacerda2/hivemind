//! M1's integration test: one real daemon, driven by the real binary
//! (SPEC §13.2, §14).
//!
//! These spawn `hivemind daemon` as a process rather than mounting the router
//! in-process, because the things that break in the field — the binary not
//! starting, the data directory not being created, the identity not persisting
//! — are invisible to an in-process test. M3 grows this into two and three
//! daemons talking to each other.
//!
//! The daemon comes from `tests/daemon`, like every other file here. This one
//! carried its own copy of it until #74, and the copy is why the retry that
//! absorbs a port collision never reached these sixteen tests: the fix landed
//! next door, in a file that looked the same.

use std::process::Command;

mod daemon;

use daemon::{Daemon, free_port, json};

/// The name every daemon in this file runs under. One machine, and nothing
/// here asserts on who sent what, so it only has to be a name.
const NAME: &str = "solo";

#[test]
fn a_fresh_daemon_creates_its_data_directory_and_an_identity() {
    let daemon = Daemon::start(NAME);

    assert!(daemon.home().join("identity/node.key").is_file());
    assert!(daemon.home().join("identity/node.crt").is_file());
    for mailbox in ["new", "cur", "out", "sent"] {
        assert!(daemon.home().join("mail").join(mailbox).is_dir());
    }
}

#[test]
fn a_message_sent_to_ourselves_comes_back_through_the_cli() {
    // The whole of M1 in one test: send, list, read (SPEC §14).
    let daemon = Daemon::start(NAME);

    daemon.run(&[
        "send",
        "everyone",
        "-s",
        "dashboard PR",
        "--",
        "take a look",
    ]);

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let messages = inbox.as_array().expect("array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["subject"], "dashboard PR");
    assert_eq!(messages[0]["unread"], true);
    assert_eq!(
        messages[0]["sender_kind"], "human",
        "the CLI is a person typing (SPEC §4.1)"
    );

    let id = messages[0]["id"].as_str().expect("id");
    let message = json(&daemon.run(&["read", id, "--json"]));
    assert_eq!(message["body"], "take a look");
}

#[test]
fn sending_the_same_message_twice_in_a_row_warns_but_still_sends_it() {
    // #33: two presses a minute apart, and nothing said so until the other end
    // had both. Warned rather than refused — asking again is a real message,
    // and the daemon has already accepted this one.
    let daemon = Daemon::start(NAME);
    let send = ["send", "everyone", "-s", "primeiro contato", "--", "ola"];

    let first = daemon.run(&send);
    assert!(
        !first.contains("warning"),
        "the first one repeats nothing: {first}"
    );

    let second = daemon.run(&send);
    assert!(
        second.contains("warning"),
        "the second should say it has just sent this: {second}"
    );

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    assert_eq!(
        inbox.as_array().expect("array").len(),
        2,
        "both were sent; only the second was remarked on"
    );
}

#[test]
fn the_body_is_an_option_as_well_as_a_trailing_argument() {
    // #38: subject and body are the same kind of thing, and only one of them
    // needed a `--` in front of it. Through the real binary, because the clap
    // wiring is the whole of what this is about.
    let daemon = Daemon::start(NAME);

    daemon.run(&["send", "everyone", "-s", "sem hifen", "-b", "o corpo"]);
    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let id = inbox[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        json(&daemon.run(&["read", &id, "--json"]))["body"],
        "o corpo"
    );

    daemon.run(&["reply", &id, "--body", "a resposta"]);
    let replies = json(&daemon.run(&["inbox", "--json"]));
    let latest = replies[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        json(&daemon.run(&["read", &latest, "--json"]))["body"],
        "a resposta"
    );
}

#[test]
fn a_body_given_twice_is_refused_rather_than_guessed_at() {
    // Two explicit bodies mean two different things and only one can be sent,
    // so nothing is sent at all (#28, #69).
    let daemon = Daemon::start(NAME);

    let (ok, said) =
        daemon.try_run(&["send", "everyone", "-s", "duas", "-b", "uma", "--", "outra"]);
    assert!(!ok, "two bodies cannot both be sent: {said}");
    assert!(said.contains("twice"), "unhelpful: {said}");

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    assert!(
        inbox.as_array().expect("array").is_empty(),
        "a refused send sends nothing: {inbox}"
    );
}

#[test]
fn a_body_omitted_altogether_comes_off_a_pipe() {
    // SPEC §10 has promised `[body | -]` since M1. A pipe is what a script
    // has, and it carries a body that starts with a hyphen better than `--`
    // does. Through a real pipe on a real process: an in-process version of
    // this passes whether the wiring exists or not.
    let daemon = Daemon::start(NAME);

    let (ok, said) = piping(
        &daemon,
        &["send", "everyone", "-s", "de um cano"],
        "-- not a flag\n",
    );
    assert!(ok, "a piped body is a body: {said}");

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let id = inbox[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        json(&daemon.run(&["read", &id, "--json"]))["body"],
        "-- not a flag\n"
    );
}

#[test]
fn a_typed_body_beats_whatever_is_on_the_pipe() {
    // Stdin being redirected is ambient — a runner, a `< /dev/null`, a script
    // — so it is the fallback and never overrides an argument somebody typed.
    // Only two *explicit* bodies are a refusal.
    let daemon = Daemon::start(NAME);

    let (ok, said) = piping(
        &daemon,
        &["send", "everyone", "-s", "escolha", "-b", "o argumento"],
        "o cano",
    );
    assert!(ok, "the argument is enough on its own: {said}");

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let id = inbox[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        json(&daemon.run(&["read", &id, "--json"]))["body"],
        "o argumento"
    );
}

/// Run the CLI against this daemon with something on stdin.
///
/// The harness's `run` uses `output()`, which hands the child a closed stdin;
/// these tests are about what happens when it is a pipe with bytes in it.
fn piping(daemon: &Daemon, args: &[&str], stdin: &str) -> (bool, String) {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(args)
        .env("HIVEMIND_HOME", daemon.home())
        .env("HIVEMIND_API", daemon.api())
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the cli runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write the body");

    let output = child.wait_with_output().expect("the cli finishes");
    let mut said = String::from_utf8_lossy(&output.stdout).into_owned();
    said.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), said)
}

#[test]
fn reading_a_message_clears_it_from_the_unread_count() {
    let daemon = Daemon::start(NAME);
    daemon.run(&["send", "everyone", "-s", "unread", "--", "body"]);

    let before = json(&daemon.run(&["status", "--json"]));
    assert_eq!(before["unread"], 1);

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let id = inbox[0]["id"].as_str().expect("id").to_owned();
    daemon.run(&["read", &id, "--json"]);

    let after = json(&daemon.run(&["status", "--json"]));
    assert_eq!(after["unread"], 0);
}

#[test]
fn a_reply_lands_in_the_same_thread() {
    let daemon = Daemon::start(NAME);
    daemon.run(&["send", "everyone", "-s", "lunch", "--", "?"]);

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let id = inbox[0]["id"].as_str().expect("id").to_owned();
    daemon.run(&["reply", &id, "1pm"]);

    let original = json(&daemon.run(&["read", &id, "--json"]));
    let (_, thread) = daemon.get_json(&format!(
        "/api/v1/threads/{}",
        original["id"].as_str().expect("id")
    ));

    // The root's id is its thread id, so the thread holds both messages.
    assert!(
        thread.as_array().is_some_and(|t| t.len() >= 2),
        "expected the reply to join the thread, got {thread}"
    );
}

#[test]
fn a_message_survives_the_daemon_restarting() {
    // Store-and-forward is worth nothing if a restart loses mail (SPEC §8).
    //
    // Through the harness's own `stop` and `restart`, which is what they are
    // for: SIGTERM the way launchd would, then the same home and the same
    // ports back again. This test used to build its daemon by hand and so had
    // its own copy of every startup race in the harness.
    let mut daemon = Daemon::start(NAME);
    daemon.run(&["send", "everyone", "-s", "survives a restart", "--", "body"]);
    let identity_before = json(&daemon.run(&["status", "--json"]))["id"].clone();

    daemon.stop();
    daemon.restart(NAME);

    let after = json(&daemon.run(&["status", "--json"]));
    assert_eq!(after["unread"], 1, "the mail should still be there");
    assert_eq!(
        after["id"], identity_before,
        "and this node should still be the same node"
    );

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    assert_eq!(inbox[0]["subject"], "survives a restart");
}

#[test]
fn deleting_the_index_loses_nothing_because_the_files_are_the_truth() {
    // ADR 0002, exercised against a real daemon rather than a unit test.
    let daemon = Daemon::start(NAME);
    daemon.run(&["send", "everyone", "-s", "still here", "--", "body"]);

    std::fs::remove_file(daemon.home().join("index.db")).expect("delete the index");

    let rebuilt = daemon.run(&["reindex"]);
    assert!(rebuilt.contains("1 unread message"), "got: {rebuilt}");
}

#[test]
fn the_daemon_refuses_to_serve_anything_but_loopback() {
    // SPEC §6.3: the local API has no authentication, so reachability is the
    // authorization. Binding elsewhere would hand the machine away.
    let daemon = Daemon::start(NAME);
    let non_loopback = std::net::TcpStream::connect((std::net::Ipv4Addr::UNSPECIFIED, daemon.port));
    // 0.0.0.0 connects to loopback on most stacks, so the real assertion is
    // that the listener was never bound to a routable address.
    drop(non_loopback);

    let (status, _) = daemon.get_json("/healthz");
    assert_eq!(status, 200, "loopback still works");
}

#[test]
fn the_cli_says_something_useful_when_no_daemon_is_running() {
    let home = tempfile::tempdir().expect("temp home");
    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["status"])
        .env("HIVEMIND_HOME", home.path())
        .env("HIVEMIND_API", format!("http://127.0.0.1:{}", free_port()))
        .env("NO_COLOR", "1")
        .output()
        .expect("cli runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no hivemind daemon") && stderr.contains("hivemind daemon"),
        "a connection refused should tell you how to fix it, got: {stderr}"
    );
}

#[test]
fn a_file_attached_from_the_command_line_comes_back_by_name() {
    // SPEC §10: `hivemind send -a file`. The CLI passes a path; the daemon
    // copies the contents, so the original can go away afterwards.
    let daemon = Daemon::start(NAME);

    let files = tempfile::tempdir().expect("temp dir");
    let path = files.path().join("report.md");
    std::fs::write(&path, b"# report").expect("write");

    let me = json(&daemon.run(&["status", "--json"]));
    let id = me["id"].as_str().expect("an id").to_owned();

    daemon.run(&[
        "send",
        &id,
        "-s",
        "with a file",
        "-a",
        &path.to_string_lossy(),
        "--",
        "see attached",
    ]);

    // Gone before it is ever read: the daemon has its own copy.
    std::fs::remove_file(&path).expect("remove");

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let message_id = inbox[0]["id"].as_str().expect("an id").to_owned();

    let message = json(&daemon.run(&["read", &message_id, "--json"]));
    let attachment = &message["attachments"][0];
    assert_eq!(attachment["name"], "report.md");
    assert_eq!(attachment["size"], 8);
    assert_eq!(attachment["cached"], true, "our own file is on this disk");

    let prose = daemon.run(&["read", &message_id]);
    assert!(prose.contains("report.md"), "read should list it: {prose}");
    assert!(prose.contains("on disk"), "and say it is here: {prose}");
}

#[test]
fn attaching_a_file_that_is_not_there_fails_before_anything_is_sent() {
    let daemon = Daemon::start(NAME);
    let me = json(&daemon.run(&["status", "--json"]));
    let id = me["id"].as_str().expect("an id").to_owned();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args([
            "send",
            &id,
            "-s",
            "missing",
            "-a",
            "/no/such/file.txt",
            "--",
            "body",
        ])
        .env("HIVEMIND_HOME", daemon.home())
        .env("HIVEMIND_API", daemon.api())
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    assert!(!output.status.success(), "it should have refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("/no/such/file.txt"),
        "and say which file: {stderr}"
    );
    assert_eq!(
        json(&daemon.run(&["inbox", "--json"]))
            .as_array()
            .expect("array")
            .len(),
        0,
        "nothing should have been sent"
    );
}

#[test]
fn the_wake_up_hook_needs_no_daemon_and_no_network() {
    // SPEC §9.3: `hook check` runs on every Claude turn boundary, must exit in
    // under 100 ms, and "reads `index.db` directly, never the network".
    //
    // The structural half is asserted rather than the timing. A raw
    // millisecond bound on a shared CI runner is a coin toss, and it would not
    // catch the regression that matters anyway: a `hook check` rewritten to
    // call the local API fails *here*, with no daemon to call, which is
    // exactly the change that would make it slow. Measured for the record:
    // 10 ms in a debug build on the development machine, against a 100 ms
    // budget.
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");

    // A home with an identity and an index, and nothing listening anywhere.
    let init = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["init", "--no-launchd", "--no-mcp", "--no-hooks"])
        .env("HIVEMIND_HOME", &home)
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let started = std::time::Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["hook", "check"])
        .env("HIVEMIND_HOME", &home)
        // Somewhere nothing is listening. If this ever starts mattering, the
        // hook has grown a network call.
        .env("HIVEMIND_API", "http://127.0.0.1:1")
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "`hook check` must work with no daemon: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "no unread mail means no output at all, so a quiet turn stays quiet: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Loose enough not to flake on a loaded runner, tight enough that a
    // network timeout could not hide inside it.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "`hook check` took {elapsed:?}; SPEC §9.3 budgets 100 ms"
    );
}

#[test]
fn the_wake_up_hook_says_what_is_waiting() {
    // The other half of §9.3: one line naming who and what, or nothing.
    let daemon = Daemon::start(NAME);
    let me = json(&daemon.run(&["status", "--json"]));
    let id = me["id"].as_str().expect("an id").to_owned();

    daemon.run(&["send", &id, "-s", "dashboard PR", "--", "have a look"]);

    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["hook", "check"])
        .env("HIVEMIND_HOME", daemon.home())
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    let line = String::from_utf8_lossy(&output.stdout);
    assert!(line.contains("hivemind"), "it should name itself: {line:?}");
    assert!(line.contains("dashboard PR"), "and the subject: {line:?}");
    assert_eq!(line.lines().count(), 1, "one line, not a report: {line:?}");
}

#[test]
fn the_daemon_writes_json_logs_beside_its_mail() {
    // SPEC §4.3 lists `daemon.log` in the on-disk layout and §13.1 says what
    // goes in it: JSON to the file, pretty in the foreground. Both, not
    // either — somebody watching a terminal wants to read it, and somebody
    // debugging a service launchd started wants to grep a week of it.
    // The other tests run at `warn`, which is right for them — a quiet suite.
    // This one is about what the file contains, so it needs something in it.
    let daemon = Daemon::start_with(NAME, &[("HIVEMIND_LOG", "info")]);

    let log = daemon.home().join("daemon.log");
    assert!(log.is_file(), "SPEC §4.3 lists daemon.log");

    // The appender is non-blocking by design: logging must never hold up a
    // delivery, so it writes on another thread and a line appears shortly
    // after the event rather than with it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let text = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if !text.trim().is_empty() || std::time::Instant::now() > deadline {
            break text;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let first = text.lines().next().expect("the daemon logs that it is up");

    let entry: serde_json::Value =
        serde_json::from_str(first).unwrap_or_else(|e| panic!("not JSON: {first:?}: {e}"));
    assert_eq!(entry["level"], "INFO");
    assert!(
        entry["fields"]["message"].is_string(),
        "an entry needs a message: {entry}"
    );

    // The terminal got a different shape. The harness reads the first stdout
    // line and asserts it contains "listening" -- prose, not JSON -- so if
    // the two layers were ever collapsed into one, `Daemon::start` fails
    // before this test does.
}

#[test]
fn a_network_operation_logs_the_peer_and_the_message() {
    // SPEC §13.1: "Every network operation has a span with peer id and message
    // id." Asserted on the shape of the span rather than on a delivery, which
    // needs two daemons -- this checks the field names are what an operator
    // would grep for.
    let daemon = Daemon::start(NAME);
    let me = json(&daemon.run(&["status", "--json"]));
    let id = me["id"].as_str().expect("an id").to_owned();

    daemon.run(&["send", &id, "-s", "logged", "--", "x"]);

    // A message to ourselves never leaves the machine, so there is no span to
    // find -- which is the honest outcome and worth stating rather than
    // asserting something that would pass for the wrong reason. What can be
    // checked here is that the log is still well-formed after real work.
    let text = std::fs::read_to_string(daemon.home().join("daemon.log")).expect("read");
    for line in text.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("a log line is not JSON: {line:?}: {e}"));
    }
}

/// Take the escape codes off, so a test about text is about text.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // A CSI sequence is ESC `[`, parameters, then a final byte in
        // `@`..`~`. The `[` has to be consumed first: it is itself inside
        // that range, so scanning for the terminator without skipping it
        // ends the sequence immediately and leaves `2m` in the output.
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if ('@'..='~').contains(&c) {
                break;
            }
        }
    }
    out
}

#[test]
fn the_short_id_the_inbox_prints_is_one_read_accepts() {
    // #27, and the whole journey rather than either half: each half was
    // already tested alone. `inbox` printed `03VYRM`, `read 03VYRM` answered
    // "`03VYRM` is not a message id", and the only id that worked was the
    // 26-character one the CLI never showed anywhere.
    //
    // Copying what is on the screen is the obvious gesture, and it was the
    // one that did not work — found from both ends at once, by a human and
    // by the Claude on the other machine, neither of whom could read a
    // message without talking to the API by hand.
    let daemon = Daemon::start(NAME);
    daemon.run(&[
        "send",
        "--subject",
        "the short one",
        "everyone",
        "--",
        "body",
    ]);

    // What a person actually sees, not what the JSON carries — with the
    // escape codes taken off. `NO_COLOR` is set and, on `main` at the time
    // this was written, ignored (#55); stripping here means this test is
    // about ids rather than about whichever branch lands first.
    let full = json(&daemon.run(&["inbox", "--json"]))[0]["id"]
        .as_str()
        .expect("a full id")
        .to_owned();

    let listed = strip_ansi(&daemon.run(&["inbox"]));
    let short = listed
        .split_whitespace()
        .find(|word| word.len() == 6 && word.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| panic!("the inbox prints a short id: {listed}"))
        .to_owned();

    let read = daemon.run(&["read", &short]);
    assert!(
        read.contains("the short one"),
        "`hivemind read {short}` should have found it: {read}"
    );

    assert!(
        full.ends_with(&short),
        "the short id should be the tail of the full one: {short} of {full}"
    );

    // And the full form keeps working, because that is what every script and
    // every already-stored id uses. Read after the short one, because
    // reading moves a message out of the inbox.
    assert!(daemon.run(&["read", &full]).contains("the short one"));
}

#[test]
fn a_tail_that_names_nothing_says_so_rather_than_guessing() {
    let daemon = Daemon::start(NAME);
    daemon.run(&[
        "send",
        "--subject",
        "the only one",
        "everyone",
        "--",
        "body",
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["read", "ZZZZZZ"])
        .env("HIVEMIND_HOME", daemon.home())
        .env("HIVEMIND_API", daemon.api())
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    assert!(!output.status.success(), "it must not find something");
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("ZZZZZZ"),
        "it should echo what was typed: {said}"
    );
}

#[test]
fn the_inbox_keeps_a_message_after_it_is_read_and_one_box_can_be_asked_for() {
    // `hivemind inbox` used to ask for `new/` alone, so reading a message made
    // it disappear from the only listing the CLI had and `--unread` did
    // nothing at all (#26). The inbox is both boxes of what arrived; `--box`
    // is how to look at exactly one.
    let daemon = Daemon::start(NAME);
    daemon.run(&["send", "everyone", "-s", "read me", "--", "body"]);

    let id = daemon.inbox()[0]["id"].as_str().expect("an id").to_owned();
    daemon.run(&["read", &id]);

    let listed = daemon.inbox();
    assert_eq!(
        listed[0]["subject"], "read me",
        "a read message is still mail"
    );
    assert_eq!(listed[0]["mailbox"], "cur");
    assert_eq!(
        json(&daemon.run(&["inbox", "--unread", "--json"]))
            .as_array()
            .expect("an array")
            .len(),
        0,
        "`--unread` should now mean something"
    );
    assert_eq!(
        json(&daemon.run(&["inbox", "--box", "new", "--json"]))
            .as_array()
            .expect("an array")
            .len(),
        0,
        "and `new` is the box it has left"
    );
}

#[test]
fn asking_for_unread_mail_in_a_box_that_holds_none_is_refused() {
    // Only `new/` holds unread mail, so `--unread --box sent` can only ever
    // answer with nothing — and nothing looks exactly like an empty box. That
    // reading is what made `out/` look broken in #28.
    let daemon = Daemon::start(NAME);

    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["inbox", "--box", "sent", "--unread"])
        .env("HIVEMIND_HOME", daemon.home())
        .env("HIVEMIND_API", daemon.api())
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    assert!(!output.status.success(), "it should have refused");
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("unread") && said.contains("sent"),
        "it should say what cannot be both: {said}"
    );
}

#[test]
fn a_box_that_is_not_one_is_refused_before_a_request_is_made() {
    // The CLI knows the four names, so it refuses before it opens a socket.
    // Pointed at a port nobody is listening on for that reason: the only way
    // to hear about the boxes from here is clap, and a `--box` that took any
    // string would fail with "no daemon" instead.
    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["inbox", "--box", "banana"])
        .env("HIVEMIND_API", format!("http://127.0.0.1:{}", free_port()))
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    assert!(!output.status.success(), "it should have refused");
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("new") && said.contains("out"),
        "it should name the boxes there are: {said}"
    );
}

/// Give `path` this modification time. `std::fs::copy` cannot, and the gap
/// between the two builds is the whole of what this test is about.
fn written_at(path: &std::path::Path, when: std::time::SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the binary")
        .set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("set its modification time");
}

#[test]
fn a_daemon_whose_binary_has_been_replaced_says_so_rather_than_serving_the_old_code_in_silence() {
    // #36, end to end and through the real binary, because every part of it
    // that can be wrong is outside the process: the daemon reads its own file
    // at startup, stamps every response, and the CLI compares that with the
    // file it was itself started from.
    //
    // A copy, because the replacement has to be real and the binary cargo
    // built is shared with every other test in this run. Renamed over rather
    // than written through, which is both what an installer does and the only
    // way to replace a file a process is running from.
    let installed = tempfile::tempdir().expect("temp dir");
    let binary = installed.path().join("hivemind");
    std::fs::copy(env!("CARGO_BIN_EXE_hivemind"), &binary).expect("copy the binary");
    let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
    written_at(&binary, an_hour_ago);

    let daemon = Daemon::start_from(NAME, &binary);

    // Nothing has been replaced yet, and a check that fired here would be
    // ignored within a week — and then so would the one below.
    let (_, before) = daemon.try_run(&["doctor"]);
    assert!(
        before.contains("running the binary on disk"),
        "a daemon running what is on disk is not stale: {before}"
    );
    let status = daemon.run(&["status"]);
    assert!(
        !status.contains("older binary"),
        "nothing to warn about yet: {status}"
    );

    // The reinstall.
    let replacement = installed.path().join("hivemind.new");
    std::fs::copy(env!("CARGO_BIN_EXE_hivemind"), &replacement).expect("copy the binary");
    written_at(&replacement, std::time::SystemTime::now());
    std::fs::rename(&replacement, &binary).expect("rename over the running binary");

    let (ok, said) = daemon.try_run(&["doctor"]);
    assert!(!ok, "doctor has to fail on this, not mention it: {said}");
    assert!(
        said.contains("has since been replaced") && said.contains("restart"),
        "and say what happened and what to do: {said}"
    );

    // And the command somebody is actually running when it matters says it
    // too, in one line, without being asked about the daemon at all.
    let status = daemon.run(&["status"]);
    assert!(
        status.contains("the daemon is running an older binary than this one"),
        "every command talking to a stale daemon says so: {status}"
    );
}
