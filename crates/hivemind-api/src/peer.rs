//! The peer router (SPEC §7.2): handshake and delivery.
//!
//! Authorisation lives here rather than in the TLS layer (ADR 0010).
//! `/peer/v1/handshake` is open, because a node we have never met has to be
//! able to prove it is in our group. Everything else requires a peer in
//! `peers.toml` — one that has proved it — and answers `403 not_paired`
//! otherwise (SPEC §6.2).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Json, Router};
use hivemind_core::message::Message;
use hivemind_core::peerbook::{AddrSource, PeerAddr};
use hivemind_net::listener::CallerIdentity;
use serde::{Deserialize, Serialize};

use crate::problem::{Problem, ProblemType};
use crate::service::MailService;

pub mod hello;

pub use hello::{Hello, PeerNote, SessionNote};

/// What each side sends in a handshake (SPEC §7.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    /// The sender's node id, as a string. Advisory: the authoritative identity
    /// is the certificate, and a mismatch is rejected.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// The owner it claims. Unverified, and never a security boundary
    /// (ADR 0003).
    pub owner: Option<String>,
    /// The version it is running, so a future change can be negotiated.
    pub version: String,
    /// The host the sender can be reached on, so the receiver can call back
    /// without guessing which of its interfaces the connection arrived through.
    pub callback_host: String,
    /// The port the sender's peer listener is on.
    pub callback_port: u16,
    /// Proof that the sender holds the group key (SPEC §6.2). Absent when it
    /// is in no group, which the receiver refuses exactly as it refuses a
    /// wrong one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
    /// The sender's peer list (SPEC §5.4).
    ///
    /// Carried here as well as on the hello so that a node admitted this
    /// second has the group now rather than at the next presence round. The
    /// hello is what keeps it current; this is what makes joining immediate.
    #[serde(default)]
    pub gossip: Vec<hello::PeerNote>,
}

/// Proof of the group key, bound to both certificates (`docs/protocol.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// When it was made, in milliseconds since the Unix epoch.
    pub sent_at: i64,
    /// HMAC-SHA256, in lowercase hex.
    pub mac: String,
}

/// What a delivery returns, so the sender can mark that recipient done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivered {
    /// The id that was received. Echoed so the sender can match it up.
    pub id: String,
}

/// Build the peer router.
pub fn router(state: Arc<MailService>) -> Router {
    let limit = state.max_delivery_bytes();
    Router::new()
        .route("/peer/v1/handshake", post(handshake))
        // SPEC §5.5. Members only, and the proof is checked afresh every
        // time: that is what makes a rotated key retire whoever kept the old.
        .route("/peer/v1/hello", post(hello::hello))
        .route(
            "/peer/v1/messages",
            post(receive_message)
                // axum defaults to 2 MiB, which is smaller than one inline
                // attachment. This is the only route that needs raising, and
                // it is raised to exactly what this node is willing to hold.
                .layer(axum::extract::DefaultBodyLimit::max(limit)),
        )
        // SPEC §8: a read receipt is mail carrying a fact. Members only, like
        // delivery — and what it says is only ever about the caller's own
        // reading, because the caller is the connection rather than the body.
        .route("/peer/v1/receipts", post(receive_receipts))
        .route(
            "/peer/v1/blobs/{sha}",
            axum::routing::get(serve_blob).head(have_blob),
        )
        .with_state(state)
}

/// Admit a node that proves the group key, and prove it back.
///
/// Open to anyone who can complete a TLS handshake (ADR 0010). A node that
/// proves the key is pinned; one that does not is remembered as seen and
/// refused. It never reads or reveals anything about the mailbox.
async fn handshake(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    Json(request): Json<Handshake>,
) -> Result<Json<Handshake>, Problem> {
    let span = tracing::info_span!("handshake", peer = %caller.node_id.short());
    let _entered = span.enter();

    // The body says who they are; the certificate proves it. When the two
    // disagree, believe neither — a node that misdescribes itself in the one
    // field we can check is not one to record.
    if request.id != caller.node_id.to_string() {
        return Err(Problem::new(
            ProblemType::IdentityMismatch,
            "the node id in the handshake does not match the certificate presented",
        ));
    }

    let addr = callback_address(&caller, &request.callback_host, request.callback_port);

    Ok(Json(service.answer_handshake(
        caller.node_id,
        &request,
        &caller.certificate,
        addr,
    )?))
}

/// Accept one signed message from a paired peer.
///
/// Idempotent on message id: redelivery is always safe (SPEC §8).
async fn receive_message(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<Delivered>), Problem> {
    let (message, blobs) = read_delivery(multipart).await?;

    // The inbound half of the same span (SPEC §13.1). Entered after the body
    // is read because the message id is not knowable before it.
    let span = tracing::info_span!(
        "receive",
        peer = %caller.node_id.short(),
        message = %message.id
    );
    let _entered = span.enter();
    // SPEC §6.2: only a node that has proved the group key may send mail.
    if !service.is_paired(caller.node_id)? {
        return Err(Problem::new(
            ProblemType::NotPaired,
            "this node has not admitted you; are both machines in the same group?",
        ));
    }

    // A peer may only speak for itself. Accepting a message whose `from` is
    // somebody else would let any paired node forge mail from any other.
    if message.from != caller.node_id {
        return Err(Problem::new(
            ProblemType::IdentityMismatch,
            "the message is signed by a different node than the one delivering it",
        ));
    }

    // Blobs before the message: a message whose inline attachment is missing
    // would show an attachment nothing can open, and redelivery is safe
    // (SPEC §8) so failing here costs a retry rather than the mail.
    service.accept_inline_blobs(&message, blobs)?;

    let id = service.receive(caller.node_id, message)?;
    Ok((StatusCode::ACCEPTED, Json(Delivered { id: id.to_string() })))
}

/// Record that a paired peer has read messages this node sent them.
///
/// Idempotent, as delivery is: a receipt for something already recorded is a
/// success and changes nothing. A receipt naming a message this node does not
/// hold, or one that was never addressed to the caller, is counted out rather
/// than refused — the sender may have deleted it, and a fact with nowhere to
/// go is not a failure to report.
async fn receive_receipts(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    Json(request): Json<hivemind_core::receipts::ReadReceipts>,
) -> Result<(StatusCode, Json<hivemind_core::receipts::Recorded>), Problem> {
    let span = tracing::info_span!("receipts", peer = %caller.node_id.short());
    let _entered = span.enter();

    if !service.is_paired(caller.node_id)? {
        return Err(Problem::new(
            ProblemType::NotPaired,
            "this node has not admitted you; are both machines in the same group?",
        ));
    }

    let recorded = service.record_read_receipts(caller.node_id, &request.read)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(hivemind_core::receipts::Recorded { recorded }),
    ))
}

/// Pull the message and its inline blobs out of a delivery.
///
/// The `message` part must come first. That is how the sender writes it, and
/// requiring it means a blob can be checked against a declared attachment as
/// it is read rather than buffering everything first.
async fn read_delivery(
    mut multipart: axum::extract::Multipart,
) -> Result<(Message, Vec<(String, Vec<u8>)>), Problem> {
    let mut message: Option<Message> = None;
    let mut blobs = Vec::new();

    loop {
        let field = multipart.next_field().await.map_err(|e| {
            Problem::new(
                ProblemType::InvalidMessage,
                format!("the delivery could not be read: {e}"),
            )
        })?;
        let Some(field) = field else { break };

        let name = field.name().unwrap_or_default().to_owned();
        let bytes = field.bytes().await.map_err(|e| {
            Problem::new(
                ProblemType::InvalidMessage,
                format!("a part of the delivery could not be read: {e}"),
            )
        })?;

        if name == "message" {
            message = Some(serde_json::from_slice(&bytes).map_err(|e| {
                Problem::new(
                    ProblemType::InvalidMessage,
                    format!("unreadable message: {e}"),
                )
            })?);
        } else {
            blobs.push((name, bytes.to_vec()));
        }
    }

    let message = message.ok_or_else(|| {
        Problem::new(
            ProblemType::InvalidMessage,
            "a delivery must carry a `message` part",
        )
    })?;
    Ok((message, blobs))
}

/// Where to call this peer back, believing the socket over the claim.
///
/// The connection's source address is a fact; `callback_host` is an opinion,
/// and every daemon used to claim `127.0.0.1` — so every peer learned its own
/// loopback as the way to reach the other one, and then delivered to itself
/// (#23).
///
/// The **port** still has to be claimed: the source port of an outbound
/// connection is ephemeral and says nothing about where that node listens.
///
/// A peer genuinely on this machine keeps its loopback address, because there
/// it is true — and that is the case every integration test exercises, which
/// is why the bug survived to be found on two real machines.
///
/// The other half of the same judgement is
/// [`PeerAddr::points_at_this_node`](hivemind_core::peerbook::PeerAddr::points_at_this_node),
/// which is what a `peers.toml` written before this fix is read through (#29).
fn callback_address(caller: &CallerIdentity, claimed_host: &str, claimed_port: u16) -> PeerAddr {
    let observed = caller.remote.ip();

    // A claimed *address* is always replaced by the observed one: the socket
    // knows and the claim only guesses. A claimed *name* is kept, because a
    // hostname outlives the address behind it — a MagicDNS name still resolves
    // after the peer moves — and because this node cannot check it anyway.
    let host = if claimed_host.parse::<std::net::IpAddr>().is_ok() {
        observed.to_string()
    } else {
        claimed_host.to_owned()
    };

    PeerAddr {
        host,
        port: claimed_port,
        source: AddrSource::Manual,
        last_ok: None,
    }
}

/// Where a range request wants to start.
///
/// Only `bytes=N-` is honoured, which is the one form resuming a download
/// needs (SPEC §7.2). Anything else — multiple ranges, a suffix range, a
/// closed range — is answered with the whole blob, which is a correct if
/// unhelpful response and much less code to get wrong.
fn resume_offset(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get(axum::http::header::RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes=")?
        .strip_suffix('-')?
        .parse()
        .ok()
}

/// Do we still have this blob?
///
/// A recipient asks before resuming, so that a sender who has since deleted
/// the file gives a clear answer rather than a stalled download.
async fn have_blob(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    axum::extract::Path(sha): axum::extract::Path<String>,
) -> Result<axum::response::Response, Problem> {
    let digest = paired_digest(&service, caller.node_id, &sha)?;
    let size = service
        .blobs()
        .size_of(&digest)
        .ok_or_else(|| Problem::new(ProblemType::BlobNotFound, "no such attachment here"))?;

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_LENGTH, size)
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .body(axum::body::Body::empty())
        .map_err(|_| Problem::new(ProblemType::Internal, "could not build a response"))
}

/// Stream a blob, honouring a resume offset.
async fn serve_blob(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    axum::extract::Path(sha): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, Problem> {
    let digest = paired_digest(&service, caller.node_id, &sha)?;
    let total = service
        .blobs()
        .size_of(&digest)
        .ok_or_else(|| Problem::new(ProblemType::BlobNotFound, "no such attachment here"))?;

    let path = service.blobs().path_of(&digest);
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| Problem::new(ProblemType::BlobNotFound, "no such attachment here"))?;

    let from = resume_offset(&headers).unwrap_or(0);
    // A resume point past the end means the two sides disagree about the file.
    // 416 tells the caller to start over rather than leaving it waiting.
    if from > total {
        return axum::response::Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(
                axum::http::header::CONTENT_RANGE,
                format!("bytes */{total}"),
            )
            .body(axum::body::Body::empty())
            .map_err(|_| Problem::new(ProblemType::Internal, "could not build a response"));
    }

    if from > 0 {
        tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(from))
            .await
            .map_err(|_| Problem::new(ProblemType::Internal, "could not seek the attachment"))?;
    }

    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let status = if from > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut response = axum::response::Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_LENGTH, total - from)
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream");
    if from > 0 {
        response = response.header(
            axum::http::header::CONTENT_RANGE,
            // An empty range has no last byte; `total - 1` would underflow.
            format!("bytes {from}-{}/{total}", total.saturating_sub(1)),
        );
    }

    response
        .body(body)
        .map_err(|_| Problem::new(ProblemType::Internal, "could not build a response"))
}

/// Check the caller may ask about blobs at all, and parse what it asked for.
///
/// Blobs are only ever fetched for a message the peer already received, so
/// this needs the same pairing check as delivery. The digest is parsed rather
/// than trusted: it arrives in a URL path from another machine.
fn paired_digest(
    service: &Arc<MailService>,
    caller: hivemind_core::peer::NodeId,
    sha: &str,
) -> Result<hivemind_core::crypto::Sha256Digest, Problem> {
    if !service.is_paired(caller)? {
        return Err(Problem::new(
            ProblemType::NotPaired,
            "this node has not admitted you; are both machines in the same group?",
        ));
    }
    sha.parse().map_err(|_| {
        Problem::new(
            ProblemType::BlobNotFound,
            "that is not a SHA-256 digest, so there is no such attachment",
        )
    })
}

#[cfg(test)]
mod blob_tests;
#[cfg(test)]
mod handshake_tests;
#[cfg(test)]
mod hello_tests;
#[cfg(test)]
mod receipt_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hivemind_core::identity::Identity;
    use hivemind_core::message::{Kind, Recipient, SenderKind};
    use hivemind_core::peer::NodeId;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use crate::service::{Draft, NodeDescription};

    pub(super) fn identity(seed: u8) -> Identity {
        Identity::from_seed([seed; 32]).expect("identity")
    }

    /// A service for `id`, plus the directory it lives in.
    pub(super) fn service(id: &Identity) -> (tempfile::TempDir, Arc<MailService>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let node = NodeDescription {
            id: id.node_id(),
            certificate: id.certificate_der().to_vec(),
            private_key: id.private_key_pkcs8().expect("key"),
            name: "host".to_owned(),
            owner: Some("host".to_owned()),
            callback_host: "127.0.0.1".to_owned(),
            peer_port: 8400,
            max_attachment_bytes: hivemind_core::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            inline_max_bytes: hivemind_core::config::DEFAULT_INLINE_MAX_BYTES,
            prefetch: false,
            read_receipts: false,
            presence_interval: hivemind_core::config::DEFAULT_PRESENCE_INTERVAL,
            tailscale: hivemind_core::config::Tailscale::Auto,
        };
        let service =
            MailService::open(dir.path(), node, id.signing_key().clone()).expect("service opens");
        (dir, Arc::new(service))
    }

    pub(super) fn caller(id: &Identity) -> CallerIdentity {
        caller_from(id, "10.0.0.2:51234")
    }

    /// A caller that reached us from a named address.
    pub(super) fn caller_from(id: &Identity, remote: &str) -> CallerIdentity {
        CallerIdentity {
            node_id: id.node_id(),
            certificate: id.certificate_der().to_vec(),
            remote: remote.parse().expect("an address"),
        }
    }

    /// Send one request through the peer router as `who`.
    pub(super) async fn call(
        service: &Arc<MailService>,
        who: &Identity,
        path: &str,
        body: &serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let router = router(Arc::clone(service)).layer(Extension(caller(who)));
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");

        let response = router.oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Deliver `message` (plus any inline blobs) as `who`, as the wire does.
    async fn deliver(
        service: &Arc<MailService>,
        who: &Identity,
        message: &serde_json::Value,
        blobs: &[(String, Vec<u8>)],
    ) -> (StatusCode, serde_json::Value) {
        const BOUNDARY: &str = "hivemind-test-boundary";

        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"message\"\r\n\
              Content-Type: application/json\r\n\r\n",
        );
        body.extend_from_slice(message.to_string().as_bytes());
        body.extend_from_slice(b"\r\n");

        for (name, bytes) in blobs {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let router = router(Arc::clone(service)).layer(Extension(caller(who)));
        let request = Request::builder()
            .method("POST")
            .uri("/peer/v1/messages")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .expect("request");

        let response = router.oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// A message from `from` to `to`, signed properly.
    fn message_from(from: &Identity, to: NodeId, subject: &str) -> serde_json::Value {
        let (_dir, sender) = service(from);
        let message = sender
            .send(
                Draft {
                    to: vec![Recipient::Node(to)],
                    subject: subject.to_owned(),
                    body: "body".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: Vec::new(),
                },
                SenderKind::Human,
            )
            .expect("send")
            .message;
        serde_json::to_value(message).expect("serialise")
    }

    /// The group every test host and friend is in, unless a test says not.
    pub(super) fn group_key() -> hivemind_core::group::GroupKey {
        hivemind_core::group::GroupKey::from_bytes([42u8; 16])
    }

    /// Put `service` in the test group.
    pub(super) fn join_group(service: &Arc<MailService>) {
        service
            .join_group(&group_key().code(), false)
            .expect("join the test group");
    }

    /// `from`'s proof of `key`, made for `to`, as the wire carries it.
    pub(super) fn proof(
        from: &Identity,
        to: &Identity,
        key: &hivemind_core::group::GroupKey,
    ) -> Proof {
        let now = chrono::Utc::now().timestamp_millis();
        Proof {
            sent_at: now,
            mac: data_encoding::HEXLOWER.encode(&key.prove(
                from.certificate_der(),
                to.certificate_der(),
                now,
            )),
        }
    }

    /// A handshake from `from` to `to`, claiming `host:port`, proving `key`.
    pub(super) fn offer(
        from: &Identity,
        to: &Identity,
        host: &str,
        port: u16,
        key: Option<&hivemind_core::group::GroupKey>,
    ) -> serde_json::Value {
        serde_json::to_value(Handshake {
            id: from.node_id().to_string(),
            name: "theirs".to_owned(),
            owner: Some("someone".to_owned()),
            version: "0.1.0".to_owned(),
            callback_host: host.to_owned(),
            callback_port: port,
            proof: key.map(|key| proof(from, to, key)),
            gossip: Vec::new(),
        })
        .expect("serialise")
    }

    /// Put `service` (which is `host`) in the test group, and have `friend`
    /// prove the key to it. Nobody confirms anything: that is ADR 0013.
    pub(super) async fn pair_with(service: &Arc<MailService>, host: &Identity, friend: &Identity) {
        join_group(service);
        let (status, _) = call(
            service,
            friend,
            "/peer/v1/handshake",
            &offer(friend, host, "127.0.0.1", 8400, Some(&group_key())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A signed message from `from` carrying `files`, plus the parts to send.
    fn message_with_files(
        from: &Identity,
        to: NodeId,
        files: &[(&str, &[u8])],
    ) -> (serde_json::Value, Vec<(String, Vec<u8>)>) {
        let (dir, sender) = service(from);
        let paths: Vec<_> = files
            .iter()
            .map(|(name, bytes)| {
                let path = dir.path().join(name);
                std::fs::write(&path, bytes).expect("write");
                path
            })
            .collect();

        let message = sender
            .send(
                Draft {
                    to: vec![Recipient::Node(to)],
                    subject: "with files".to_owned(),
                    body: "see attached".to_owned(),
                    kind: Kind::Message,
                    in_reply_to: None,
                    attachments: paths,
                },
                SenderKind::Human,
            )
            .expect("send")
            .message;

        let parts = message
            .attachments
            .iter()
            .filter(|a| a.inline)
            .map(|a| {
                let bytes = files
                    .iter()
                    .find(|(name, _)| *name == a.name)
                    .map(|(_, bytes)| (*bytes).to_vec())
                    .expect("the file we wrote");
                (a.sha256.to_hex(), bytes)
            })
            .collect();

        (serde_json::to_value(message).expect("serialise"), parts)
    }

    #[tokio::test]
    async fn an_inline_attachment_arrives_with_its_message() {
        let host = identity(27);
        let friend = identity(28);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let (message, parts) =
            message_with_files(&friend, host.node_id(), &[("notes.md", b"the contents")]);
        assert_eq!(
            parts.len(),
            1,
            "a small file should travel with the message"
        );

        let (status, _) = deliver(&service, &friend, &message, &parts).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let digest: hivemind_core::crypto::Sha256Digest = parts[0].0.parse().expect("digest");
        assert!(
            service.blobs().has(&digest),
            "the attachment should be readable without another request"
        );
    }

    #[tokio::test]
    async fn a_delivery_cannot_push_a_file_the_message_does_not_declare() {
        // Otherwise any paired peer could write arbitrary files into this
        // node's blob store by attaching them to an unrelated message.
        let host = identity(29);
        let friend = identity(30);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let message = message_from(&friend, host.node_id(), "nothing attached");
        let smuggled = hivemind_core::crypto::Sha256Digest::of(b"not declared");

        let (status, _) = deliver(
            &service,
            &friend,
            &message,
            &[(smuggled.to_hex(), b"not declared".to_vec())],
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!service.blobs().has(&smuggled));
        assert_eq!(
            service.unread_count().expect("count"),
            0,
            "and the message it rode in on is refused too"
        );
    }

    #[tokio::test]
    async fn a_part_whose_bytes_are_not_what_was_declared_is_refused() {
        // The digest is in the signed message, so substituting content here
        // means the sender is lying about something it signed.
        let host = identity(31);
        let friend = identity(32);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let (message, parts) =
            message_with_files(&friend, host.node_id(), &[("notes.md", b"the real thing")]);
        let swapped = vec![(parts[0].0.clone(), b"something else".to_vec())];

        let (status, _) = deliver(&service, &friend, &message, &swapped).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let digest: hivemind_core::crypto::Sha256Digest = parts[0].0.parse().expect("digest");
        assert!(
            !service.blobs().has(&digest),
            "nothing should have been written under a digest it does not match"
        );
    }

    #[tokio::test]
    async fn a_large_attachment_is_left_to_be_fetched_rather_than_sent() {
        // SPEC §8: over inline_max ships as a ref. The message still arrives.
        let host = identity(33);
        let friend = identity(34);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let big = vec![
            b'x';
            usize::try_from(hivemind_core::config::DEFAULT_INLINE_MAX_BYTES).unwrap() + 1
        ];
        let (message, parts) = message_with_files(&friend, host.node_id(), &[("big.bin", &big)]);

        assert!(parts.is_empty(), "too big to travel with the message");

        let (status, _) = deliver(&service, &friend, &message, &parts).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(service.unread_count().expect("count"), 1);
    }

    #[tokio::test]
    async fn a_delivery_with_no_message_part_is_refused() {
        let host = identity(35);
        let friend = identity(36);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let router = router(Arc::clone(&service)).layer(Extension(caller(&friend)));
        let request = Request::builder()
            .method("POST")
            .uri("/peer/v1/messages")
            .header("content-type", "multipart/form-data; boundary=x")
            .body(Body::from("--x--\r\n"))
            .expect("request");

        let response = router.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn redelivering_a_message_neither_duplicates_it_nor_marks_it_unread() {
        // SPEC §8: recipients dedupe on message id, so the sender can retry
        // forever without having to know whether the last attempt landed.
        let host = identity(13);
        let friend = identity(14);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let message = message_from(&friend, host.node_id(), "say it twice");
        let id: ulid::Ulid = message["id"]
            .as_str()
            .expect("an id")
            .parse()
            .expect("ulid");

        let (first, _) = deliver(&service, &friend, &message, &[]).await;
        assert_eq!(first, StatusCode::ACCEPTED);
        service.mark_read(id).expect("read it");

        let (again, body) = deliver(&service, &friend, &message, &[]).await;
        assert_eq!(again, StatusCode::ACCEPTED, "a retry is a success");
        assert_eq!(body["id"], message["id"]);

        assert_eq!(
            service.unread_count().expect("count"),
            0,
            "a redelivery must not drag a message back into the inbox"
        );
        assert_eq!(
            service
                .list(&hivemind_core::index::Query::default())
                .expect("list")
                .len(),
            1,
            "and must not leave two copies"
        );
    }

    #[tokio::test]
    async fn an_unpaired_caller_is_refused_with_not_paired() {
        // SPEC §6.2 step 3. Reaching the port is not being trusted (ADR 0010).
        let host = identity(1);
        let stranger = identity(2);
        let (_dir, service) = service(&host);
        let message = message_from(&stranger, host.node_id(), "let me in");

        let (status, problem) = deliver(&service, &stranger, &message, &[]).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(problem["type"], "/problems/not-paired");
        assert_eq!(
            service.unread_count().expect("count"),
            0,
            "nothing should have been stored"
        );
    }

    #[tokio::test]
    async fn a_paired_caller_can_deliver() {
        let host = identity(3);
        let friend = identity(4);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let message = message_from(&friend, host.node_id(), "hello");
        let (status, body) = deliver(&service, &friend, &message, &[]).await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["id"], message["id"]);
        assert_eq!(service.unread_count().expect("count"), 1);
    }

    #[tokio::test]
    async fn a_peer_may_not_deliver_a_message_signed_by_somebody_else() {
        // Otherwise any paired node could forge mail from any other.
        let host = identity(5);
        let friend = identity(6);
        let third_party = identity(7);
        let (_dir, service) = service(&host);
        pair_with(&service, &host, &friend).await;

        let forged = message_from(&third_party, host.node_id(), "not mine");
        let (status, problem) = deliver(&service, &friend, &forged, &[]).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["type"], "/problems/identity-mismatch");
        assert_eq!(service.unread_count().expect("count"), 0);
    }
}
