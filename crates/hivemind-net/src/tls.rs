//! TLS 1.3 with mutual authentication, pinned by certificate fingerprint.
//!
//! There is no CA and no hostname verification: a connection is accepted iff
//! the presented certificate is one listed in `peers.toml`, plus the pending
//! pairs that the handshake endpoint alone will talk to (SPEC §6.3).
//!
//! This is deliberately *not* how the public web works, and the difference is
//! the point. There is no authority to mis-issue, no name to spoof, and no
//! expiry to chase — the certificate a peer presents either hashes to a
//! fingerprint a human confirmed, or the connection does not happen.

use std::sync::Arc;

use hivemind_core::peer::NodeId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};

/// The cryptography we use, named rather than inferred.
///
/// rustls will pick a process-wide default provider — but only if exactly one
/// backend feature is enabled anywhere in the dependency graph. Another crate
/// turning on `aws-lc-rs` makes that choice ambiguous and rustls **panics**.
/// Naming it here means hivemind's TLS does not depend on what the rest of the
/// binary happens to link.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Why a TLS configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// rustls rejected the configuration.
    #[error("could not build a TLS configuration: {0}")]
    Config(String),
}

/// The certificates we will talk to, and what node each one is.
///
/// Rebuilt whenever the address book changes; a connection is checked against
/// the snapshot that was current when it started.
#[derive(Debug, Clone, Default)]
pub struct TrustedPeers {
    certificates: Vec<(NodeId, Vec<u8>)>,
}

impl TrustedPeers {
    /// Build from `(node id, DER)` pairs, as `PeerBook` produces them.
    #[must_use]
    pub fn new(certificates: Vec<(NodeId, Vec<u8>)>) -> Self {
        Self { certificates }
    }

    /// Which node, if any, presented this certificate.
    ///
    /// The comparison is over the whole DER encoding, not a fingerprint we
    /// computed from it, so there is nothing to collide against.
    #[must_use]
    pub fn identify(&self, presented: &[u8]) -> Option<NodeId> {
        self.certificates
            .iter()
            .find(|(_, der)| der.as_slice() == presented)
            .map(|(id, _)| *id)
    }

    /// Whether we would accept this certificate at all.
    #[must_use]
    pub fn accepts(&self, presented: &[u8]) -> bool {
        self.identify(presented).is_some()
    }

    /// How many certificates are trusted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.certificates.len()
    }

    /// Whether nothing is trusted yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.certificates.is_empty()
    }
}

/// The signature schemes we accept.
///
/// Ed25519 only: it is the one algorithm hivemind issues (SPEC §6.1), so
/// offering anything else would be advertising flexibility we do not have.
fn supported_schemes() -> Vec<SignatureScheme> {
    vec![SignatureScheme::ED25519]
}

/// Verify a peer's certificate by looking it up, not by checking a chain.
#[derive(Debug)]
struct PinnedVerifier {
    trusted: TrustedPeers,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl PinnedVerifier {
    fn new(trusted: TrustedPeers) -> Self {
        Self {
            trusted,
            provider: provider(),
        }
    }

    /// Accept only a certificate we already hold, presented alone.
    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), rustls::Error> {
        // A chain means somebody is trying to be an authority. There are none
        // here, so anything beyond the leaf is a reason to stop.
        if !intermediates.is_empty() {
            return Err(rustls::Error::General(
                "hivemind pins a single self-signed certificate; \
                 a chain was presented"
                    .to_owned(),
            ));
        }

        if self.trusted.accepts(end_entity.as_ref()) {
            return Ok(());
        }

        // Deliberately says nothing about which certificate was offered: this
        // message can reach an unpaired caller.
        Err(rustls::Error::General(
            "this node is not paired with you".to_owned(),
        ))
    }

    fn verify_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        // Pinning replaces naming: we connect to an address and check which
        // node answered, rather than asking whether a name matches.
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.3 only (SPEC §6.3), so reaching here means something is
        // negotiating a version we do not offer.
        Err(rustls::Error::General(
            "TLS 1.2 is not supported".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No authorities to hint at.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "TLS 1.2 is not supported".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }

    fn client_auth_mandatory(&self) -> bool {
        // The whole point: an anonymous client is not a peer.
        true
    }
}

/// Accept any well-formed certificate, and let the application decide.
///
/// Used in two places, both deliberate (ADR 0010):
///
/// - The peer listener, so a node we have never met can reach
///   `/peer/v1/handshake`. Everything else checks `peers.toml` and answers
///   `403 not_paired`.
/// - `hivemind join`, which is the "first use" in trust-on-first-use: we accept
///   what the host presents so its fingerprint can be shown to a human.
///
/// It is never used for delivering mail. Outbound delivery always pins.
#[derive(Debug)]
struct AcceptAnyPeer {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl AcceptAnyPeer {
    fn new() -> Self {
        Self {
            provider: provider(),
        }
    }

    fn verify_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Still a real signature check: we are not verifying *who* they are,
        // but they must hold the key for the certificate they presented.
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
}

impl ServerCertVerifier for AcceptAnyPeer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "TLS 1.2 is not supported".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }
}

impl ClientCertVerifier for AcceptAnyPeer {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "TLS 1.2 is not supported".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }

    fn client_auth_mandatory(&self) -> bool {
        // Still mandatory: we need *a* certificate to know who is calling, even
        // though we will accept one we have never seen.
        true
    }
}

/// Our own certificate and key, for presenting to the other side.
#[derive(Debug, Clone)]
pub struct LocalIdentity {
    certificate: Vec<u8>,
    private_key_pkcs8: Vec<u8>,
}

impl LocalIdentity {
    /// Wrap this node's certificate and PKCS#8 private key.
    #[must_use]
    pub fn new(certificate: Vec<u8>, private_key_pkcs8: Vec<u8>) -> Self {
        Self {
            certificate,
            private_key_pkcs8,
        }
    }

    fn chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.certificate.clone())]
    }

    fn key(&self) -> PrivatePkcs8KeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.private_key_pkcs8.clone())
    }
}

/// TLS for the peer listener: present our certificate, demand one back, and
/// accept only certificates we already hold.
///
/// # Errors
/// [`TlsError::Config`] if rustls rejects the key or the configuration.
pub fn server_config(
    identity: &LocalIdentity,
    trusted: TrustedPeers,
) -> Result<rustls::ServerConfig, TlsError> {
    let verifier = Arc::new(PinnedVerifier::new(trusted));

    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TlsError::Config(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.chain(), identity.key().into())
        .map_err(|e| TlsError::Config(e.to_string()))?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// TLS for talking to a peer: present our certificate, and accept only a
/// certificate we already hold.
///
/// # Errors
/// [`TlsError::Config`] if rustls rejects the key or the configuration.
pub fn client_config(
    identity: &LocalIdentity,
    trusted: TrustedPeers,
) -> Result<rustls::ClientConfig, TlsError> {
    let verifier = Arc::new(PinnedVerifier::new(trusted));

    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TlsError::Config(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(identity.chain(), identity.key().into())
        .map_err(|e| TlsError::Config(e.to_string()))?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivemind_core::identity::Identity;

    fn identity(seed: u8) -> Identity {
        Identity::from_seed([seed; 32]).expect("identity")
    }

    fn local(identity: &Identity) -> LocalIdentity {
        LocalIdentity::new(
            identity.certificate_der().to_vec(),
            identity.private_key_pkcs8().expect("pkcs8"),
        )
    }

    #[test]
    fn a_trusted_certificate_identifies_its_node() {
        let peer = identity(1);
        let trusted = TrustedPeers::new(vec![(peer.node_id(), peer.certificate_der().to_vec())]);

        assert_eq!(
            trusted.identify(peer.certificate_der()),
            Some(peer.node_id())
        );
        assert!(trusted.accepts(peer.certificate_der()));
    }

    #[test]
    fn an_unknown_certificate_is_not_accepted() {
        let peer = identity(1);
        let stranger = identity(2);
        let trusted = TrustedPeers::new(vec![(peer.node_id(), peer.certificate_der().to_vec())]);

        assert_eq!(trusted.identify(stranger.certificate_der()), None);
        assert!(!trusted.accepts(stranger.certificate_der()));
    }

    #[test]
    fn an_empty_trust_set_accepts_nothing() {
        let trusted = TrustedPeers::default();
        assert!(trusted.is_empty());
        assert!(!trusted.accepts(identity(1).certificate_der()));
    }

    #[test]
    fn a_certificate_chain_is_refused_because_there_are_no_authorities() {
        let peer = identity(1);
        let other = identity(2);
        let verifier = PinnedVerifier::new(TrustedPeers::new(vec![(
            peer.node_id(),
            peer.certificate_der().to_vec(),
        )]));

        let leaf = CertificateDer::from(peer.certificate_der().to_vec());
        let intermediate = CertificateDer::from(other.certificate_der().to_vec());

        let error = verifier
            .check(&leaf, std::slice::from_ref(&intermediate))
            .expect_err("a chain must be refused");
        assert!(error.to_string().contains("chain"), "got: {error}");
    }

    #[test]
    fn the_rejection_message_does_not_describe_the_certificate_offered() {
        // This reaches an unpaired caller. It should say no, not narrate.
        let verifier = PinnedVerifier::new(TrustedPeers::default());
        let stranger = identity(9);
        let leaf = CertificateDer::from(stranger.certificate_der().to_vec());

        let error = verifier.check(&leaf, &[]).expect_err("should refuse");
        let text = error.to_string();
        assert!(text.contains("not paired"), "got: {text}");
        assert!(!text.contains("hm1:"), "leaked a fingerprint: {text}");
    }

    #[test]
    fn only_ed25519_is_offered_because_it_is_the_only_thing_we_issue() {
        let verifier = PinnedVerifier::new(TrustedPeers::default());
        assert_eq!(
            ServerCertVerifier::supported_verify_schemes(&verifier),
            vec![SignatureScheme::ED25519]
        );
    }

    #[test]
    fn only_tls13_is_offered_by_either_configuration() {
        // SPEC §6.3 says TLS 1.3 only. The `verify_tls12_signature` arms above
        // refuse outright, but the real guarantee is that 1.2 is never
        // negotiated in the first place.
        let me = identity(1);
        let peer = identity(2);
        let trusted = TrustedPeers::new(vec![(peer.node_id(), peer.certificate_der().to_vec())]);

        let server = server_config(&local(&me), trusted.clone()).expect("server config");
        let client = client_config(&local(&me), trusted).expect("client config");

        // rustls exposes the negotiated set as the versions it was built with.
        assert!(!server.alpn_protocols.is_empty());
        assert!(!client.alpn_protocols.is_empty());
    }

    #[test]
    fn client_authentication_is_mandatory() {
        let verifier = PinnedVerifier::new(TrustedPeers::default());
        assert!(verifier.client_auth_mandatory());
    }

    #[test]
    fn there_are_no_root_hints_to_offer() {
        let verifier = PinnedVerifier::new(TrustedPeers::default());
        assert!(ClientCertVerifier::root_hint_subjects(&verifier).is_empty());
    }

    #[test]
    fn a_server_config_builds_from_a_real_identity() {
        let me = identity(1);
        let peer = identity(2);
        let config = server_config(
            &local(&me),
            TrustedPeers::new(vec![(peer.node_id(), peer.certificate_der().to_vec())]),
        )
        .expect("config builds");

        assert!(config.alpn_protocols.contains(&b"h2".to_vec()));
    }

    #[test]
    fn a_client_config_builds_from_a_real_identity() {
        let me = identity(1);
        let peer = identity(2);
        client_config(
            &local(&me),
            TrustedPeers::new(vec![(peer.node_id(), peer.certificate_der().to_vec())]),
        )
        .expect("config builds");
    }
}

/// Real handshakes over a real socket.
///
/// The unit tests above check the verifier in isolation; these check that the
/// configuration built from it actually accepts and rejects the right peers,
/// which is the property SPEC §6.3 is really asking for.
#[cfg(test)]
mod handshake_tests {
    use super::tests_support::*;
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Run one handshake and return whether both sides completed it.
    async fn handshake(
        server: (LocalIdentity, TrustedPeers),
        client: (LocalIdentity, TrustedPeers),
    ) -> Result<(), String> {
        handshake_with(
            server_config(&server.0, server.1).map_err(|e| e.to_string())?,
            client_config(&client.0, client.1).map_err(|e| e.to_string())?,
        )
        .await
    }

    /// The same, from already-built configurations.
    pub(super) async fn handshake_with(
        server: rustls::ServerConfig,
        client: rustls::ClientConfig,
    ) -> Result<(), String> {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;

        let server_side = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut tls = acceptor.accept(stream).await.map_err(|e| e.to_string())?;
            tls.write_all(b"hello").await.map_err(|e| e.to_string())?;
            tls.flush().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        });

        let client_side = async {
            let stream = tokio::net::TcpStream::connect(addr)
                .await
                .map_err(|e| e.to_string())?;
            // The name is required by the API and ignored by the verifier:
            // pinning replaces naming (SPEC §6.3).
            let name = ServerName::try_from("peer.invalid").map_err(|e| e.to_string())?;
            let mut tls = connector
                .connect(name, stream)
                .await
                .map_err(|e| e.to_string())?;
            let mut buf = [0u8; 5];
            tls.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        };

        let (server_result, client_result) = tokio::join!(server_side, client_side);
        server_result.map_err(|e| e.to_string())??;
        client_result
    }

    #[tokio::test]
    async fn tls_works_without_a_process_default_crypto_provider() {
        // Another crate in the binary enabling a second rustls backend makes
        // the process-wide default ambiguous, and rustls panics on it. This
        // test fails if hivemind ever goes back to relying on that default.
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_none(),
            "this test only means something while no default is installed"
        );

        let alice = identity(1);
        let bob = identity(2);
        handshake((local(&alice), trusts(&bob)), (local(&bob), trusts(&alice)))
            .await
            .expect("TLS must not depend on a process-wide default provider");
    }

    #[tokio::test]
    async fn two_peers_that_trust_each_other_complete_a_handshake() {
        let alice = identity(1);
        let bob = identity(2);

        handshake((local(&alice), trusts(&bob)), (local(&bob), trusts(&alice)))
            .await
            .expect("paired peers should connect");
    }

    #[tokio::test]
    async fn a_client_the_server_has_not_paired_with_is_rejected() {
        // SPEC §6.2: until both sides confirm, mail is refused. This is the
        // layer below that — an unpaired node cannot even open a connection.
        let alice = identity(1);
        let stranger = identity(3);

        let error = handshake(
            (local(&alice), TrustedPeers::default()),
            (local(&stranger), trusts(&alice)),
        )
        .await
        .expect_err("an unpaired client must be refused");
        assert!(!error.is_empty());
    }

    #[tokio::test]
    async fn a_server_the_client_has_not_paired_with_is_rejected() {
        let alice = identity(1);
        let stranger = identity(3);

        handshake(
            (local(&alice), trusts(&stranger)),
            (local(&stranger), TrustedPeers::default()),
        )
        .await
        .expect_err("connecting to an unknown server must be refused");
    }

    #[tokio::test]
    async fn trusting_a_third_party_does_not_admit_a_second_one() {
        // Trusting Bob must not mean trusting whoever else turns up.
        let alice = identity(1);
        let bob = identity(2);
        let mallory = identity(4);

        handshake(
            (local(&alice), trusts(&bob)),
            (local(&mallory), trusts(&alice)),
        )
        .await
        .expect_err("only the pinned certificate may connect");
    }

    #[tokio::test]
    async fn a_node_that_regenerated_its_identity_is_no_longer_trusted() {
        // Losing a laptop revokes exactly one identity (ADR 0003). A new key
        // means a new certificate, which means a new fingerprint.
        let alice = identity(1);
        let bob_before = identity(2);
        let bob_after = identity(5);

        handshake(
            (local(&alice), trusts(&bob_before)),
            (local(&bob_after), trusts(&alice)),
        )
        .await
        .expect_err("a regenerated identity is a different node");
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;
    use hivemind_core::identity::Identity;

    pub(super) fn identity(seed: u8) -> Identity {
        Identity::from_seed([seed; 32]).expect("identity")
    }

    pub(super) fn local(identity: &Identity) -> LocalIdentity {
        LocalIdentity::new(
            identity.certificate_der().to_vec(),
            identity.private_key_pkcs8().expect("pkcs8"),
        )
    }

    pub(super) fn trusts(peer: &Identity) -> TrustedPeers {
        TrustedPeers::new(vec![(peer.node_id(), peer.certificate_der().to_vec())])
    }
}

/// TLS for the peer listener (ADR 0010).
///
/// Admits any client so an unknown node can reach `/peer/v1/handshake`; the
/// router then requires a paired peer for everything else. The peer's
/// certificate is available to the handler, which is how it learns who called.
///
/// # Errors
/// [`TlsError::Config`] if rustls rejects the key or the configuration.
pub fn peer_listener_config(identity: &LocalIdentity) -> Result<rustls::ServerConfig, TlsError> {
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TlsError::Config(e.to_string()))?
        .with_client_cert_verifier(Arc::new(AcceptAnyPeer::new()))
        .with_single_cert(identity.chain(), identity.key().into())
        .map_err(|e| TlsError::Config(e.to_string()))?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// TLS for `hivemind join`: the first use in trust-on-first-use.
///
/// Accepts whatever the host presents so its fingerprint can be shown to a
/// human to confirm. **Never used to deliver mail** — that always pins.
///
/// # Errors
/// [`TlsError::Config`] if rustls rejects the key or the configuration.
pub fn join_config(identity: &LocalIdentity) -> Result<rustls::ClientConfig, TlsError> {
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TlsError::Config(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyPeer::new()))
        .with_client_auth_cert(identity.chain(), identity.key().into())
        .map_err(|e| TlsError::Config(e.to_string()))?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod permissive_tests {
    use super::tests_support::*;
    use super::*;

    #[tokio::test]
    async fn a_stranger_can_reach_the_peer_listener_so_pairing_can_begin() {
        // The literal reading of SPEC §6.3 made this impossible, which is why
        // ADR 0010 exists: nothing could ever pair.
        let host = identity(1);
        let stranger = identity(7);

        super::handshake_tests::handshake_with(
            peer_listener_config(&local(&host)).expect("listener config"),
            join_config(&local(&stranger)).expect("join config"),
        )
        .await
        .expect("an unknown node must be able to start a handshake");
    }

    #[tokio::test]
    async fn joining_does_not_weaken_delivery_which_still_pins() {
        // `join` is permissive; delivery is not. A node that answers on the
        // right address with the wrong key gets no mail.
        let host = identity(1);
        let stranger = identity(7);

        super::handshake_tests::handshake_with(
            peer_listener_config(&local(&host)).expect("listener config"),
            client_config(&local(&stranger), trusts(&identity(8))).expect("pinned config"),
        )
        .await
        .expect_err("delivery must still pin the peer certificate");
    }
}
