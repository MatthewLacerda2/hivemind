//! The harness's own tests: the retry that absorbs a port collision (#74).
//!
//! Every other file here tests the product through the harness. This one tests
//! the harness, because the retry it carries was presumed rather than
//! exercised — and a retry nothing exercises is a comment. `free_port` hands
//! out a number it cannot reserve, so the collision arrives at random in
//! whichever test drew second, which is the one condition a test must never
//! wait for.
//!
//! So the collision is arranged: a listener is held on the port the first
//! attempt is given, and the assertion is that the daemon is up and answering
//! anyway, on a different number.

mod daemon;

use daemon::{Daemon, free_port};

/// The API port taken, which is how the collision reads in the daemon's first
/// bind: it never prints that it is listening.
///
/// `127.0.0.1` exactly, not `0.0.0.0`, because the daemon binds the loopback
/// address (SPEC §6.3) and only an identical bind is refused on both macOS and
/// Linux. A wildcard decoy would be a test that passes on one platform.
#[test]
fn a_taken_api_port_costs_an_attempt_and_not_the_test() {
    let decoy = std::net::TcpListener::bind("127.0.0.1:0").expect("a port to sit on");
    let taken = decoy.local_addr().expect("addr").port();

    // `decoy` is held until the end of the test on purpose. Dropping it here
    // would free the port, the first attempt would succeed, and this would
    // pass without the retry existing at all.
    let daemon = Daemon::start_with_first_ports("collider", taken, free_port());

    assert_ne!(
        daemon.port, taken,
        "the first attempt should have lost this port and the second drawn another"
    );
    let status = daemon::json(&daemon.run(&["status", "--json"]));
    assert!(
        status["id"].is_string(),
        "and the daemon it returned should be a working one: {status}"
    );

    drop(decoy);
}

/// The peer port taken, which reads differently: the daemon prints that it is
/// listening, then dies binding the peer listener a moment later.
///
/// This is the shape the incident took — `could not bind 0.0.0.0:58646` on a
/// documentation-only branch — so it is worth its own case. `0.0.0.0` here
/// because that is what the peer listener binds, and a decoy on the loopback
/// address alone would not collide with it on Linux.
#[test]
fn a_taken_peer_port_costs_an_attempt_and_not_the_test() {
    let decoy = std::net::TcpListener::bind("0.0.0.0:0").expect("a port to sit on");
    let taken = decoy.local_addr().expect("addr").port();

    let daemon = Daemon::start_with_first_ports("collider", free_port(), taken);

    assert_ne!(
        daemon.peer_port, taken,
        "the first attempt should have died binding this port and the second drawn another"
    );
    let status = daemon::json(&daemon.run(&["status", "--json"]));
    assert!(
        status["id"].is_string(),
        "and the daemon it returned should be a working one: {status}"
    );

    drop(decoy);
}
