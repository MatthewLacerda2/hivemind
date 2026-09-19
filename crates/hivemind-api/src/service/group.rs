//! The half of the service that is about **which group this node is in, and
//! who else it has seen** (SPEC §6.2, ADR 0013).
//!
//! A child module for the same reason as `peering`: it continues
//! `impl MailService` and needs the private fields, and `service.rs` is the
//! file the size gate keeps naming.

use hivemind_core::group::{Group, GroupError, GroupKey, ProofError};

use super::*;
use crate::peer::Proof;

/// How many nodes outside the group are remembered as seen.
///
/// Anyone who can reach the peer port can add one (ADR 0010), so the list is
/// bounded; the oldest is forgotten first. Nothing about being seen grants
/// anything, so forgetting one costs nothing but a line in `hivemind peers`.
pub const MAX_SEEN: usize = 64;

/// Which group this node is in, without the key (SPEC §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStatus {
    /// When this node joined or created it. `None` when it is in no group.
    pub joined_at: Option<DateTime<Utc>>,
    /// How many other nodes have proved the key to this one.
    pub members: usize,
}

/// A node that answered, or was discovered, but is not in this node's group.
///
/// Kept in memory only. It is not a decision anybody made, so there is
/// nothing to preserve across a restart; discovery will see it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenNode {
    /// Its fingerprint.
    pub id: NodeId,
    /// What it called itself, if it said. A node that refused our handshake
    /// said nothing but its certificate.
    pub name: Option<String>,
    /// Who it says owns it. Unverified.
    pub owner: Option<String>,
    /// Where it was.
    pub addr: PeerAddr,
    /// When.
    pub last_seen: DateTime<Utc>,
}

impl MailService {
    /// Which group this node is in.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if a lock is poisoned.
    pub fn group_status(&self) -> Result<GroupStatus, ServiceError> {
        let joined_at = self.group()?.as_ref().map(|g| g.joined_at);
        Ok(GroupStatus {
            joined_at,
            members: self.peers()?.peers().count(),
        })
    }

    /// Whether this node is in a group at all.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the lock is poisoned.
    pub fn in_group(&self) -> Result<bool, ServiceError> {
        Ok(self.group()?.is_some())
    }

    /// Make a new group and return its code (SPEC §6.2).
    ///
    /// Refuses when this node is already in one unless `replace` is set:
    /// replacing is how a group rotates its key, and doing it by accident
    /// would cut this machine off from every other until each is given the
    /// new code.
    ///
    /// # Errors
    /// [`ServiceError::AlreadyInGroup`] without `replace`, or
    /// [`ServiceError::Group`] if the key cannot be made or stored.
    pub fn create_group(&self, replace: bool) -> Result<GroupKey, ServiceError> {
        let key = GroupKey::generate()?;
        self.set_group(key.clone(), replace)?;
        Ok(key)
    }

    /// Join the group whose code a person pasted (SPEC §6.2).
    ///
    /// Pasting the code this node already holds is a success, so running the
    /// setup command twice is harmless. A different code is refused unless
    /// `replace` is set, for the same reason as [`MailService::create_group`].
    ///
    /// # Errors
    /// [`ServiceError::Group`] if the code is not one,
    /// [`ServiceError::AlreadyInGroup`] if it is another group's and `replace`
    /// is not set.
    pub fn join_group(&self, code: &str, replace: bool) -> Result<(), ServiceError> {
        let key: GroupKey = code.parse()?;
        if self.group()?.as_ref().is_some_and(|g| g.key == key) {
            return Ok(());
        }
        self.set_group(key, replace)
    }

    fn set_group(&self, key: GroupKey, replace: bool) -> Result<(), ServiceError> {
        let mut group = self.group()?;
        if group.is_some() && !replace {
            return Err(ServiceError::AlreadyInGroup);
        }
        let joined = Group::new(key);
        joined.save(&self.home)?;
        *group = Some(joined);
        Ok(())
    }

    /// Greet every node seen outside the group, now that we may be in it.
    ///
    /// Called after joining or creating a group: the nodes discovery turned up
    /// while this one had no key are exactly the ones worth trying again, and
    /// waiting for their next mDNS announcement would make the first minute
    /// after `hivemind pair` look like it had not worked.
    pub async fn greet_everyone_seen(&self) {
        let seen = self.seen_nodes().unwrap_or_default();
        for node in seen {
            let authority = node.addr.authority();
            if let Err(error) = self.greet(&authority, node.addr.source).await {
                tracing::debug!(%authority, %error, "could not greet a node seen earlier");
            }
        }
    }

    /// A proof of the group key for the node holding `receiver_cert`, if this
    /// node is in a group.
    pub(crate) fn prove_to(&self, receiver_cert: &[u8]) -> Result<Option<Proof>, ServiceError> {
        let now = Utc::now().timestamp_millis();
        Ok(self.group()?.as_ref().map(|g| Proof {
            sent_at: now,
            mac: data_encoding::HEXLOWER.encode(&g.key.prove(
                &self.certificate,
                receiver_cert,
                now,
            )),
        }))
    }

    /// Check a proof from the node that presented `sender_cert`.
    ///
    /// # Errors
    /// [`ServiceError::NotInGroup`], saying why, if there is no group here, no
    /// proof, or a proof that does not verify.
    pub(crate) fn check_proof(
        &self,
        sender_cert: &[u8],
        proof: Option<&Proof>,
    ) -> Result<(), ServiceError> {
        let group = self.group()?;
        let Some(group) = group.as_ref() else {
            return Err(ServiceError::NotInGroup(
                "this node is not in a group yet; run `hivemind group create`, or `hivemind pair <code>`".to_owned(),
            ));
        };
        let Some(proof) = proof else {
            return Err(ServiceError::NotInGroup(
                "no proof of the group key was offered; the other node may be in no group, or running an older hivemind".to_owned(),
            ));
        };
        let mac = data_encoding::HEXLOWER
            .decode(proof.mac.as_bytes())
            .map_err(|_| ServiceError::NotInGroup("the proof is not hex".to_owned()))?;

        group
            .key
            .verify(
                sender_cert,
                &self.certificate,
                proof.sent_at,
                &mac,
                Utc::now().timestamp_millis(),
            )
            .map_err(|error| {
                ServiceError::NotInGroup(match error {
                    ProofError::Mismatch => "the two nodes are in different groups".to_owned(),
                    ProofError::Stale => {
                        "the two machines' clocks are more than an hour apart".to_owned()
                    }
                })
            })
    }

    /// Remember a node outside the group, so `hivemind peers` can show it.
    ///
    /// A node that is already a member is left alone: it is listed as one,
    /// and a second line calling it a stranger would only confuse.
    pub(crate) fn record_seen(
        &self,
        id: NodeId,
        name: Option<String>,
        owner: Option<String>,
        addr: PeerAddr,
    ) {
        if self.is_paired(id).unwrap_or(false) {
            return;
        }
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };

        let first_time = !seen.contains_key(&id);
        if first_time && seen.len() >= MAX_SEEN {
            let oldest = seen.values().min_by_key(|s| s.last_seen).map(|s| s.id);
            if let Some(oldest) = oldest {
                seen.remove(&oldest);
            }
        }

        let entry = seen.entry(id).or_insert_with(|| SeenNode {
            id,
            name: None,
            owner: None,
            addr: addr.clone(),
            last_seen: Utc::now(),
        });
        // A refusal says nothing but the certificate. Keep what mDNS said
        // earlier rather than forgetting the node's name because of it.
        if name.is_some() {
            entry.name = name;
        }
        if owner.is_some() {
            entry.owner = owner;
        }
        entry.addr = addr;
        entry.last_seen = Utc::now();
        drop(seen);

        if first_time {
            let _ = self.events.send(Event::PeerSeen { id });
        }
    }

    /// Every node seen outside the group, most recent first.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the lock is poisoned.
    pub fn seen_nodes(&self) -> Result<Vec<SeenNode>, ServiceError> {
        let mut nodes: Vec<SeenNode> = self
            .seen
            .lock()
            .map_err(|_| ServiceError::Unavailable)?
            .values()
            .cloned()
            .collect();
        nodes.sort_by_key(|n| std::cmp::Reverse(n.last_seen));
        Ok(nodes)
    }

    /// Forget a node seen outside the group. Returns whether it was there.
    pub(crate) fn forget_seen(&self, id: NodeId) -> bool {
        self.seen
            .lock()
            .is_ok_and(|mut seen| seen.remove(&id).is_some())
    }

    fn group(&self) -> Result<std::sync::MutexGuard<'_, Option<Group>>, ServiceError> {
        self.group.lock().map_err(|_| ServiceError::Unavailable)
    }
}

impl From<GroupError> for ServiceError {
    fn from(error: GroupError) -> Self {
        Self::Group(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::service;

    fn addr(host: &str) -> PeerAddr {
        PeerAddr {
            host: host.to_owned(),
            port: 8400,
            source: AddrSource::Mdns,
            last_ok: None,
        }
    }

    fn node(seed: u8) -> NodeId {
        NodeId::from_certificate_der(&[seed])
    }

    #[test]
    fn a_node_starts_in_no_group_and_a_created_one_is_held() {
        let (_dir, service) = service();
        assert!(!service.in_group().expect("in_group"));

        let key = service.create_group(false).expect("create");
        assert!(service.in_group().expect("in_group"));
        assert_eq!(key.code().parse::<GroupKey>().expect("parses"), key);
    }

    #[test]
    fn creating_a_second_group_needs_to_be_asked_for() {
        // Replacing is rotation: every other machine is cut off until it gets
        // the new code. That should never be what an accident does.
        let (_dir, service) = service();
        let first = service.create_group(false).expect("create");

        assert!(matches!(
            service.create_group(false),
            Err(ServiceError::AlreadyInGroup)
        ));
        let second = service.create_group(true).expect("rotate");
        assert_ne!(first, second, "a rotation makes a new key");
    }

    #[test]
    fn pasting_the_code_already_held_is_harmless() {
        // So running the setup command twice does not look like an error.
        let (_dir, service) = service();
        let key = service.create_group(false).expect("create");
        service
            .join_group(&key.code(), false)
            .expect("the same group again");
    }

    #[test]
    fn joining_a_different_group_needs_to_be_asked_for() {
        let (_dir, service) = service();
        service.create_group(false).expect("create");
        let other = GroupKey::from_bytes([3u8; 16]).code();

        assert!(matches!(
            service.join_group(&other, false),
            Err(ServiceError::AlreadyInGroup)
        ));
        service.join_group(&other, true).expect("replace");
        assert_eq!(
            service
                .group()
                .expect("lock")
                .as_ref()
                .map(|g| g.key.code()),
            Some(other)
        );
    }

    #[test]
    fn something_that_is_not_a_code_is_refused_as_such() {
        let (_dir, service) = service();
        assert!(matches!(
            service.join_group("hm1:not-a-code", false),
            Err(ServiceError::Group(GroupError::InvalidCode))
        ));
        assert!(!service.in_group().expect("in_group"));
    }

    #[test]
    fn the_group_survives_a_restart() {
        let (dir, service) = service();
        let key = service.create_group(false).expect("create");
        drop(service);

        let reopened = MailService::open(
            dir.path(),
            crate::service::tests::describe(NodeId::from_certificate_der(b"this node")),
            SigningKey::from_bytes(&[11u8; 32]),
        )
        .expect("reopen");
        assert_eq!(
            reopened
                .group()
                .expect("lock")
                .as_ref()
                .map(|g| g.key.clone()),
            Some(key)
        );
    }

    #[test]
    fn nodes_seen_are_bounded_and_the_oldest_goes_first() {
        // Anyone who can reach the port can add one (ADR 0010).
        let (_dir, service) = service();
        for seed in 0..=u8::try_from(MAX_SEEN).expect("small") {
            service.record_seen(node(seed), None, None, addr("10.0.0.1"));
        }

        let seen = service.seen_nodes().expect("seen");
        assert_eq!(seen.len(), MAX_SEEN);
        assert!(
            !seen.iter().any(|s| s.id == node(0)),
            "the first one seen is the one forgotten"
        );
    }

    #[test]
    fn a_refusal_does_not_forget_the_name_discovery_gave() {
        // mDNS says what a node is called; a 403 says only who it is.
        let (_dir, service) = service();
        service.record_seen(node(1), Some("laptop".to_owned()), None, addr("10.0.0.1"));
        service.record_seen(node(1), None, None, addr("10.0.0.2"));

        let seen = service.seen_nodes().expect("seen");
        assert_eq!(seen[0].name.as_deref(), Some("laptop"));
        assert_eq!(seen[0].addr.host, "10.0.0.2", "but where it is, is news");
    }

    #[test]
    fn admitting_a_node_takes_it_off_the_seen_list() {
        let (_dir, service) = service();
        let friend = hivemind_core::identity::Identity::from_seed([30u8; 32]).expect("identity");
        service.record_seen(friend.node_id(), None, None, addr("10.0.0.1"));

        service
            .admit(
                friend.node_id(),
                &crate::peer::Handshake {
                    id: friend.node_id().to_string(),
                    name: "friend".to_owned(),
                    owner: None,
                    version: "0.1.0".to_owned(),
                    callback_host: "10.0.0.1".to_owned(),
                    callback_port: 8400,
                    proof: None,
                    gossip: None,
                },
                friend.certificate_der().to_vec(),
                addr("10.0.0.1"),
            )
            .expect("admit");

        assert!(service.seen_nodes().expect("seen").is_empty());
        // And a member is never listed as a stranger afterwards either.
        service.record_seen(friend.node_id(), None, None, addr("10.0.0.1"));
        assert!(service.seen_nodes().expect("seen").is_empty());
    }

    #[test]
    fn a_node_is_announced_as_seen_once_not_every_time() {
        let (_dir, service) = service();
        let mut events = service.subscribe();
        service.record_seen(node(1), None, None, addr("10.0.0.1"));
        service.record_seen(node(1), None, None, addr("10.0.0.1"));

        assert_eq!(
            events.try_recv().ok(),
            Some(Event::PeerSeen { id: node(1) })
        );
        assert!(
            events.try_recv().is_err(),
            "the second sighting is not news"
        );
    }
}
