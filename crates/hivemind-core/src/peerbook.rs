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
}

impl PeerBook {
    /// Load `peers.toml` from `dir`, or start an empty one.
    ///
    /// # Errors
    /// [`PeerBookError::Malformed`] if the file will not parse, or
    /// [`PeerBookError::FingerprintMismatch`] if a stored certificate does not
    /// hash to the id it is filed under.
    pub fn load(dir: &Path) -> Result<Self, PeerBookError> {
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

        Ok(Self { path, file })
    }

    /// Write the book back, atomically.
    ///
    /// # Errors
    /// [`PeerBookError::Io`] if the write or rename fails.
    pub fn save(&self) -> Result<(), PeerBookError> {
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
            .map_err(io(format!("could not move {} into place", temp.display())))
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
        let book = PeerBook::load(dir.path()).expect("load");
        assert_eq!(book.peers().count(), 0);
    }

    #[test]
    fn a_peer_survives_a_save_and_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path()).expect("load");
        let peer = peer(b"their certificate", Some("rafael"));
        book.insert_peer(peer.clone());
        book.save().expect("save");

        let reloaded = PeerBook::load(dir.path()).expect("reload");
        assert_eq!(reloaded.peer(peer.id), Some(&peer));
        assert!(reloaded.is_paired(peer.id));
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let book = PeerBook::load(dir.path()).expect("load");
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
        let mut book = PeerBook::load(dir.path()).expect("load");
        let mut peer = peer(b"their certificate", None);
        peer.certificate = certificate(b"a different certificate entirely");
        book.insert_peer(peer);
        book.save().expect("save");

        assert!(matches!(
            PeerBook::load(dir.path()),
            Err(PeerBookError::FingerprintMismatch { .. })
        ));
    }

    #[test]
    fn an_unpaired_node_is_not_paired() {
        let dir = tempfile::tempdir().expect("temp dir");
        let book = PeerBook::load(dir.path()).expect("load");
        assert!(!book.is_paired(certificate(b"a stranger").node_id()));
    }

    #[test]
    fn owner_lookup_ignores_case_because_a_human_typed_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path()).expect("load");
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
        let mut book = PeerBook::load(dir.path()).expect("load");
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
        let mut book = PeerBook::load(dir.path()).expect("load");
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

        let book = PeerBook::load(dir.path()).expect("an old book must still load");
        assert!(book.is_paired(kept_id));
        assert_eq!(book.peers().count(), 1);
    }

    #[test]
    fn removing_a_peer_reports_whether_it_was_there() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut book = PeerBook::load(dir.path()).expect("load");
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
            PeerBook::load(dir.path()),
            Err(PeerBookError::Malformed { .. })
        ));
    }
}
