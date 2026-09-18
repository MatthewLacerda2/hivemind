//! `hivemind doctor` (SPEC §10).
//!
//! Every check answers one question a person might actually be asking when
//! something is not working, and says what to do about it. A check that can
//! only say "failed" is not worth running.
//!
//! Nothing here is fatal on its own: a machine with no Tailscale and no Claude
//! is a perfectly good hivemind node. The exit code is non-zero only when
//! something is actually broken.

use std::fmt;
use std::path::Path;

use owo_colors::OwoColorize as _;

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    /// Working.
    Good,
    /// Not present, and that is allowed.
    ///
    /// Tailscale and Claude Code are both optional; saying "missing" in red
    /// would send somebody to fix a thing that is not broken.
    Absent,
    /// Broken, and worth acting on.
    Bad,
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A symbol as well as a colour: this gets piped into files and read on
        // terminals that do not do colour.
        match self {
            Self::Good => write!(f, "{}", "ok  ".green()),
            Self::Absent => write!(f, "{}", "--  ".dimmed()),
            Self::Bad => write!(f, "{}", "!!  ".red()),
        }
    }
}

/// One line of the report.
#[derive(Debug)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) health: Health,
    pub(crate) detail: String,
    /// What to do about it. Only when there is something to do.
    pub(crate) fix: Option<String>,
}

impl Check {
    fn good(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Good,
            detail: detail.into(),
            fix: None,
        }
    }

    fn absent(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Absent,
            detail: detail.into(),
            fix: None,
        }
    }

    fn bad(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Bad,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Is `binary` on PATH?
fn on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
}

/// Can anything be reached at this address?
fn reachable(addr: &str) -> bool {
    use std::net::ToSocketAddrs as _;

    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| {
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).is_ok()
    })
}

/// Everything `doctor` looks at.
pub(crate) fn checks(home: &Path, api: &str, daemon: Option<&DaemonFacts>) -> Vec<Check> {
    vec![
        daemon_check(api, daemon),
        peer_port_check(daemon),
        home_check(home),
        identity_check(home),
        tailscale_check(),
        claude_check(),
        hooks_check(),
        mdns_check(),
    ]
}

/// What `doctor` learned from a running daemon.
#[derive(Debug)]
pub(crate) struct DaemonFacts {
    pub(crate) short_id: String,
    pub(crate) version: String,
    pub(crate) peer_port: u16,
    pub(crate) peers: usize,
    pub(crate) discovery: bool,
}

fn daemon_check(api: &str, daemon: Option<&DaemonFacts>) -> Check {
    match daemon {
        Some(facts) => Check::good(
            "daemon",
            format!(
                "v{} as {} on {api}, {} peer{}",
                facts.version,
                facts.short_id,
                facts.peers,
                if facts.peers == 1 { "" } else { "s" }
            ),
        ),
        None => Check::bad(
            "daemon",
            format!("nothing answering on {api}"),
            "start it with `hivemind daemon`, or `hivemind service install` to keep it running",
        ),
    }
}

fn peer_port_check(daemon: Option<&DaemonFacts>) -> Check {
    let Some(facts) = daemon else {
        return Check::absent("peer port", "not checked — the daemon is not running");
    };

    let addr = format!("127.0.0.1:{}", facts.peer_port);
    if reachable(&addr) {
        Check::good(
            "peer port",
            format!("0.0.0.0:{} accepting connections", facts.peer_port),
        )
    } else {
        Check::bad(
            "peer port",
            format!("{addr} is not accepting connections"),
            "another process may hold the port; set `peer_port` in config.toml",
        )
    }
}

fn home_check(home: &Path) -> Check {
    if !home.is_dir() {
        return Check::bad(
            "data",
            format!("{} does not exist", home.display()),
            "run `hivemind init`",
        );
    }

    // Read-only is the interesting failure: everything looks fine until the
    // first message arrives and cannot be written.
    let probe = home.join(".doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::good("data", format!("{} is writable", home.display()))
        }
        Err(error) => Check::bad(
            "data",
            format!("{} is not writable: {error}", home.display()),
            "check the directory's permissions and owner",
        ),
    }
}

fn identity_check(home: &Path) -> Check {
    let key = home.join("identity").join("node.key");
    if !key.is_file() {
        return Check::bad(
            "identity",
            "no keypair yet",
            "run `hivemind init`, or start the daemon once to generate one",
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(metadata) = std::fs::metadata(&key) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Check::bad(
                    "identity",
                    format!("node.key is mode {mode:o}, readable by others"),
                    format!("chmod 600 {}", key.display()),
                );
            }
        }
    }

    Check::good("identity", "keypair present and private")
}

fn tailscale_check() -> Check {
    if !on_path("tailscale") {
        return judge_tailscale(None);
    }

    let output = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            judge_tailscale(Some(&String::from_utf8_lossy(&output.stdout)))
        }
        _ => judge_tailscale(Some("")),
    }
}

/// What to say about Tailscale, given what it said about itself.
///
/// Split out from running the binary so both branches can be tested on a
/// machine that happens to have Tailscale installed — which is most of the
/// machines this will be written on, and none of the interesting case.
fn judge_tailscale(status: Option<&str>) -> Check {
    let Some(status) = status else {
        // SPEC §5.2: never required. Absent, not broken.
        return Check::absent("tailscale", "not installed — hivemind does not need it");
    };

    let hosts = hivemind_net::discovery::hosts_from_status(status);
    if hosts.is_empty() {
        // Installed but not logged in, or logged in with nothing else on the
        // tailnet. Worth saying, not worth failing over.
        Check::absent("tailscale", "installed, but no peers to try")
    } else {
        Check::good("tailscale", format!("up, {} addresses to try", hosts.len()))
    }
}

fn claude_check() -> Check {
    judge_claude(on_path("claude"))
}

/// Split out for the same reason as [`judge_tailscale`].
fn judge_claude(present: bool) -> Check {
    if present {
        Check::good("claude", "on PATH — `hivemind mcp install` will find it")
    } else {
        Check::absent(
            "claude",
            "not on PATH — the MCP server still works for any client",
        )
    }
}

fn hooks_check() -> Check {
    let Ok(settings) = crate::hooks::claude_settings_path() else {
        return Check::absent("hooks", "no home directory to look in");
    };
    if !settings.is_file() {
        return Check::absent(
            "hooks",
            "Claude Code settings not found — `hivemind hook install` creates them",
        );
    }

    match std::fs::read_to_string(&settings) {
        Ok(text) if text.contains("hivemind hook check") => {
            Check::good("hooks", "installed in Claude Code's settings")
        }
        Ok(_) => Check::absent("hooks", "not installed — `hivemind hook install`"),
        Err(error) => Check::bad(
            "hooks",
            format!("could not read {}: {error}", settings.display()),
            "check the file's permissions",
        ),
    }
}

fn mdns_check() -> Check {
    // Binding the multicast port is the part that fails on a locked-down
    // machine, and it fails silently at runtime — the daemon logs and carries
    // on. Doing it here is the only way anyone finds out.
    match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => {
            let group = std::net::Ipv4Addr::new(224, 0, 0, 251);
            match socket.join_multicast_v4(&group, &std::net::Ipv4Addr::UNSPECIFIED) {
                Ok(()) => Check::good("mDNS", "multicast available on this machine"),
                Err(error) => Check::bad(
                    "mDNS",
                    format!("cannot join the mDNS group: {error}"),
                    "discovery will not work; pair with `hivemind join <host>` instead",
                ),
            }
        }
        Err(error) => Check::bad(
            "mDNS",
            format!("cannot open a UDP socket: {error}"),
            "discovery will not work; pair with `hivemind join <host>` instead",
        ),
    }
}

/// Run every check and print the report (SPEC §10).
///
/// Exits non-zero only if something is actually broken. A machine with no
/// Tailscale and no Claude Code is a perfectly good hivemind node, and telling
/// somebody otherwise would send them to fix nothing.
pub(crate) async fn run(home: Option<&Path>, api: &str, json: bool) -> anyhow::Result<()> {
    let home = crate::paths::home(home)?;
    let daemon = ask_the_daemon(api).await;
    let checks = checks(&home, api, daemon.as_ref());

    if json {
        let rows: Vec<serde_json::Value> = checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "name": check.name,
                    "health": match check.health {
                        Health::Good => "ok",
                        Health::Absent => "absent",
                        Health::Bad => "broken",
                    },
                    "detail": check.detail,
                    "fix": check.fix,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for check in &checks {
            println!("{}{:<10} {}", check.health, check.name, check.detail);
            if let Some(fix) = &check.fix {
                println!("    {}", fix.dimmed());
            }
        }

        if let Some(facts) = &daemon
            && !facts.discovery
        {
            println!();
            println!(
                "{}",
                "note: discovery is switched off in config.toml".dimmed()
            );
        }
    }

    let broken = checks.iter().filter(|c| c.health == Health::Bad).count();
    if broken > 0 {
        anyhow::bail!(
            "{broken} {} need attention",
            if broken == 1 { "check" } else { "checks" }
        );
    }
    Ok(())
}

/// What the daemon says about itself, if it is up.
async fn ask_the_daemon(api: &str) -> Option<DaemonFacts> {
    #[derive(serde::Deserialize)]
    struct Me {
        short_id: String,
        version: String,
        peer_port: u16,
        peers: usize,
    }

    let me: Me = crate::client::Client::new(api)
        .get("/api/v1/me")
        .await
        .ok()?;
    Some(DaemonFacts {
        short_id: me.short_id,
        version: me.version,
        peer_port: me.peer_port,
        peers: me.peers,
        // Read from config rather than asked over the wire: the daemon does
        // not report it, and `doctor` runs beside it on the same machine.
        discovery: hivemind_core::config::Config::load(
            &crate::paths::home(None).unwrap_or_default(),
        )
        .map_or(true, |c| c.discovery),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> DaemonFacts {
        DaemonFacts {
            short_id: "abcd1234".to_owned(),
            version: "0.1.0".to_owned(),
            peer_port: 8400,
            peers: 2,
            discovery: true,
        }
    }

    #[test]
    fn a_daemon_that_is_not_running_is_broken_and_says_how_to_start_it() {
        // The single most likely reason somebody runs this command.
        let check = daemon_check("http://127.0.0.1:8401", None);
        assert_eq!(check.health, Health::Bad);
        assert!(
            check
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("hivemind daemon")),
            "a check that can only say 'failed' is not worth running: {check:?}"
        );
    }

    #[test]
    fn a_missing_optional_tool_is_absent_not_broken() {
        // SPEC §5.2: Tailscale is never required. Saying "missing" in red
        // would send somebody to fix a thing that is not broken. Driven
        // through the judgement rather than the machine, because whether this
        // laptop has Tailscale is not what is being tested.
        assert_eq!(judge_tailscale(None).health, Health::Absent);
        assert_eq!(judge_tailscale(Some("")).health, Health::Absent);
        assert_eq!(judge_claude(false).health, Health::Absent);

        assert_eq!(judge_claude(true).health, Health::Good);
    }

    #[test]
    fn a_connected_tailscale_says_how_many_addresses_it_found() {
        let status =
            r#"{"Peer":{"k":{"DNSName":"a.ts.net.","TailscaleIPs":["100.64.0.2"],"Online":true}}}"#;
        let check = judge_tailscale(Some(status));
        assert_eq!(check.health, Health::Good);
        assert!(check.detail.contains('2'), "name and address: {check:?}");
    }

    #[test]
    fn a_data_directory_that_does_not_exist_says_to_run_init() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("never-created");

        let check = home_check(&missing);
        assert_eq!(check.health, Health::Bad);
        assert!(check.fix.as_deref().is_some_and(|f| f.contains("init")));
    }

    #[test]
    fn a_writable_data_directory_is_fine_and_leaves_no_probe_behind() {
        let dir = tempfile::tempdir().expect("temp dir");

        let check = home_check(dir.path());
        assert_eq!(check.health, Health::Good);

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftovers.is_empty(),
            "the write probe should have been cleaned up"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_private_key_other_people_can_read_is_broken() {
        // The whole of this node's identity is in that file.
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let identity = dir.path().join("identity");
        std::fs::create_dir_all(&identity).expect("mkdir");
        let key = identity.join("node.key");
        std::fs::write(&key, b"not really a key").expect("write");

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(identity_check(dir.path()).health, Health::Good);

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let check = identity_check(dir.path());
        assert_eq!(check.health, Health::Bad);
        assert!(
            check
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("chmod 600"))
        );
    }

    #[test]
    fn a_missing_identity_says_to_generate_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let check = identity_check(dir.path());
        assert_eq!(check.health, Health::Bad);
        assert!(check.fix.is_some());
    }

    #[test]
    fn the_peer_port_is_not_reported_broken_when_the_daemon_is_down() {
        // It would be, and saying so would send somebody chasing a port
        // conflict that does not exist.
        assert_eq!(peer_port_check(None).health, Health::Absent);
    }

    #[test]
    fn a_peer_port_nothing_is_listening_on_is_broken() {
        let port = {
            let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            socket.local_addr().expect("addr").port()
        };
        let mut facts = facts();
        facts.peer_port = port;

        let check = peer_port_check(Some(&facts));
        assert_eq!(check.health, Health::Bad);
        assert!(check.fix.is_some());
    }

    #[test]
    fn every_broken_check_says_what_to_do_about_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let checks = checks(&dir.path().join("nothing-here"), "http://127.0.0.1:1", None);

        for check in &checks {
            if check.health == Health::Bad {
                assert!(
                    check.fix.is_some(),
                    "{} is broken with nothing to do about it",
                    check.name
                );
            }
        }
        assert!(
            checks.iter().any(|c| c.health == Health::Bad),
            "this setup is definitely broken, so something should have said so"
        );
    }

    #[test]
    fn health_prints_a_symbol_as_well_as_a_colour() {
        // It gets piped into files and read on terminals without colour.
        for health in [Health::Good, Health::Absent, Health::Bad] {
            let printed = health.to_string();
            let bare: String = printed.chars().filter(|c| !c.is_control()).collect();
            assert!(
                bare.contains("ok") || bare.contains("--") || bare.contains("!!"),
                "{health:?} printed as {printed:?} with nothing to read"
            );
        }
    }
}
