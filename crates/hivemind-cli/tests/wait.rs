//! `hivemind wait` against a real daemon (#40).
//!
//! Its own file rather than another case in `single_daemon.rs`, which is two
//! lines under the test-size ratchet, and because everything here needs
//! something no other test does: a `hivemind` left running in the background
//! while the test goes and does something to the daemon.
//!
//! **Every wait in here is bounded twice.** The command gets a `--timeout`, and
//! [`Waiting::finish`] gives up on the child process and kills it rather than
//! joining it for ever. That is deliberate: a wait test in this repo once hung
//! instead of failing, because it mocked `sleep` and left the clock frozen, and
//! a hanging test is worse than a failing one — nothing tells you which it was
//! (CLAUDE.md). A bound on both sides means every way this can go wrong ends in
//! a red test with a sentence on it.

mod daemon;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use daemon::Daemon;

/// Long enough that a loaded CI runner is not the thing that fails, short
/// enough that a broken `wait` does not hold the suite up.
const GENEROUS: &str = "30s";

/// A `hivemind wait` running in the background.
struct Waiting {
    child: Child,
}

impl Waiting {
    /// Start one against this daemon. Nothing is awaited yet.
    fn start(daemon: &Daemon, args: &[&str]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_hivemind"))
            .arg("wait")
            .args(args)
            .env("HIVEMIND_HOME", daemon.home())
            .env("HIVEMIND_API", daemon.api())
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the cli runs");
        Self { child }
    }

    /// What it exited with and everything it said, or a failure rather than a
    /// hang.
    ///
    /// The deadline is the point. A `wait` that never returns would otherwise
    /// take the whole test binary with it, and a test that hangs says nothing
    /// about why.
    fn finish(mut self, within: Duration) -> Finished {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                let output = self.child.wait_with_output().expect("the wait finishes");
                return Finished {
                    code: output.status.code(),
                    out: String::from_utf8_lossy(&output.stdout).into_owned(),
                    err: String::from_utf8_lossy(&output.stderr).into_owned(),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!("`hivemind wait` was still running after {within:?}");
    }
}

/// What a finished `hivemind wait` left behind.
struct Finished {
    code: Option<i32>,
    out: String,
    err: String,
}

/// Send a message this node is itself a recipient of.
///
/// `everyone` includes the sender: local delivery does not need a peer, which
/// is what lets every test here run against one daemon.
fn send(daemon: &Daemon, subject: &str) {
    daemon.run(&[
        "send",
        "everyone",
        "--subject",
        subject,
        "--body",
        "the body",
    ]);
}

#[test]
fn unread_mail_already_in_the_box_ends_the_wait_at_once() {
    // The classic bug of these commands, and the reason the issue names it:
    // waiting for a message that is already there. Nothing else arrives during
    // this test, so a `wait` that only listened for events could not pass it —
    // it would sit until its timeout and exit 3.
    let daemon = Daemon::start("already");
    send(&daemon, "already here");
    daemon.wait_for("already here");

    let finished =
        Waiting::start(&daemon, &["--timeout", GENEROUS]).finish(Duration::from_secs(40));

    assert_eq!(finished.code, Some(0), "it should exit 0: {}", finished.err);
    assert!(
        finished.out.contains("already here"),
        "it should print the mail as `inbox` does: {}",
        finished.out
    );
}

#[test]
fn mail_that_arrives_during_the_wait_ends_it() {
    let daemon = Daemon::start("during");
    let waiting = Waiting::start(&daemon, &["--timeout", GENEROUS]);

    // Long enough for the child to have subscribed and looked in the box, and
    // then the box is checked empty: between them, what the send below ends is
    // the wait rather than the look.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        daemon.inbox().as_array().map(Vec::len),
        Some(0),
        "the box has to still be empty, or this proves nothing"
    );

    send(&daemon, "arrived later");

    let finished = waiting.finish(Duration::from_secs(40));
    assert_eq!(finished.code, Some(0), "it should exit 0: {}", finished.err);
    assert!(
        finished.out.contains("arrived later"),
        "it should print what arrived: {}",
        finished.out
    );
}

#[test]
fn a_timeout_exits_three_and_leaves_stdout_empty() {
    // Three, and not 0, and not the 1 of an error: "nothing arrived" that a
    // script cannot tell from "something did" is the whole failure this command
    // was asked for (#40).
    let daemon = Daemon::start("timeout");

    let finished = Waiting::start(&daemon, &["--timeout", "2s"]).finish(Duration::from_secs(30));

    assert_eq!(finished.code, Some(3), "said instead: {}", finished.err);
    assert!(
        finished.out.is_empty(),
        "stdout carries mail and nothing else: {:?}",
        finished.out
    );
    assert!(
        finished.err.contains("nothing arrived"),
        "and it should say so on stderr: {:?}",
        finished.err
    );
}

#[test]
fn a_wait_on_one_thread_takes_that_thread_and_not_the_other() {
    let daemon = Daemon::start("threads");
    send(&daemon, "the one I asked about");
    send(&daemon, "somebody else entirely");
    let wanted = daemon.wait_for("the one I asked about");
    daemon.wait_for("somebody else entirely");

    let id = wanted["id"].as_str().expect("an id");
    let finished = Waiting::start(&daemon, &["--thread", id, "--timeout", GENEROUS])
        .finish(Duration::from_secs(40));

    assert_eq!(finished.code, Some(0), "it should exit 0: {}", finished.err);
    assert!(
        finished.out.contains("the one I asked about"),
        "it should print the thread asked for: {}",
        finished.out
    );
    assert!(
        !finished.out.contains("somebody else entirely"),
        "and only that thread: {}",
        finished.out
    );
}

#[test]
fn a_wait_on_a_sender_nobody_here_knows_is_refused_rather_than_waited_out() {
    // A wait that can never end is the failure being fixed, so a typo in
    // `--from` has to be a refusal now rather than a silence for four hours.
    let daemon = Daemon::start("unknown");

    let finished =
        Waiting::start(&daemon, &["--from", "nobody-by-that-name"]).finish(Duration::from_secs(30));

    assert_eq!(finished.code, Some(1), "an error, not a wait and not mail");
    assert!(
        finished.err.contains("hivemind peers"),
        "and it should say where to look: {:?}",
        finished.err
    );
}

#[test]
fn a_wait_on_this_machine_by_name_takes_its_own_mail() {
    // The other half of `--from`: a name that does resolve still matches. The
    // harness names the daemon and its owner the same thing, which is as good
    // as a peer for this and costs one daemon rather than two.
    let daemon = Daemon::start("byname");
    send(&daemon, "from myself");
    daemon.wait_for("from myself");

    let finished = Waiting::start(&daemon, &["--from", "byname", "--timeout", GENEROUS])
        .finish(Duration::from_secs(40));

    assert_eq!(finished.code, Some(0), "it should exit 0: {}", finished.err);
    assert!(
        finished.out.contains("from myself"),
        "it should print the mail: {}",
        finished.out
    );
}

#[test]
fn a_daemon_that_dies_mid_wait_ends_the_wait_with_an_error() {
    // Hanging for ever on a daemon that is gone would be the same silence in a
    // new place. No `--timeout` here on purpose: the death is what has to end
    // it, and a timeout would end it either way.
    let mut daemon = Daemon::start("dies");
    let waiting = Waiting::start(&daemon, &[]);

    // Subscribed and waiting before the daemon goes away, or this would be
    // testing a failed connection instead.
    std::thread::sleep(Duration::from_secs(1));
    daemon.kill();

    let finished = waiting.finish(Duration::from_secs(30));
    assert_eq!(
        finished.code,
        Some(1),
        "a dead daemon is an error: {}",
        finished.err
    );
    assert!(
        finished.err.contains("daemon"),
        "and it should say what happened: {:?}",
        finished.err
    );
}

#[test]
fn a_daemon_that_shuts_down_politely_mid_wait_ends_it_the_same_way() {
    // The other half of the test above, and reachable only since #108. A
    // SIGTERM used to leave the daemon running *because* this wait was
    // attached, so the only way a wait ever ended was the daemon being killed
    // — and a kill leaves a truncated chunked body, which is a different
    // branch of the reader from a stream that ends cleanly. Both have to say
    // the same thing, or `hivemind wait && …` behaves differently depending on
    // how the daemon went.
    let mut daemon = Daemon::start("polite");
    let waiting = Waiting::start(&daemon, &[]);

    // Subscribed and waiting before the daemon goes away, as above.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        daemon.terminate_within(Duration::from_secs(15)).is_some(),
        "the daemon ignored SIGTERM with a wait attached, which is #108 itself"
    );

    let finished = waiting.finish(Duration::from_secs(30));
    assert_eq!(
        finished.code,
        Some(1),
        "a daemon that has gone is an error however it went: {}",
        finished.err
    );
    assert!(
        finished.err.contains("stopped sending events"),
        "and it should say so rather than exiting quietly: {:?}",
        finished.err
    );
}
