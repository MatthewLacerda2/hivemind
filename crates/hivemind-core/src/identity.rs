//! This node's keypair and self-signed certificate (SPEC §6.1).
//!
//! One Ed25519 key does two jobs: it signs messages, and it is the key inside
//! the self-signed X.509 certificate that peers pin by fingerprint (SPEC §6.3).
//! The [`NodeId`] is the SHA-256 of that certificate's DER encoding, so the
//! certificate has to exist before this node can put a `from` on anything.

use std::path::Path;

use crate::crypto::SigningKey;
use crate::peer::NodeId;

/// The private key file name under `identity/`.
const KEY_FILE: &str = "node.key";
/// The certificate file name under `identity/`.
const CERT_FILE: &str = "node.crt";

/// Why an identity could not be loaded or created.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The filesystem said no.
    #[error("{context}")]
    Io {
        /// What we were trying to do.
        context: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The system had no randomness to give.
    #[error("could not generate a key: no randomness available")]
    NoRandomness,
    /// The certificate could not be built.
    #[error("could not create the node certificate: {0}")]
    Certificate(String),
    /// The key file on disk is not a key.
    #[error("{path} is not a usable node key")]
    MalformedKey {
        /// The offending file.
        path: String,
    },
}

/// This node's identity.
#[derive(Debug, Clone)]
pub struct Identity {
    node_id: NodeId,
    signing_key: SigningKey,
    certificate_der: Vec<u8>,
}

impl Identity {
    /// Generate a fresh keypair and self-signed certificate.
    ///
    /// # Errors
    /// [`IdentityError::NoRandomness`] or [`IdentityError::Certificate`].
    pub fn generate() -> Result<Self, IdentityError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| IdentityError::NoRandomness)?;
        Self::from_seed(seed)
    }

    /// Build an identity from a known 32-byte seed.
    ///
    /// The certificate is derived deterministically from the seed, so the same
    /// seed always yields the same [`NodeId`] — which is what makes identity
    /// reproducible in tests.
    ///
    /// # Errors
    /// [`IdentityError::Certificate`] if the certificate cannot be built.
    pub fn from_seed(seed: [u8; 32]) -> Result<Self, IdentityError> {
        use ed25519_dalek::pkcs8::EncodePrivateKey as _;

        let signing_key = SigningKey::from_bytes(&seed);
        let pkcs8 = signing_key
            .to_pkcs8_der()
            .map_err(|e| IdentityError::Certificate(e.to_string()))?;

        // rcgen gets the key we already have rather than generating its own, so
        // that the certificate and the message signatures are the same identity
        // rather than two that happen to travel together.
        let key_der = rustls_pki_types::PrivatePkcs8KeyDer::from(pkcs8.as_bytes());
        let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&key_der, &rcgen::PKCS_ED25519)
            .map_err(|e| IdentityError::Certificate(e.to_string()))?;

        let mut params = rcgen::CertificateParams::new(Vec::new())
            .map_err(|e| IdentityError::Certificate(e.to_string()))?;
        // No CA and no hostname verification: peers pin the fingerprint, so the
        // subject is documentation, not a trust input (SPEC §6.3).
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "hivemind node");

        let certificate = params
            .self_signed(&key_pair)
            .map_err(|e| IdentityError::Certificate(e.to_string()))?;
        let certificate_der = certificate.der().to_vec();

        Ok(Self {
            node_id: NodeId::from_certificate_der(&certificate_der),
            signing_key,
            certificate_der,
        })
    }

    /// Load the identity in `dir`, creating one if it is not there yet.
    ///
    /// The private key is written with mode `0600` (SPEC §4.3).
    ///
    /// # Errors
    /// [`IdentityError::Io`] if the files cannot be read or written,
    /// [`IdentityError::MalformedKey`] if the key file is not 32 bytes.
    pub fn load_or_create(dir: &Path) -> Result<Self, IdentityError> {
        let key_path = dir.join(KEY_FILE);

        if let Ok(bytes) = std::fs::read(&key_path) {
            let seed = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                IdentityError::MalformedKey {
                    path: key_path.display().to_string(),
                }
            })?;
            return Self::from_seed(seed);
        }

        std::fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
            context: format!("could not create {}", dir.display()),
            source,
        })?;

        let identity = Self::generate()?;
        write_private(&key_path, identity.signing_key.to_bytes().as_slice())?;

        let cert_path = dir.join(CERT_FILE);
        std::fs::write(&cert_path, &identity.certificate_der).map_err(|source| {
            IdentityError::Io {
                context: format!("could not write {}", cert_path.display()),
                source,
            }
        })?;

        Ok(identity)
    }

    /// This node's fingerprint.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// The key that signs messages.
    #[must_use]
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// The DER-encoded self-signed certificate that peers pin.
    #[must_use]
    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }
}

/// Write a file only the owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), IdentityError> {
    let io = |source: std::io::Error| IdentityError::Io {
        context: format!("could not write {}", path.display()),
        source,
    };

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        // Created with 0600 rather than chmodded afterwards: between the two
        // there would be a moment when the key is world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        file.write_all(bytes).map_err(io)
    }

    #[cfg(not(unix))]
    std::fs::write(path, bytes).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_id_is_the_fingerprint_of_the_generated_certificate() {
        let identity = Identity::from_seed([3u8; 32]).expect("identity");
        assert_eq!(
            identity.node_id(),
            NodeId::from_certificate_der(identity.certificate_der())
        );
    }

    #[test]
    fn the_same_seed_always_produces_the_same_node_id() {
        // Identity has to survive a restart. If the certificate carried a
        // random serial or a timestamp, the NodeId would change and every
        // message already stored would have a `from` nobody recognises.
        let first = Identity::from_seed([3u8; 32]).expect("identity");
        let second = Identity::from_seed([3u8; 32]).expect("identity");
        assert_eq!(first.node_id(), second.node_id());
    }

    #[test]
    fn different_seeds_produce_different_node_ids() {
        let first = Identity::from_seed([3u8; 32]).expect("identity");
        let second = Identity::from_seed([4u8; 32]).expect("identity");
        assert_ne!(first.node_id(), second.node_id());
    }

    #[test]
    fn a_generated_identity_signs_messages_that_verify() {
        let identity = Identity::generate().expect("identity");
        let mut message = crate::message::fixture();
        message.from = identity.node_id();
        message.sign(identity.signing_key()).expect("sign");
        assert!(
            message
                .verify(&identity.signing_key().verifying_key())
                .is_ok()
        );
    }

    #[test]
    fn an_identity_written_to_disk_is_the_same_one_when_loaded_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let created = Identity::load_or_create(dir.path()).expect("create");
        let loaded = Identity::load_or_create(dir.path()).expect("load");
        assert_eq!(created.node_id(), loaded.node_id());
    }

    #[test]
    fn the_private_key_is_not_readable_by_anyone_else() {
        let dir = tempfile::tempdir().expect("temp dir");
        Identity::load_or_create(dir.path()).expect("create");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(KEY_FILE))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "SPEC §4.3 requires 0600");
        }
    }

    #[test]
    fn a_key_file_that_is_not_a_key_is_reported_rather_than_silently_replaced() {
        // Silently generating a new identity would change this node's NodeId
        // and orphan every message already stored under the old one.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path()).expect("mkdir");
        std::fs::write(dir.path().join(KEY_FILE), b"nope").expect("write");

        assert!(matches!(
            Identity::load_or_create(dir.path()),
            Err(IdentityError::MalformedKey { .. })
        ));
    }
}
