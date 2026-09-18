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
    Router::new()
        .route("/peer/v1/handshake", post(handshake))
        .route("/peer/v1/messages", post(receive_message))
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
    Json(message): Json<Message>,
) -> Result<(StatusCode, Json<Delivered>), Problem> {
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

    let id = service.receive(caller.node_id, message)?;
    Ok((StatusCode::ACCEPTED, Json(Delivered { id: id.to_string() })))
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

        let (first, _) = call(&service, &friend, "/peer/v1/messages", &message).await;
        assert_eq!(first, StatusCode::ACCEPTED);
        service.mark_read(id).expect("read it");

        let (again, body) = call(&service, &friend, "/peer/v1/messages", &message).await;
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

        let (status, problem) = call(&service, &stranger, "/peer/v1/messages", &message).await;

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
        let (status, body) = call(&service, &friend, "/peer/v1/messages", &message).await;

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
        let (status, problem) = call(&service, &friend, "/peer/v1/messages", &forged).await;

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
}
