//! Accepting a message a paired peer delivered (SPEC §8).
//!
//! The signature is checked against the certificate this node pinned when it
//! paired, never against anything the message or the delivery carries: a peer
//! that could nominate its own key could sign as anybody.

use super::*;

impl MailService {
    /// Accept a message delivered by a paired peer.
    ///
    /// Idempotent on message id, because redelivery is always safe (SPEC §8).
    ///
    /// # Errors
    /// [`ServiceError::BadSignature`] if it does not verify against the
    /// sender's key.
    pub fn receive(&self, from: NodeId, mut message: Message) -> Result<Ulid, ServiceError> {
        // The signature is checked against the certificate we pinned, not
        // against anything in the message: a peer cannot nominate its own key.
        let verifying_key = {
            let peers = self.peers()?;
            let peer = peers.peer(from).ok_or_else(|| ServiceError::NoSuchPeer {
                id: from.to_string(),
            })?;
            verifying_key_from_certificate(peer.certificate.as_bytes())
                .ok_or(ServiceError::BadSignature)?
        };
        message
            .verify(&verifying_key)
            .map_err(|_| ServiceError::BadSignature)?;

        let id = message.id;

        // Idempotent: a redelivery of something we already hold is a success,
        // and must not reset its read state by rewriting it into `new`.
        if self.store.get(Mailbox::New, id).is_ok() || self.store.get(Mailbox::Cur, id).is_ok() {
            return Ok(id);
        }

        message.received_at = Some(Utc::now().trunc_subsecs(3));
        self.put(Mailbox::New, &message)?;

        if let Ok(mut peers) = self.peers()
            && let Some(peer) = peers.peer_mut(from)
        {
            peer.last_seen = Some(Utc::now());
            let _ = peers.save();
        }

        let _ = self.events.send(Event::MessageReceived { id });
        Ok(id)
    }
}

/// Pull the Ed25519 public key out of a DER certificate.
///
/// The `SubjectPublicKeyInfo` of an Ed25519 certificate ends with the 32-byte
/// key, and the OID that precedes it is fixed. Scanning for that OID avoids
/// pulling in a full X.509 parser for one field — and a wrong answer here
/// cannot forge anything, it can only fail to verify.
fn verifying_key_from_certificate(der: &[u8]) -> Option<hivemind_core::crypto::VerifyingKey> {
    /// `AlgorithmIdentifier` for Ed25519: SEQUENCE(6) { OID 1.3.101.112 }.
    const ED25519_SPKI_PREFIX: &[u8] =
        &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

    let start = der
        .windows(ED25519_SPKI_PREFIX.len())
        .position(|window| window == ED25519_SPKI_PREFIX)?
        + ED25519_SPKI_PREFIX.len();
    let bytes: [u8; 32] = der.get(start..start + 32)?.try_into().ok()?;
    hivemind_core::crypto::VerifyingKey::from_bytes(&bytes).ok()
}
