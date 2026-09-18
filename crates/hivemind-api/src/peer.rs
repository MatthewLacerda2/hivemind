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
