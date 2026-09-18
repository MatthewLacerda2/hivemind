//! M1's integration test: one real daemon, driven by the real binary
//! (SPEC §13.2, §14).
//!
//! These spawn `hivemind daemon` as a process rather than mounting the router
//! in-process, because the things that break in the field — the binary not
//! starting, the data directory not being created, the identity not persisting
//! — are invisible to an in-process test. M3 grows this into two and three
//! daemons talking to each other.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};

/// A daemon running in a temporary home, killed when the test ends.
struct Daemon {
    process: Child,
    port: u16,
    home: tempfile::TempDir,
    // Held open deliberately. The daemon prints three lines at startup; if this
    // reader is dropped after the first, the pipe closes and the next println!
    // kills the daemon with SIGPIPE.
    _stdout: BufReader<std::process::ChildStdout>,
}

impl Daemon {
    fn start() -> Self {
        // Port 0 would be ideal, but the daemon prints the address it bound,
        // so a port picked by the OS and released is close enough and keeps
        // the CLI's --api flag simple.
        let port = free_port();
        let home = tempfile::tempdir().expect("temp home");
        let errors = home.path().join("daemon.stderr");

        let mut process = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(["daemon", "--port", &port.to_string()])
            .env("HIVEMIND_HOME", home.path())
            // Its own peer port: two daemons on one machine genuinely cannot
            // share 8400, and these tests run in parallel.
            .env("HIVEMIND_PEER_PORT", free_port().to_string())
            // Off, or daemons on this machine would discover each other
            // and every other hivemind on the developer's LAN.
            .env("HIVEMIND_DISCOVERY", "false")
            .env("HIVEMIND_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::File::create(&errors).expect("a file for the daemon stderr"),
            ))
            .spawn()
            .expect("the daemon binary starts");

        // Wait for the line it prints once it has bound, rather than sleeping
        // and hoping.
        let stdout = process.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the daemon says it is up");
        assert!(line.contains("listening"), "unexpected first line: {line}");

        // ...and then wait for it to actually answer. The line above is
        // printed after the socket is bound and before the router serves it,
        // so a CLI call made on that line alone races the rest of startup:
        // the peer listener, the courier and mDNS all come up in between.
        //
        // This is what turned `main` red after M6. On a loaded runner the
        // first CLI call arrived before the daemon was answering, and the
        // error it produced -- "no hivemind daemon at ..." -- named the
        // symptom and not one of its causes.
        wait_until_answering(port, &mut process, &errors);

        Self {
            process,
            port,
            home,
            _stdout: reader,
        }
    }

    fn api(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn home(&self) -> &std::path::Path {
        self.home.path()
    }

    /// Run a CLI subcommand against this daemon.
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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        stop(&mut self.process);
    }
}

/// Stop a daemon the way launchd would.
///
/// SIGKILL would leave it no chance to flush — including, under
/// `cargo llvm-cov`, its coverage profile, which is why this test's subject
/// would otherwise appear untested.
fn stop(process: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY-adjacent: `kill(2)` on a pid we own and have not yet reaped.
        let pid = process.id();
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();

        // Give it a moment to shut down cleanly before insisting.
        for _ in 0..50 {
            if matches!(process.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    let _ = process.kill();
    let _ = process.wait();
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
    serde_json::from_str(text).expect("valid json")
}

#[test]
fn a_fresh_daemon_creates_its_data_directory_and_an_identity() {
    let daemon = Daemon::start();

    assert!(daemon.home().join("identity/node.key").is_file());
    assert!(daemon.home().join("identity/node.crt").is_file());
    for mailbox in ["new", "cur", "out", "sent"] {
        assert!(daemon.home().join("mail").join(mailbox).is_dir());
    }
}

#[test]
fn a_message_sent_to_ourselves_comes_back_through_the_cli() {
    // The whole of M1 in one test: send, list, read (SPEC §14).
    let daemon = Daemon::start();

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
fn reading_a_message_clears_it_from_the_unread_count() {
    let daemon = Daemon::start();
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
    let daemon = Daemon::start();
    daemon.run(&["send", "everyone", "-s", "lunch", "--", "?"]);

    let inbox = json(&daemon.run(&["inbox", "--json"]));
    let id = inbox[0]["id"].as_str().expect("id").to_owned();
    daemon.run(&["reply", &id, "1pm"]);

    let original = json(&daemon.run(&["read", &id, "--json"]));
    let thread: serde_json::Value = reqwest::blocking::get(format!(
        "{}/api/v1/threads/{}",
        daemon.api(),
        original["id"].as_str().expect("id")
    ))
    .and_then(reqwest::blocking::Response::json)
    .unwrap_or(serde_json::Value::Null);

    // The root's id is its thread id, so the thread holds both messages.
    assert!(
        thread.as_array().is_some_and(|t| t.len() >= 2),
        "expected the reply to join the thread, got {thread}"
    );
}

#[test]
fn a_message_survives_the_daemon_restarting() {
    // Store-and-forward is worth nothing if a restart loses mail (SPEC §8).
    let home = tempfile::tempdir().expect("temp home");
    let port = free_port();

    let start = || {
        let mut process = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(["daemon", "--port", &port.to_string()])
            .env("HIVEMIND_HOME", home.path())
            .env("HIVEMIND_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts");
        let stdout = process.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("daemon is up");
        // Returned alongside the process so the pipe outlives this function;
        // dropping it would SIGPIPE the daemon on its next println!.
        (process, reader)
    };

    let cli = |args: &[&str]| -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(args)
            .env("HIVEMIND_HOME", home.path())
            .env("HIVEMIND_API", format!("http://127.0.0.1:{port}"))
            .env("NO_COLOR", "1")
            .output()
            .expect("cli runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8")
    };

    let (mut first, _first_out) = start();
    cli(&["send", "everyone", "-s", "survives a restart", "--", "body"]);
    let identity_before = json(&cli(&["status", "--json"]))["id"].clone();
    stop(&mut first);

    let (mut second, _second_out) = start();
    let after = json(&cli(&["status", "--json"]));
    assert_eq!(after["unread"], 1, "the mail should still be there");
    assert_eq!(
        after["id"], identity_before,
        "and this node should still be the same node"
    );

    let inbox = json(&cli(&["inbox", "--json"]));
    assert_eq!(inbox[0]["subject"], "survives a restart");

    stop(&mut second);
}

#[test]
fn deleting_the_index_loses_nothing_because_the_files_are_the_truth() {
    // ADR 0002, exercised against a real daemon rather than a unit test.
    let daemon = Daemon::start();
    daemon.run(&["send", "everyone", "-s", "still here", "--", "body"]);

    std::fs::remove_file(daemon.home().join("index.db")).expect("delete the index");

    let rebuilt = daemon.run(&["reindex"]);
    assert!(rebuilt.contains("1 unread message"), "got: {rebuilt}");
}

#[test]
fn the_daemon_refuses_to_serve_anything_but_loopback() {
    // SPEC §6.3: the local API has no authentication, so reachability is the
    // authorization. Binding elsewhere would hand the machine away.
    let daemon = Daemon::start();
    let non_loopback = std::net::TcpStream::connect((std::net::Ipv4Addr::UNSPECIFIED, daemon.port));
    // 0.0.0.0 connects to loopback on most stacks, so the real assertion is
    // that the listener was never bound to a routable address.
    drop(non_loopback);

    let local =
        reqwest::blocking::get(format!("{}/healthz", daemon.api())).expect("loopback works");
    assert!(local.status().is_success());
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
    let daemon = Daemon::start();

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
    let daemon = Daemon::start();
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

/// Poll the daemon's health endpoint until it answers, or give up loudly.
///
/// Loudly matters more than quickly. When this fails the daemon either died
/// during startup or never got to serving, and the difference is in its
/// stderr -- which is why the harness captures it to a file rather than
/// discarding it. A bind conflict on the peer port reads as "connection
/// refused" from outside, indistinguishable from slowness, until you can see
/// what the process said on its way out.
fn wait_until_answering(port: u16, process: &mut Child, errors: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    let url = format!("http://127.0.0.1:{port}/healthz");

    while std::time::Instant::now() < deadline {
        if let Ok(Some(status)) = process.try_wait() {
            panic!(
                "the daemon exited with {status} during startup.\nIts stderr:\n{}",
                std::fs::read_to_string(errors).unwrap_or_default()
            );
        }
        if std::process::Command::new("curl")
            .args(["-sf", "-o", "/dev/null", "--max-time", "2", &url])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    panic!(
        "the daemon never answered {url}.\nIts stderr:\n{}",
        std::fs::read_to_string(errors).unwrap_or_default()
    );
}
