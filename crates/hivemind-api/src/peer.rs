//! The peer router (SPEC §7.2): handshake and delivery.
//!
//! Authorisation lives here rather than in the TLS layer (ADR 0010).
//! `/peer/v1/handshake` is open, because a node we have never met has to be
//! able to introduce itself. Everything else requires a peer in `peers.toml`
//! and answers `403 not_paired` otherwise, exactly as SPEC §6.2 specifies.

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
    /// Reserved for gossiping peer lists in v2 (SPEC §12). Always `None` today,
    /// and ignored on receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gossip: Option<serde_json::Value>,
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
        .route(
            "/peer/v1/messages",
            post(receive_message)
                // axum defaults to 2 MiB, which is smaller than one inline
                // attachment. This is the only route that needs raising, and
                // it is raised to exactly what this node is willing to hold.
                .layer(axum::extract::DefaultBodyLimit::max(limit)),
        )
        .route(
            "/peer/v1/blobs/{sha}",
            axum::routing::get(serve_blob).head(have_blob),
        )
        .with_state(state)
}

/// Introduce ourselves, and record who introduced themselves to us.
///
/// Open to anyone who can complete a TLS handshake (ADR 0010). It records a
/// pending pair and returns our own details; it never reads or reveals anything
/// about the mailbox.
async fn handshake(
    State(service): State<Arc<MailService>>,
    Extension(caller): Extension<CallerIdentity>,
    Json(request): Json<Handshake>,
) -> Result<Json<Handshake>, Problem> {
    // The body says who they are; the certificate proves it. When the two
    // disagree, believe neither — a node that misdescribes itself in the one
    // field we can check is not one to record.
    if request.id != caller.node_id.to_string() {
        return Err(Problem::new(
            ProblemType::IdentityMismatch,
            "the node id in the handshake does not match the certificate presented",
        ));
    }

    // Where they reached us from is not knowable here — the address we record
    // is the one they tell us to call back on, which `hivemind join` fills in
    // from the host the user typed.
    let addr = PeerAddr {
        host: request.callback_host.clone(),
        port: request.callback_port,
        source: AddrSource::Manual,
        last_ok: None,
    };

    service.record_pairing_offer(caller.node_id, &request, caller.certificate.clone(), addr)?;
    Ok(Json(service.own_handshake()))
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
    // SPEC §6.2 step 3: until both sides confirm, mail is refused.
    if !service.is_paired(caller.node_id)? {
        return Err(Problem::new(
            ProblemType::NotPaired,
            "this node has not paired with you; run `hivemind pair` on both sides",
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
            "this node has not paired with you; run `hivemind pair` on both sides",
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

    fn identity(seed: u8) -> Identity {
        Identity::from_seed([seed; 32]).expect("identity")
    }

    /// A service for `id`, plus the directory it lives in.
    fn service(id: &Identity) -> (tempfile::TempDir, Arc<MailService>) {
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
        };
        let service =
            MailService::open(dir.path(), node, id.signing_key().clone()).expect("service opens");
        (dir, Arc::new(service))
    }

    fn caller(id: &Identity) -> CallerIdentity {
        CallerIdentity {
            node_id: id.node_id(),
            certificate: id.certificate_der().to_vec(),
        }
    }

    /// Send one request through the peer router as `who`.
    async fn call(
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
            .expect("send");
        serde_json::to_value(message).expect("serialise")
    }

    /// Handshake as `friend` and confirm from this side.
    async fn pair_with(service: &Arc<MailService>, friend: &Identity) {
        let (status, _) = call(
            service,
            friend,
            "/peer/v1/handshake",
            &serde_json::to_value(Handshake {
                id: friend.node_id().to_string(),
                name: "friend".to_owned(),
                owner: Some("friend".to_owned()),
                version: "0.1.0".to_owned(),
                callback_host: "127.0.0.1".to_owned(),
                callback_port: 8400,
                gossip: None,
            })
            .expect("serialise"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        service.confirm_pair(friend.node_id()).expect("confirm");
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
            .expect("send");

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
        pair_with(&service, &friend).await;

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
        pair_with(&service, &friend).await;

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
        pair_with(&service, &friend).await;

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
        pair_with(&service, &friend).await;

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
        pair_with(&service, &friend).await;

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
        pair_with(&service, &friend).await;

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

        // Pair, exactly as a handshake followed by a confirmation would.
        let (status, _) = call(
            &service,
            &friend,
            "/peer/v1/handshake",
            &serde_json::to_value(Handshake {
                id: friend.node_id().to_string(),
                name: "friend".to_owned(),
                owner: Some("friend".to_owned()),
                version: "0.1.0".to_owned(),
                callback_host: "127.0.0.1".to_owned(),
                callback_port: 8400,
                gossip: None,
            })
            .expect("serialise"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        service.confirm_pair(friend.node_id()).expect("confirm");

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

        call(
            &service,
            &friend,
            "/peer/v1/handshake",
            &serde_json::to_value(Handshake {
                id: friend.node_id().to_string(),
                name: "friend".to_owned(),
                owner: None,
                version: "0.1.0".to_owned(),
                callback_host: "127.0.0.1".to_owned(),
                callback_port: 8400,
                gossip: None,
            })
            .expect("serialise"),
        )
        .await;
        service.confirm_pair(friend.node_id()).expect("confirm");

        let forged = message_from(&third_party, host.node_id(), "not mine");
        let (status, problem) = deliver(&service, &friend, &forged, &[]).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["type"], "/problems/identity-mismatch");
        assert_eq!(service.unread_count().expect("count"), 0);
    }

    #[tokio::test]
    async fn a_handshake_whose_body_disagrees_with_its_certificate_is_refused() {
        let host = identity(8);
        let caller_id = identity(9);
        let someone_else = identity(10);
        let (_dir, service) = service(&host);

        let (status, problem) = call(
            &service,
            &caller_id,
            "/peer/v1/handshake",
            &serde_json::to_value(Handshake {
                // Claims to be someone else.
                id: someone_else.node_id().to_string(),
                name: "liar".to_owned(),
                owner: None,
                version: "0.1.0".to_owned(),
                callback_host: "127.0.0.1".to_owned(),
                callback_port: 8400,
                gossip: None,
            })
            .expect("serialise"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["type"], "/problems/identity-mismatch");
        assert!(
            service.pending_pairs().expect("pending").is_empty(),
            "a node that misdescribes itself should not be recorded at all"
        );
    }

    #[tokio::test]
    async fn a_handshake_from_a_stranger_is_recorded_as_pending_not_paired() {
        let host = identity(11);
        let stranger = identity(12);
        let (_dir, service) = service(&host);

        let (status, answer) = call(
            &service,
            &stranger,
            "/peer/v1/handshake",
            &serde_json::to_value(Handshake {
                id: stranger.node_id().to_string(),
                name: "stranger".to_owned(),
                owner: Some("someone".to_owned()),
                version: "0.1.0".to_owned(),
                callback_host: "10.0.0.7".to_owned(),
                callback_port: 9000,
                gossip: None,
            })
            .expect("serialise"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["id"], host.node_id().to_string());
        assert!(
            !service.is_paired(stranger.node_id()).expect("is_paired"),
            "a handshake is an introduction, not an agreement"
        );

        let pending = service.pending_pairs().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, stranger.node_id());
        assert_eq!(
            pending[0].addr.authority(),
            "10.0.0.7:9000",
            "the callback address it gave should be what we record"
        );
    }

    /// GET or HEAD the peer blob route as `who`, with optional headers.
    async fn fetch(
        service: &Arc<MailService>,
        who: &Identity,
        method: &str,
        sha: &str,
        range: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let router = router(Arc::clone(service)).layer(Extension(caller(who)));
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/peer/v1/blobs/{sha}"));
        if let Some(range) = range {
            request = request.header("range", range);
        }

        let response = router
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        (status, headers, bytes)
    }

    fn header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    }

    #[tokio::test]
    async fn a_paired_peer_can_fetch_a_whole_blob() {
        let host = identity(15);
        let friend = identity(16);
        let (_dir, service) = service(&host);
        pair_with(&service, &friend).await;

        let digest = service.blobs().put_bytes(b"attachment bytes").expect("put");

        let (status, headers, body) = fetch(&service, &friend, "GET", &digest.to_hex(), None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"attachment bytes");
        assert_eq!(header(&headers, "accept-ranges").as_deref(), Some("bytes"));
        assert_eq!(header(&headers, "content-length").as_deref(), Some("16"));
    }

    #[tokio::test]
    async fn an_interrupted_transfer_resumes_from_where_it_stopped() {
        // SPEC §7.2: range requests supported (resume). This is the wire half
        // of what the blob store does on disk.
        let host = identity(17);
        let friend = identity(18);
        let (_dir, service) = service(&host);
        pair_with(&service, &friend).await;

        let content = b"0123456789abcdef";
        let digest = service.blobs().put_bytes(content).expect("put");

        let (status, headers, body) = fetch(
            &service,
            &friend,
            "GET",
            &digest.to_hex(),
            Some("bytes=10-"),
        )
        .await;

        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, b"abcdef", "only the part that was missing");
        assert_eq!(
            header(&headers, "content-range").as_deref(),
            Some("bytes 10-15/16")
        );
        assert_eq!(header(&headers, "content-length").as_deref(), Some("6"));
    }

    #[tokio::test]
    async fn resuming_past_the_end_says_so_rather_than_hanging() {
        // The two sides disagree about the file. 416 tells the caller to start
        // over; an empty 206 would leave it waiting for bytes never coming.
        let host = identity(19);
        let friend = identity(20);
        let (_dir, service) = service(&host);
        pair_with(&service, &friend).await;

        let digest = service.blobs().put_bytes(b"short").expect("put");

        let (status, headers, _) = fetch(
            &service,
            &friend,
            "GET",
            &digest.to_hex(),
            Some("bytes=99-"),
        )
        .await;

        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            header(&headers, "content-range").as_deref(),
            Some("bytes */5")
        );
    }

    #[tokio::test]
    async fn asking_whether_a_blob_is_still_there_does_not_send_it() {
        let host = identity(21);
        let friend = identity(22);
        let (_dir, service) = service(&host);
        pair_with(&service, &friend).await;

        let digest = service.blobs().put_bytes(b"still here").expect("put");

        let (status, headers, body) =
            fetch(&service, &friend, "HEAD", &digest.to_hex(), None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, "content-length").as_deref(), Some("10"));
        assert!(body.is_empty(), "HEAD has no body");

        let gone = hivemind_core::crypto::Sha256Digest::of(b"deleted since");
        let (status, _, _) = fetch(&service, &friend, "HEAD", &gone.to_hex(), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a sender that deleted the file should say so, not stall the download"
        );
    }

    #[tokio::test]
    async fn an_unpaired_peer_cannot_fetch_anything() {
        // Blobs are only fetched for mail the peer already received, so this
        // takes the same check as delivery.
        let host = identity(23);
        let stranger = identity(24);
        let (_dir, service) = service(&host);

        let digest = service.blobs().put_bytes(b"private").expect("put");

        let (status, _, _) = fetch(&service, &stranger, "GET", &digest.to_hex(), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _, _) = fetch(&service, &stranger, "HEAD", &digest.to_hex(), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_digest_that_is_not_a_digest_cannot_name_a_file() {
        // It arrives in a URL path from another machine.
        let host = identity(25);
        let friend = identity(26);
        let (_dir, service) = service(&host);
        pair_with(&service, &friend).await;

        for attempt in ["..", "not-hex", "%2e%2e%2fpeers.toml"] {
            let (status, _, _) = fetch(&service, &friend, "GET", attempt, None).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{attempt:?} should not name anything"
            );
        }
    }

    #[test]
    fn only_a_resume_range_is_honoured() {
        let range = |value: &str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("range", value.parse().expect("header"));
            resume_offset(&headers)
        };

        assert_eq!(range("bytes=1024-"), Some(1024));
        assert_eq!(range("bytes=0-"), Some(0));
        // Everything else gets the whole blob, which is correct if unhelpful.
        assert_eq!(range("bytes=0-99"), None);
        assert_eq!(range("bytes=-500"), None);
        assert_eq!(range("bytes=0-10,20-30"), None);
        assert_eq!(range("items=1-"), None);
        assert_eq!(resume_offset(&axum::http::HeaderMap::new()), None);
    }
}
