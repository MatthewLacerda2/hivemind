//! One real daemon in a temporary home, for the integration tests to drive.
//!
//! **The one harness.** Every file under `tests/` starts its daemons through
//! this, because starting one, waiting until it answers, retrying a port
//! collision and stopping it politely is the same job in all of them. It was
//! copied three ways once, and the copies are exactly why the retry below
//! reached one of them and not the other two (#74). A test that needs
//! something this does not do grows a parameter here rather than a fourth
//! copy.
//!
//! The entry points are [`Daemon::start`], [`Daemon::start_with`] for a test
//! that has to bend a setting, [`Daemon::start_with_first_ports`], which
//! exists so the retry can be exercised rather than presumed, and
//! [`Daemon::start_from`], for the one test that has to replace the binary
//! underneath a running daemon and so cannot use the one cargo built.
//!
//! All of it is safe inside a `#[tokio::test]`, which is what `mcp_tools.rs`
//! needs. That was not always true: [`Daemon::post`], [`Daemon::get_json`] and
//! [`Daemon::get_bytes`] went through `reqwest::blocking`, which builds a
//! runtime of its own and panics when it is dropped in an async context, so
//! the harness carried a warning telling an async test not to call them. All
//! three speak HTTP over a `TcpStream` now, like [`Daemon::answers_locally`]
//! already did, and `tests/harness.rs` drives them from a `#[tokio::test]` so
//! the claim is exercised rather than merely written (#77).
//!
//! Each integration test is its own binary, so anything an individual file
//! does not use looks dead to that binary — hence the allow.

#![allow(dead_code)]

mod http;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A daemon in a temporary home, stopped when the test ends.
pub(crate) struct Daemon {
    process: Child,
    pub(crate) port: u16,
    pub(crate) peer_port: u16,
    // Reached through `home()`, so that the TempDir's lifetime stays this
    // struct's business and a test only ever sees a path.
    home: tempfile::TempDir,
    // Where the daemon's stderr went. A daemon that died binding a port reads
    // as "connection refused" from outside, indistinguishable from a slow one,
    // until you can see what it said on the way out — so it is kept and
    // quoted rather than discarded.
    errors: PathBuf,
    // Held open: the daemon prints several startup lines and would take a
    // SIGPIPE on the next one if this were dropped. Replaced on restart,
    // which is why it is not underscore-prefixed.
    stdout: BufReader<std::process::ChildStdout>,
    // Which `hivemind` this daemon is, and the one its CLI calls go through.
    // Almost always the binary cargo built; a copy only for the test that
    // replaces it while the daemon runs.
    binary: PathBuf,
}

/// The binary cargo built for this test run.
fn built() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hivemind"))
}

impl Daemon {
    pub(crate) fn start(name: &str) -> Self {
        Self::start_with(name, &[])
    }

    /// Start with extra environment, for the settings a test needs to bend.
    pub(crate) fn start_with(name: &str, extra: &[(&str, &str)]) -> Self {
        Self::start_retrying(name, extra, None, &built())
    }

    /// Start from a particular `hivemind`, which its CLI calls also go through.
    ///
    /// For #36 and nothing else: the only way to watch a daemon go stale is to
    /// replace the file it started from, and the file cargo built is shared
    /// with every other test in this run.
    pub(crate) fn start_from(name: &str, binary: &Path) -> Self {
        Self::start_retrying(name, &[], None, binary)
    }

    /// Start with the first attempt forced onto these ports.
    ///
    /// The retry is invisible from outside unless the first attempt can be
    /// made to fail, so this is how `tests/harness.rs` occupies a port and
    /// watches the daemon come up on the next attempt anyway. Nothing else
    /// should want it: a test that picks its own ports is reintroducing the
    /// race this harness exists to absorb.
    pub(crate) fn start_with_first_ports(name: &str, port: u16, peer_port: u16) -> Self {
        Self::start_retrying(name, &[], Some((port, peer_port)), &built())
    }

    /// Start, retrying on a port collision.
    ///
    /// [`free_port`] cannot reserve anything — it asks the OS for a free port
    /// and closes it again — so two daemons starting at once can be handed the
    /// same number, and the loser exits during startup. Retrying with fresh
    /// numbers turns that from a flake in whichever test drew second into two
    /// seconds of nothing.
    fn start_retrying(
        name: &str,
        extra: &[(&str, &str)],
        first: Option<(u16, u16)>,
        binary: &Path,
    ) -> Self {
        // Three, because a collision is already unlikely and three in a row
        // is not a race any more — it is something else, and it should say so
        // rather than spin.
        for attempt in 1..=3 {
            let ports = first
                .filter(|_| attempt == 1)
                .unwrap_or_else(|| (free_port(), free_port()));
            match Self::try_start(name, extra, ports, binary) {
                Ok(daemon) => return daemon,
                Err(why) => eprintln!("daemon start attempt {attempt} failed: {why}"),
            }
        }
        panic!("three daemons in a row failed to start; this is not a port collision");
    }

    /// One attempt, which fails rather than panics so the caller can retry.
    fn try_start(
        name: &str,
        extra: &[(&str, &str)],
        (port, peer_port): (u16, u16),
        binary: &Path,
    ) -> Result<Self, String> {
        let home = tempfile::tempdir().expect("temp home");
        let errors = home.path().join("daemon.stderr");

        let mut process = Command::new(binary)
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
            .stderr(Stdio::from(
                std::fs::File::create(&errors).expect("a file for the daemon stderr"),
            ))
            .spawn()
            .expect("the daemon binary starts");

        let stdout = process.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        if let Err(why) = wait_for_listening(&mut reader) {
            // It died before saying it, which is what binding a taken local
            // API port looks like from here.
            let _ = process.kill();
            let _ = process.wait();
            let said = complaint(&errors);
            return Err(format!("{why}{said}"));
        }

        let mut daemon = Self {
            process,
            port,
            peer_port,
            home,
            errors,
            stdout: reader,
            binary: binary.to_path_buf(),
        };
        daemon.wait_until_ready()?;
        Ok(daemon)
    }

    /// Wait until **this** daemon answers, or say why it never will.
    ///
    /// Both ports, because they prove different things and the peer port
    /// alone proved the wrong one. A TCP connect there says *somebody* is
    /// listening — and when two tests are handed the same port number, the
    /// somebody can be the other daemon. Ours then failed to bind and
    /// exited, this check passed anyway, and the failure surfaced later as
    /// "no hivemind daemon at 127.0.0.1:43695" from an unrelated CLI call.
    /// That is how it read on `main` after #52.
    ///
    /// So: the loopback API has to answer an actual request, which only our
    /// process can do, and a child that has exited ends the wait at once
    /// rather than at the deadline.
    ///
    /// A minute rather than ten seconds. This returns the moment both are
    /// up, so a generous deadline costs nothing in the normal case — and ten
    /// seconds was not enough on a machine running a mutation sweep beside
    /// it. CI runs on a shared runner, so the same squeeze is waiting there.
    fn wait_until_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_mins(1);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.process.try_wait() {
                return Err(format!(
                    "it exited during startup with {status}{}",
                    complaint(&self.errors)
                ));
            }
            if self.answers_locally() && self.peer_port_open() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!(
            "ports {} and {} were not both up within the deadline{}",
            self.port,
            self.peer_port,
            complaint(&self.errors)
        ))
    }

    /// Does our own loopback API answer? Only our process can.
    ///
    /// Polled every 20ms during start-up, so a refused connection is an
    /// ordinary answer here rather than a failure: the daemon has not got
    /// there yet.
    fn answers_locally(&self) -> bool {
        http::request(self.port, "GET", "/healthz", None, Duration::from_secs(2))
            .is_ok_and(|response| response.status == 200)
    }

    /// Is anything listening on the peer port? By now that is us.
    fn peer_port_open(&self) -> bool {
        std::net::TcpStream::connect(("127.0.0.1", self.peer_port)).is_ok()
    }

    pub(crate) fn api(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Where an MCP client connects, which is the same router one path down.
    pub(crate) fn mcp_url(&self) -> String {
        format!("{}/mcp", self.api())
    }

    pub(crate) fn home(&self) -> &Path {
        self.home.path()
    }

    pub(crate) fn run(&self, args: &[&str]) -> String {
        let (ok, said) = self.try_run(args);
        assert!(ok, "`hivemind {}` failed: {said}", args.join(" "));
        said
    }

    /// The same, for a command whose non-zero exit is the point rather than a
    /// failure: `doctor` says so when it has found something to fix.
    ///
    /// Returns whether it succeeded, and everything it said — stdout and
    /// stderr together, because which stream a complaint came out of is not
    /// what any test here is about.
    pub(crate) fn try_run(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(&self.binary)
            .args(args)
            .env("HIVEMIND_HOME", self.home())
            .env("HIVEMIND_API", self.api())
            .env("NO_COLOR", "1")
            .output()
            .expect("the cli runs");

        let mut said = String::from_utf8_lossy(&output.stdout).into_owned();
        said.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), said)
    }

    /// Run a hook against this daemon, feeding it what Claude Code would.
    ///
    /// Through the real binary and the real stdin, because the thing worth
    /// testing is that a hook reads its payload at all — every in-process
    /// version of this passes whether or not the wiring exists.
    pub(crate) fn hook(&self, payload: &serde_json::Value) -> std::time::Duration {
        Self::hook_against(&self.api(), payload)
    }

    /// The same, against an address rather than a running daemon — so a test
    /// can point one at a port nobody is listening on.
    pub(crate) fn hook_against(api: &str, payload: &serde_json::Value) -> std::time::Duration {
        let home = tempfile::tempdir().expect("temp home");
        let started = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .args(["hook", "check"])
            .env("HIVEMIND_HOME", home.path())
            .env("HIVEMIND_API", api)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the cli runs");

        {
            use std::io::Write as _;
            let mut stdin = child.stdin.take().expect("stdin");
            stdin
                .write_all(payload.to_string().as_bytes())
                .expect("write the payload");
        }

        let output = child.wait_with_output().expect("the hook finishes");
        assert!(
            output.status.success(),
            "a hook must never fail: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        started.elapsed()
    }

    /// POST to this daemon's local API, returning the decoded JSON.
    ///
    /// A body that is not JSON decodes to `Null` rather than panicking: the
    /// status code is half of what these tests assert, and an empty 204 is a
    /// legitimate answer to assert on.
    pub(crate) fn post(&self, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
        let encoded = body.to_string();
        let response = http::request(
            self.port,
            "POST",
            path,
            Some(encoded.as_bytes()),
            Duration::from_secs(30),
        )
        .expect("the daemon answers");
        (response.status, decode(&response.body))
    }

    /// GET from this daemon's local API, returning the decoded JSON.
    pub(crate) fn get_json(&self, path: &str) -> (u16, serde_json::Value) {
        let response = http::request(self.port, "GET", path, None, Duration::from_secs(30))
            .expect("the daemon answers");
        (response.status, decode(&response.body))
    }

    /// GET raw bytes from this daemon's local API.
    ///
    /// Bytes and not text: this is how an attachment is fetched, and a blob
    /// read as UTF-8 and re-encoded would come back as replacement
    /// characters rather than as itself.
    pub(crate) fn get_bytes(&self, path: &str) -> (u16, Vec<u8>) {
        // A minute, because a first fetch pulls the whole file from the other
        // daemon before a byte of this response is written.
        let response = http::request(self.port, "GET", path, None, Duration::from_mins(1))
            .expect("the daemon answers");
        (response.status, response.body)
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

    /// The rows `hivemind sent` prints: `out/` and `sent/` together.
    pub(crate) fn sent(&self) -> serde_json::Value {
        json(&self.run(&["sent", "--json"]))
    }

    /// Poll what we sent until `subject` is sitting in `mailbox`.
    ///
    /// Two states rather than one: a message is in `out` until every
    /// recipient has it and in `sent` afterwards, and a test that only waited
    /// for the second could not tell "still going" from "gone missing".
    pub(crate) fn wait_for_sent(&self, subject: &str, mailbox: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_mins(1);
        while Instant::now() < deadline {
            let sent = self.sent();
            if let Some(found) = sent
                .as_array()
                .expect("an array")
                .iter()
                .find(|m| m["subject"] == subject && m["mailbox"] == mailbox)
            {
                return found.clone();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "{subject:?} never reached {mailbox:?}; what we sent is {}",
            self.sent()
        );
    }
}

impl Daemon {
    /// Stop the daemon, keeping its home so it can be started again.
    ///
    /// SIGTERM rather than a kill: the daemon handles it (SPEC §2 — launchd
    /// stops a service that way), and this is the test that proves mail
    /// survives it.
    pub(crate) fn stop(&mut self) {
        if self.terminate_within(Duration::from_secs(4)).is_some() {
            return;
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    /// SIGTERM it and say how long it took to go, or `None` if it never did.
    ///
    /// The number is the point, and it is why this is not folded into
    /// [`Self::stop`]: `stop` kills whatever SIGTERM did not, which is right
    /// for a test that only needs the daemon gone — and which makes a daemon
    /// that *ignores* SIGTERM indistinguishable from one that obeys it. One
    /// holding an open event stream ignored it for ever (#108), and a test
    /// about that has to be able to tell.
    ///
    /// Bounded, and it never waits on a process that is not going: the caller
    /// gets `None` and says what it was waiting for, while `Drop` still kills
    /// the child. Nothing here can hang.
    pub(crate) fn terminate_within(&mut self, within: Duration) -> Option<Duration> {
        let started = Instant::now();
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &self.process.id().to_string()])
                // A start that failed has already reaped its child, so kill(1)
                // would print "No such process" into the middle of the test
                // output and name a pid nobody is looking for.
                .stderr(Stdio::null())
                .status();
            while started.elapsed() < within {
                if matches!(self.process.try_wait(), Ok(Some(_))) {
                    return Some(started.elapsed());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        None
    }

    /// Kill the daemon outright, the way a crash or a lost battery does.
    ///
    /// Not [`Self::stop`], which is SIGTERM and therefore the polite path: a
    /// graceful shutdown waits for connections that are still open, and an
    /// event stream is open by design. A test about what a client does when the
    /// daemon *dies* wants it dead rather than draining (#40).
    pub(crate) fn kill(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    /// Start it again on the same home, ports included.
    ///
    /// The ports have to be the same: the sender learned where to reach this
    /// node when they paired, and a peer that comes back on a different port
    /// is a different machine as far as the address book is concerned.
    pub(crate) fn restart(&mut self, name: &str) {
        let mut process = Command::new(&self.binary)
            .args(["daemon", "--port", &self.port.to_string()])
            .env("HIVEMIND_HOME", self.home())
            .env("HIVEMIND_PEER_PORT", self.peer_port.to_string())
            .env("HIVEMIND_NAME", name)
            .env("HIVEMIND_OWNER", name)
            .env("HIVEMIND_NOTIFICATIONS", "false")
            .env("HIVEMIND_DISCOVERY", "false")
            .env("HIVEMIND_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::File::create(&self.errors).expect("a file for the daemon stderr"),
            ))
            .spawn()
            .expect("the daemon binary starts");

        let stdout = process.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        if let Err(why) = wait_for_listening(&mut reader) {
            panic!("{why}{}", complaint(&self.errors));
        }

        self.process = process;
        self.stdout = reader;
        // A restart reuses the same ports deliberately — the sender learned
        // where this node was when they paired — so there is nothing to
        // retry with. If it will not come back, the test should say so here
        // rather than in whatever it does next.
        self.wait_until_ready()
            .expect("the daemon comes back on the same ports");
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
                // A start that failed has already reaped its child, so kill(1)
                // would print "No such process" into the middle of the test
                // output and name a pid nobody is looking for.
                .stderr(Stdio::null())
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
/// not a theoretical one — it red-lit `main` after M6 and again on a
/// documentation-only branch (#74), both times as a daemon that died during
/// startup while the CLI reported only that nothing was listening.
///
/// The window is not closed here, because closing it properly means the daemon
/// binding port 0 and reporting what it got, which is a change to the product
/// for the sake of the tests. `start_retrying` absorbs it instead,
/// and a start that fails three times over quotes the daemon's stderr so the
/// next occurrence names itself rather than being guessed at.
/// Read the daemon's stdout until it says it is listening.
///
/// Not "the first line it prints": a warning on the way up arrives before the
/// greeting — a `peers.toml` holding an address that points at this node logs
/// one (#29) — and reading a single line turned that into "it never said it
/// was listening", which is the same thing a taken port says. Bounded, so a
/// daemon that talks without ever coming up still fails rather than hanging.
fn wait_for_listening(reader: &mut BufReader<std::process::ChildStdout>) -> Result<(), String> {
    let mut said = String::new();
    for _ in 0..20 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.contains("listening") {
            return Ok(());
        }
        said.push_str(&line);
    }
    Err(format!("it never said it was listening (said {said:?})"))
}

pub(crate) fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// What the daemon said on its way out, ready to append to an error.
///
/// Empty when it said nothing, so the caller's message reads the same either
/// way. A bind conflict is invisible from outside — "connection refused" looks
/// exactly like a slow start — and this is the line that names it.
fn complaint(errors: &Path) -> String {
    match std::fs::read_to_string(errors) {
        Ok(text) if !text.trim().is_empty() => format!("\nIts stderr:\n{text}"),
        _ => String::new(),
    }
}

/// A response body as JSON, or `Null` when it is not JSON at all.
fn decode(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
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
