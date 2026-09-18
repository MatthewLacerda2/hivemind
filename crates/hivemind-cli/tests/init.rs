//! `hivemind init` end to end (SPEC §2, §14).
//!
//! Driven through the shipped binary in a temporary home, with every step that
//! would touch the real machine switched off. The steps that *are* exercised
//! are the ones that can silently go wrong: generating an identity once,
//! keeping a config somebody edited, and being safe to run twice.

use std::process::Command;

/// Run `hivemind` in `home` with the side effects turned off.
///
/// `--no-launchd`, `--no-mcp` and `--no-hooks` are not politeness: without
/// them this test would install a launchd agent and edit the developer's
/// `~/.claude/settings.json`.
fn init(home: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["init", "--no-launchd", "--no-mcp", "--no-hooks"];
    args.extend_from_slice(extra);

    Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(&args)
        .env("HIVEMIND_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs")
}

fn stdout(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn init_creates_everything_a_node_needs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");

    let text = stdout(&init(&home, &["--name", "workshop", "--owner", "matheus"]));

    assert!(home.join("config.toml").is_file(), "SPEC §2 step 2");
    assert!(home.join("identity/node.key").is_file(), "SPEC §2 step 1");
    assert!(home.join("identity/node.crt").is_file());

    // SPEC §2 step 6: the name, the fingerprint and where to be reached.
    assert!(text.contains("workshop"), "the name: {text}");
    assert!(text.contains("matheus"), "the owner: {text}");
    assert!(text.contains("hm1:"), "the fingerprint: {text}");
    assert!(text.contains("reachable at"), "the addresses: {text}");

    // And what to do next, since somebody who will not read docs just ran it.
    assert!(text.contains("hivemind join"), "next steps: {text}");
}

#[test]
fn the_private_key_is_not_readable_by_anybody_else() {
    // The whole of this node's identity is in that file.
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");
    init(&home, &[]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(home.join("identity/node.key"))
            .expect("the key exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "node.key should be mode 600, is {mode:o}");
    }
}

#[test]
fn running_init_twice_keeps_the_identity_it_already_made() {
    // Somebody who ran it, read the output and ran it again is the normal
    // case. A second identity would orphan every message already sent.
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");

    let first = stdout(&init(&home, &["--name", "workshop"]));
    let second = stdout(&init(&home, &["--name", "workshop"]));

    let id_of = |text: &str| {
        text.lines()
            .find(|line| line.contains("hm1:"))
            .map(|line| line.trim().to_owned())
            .expect("an id in the output")
    };
    assert_eq!(
        id_of(&first),
        id_of(&second),
        "the identity must not change"
    );
    assert!(
        second.contains("kept your settings"),
        "and it should say so: {second}"
    );
}

#[test]
fn running_init_twice_does_not_clobber_a_config_somebody_edited() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");
    init(&home, &["--name", "workshop"]);

    // Edit it the way a person would.
    let config = home.join("config.toml");
    let edited = std::fs::read_to_string(&config)
        .expect("read")
        .replace("notifications = true", "notifications = false");
    std::fs::write(&config, &edited).expect("write");

    init(&home, &[]);

    let after = std::fs::read_to_string(&config).expect("read");
    assert!(
        after.contains("notifications = false"),
        "their edit should have survived: {after}"
    );
    assert!(
        after.contains("name = \"workshop\""),
        "and so should the name: {after}"
    );
}

#[test]
fn a_name_given_on_the_second_run_replaces_the_first() {
    // Otherwise `--name` would silently do nothing on any machine that had
    // already been set up, which is the machine you are most likely to be
    // renaming.
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");

    init(&home, &["--name", "before"]);
    init(&home, &["--name", "after"]);

    let config = std::fs::read_to_string(home.join("config.toml")).expect("read");
    assert!(config.contains("name = \"after\""), "{config}");
}

#[test]
fn an_impossible_configuration_is_refused_rather_than_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");

    let output = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args([
            "init",
            "--no-launchd",
            "--no-mcp",
            "--no-hooks",
            "--name",
            "  ",
        ])
        .env("HIVEMIND_HOME", &home)
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    assert!(!output.status.success(), "a blank name should be refused");
    assert!(
        !home.join("config.toml").is_file(),
        "and nothing should have been written"
    );
}

#[test]
fn the_daemon_starts_from_what_init_wrote() {
    // The point of the whole command: after it, the daemon runs.
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("hivemind");
    init(&home, &["--name", "workshop", "--owner", "matheus"]);

    let port = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        socket.local_addr().expect("addr").port()
    };
    let peer_port = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        socket.local_addr().expect("addr").port()
    };

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["daemon", "--port", &port.to_string()])
        .env("HIVEMIND_HOME", &home)
        .env("HIVEMIND_PEER_PORT", peer_port.to_string())
        .env("HIVEMIND_DISCOVERY", "false")
        .env("HIVEMIND_LOG", "warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the daemon starts");

    let ready = {
        use std::io::{BufRead as _, BufReader};
        let mut reader = BufReader::new(daemon.stdout.take().expect("stdout"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("a first line");
        // Held so the daemon does not take a SIGPIPE on its next println.
        std::mem::forget(reader);
        line
    };
    assert!(ready.contains("listening"), "unexpected: {ready}");

    let status = Command::new(env!("CARGO_BIN_EXE_hivemind"))
        .args(["status", "--json"])
        .env("HIVEMIND_HOME", &home)
        .env("HIVEMIND_API", format!("http://127.0.0.1:{port}"))
        .env("NO_COLOR", "1")
        .output()
        .expect("the cli runs");

    let _ = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status();
    let _ = daemon.wait();

    let text = String::from_utf8_lossy(&status.stdout);
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(json["name"], "workshop");
    assert_eq!(json["owner"], "matheus");
}
