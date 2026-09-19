//! One real daemon in a temporary home, for the integration tests to drive.
//!
//! Shared by every file under `tests/`, because starting a daemon, waiting for
//! its peer port and stopping it politely is the same job in all of them and
//! was getting copied. Each integration test is its own binary, so anything an
//! individual file does not use looks dead to that binary — hence the allow.

#![allow(dead_code)]

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A daemon in a temporary home, stopped when the test ends.
pub(crate) struct Daemon {
    process: Child,
    port: u16,
    pub(crate) peer_port: u16,
    pub(crate) home: tempfile::TempDir,
    // Held open: the daemon prints several startup lines and would take a
    // SIGPIPE on the next one if this were dropped. Replaced on restart,
    // which is why it is not underscore-prefixed.
    stdout: BufReader<std::process::ChildStdout>,
}

impl Daemon {
    pub(crate) fn start(name: &str) -> Self {
        Self::start_with(name, &[])
    }

    /// Start with extra environment, for the settings a test needs to bend.
    pub(crate) fn start_with(name: &str, extra: &[(&str, &str)]) -> Self {
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
    pub(crate) fn wait_until_ready(&self) {
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

    pub(crate) fn api(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub(crate) fn run(&self, args: &[&str]) -> String {
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
    pub(crate) fn post(&self, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
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
    pub(crate) fn get_bytes(&self, path: &str) -> (u16, Vec<u8>) {
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

    pub(crate) fn node_id(&self) -> String {
        let status = json(&self.run(&["status", "--json"]));
        status["id"].as_str().expect("an id").to_owned()
    }

    pub(crate) fn inbox(&self) -> serde_json::Value {
        json(&self.run(&["inbox", "--json"]))
    }

    /// Poll the inbox until `subject` shows up, or give up.
    ///
    /// Delivery is asynchronous by design (SPEC §8), so there is nothing to
    /// await — but a test that slept a fixed second would be both slower and
    /// flakier than one that asks.
    pub(crate) fn wait_for(&self, subject: &str) -> serde_json::Value {
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
    pub(crate) fn stop(&mut self) {
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
    pub(crate) fn restart(&mut self, name: &str) {
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
pub(crate) fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

pub(crate) fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid json {text:?}: {e}"))
}

/// The group every test daemon is in unless a test says otherwise.
pub(crate) const CODE: &str = "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4";

/// Put two daemons in the same group and have one contact the other
/// (SPEC §6.2). Pasting the code twice is harmless, which is what lets a
/// daemon be paired with several others through this.
pub(crate) fn pair(a: &Daemon, b: &Daemon) {
    a.run(&["pair", CODE]);
    b.run(&["pair", CODE]);
    a.run(&["join", &format!("127.0.0.1:{}", b.peer_port)]);
}

/// The code `hivemind group create` printed: its first line.
pub(crate) fn created_code(output: &str) -> String {
    output.lines().next().expect("a line").trim().to_owned()
}
