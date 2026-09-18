//! mDNS/DNS-SD and Tailscale discovery sources (SPEC §5).
//!
//! Discovery only ever answers "this node exists at this address". It never
//! pairs: trust is established by the TOFU flow in SPEC §6.2, and a node that
//! could pair itself by shouting on a multicast address would make that flow
//! pointless.
//!
//! The parts that can be got wrong — reading a TXT record, reading
//! `tailscale status --json`, deciding what is worth probing — are pure
//! functions with tests. The parts that need a network are behind traits, and
//! the real multicast tests are `#[ignore]`d and run by `just test-network`
//! (SPEC §13.2), because multicast in a CI container is a coin toss.

use std::collections::HashMap;
use std::time::Duration;

use hivemind_core::peer::NodeId;
use hivemind_core::peerbook::{AddrSource, PeerAddr};

/// The DNS-SD service type (SPEC §5.1).
pub const SERVICE_TYPE: &str = "_hivemind._tcp.local.";

/// The TXT record version we speak.
pub const TXT_VERSION: &str = "1";

/// How long to wait for a Tailscale peer to answer before moving on.
///
/// SPEC §5.2 names one second. A node that is up answers a TCP connect in
/// milliseconds; anything slower is a node that is off, and there may be
/// dozens of them.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// A node some discovery source says exists.
///
/// Deliberately not a `Peer`: nothing here is verified, and believing any of
/// it beyond "try this address" would be trusting whoever answered a multicast
/// query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// The fingerprint it claims. Checked against the certificate on contact.
    pub id: NodeId,
    /// What it calls itself.
    pub name: String,
    /// Who it says owns it.
    pub owner: Option<String>,
    /// Where it says it can be reached.
    pub addr: PeerAddr,
}

/// The TXT properties this node advertises (SPEC §5.1).
#[must_use]
pub fn txt_properties(id: NodeId, owner: Option<&str>, port: u16) -> Vec<(String, String)> {
    vec![
        ("v".to_owned(), TXT_VERSION.to_owned()),
        ("id".to_owned(), id.to_string()),
        // Always present, empty when unset: a reader can then tell "no owner"
        // from "an older version that did not say".
        ("owner".to_owned(), owner.unwrap_or_default().to_owned()),
        ("port".to_owned(), port.to_string()),
    ]
}

/// Read a TXT record into a [`Discovered`], or `None` if it is not one of ours.
///
/// `host` is where the responder was actually seen, which is more trustworthy
/// than anything in the record — a responder that could name its own address
/// could point us at somebody else's machine. The port is not knowable from
/// the socket, so that one is taken from the record.
#[must_use]
pub fn from_txt<S: std::hash::BuildHasher>(
    properties: &HashMap<String, String, S>,
    host: &str,
    default_port: u16,
) -> Option<Discovered> {
    // A record from a future version might mean anything. Ignoring it is the
    // only safe reading.
    if properties.get("v").map(String::as_str) != Some(TXT_VERSION) {
        return None;
    }

    let id: NodeId = properties.get("id")?.parse().ok()?;
    let port = properties
        .get("port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(default_port);

    Some(Discovered {
        id,
        name: properties
            .get("name")
            .cloned()
            .unwrap_or_else(|| id.short()),
        // An empty owner is no owner, not an owner called "".
        owner: properties
            .get("owner")
            .filter(|o| !o.is_empty())
            .map(ToOwned::to_owned),
        addr: PeerAddr {
            host: host.to_owned(),
            port,
            source: AddrSource::Mdns,
            last_ok: None,
        },
    })
}

/// Where `tailscale status --json` comes from.
///
/// A trait so the parsing and probing can be tested against a canned payload:
/// requiring a Tailscale login to run the test suite would mean the test never
/// runs.
pub trait Tailscale {
    /// The raw JSON, or an error if Tailscale is not usable here.
    ///
    /// # Errors
    /// A human-readable reason, most often that `tailscale` is not on PATH.
    fn status_json(&self) -> Result<String, String>;
}

/// Runs the real `tailscale` binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct TailscaleCli;

impl Tailscale for TailscaleCli {
    fn status_json(&self) -> Result<String, String> {
        let output = std::process::Command::new("tailscale")
            .args(["status", "--json"])
            .output()
            // Overwhelmingly this is "not on PATH", which is not an error
            // worth shouting about: Tailscale is never required (SPEC §5.2).
            .map_err(|e| format!("could not run tailscale: {e}"))?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        String::from_utf8(output.stdout).map_err(|e| e.to_string())
    }
}

/// Hosts worth probing, pulled out of `tailscale status --json` (SPEC §5.2).
///
/// Both the IP addresses and the `MagicDNS` name, because which one resolves
/// depends on the machine's DNS settings and trying both costs one connect.
/// Offline peers are dropped: Tailscale already knows they will not answer,
/// and a one-second timeout each would be the slowest part of a refresh.
#[must_use]
pub fn hosts_from_status(json: &str) -> Vec<String> {
    let Ok(status) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };

    let Some(peers) = status.get("Peer").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };

    let mut hosts = Vec::new();
    for peer in peers.values() {
        if peer.get("Online").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }

        if let Some(name) = peer.get("DNSName").and_then(serde_json::Value::as_str) {
            // `MagicDNS` names are fully qualified with a trailing dot, which is
            // correct DNS and wrong in a socket address.
            let name = name.trim_end_matches('.');
            if !name.is_empty() {
                hosts.push(name.to_owned());
            }
        }

        for ip in peer
            .get("TailscaleIPs")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            hosts.push(ip.to_owned());
        }
    }

    hosts.sort();
    hosts.dedup();
    hosts
}

/// Which of `hosts` have something listening on `port`.
///
/// A TCP connect, not a handshake: this answers "is it worth trying to pair
/// with this?" and a full TLS handshake against every machine on a tailnet
/// would be both slow and rude. All of them are tried at once, because one
/// host that never answers must not add its timeout to everybody else's wait.
pub async fn probe(hosts: Vec<String>, port: u16, timeout: Duration) -> Vec<String> {
    let mut checks = Vec::with_capacity(hosts.len());
    for host in hosts {
        checks.push(tokio::spawn(async move {
            let authority = PeerAddr::manual(host, port).authority();
            match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&authority)).await {
                Ok(Ok(_)) => Some(authority),
                _ => None,
            }
        }));
    }

    let mut reachable = Vec::new();
    for check in checks {
        if let Ok(Some(authority)) = check.await {
            reachable.push(authority);
        }
    }
    reachable
}

/// Where discovered nodes go.
///
/// A trait so the loop below can be driven in a test without multicast: what
/// is worth checking is that a browse result becomes exactly one call with the
/// right address, and that nothing here can pair anything.
pub trait Seen: Send + Sync {
    /// A node was seen. Never an invitation to trust it (SPEC §5.4).
    fn seen(&self, node: Discovered);
}

/// Advertise this node and browse for others until `shutdown` (SPEC §5.1).
///
/// Failing to start is logged and returns: a machine with multicast blocked
/// should still deliver mail to the peers it already knows, and taking the
/// daemon down over it would be a worse answer than a LAN with no discovery.
pub async fn run_mdns<S, F>(
    id: NodeId,
    name: &str,
    owner: Option<&str>,
    port: u16,
    sink: &S,
    shutdown: F,
) where
    S: Seen,
    F: std::future::Future<Output = ()> + Send,
{
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(error) => {
            tracing::warn!(%error, "mDNS is unavailable; discovery is off on this machine");
            return;
        }
    };

    // The instance name is the display name (SPEC §5.1), but two machines
    // called "laptop" would collide, so the short id disambiguates.
    let instance = format!("{name}-{}", id.short());
    let service = mdns_sd::ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{instance}.local."),
        (),
        port,
        &txt_properties(id, owner, port)[..],
    );

    match service {
        Ok(service) => {
            // `enable_addr_auto` fills in this machine's addresses and keeps
            // them current, which is the whole point on a laptop that moves.
            if let Err(error) = daemon.register(service.enable_addr_auto()) {
                tracing::warn!(%error, "could not advertise over mDNS");
            }
        }
        Err(error) => tracing::warn!(%error, "could not build the mDNS service record"),
    }

    let browse = match daemon.browse(SERVICE_TYPE) {
        Ok(browse) => browse,
        Err(error) => {
            tracing::warn!(%error, "could not browse for peers over mDNS");
            return;
        }
    };

    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let event = tokio::select! {
            () = &mut shutdown => break,
            event = browse.recv_async() => event,
        };

        let Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) = event else {
            // Anything else is a search started, a service removed, or the
            // channel closing — none of which tells us where a node is.
            continue;
        };

        // The record carries the responder's own port; the one we advertise
        // on is only the fallback for a record that omits it.
        let properties = properties_of(&info.txt_properties);
        for address in &info.addresses {
            let Some(found) = from_txt(&properties, &address.to_ip_addr().to_string(), info.port)
            else {
                continue;
            };
            // Ourselves, on every interface, several times a minute.
            if found.id == id {
                continue;
            }
            sink.seen(found);
        }
    }

    let _ = daemon.shutdown();
}

/// `mdns-sd` exposes TXT properties as its own type; this is the shape the
/// parsing above is written against.
fn properties_of(properties: &mdns_sd::TxtProperties) -> HashMap<String, String> {
    properties
        .iter()
        .map(|property| (property.key().to_owned(), property.val_str().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivemind_core::identity::Identity;

    fn node(seed: u8) -> NodeId {
        Identity::from_seed([seed; 32]).expect("identity").node_id()
    }

    fn txt(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn what_we_advertise_is_what_we_can_read_back() {
        // The two halves of SPEC §5.1 have to agree, and they are written in
        // different places.
        let id = node(1);
        let properties: HashMap<String, String> = txt_properties(id, Some("matheus"), 8400)
            .into_iter()
            .collect();

        let found = from_txt(&properties, "10.0.0.5", 9999).expect("our own record parses");
        assert_eq!(found.id, id);
        assert_eq!(found.owner.as_deref(), Some("matheus"));
        assert_eq!(found.addr.authority(), "10.0.0.5:8400");
        assert_eq!(found.addr.source, AddrSource::Mdns);
    }

    #[test]
    fn an_unset_owner_reads_back_as_none_not_as_an_empty_name() {
        let properties: HashMap<String, String> =
            txt_properties(node(2), None, 8400).into_iter().collect();
        let found = from_txt(&properties, "10.0.0.5", 8400).expect("parses");
        assert_eq!(found.owner, None);
    }

    #[test]
    fn a_record_from_a_future_version_is_ignored() {
        // It might mean anything; acting on half of it is worse than not
        // seeing the node.
        let properties = txt(&[("v", "2"), ("id", &node(3).to_string()), ("port", "8400")]);
        assert_eq!(from_txt(&properties, "10.0.0.5", 8400), None);
    }

    #[test]
    fn a_record_with_no_usable_id_is_ignored() {
        assert_eq!(from_txt(&txt(&[("v", "1")]), "10.0.0.5", 8400), None);
        assert_eq!(
            from_txt(&txt(&[("v", "1"), ("id", "not-a-node")]), "10.0.0.5", 8400),
            None
        );
    }

    #[test]
    fn the_address_seen_beats_anything_the_record_claims() {
        // A responder that could name its own address could point us at
        // somebody else's machine.
        let properties = txt(&[
            ("v", "1"),
            ("id", &node(4).to_string()),
            ("port", "8400"),
            ("host", "somewhere-else.local"),
        ]);
        let found = from_txt(&properties, "10.0.0.5", 8400).expect("parses");
        assert_eq!(found.addr.host, "10.0.0.5");
    }

    #[test]
    fn a_record_without_a_port_falls_back_to_the_default() {
        let properties = txt(&[("v", "1"), ("id", &node(5).to_string())]);
        let found = from_txt(&properties, "10.0.0.5", 8400).expect("parses");
        assert_eq!(found.addr.port, 8400);
    }

    /// A trimmed `tailscale status --json`, keeping only the shape we read.
    const STATUS: &str = r#"{
      "Self": { "DNSName": "mine.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.1"] },
      "Peer": {
        "nodekey:aaa": {
          "DNSName": "laptop.tail1234.ts.net.",
          "TailscaleIPs": ["100.64.0.2", "fd7a::2"],
          "Online": true
        },
        "nodekey:bbb": {
          "DNSName": "desktop.tail1234.ts.net.",
          "TailscaleIPs": ["100.64.0.3"],
          "Online": false
        }
      }
    }"#;

    #[test]
    fn tailscale_peers_give_both_their_name_and_their_addresses() {
        // Which one resolves depends on the machine's DNS settings, and
        // trying both costs one connect.
        let hosts = hosts_from_status(STATUS);
        assert!(hosts.contains(&"laptop.tail1234.ts.net".to_owned()));
        assert!(hosts.contains(&"100.64.0.2".to_owned()));
        assert!(hosts.contains(&"fd7a::2".to_owned()));
    }

    #[test]
    fn a_peer_tailscale_says_is_offline_is_not_probed() {
        // Tailscale already knows; a one-second timeout each would be the
        // slowest part of `peers refresh`.
        let hosts = hosts_from_status(STATUS);
        assert!(!hosts.iter().any(|h| h.contains("desktop")));
        assert!(!hosts.contains(&"100.64.0.3".to_owned()));
    }

    #[test]
    fn this_machine_is_not_one_of_its_own_peers() {
        let hosts = hosts_from_status(STATUS);
        assert!(!hosts.iter().any(|h| h.contains("mine")));
        assert!(!hosts.contains(&"100.64.0.1".to_owned()));
    }

    #[test]
    fn a_tailscale_that_is_not_installed_is_not_an_error_it_is_no_peers() {
        // SPEC §5.2: never require Tailscale.
        assert!(hosts_from_status("").is_empty());
        assert!(hosts_from_status("not json at all").is_empty());
        assert!(hosts_from_status(r#"{"BackendState":"NeedsLogin"}"#).is_empty());
    }

    #[tokio::test]
    async fn probing_finds_what_is_listening_and_not_what_is_not() {
        let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listening.local_addr().expect("addr").port();

        let closed = {
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            socket.local_addr().expect("addr").port()
        };

        let reachable = probe(vec!["127.0.0.1".to_owned()], port, PROBE_TIMEOUT).await;
        assert_eq!(reachable, vec![format!("127.0.0.1:{port}")]);

        let unreachable = probe(vec!["127.0.0.1".to_owned()], closed, PROBE_TIMEOUT).await;
        assert!(unreachable.is_empty(), "nothing is listening there");
    }

    #[tokio::test]
    async fn a_host_that_never_answers_does_not_hold_up_the_others() {
        // 198.51.100.0/24 is reserved for documentation and routes nowhere,
        // so a connect to it hangs until the timeout rather than being
        // refused — which is exactly the case the timeout exists for.
        let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listening.local_addr().expect("addr").port();

        let started = std::time::Instant::now();
        let reachable = probe(
            vec!["198.51.100.1".to_owned(), "127.0.0.1".to_owned()],
            port,
            Duration::from_millis(200),
        )
        .await;

        assert_eq!(reachable, vec![format!("127.0.0.1:{port}")]);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "probes should run concurrently, took {:?}",
            started.elapsed()
        );
    }
}

/// The real thing: two mDNS daemons on whatever network this machine is on.
///
/// `#[ignore]`d and run by `just test-network` (SPEC §13.2). Multicast in a CI
/// container is a coin toss, and a test that fails for reasons outside the
/// code is worse than no test — but the code above has never spoken to a real
/// responder without this.
#[cfg(test)]
mod network_tests {
    use super::*;
    use hivemind_core::identity::Identity;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collected(Mutex<Vec<Discovered>>);

    impl Seen for Collected {
        fn seen(&self, node: Discovered) {
            self.0.lock().expect("lock").push(node);
        }
    }

    #[tokio::test]
    #[ignore = "needs real multicast; run with `just test-network`"]
    async fn network_two_nodes_find_each_other_over_real_mdns() {
        let our_id = Identity::from_seed([21u8; 32]).expect("identity").node_id();
        let their_id = Identity::from_seed([22u8; 32]).expect("identity").node_id();

        let found = Collected::default();
        let (stop_us, stopped_us) = tokio::sync::oneshot::channel();
        let (stop_them, stopped_them) = tokio::sync::oneshot::channel();

        let theirs = tokio::spawn(async move {
            let nothing = Collected::default();
            run_mdns(their_id, "them", Some("them"), 18401, &nothing, async {
                let _ = stopped_them.await;
            })
            .await;
        });

        let ours = tokio::spawn(async move {
            run_mdns(our_id, "us", Some("us"), 18400, &found, async {
                let _ = stopped_us.await;
            })
            .await;
            found
        });

        // Announcements are not instant and there is no event to await.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let _ = stop_us.send(());
        let _ = stop_them.send(());

        let found = ours.await.expect("our browser should not panic");
        let _ = theirs.await;

        let seen = found.0.lock().expect("lock");
        assert!(
            seen.iter().any(|d| d.id == their_id),
            "the other node was never seen; found {seen:?}"
        );
        assert!(
            !seen.iter().any(|d| d.id == our_id),
            "we should skip our own announcements"
        );
    }
}
