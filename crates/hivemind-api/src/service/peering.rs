//! The half of the service that is about **who this node trusts, and where
//! they are** (SPEC §5, §6.2).
//!
//! Split out of `service.rs` when it grew past the size gate, along a boundary
//! that was already there: everything here answers a question about peers, and
//! nothing here touches a mailbox. A child module rather than a sibling
//! because it continues `impl MailService` and needs the private fields.
//!
//! Trust comes from the group key (ADR 0013). A node that proves it in a
//! handshake is admitted — its certificate pinned in `peers.toml` — with
//! nobody asked anything; one that does not is remembered as seen, and that
//! is all.

use super::*;

/// What came of greeting a node.
#[derive(Debug, Clone)]
pub enum Met {
    /// It proved the group key, and is now a peer.
    Member(Peer),
    /// It answered, but is not in this node's group — or this node is in none.
    Stranger(SeenNode),
}

impl MailService {
    /// How this node introduces itself to the node holding `receiver_cert`
    /// (SPEC §7.2).
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the group lock is poisoned.
    pub fn own_handshake(
        &self,
        receiver_cert: &[u8],
    ) -> Result<crate::peer::Handshake, ServiceError> {
        Ok(crate::peer::Handshake {
            id: self.identity.to_string(),
            name: self.name.clone(),
            owner: self.owner.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            callback_host: self.callback_host.clone(),
            callback_port: self.peer_port,
            proof: self.prove_to(receiver_cert)?,
            gossip: self.peer_notes()?,
        })
    }

    /// Answer a handshake from the node that presented `certificate`.
    ///
    /// Admits it if its proof of the group key verifies, and answers with ours;
    /// otherwise remembers it as seen and refuses (SPEC §6.2).
    ///
    /// # Errors
    /// [`ServiceError::NotInGroup`] if the proof does not verify, saying why,
    /// or [`ServiceError::PeerBook`] if the admission cannot be written.
    pub fn answer_handshake(
        &self,
        id: NodeId,
        theirs: &crate::peer::Handshake,
        certificate: &[u8],
        addr: PeerAddr,
    ) -> Result<crate::peer::Handshake, ServiceError> {
        if let Err(refusal) = self.check_proof(certificate, theirs.proof.as_ref()) {
            self.record_seen(id, Some(theirs.name.clone()), theirs.owner.clone(), addr);
            return Err(refusal);
        }
        self.admit(
            id,
            &theirs.name,
            theirs.owner.as_deref(),
            certificate.to_vec(),
            addr,
        )?;
        self.own_handshake(certificate)
    }

    /// Contact a host discovery cannot find (SPEC §5.3).
    ///
    /// The host's certificate is accepted for the length of the handshake so
    /// the two proofs can be exchanged, and pinned only if theirs verifies.
    /// The key decides, as it does everywhere; nobody is asked to confirm.
    ///
    /// `host` may name a port; without one the peer port is assumed.
    ///
    /// # Errors
    /// [`ServiceError::Peer`] if the host cannot be reached, or
    /// [`ServiceError::IdentityMismatch`] if it does not answer as the node it
    /// claims to be.
    pub async fn join(&self, host: &str) -> Result<Met, ServiceError> {
        self.greet(host, AddrSource::Manual).await
    }

    /// `join`, recording how the address was come by.
    pub(crate) async fn greet(&self, host: &str, source: AddrSource) -> Result<Met, ServiceError> {
        let (hostname, port) = split_host(host, DEFAULT_PEER_PORT);
        let addr = PeerAddr {
            host: hostname.clone(),
            port,
            source,
            last_ok: None,
        };
        let authority = addr.authority();

        // The body is built after TLS, because our proof covers the
        // certificate the host is about to present (SPEC §6.2). A poisoned
        // lock here sends `null`, which the host refuses as unreadable — a
        // failure either way, after an earlier panic, and not worth a second
        // error path.
        let client = hivemind_net::client::PeerClient::joining(&self.tls)?;
        let answer = client
            .post_bound::<_, crate::peer::Handshake>(&authority, "/peer/v1/handshake", |cert| {
                self.own_handshake(cert).ok()
            })
            .await?;

        // The certificate is the identity (SPEC §6.1); the body is a claim.
        let id = NodeId::from_certificate_der(&answer.certificate);
        if id == self.identity {
            return Err(ServiceError::IdentityMismatch(
                "that address is this node".to_owned(),
            ));
        }

        match answer.body {
            Ok(theirs) => {
                // A host whose one checkable claim is wrong is not one to
                // record, as a member or as anything else.
                if theirs.id != id.to_string() {
                    return Err(ServiceError::IdentityMismatch(format!(
                        "{authority} calls itself {} but presented {id}",
                        theirs.id
                    )));
                }
                if self
                    .check_proof(&answer.certificate, theirs.proof.as_ref())
                    .is_ok()
                {
                    let peer = self.admit(
                        id,
                        &theirs.name,
                        theirs.owner.as_deref(),
                        answer.certificate,
                        addr,
                    )?;
                    return Ok(Met::Member(peer));
                }
                self.record_seen(id, Some(theirs.name), theirs.owner, addr.clone());
            }
            // It refused our proof, or we had none to offer. Either way it is
            // a node, and now we know which one.
            Err(hivemind_net::client::ClientError::Status { status: 403, .. }) => {
                self.record_seen(id, None, None, addr.clone());
            }
            Err(error) => return Err(error.into()),
        }

        Ok(Met::Stranger(self.seen_node(id).unwrap_or(SeenNode {
            id,
            name: None,
            owner: None,
            addr,
            last_seen: Utc::now(),
            was_a_member: false,
        })))
    }

    /// Pin a node that has proved the group key (SPEC §6.2).
    ///
    /// A node already pinned gets its address refreshed, and the name and
    /// owner it has just reported taken again rather than kept from the first
    /// meeting (#37). Its certificate needs no comparing: the id is that
    /// certificate's fingerprint, so the same id is the same certificate.
    ///
    /// # Errors
    /// [`ServiceError::PeerBook`] if the book cannot be written.
    pub fn admit(
        &self,
        id: NodeId,
        name: &str,
        owner: Option<&str>,
        certificate: Vec<u8>,
        addr: PeerAddr,
    ) -> Result<Peer, ServiceError> {
        let mut peers = self.peers()?;
        if let Some(peer) = peers.peer_mut(id) {
            revise(peer, name, owner);
            peer.learn_addr(addr);
            peer.last_seen = Some(Utc::now());
            let peer = peer.clone();
            peers.save()?;
            return Ok(peer);
        }

        let peer = Peer {
            id,
            name: name.to_owned(),
            owner: owner.map(str::to_owned),
            certificate: CertificateDer::new(certificate),
            addrs: vec![addr],
            paired_at: Utc::now(),
            last_seen: Some(Utc::now()),
        };
        peers.insert_peer(peer.clone());
        peers.save()?;
        drop(peers);

        self.forget_seen(id);
        Ok(peer)
    }

    /// Take the name and owner a peer reports about itself, nothing else
    /// (SPEC §5.5).
    ///
    /// Separate from [`Self::admit`] because the answer to *our* hello brings
    /// neither a proof to check nor an address to learn — we chose the address,
    /// and the connection was pinned to the peer's certificate before it was
    /// sent. The name in it is as fresh as the one in a hello we answer, and
    /// without this a peer that can be reached but cannot reach back would keep
    /// the name it had at the first handshake forever. That asymmetry is the
    /// case presence is shaped for: whichever direction works is the one both
    /// sides learn from.
    ///
    /// A node that is not pinned is not an error and not admitted: nothing here
    /// creates trust (ADR 0013).
    ///
    /// # Errors
    /// [`ServiceError::PeerBook`] if the book cannot be written.
    pub(crate) fn learn_identity(
        &self,
        id: NodeId,
        name: &str,
        owner: Option<&str>,
    ) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;
        let Some(peer) = peers.peer_mut(id) else {
            return Ok(());
        };
        if !revise(peer, name, owner) {
            return Ok(());
        }
        peers.save()?;
        Ok(())
    }

    /// Turn what a human typed into a node id.
    ///
    /// Accepts the full `hm1:` form or the eight-character short form people
    /// actually compare by eye (SPEC §6.1). A short form that matches more than
    /// one node is refused rather than guessed at.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if nothing matches, or more than one does.
    pub fn resolve_peer(&self, typed: &str) -> Result<NodeId, ServiceError> {
        if let Ok(id) = typed.parse::<NodeId>() {
            return Ok(id);
        }

        let paired: Vec<NodeId> = self.peers()?.peers().map(|p| p.id).collect();
        let mut matches: Vec<NodeId> = paired
            .into_iter()
            .chain(self.seen_nodes()?.into_iter().map(|s| s.id))
            .filter(|id| id.short().eq_ignore_ascii_case(typed))
            .collect();
        matches.sort();
        matches.dedup();

        match matches.as_slice() {
            [id] => Ok(*id),
            _ => Err(ServiceError::NoSuchPeer {
                id: typed.to_owned(),
            }),
        }
    }

    /// Record an address discovery found for a peer we already know.
    ///
    /// Returns whether it was one. SPEC §5.4: discovery keeps the address book
    /// current for known peers and never creates trust — a node gets nothing
    /// from being on the same LAN.
    ///
    /// # Errors
    /// [`ServiceError::PeerBook`] if the book cannot be written.
    pub fn learn_discovered_addr(&self, id: NodeId, addr: PeerAddr) -> Result<bool, ServiceError> {
        let mut peers = self.peers()?;
        let Some(peer) = peers.peer_mut(id) else {
            return Ok(false);
        };
        peer.learn_addr(addr);
        peer.last_seen = Some(Utc::now());
        peers.save()?;
        Ok(true)
    }

    /// Forget one of a peer's addresses, keeping the peer (SPEC §10).
    ///
    /// The escape hatch #29 asks for. Before it, one unusable line in a list of
    /// addresses cost the whole trust relationship: `hivemind peers remove` and
    /// pair again. It also reaches an address the read discarded, which is the
    /// case `doctor` sends people here for — the daemon is not using it and the
    /// file still says it, so forgetting it is what writes the file back.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if the peer is not in the book,
    /// [`ServiceError::NoSuchAddr`] if it is and that is not one of its
    /// addresses, or [`ServiceError::PeerBook`] if the book cannot be written.
    pub fn forget_addr(&self, id: NodeId, authority: &str) -> Result<Peer, ServiceError> {
        let mut peers = self.peers()?;
        if peers.peer(id).is_none() {
            return Err(ServiceError::NoSuchPeer { id: id.to_string() });
        }

        if !peers.forget_addr(id, authority) {
            return Err(ServiceError::NoSuchAddr {
                peer: id.short(),
                addr: authority.to_owned(),
            });
        }
        peers.save()?;
        peers
            .peer(id)
            .cloned()
            .ok_or_else(|| ServiceError::NoSuchPeer { id: id.to_string() })
    }

    /// Re-run discovery now and return how many nodes answered (SPEC §5.2).
    ///
    /// Tailscale is a source, never a requirement: if it is not installed, not
    /// logged in, or answers with nonsense, that is zero nodes rather than an
    /// error.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book is poisoned.
    pub async fn refresh_peers(&self) -> Result<usize, ServiceError> {
        self.refresh_peers_from(&hivemind_net::discovery::TailscaleCli)
            .await
    }

    /// `refresh_peers`, against a named source of `tailscale status --json`.
    ///
    /// The backend is a parameter for the reason the trait exists: a test
    /// that used the real binary would pass on a machine with no Tailscale
    /// *and* on one where the setting was ignored, which is no test at all.
    /// That is the same shape `doctor`'s optional-tools rule had to be split
    /// into, and it was found here the same way — by applying the mutation
    /// and watching nothing fail.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book is poisoned.
    pub async fn refresh_peers_from<T>(&self, backend: &T) -> Result<usize, ServiceError>
    where
        T: hivemind_net::discovery::Tailscale + Sync,
    {
        use hivemind_net::discovery::{hosts_from_status, probe};

        // `tailscale = false` means the binary is never run, not that its
        // answer is thrown away (SPEC §5.2). `hivemind peers refresh` is
        // "now rather than in thirty seconds", never "despite the setting".
        if !self.tailscale.wanted() {
            tracing::debug!("tailscale discovery is off; not refreshing from it");
            return Ok(0);
        }

        let status = backend.status_json();
        let hosts = match status {
            Ok(json) => hosts_from_status(&json),
            Err(reason) => {
                tracing::debug!(%reason, "no Tailscale peers to refresh from");
                Vec::new()
            }
        };

        let reachable = probe(
            hosts,
            self.peer_port,
            hivemind_net::discovery::PROBE_TIMEOUT,
        )
        .await;

        // Each one is greeted: a member is admitted, anybody else is listed as
        // seen. Counted by node, not by address — the same machine answers on
        // its MagicDNS name *and* its IP, and trying both is deliberate, since
        // which resolves depends on the asking machine's DNS (#22).
        let mut answered: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for authority in reachable {
            match self.greet(&authority, AddrSource::Tailscale).await {
                Ok(Met::Member(peer)) => {
                    answered.insert(peer.id);
                }
                Ok(Met::Stranger(node)) => {
                    answered.insert(node.id);
                }
                Err(error) => tracing::debug!(%authority, %error, "could not greet a peer"),
            }
        }
        Ok(answered.len())
    }

    /// Is this node allowed to send us mail?
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn is_paired(&self, id: NodeId) -> Result<bool, ServiceError> {
        Ok(self.peers()?.is_paired(id))
    }

    /// Every paired peer.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn paired_peers(&self) -> Result<Vec<Peer>, ServiceError> {
        Ok(self.peers()?.peers().cloned().collect())
    }

    /// Forget a peer, or a node merely seen.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if it was neither.
    pub fn remove_peer(&self, id: NodeId) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;
        let was_peer = peers.remove_peer(id);
        peers.save()?;
        drop(peers);

        if self.forget_seen(id) || was_peer {
            Ok(())
        } else {
            Err(ServiceError::NoSuchPeer { id: id.to_string() })
        }
    }

    /// One node seen outside the group.
    fn seen_node(&self, id: NodeId) -> Option<SeenNode> {
        self.seen_nodes()
            .ok()?
            .into_iter()
            .find(|node| node.id == id)
    }
}

/// Follow what a node now says about itself, saying so when it has changed.
///
/// The one place a revision happens, so that "learned once and never revised"
/// cannot come back in one path and not the other (#37). A rename is rare and
/// a person who renamed a machine wants to see it land, so it is worth a line;
/// the unchanged case is every hello of every minute and is worth none.
fn revise(peer: &mut Peer, name: &str, owner: Option<&str>) -> bool {
    if !peer.relabel(name, owner) {
        return false;
    }
    tracing::info!(
        peer = %peer.id.short(),
        name = %peer.name,
        owner = ?peer.owner,
        "a peer revised what it calls itself"
    );
    true
}

#[cfg(test)]
mod tests {
    use hivemind_core::peer::NodeId;
    use hivemind_core::peerbook::PeerAddr;

    use crate::service::{MailService, ServiceError};

    /// A member of this node's group with two ways to reach it.
    fn admitted(service: &MailService) -> NodeId {
        let friend = hivemind_core::identity::Identity::from_seed([29; 32]).expect("identity");
        let id = friend.node_id();
        service
            .admit(
                id,
                "arch",
                Some("matthew"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("100.102.24.1", 8400),
            )
            .expect("admit");
        service
            .admit(
                id,
                "arch",
                Some("matthew"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("192.168.1.9", 8400),
            )
            .expect("admit again");
        id
    }

    #[test]
    fn a_second_handshake_revises_the_name_and_the_owner() {
        // #37: the Arch machine renamed itself from `hivemind-node` to
        // `archlinux` and confirmed it in its own `status`; every machine that
        // had already met it went on saying `hivemind-node` through several
        // handshakes and deliveries. The name exists so a person knows which
        // machine they are talking to, and one learned once is one that stops
        // being true the first time somebody edits their config.
        let (_dir, service) = crate::service::tests::service();
        let friend = hivemind_core::identity::Identity::from_seed([37; 32]).expect("identity");
        let id = friend.node_id();
        let addr = PeerAddr::manual("100.116.89.94", 8400);

        service
            .admit(
                id,
                "hivemind-node",
                None,
                friend.certificate_der().to_vec(),
                addr.clone(),
            )
            .expect("the first handshake");
        let peer = service
            .admit(
                id,
                "archlinux",
                Some("matthew"),
                friend.certificate_der().to_vec(),
                addr,
            )
            .expect("the second one");

        assert_eq!(peer.name, "archlinux");
        assert_eq!(peer.owner.as_deref(), Some("matthew"));
    }

    #[test]
    fn admitting_a_node_never_leaves_last_seen_empty() {
        // The other half of #37, and the reason it is here rather than in the
        // issue: the reporter saw a node's `last_seen` filled while it was
        // merely seen, `null` the moment it was paired, and filled again on
        // the next contact. That was the pairing of the day rewriting the
        // record from scratch — the confirm step, which ADR 0013 deleted
        // along with the whole two-sided confirmation. Admission has built the
        // record since, and this is what stops the field being dropped again.
        // The *first* admission, deliberately: it is the one that builds the
        // record, and a second one through the already-pinned branch would
        // fill the field back in and assert nothing.
        let (_dir, service) = crate::service::tests::service();
        let friend = hivemind_core::identity::Identity::from_seed([38; 32]).expect("identity");
        let peer = service
            .admit(
                friend.node_id(),
                "leonardos-macbook-air",
                Some("leonardo"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.1.101.30", 8400),
            )
            .expect("admit");
        assert!(
            peer.last_seen.is_some(),
            "a node that has just proved the key was heard from a moment ago"
        );
    }

    #[test]
    fn forgetting_one_address_keeps_the_peer_and_the_others() {
        // #29: losing a trust relationship over one bad line in a list of
        // addresses is out of proportion, and it was the only escape.
        let (_dir, service) = crate::service::tests::service();
        let id = admitted(&service);

        let peer = service
            .forget_addr(id, "100.102.24.1:8400")
            .expect("the address goes");
        let left: Vec<String> = peer.addrs.iter().map(PeerAddr::authority).collect();
        assert_eq!(left, ["192.168.1.9:8400"]);
        assert!(
            service
                .paired_peers()
                .expect("peers")
                .iter()
                .any(|p| p.id == id),
            "the peer itself stays paired"
        );
    }

    #[test]
    fn forgetting_an_address_survives_the_daemon_restarting() {
        // It is the file that has to change: a correction held only in memory
        // is one the next start undoes.
        let (dir, service) = crate::service::tests::service();
        let id = admitted(&service);
        service
            .forget_addr(id, "100.102.24.1:8400")
            .expect("the address goes");
        drop(service);

        let book = hivemind_core::peerbook::PeerBook::load(dir.path(), 8400).expect("reload");
        let left: Vec<String> = book
            .peer(id)
            .expect("the peer")
            .addrs
            .iter()
            .map(PeerAddr::authority)
            .collect();
        assert_eq!(left, ["192.168.1.9:8400"]);
    }

    #[test]
    fn an_address_the_peer_does_not_have_is_refused_rather_than_shrugged_at() {
        // A typo answering "done" would have somebody believe they had fixed
        // the thing `doctor` complained about.
        let (_dir, service) = crate::service::tests::service();
        let id = admitted(&service);

        assert!(matches!(
            service.forget_addr(id, "10.0.0.1:8400"),
            Err(ServiceError::NoSuchAddr { .. })
        ));
    }

    #[test]
    fn forgetting_an_address_of_a_peer_we_do_not_know_says_so() {
        let (_dir, service) = crate::service::tests::service();
        assert!(matches!(
            service.forget_addr(NodeId::from_certificate_der(b"a stranger"), "10.0.0.1:8400"),
            Err(ServiceError::NoSuchPeer { .. })
        ));
    }
}
