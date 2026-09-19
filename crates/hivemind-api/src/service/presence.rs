//! The half of the service that is about **who is up right now** (SPEC §5.5).
//!
//! A sibling of `peering.rs` in the same way `peering.rs` is a sibling of the
//! mailbox: everything here answers "is that machine there", and nothing here
//! touches a mailbox. A child module rather than a separate file beside
//! `service.rs` because it continues `impl MailService` and needs the private
//! fields.
//!
//! **Presence is not a fact and is not written down.** `last_seen` is — it
//! survives a restart and means "this node existed and answered, once" — but
//! online lives in memory and a daemon that has just started knows nothing
//! about anybody until the first round. That is correct: a `peers.toml` that
//! claimed somebody was online would be claiming it about a moment that has
//! passed.

use std::collections::HashSet;

use super::*;
use crate::peer::{Hello, PeerNote, SessionNote};

/// What is known about a peer that said hello.
#[derive(Debug, Clone)]
pub struct Presence {
    /// When its last hello arrived.
    pub since: DateTime<Utc>,
    /// The sessions it reported then (SPEC §9.3).
    pub sessions: Vec<SessionNote>,
}

impl MailService {
    /// How this node introduces itself to the node holding `receiver_cert`
    /// every `presence_interval` (SPEC §5.5).
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the group or address book lock is
    /// poisoned.
    pub fn own_hello(&self, receiver_cert: &[u8]) -> Result<Hello, ServiceError> {
        Ok(Hello {
            id: self.identity.to_string(),
            name: self.name.clone(),
            owner: self.owner.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            callback_host: self.callback_host.clone(),
            callback_port: self.peer_port,
            proof: self.prove_to(receiver_cert)?,
            peers: self.peer_notes()?,
            sessions: Self::open_sessions(),
            up: self.hints_for(NodeId::from_certificate_der(receiver_cert)),
        })
    }

    /// Answer a hello from the node that presented `certificate`.
    ///
    /// Marks it online, learns where it is, takes its peer list, wakes
    /// delivery for it, and answers in kind. A node that cannot prove the
    /// group key is **dropped** rather than flagged: SPEC §6.2.4 says a member
    /// whose key was rotated away stops receiving mail, and a peer left in
    /// `peers.toml` with an "out of the group" bit beside it would be a second
    /// state to keep in step with the first. It comes back through the
    /// ordinary path if it pastes the new code.
    ///
    /// # Errors
    /// [`ServiceError::NotInGroup`] if the proof does not verify, which the
    /// router turns into `403 not_paired`.
    pub fn answer_hello(
        &self,
        id: NodeId,
        theirs: &Hello,
        certificate: &[u8],
        addr: PeerAddr,
    ) -> Result<Hello, ServiceError> {
        if let Err(refusal) = self.check_proof(certificate, theirs.proof.as_ref()) {
            self.retire(id, theirs, addr);
            return Err(refusal);
        }

        // A hello from a node not yet pinned admits it, exactly as a handshake
        // would. It proved the key; there is nothing else to ask.
        self.admit(
            id,
            &theirs.name,
            theirs.owner.as_deref(),
            certificate.to_vec(),
            addr,
        )?;

        self.mark_online(id, theirs.sessions.clone());
        // What the next round passes on as "X is up", so a node that reached
        // only one member is visible to the rest within an interval rather
        // than whenever each of them happens to try it.
        self.hint(id);
        self.absorb(&theirs.peers)?;
        for hinted in &theirs.up {
            self.follow_up(hinted);
        }

        // SPEC §8's "Monday morning": the laptop is demonstrably up, so the
        // queue goes now rather than at the end of a backoff that may be five
        // minutes long.
        self.wake_delivery(id);

        self.own_hello(certificate)
    }

    /// Say hello to everybody, and chase anything worth chasing (SPEC §5.5).
    ///
    /// One request per peer, none of them held open, all of them at once: a
    /// round takes as long as the slowest peer rather than the sum, which
    /// matters on a tailnet where most of the machines are asleep and each
    /// costs a full connection timeout.
    ///
    /// Nothing here returns an error. A peer that did not answer is offline,
    /// which is an outcome rather than a failure, and one unreachable machine
    /// must not stop the round reaching the rest.
    pub async fn presence_round(&self) {
        let peers = match self.paired_peers() {
            Ok(peers) => peers,
            Err(error) => {
                tracing::warn!(%error, "could not read the address book for a presence round");
                return;
            }
        };

        let greetings = peers.iter().map(|peer| self.say_hello_to(peer));
        futures_util::future::join_all(greetings).await;

        // Gossip about a node we do not peer with, and another member's
        // "X is up". Both are claims, and this is the attempt they buy.
        for authority in self.take_candidates() {
            if let Err(error) = self.greet(&authority, AddrSource::Gossip).await {
                tracing::debug!(%authority, %error, "could not greet a node we were told about");
            }
        }
    }

    /// Say hello to one peer, at the first address that answers.
    ///
    /// Marks it online on an answer and offline on running out of addresses.
    /// An answer that cannot prove the key to *us* is refused by the ordinary
    /// path — we are the ones checking — so what is handled here is only
    /// whether anybody was there.
    async fn say_hello_to(&self, peer: &Peer) {
        let trusted = hivemind_net::tls::TrustedPeers::new(vec![(
            peer.id,
            peer.certificate.as_bytes().to_vec(),
        )]);
        let client = match hivemind_net::client::PeerClient::pinned(&self.tls, trusted) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, peer = %peer.id.short(), "could not build a client");
                return;
            }
        };

        let hello = match self.own_hello(peer.certificate.as_bytes()) {
            Ok(hello) => hello,
            Err(error) => {
                tracing::warn!(%error, "could not compose a hello");
                return;
            }
        };

        for addr in peer.addrs_by_preference() {
            let authority = addr.authority();
            match client
                .post::<_, Hello>(&authority, "/peer/v1/hello", &hello)
                .await
            {
                Ok(answer) => {
                    self.take_in(peer.id, &answer.body);
                    // The address worked, so the next message tries it first.
                    if let Err(error) = self.record_reached(peer.id, &authority, Utc::now()) {
                        tracing::debug!(%error, %authority, "could not record a working address");
                    }
                    return;
                }
                Err(error) => {
                    tracing::debug!(
                        peer = %peer.id.short(),
                        %authority,
                        %error,
                        "no answer to a hello"
                    );
                }
            }
        }

        // Every address tried and none answered. That is the same evidence a
        // failed delivery gives, and SPEC §5.5 says it wins over any hello.
        self.mark_offline(peer.id);
    }

    /// Take what a peer said in answer to our hello.
    ///
    /// The same work `answer_hello` does on the inbound side, minus the proof:
    /// the answer reached us over a connection pinned to that peer's
    /// certificate, and it is *our* key that decides what we accept, which we
    /// have already checked by being willing to talk to it.
    fn take_in(&self, id: NodeId, theirs: &Hello) {
        self.mark_online(id, theirs.sessions.clone());
        self.hint(id);
        if let Err(error) = self.absorb(&theirs.peers) {
            tracing::debug!(%error, "could not take in a peer list");
        }
        for hinted in &theirs.up {
            self.follow_up(hinted);
        }
        self.wake_delivery(id);
    }

    /// Note that `id` was heard from, with the sessions it reported.
    ///
    /// Announces `peer.online` only on the edge, so a peer saying hello every
    /// minute does not become an event every minute.
    pub fn mark_online(&self, id: NodeId, sessions: Vec<SessionNote>) {
        let was_online = self.is_online(id);
        if let Ok(mut presence) = self.presence.lock() {
            presence.insert(
                id,
                Presence {
                    since: Utc::now(),
                    sessions,
                },
            );
        }
        if !was_online {
            let _ = self.events.send(Event::PeerOnline { id });
        }
    }

    /// Note that `id` could not be reached, so it is not online any more.
    ///
    /// A delivery failure is the evidence: it is stronger and more recent than
    /// any hello, which is why there is no ping (SPEC §5.5).
    pub fn mark_offline(&self, id: NodeId) {
        let removed = self
            .presence
            .lock()
            .is_ok_and(|mut presence| presence.remove(&id).is_some());
        if removed {
            let _ = self.events.send(Event::PeerOffline { id });
        }
    }

    /// Is this peer up?
    ///
    /// True while its last hello is younger than two intervals — one missed
    /// round is a dropped packet, two is a machine that has gone.
    #[must_use]
    pub fn is_online(&self, id: NodeId) -> bool {
        self.presence_of(id).is_some()
    }

    /// What this peer last reported, if it is still online.
    #[must_use]
    pub fn presence_of(&self, id: NodeId) -> Option<Presence> {
        let presence = self.presence.lock().ok()?;
        let found = presence.get(&id)?;
        let age = Utc::now()
            .signed_duration_since(found.since)
            .to_std()
            .ok()?;
        still_here(age, self.presence_interval).then(|| found.clone())
    }

    /// Every peer believed to be up, and the sessions each reported.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn online_peers(&self) -> Result<Vec<(NodeId, Presence)>, ServiceError> {
        Ok(self
            .paired_peers()?
            .into_iter()
            .filter_map(|peer| Some((peer.id, self.presence_of(peer.id)?)))
            .collect())
    }

    /// Ask the delivery worker to try `id` now, ignoring its backoff.
    pub fn wake_delivery(&self, id: NodeId) {
        if let Ok(mut woken) = self.woken.lock() {
            woken.insert(id);
        }
    }

    /// Take the set of peers to try now, clearing it.
    ///
    /// Draining rather than reading: a wake is one instruction, and leaving it
    /// set would make every subsequent pass ignore that peer's backoff for as
    /// long as it stayed unreachable — which is the herd the backoff exists to
    /// prevent.
    #[must_use]
    pub fn take_woken(&self) -> HashSet<NodeId> {
        self.woken
            .lock()
            .map(|mut woken| std::mem::take(&mut *woken))
            .unwrap_or_default()
    }

    /// Addresses worth a hello at the next round, taken and cleared.
    ///
    /// These come from gossip about a node we do not yet peer with, and from
    /// another member's "X is up" hint. Both are unverified claims, so they
    /// buy an attempt and nothing else.
    #[must_use]
    pub fn take_candidates(&self) -> Vec<String> {
        self.candidates
            .lock()
            .map(|mut set| std::mem::take(&mut *set).into_iter().collect())
            .unwrap_or_default()
    }

    /// Say that `id` was heard from, so the next round can pass it on.
    fn hint(&self, id: NodeId) {
        if let Ok(mut hints) = self.hints.lock() {
            hints.insert(id, Utc::now());
        }
    }

    /// Who to tell `receiver` about.
    ///
    /// Hints **expire** rather than being taken. Draining them was the first
    /// shape and it was wrong twice over: the first hello composed after a
    /// peer was heard from is the *answer to that peer*, so the hint went
    /// straight back to the node it was about and nobody else ever saw it.
    ///
    /// One interval of life means every peer greeted in the next round hears
    /// it once, and the news stops rather than circulating.
    fn hints_for(&self, receiver: NodeId) -> Vec<String> {
        let Ok(mut hints) = self.hints.lock() else {
            return Vec::new();
        };
        let now = Utc::now();
        hints.retain(|_, at| {
            now.signed_duration_since(*at)
                .to_std()
                .is_ok_and(|age| age < self.presence_interval)
        });
        hints
            .keys()
            .filter(|id| **id != receiver)
            .map(ToString::to_string)
            .collect()
    }

    /// Act on somebody else's "X is up".
    ///
    /// Never believed (SPEC §5.5): a peer we know and think is down is queued
    /// for a hello of our own, and it is that answer — not the hint — that
    /// marks it online.
    fn follow_up(&self, hinted: &str) {
        let Ok(id) = hinted.parse::<NodeId>() else {
            return;
        };
        if id == self.identity || self.is_online(id) {
            return;
        }
        let Ok(peers) = self.peers() else { return };
        let Some(peer) = peers.peer(id) else { return };
        let addrs: Vec<String> = peer
            .addrs_by_preference()
            .into_iter()
            .map(PeerAddr::authority)
            .collect();
        drop(peers);
        self.note_candidates(addrs);
    }

    /// This node's own peer list, as a hello carries it (SPEC §5.4).
    pub(super) fn peer_notes(&self) -> Result<Vec<PeerNote>, ServiceError> {
        Ok(self
            .paired_peers()?
            .into_iter()
            .map(|peer| PeerNote {
                id: peer.id.to_string(),
                name: peer.name.clone(),
                owner: peer.owner.clone(),
                addrs: peer
                    .addrs_by_preference()
                    .into_iter()
                    .map(PeerAddr::authority)
                    .collect(),
            })
            .collect())
    }

    /// Take a member's peer list into the address book (SPEC §5.4).
    ///
    /// A node already pinned gets its addresses topped up. One we have never
    /// met becomes a candidate for a handshake, which is where the group key
    /// decides whether it is anybody. Nothing here creates trust.
    fn absorb(&self, notes: &[PeerNote]) -> Result<(), ServiceError> {
        let mut candidates = Vec::new();
        for note in notes {
            let Ok(id) = note.id.parse::<NodeId>() else {
                continue;
            };
            if id == self.identity {
                continue;
            }
            for authority in &note.addrs {
                let (host, port) = super::split_host(authority, DEFAULT_PEER_PORT);
                let addr = PeerAddr {
                    host,
                    port,
                    source: AddrSource::Gossip,
                    last_ok: None,
                };
                if !self.learn_discovered_addr(id, addr)? {
                    candidates.push(authority.clone());
                }
            }
        }
        self.note_candidates(candidates);
        Ok(())
    }

    /// Queue addresses for a greeting at the next presence round.
    fn note_candidates(&self, addrs: Vec<String>) {
        if addrs.is_empty() {
            return;
        }
        if let Ok(mut candidates) = self.candidates.lock() {
            candidates.extend(addrs);
        }
    }

    /// Drop a node that can no longer prove the key (SPEC §6.2.4).
    fn retire(&self, id: NodeId, theirs: &Hello, addr: PeerAddr) {
        let was_a_peer = self
            .peers()
            .ok()
            .is_some_and(|mut peers| peers.remove_peer(id) && peers.save().is_ok());
        if was_a_peer {
            tracing::info!(
                peer = %id.short(),
                "dropped: it can no longer prove the group key"
            );
        }
        self.mark_offline(id);
        self.record_seen(id, Some(theirs.name.clone()), theirs.owner.clone(), addr);
    }

    /// The sessions open on this machine (SPEC §9.3).
    ///
    /// Empty until #52 puts a registry behind it. The field travels now so
    /// that filling it later is not a protocol change.
    fn open_sessions() -> Vec<SessionNote> {
        Vec::new()
    }
}

/// Is a hello of this age still worth believing (SPEC §5.5)?
///
/// Two intervals: one missed round is a dropped packet on a network that
/// drops packets, and two is a machine that has gone. A judgement rather than
/// a lookup, so both sides of the boundary can be tested without waiting two
/// minutes — the shape `doctor`'s "optional tools" rule had to be split into
/// for the same reason.
fn still_here(age: std::time::Duration, interval: std::time::Duration) -> bool {
    // Saturating, not checked: an interval too large to double is still an
    // interval, and `checked_mul` returning `None` there would mark every
    // peer permanently offline — which is how the test that found this read.
    //
    // Zero stays zero, and that is presence turned off: nobody is online
    // under it, including a peer that said hello a moment ago.
    age < interval.saturating_mul(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_hello_counts_for_two_intervals_and_not_a_moment_longer() {
        let interval = Duration::from_mins(1);

        assert!(still_here(Duration::ZERO, interval), "just arrived");
        assert!(
            still_here(Duration::from_secs(90), interval),
            "one missed round is a dropped packet, not an absence"
        );
        assert!(
            !still_here(Duration::from_mins(2), interval),
            "exactly two intervals is already too old"
        );
        assert!(!still_here(Duration::from_secs(121), interval));
    }

    #[test]
    fn presence_turned_off_means_nobody_is_online() {
        // `presence_interval = 0` stops the rounds, so nothing would ever
        // refresh a peer's entry. Reporting the last one forever would be
        // worse than reporting nothing.
        assert!(!still_here(Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn an_interval_too_large_to_double_still_believes_the_hello() {
        // Nobody configures this, but `checked_mul` has two answers and the
        // wrong one would make every peer permanently offline.
        assert!(still_here(Duration::from_secs(1), Duration::MAX));
    }
}
