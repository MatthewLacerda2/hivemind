//! The peer listener: mutual TLS, with the caller's identity handed to the
//! router.
//!
//! `axum::serve` cannot be used here. The whole authorisation model depends on
//! knowing *which node* is calling (ADR 0010), and that is only knowable from
//! the TLS session — so the accept loop is written out, the peer certificate is
//! pulled off the finished handshake, and the resulting [`CallerIdentity`] is
//! inserted as a request extension before the router sees it.

use std::sync::Arc;

use hivemind_core::peer::NodeId;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Who is on the other end of a peer request.
///
/// Present on every request the peer router handles: TLS demands a client
/// certificate, so there is no anonymous caller. Being *identified* is not
/// being *authorised* — the router still checks `peers.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    /// The fingerprint of the certificate they presented.
    pub node_id: NodeId,
    /// The certificate itself, which pairing stores so that later connections
    /// can be pinned against it.
    pub certificate: Vec<u8>,
}

/// Why the listener stopped.
#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    /// The socket could not be bound.
    #[error("could not bind {addr}")]
    Bind {
        /// The address we tried.
        addr: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
}

/// Serve `router` over mutual TLS until `shutdown` resolves.
///
/// # Errors
/// [`ListenerError::Bind`] if the socket cannot be bound. Once serving, a
/// failed connection is logged and dropped rather than stopping the listener:
/// one peer dialling badly must not take the daemon down.
pub async fn serve<F>(
    listener: TcpListener,
    tls: rustls::ServerConfig,
    router: axum::Router,
    shutdown: F,
) where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let (stream, remote) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::warn!(%error, "could not accept a peer connection");
                    continue;
                }
            },
            () = &mut shutdown => break,
        };

        let acceptor = acceptor.clone();
        let router = router.clone();

        tokio::spawn(async move {
            if let Err(error) = serve_connection(acceptor, stream, router).await {
                // Expected constantly: port scanners, a peer we un-paired, a
                // half-open connection. Debug, not warn.
                tracing::debug!(%remote, %error, "peer connection ended");
            }
        });
    }
}

/// Complete one TLS handshake and serve HTTP over it.
async fn serve_connection(
    acceptor: TlsAcceptor,
    stream: tokio::net::TcpStream,
    router: axum::Router,
) -> Result<(), String> {
    let tls = acceptor.accept(stream).await.map_err(|e| e.to_string())?;

    // The certificate is the caller's identity (SPEC §6.1). Client auth is
    // mandatory, so its absence means something is wrong rather than that the
    // caller is anonymous.
    let certificate = {
        let (_, session) = tls.get_ref();
        let certificates = session
            .peer_certificates()
            .ok_or_else(|| "peer presented no certificate".to_owned())?;
        certificates
            .first()
            .ok_or_else(|| "peer presented an empty certificate chain".to_owned())?
            .as_ref()
            .to_vec()
    };

    let caller = CallerIdentity {
        node_id: NodeId::from_certificate_der(&certificate),
        certificate,
    };
    let router = router.layer(axum::Extension(caller));

    let service = hyper_util::service::TowerToHyperService::new(router);
    hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_is_identified_by_its_certificate_fingerprint() {
        let identity = hivemind_core::identity::Identity::from_seed([3u8; 32]).expect("identity");
        let caller = CallerIdentity {
            node_id: NodeId::from_certificate_der(identity.certificate_der()),
            certificate: identity.certificate_der().to_vec(),
        };
        assert_eq!(caller.node_id, identity.node_id());
    }
}
