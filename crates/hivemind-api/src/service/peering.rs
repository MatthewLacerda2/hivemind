//! The half of the service that is about **who this node trusts, and where
//! they are** (SPEC §5, §6.2).
//!
//! Split out of `service.rs` when it grew past the size gate, along a boundary
//! that was already there: everything here answers a question about peers, and
//! nothing here touches a mailbox. A child module rather than a sibling
//! because it continues `impl MailService` and needs the private fields.

use super::*;

impl MailService {
    /// How this node introduces itself (SPEC §7.2).
    pub fn own_handshake(&self) -> crate::peer::Handshake {
        crate::peer::Handshake {
            id: self.identity.to_string(),
            name: self.name.clone(),
            owner: self.owner.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            callback_host: self.callback_host.clone(),
            callback_port: self.peer_port,
            // Reserved for v2 (SPEC §12). Sending nothing today keeps the
            // field's meaning open.
            gossip: None,
        }
    }

    /// Introduce ourselves to a host, and record what it says back (SPEC §6.2).
    ///
    /// This is the "first use" in trust-on-first-use: the host's certificate is
    /// accepted so that its fingerprint can be shown to a human, and nothing is
    /// trusted until [`MailService::confirm_pair`] is called on both sides.
    ///
    /// `host` may name a port; without one the peer port is assumed.
    ///
    /// # Errors
    /// [`ServiceError::Peer`] if the host cannot be reached,
    /// [`ServiceError::IdentityMismatch`] if it does not answer as the node it
    /// claims to be, or [`ServiceError::PeerBook`] if the offer cannot be
    /// written.
    pub async fn join(&self, host: &str) -> Result<PendingPair, ServiceError> {
        self.greet(host, AddrSource::Manual).await
    }

    /// `join`, recording how the address was come by.
    ///
    /// `join` is what a person typed; discovery reaches the same code and did
    /// not. The distinction is what `pair --trust-network` turns on: "I found
    /// this myself, on a network I control" is a different claim from
    /// "somebody typed a host", and the flag used to cover only mDNS because
    /// Tailscale's discovery went through `join` and inherited `Manual` (#21).
    async fn greet(&self, host: &str, source: AddrSource) -> Result<PendingPair, ServiceError> {
        let (hostname, port) = split_host(host, DEFAULT_PEER_PORT);
        let addr = PeerAddr {
            host: hostname.clone(),
            port,
            source,
            last_ok: None,
        };
        let authority = addr.authority();

        let client = hivemind_net::client::PeerClient::joining(&self.tls)?;
        let answer: hivemind_net::client::PeerResponse<crate::peer::Handshake> = client
            .post(&authority, "/peer/v1/handshake", &self.own_handshake())
            .await?;

        // The certificate is the identity (SPEC §6.1); the body is a claim.
        // A host whose one checkable claim is wrong is not one to record.
        let id = NodeId::from_certificate_der(&answer.certificate);
        if answer.body.id != id.to_string() {
            return Err(ServiceError::IdentityMismatch(format!(
                "{authority} calls itself {} but presented {id}",
                answer.body.id
            )));
        }
        if id == self.identity {
            return Err(ServiceError::IdentityMismatch(
                "that address is this node".to_owned(),
            ));
        }

        self.record_pairing_offer(id, &answer.body, answer.certificate, addr)?;
        self.peers()?
            .pending_pair(id)
            .cloned()
            .ok_or_else(|| ServiceError::NoSuchPeer { id: id.to_string() })
    }

    /// Turn what a human typed into a node id.
    ///
    /// Accepts the full `hm1:` form or the eight-character short form people
    /// actually compare by eye (SPEC §6.1). A short form that matches more than
    /// one peer is refused rather than guessed at — picking one would be
    /// picking who gets trusted.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if nothing matches, or more than one does.
    pub fn resolve_peer(&self, typed: &str) -> Result<NodeId, ServiceError> {
        if let Ok(id) = typed.parse::<NodeId>() {
            return Ok(id);
        }

        let peers = self.peers()?;
        let mut matches: Vec<NodeId> = peers
            .peers()
            .map(|p| p.id)
            .chain(peers.pending().map(|p| p.id))
            .filter(|id| id.short().eq_ignore_ascii_case(typed))
            .collect();
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
    /// current for known peers and never creates trust — a node we have not
    /// paired with gets nothing from being on the same LAN.
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

    /// Re-run discovery now and return how many nodes it turned up (SPEC §5.2).
    ///
    /// Tailscale is a source, never a requirement: if it is not installed, not
    /// logged in, or answers with nonsense, that is zero nodes rather than an
    /// error.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book is poisoned.
    pub async fn refresh_peers(&self) -> Result<usize, ServiceError> {
        use hivemind_net::discovery::{Tailscale as _, hosts_from_status, probe};

        let status = hivemind_net::discovery::TailscaleCli.status_json();
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

        // Each one gets a handshake, which is an introduction and not an
        // agreement: it records a pending offer that still needs a human on
        // both sides (SPEC §6.2). Without it there would be nothing for
        // `hivemind peers` to list and nothing to confirm.
        // Counted by node, not by address. The same machine answers on its
        // MagicDNS name *and* its IP — trying both is deliberate, since which
        // one resolves depends on the asking machine's DNS — so counting
        // greetings reported two nodes where `hivemind peers` then listed one
        // (#22).
        let mut greeted: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for authority in reachable {
            match self.greet(&authority, AddrSource::Tailscale).await {
                Ok(pending) => {
                    greeted.insert(pending.id);
                }
                Err(error) => tracing::debug!(%authority, %error, "could not greet a peer"),
            }
        }
        Ok(greeted.len())
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

    /// Everything waiting on a confirmation.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn pending_pairs(&self) -> Result<Vec<PendingPair>, ServiceError> {
        Ok(self.peers()?.pending().cloned().collect())
    }

    /// Record that a node introduced itself, without trusting it yet.
    ///
    /// # Errors
    /// [`ServiceError::PeerBook`] if the book cannot be written.
    pub fn record_pairing_offer(
        &self,
        id: NodeId,
        handshake: &crate::peer::Handshake,
        certificate: Vec<u8>,
        addr: PeerAddr,
    ) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;

        // Already paired: this is a peer saying hello again, not an offer.
        // Refresh where it can be reached and leave the trust decision alone.
        if let Some(peer) = peers.peer_mut(id) {
            peer.learn_addr(addr);
            peer.last_seen = Some(Utc::now());
            return peers.save().map_err(Into::into);
        }

        peers.insert_pending(PendingPair {
            id,
            name: handshake.name.clone(),
            owner: handshake.owner.clone(),
            certificate: CertificateDer::new(certificate),
            addr,
            first_seen: Utc::now(),
            confirmed_by_us: false,
        });
        peers.save()?;
        let _ = self.events.send(Event::PairPending { id });
        Ok(())
    }

    /// Confirm a pending pair from this side (SPEC §6.2).
    ///
    /// Promotes it to a real peer once *we* have agreed; the other side does
    /// the same independently, and neither will accept mail until it has.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if nothing is pending for that id.
    pub fn confirm_pair(&self, id: NodeId) -> Result<Peer, ServiceError> {
        let mut peers = self.peers()?;
        let Some(pending) = peers.remove_pending(id) else {
            return Err(ServiceError::NoSuchPeer { id: id.to_string() });
        };

        let peer = Peer {
            id: pending.id,
            name: pending.name,
            owner: pending.owner,
            certificate: pending.certificate,
            addrs: vec![pending.addr],
            paired_at: Utc::now(),
            last_seen: None,
        };
        peers.insert_peer(peer.clone());
        peers.save()?;
        Ok(peer)
    }

    /// Confirm every pending offer discovery found on the LAN (SPEC §6.2.4).
    ///
    /// For a network you fully trust and nothing else. It skips offers that
    /// arrived any other way — a node that dialled in from a manual address is
    /// not on "the network you trust", it is whoever could reach the port.
    ///
    /// # Errors
    /// [`ServiceError::PeerBook`] if the book cannot be written.
    pub fn confirm_all_discovered(&self) -> Result<Vec<Peer>, ServiceError> {
        // Both kinds of discovery, not just mDNS. A tailnet is a *stronger*
        // boundary than a LAN segment, not a weaker one — only what was
        // authenticated and authorised gets in — and it was excluded by
        // accident rather than by argument (#21).
        //
        // `Manual` is deliberately not here: somebody who typed a host made a
        // choice, and erasing it with a blanket flag would be a surprise.
        let discovered: Vec<NodeId> = self
            .peers()?
            .pending()
            .filter(|p| matches!(p.addr.source, AddrSource::Mdns | AddrSource::Tailscale))
            .map(|p| p.id)
            .collect();

        discovered
            .into_iter()
            .map(|id| self.confirm_pair(id))
            .collect()
    }

    /// Forget a peer.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] if it was not there.
    pub fn remove_peer(&self, id: NodeId) -> Result<(), ServiceError> {
        let mut peers = self.peers()?;
        if !peers.remove_peer(id) {
            peers.remove_pending(id);
        }
        peers.save().map_err(Into::into)
    }

    /// The certificates TLS should accept, for rebuilding the trust set.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn trusted_certificates(&self) -> Result<Vec<(NodeId, Vec<u8>)>, ServiceError> {
        Ok(self.peers()?.acceptable_certificates())
    }
}
