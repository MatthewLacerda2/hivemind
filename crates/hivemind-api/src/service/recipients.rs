//! Who a message is for, and where they are.
//!
//! Two jobs that share the address book: reading what somebody typed in a `To`
//! box, and expanding that into the nodes it reaches at this moment. SPEC §8
//! expands at send time and stores the result, so both halves are needed
//! before a message is written rather than at delivery.

use super::*;

impl MailService {
    /// Turn what somebody typed in a `To` box into a recipient.
    ///
    /// Three forms, in this order:
    ///
    /// 1. the full `hm1:` fingerprint,
    /// 2. `everyone`,
    /// 3. a **short id** of a peer we know, which is the form the interface
    ///    teaches — `peers`, `status` and `init` all print it and `pair` takes
    ///    it — and which used to be read as a person's name, delivering the
    ///    message to nobody and saying it was sent (#19),
    /// 4. anything else: a person's name.
    ///
    /// An owner's name wins over a short id that looks like it. That ordering
    /// is decided rather than discovered: what somebody called themselves is
    /// what they meant by it.
    ///
    /// # Errors
    /// [`ServiceError::NoSuchPeer`] for a short id that matches more than one
    /// peer, or for an empty string. Guessing which peer gets somebody's mail
    /// is not a thing to do by accident.
    pub fn parse_recipient(&self, typed: &str) -> Result<Recipient, ServiceError> {
        let typed = typed.trim();
        if typed.is_empty() {
            return Err(ServiceError::NoSuchPeer { id: String::new() });
        }

        if let Ok(id) = typed.parse::<NodeId>() {
            return Ok(Recipient::Node(id));
        }
        if typed.eq_ignore_ascii_case("everyone") {
            return Ok(Recipient::Everyone);
        }

        let peers = self.peers()?;

        // A person's name first, including this machine's own owner.
        let names_somebody = peers.peers().any(|p| {
            p.owner
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(typed))
        }) || self
            .owner
            .as_deref()
            .is_some_and(|o| o.eq_ignore_ascii_case(typed));
        if names_somebody {
            return Ok(Recipient::Owner(typed.to_owned()));
        }

        let mut matches = peers
            .peers()
            .map(|p| p.id)
            .chain(std::iter::once(self.identity))
            .filter(|id| id.short().eq_ignore_ascii_case(typed));

        match (matches.next(), matches.next()) {
            (Some(id), None) => Ok(Recipient::Node(id)),
            (Some(_), Some(_)) => Err(ServiceError::NoSuchPeer {
                id: typed.to_owned(),
            }),
            // Not a fingerprint, not `everyone`, not a short id we know: a
            // person we have not met. Expansion will then find nobody, and
            // `send` refuses rather than filing it as sent.
            (None, _) => Ok(Recipient::Owner(typed.to_owned())),
        }
    }

    /// Which nodes a recipient list actually reaches, at this moment.
    ///
    /// Expanded at send time and stored with the message, so a peer paired
    /// tomorrow does not retroactively receive today's mail (SPEC §8).
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book lock is poisoned.
    pub fn expand_recipients(&self, to: &[Recipient]) -> Result<Vec<NodeId>, ServiceError> {
        let peers = self.peers()?;
        let mut out: Vec<NodeId> = Vec::new();

        for recipient in to {
            match recipient {
                Recipient::Node(id) => out.push(*id),
                Recipient::Owner(owner) => {
                    out.extend(peers.peers_owned_by(owner).map(|p| p.id));
                    // `everyone` and an owner name can both name this machine.
                    if self
                        .owner
                        .as_deref()
                        .is_some_and(|o| o.eq_ignore_ascii_case(owner))
                    {
                        out.push(self.identity);
                    }
                }
                Recipient::Everyone => {
                    out.extend(peers.peers().map(|p| p.id));
                    out.push(self.identity);
                }
            }
        }

        out.sort_unstable();
        out.dedup();
        Ok(out)
    }
    /// The certificate pinned for one peer, if we are paired with it.
    ///
    /// `None` also when the address book is unreadable, because "do not trust
    /// this certificate" is the safe answer to "I cannot tell".
    #[must_use]
    pub fn certificate_of(&self, node: NodeId) -> Option<Vec<u8>> {
        let peers = self.peers().ok()?;
        peers
            .peer(node)
            .map(|peer| peer.certificate.as_bytes().to_vec())
    }
    /// Where a peer might be reached, best guess first.
    ///
    /// Empty for a peer we are not paired with: an address without a pinned
    /// certificate is not somewhere we will send mail.
    ///
    /// # Errors
    /// [`ServiceError::Unavailable`] if the address book is poisoned.
    pub fn peer_addresses(&self, node: NodeId) -> Result<Vec<String>, ServiceError> {
        Ok(self.peers()?.peer(node).map_or_else(Vec::new, |peer| {
            peer.addrs_by_preference()
                .into_iter()
                .map(PeerAddr::authority)
                .collect()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::service;

    /// A service that has already paired with `friend`.
    fn service_knowing(
        friend: &hivemind_core::identity::Identity,
    ) -> (tempfile::TempDir, MailService) {
        let (dir, service) = service();
        service
            .admit(
                friend.node_id(),
                "their-laptop",
                Some("ana"),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.2", 8400),
            )
            .expect("admit");
        (dir, service)
    }
    #[test]
    fn a_short_id_names_the_peer_it_belongs_to() {
        // It is the form the interface teaches: `peers`, `status` and `init`
        // all print it, and `pair` takes it. Issue #19 — sending to one
        // silently delivered to nobody, and fooled an agent on another
        // machine into reporting it had made contact.
        let friend = hivemind_core::identity::Identity::from_seed([5u8; 32]).expect("identity");
        let (_dir, service) = service_knowing(&friend);

        assert_eq!(
            service
                .parse_recipient(&friend.node_id().short())
                .expect("resolves"),
            Recipient::Node(friend.node_id())
        );
        // And case does not matter, since people copy it by eye.
        assert_eq!(
            service
                .parse_recipient(&friend.node_id().short().to_uppercase())
                .expect("resolves"),
            Recipient::Node(friend.node_id())
        );
    }

    #[test]
    fn the_full_form_and_everyone_still_mean_what_they_did() {
        let friend = hivemind_core::identity::Identity::from_seed([6u8; 32]).expect("identity");
        let (_dir, service) = service_knowing(&friend);

        assert_eq!(
            service
                .parse_recipient(&friend.node_id().to_string())
                .expect("resolves"),
            Recipient::Node(friend.node_id())
        );
        assert_eq!(
            service.parse_recipient("EVERYONE").expect("resolves"),
            Recipient::Everyone
        );
        assert_eq!(
            service.parse_recipient("ana").expect("resolves"),
            Recipient::Owner("ana".to_owned()),
            "a name that is not a short id is still a person"
        );
    }

    #[test]
    fn an_owner_name_wins_over_a_short_id_that_looks_like_it() {
        // Vanishingly unlikely, and the order has to be decided rather than
        // discovered: what somebody called themselves is what they meant.
        let friend = hivemind_core::identity::Identity::from_seed([7u8; 32]).expect("identity");
        let short = friend.node_id().short();
        let (_dir, service) = service();
        service
            .admit(
                friend.node_id(),
                "odd",
                Some(&short),
                friend.certificate_der().to_vec(),
                PeerAddr::manual("10.0.0.3", 8400),
            )
            .expect("admit");

        assert_eq!(
            service.parse_recipient(&short).expect("resolves"),
            Recipient::Owner(short),
            "somebody's name is what they meant by it"
        );
    }
    #[test]
    fn an_ambiguous_short_id_is_refused_rather_than_guessed() {
        // Two peers cannot share a short id in practice — it is 40 bits of a
        // hash — but guessing which one gets the mail is not a thing to do by
        // accident, so the code says so rather than relying on that.
        let (_dir, service) = service();
        assert!(matches!(
            service.parse_recipient(""),
            Err(ServiceError::NoSuchPeer { .. })
        ));
    }
}
