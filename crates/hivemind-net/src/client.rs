//! The daemon-to-daemon HTTP client used for delivery and blob transfer.
//!
//! Two things make this a hand-rolled hyper client rather than a `reqwest`
//! call:
//!
//! - The certificate the peer presented has to come back to the caller.
//!   `hivemind join` shows it to a human to confirm (SPEC §6.2), and it cannot
//!   be recovered after the connection is dropped.
//! - The TLS configuration is per-peer. Delivery pins one certificate; joining
//!   pins none. A pooled client keyed by hostname would reuse the wrong one.
//!
//! Only HTTP/1.1 is offered outbound. The listener speaks both, but peer
//! traffic is a handful of small requests and h2's multiplexing buys nothing
//! here — it would only cost a second connection type to get wrong.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::tls::{LocalIdentity, TlsError, TrustedPeers};

/// How long a single peer request may take before it is abandoned.
///
/// Long enough for a sleepy laptop on a slow link, short enough that the
/// delivery worker gets to retry rather than blocking on a black hole.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The name offered in SNI.
///
/// Peers are identified by certificate fingerprint, never by name (SPEC §6.1),
/// and both verifiers ignore this — but rustls requires *some* name, and a
/// constant in `.invalid` is clearer than an IP address that means nothing.
const SERVER_NAME: &str = "peer.hivemind.invalid";

/// Why a peer request failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The TLS configuration could not be built.
    #[error(transparent)]
    Tls(#[from] TlsError),

    /// The address could not be reached at all.
    #[error("could not reach {addr}")]
    Connect {
        /// The address we dialled.
        addr: String,
        /// What the socket said.
        #[source]
        source: std::io::Error,
    },

    /// The TLS handshake failed — most often because the certificate is not
    /// the one we pinned.
    #[error("could not establish a secure connection to {addr}: {reason}")]
    Handshake {
        /// The address we dialled.
        addr: String,
        /// What rustls said.
        reason: String,
    },

    /// The connection was made but the exchange failed.
    #[error("the request to {addr} failed: {reason}")]
    Http {
        /// The address we dialled.
        addr: String,
        /// What went wrong.
        reason: String,
    },

    /// The peer answered, but not with success.
    #[error("{addr} answered {status}: {detail}")]
    Status {
        /// The address we dialled.
        addr: String,
        /// The HTTP status.
        status: u16,
        /// The `detail` from the problem document, or the raw body.
        detail: String,
    },

    /// The peer answered with something we could not read.
    #[error("could not read the answer from {addr}: {reason}")]
    Decode {
        /// The address we dialled.
        addr: String,
        /// What serde said.
        reason: String,
    },

    /// The peer did not answer in time.
    #[error("{addr} did not answer within {}s", timeout.as_secs())]
    Timeout {
        /// The address we dialled.
        addr: String,
        /// How long we waited.
        timeout: Duration,
    },
}

impl ClientError {
    /// Whether retrying later could plausibly succeed (SPEC §8).
    ///
    /// A closed laptop is temporary; a rejected signature is not. The delivery
    /// worker retries the first forever and gives up on the second.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connect { .. } | Self::Timeout { .. } | Self::Http { .. } => true,
            // A 5xx is the peer's problem and may pass; a 4xx is ours and will
            // not. 403 not_paired is the interesting one: it stays a 403 until
            // a human acts, and "forever" is the right retry interval for that.
            Self::Status { status, .. } => *status >= 500,
            Self::Tls(_) | Self::Handshake { .. } | Self::Decode { .. } => false,
        }
    }
}

/// What a peer answered, and who it turned out to be.
#[derive(Debug, Clone)]
pub struct PeerResponse<R> {
    /// The decoded body.
    pub body: R,
    /// The certificate the peer presented. `hivemind join` shows its
    /// fingerprint to a human; pairing stores it so later calls can pin it.
    pub certificate: Vec<u8>,
}

/// One file travelling with a message (SPEC §7.2).
#[derive(Debug, Clone)]
pub struct Part {
    /// The form field name. For a delivery this is the blob's digest, so the
    /// recipient can match a part to the attachment that declared it.
    pub name: String,
    /// What the sender says it is. Advisory (SPEC §4.1).
    pub content_type: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// An HTTP client for one peer's trust settings.
#[derive(Debug, Clone)]
pub struct PeerClient {
    config: Arc<rustls::ClientConfig>,
    timeout: Duration,
}

impl PeerClient {
    /// A client that will talk to nothing but the certificates in `trusted`.
    ///
    /// This is what delivery uses. An impostor on the right IP address fails
    /// the handshake rather than receiving mail.
    ///
    /// # Errors
    /// [`ClientError::Tls`] if rustls rejects the configuration.
    pub fn pinned(identity: &LocalIdentity, trusted: TrustedPeers) -> Result<Self, ClientError> {
        Ok(Self::from_config(crate::tls::client_config(
            identity, trusted,
        )?))
    }

    /// A client for `hivemind join`: the first use in trust-on-first-use.
    ///
    /// Accepts whatever the host presents so its fingerprint can be shown to a
    /// human. **Never used to deliver mail.**
    ///
    /// # Errors
    /// [`ClientError::Tls`] if rustls rejects the configuration.
    pub fn joining(identity: &LocalIdentity) -> Result<Self, ClientError> {
        Ok(Self::from_config(crate::tls::join_config(identity)?))
    }

    fn from_config(mut config: rustls::ClientConfig) -> Self {
        // Offer only HTTP/1.1. The listener advertises h2 as well and would
        // pick it if we offered it, and then this client would be speaking a
        // protocol it cannot frame.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self {
            config: Arc::new(config),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Use a different timeout than [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// POST `body` as JSON plus `parts` as files, as one multipart request.
    ///
    /// The body is assembled in memory. That is deliberate rather than lazy:
    /// what travels this way is bounded by the sender's inline budget (SPEC
    /// §8), and anything larger is fetched separately with a range request
    /// that can resume — which streaming the request would not give us.
    ///
    /// # Errors
    /// Any [`ClientError`].
    pub async fn post_multipart<B, R>(
        &self,
        authority: &str,
        path: &str,
        body: &B,
        parts: &[Part],
    ) -> Result<PeerResponse<R>, ClientError>
    where
        B: Serialize + Sync,
        R: DeserializeOwned,
    {
        let json = encode(authority, body)?;

        let boundary = multipart_boundary();
        let mut request = Vec::new();
        write_part(
            &mut request,
            &boundary,
            "message",
            None,
            "application/json",
            &json,
        );
        for part in parts {
            write_part(
                &mut request,
                &boundary,
                &part.name,
                Some(&part.name),
                &part.content_type,
                &part.bytes,
            );
        }
        request.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={boundary}");
        self.timed(
            authority,
            self.exchange(authority, path, |_| Ok(request), &content_type),
        )
        .await
        .and_then(PeerResponse::transpose)
    }

    /// POST `body` as JSON to `path` at `authority` (`host:port`).
    ///
    /// # Errors
    /// Any [`ClientError`]; see [`ClientError::is_transient`] for whether the
    /// caller should try again.
    pub async fn post<B, R>(
        &self,
        authority: &str,
        path: &str,
        body: &B,
    ) -> Result<PeerResponse<R>, ClientError>
    where
        B: Serialize + Sync,
        R: DeserializeOwned,
    {
        let request = encode(authority, body)?;
        self.timed(
            authority,
            self.exchange(authority, path, |_| Ok(request), "application/json"),
        )
        .await
        .and_then(PeerResponse::transpose)
    }

    /// POST JSON built from the certificate the peer presented.
    ///
    /// For the handshake: its group proof covers the receiver's certificate
    /// (SPEC §6.2), which does not exist until TLS has finished, so the body
    /// cannot be written first.
    ///
    /// The outer `Result` is whether the peer could be reached at all. The
    /// inner one is what it answered — and a refusal still comes back with the
    /// certificate, because a node that says no has still said who it is.
    ///
    /// # Errors
    /// [`ClientError::Connect`], [`ClientError::Handshake`] or
    /// [`ClientError::Timeout`] if nobody answered as a TLS peer.
    pub async fn post_bound<B, R>(
        &self,
        authority: &str,
        path: &str,
        body: impl FnOnce(&[u8]) -> B + Send,
    ) -> Result<PeerResponse<Result<R, ClientError>>, ClientError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        self.timed(
            authority,
            self.exchange(
                authority,
                path,
                |certificate| encode(authority, &body(certificate)),
                "application/json",
            ),
        )
        .await
    }

    /// Bound `request` by this client's timeout.
    async fn timed<T>(
        &self,
        authority: &str,
        request: impl std::future::Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        tokio::time::timeout(self.timeout, request)
            .await
            .unwrap_or_else(|_| {
                Err(ClientError::Timeout {
                    addr: authority.to_owned(),
                    timeout: self.timeout,
                })
            })
    }

    /// GET `path`, resuming from byte `from`, handing each chunk to `sink`.
    ///
    /// Returns how many bytes arrived. Streamed rather than buffered: this is
    /// the path a file too large to travel with its message takes, and holding
    /// a two-gigabyte attachment in memory to write it to disk would be a
    /// strange way to save a round trip.
    ///
    /// # Errors
    /// Any [`ClientError`]. A partial download is not an error — the caller
    /// keeps what arrived and resumes from it.
    pub async fn download<F>(
        &self,
        authority: &str,
        path: &str,
        from: u64,
        sink: F,
    ) -> Result<u64, ClientError>
    where
        F: FnMut(&[u8]) -> Result<(), ClientError> + Send,
    {
        match tokio::time::timeout(self.timeout, self.stream(authority, path, from, sink)).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Timeout {
                addr: authority.to_owned(),
                timeout: self.timeout,
            }),
        }
    }

    async fn stream<F>(
        &self,
        authority: &str,
        path: &str,
        from: u64,
        mut sink: F,
    ) -> Result<u64, ClientError>
    where
        F: FnMut(&[u8]) -> Result<(), ClientError> + Send,
    {
        let (mut sender, certificate, pump) = self.connect(authority).await?;
        let _ = certificate;

        let result = async {
            let mut builder = hyper::Request::builder()
                .method(hyper::Method::GET)
                .uri(path)
                .header(hyper::header::HOST, authority);
            if from > 0 {
                builder = builder.header(hyper::header::RANGE, format!("bytes={from}-"));
            }

            let request = builder
                // An empty body rather than a separate type: the connection is
                // typed by its body, and one type keeps `connect` shared.
                .body(http_body_util::Full::new(hyper::body::Bytes::new()))
                .map_err(|e| ClientError::Http {
                    addr: authority.to_owned(),
                    reason: e.to_string(),
                })?;

            let mut response =
                sender
                    .send_request(request)
                    .await
                    .map_err(|e| ClientError::Http {
                        addr: authority.to_owned(),
                        reason: e.to_string(),
                    })?;

            let status = response.status();
            if !status.is_success() {
                use http_body_util::BodyExt as _;
                let bytes = response
                    .body_mut()
                    .collect()
                    .await
                    .map(http_body_util::Collected::to_bytes)
                    .unwrap_or_default();
                return Err(ClientError::Status {
                    addr: authority.to_owned(),
                    status: status.as_u16(),
                    detail: problem_detail(&bytes),
                });
            }

            // A server that ignored the range header answers 200 and starts
            // from zero. Appending that to what we already hold would corrupt
            // the file, so it is a failure rather than something to salvage.
            if from > 0 && status != hyper::StatusCode::PARTIAL_CONTENT {
                return Err(ClientError::Http {
                    addr: authority.to_owned(),
                    reason: format!("asked to resume from {from} but got {status}"),
                });
            }

            let mut received = 0u64;
            loop {
                use http_body_util::BodyExt as _;
                let Some(frame) = response.frame().await else {
                    break;
                };
                let frame = frame.map_err(|e| ClientError::Http {
                    addr: authority.to_owned(),
                    reason: e.to_string(),
                })?;
                if let Some(chunk) = frame.data_ref() {
                    received += chunk.len() as u64;
                    sink(chunk)?;
                }
            }
            Ok(received)
        }
        .await;

        drop(sender);
        pump.abort();
        result
    }

    /// Connect, complete the TLS handshake, and start driving the connection.
    ///
    /// Returns the request sender, the certificate the peer presented — only
    /// available while the session is alive, which is the whole reason this
    /// client exists rather than a pooled one — and the task pumping the
    /// socket, which the caller aborts when it is done.
    #[allow(clippy::type_complexity)]
    async fn connect(
        &self,
        authority: &str,
    ) -> Result<
        (
            hyper::client::conn::http1::SendRequest<http_body_util::Full<hyper::body::Bytes>>,
            Vec<u8>,
            tokio::task::JoinHandle<()>,
        ),
        ClientError,
    > {
        let tcp = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|source| ClientError::Connect {
                addr: authority.to_owned(),
                source,
            })?;
        // Each request is small and immediately awaited; waiting 40 ms for a
        // segment that will never be joined by another only adds latency.
        let _ = tcp.set_nodelay(true);

        let name = rustls_pki_types::ServerName::try_from(SERVER_NAME)
            .map_err(|e| ClientError::Handshake {
                addr: authority.to_owned(),
                reason: e.to_string(),
            })?
            .to_owned();

        let tls = tokio_rustls::TlsConnector::from(Arc::clone(&self.config))
            .connect(name, tcp)
            .await
            .map_err(|source| ClientError::Handshake {
                addr: authority.to_owned(),
                reason: source.to_string(),
            })?;

        // Only available while the session is alive, which is the whole reason
        // this client exists rather than a pooled one.
        let certificate = {
            let (_, session) = tls.get_ref();
            session
                .peer_certificates()
                .and_then(<[_]>::first)
                .ok_or_else(|| ClientError::Handshake {
                    addr: authority.to_owned(),
                    reason: "the peer presented no certificate".to_owned(),
                })?
                .as_ref()
                .to_vec()
        };

        let (sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
                .await
                .map_err(|e| ClientError::Http {
                    addr: authority.to_owned(),
                    reason: e.to_string(),
                })?;

        let pump = tokio::spawn(async move {
            // The connection task drives the socket. It ends when the sender
            // is dropped, so there is nothing for the caller to clean up.
            let _ = connection.await;
        });

        Ok((sender, certificate, pump))
    }

    /// One request, from TCP connect to decoded body.
    ///
    /// `request` builds the body once the peer's certificate is known. The
    /// outer error is "could not reach a TLS peer"; everything after the
    /// handshake is inside the response, beside the certificate.
    async fn exchange<R: DeserializeOwned>(
        &self,
        authority: &str,
        path: &str,
        request: impl FnOnce(&[u8]) -> Result<Vec<u8>, ClientError>,
        content_type: &str,
    ) -> Result<PeerResponse<Result<R, ClientError>>, ClientError> {
        let (mut sender, certificate, pump) = self.connect(authority).await?;

        let response = async {
            let request = request(&certificate)?;
            let http = hyper::Request::builder()
                .method(hyper::Method::POST)
                .uri(path)
                .header(hyper::header::HOST, authority)
                .header(hyper::header::CONTENT_TYPE, content_type)
                .body(http_body_util::Full::new(hyper::body::Bytes::from(request)))
                .map_err(|e| ClientError::Http {
                    addr: authority.to_owned(),
                    reason: e.to_string(),
                })?;

            let response = sender
                .send_request(http)
                .await
                .map_err(|e| ClientError::Http {
                    addr: authority.to_owned(),
                    reason: e.to_string(),
                })?;

            let status = response.status();
            let bytes = {
                use http_body_util::BodyExt as _;
                response
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| ClientError::Http {
                        addr: authority.to_owned(),
                        reason: e.to_string(),
                    })?
                    .to_bytes()
            };

            if !status.is_success() {
                return Err(ClientError::Status {
                    addr: authority.to_owned(),
                    status: status.as_u16(),
                    detail: problem_detail(&bytes),
                });
            }

            serde_json::from_slice(&bytes).map_err(|e| ClientError::Decode {
                addr: authority.to_owned(),
                reason: e.to_string(),
            })
        }
        .await;

        drop(sender);
        pump.abort();

        Ok(PeerResponse {
            body: response,
            certificate,
        })
    }
}

impl<R> PeerResponse<Result<R, ClientError>> {
    /// Fail the whole call if the peer's answer was a failure.
    ///
    /// # Errors
    /// The answer's error, when it was one.
    pub fn transpose(self) -> Result<PeerResponse<R>, ClientError> {
        Ok(PeerResponse {
            body: self.body?,
            certificate: self.certificate,
        })
    }
}

/// Serialise a request body, naming the peer it was for if that fails.
fn encode<B: Serialize + ?Sized>(authority: &str, body: &B) -> Result<Vec<u8>, ClientError> {
    serde_json::to_vec(body).map_err(|e| ClientError::Http {
        addr: authority.to_owned(),
        reason: format!("could not encode the request: {e}"),
    })
}

/// Pull `detail` out of an RFC 9457 problem document, falling back to the body.
///
/// A peer that answers with something else is still telling us *something*,
/// and an operator reading a log wants whatever that was.
fn problem_detail(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            json.get("detail")
                .or_else(|| json.get("title"))
                .and_then(|d| d.as_str().map(ToOwned::to_owned))
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
}

/// A boundary no body will contain.
///
/// 128 random bits, hex-encoded. A collision would corrupt one request; the
/// alternative — scanning every part for every candidate — costs a pass over
/// the whole body to avoid something that will not happen.
fn multipart_boundary() -> String {
    let mut bytes = [0u8; 16];
    // A failure here is not worth failing a delivery over, and the fallback is
    // still a string no reasonable body contains.
    if getrandom::fill(&mut bytes).is_err() {
        return "hivemind-boundary-fallback-0000".to_owned();
    }
    let mut out = String::with_capacity(32 + 9);
    out.push_str("hivemind-");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Append one `multipart/form-data` part.
fn write_part(
    out: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    filename: Option<&str>,
    content_type: &str,
    bytes: &[u8],
) {
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    // The name is a digest or the literal "message" — never anything a sender
    // chose — so there is nothing here to escape.
    match filename {
        Some(filename) => out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n")
                .as_bytes(),
        ),
        None => out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
        ),
    }
    out.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivemind_core::identity::Identity;
    use hivemind_core::peer::NodeId;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Echo {
        said: String,
    }

    fn identity(seed: u8) -> Identity {
        Identity::from_seed([seed; 32]).expect("identity")
    }

    fn local(id: &Identity) -> LocalIdentity {
        LocalIdentity::new(
            id.certificate_der().to_vec(),
            id.private_key_pkcs8().expect("key"),
        )
    }

    fn trusting(id: &Identity) -> TrustedPeers {
        TrustedPeers::new(vec![(id.node_id(), id.certificate_der().to_vec())])
    }

    /// A peer listener that echoes JSON back, plus the address it is on.
    ///
    /// Returns a guard whose drop stops the server, so a failing test cannot
    /// leave a task running.
    struct TestPeer {
        addr: String,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for TestPeer {
        fn drop(&mut self) {
            drop(self.stop.take());
        }
    }

    async fn peer_serving(tls: rustls::ServerConfig, router: axum::Router) -> TestPeer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let (stop, stopped) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            crate::listener::serve(listener, tls, router, async {
                let _ = stopped.await;
            })
            .await;
        });

        TestPeer {
            addr,
            stop: Some(stop),
        }
    }

    fn echo_router() -> axum::Router {
        use axum::routing::post;
        axum::Router::new().route(
            "/peer/v1/echo",
            post(|axum::Json(echo): axum::Json<Echo>| async move { axum::Json(echo) }),
        )
    }

    #[tokio::test]
    async fn a_pinned_client_reaches_the_peer_it_pinned() {
        let host = identity(1);
        let caller = identity(2);

        let peer = peer_serving(
            crate::tls::server_config(&local(&host), trusting(&caller)).expect("server config"),
            echo_router(),
        )
        .await;

        let client = PeerClient::pinned(&local(&caller), trusting(&host)).expect("client");
        let answer: PeerResponse<Echo> = client
            .post(
                &peer.addr,
                "/peer/v1/echo",
                &Echo {
                    said: "hello".to_owned(),
                },
            )
            .await
            .expect("the request should succeed");

        assert_eq!(answer.body.said, "hello");
    }

    #[tokio::test]
    async fn the_certificate_the_peer_presented_comes_back_so_a_human_can_confirm_it() {
        // SPEC §6.2: `hivemind join` shows the fingerprint. It is only
        // available while the connection is open, so the client must hand it
        // back rather than making the caller reconnect to ask.
        let host = identity(3);
        let caller = identity(4);

        let peer = peer_serving(
            crate::tls::peer_listener_config(&local(&host)).expect("listener config"),
            echo_router(),
        )
        .await;

        let client = PeerClient::joining(&local(&caller)).expect("client");
        let answer: PeerResponse<Echo> = client
            .post(
                &peer.addr,
                "/peer/v1/echo",
                &Echo {
                    said: "who are you".to_owned(),
                },
            )
            .await
            .expect("joining should succeed");

        assert_eq!(
            NodeId::from_certificate_der(&answer.certificate),
            host.node_id(),
            "the certificate returned must be the host's own"
        );
    }

    #[tokio::test]
    async fn a_bound_body_is_built_knowing_who_answered() {
        // The group proof covers the receiver's certificate (SPEC §6.2), which
        // does not exist until TLS has finished. So the body has to be written
        // after the handshake, with the certificate in hand.
        let host = identity(8);
        let caller = identity(9);

        let peer = peer_serving(
            crate::tls::peer_listener_config(&local(&host)).expect("listener config"),
            echo_router(),
        )
        .await;

        let client = PeerClient::joining(&local(&caller)).expect("client");
        let answer: PeerResponse<Result<Echo, ClientError>> = client
            .post_bound(&peer.addr, "/peer/v1/echo", |certificate| Echo {
                said: NodeId::from_certificate_der(certificate).to_string(),
            })
            .await
            .expect("the connection should succeed");

        assert_eq!(
            answer.body.expect("the host answers").said,
            host.node_id().to_string(),
            "the body must have been built from the host's own certificate"
        );
    }

    #[tokio::test]
    async fn a_refusal_still_says_who_refused() {
        // A node that is not in our group answers 403. That is still news that
        // it exists, and who it is, which `hivemind peers` shows as "seen".
        let host = identity(10);
        let caller = identity(11);

        let refusing = axum::Router::new().route(
            "/peer/v1/echo",
            axum::routing::post(|| async { axum::http::StatusCode::FORBIDDEN }),
        );
        let peer = peer_serving(
            crate::tls::peer_listener_config(&local(&host)).expect("listener config"),
            refusing,
        )
        .await;

        let client = PeerClient::joining(&local(&caller)).expect("client");
        let answer: PeerResponse<Result<Echo, ClientError>> = client
            .post_bound(&peer.addr, "/peer/v1/echo", |_| Echo {
                said: "let me in".to_owned(),
            })
            .await
            .expect("the connection itself should succeed");

        assert!(
            matches!(answer.body, Err(ClientError::Status { status: 403, .. })),
            "the refusal is the answer: {:?}",
            answer.body
        );
        assert_eq!(
            NodeId::from_certificate_der(&answer.certificate),
            host.node_id()
        );
    }

    #[tokio::test]
    async fn a_client_refuses_a_host_whose_certificate_it_did_not_pin() {
        // The impostor is on the address we expected and completes TCP. Only
        // the pin stops it.
        let impostor = identity(5);
        let expected = identity(6);
        let caller = identity(7);

        let peer = peer_serving(
            crate::tls::peer_listener_config(&local(&impostor)).expect("listener config"),
            echo_router(),
        )
        .await;

        let client = PeerClient::pinned(&local(&caller), trusting(&expected)).expect("client");
        let error = client
            .post::<_, Echo>(
                &peer.addr,
                "/peer/v1/echo",
                &Echo {
                    said: "hello".to_owned(),
                },
            )
            .await
            .expect_err("an unpinned host must be refused");

        assert!(
            matches!(error, ClientError::Handshake { .. }),
            "expected a handshake failure, got {error:?}"
        );
        assert!(
            !error.is_transient(),
            "a wrong certificate will not become right by waiting"
        );
    }

    #[tokio::test]
    async fn a_refusal_is_reported_with_its_status_and_detail() {
        let host = identity(8);
        let caller = identity(9);

        let router = axum::Router::new().route(
            "/peer/v1/echo",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({
                        "type": "/problems/not_paired",
                        "title": "not paired",
                        "status": 403,
                        "detail": "this node has not paired with you",
                    })),
                )
            }),
        );

        let peer = peer_serving(
            crate::tls::server_config(&local(&host), trusting(&caller)).expect("server config"),
            router,
        )
        .await;

        let client = PeerClient::pinned(&local(&caller), trusting(&host)).expect("client");
        let error = client
            .post::<_, Echo>(
                &peer.addr,
                "/peer/v1/echo",
                &Echo {
                    said: "let me in".to_owned(),
                },
            )
            .await
            .expect_err("403 is an error");

        match error {
            ClientError::Status { status, detail, .. } => {
                assert_eq!(status, 403);
                assert!(
                    detail.contains("has not paired"),
                    "the problem detail should be surfaced, got {detail:?}"
                );
            }
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_address_nothing_is_listening_on_is_a_transient_failure() {
        // SPEC §8: a peer that is simply off is the normal case, and the
        // delivery worker must keep trying.
        let caller = identity(10);
        let absent = identity(11);

        // Bound and immediately dropped, so the port is almost certainly free.
        let addr = {
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            socket.local_addr().expect("addr").to_string()
        };

        let client = PeerClient::pinned(&local(&caller), trusting(&absent)).expect("client");
        let error = client
            .post::<_, Echo>(
                &addr,
                "/peer/v1/echo",
                &Echo {
                    said: "anyone home".to_owned(),
                },
            )
            .await
            .expect_err("nothing is listening");

        assert!(
            matches!(error, ClientError::Connect { .. }),
            "expected a connect failure, got {error:?}"
        );
        assert!(error.is_transient(), "a peer that is off comes back");
    }

    #[test]
    fn a_rejected_signature_is_not_retried_but_a_server_error_is() {
        let status = |status| ClientError::Status {
            addr: "10.0.0.1:8400".to_owned(),
            status,
            detail: String::new(),
        };
        assert!(!status(400).is_transient(), "a bad request stays bad");
        assert!(
            !status(403).is_transient(),
            "pairing needs a human, not time"
        );
        assert!(
            status(503).is_transient(),
            "a peer that is busy will not be"
        );
    }
}
