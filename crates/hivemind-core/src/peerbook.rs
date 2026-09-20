//! `peers.toml`: the address book — where each member can be reached, and the
//! certificate it is pinned to.
//!
//! A peer is in this file once it has proved the group key in a handshake
//! (SPEC §6.2, ADR 0013). It is not a list of decisions any more: the key is
//! what admits, and this is what remembers who was admitted. Discovery never
//! writes a new entry here — it only updates the addresses of peers already in
//! it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::peer::NodeId;

/// How an address was learned (SPEC §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddrSource {
    /// Found by mDNS on the local network.
    Mdns,
    /// Found through `tailscale status`.
    Tailscale,
    /// Typed in by a human with `hivemind join`.
    Manual,
    /// Told to us by another member, in its hello (SPEC §5.4).
    ///
    /// Believed no further than "try this": what admits a node is the group
    /// key, and what identifies it is the certificate it presents (ADR 0013).
    Gossip,
}

/// One way to reach a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAddr {
    /// Hostname or IP.
    pub host: String,
    /// The peer listener's port.
    pub port: u16,
    /// Where we learned it.
    pub source: AddrSource,
    /// When delivery to this address last succeeded.
    pub last_ok: Option<DateTime<Utc>>,
}

impl PeerAddr {
    /// A manually supplied address.
    #[must_use]
    pub fn manual(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            source: AddrSource::Manual,
            last_ok: None,
        }
    }

    /// Does this address point at this node's own peer listener rather than at
    /// somebody else?
    ///
    /// This is the one place that decides it, because the same judgement is
    /// needed twice: a handshake must not record such an address (#23), and a
    /// file written before that fix must not have it read back (#29). Two
    /// machines that met in the first real test both wrote `127.0.0.1:8400`
    /// down beside the other's real address, and delivery tries addresses in
    /// order — at best it reached the peer's own daemon, at worst whatever
    /// else was listening on that port.
    ///
    /// The **port** carries as much of the rule as the host. A loopback
    /// address on some *other* port is a second daemon on this machine, where
    /// loopback is the truth — that is what every integration test here is, and
    /// discarding it would break the case #23 was careful to keep.
    ///
    /// `0.0.0.0` counts with the loopback addresses: "every interface on this
    /// machine" is not a way to reach another one either.
    #[must_use]
    pub fn points_at_this_node(&self, own_peer_port: u16) -> bool {
        self.port == own_peer_port && host_is_this_machine(&self.host)
    }

    /// `host:port`, for a connection attempt.
    #[must_use]
    pub fn authority(&self) -> String {
        // An IPv6 literal has to be bracketed or the port is ambiguous.
        if self.host.contains(':') && !self.host.starts_with('[') {
            return format!("[{}]:{}", self.host, self.port);
        }
        format!("{}:{}", self.host, self.port)
    }
}

/// Is `host` a name for the machine this code is running on?
///
/// A literal is judged by what it is; a name is only judged if it is
/// `localhost`, because a hostname is not resolved here and a `MagicDNS` name
/// outlives the address behind it (#23).
fn host_is_this_machine(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

/// An address the address book refused to read back, and whose peer it was
/// filed against.
///
/// Kept so that nothing vanishes without saying so: the daemon logs these at
/// start and `hivemind doctor` names the line still in the file (#29).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardedAddr {
    /// The peer it was filed against.
    pub peer: NodeId,
    /// `host:port`, as it reads in the file.
    pub authority: String,
}

/// A machine we have paired with (SPEC §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// Its fingerprint, which is also its identity.
    pub id: NodeId,
    /// Its display name.
    pub name: String,
    /// The human who owns it, for fan-out addressing.
    pub owner: Option<String>,
    /// Its DER-encoded certificate, which is what TLS pins against.
    pub certificate: CertificateDer,
    /// Everywhere we know to reach it.
    pub addrs: Vec<PeerAddr>,
    /// When it proved the group key to this node (SPEC §6.2).
    pub paired_at: DateTime<Utc>,
    /// When it last answered.
    pub last_seen: Option<DateTime<Utc>>,
}

impl Peer {
    /// Addresses to try, best first.
    ///
    /// Ordered by `last_ok` descending, so the address that worked most
    /// recently is tried first and a peer that moved networks converges after
    /// one failed attempt rather than several (SPEC §5.4).
    #[must_use]
    pub fn addrs_by_preference(&self) -> Vec<&PeerAddr> {
        let mut addrs: Vec<&PeerAddr> = self.addrs.iter().collect();
        // Reverse so the most recent success sorts first.
        addrs.sort_by_key(|a| std::cmp::Reverse(a.last_ok));
        addrs
    }

    /// Record that we reached this peer at `authority`.
    pub fn mark_reached(&mut self, authority: &str, at: DateTime<Utc>) {
        if let Some(addr) = self.addrs.iter_mut().find(|a| a.authority() == authority) {
            addr.last_ok = Some(at);
        }
        self.last_seen = Some(at);
    }

    /// Take the name and owner a node reports for itself, and say whether
    /// either moved (SPEC §5.5, §6.2).
    ///
    /// A node is the only authority on what it is called, so this follows it
    /// exactly — including an owner it has stopped claiming, which is dropped
    /// rather than kept. Following it halfway would make `owner` a field that
    /// can be set and never unset, which is a second kind of stale on top of
    /// the one #37 was: a name learned at the first handshake and never
    /// revised, so a machine renamed by hand went on reading as
    /// `hivemind-node` on every other machine.
    ///
    /// The answer is whether anything changed, so a caller can leave
    /// `peers.toml` alone in the ordinary case — presence revisits every peer
    /// every minute and almost never has news.
    pub fn relabel(&mut self, name: &str, owner: Option<&str>) -> bool {
        if self.name == name && self.owner.as_deref() == owner {
            return false;
        }
        name.clone_into(&mut self.name);
        self.owner = owner.map(str::to_owned);
        true
    }

    /// Add an address we did not already know, or refresh how we learned it.
    pub fn learn_addr(&mut self, addr: PeerAddr) {
        match self
            .addrs
            .iter_mut()
            .find(|a| a.host == addr.host && a.port == addr.port)
        {
            // Already known: keep `last_ok`, which is worth more than the
            // source that reminded us it exists.
            Some(existing) => existing.source = addr.source,
            None => self.addrs.push(addr),
        }
    }
}

/// A DER-encoded certificate, stored as hex in `peers.toml` so the file stays
/// readable and diffable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateDer(Vec<u8>);

impl CertificateDer {
    /// Wrap DER bytes.
    #[must_use]
    pub fn new(der: Vec<u8>) -> Self {
        Self(der)
    }

    /// The raw DER.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The fingerprint this certificate would have.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        NodeId::from_certificate_der(&self.0)
    }
}

impl Serialize for CertificateDer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut hex = String::with_capacity(self.0.len() * 2);
        for byte in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        s.serialize_str(&hex)
    }
}

impl<'de> Deserialize<'de> for CertificateDer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let hex = String::deserialize(d)?;
        if !hex.len().is_multiple_of(2) {
            return Err(D::Error::custom("certificate hex has an odd length"));
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        for i in (0..hex.len()).step_by(2) {
            let pair = hex
                .get(i..i + 2)
                .ok_or_else(|| D::Error::custom("certificate hex split a character"))?;
            out.push(
                u8::from_str_radix(pair, 16)
                    .map_err(|_| D::Error::custom("certificate hex is not hex"))?,
            );
        }
        Ok(Self(out))
    }
}

/// The on-disk form of `peers.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct PeerBookFile {
    /// Paired peers, keyed by node id so the file is stable under diff.
    peers: BTreeMap<String, Peer>,
}

/// Why the address book could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum PeerBookError {
    /// The filesystem said no.
    #[error("{context}")]
    Io {
        /// What we were doing.
        context: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML, or not the shape we expect.
    #[error("{path} is not a readable address book: {detail}")]
    Malformed {
        /// The file.
        path: String,
        /// What the parser said.
        detail: String,
    },
    /// A certificate did not hash to the node id filed against it.
    #[error("the certificate stored for {id} does not match that fingerprint")]
    FingerprintMismatch {
        /// The offending entry.
        id: String,
    },
}

/// The address book (SPEC §4.2, §5.4).
#[derive(Debug)]
pub struct PeerBook {
    path: PathBuf,
    file: PeerBookFile,
    /// This node's own peer port, which is half of the rule in
    /// [`PeerAddr::points_at_this_node`].
    own_peer_port: u16,
    /// What the file holds that this node will not use.
    discarded: Vec<DiscardedAddr>,
}

impl PeerBook {
    /// Load `peers.toml` from `dir`, or start an empty one.
    ///
    /// # Errors
    /// [`PeerBookError::Malformed`] if the file will not parse, or
    /// [`PeerBookError::FingerprintMismatch`] if a stored certificate does not
    /// hash to the id it is filed under.
    pub fn load(dir: &Path, own_peer_port: u16) -> Result<Self, PeerBookError> {
        let path = dir.join("peers.toml");
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let file: PeerBookFile =
                    toml::from_str(&text).map_err(|e| PeerBookError::Malformed {
                        path: path.display().to_string(),
                        detail: e.message().to_owned(),
                    })?;

                // The id is the fingerprint of the certificate. If the file
                // disagrees with itself, trusting it would mean pinning TLS
                // against a certificate nobody admitted.
                for (id, peer) in &file.peers {
                    if peer.certificate.node_id() != peer.id || peer.id.to_string() != *id {
                        return Err(PeerBookError::FingerprintMismatch { id: id.clone() });
                    }
                }
                file
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PeerBookFile::default(),
            Err(source) => {
                return Err(PeerBookError::Io {
                    context: format!("could not read {}", path.display()),
                    source,
                });
            }
        };

        let mut book = Self {
            path,
            file,
            own_peer_port,
            discarded: Vec::new(),
        };
        book.discard_addrs_that_point_here();
        Ok(book)
    }

    /// What the file holds and this node refuses to use (SPEC §5.4, #29).
    #[must_use]
    pub fn discarded(&self) -> &[DiscardedAddr] {
        &self.discarded
    }

    /// Forget one address of one peer, keeping the peer.
    ///
    /// Returns whether it was there — including when the read discarded it,
    /// which is the case `doctor` sends people here for: the address is not
    /// among the peer's any more, and the file still says it.
    pub fn forget_addr(&mut self, id: NodeId, authority: &str) -> bool {
        let discarded = self.discarded.len();
        self.discarded
            .retain(|d| d.peer != id || d.authority != authority);
        let was_discarded = self.discarded.len() != discarded;

        let Some(peer) = self.file.peers.get_mut(&id.to_string()) else {
            return was_discarded;
        };
        let held = peer.addrs.len();
        peer.addrs.retain(|a| a.authority() != authority);
        peer.addrs.len() != held || was_discarded
    }

    /// Write the book back, atomically.
    ///
    /// Takes `&mut self` because it settles what the file says: an address the
    /// read discarded is gone from the file once this returns, so the list of
    /// discards — which is a statement about the file — is emptied with it.
    ///
    /// # Errors
    /// [`PeerBookError::Io`] if the write or rename fails.
    pub fn save(&mut self) -> Result<(), PeerBookError> {
        let io = |context: String| move |source| PeerBookError::Io { context, source };

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(io(format!("could not create {}", parent.display())))?;
        }

        let text = toml::to_string_pretty(&self.file).map_err(|e| PeerBookError::Malformed {
            path: self.path.display().to_string(),
            detail: e.to_string(),
        })?;

        // Same directory, atomic rename: losing the address book to a crash
        // would mean re-pairing with everybody.
        let temp = self.path.with_extension("toml.tmp");
        std::fs::write(&temp, text.as_bytes())
            .map_err(io(format!("could not write {}", temp.display())))?;
        std::fs::rename(&temp, &self.path)
            .map_err(io(format!("could not move {} into place", temp.display())))?;

        self.discarded.clear();
        Ok(())
    }

    /// Take out every address that is this node's own listener.
    ///
    /// In memory, and not written back here: `load` is a read, and a read that
    /// rewrites its input would fail on a read-only home and would eat the
    /// evidence if this rule were ever found to be wrong. `peers.toml` is the
    /// source of truth (ADR 0002), so it is edited when something asks — the
    /// next save for any reason writes the corrected book, and
    /// `hivemind peers forget-addr` asks for it now.
    fn discard_addrs_that_point_here(&mut self) {
        let own_port = self.own_peer_port;
        let mut discarded = Vec::new();
        for peer in self.file.peers.values_mut() {
            let id = peer.id;
            peer.addrs.retain(|addr| {
                if addr.points_at_this_node(own_port) {
                    discarded.push(DiscardedAddr {
                        peer: id,
                        authority: addr.authority(),
                    });
                    return false;
                }
                true
            });
        }
        self.discarded = discarded;
    }

    /// Every paired peer.
    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.file.peers.values()
    }

    /// One paired peer.
    #[must_use]
    pub fn peer(&self, id: NodeId) -> Option<&Peer> {
        self.file.peers.get(&id.to_string())
    }

    /// One paired peer, mutably.
    pub fn peer_mut(&mut self, id: NodeId) -> Option<&mut Peer> {
        self.file.peers.get_mut(&id.to_string())
    }

    /// Is this node allowed to send us mail (SPEC §6.2)?
    #[must_use]
    pub fn is_paired(&self, id: NodeId) -> bool {
        self.file.peers.contains_key(&id.to_string())
    }

    /// Every peer with this owner label, for fan-out (SPEC §8).
    ///
    /// The comparison is case-insensitive: `owner` is something a human typed
    /// into a config file, and `Matthew` and `matthew` are the same person.
    pub fn peers_owned_by<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a Peer> + 'a {
        self.file.peers.values().filter(move |p| {
            p.owner
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(owner))
        })
    }

    /// Add or replace a paired peer.
    pub fn insert_peer(&mut self, peer: Peer) {
        self.file.peers.insert(peer.id.to_string(), peer);
    }

    /// Forget a peer. Returns whether it was there.
    pub fn remove_peer(&mut self, id: NodeId) -> bool {
        self.file.peers.remove(&id.to_string()).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The peer port these tests pretend this node's listener is on.
    const OWN_PORT: u16 = 8400;

    fn certificate(seed: &[u8]) -> CertificateDer {
        CertificateDer::new(seed.to_vec())
    }

    fn peer(seed: &[u8], owner: Option<&str>) -> Peer {
        let certificate = certificate(seed);
        Peer {
            id: certificate.node_id(),
            name: format!("node-{}", seed.len()),
            owner: owner.map(str::to_owned),
            certificate,
            addrs: vec![PeerAddr::manual("10.0.0.1", 8400)],
            paired_at: Utc::now(),
            last_seen: None,
        }
    }

    /// Takes a `usize` so that loop counters can be timestamps without a cast.
    fn at(secs: usize) -> DateTime<Utc> {
        DateTime::from_timestamp(i64::try_from(secs).expect("in range"), 0).expect("in range")
    }

    #[test]
    fn a_missing_address_book_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        assert_eq!(book.peers().count(), 0);
    }

    #[test]
    fn a_node_that_renamed_itself_is_followed() {
        // #37: the name is how a person knows which machine they are talking
        // to, and this one was learned at the first handshake and never
        // revised.
        let mut peer = peer(b"their certificate", None);
        assert!(peer.relabel("archlinux", Some("matthew")));
        assert_eq!(peer.name, "archlinux");
        assert_eq!(peer.owner.as_deref(), Some("matthew"));
    }

    #[test]
    fn an_owner_a_node_has_stopped_claiming_is_dropped() {
        // Following a node halfway would leave `owner` settable and never
        // unsettable, which is a second kind of stale on top of #37's.
        let mut peer = peer(b"their certificate", Some("matthew"));
        assert!(peer.relabel("archlinux", None));
        assert_eq!(peer.owner, None);
    }

    #[test]
    fn a_name_that_has_not_moved_is_not_worth_a_write() {
        // Presence revisits every peer every minute. Reporting a change each
        // time would rewrite `peers.toml` sixty times an hour for no news.
        let mut peer = peer(b"their certificate", Some("matthew"));
        let name = peer.name.clone();
        assert!(!peer.relabel(&name, Some("matthew")));
    }

    #[test]
    fn a_peer_survives_a_save_and_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        let peer = peer(b"their certificate", Some("rafael"));
        book.insert_peer(peer.clone());
        book.save().expect("save");

        let reloaded = PeerBook::load(dir.path(), OWN_PORT).expect("reload");
        assert_eq!(reloaded.peer(peer.id), Some(&peer));
        assert!(reloaded.is_paired(peer.id));
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        book.save().expect("save");

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["peers.toml".to_owned()]);
    }

    #[test]
    fn a_certificate_that_does_not_match_its_node_id_is_refused() {
        // The id *is* the fingerprint. A file that disagrees with itself would
        // have TLS pinning against a certificate nobody admitted (SPEC §6.3).
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        let mut peer = peer(b"their certificate", None);
        peer.certificate = certificate(b"a different certificate entirely");
        book.insert_peer(peer);
        book.save().expect("save");

        assert!(matches!(
            PeerBook::load(dir.path(), OWN_PORT),
            Err(PeerBookError::FingerprintMismatch { .. })
        ));
    }

    #[test]
    fn an_unpaired_node_is_not_paired() {
        let dir = tempfile::tempdir().expect("temp dir");
        let book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        assert!(!book.is_paired(certificate(b"a stranger").node_id()));
    }

    #[test]
    fn owner_lookup_ignores_case_because_a_human_typed_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        book.insert_peer(peer(b"laptop", Some("Matthew")));
        book.insert_peer(peer(b"desktop!!", Some("matthew")));
        book.insert_peer(peer(b"someone else's", Some("rafael")));

        assert_eq!(book.peers_owned_by("matthew").count(), 2);
        assert_eq!(book.peers_owned_by("MATTHEW").count(), 2);
        assert_eq!(book.peers_owned_by("rafael").count(), 1);
        assert_eq!(book.peers_owned_by("nobody").count(), 0);
    }

    #[test]
    fn a_peer_with_no_owner_is_not_reached_by_an_owner_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        book.insert_peer(peer(b"anonymous", None));
        assert_eq!(book.peers_owned_by("matthew").count(), 0);
    }

    #[test]
    fn addresses_are_tried_most_recently_successful_first() {
        // A laptop that moved networks should converge after one failure, not
        // after working through a list in the order we happened to learn it.
        let mut peer = peer(b"roaming", None);
        peer.addrs = vec![
            PeerAddr {
                host: "office.local".to_owned(),
                port: 8400,
                source: AddrSource::Mdns,
                last_ok: Some(at(100)),
            },
            PeerAddr {
                host: "100.64.0.2".to_owned(),
                port: 8400,
                source: AddrSource::Tailscale,
                last_ok: Some(at(900)),
            },
            PeerAddr::manual("never-worked.example", 8400),
        ];

        let order: Vec<&str> = peer
            .addrs_by_preference()
            .iter()
            .map(|a| a.host.as_str())
            .collect();
        assert_eq!(
            order,
            ["100.64.0.2", "office.local", "never-worked.example"]
        );
    }

    #[test]
    fn reaching_a_peer_records_where_and_when() {
        let mut peer = peer(b"reachable", None);
        peer.mark_reached("10.0.0.1:8400", at(500));

        assert_eq!(peer.addrs[0].last_ok, Some(at(500)));
        assert_eq!(peer.last_seen, Some(at(500)));
    }

    #[test]
    fn learning_an_address_we_already_have_keeps_when_it_last_worked() {
        // Discovery telling us a peer exists must not erase the evidence that
        // we have actually reached it there.
        let mut peer = peer(b"known", None);
        peer.addrs[0].last_ok = Some(at(700));
        peer.learn_addr(PeerAddr {
            host: "10.0.0.1".to_owned(),
            port: 8400,
            source: AddrSource::Mdns,
            last_ok: None,
        });

        assert_eq!(peer.addrs.len(), 1);
        assert_eq!(peer.addrs[0].last_ok, Some(at(700)));
        assert_eq!(peer.addrs[0].source, AddrSource::Mdns);
    }

    #[test]
    fn learning_a_new_address_adds_it() {
        let mut peer = peer(b"known", None);
        peer.learn_addr(PeerAddr::manual("100.64.0.9", 8400));
        assert_eq!(peer.addrs.len(), 2);
    }

    #[test]
    fn an_ipv6_address_is_bracketed_so_the_port_is_unambiguous() {
        let addr = PeerAddr::manual("fd7a::1", 8400);
        assert_eq!(addr.authority(), "[fd7a::1]:8400");
        assert_eq!(
            PeerAddr::manual("10.0.0.1", 8400).authority(),
            "10.0.0.1:8400"
        );
    }

    #[test]
    fn an_address_book_with_offers_from_before_the_group_key_still_loads() {
        // Before ADR 0013 the book also held `pending` offers. They meant
        // "waiting for a human", which no longer exists; the peers beside
        // them are real pins and must survive the upgrade.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        let kept = peer(b"already paired", None);
        let kept_id = kept.id;
        book.insert_peer(kept);
        book.save().expect("save");

        let path = dir.path().join("peers.toml");
        let mut text = std::fs::read_to_string(&path).expect("read");
        text.push_str(
            "\n[pending.\"hm1:whatever\"]\nname = \"old offer\"\nconfirmed_by_us = false\n",
        );
        std::fs::write(&path, text).expect("write");

        let book = PeerBook::load(dir.path(), OWN_PORT).expect("an old book must still load");
        assert!(book.is_paired(kept_id));
        assert_eq!(book.peers().count(), 1);
    }

    #[test]
    fn removing_a_peer_reports_whether_it_was_there() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        let peer = peer(b"leaving", None);
        let id = peer.id;
        book.insert_peer(peer);

        assert!(book.remove_peer(id));
        assert!(!book.remove_peer(id));
        assert!(!book.is_paired(id));
    }

    #[test]
    fn a_malformed_address_book_is_reported_not_silently_emptied() {
        // Silently starting fresh would drop every trust decision the user has
        // ever made, without telling them.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("peers.toml"), b"[peers\nbroken").expect("write");

        assert!(matches!(
            PeerBook::load(dir.path(), OWN_PORT),
            Err(PeerBookError::Malformed { .. })
        ));
    }

    /// A `peers.toml` as the two machines in #29 ended up with it: the real
    /// address, and beside it this node's own loopback, written by a handshake
    /// from before #23 taught each side to believe the socket over the claim.
    ///
    /// Written out by hand rather than saved by this module, because the whole
    /// question is what happens to a file somebody else's version wrote.
    const POISONED: &str = r#"
[peers."hm1:nidx-mhdl-c4gx-u3a4-ymm5-jzsx-tcep-baeg-pjdr-vqb2-od2s-6dn6-ig2q"]
id = "6a07761c6b170d7a6c1cc319d4e6579888f080867a471ac03a70f52f0dbe41b5"
name = "arch"
owner = "matthew"
certificate = "7468656972206365727469666963617465"
paired_at = "2026-09-18T12:00:00Z"

[[peers."hm1:nidx-mhdl-c4gx-u3a4-ymm5-jzsx-tcep-baeg-pjdr-vqb2-od2s-6dn6-ig2q".addrs]]
host = "100.102.24.1"
port = 8400
source = "manual"

[[peers."hm1:nidx-mhdl-c4gx-u3a4-ymm5-jzsx-tcep-baeg-pjdr-vqb2-od2s-6dn6-ig2q".addrs]]
host = "127.0.0.1"
port = 8400
source = "manual"
"#;

    /// Write `text` as the address book in a fresh directory.
    fn book_holding(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("peers.toml");
        std::fs::write(&path, text).expect("write");
        (dir, path)
    }

    #[test]
    fn our_own_loopback_does_not_survive_the_read() {
        // #29: #23 stopped the handshake recording this, and did nothing for
        // the file that already said it. Delivery tries addresses in order, so
        // the first attempt went to this machine's own peer port.
        let (dir, _) = book_holding(POISONED);
        let book = PeerBook::load(dir.path(), OWN_PORT).expect("load");

        let peer = book.peers().next().expect("the peer itself must survive");
        let kept: Vec<String> = peer.addrs.iter().map(PeerAddr::authority).collect();
        assert_eq!(
            kept,
            ["100.102.24.1:8400"],
            "the real address stays and the loopback goes"
        );

        let discarded: Vec<&str> = book
            .discarded()
            .iter()
            .map(|d| d.authority.as_str())
            .collect();
        assert_eq!(
            discarded,
            ["127.0.0.1:8400"],
            "and what was dropped is remembered, so `doctor` can name it"
        );
    }

    #[test]
    fn a_loopback_address_on_another_port_is_a_second_daemon_and_is_kept() {
        // Two daemons on one machine is what every integration test is, and
        // there loopback is the truth. The rule is about the port as much as
        // the host: only our own listener cannot be somebody else.
        let (dir, _) = book_holding(POISONED);
        let book = PeerBook::load(dir.path(), 8401).expect("load");

        let peer = book.peers().next().expect("the peer");
        assert_eq!(
            peer.addrs.len(),
            2,
            "both addresses are reachable from here"
        );
        assert!(book.discarded().is_empty());
    }

    #[test]
    fn the_discard_is_not_written_back_by_the_read_itself() {
        // `load` is a read. The correction holds in memory and reaches the file
        // the next time the book is written for a reason of its own — which
        // keeps a read-only home loadable and leaves the evidence in place if
        // this rule is ever found to be wrong.
        let (dir, path) = book_holding(POISONED);
        let before = std::fs::read_to_string(&path).expect("read");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "loading must not rewrite the file"
        );

        book.save().expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.contains("127.0.0.1"),
            "and the first save for any reason takes it out for good: {text}"
        );
    }

    #[test]
    fn a_saved_book_has_nothing_left_to_discard() {
        // `discarded` is a statement about what the file still holds. Left
        // standing after the save that removed it, `doctor` would report a
        // line nobody can find and `forget_addr` would claim a hit on it.
        let (dir, _) = book_holding(POISONED);
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        assert_eq!(book.discarded().len(), 1);
        book.save().expect("save");
        assert!(book.discarded().is_empty());
    }

    #[test]
    fn one_address_can_be_forgotten_without_forgetting_the_peer() {
        // The escape hatch #29 asks for: losing a whole trust relationship
        // over one bad line in a list of addresses is out of proportion.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        let mut peer = peer(b"two addresses", None);
        let id = peer.id;
        peer.learn_addr(PeerAddr::manual("100.64.0.9", 8400));
        book.insert_peer(peer);

        assert!(book.forget_addr(id, "10.0.0.1:8400"));
        let left: Vec<String> = book
            .peer(id)
            .expect("the peer stays")
            .addrs
            .iter()
            .map(PeerAddr::authority)
            .collect();
        assert_eq!(left, ["100.64.0.9:8400"]);
        assert!(
            !book.forget_addr(id, "10.0.0.1:8400"),
            "forgetting it twice is not two hits"
        );
        assert!(!book.forget_addr(id, "nowhere:1"));
    }

    #[test]
    fn an_address_the_read_discarded_can_still_be_forgotten() {
        // Otherwise `doctor` would name a line in the file and the command it
        // recommends would answer "no such address": the daemon dropped it on
        // the way in, so it is not among the peer's addresses to remove.
        let (dir, path) = book_holding(POISONED);
        let mut book = PeerBook::load(dir.path(), OWN_PORT).expect("load");
        let id = book.peers().next().expect("the peer").id;

        assert!(book.forget_addr(id, "127.0.0.1:8400"));
        assert!(book.discarded().is_empty());
        book.save().expect("save");
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("127.0.0.1")
        );
    }

    #[test]
    fn only_this_machine_at_our_own_port_points_at_this_node() {
        for host in [
            "127.0.0.1",
            "127.0.1.1",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
            "0.0.0.0",
        ] {
            assert!(
                PeerAddr::manual(host, OWN_PORT).points_at_this_node(OWN_PORT),
                "{host} at our own port is us"
            );
            assert!(
                !PeerAddr::manual(host, OWN_PORT + 1).points_at_this_node(OWN_PORT),
                "{host} on another port is another daemon on this machine"
            );
        }
        for host in ["100.102.24.1", "arch.taile.ts.net", "10.0.0.1", "::2"] {
            assert!(
                !PeerAddr::manual(host, OWN_PORT).points_at_this_node(OWN_PORT),
                "{host} is somewhere else"
            );
        }
    }
}
