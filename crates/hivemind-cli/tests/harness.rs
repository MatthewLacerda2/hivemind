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

/// The three HTTP helpers, driven from inside a `#[tokio::test]` (#77).
///
/// The rule they used to carry — that they could not be called from an async
/// test, because `reqwest::blocking` builds a runtime and panics when it is
/// dropped in one — was written in the module doc and exercised nowhere, so
/// the first async test to want a `POST` would have been the one to find out.
/// This is that test, standing in for it permanently.
///
/// All three in one daemon rather than three tests: they share a start-up,
/// which is the expensive part, and the point is the runtime they run in
/// rather than any one of the requests.
#[tokio::test]
async fn the_http_helpers_answer_from_inside_an_async_test() {
    let daemon = Daemon::start("async");

    let (status, me) = daemon.get_json("/api/v1/me");
    assert_eq!(status, 200, "GET should answer: {me}");

    let files = tempfile::tempdir().expect("temp dir");
    let path = files.path().join("bytes.bin");
    // Every byte there is, so that a body decoded as UTF-8 and re-encoded
    // would come back as replacement characters rather than as itself.
    // `get_bytes` exists for attachments, and an attachment is a blob.
    let raw: Vec<u8> = (0..=u8::MAX).collect();
    std::fs::write(&path, &raw).expect("write the attachment");

    let (status, accepted) = daemon.post(
        "/api/v1/messages",
        &serde_json::json!({
            "to": ["everyone"],
            "subject": "from an async test",
            "body": "with a blob on it",
            "attachments": [path],
        }),
    );
    assert_eq!(status, 202, "POST should be accepted: {accepted}");

    let arrived = daemon.wait_for("from an async test");
    let id = arrived["id"].as_str().expect("an id");
    let full = daemon::json(&daemon.run(&["read", id, "--json"]));
    let sha = full["attachments"][0]["sha256"].as_str().expect("a digest");

    let (status, bytes) = daemon.get_bytes(&format!("/api/v1/messages/{id}/attachments/{sha}"));
    assert_eq!(status, 200);
    assert_eq!(bytes, raw, "the blob should arrive byte for byte");
}
